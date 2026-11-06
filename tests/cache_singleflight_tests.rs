//! singleflight 并发正确性的集成测试。
//!
//! 这里用**可控同步**复现「缓存检查」与「注册 flight」之间的窗口，不依赖 sleep 与
//! 碰运气式并发：
//!
//! * 请求 A 命中首次缓存检查（未命中）后，会在 `key.clone()` 处被拦停 —— 这一刻它
//!   恰好停在「已确认未命中」与「注册 flight」之间；
//! * 请求 B 完整跑完一次 leader 流程：加载、写缓存、注销 flight；
//! * 放行 A。如果 A 成为 leader 后不回查缓存，就会重复打一次上游。

use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use weather_mcp_cn::cache::TtlCache;

/// 请求 A 的停靠点。
#[derive(Default)]
struct GateState {
    a_parked: bool,
    released: bool,
}

#[derive(Default)]
struct Gate {
    /// 只有第一次带 gate 的 clone 会停下来（也就是 A 的那一次）。
    armed: AtomicBool,
    state: Mutex<GateState>,
    cv: Condvar,
}

impl Gate {
    fn armed() -> Self {
        Self {
            armed: AtomicBool::new(true),
            ..Self::default()
        }
    }

    fn is_parked(&self) -> bool {
        self.state.lock().expect("锁未中毒").a_parked
    }

    fn release(&self) {
        let mut state = self.state.lock().expect("锁未中毒");
        state.released = true;
        self.cv.notify_all();
    }

    /// 停在「已确认缓存未命中、尚未注册 flight」的窗口里，直到被放行（最多 5 秒）。
    fn park(&self) {
        let mut state = self.state.lock().expect("锁未中毒");
        state.a_parked = true;
        self.cv.notify_all();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !state.released {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (next, _) = self
                .cv
                .wait_timeout(state, deadline - now)
                .expect("锁未中毒");
            state = next;
        }
    }
}

/// 缓存键：相等性只看 `id`，`gate` 只是把停靠点塞进 `Clone` 的载体。
struct GatedKey {
    id: u32,
    gate: Option<Arc<Gate>>,
}

impl Clone for GatedKey {
    fn clone(&self) -> Self {
        if let Some(gate) = &self.gate
            && gate.armed.swap(false, Ordering::SeqCst)
        {
            gate.park();
        }
        Self {
            id: self.id,
            gate: self.gate.clone(),
        }
    }
}

impl PartialEq for GatedKey {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for GatedKey {}

impl Hash for GatedKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

/// 有上限地等待一个同步条件成立，避免测试永久挂起。
async fn wait_until(mut predicate: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < deadline, "等待 {what} 超时");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// 竞态复现：A 的「检查缓存」与「注册 flight」之间，B 完成了加载并写进缓存。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn becoming_leader_rechecks_the_cache_before_loading() {
    let cache = Arc::new(TtlCache::<GatedKey, u32>::new(Duration::from_secs(60), 8));
    let gate = Arc::new(Gate::armed());
    let calls = Arc::new(AtomicUsize::new(0));

    // A：停在首次缓存检查之后。
    let a = {
        let cache = Arc::clone(&cache);
        let calls = Arc::clone(&calls);
        let key = GatedKey {
            id: 1,
            gate: Some(Arc::clone(&gate)),
        };
        tokio::spawn(async move {
            cache
                .get_or_try_insert_with(key, || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok::<u32, &'static str>(111)
                })
                .await
        })
    };

    wait_until(|| gate.is_parked(), "A 停在缓存检查与 flight 注册之间").await;

    // B：完整跑完 leader 流程（加载 → 写缓存 → 注销 flight）。
    let b = {
        let cache = Arc::clone(&cache);
        let calls = Arc::clone(&calls);
        let key = GatedKey { id: 1, gate: None };
        tokio::spawn(async move {
            cache
                .get_or_try_insert_with(key, || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok::<u32, &'static str>(222)
                })
                .await
        })
    };
    assert_eq!(
        b.await.expect("B 不应 panic"),
        Ok(222),
        "B 应当成为 leader 并成功加载"
    );

    gate.release();

    let outcome = tokio::time::timeout(Duration::from_secs(5), a)
        .await
        .expect("A 不应挂起")
        .expect("A 不应 panic");
    assert_eq!(
        outcome,
        Ok(222),
        "成为 leader 后必须先回查缓存，直接复用 B 已经载入的结果"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "同一键只应打一次上游；旧实现会在 A 变成 leader 后重复加载"
    );
}

/// 连续失败 + 大量等待者：加载次数必须与调用数同阶，不能被放大成请求风暴。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_failure_does_not_amplify_loads() {
    const CALLERS: usize = 8;

    let cache = Arc::new(TtlCache::<&'static str, u32>::new(
        Duration::from_secs(60),
        8,
    ));
    let calls = Arc::new(AtomicUsize::new(0));

    let mut tasks = Vec::new();
    for _ in 0..CALLERS {
        let cache = Arc::clone(&cache);
        let calls = Arc::clone(&calls);
        tasks.push(tokio::spawn(async move {
            cache
                .get_or_try_insert_with("boom", || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    Err::<u32, &'static str>("上游失败")
                })
                .await
        }));
    }

    for task in tasks {
        assert_eq!(
            task.await.expect("不应 panic"),
            Err("上游失败"),
            "失败必须如实返回给每一个调用者"
        );
    }

    let loads = calls.load(Ordering::SeqCst);
    assert!(
        loads <= CALLERS + 3,
        "{CALLERS} 个并发调用者最多允许 3 轮协作 + 各自兜底，实际加载 {loads} 次"
    );
    assert_eq!(cache.len(), 0, "失败结果不得进入缓存");
}
