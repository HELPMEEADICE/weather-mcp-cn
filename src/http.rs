//! 统一的 HTTP 访问层。
//!
//! 职责：
//! * 复用连接池（同一个 [`reqwest::Client`]）；
//! * 连接超时 + 整体超时；
//! * 全局并发上限（信号量）与「同一目标最小请求间隔」（礼貌抓取）；
//! * **所有**响应路径（2xx / 非 2xx）的统一有界读取；
//! * 有上限的指数退避重试（只对超时 / 429 / 5xx 重试）；
//! * 正确解码 UTF-8 与 GBK/GB18030 网页。
//!
//! # 限速与并发的获取顺序
//!
//! 旧实现先 `wait_for_slot()` 预约一个时间片，再去抢全局信号量。问题在于「预约时刻」
//! 不等于「实际发送时刻」：信号量被占满时，排队者会在预约时刻上先耗尽等待，等许可
//! 集中放行时再一起把请求打出去，同主机的真实发送间隔被压缩到接近 0。
//!
//! 现在的顺序是（见 [`HttpClient::get_text`]）：
//!
//! 1. 先进入**主机独占区**（[`HostLimiter::enter`]）：同一主机同一时刻只有一个请求
//!    在这里排队等自己的时间片 —— 限速等待期间不占用任何并发许可；
//! 2. 再申请全局并发许可：许可证只在真正要发请求时才持有；
//! 3. 拿到许可、真正发送之前调用 [`HostLease::anchor`]，把「下一次最早发送时刻」锚定在
//!    `Instant::now()`，因此记录的是**真实发送时刻**而不是预约时刻；
//! 4. 锚定后立刻释放主机独占区，让同主机的下一个请求开始等它自己的时刻，而本次请求
//!    继续带着并发许可读响应体。
//!
//! 不同主机的独占区互不相关，可以并行推进。
//!
//! 本项目不做任何访问控制规避：不换代理池、不伪造验证码、不并发轰炸单一站点。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, Semaphore};
use tracing::{debug, warn};

use crate::config::Config;
use crate::error::{Result, WeatherError};

/// 成功响应的字节上限。
///
/// 实测：城市选择器脚本约 1 MB，7 日预报页面约 3 KB，逐 3 小时页面约 67 KB。
/// 4 MiB 已经非常宽裕；超过它说明响应根本不是我们要的东西（上游故障、被劫持、
/// 或者返回了体积异常的页面），此时必须尽快失败，而不是先把几十 MB 读进内存。
pub(crate) const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// 非 2xx 响应的字节上限。
///
/// 错误页面只用来区分「CDN 拦截」与「普通错误状态」：`X-CCDN-FORBID-CODE` 在响应头里，
/// 页面里的 `Forbid_code:` 实测出现在开头几百字节内。32 KiB 足以覆盖任何真实错误页，
/// 又保证异常 403 / 429 / 5xx 页面不会把内存拖垮。
pub(crate) const MAX_ERROR_BODY_BYTES: usize = 32 * 1024;

/// 取锁，锁中毒时继续使用内部数据（限速表不承载跨字段不变量）。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 同一主机的发送闸门。
///
/// 每个主机一把异步互斥量，值是该主机「下一次最早允许发送的时刻」。
struct HostLimiter {
    gates: Mutex<HashMap<String, Arc<AsyncMutex<Instant>>>>,
    min_interval: Duration,
}

/// 已经进入某主机独占发送区的凭据。
///
/// 持有期间同一主机的其它请求会在 [`HostLimiter::enter`] 里排队，因此 `anchor` 一定
/// 是串行执行的 —— 不存在两个请求同时把时间锚到同一个瞬间。
struct HostLease {
    next_allowed: OwnedMutexGuard<Instant>,
    min_interval: Duration,
}

impl HostLease {
    /// 把「下一次最早发送时刻」锚定到**现在**。
    ///
    /// 调用点必须紧贴真正的 `send()`：这里记录的就是真实发送时刻，下一个同主机请求会
    /// 等到 `此刻 + min_interval`，从而保证两次实际发送之间满足最小间隔。
    fn anchor(&mut self) {
        *self.next_allowed = Instant::now() + self.min_interval;
    }
}

impl HostLimiter {
    fn new(min_interval: Duration) -> Self {
        Self {
            gates: Mutex::new(HashMap::new()),
            min_interval,
        }
    }

