//! 统一的 HTTP 访问层。
//!
//! 职责：
//! * 复用连接池（同一个 [`reqwest::Client`]）；
//! * 连接超时 + 整体超时；
//! * 全局并发上限（信号量）与「同一目标最小请求间隔」（礼貌抓取）；
//! * 有上限的指数退避重试（只对超时 / 429 / 5xx 重试）；
//! * 正确解码 UTF-8 与 GBK/GB18030 网页。
//!
//! 本项目不做任何访问控制规避：不换代理池、不伪造验证码、不并发轰炸单一站点。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;
use tracing::{debug, warn};

use crate::config::Config;
use crate::error::{Result, WeatherError};

/// 带限速与重试的 HTTP 客户端。
pub struct HttpClient {
    client: reqwest::Client,
    semaphore: Semaphore,
    throttle: Mutex<HashMap<String, Instant>>,
    min_interval: Duration,
    max_retries: u32,
}

impl HttpClient {
    /// 依据配置构建客户端。
    pub fn new(config: &Config) -> Result<Self> {
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
            throttle: Mutex::new(HashMap::new()),
            min_interval: config.min_request_interval,
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

            self.wait_for_slot(&host).await;

            let _permit = self
                .semaphore
                .acquire()
                .await
                .map_err(|_| WeatherError::Upstream("HTTP 并发控制已关闭".to_string()))?;

            let mut request = self.client.get(url).header("Accept", "*/*");
            if let Some(referer) = referer {
                request = request.header("Referer", referer);
            }

            let response = match request.send().await {
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
            if status.is_success() {
                let content_type = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                let bytes = match response.bytes().await {
                    Ok(bytes) => bytes,
                    Err(err) => {
                        let mapped = WeatherError::from(err);
                        let retryable = mapped.is_retryable();
                        warn!(url, attempt, error = %mapped, "读取响应体失败");
                        last_error = Some(mapped);
                        if retryable && attempt + 1 < attempts {
                            continue;
                        }
                        break;
                    }
                };
                return Ok(decode_body(&bytes, content_type.as_deref()));
            }

            let code = status.as_u16();
            let error = WeatherError::UpstreamStatus {
                status: code,
                url: url.to_string(),
            };
            warn!(url, status = code, attempt, "上游返回非 2xx");
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

    /// 等待到允许访问该主机的时刻，并预占下一个时间片。
    async fn wait_for_slot(&self, host: &str) {
        let now = Instant::now();
        let wait = {
            let Ok(mut guard) = self.throttle.lock() else {
                return;
            };
            let next = guard.get(host).copied().unwrap_or(now);
            let slot = if next > now { next } else { now };
            guard.insert(host.to_string(), slot + self.min_interval);
            slot.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    // 去掉端口，同一站点不同端口共享限速槽位。
    host.split(':').next().unwrap_or(host).to_string()
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
}
