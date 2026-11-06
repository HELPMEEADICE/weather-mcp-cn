//! HTTP 层的可靠性回归测试：**所有**响应路径都必须有界读取。
//!
//! 全部使用本地回放的假 HTTP 服务器，不访问外网，也不对公共站点做任何压力测试。
//! 服务端会记录「真正写出去的字节数」，因此可以直接断言客户端是否及时停止读取：
//!
//! * 旧实现在非 2xx 路径上调用 `response.text()`，会把整个响应体读进内存；
//! * 一旦上游故障返回一个 16 MB 的 403/5xx 页面，内存占用就随响应体线性增长。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use weather_mcp_cn::config::Config;
use weather_mcp_cn::http::HttpClient;

/// 服务器要回放的响应体。
enum Body {
    /// 固定响应体，带 `Content-Length`。
    Fixed(Vec<u8>),
    /// 不声明长度、由连接关闭分隔的流式响应体。
    Stream { chunk: usize, total: usize },
    /// 分块传输（`Transfer-Encoding: chunked`）。
    Chunked { chunk: usize, total: usize },
    /// 声明一个比实际大得多的 `Content-Length`，只写出一小段就断开。
    Truncated { declared: usize, sent: usize },
}

struct Reply {
    status: &'static str,
    content_type: &'static str,
    extra_headers: Vec<(&'static str, &'static str)>,
    body: Body,
}

struct Server {
    addr: SocketAddr,
    written: Arc<AtomicUsize>,
}

impl Server {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn written(&self) -> usize {
        self.written.load(Ordering::SeqCst)
    }
}

async fn spawn(reply: Reply) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("应能监听本地端口");
    let addr = listener.local_addr().expect("本地地址");
    let written = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&written);

    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        // 读掉请求头：不读就会在写响应前被客户端判为协议错误。
        let mut scratch = [0u8; 1024];
        let mut head = Vec::new();
        loop {
            match socket.read(&mut scratch).await {
                Ok(0) => break,
                Ok(read) => {
                    head.extend_from_slice(&scratch[..read]);
                    if head.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(_) => return,
            }
        }

        let mut out = format!(
            "HTTP/1.1 {}\r\nContent-Type: {}\r\n",
            reply.status, reply.content_type
        );
        for (name, value) in &reply.extra_headers {
            out.push_str(name);
            out.push_str(": ");
            out.push_str(value);
            out.push_str("\r\n");
        }
        match &reply.body {
            Body::Fixed(body) => out.push_str(&format!("Content-Length: {}\r\n", body.len())),
            Body::Stream { .. } => {}
            Body::Chunked { .. } => out.push_str("Transfer-Encoding: chunked\r\n"),
            Body::Truncated { declared, .. } => {
                out.push_str(&format!("Content-Length: {declared}\r\n"));
            }
        }
        out.push_str("Connection: close\r\n\r\n");
        if socket.write_all(out.as_bytes()).await.is_err() {
            return;
        }

        match reply.body {
            Body::Fixed(body) => {
                let _ = socket.write_all(&body).await;
            }
            Body::Stream { chunk, total } => {
                let payload = vec![b'a'; chunk];
                let mut sent = 0usize;
                while sent < total {
                    if socket.write_all(&payload).await.is_err() {
                        break;
                    }
                    sent += payload.len();
                    counter.fetch_add(payload.len(), Ordering::SeqCst);
                }
            }
            Body::Chunked { chunk, total } => {
                let payload = vec![b'a'; chunk];
                let mut sent = 0usize;
                while sent < total {
                    let header = format!("{:x}\r\n", payload.len());
                    if socket.write_all(header.as_bytes()).await.is_err() {
                        return;
                    }
                    if socket.write_all(&payload).await.is_err() {
                        return;
                    }
                    if socket.write_all(b"\r\n").await.is_err() {
                        return;
                    }
                    sent += payload.len();
                    counter.fetch_add(payload.len(), Ordering::SeqCst);
                }
                let _ = socket.write_all(b"0\r\n\r\n").await;
            }
            Body::Truncated { sent, .. } => {
                let _ = socket.write_all(&vec![b'a'; sent]).await;
            }
        }
        let _ = socket.shutdown().await;
    });

    Server { addr, written }
}

fn client() -> HttpClient {
    let config = Config {
        max_retries: 0,
        request_timeout: Duration::from_secs(10),
        min_request_interval: Duration::ZERO,
        ..Default::default()
    };
    HttpClient::new(&config).expect("构建客户端")
}

/// 记录**真实到达时刻**的多连接服务器，并且可以扣住前 N 个响应。
///
/// 扣住前 N 个响应的用途：让前 N 个请求长期占满并发许可，然后**一次性**放行，
/// 复现「许可集中释放」的时序。
struct MultiServer {
    addr: SocketAddr,
    arrivals: Arc<Mutex<Vec<Instant>>>,
    peak: Arc<AtomicUsize>,
    gate: Arc<tokio::sync::Semaphore>,
    held: usize,
}