    /// 进入主机独占区：等到该主机允许发送的时刻再返回凭据。
    ///
    /// 取消安全：排队或 sleep 期间被 drop 不会留下任何状态（`next_allowed` 只在
    /// [`HostLease::anchor`] 里被改写，而它发生在真正发送之前）。
    async fn enter(&self, host: &str) -> HostLease {
        let gate = {
            // 同步锁只用于取出（或创建）该主机的闸门，绝不跨越 await。
            let mut gates = lock(&self.gates);
            Arc::clone(
                gates
                    .entry(host.to_string())
                    .or_insert_with(|| Arc::new(AsyncMutex::new(Instant::now()))),
            )
        };

        // 异步锁可以安全地跨越挂起点；`lock_owned` 避免了「自引用」的结构问题。
        let next_allowed = gate.lock_owned().await;
        let now = Instant::now();
        if *next_allowed > now {
            tokio::time::sleep(*next_allowed - now).await;
        }
        HostLease {
            next_allowed,
            min_interval: self.min_interval,
        }
    }
}

/// 带限速与重试的 HTTP 客户端。
pub struct HttpClient {
    client: reqwest::Client,
    semaphore: Semaphore,
    hosts: HostLimiter,
    max_retries: u32,
}

impl HttpClient {
    /// 依据配置构建客户端。
    pub fn new(config: &Config) -> Result<Self> {
        if looks_like_blocked_agent(&config.user_agent) {
            warn!(
                user_agent = %config.user_agent,
                "这个 User-Agent 会被中国天气网 CDN 拦截（实测 curl/wget/python-requests/go-http-client/java/scrapy/okhttp 均返回 403），\
                 实况与预警接口将不可用；建议用 WEATHER_MCP_USER_AGENT 换成默认值或浏览器 UA"
            );
        }

        let client = reqwest::Client::builder()
            .user_agent(config.user_agent.clone())
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout)
            .redirect(reqwest::redirect::Policy::limited(5))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(4)
            .build()
            .map_err(|e| WeatherError::Config(format!("构建 HTTP 客户端失败：{e}")))?;

        Ok(Self {
            client,
            semaphore: Semaphore::new(config.max_concurrency.max(1)),
            hosts: HostLimiter::new(config.min_request_interval),
            max_retries: config.max_retries,
        })
    }

    /// GET 一个上游地址，返回解码后的文本。
    ///
    /// `referer` 是中国天气网部分数据接口的要求（它们会校验来源页面）。
    pub async fn get_text(&self, url: &str, referer: Option<&str>) -> Result<String> {
        let host = host_of(url);
        let mut last_error: Option<WeatherError> = None;
        let attempts = self.max_retries.saturating_add(1);

        for attempt in 0..attempts {
            if attempt > 0 {
                let backoff = backoff_delay(attempt);
                debug!(url, attempt, ?backoff, "重试上游请求");
                tokio::time::sleep(backoff).await;
            }

            // 1) 先在主机独占区里等到允许发送的时刻（不占并发许可）。
            let mut lease = self.hosts.enter(&host).await;
            // 2) 再申请全局并发许可。
            let _permit = self
                .semaphore
                .acquire()
                .await
                .map_err(|_| WeatherError::Upstream("HTTP 并发控制已关闭".to_string()))?;
            // 3) 真正发送之前锚定时刻，然后让出主机独占区。
            lease.anchor();
            drop(lease);

            let mut request = self.client.get(url).header("Accept", "*/*");
            if let Some(referer) = referer {
                request = request.header("Referer", referer);
            }

            let mut response = match request.send().await {
                Ok(response) => response,
                Err(err) => {
                    let mapped = WeatherError::from(err);
                    let retryable = mapped.is_retryable();
                    warn!(url, attempt, error = %mapped, "上游请求失败");
                    last_error = Some(mapped);
                    if retryable && attempt + 1 < attempts {
                        continue;
                    }
                    break;
                }
            };

            let status = response.status();
            let code = status.as_u16();
            // CDN 会在响应头里打上 Forbid 标识，先取下来再消费响应体。
            let forbid_header = header_value(&response, "x-ccdn-forbid-code");
            // 成功与失败都要解码，因此统一取一次 Content-Type。
            let content_type = header_value(&response, "content-type");

            if status.is_success() {
                // 先看声明的长度：能提前拒绝就不必开始读。
                if let Some(declared) = response.content_length()
                    && declared > MAX_BODY_BYTES as u64
                {
                    return Err(oversized_body(url, declared as usize));
                }
                let read = read_body_capped(&mut response, MAX_BODY_BYTES).await;
                let bytes = match read.into_ok(url) {
                    Ok(bytes) => bytes,
                    Err(err) => {
                        let retryable = err.is_retryable();
                        warn!(url, attempt, error = %err, "读取响应体失败");
                        last_error = Some(err);
                        if retryable && attempt + 1 < attempts {
                            continue;
                        }
                        break;
                    }
                };
                return Ok(decode_body(&bytes, content_type.as_deref()));
            }

            // 非 2xx：与成功路径一样有界读取，只是上限更小。已经读到的前缀仍然会被
            // 用来提取 `Forbid_code`，所以 Forbid_code 落在前 32 KiB 里的情况不受影响。
            let read = read_body_capped(&mut response, MAX_ERROR_BODY_BYTES).await;
            let body = decode_body(&read.bytes, content_type.as_deref());
            let error = match read.failure {
                // 响应体完整：按既有规则分类。
                None => classify_http_error(code, forbid_header.as_deref(), &body, url),
                // 响应体超限 / 提前断开：已经读到的前缀仍然要参与分类（`Forbid_code`
                // 常常就在前几百字节里），响应头给出的 Forbid_code 同样可信。两者都没有
                // 命中时，不能假装「响应体就是空的」—— 那会把结论误导成普通状态码错误。
                Some(failure) => {
                    match classify_http_error(code, forbid_header.as_deref(), &body, url) {
                        blocked @ WeatherError::Blocked { .. } => blocked,
                        _ => WeatherError::UnreadableErrorBody {
                            status: code,
                            url: url.to_string(),
                            reason: failure.to_string(),
                        },
                    }
                }
            };
            warn!(url, status = code, attempt, error = %error, "上游返回非 2xx");
            let retryable = error.is_retryable();
            last_error = Some(error);
            if retryable && attempt + 1 < attempts {
                continue;
            }
            break;
        }

        Err(last_error.unwrap_or_else(|| {
            WeatherError::Upstream(format!("请求上游失败且未取得任何响应：{url}"))
        }))
    }
}

fn header_value(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    // 去掉端口，同一站点不同端口共享限速槽位。
    host.split(':').next().unwrap_or(host).to_string()
}

/// 有界读取的结果。
///
/// 读取失败时保留已经读到的前缀：错误页面里可能就藏着 `Forbid_code`，直接丢掉前缀
/// 会让诊断更难定位。
struct BodyRead {
    bytes: Vec<u8>,
    /// 读取过程中的异常（超限、提前断开）。`None` 表示响应体已完整读完。
    failure: Option<BodyFailure>,
}

impl BodyRead {
    /// 成功路径：要求响应体完整，否则转成具体错误。
    fn into_ok(self, url: &str) -> Result<Vec<u8>> {
        match self.failure {
            None => Ok(self.bytes),
            Some(failure) => Err(failure.into_error(url)),
        }
    }
}

/// 响应体读取过程中的异常。
enum BodyFailure {
    /// 超过上限。
    TooLarge { limit: usize, at_least: usize },
    /// 底层读取失败（连接提前断开、解码错误等）。
    Transport(WeatherError),
}

impl BodyFailure {
    fn into_error(self, url: &str) -> WeatherError {
        match self {
            Self::TooLarge { limit, at_least } => WeatherError::Parse(format!(
                "上游响应体超过 {limit} 字节上限（至少 {at_least} 字节）：{url}"
            )),
            Self::Transport(err) => err,
        }
    }
}

impl std::fmt::Display for BodyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { limit, .. } => write!(f, "响应体超过 {limit} 字节读取上限"),
            Self::Transport(err) => write!(f, "{err}"),
        }
    }
}

/// 分块读取响应体，最多 `limit` 字节。
///
/// 不用 `response.text()` / `response.bytes()`：那会把整个响应体一次性读进内存，
/// 没有上限，一个异常的 403/429/5xx 页面就能把内存拖垮。
///
/// 预分配只取 8 KiB 而不是 `limit`：正常页面（3 KB ~ 67 KB）远小于上限，
/// 按上限预留会让每个请求都白占几 MiB 虚拟内存。
async fn read_body_capped(response: &mut reqwest::Response, limit: usize) -> BodyRead {
    let mut body: Vec<u8> = Vec::with_capacity(8 * 1024);
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let total = body.len() + chunk.len();
                if total > limit {
                    // 只保留上限内的前缀，供调用方继续提取 `Forbid_code`。
                    let keep = limit.saturating_sub(body.len()).min(chunk.len());
                    body.extend_from_slice(&chunk[..keep]);
                    return BodyRead {
                        bytes: body,
                        failure: Some(BodyFailure::TooLarge {
                            limit,
                            at_least: total,
                        }),
                    };
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => {
                return BodyRead {
                    bytes: body,
                    failure: None,
                };
            }
            Err(err) => {
                return BodyRead {
                    bytes: body,
                    failure: Some(BodyFailure::Transport(WeatherError::from(err))),
                };
            }
        }
    }
}