impl MultiServer {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// 按到达时间排序后的时间戳。
    fn arrivals(&self) -> Vec<Instant> {
        let mut arrivals = self.arrivals.lock().expect("锁未中毒").clone();
        arrivals.sort();
        arrivals
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }

    /// 一次性放行所有被扣住的响应（= 并发许可在同一瞬间释放）。
    fn release_held(&self) {
        self.gate.add_permits(self.held);
    }
}

async fn spawn_multi(held: usize) -> MultiServer {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("应能监听本地端口");
    let addr = listener.local_addr().expect("本地地址");
    let arrivals = Arc::new(Mutex::new(Vec::new()));
    let peak = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let order = Arc::new(AtomicUsize::new(0));

    let state = (
        Arc::clone(&arrivals),
        Arc::clone(&peak),
        Arc::clone(&active),
        Arc::clone(&gate),
        Arc::clone(&order),
    );

    tokio::spawn(async move {
        let (arrivals, peak, active, gate, order) = state;
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let arrivals = Arc::clone(&arrivals);
            let peak = Arc::clone(&peak);
            let active = Arc::clone(&active);
            let gate = Arc::clone(&gate);
            let order = Arc::clone(&order);
            tokio::spawn(async move {
                let mut scratch = [0u8; 1024];
                let mut head = Vec::new();
                loop {
                    match socket.read(&mut scratch).await {
                        Ok(0) => break,
                        Ok(read) => {
                            head.extend_from_slice(&scratch[..read]);
                            if head.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => return,
                    }
                }

                // 请求头到齐的时刻就是客户端真正把请求发出来的时刻。
                arrivals.lock().expect("锁未中毒").push(Instant::now());
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(current, Ordering::SeqCst);

                if order.fetch_add(1, Ordering::SeqCst) < held
                    && let Ok(permit) = gate.acquire().await
                {
                    permit.forget();
                }

                let body = "{}";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
                active.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });

    MultiServer {
        addr,
        arrivals,
        peak,
        gate,
        held,
    }
}

/// 有上限地等待一个条件成立，避免测试挂死。
async fn wait_until(mut predicate: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(Instant::now() < deadline, "等待 {what} 超时");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// 核心时序用例：4 个请求长期占满并发许可，其余请求排队，随后许可**集中释放**。
///
/// 旧实现先在 `wait_for_slot()` 里预约一个时间片、再去抢信号量，于是排队者早在
/// 预约时刻上把等待耗完了；许可一放行，它们就在同一瞬间把请求打出去，
/// 同主机的真实发送间隔被压到接近 0。
///
/// 这里断言的是「服务端观测到的真实到达间隔」，而不是客户端内部记的预约时刻。
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn same_host_sends_keep_the_min_interval_when_permits_are_released_together() {
    const MIN_INTERVAL: Duration = Duration::from_millis(100);
    const MAX_CONCURRENCY: usize = 4;
    const REQUESTS: usize = 10;

    let server = spawn_multi(MAX_CONCURRENCY).await;
    let config = Config {
        max_concurrency: MAX_CONCURRENCY,
        min_request_interval: MIN_INTERVAL,
        max_retries: 0,
        request_timeout: Duration::from_secs(30),
        ..Default::default()
    };
    let client = Arc::new(HttpClient::new(&config).expect("构建客户端"));
    let url = server.url("/slow");
    let start = Instant::now();

    let mut tasks = Vec::new();
    for _ in 0..REQUESTS {
        let client = Arc::clone(&client);
        let url = url.clone();
        tasks.push(tokio::spawn(
            async move { client.get_text(&url, None).await },
        ));
    }

    // 1) 前 4 个请求抵达并占满并发许可（它们的响应被服务器扣住）。
    wait_until(
        || server.arrivals().len() >= MAX_CONCURRENCY,
        "前 4 个请求抵达服务端",
    )
    .await;

    // 2) 等其余请求把自己的「预约时刻」全部耗完并堵在许可队列里。
    //    旧的预约序列是 0、1、2、… 个最小间隔，因此 REQUESTS + 2 个间隔之后，
    //    所有排队者都已经越过自己的预约时刻。
    let drain = start + MIN_INTERVAL * (REQUESTS as u32 + 2);
    while Instant::now() < drain {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // 3) 一次性放行 4 个响应 → 4 个并发许可在同一瞬间释放。
    server.release_held();

    for task in tasks {
        let result = tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .expect("请求不应超时")
            .expect("任务不应 panic");
        result.expect("本地服务应返回 200");
    }

    let arrivals = server.arrivals();
    assert_eq!(arrivals.len(), REQUESTS, "每个请求都必须真正抵达服务端");
    let gaps: Vec<Duration> = arrivals.windows(2).map(|pair| pair[1] - pair[0]).collect();
    let min_gap = gaps.iter().copied().min().expect("至少有两个请求");

    // 容差只覆盖线程调度与计时器精度（本地回环 + 毫秒级计时），
    // 不放宽到「接近 0」—— 旧实现被压穿后间隔是微秒级。
    let tolerance = Duration::from_millis(25);
    assert!(
        min_gap + tolerance >= MIN_INTERVAL,
        "同一主机的真实发送间隔必须满足最小间隔：最小观测 {min_gap:?}，全部间隔 {gaps:?}"
    );

    let peak = server.peak();
    assert!(
        peak <= MAX_CONCURRENCY,
        "服务端观测到的并发 {peak} 超过配置上限 {MAX_CONCURRENCY}"
    );
}

/// 32 MB 的 403 页面：错误响应体也必须有读取上限。
///
/// 断言口径：客户端**返回的那一刻**服务端写出去多少字节。
/// * 旧实现会先把整个响应体读进内存，所以此刻服务端已经写满 `TOTAL`；
/// * 有界读取会在 32 KiB 处停下，此刻服务端只写出了「内核 socket 缓冲区能吸收的
///   那一小部分」。这个量取决于操作系统的收发缓冲区大小（本机实测几 MB），
///   所以阈值取 `TOTAL / 2`，两边都留有充足余量。
#[tokio::test]
async fn oversized_403_body_is_read_with_a_cap() {
    const TOTAL: usize = 32 * 1024 * 1024;

    let server = spawn(Reply {
        status: "403 Forbidden",
        content_type: "text/html",
        extra_headers: vec![],
        body: Body::Stream {
            chunk: 64 * 1024,
            total: TOTAL,
        },
    })
    .await;

    let err = client()
        .get_text(&server.url("/blocked"), None)
        .await
        .expect_err("403 必须报错");

    let written = server.written();
    assert_eq!(err.code(), "upstream_status", "{err}");
    assert!(!err.is_retryable(), "无 Forbid_code 的 403 不该重试：{err}");
    assert!(
        written < TOTAL / 2,
        "错误响应体必须被有界读取：客户端返回时服务端已写出 {written} 字节（共 {TOTAL} 字节）"
    );
}

/// 分块传输的 500 页面同样必须有界。
#[tokio::test]
async fn oversized_chunked_500_body_is_read_with_a_cap() {
    const TOTAL: usize = 32 * 1024 * 1024;

    let server = spawn(Reply {
        status: "500 Internal Server Error",
        content_type: "text/html",
        extra_headers: vec![],
        body: Body::Chunked {
            chunk: 16 * 1024,
            total: TOTAL,
        },
    })
    .await;

    let err = client()
        .get_text(&server.url("/boom"), None)
        .await
        .expect_err("500 必须报错");

    let written = server.written();
    assert_eq!(err.code(), "upstream_status", "{err}");
    assert!(err.is_retryable(), "5xx 应可重试：{err}");
    assert!(
        written < TOTAL / 2,
        "分块错误响应体必须被有界读取：客户端返回时服务端已写出 {written} 字节（共 {TOTAL} 字节）"
    );
}

/// 错误响应体读到一半就断开：不能静默变成「一个没有任何说明的状态码错误」，
/// 也不能丢掉已经读到的前缀（CDN `Forbid_code` 常常就在前几百字节里）。
#[tokio::test]
async fn truncated_error_body_is_reported_not_swallowed() {
    let server = spawn(Reply {
        status: "500 Internal Server Error",
        content_type: "text/html",
        extra_headers: vec![],
        body: Body::Truncated {
            declared: 512 * 1024,
            sent: 512,
        },
    })
    .await;

    let err = client()
        .get_text(&server.url("/half"), None)
        .await
        .expect_err("500 必须报错");

    assert_eq!(err.code(), "upstream_status", "{err}");
    assert!(err.is_retryable(), "5xx 仍应可重试：{err}");
    let text = err.to_string();
    assert!(
        text.contains("未能完整读取"),
        "响应体读取失败必须写进诊断，不能吞掉异常：{text}"
    );
}

/// 正常的 CDN 拦截页（体积很小）仍要能识别出 `Forbid_code`，不能被上限改造破坏。
#[tokio::test]
async fn cdn_forbid_page_keeps_its_classification() {
    let body = "<html><body><h1>403 Forbidden</h1>\
<p>Node_info: 3034-CACHE8</p><p>Client_ip: 1.2.3.4</p><p>Forbid_code: 020200</p>\
<p>Hit-status: MISS</p></body></html>"
        .as_bytes()
        .to_vec();

    let server = spawn(Reply {
        status: "403 Forbidden",
        content_type: "text/html",
        extra_headers: vec![("X-CCDN-FORBID-CODE", "020200")],
        body: Body::Fixed(body.clone()),
    })
    .await;

    let err = client()
        .get_text(&server.url("/cdn"), None)
        .await
        .expect_err("403 必须报错");
    assert_eq!(err.code(), "upstream_blocked", "{err}");
    assert!(err.to_string().contains("020200"), "{err}");

    // 只有响应体、没有响应头时也要能认出来。
    let server = spawn(Reply {
        status: "403 Forbidden",
        content_type: "text/html",
        extra_headers: vec![],
        body: Body::Fixed(body),
    })
    .await;

    let err = client()
        .get_text(&server.url("/cdn-body-only"), None)
        .await
        .expect_err("403 必须报错");
    assert_eq!(err.code(), "upstream_blocked", "{err}");
}