/// 响应体超限错误（不是可重试错误：重试只会再读一遍）。
fn oversized_body(url: &str, size: usize) -> WeatherError {
    WeatherError::Parse(format!(
        "上游响应体超过 {MAX_BODY_BYTES} 字节上限（至少 {size} 字节）：{url}"
    ))
}

/// 把非 2xx 响应分类成具体错误。
///
/// 中国天气网的 CDN 在拦截请求时会回 403，并在响应头 `X-CCDN-FORBID-CODE`
/// 与响应体里带上 `Forbid_code`。这类拦截和「接口真的不存在」表现完全不同，
/// 需要给出可排查的提示，而不是一句干巴巴的 403。
pub fn classify_http_error(
    status: u16,
    forbid_header: Option<&str>,
    body: &str,
    url: &str,
) -> WeatherError {
    let forbid = forbid_header
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| extract_forbid_code(body));

    match forbid {
        Some(code) if matches!(status, 403 | 406 | 412) => WeatherError::Blocked {
            status,
            hint: forbid_hint(&code).to_string(),
            code,
            url: url.to_string(),
        },
        _ => WeatherError::UpstreamStatus {
            status,
            url: url.to_string(),
        },
    }
}

/// 把实测得到的 `Forbid_code` 翻译成可执行的排查建议。
///
/// 映射来自对 `d1.weather.com.cn` 的直接实测：
/// `020200` 只在 Referer 不合规时出现，`020300` 只在 User-Agent 不合规时出现。
fn forbid_hint(code: &str) -> &'static str {
    match code {
        "020200" => {
            "该错误码实测对应 Referer 不合规：缺失，或不是 https://www.weather.com.cn/ 域下的地址。\
             本项目所有请求都带正确 Referer，出现此错误通常是中间代理改写了请求头"
        }
        "020300" => {
            "该错误码实测对应 User-Agent 不合规：命中爬虫黑名单\
             （curl/、Wget/、python-requests、Go-http-client、Java/、Scrapy/、okhttp/）或为空。\
             请检查 WEATHER_MCP_USER_AGENT；默认值可用"
        }
        _ => {
            "请求头不符合站点要求。请检查 WEATHER_MCP_USER_AGENT，\
             并确认请求没有经过会改写请求头的代理"
        }
    }
}

/// 从 CDN 的 403 页面里抠出 `Forbid_code`。
fn extract_forbid_code(body: &str) -> Option<String> {
    let marker = "Forbid_code:";
    let start = body.find(marker)? + marker.len();
    let rest = body[start..].trim_start();
    let code: String = rest
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric())
        .collect();
    if code.is_empty() { None } else { Some(code) }
}

/// 判断一个 User-Agent 是否会被中国天气网 CDN 拦截。
///
/// 实测（2026-11-05）：`curl/`、`Wget/`、`python-requests/`、`Go-http-client`、
/// `Java/`、`Scrapy/`、`okhttp/` 一律 403，而自定义 UA 与浏览器 UA 正常。
pub fn looks_like_blocked_agent(user_agent: &str) -> bool {
    const BLOCKED: &[&str] = &[
        "curl/",
        "wget/",
        "python-requests",
        "python-urllib",
        "go-http-client",
        "java/",
        "scrapy/",
        "okhttp/",
        "httpclient",
        "libwww",
        "phantomjs",
        "headlesschrome",
    ];
    let lower = user_agent.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return true;
    }
    BLOCKED
        .iter()
        .any(|marker| lower.starts_with(marker) || lower.contains(marker))
}

fn backoff_delay(attempt: u32) -> Duration {
    let base_ms = 300u64.saturating_mul(1u64 << attempt.min(5));
    let jitter_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_millis() as u64 % 200)
        .unwrap_or(0);
    Duration::from_millis(base_ms.min(4000) + jitter_ms)
}

/// 按 Content-Type 声明的字符集解码；否则先尝试 UTF-8，失败回落 GB18030。
pub fn decode_body(bytes: &[u8], content_type: Option<&str>) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let charset = content_type
        .and_then(|value| {
            value
                .split(';')
                .find_map(|part| part.trim().strip_prefix("charset="))
        })
        .map(|value| value.trim().trim_matches('"').to_ascii_lowercase());

    match charset.as_deref() {
        Some("gbk") | Some("gb2312") | Some("gb18030") | Some("gb_2312-80") => {
            return encoding_rs::GB18030.decode(bytes).0.into_owned();
        }
        Some("utf-8") | Some("utf8") | Some("us-ascii") => {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        _ => {}
    }

    // 无 charset 声明：先看 BOM，再严格尝试 UTF-8，最后回落 GB18030。
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(&bytes[3..]).into_owned();
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => encoding_rs::GB18030.decode(bytes).0.into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_hosts() {
        assert_eq!(
            host_of("https://d1.weather.com.cn/sk_2d/1.html"),
            "d1.weather.com.cn"
        );
        assert_eq!(
            host_of("http://www.weather.com.cn:80/x"),
            "www.weather.com.cn"
        );
    }

    #[test]
    fn decodes_gbk_when_declared() {
        let (encoded, _, _) = encoding_rs::GB18030.encode("北京晴");
        let text = decode_body(&encoded, Some("text/html; charset=gb2312"));
        assert_eq!(text, "北京晴");
    }

    #[test]
    fn decodes_utf8_without_declaration() {
        let text = decode_body("北京晴".as_bytes(), None);
        assert_eq!(text, "北京晴");
    }

    #[test]
    fn decodes_gbk_without_declaration() {
        let (encoded, _, _) = encoding_rs::GB18030.encode("多云转晴");
        assert_eq!(decode_body(&encoded, None), "多云转晴");
    }

    /// 实测抓到的 CDN 403 页面（缺 Referer 时）。
    const FORBID_BODY: &str = "<html><body><center><h1>403 Forbidden</h1></center>\
<p>Node_info: 3034-CACHE8</p><p>Client_ip: 1.2.3.4</p><p>Forbid_code: 020200</p>\
<p>Hit-status: MISS</p></body></html>";

    #[test]
    fn extracts_forbid_code_from_cdn_page() {
        assert_eq!(extract_forbid_code(FORBID_BODY).as_deref(), Some("020200"));
        assert_eq!(extract_forbid_code("<html>403</html>"), None);
        assert_eq!(extract_forbid_code("Forbid_code:"), None);
    }

    #[test]
    fn classifies_cdn_403_as_blocked() {
        let error = classify_http_error(
            403,
            Some("020200"),
            FORBID_BODY,
            "http://d1.weather.com.cn/sk_2d/101010100.html",
        );
        assert_eq!(error.code(), "upstream_blocked");
        assert!(error.is_retryable(), "CDN 拦截可能是短时风控，应允许重试");
        let text = error.to_string();
        assert!(text.contains("020200"), "错误里要带 Forbid_code：{text}");
        assert!(text.contains("Referer"), "错误里要给出排查方向：{text}");
    }

    /// 实测映射：020200 只出现在 Referer 不合规时，020300 只出现在 UA 不合规时。
    #[test]
    fn forbid_codes_map_to_their_cause() {
        let referer = classify_http_error(403, Some("020200"), "", "u").to_string();
        assert!(referer.contains("Referer"), "{referer}");

        let agent = classify_http_error(403, Some("020300"), "", "u").to_string();
        assert!(agent.contains("User-Agent"), "{agent}");
        assert!(agent.contains("WEATHER_MCP_USER_AGENT"), "{agent}");

        let unknown = classify_http_error(403, Some("999999"), "", "u").to_string();
        assert!(unknown.contains("WEATHER_MCP_USER_AGENT"), "{unknown}");
    }

    #[test]
    fn classifies_403_without_marker_as_plain_status() {
        let error = classify_http_error(403, None, "<html>nope</html>", "https://x/y");
        assert_eq!(error.code(), "upstream_status");
        assert!(!error.is_retryable());
    }

    #[test]
    fn classifies_forbid_code_from_body_only() {
        let error = classify_http_error(403, None, FORBID_BODY, "https://x/y");
        assert_eq!(error.code(), "upstream_blocked");
    }

    #[test]
    fn keeps_5xx_retryable() {
        let error = classify_http_error(503, None, "", "https://x/y");
        assert_eq!(error.code(), "upstream_status");
        assert!(error.is_retryable());
    }

    #[test]
    fn recognizes_agents_the_cdn_blocks() {
        for ua in [
            "curl/8.7.1",
            "Wget/1.21",
            "python-requests/2.31",
            "Go-http-client/1.1",
            "Java/17",
            "Scrapy/2.11",
            "okhttp/4.9",
            "",
        ] {
            assert!(looks_like_blocked_agent(ua), "{ua} 应被判为会被拦截");
        }
        for ua in [
            "weather-mcp-cn/0.1.0 (+https://github.com/HELPMEEADICE/weather-mcp-cn)",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64)",
        ] {
            assert!(!looks_like_blocked_agent(ua), "{ua} 应放行");
        }
    }

    /// 超限的响应体必须被拒绝，而不是先完整读进内存。
    ///
    /// 用本地 TCP 监听器回放一个没有 `Content-Length`、只有连接关闭分隔的超大响应，
    /// 从而覆盖「无法靠声明长度提前拒绝」的那条路径。整个用例不访问外网。
    #[tokio::test]
    async fn rejects_oversized_response_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("应能监听本地端口");
        let addr = listener.local_addr().expect("本地地址");

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("接受连接");
            let mut scratch = [0u8; 2048];
            let _ = socket.read(&mut scratch).await;
            let head = b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\r\n";
            if socket.write_all(head).await.is_err() {
                return;
            }
            let chunk = vec![b'a'; 64 * 1024];
            for _ in 0..(MAX_BODY_BYTES / chunk.len() + 4) {
                if socket.write_all(&chunk).await.is_err() {
                    break;
                }
            }
            let _ = socket.shutdown().await;
        });

        let config = Config {
            max_retries: 0,
            ..Default::default()
        };
        let client = HttpClient::new(&config).expect("构建客户端");
        let url = format!("http://{addr}/huge");
        let err = client
            .get_text(&url, None)
            .await
            .expect_err("超过上限必须报错");
        assert_eq!(err.code(), "parse_error", "超限属于响应内容不可用：{err}");
        assert!(err.to_string().contains("字节上限"), "{err}");
        assert!(!err.is_retryable(), "超限重试没有意义");

        server.await.expect("服务端任务不应 panic");
    }

    /// 同主机的第二个请求必须等到第一个的最小间隔之后才能进入发送区。
    #[tokio::test]
    async fn host_limiter_serializes_the_same_host() {
        let limiter = HostLimiter::new(Duration::from_millis(120));
        let mut first = limiter.enter("a.example").await;
        first.anchor();
        drop(first);

        let started = Instant::now();
        let mut second = limiter.enter("a.example").await;
        let waited = started.elapsed();
        second.anchor();

        assert!(
            waited >= Duration::from_millis(100),
            "同主机的第二个请求必须等到最小间隔之后，实际只等了 {waited:?}"
        );
    }

    /// 不同主机互不排队：限速是「每主机」的，不是全局串行。
    #[tokio::test]
    async fn host_limiter_schedules_hosts_independently() {
        let limiter = HostLimiter::new(Duration::from_millis(200));
        let started = Instant::now();
        let (_a, _b) = tokio::join!(limiter.enter("a.example"), limiter.enter("b.example"));
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "不同主机不应互相排队，实际耗时 {:?}",
            started.elapsed()
        );
    }

    /// 上限裁剪只能保留前缀，不能把整块 chunk 都塞进去。
    #[tokio::test]
    async fn read_body_capped_keeps_only_the_prefix() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("应能监听本地端口");
        let addr = listener.local_addr().expect("本地地址");

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("接受连接");
            let mut scratch = [0u8; 2048];
            let _ = socket.read(&mut scratch).await;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\n")
                .await;
            // 故意用一个大 chunk 越过上限。
            let _ = socket.write_all(&vec![b'x'; 64 * 1024]).await;
            let _ = socket.shutdown().await;
        });

        let config = Config {
            max_retries: 0,
            ..Default::default()
        };
        let client = HttpClient::new(&config).expect("构建客户端");
        let url = format!("http://{addr}/prefix");

        // 直接复用成功路径的 4 MiB 上限不方便，这里用错误路径的 32 KiB 上限验证前缀语义。
        let mut response = client
            .client
            .get(&url)
            .send()
            .await
            .expect("本地服务应可访问");
        let read = read_body_capped(&mut response, 1024).await;
        assert_eq!(read.bytes.len(), 1024, "只保留上限内的前缀");
        assert!(
            matches!(read.failure, Some(BodyFailure::TooLarge { limit, .. }) if limit == 1024),
            "超限必须被记录"
        );

        server.await.expect("服务端任务不应 panic");
    }
}
