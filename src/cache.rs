//! 线程安全的 **LRU + TTL** 缓存。
//!
//! 为什么不引第三方缓存库：本项目需要的语义只有三件事——「键 → 值 + 过期时间」、
//! 「容量上限 + 按最近使用顺序淘汰」、以及「同一键并发未命中时只打一次上游」
//! （singleflight）。现成的缓存库通常自带定时器、后台清理线程和异步任务，
//! 与「单文件、低内存、无后台线程」的约束冲突，所以这里是手写实现。
//!
//! # 数据结构
//!
//! * `nodes` 是槽位数组，每个槽位是一个「双向链表节点 + 键值对」；删除后的槽位
//!   进入 `free` 空闲列表被复用，所以增删都不需要搬移其它元素；
//! * `index` 把键映射到槽位下标；
//! * `head` 是**最近使用**的槽位，`tail` 是**最久未使用**的槽位；
//! * `get` 命中会把节点移到表头，`insert` 溢出时从表尾淘汰 —— 这才是真正的 LRU，
//!   而不是「按过期时间远近挑一个删掉」。
//!
//! 所有链表操作都是 O(1)：不复制键、不做全表扫描、不引入额外分配。只有「写满容量
//! 之后仍要插入」时才会做一次 O(n) 的过期清理（先清死条目，再退化为淘汰 LRU 尾节点）。
//!
//! # 并发与取消
//!
//! * 内部状态由 `std::sync::Mutex` 保护，**临界区里不 await**，锁不会跨越挂起点；
//! * singleflight 用 `tokio::sync::oneshot` 唤醒等待者，leader 的 future 被 drop
//!   （客户端取消、超时）时由 [`FlightGuard`] 负责清理 inflight 表并唤醒等待者，
//!   等待者会重新回查缓存或自己成为新的 leader，因此不会死锁；
//! * 只有 `Ok` 结果会写进缓存，失败结果不缓存 —— 一次网络抖动不会被固化成一整段
//!   时间内的错误。

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

/// 等待 leader 结束（并回查缓存）的最大轮次。
///
/// 只有在「leader 连续被取消」这种极端情况下才会走到第二轮：正常情况下等待者被
/// 唤醒时缓存里已经有值了。给一个上限是为了避免任何形式的活锁。
const MAX_COOPERATIVE_ATTEMPTS: usize = 3;

/// 取锁，并在锁中毒时继续使用内部数据。
///
/// 缓存不承载任何跨字段不变量，一次持锁期间的 panic 不会让数据变得危险；
/// 相比「中毒即永久失效」，恢复使用更符合缓存的本意。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Node<K, V> {
    key: K,
    value: V,
    expires_at: Instant,
    prev: Option<usize>,
    next: Option<usize>,
}

struct Inner<K, V> {
    nodes: Vec<Option<Node<K, V>>>,
    index: HashMap<K, usize>,
    free: Vec<usize>,
    head: Option<usize>,
    tail: Option<usize>,
    len: usize,
}

impl<K, V> Default for Inner<K, V> {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            index: HashMap::new(),
            free: Vec::new(),
            head: None,
            tail: None,
            len: 0,
        }
    }
}

impl<K, V> Inner<K, V>
where
    K: Eq + Hash + Clone,
{
    /// 把槽位挂到表头（最近使用）。
    fn push_front(&mut self, idx: usize) {
        let old_head = self.head;
        if let Some(node) = self.nodes[idx].as_mut() {
            node.prev = None;
            node.next = old_head;
        }
        match old_head {
            Some(head) => {
                if let Some(node) = self.nodes[head].as_mut() {
                    node.prev = Some(idx);
                }
            }
            None => self.tail = Some(idx),
        }
        self.head = Some(idx);
    }

    /// 从链表里摘掉槽位，但保留槽位内容。
    fn unlink(&mut self, idx: usize) {
        let Some((prev, next)) = self.nodes[idx].as_ref().map(|node| (node.prev, node.next)) else {
            return;
        };
        match prev {
            Some(prev) => {
                if let Some(node) = self.nodes[prev].as_mut() {
                    node.next = next;
                }
            }
            None => self.head = next,
        }
        match next {
            Some(next) => {
                if let Some(node) = self.nodes[next].as_mut() {
                    node.prev = prev;
                }
            }
            None => self.tail = prev,
        }
        if let Some(node) = self.nodes[idx].as_mut() {
            node.prev = None;
            node.next = None;
        }
    }

    /// 标记为最近使用。
    fn touch(&mut self, idx: usize) {
        if self.head == Some(idx) {
            return;
        }
        self.unlink(idx);
        self.push_front(idx);
    }

    /// 彻底删除一个槽位（同时从索引和链表里摘掉，槽位回收进 `free`）。
    fn remove(&mut self, idx: usize) -> Option<Node<K, V>> {
        // 必须先把节点从链表里摘掉，再取走槽位内容：`unlink` 需要读这个节点的
        // prev / next，顺序颠倒会让 head / tail 留下悬空下标。
        if self.nodes.get(idx)?.is_none() {
            return None;
        }
        self.unlink(idx);
        let node = self.nodes.get_mut(idx)?.take()?;
        self.index.remove(&node.key);
        self.free.push(idx);
        self.len = self.len.saturating_sub(1);
        Some(node)
    }

    /// 插入一个全新键值对（调用方保证该键不在索引里，且已有空余容量）。
    fn push_node(&mut self, key: K, value: V, expires_at: Instant) {
        let node = Node {
            key: key.clone(),
            value,
            expires_at,
            prev: None,
            next: None,
        };
        let idx = match self.free.pop() {
            Some(idx) => {
                self.nodes[idx] = Some(node);
                idx
            }
            None => {
                self.nodes.push(Some(node));
                self.nodes.len() - 1
            }
        };
        self.index.insert(key, idx);
        self.push_front(idx);
        self.len += 1;
    }

    /// 淘汰最久未使用的条目。
    fn evict_lru(&mut self) -> bool {
        match self.tail {
            Some(idx) => self.remove(idx).is_some(),
            None => false,
        }
    }

    /// 清掉所有已过期条目，返回删除数量。
    fn sweep_expired(&mut self, now: Instant) -> usize {
        let mut removed = 0;
        // 槽位下标在生命周期内保持稳定（`nodes` 只增长、槽位只换内容），
        // 因此可以在遍历过程中安全删除当前下标对应的槽位。
        for idx in 0..self.nodes.len() {
            let expired = self.nodes[idx]
                .as_ref()
                .is_some_and(|node| node.expires_at <= now);
            if expired && self.remove(idx).is_some() {
                removed += 1;
            }
        }
        removed
    }

    /// 校验「索引 ↔ 双向链表 ↔ 槽位」三者的内部一致性（测试用）。
    ///
    /// 这条不变量是最容易写错的地方：head/tail 悬空、链表成环、索引指向空槽位，
    /// 都会让淘汰顺序悄悄失效而不报错。所以测试里每次结构变动后都验一遍。
    #[cfg(test)]
    fn assert_consistent(&self) {
        let mut seen = 0usize;
        let mut prev: Option<usize> = None;
        let mut cursor = self.head;
        while let Some(idx) = cursor {
            let node = self
                .nodes
                .get(idx)
                .and_then(Option::as_ref)
                .unwrap_or_else(|| panic!("head 链指向空槽位 {idx}"));
            assert_eq!(node.prev, prev, "槽位 {idx} 的 prev 链接不一致");
            assert_eq!(
                self.index.get(&node.key),
                Some(&idx),
                "索引与链表不一致（槽位 {idx}）"
            );
            seen += 1;
            assert!(seen <= self.len + 1, "正向链表出现环");
            prev = Some(idx);
            cursor = node.next;
        }
        assert_eq!(seen, self.len, "链表长度与 len 不一致");
        assert_eq!(prev, self.tail, "tail 不是链尾");

        let mut back = 0usize;
        let mut cursor = self.tail;
        while let Some(idx) = cursor {
            back += 1;
            assert!(back <= self.len + 1, "反向链表出现环");
            cursor = self.nodes[idx].as_ref().and_then(|node| node.prev);
        }
        assert_eq!(back, seen, "反向遍历长度与正向不一致");

        assert_eq!(self.index.len(), self.len, "索引条目数与 len 不一致");
        assert_eq!(
            self.free.len() + self.len,
            self.nodes.len(),
            "空闲槽位 + 有效槽位应等于槽位总数"
        );
    }
}

/// 一次「同键并发未命中」的协调对象。
#[derive(Default)]
struct Flight {
    state: Mutex<FlightState>,
}

#[derive(Default)]
struct FlightState {
    done: bool,
    waiters: Vec<oneshot::Sender<()>>,
}

impl Flight {
    /// 注册一个等待者。
    ///
    /// `done` 判定与注册在同一把锁里完成，因此不存在「leader 刚好在注册前后完成，
    /// 等待者永远收不到通知」的竞态。
    fn wait(&self) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        let mut state = lock(&self.state);
        if state.done {
            let _ = tx.send(());
        } else {
            state.waiters.push(tx);
        }
        rx
    }

    /// 唤醒所有等待者（重复调用无副作用）。
    fn complete(&self) {
        let mut state = lock(&self.state);
        state.done = true;
        for waiter in state.waiters.drain(..) {
            let _ = waiter.send(());
        }
    }
}

/// leader 的清理守卫：无论正常结束、报错还是被取消，都会注销 inflight 并唤醒等待者。
struct FlightGuard<'a, K: Eq + Hash + Clone, V: Clone> {
    cache: &'a TtlCache<K, V>,
    key: K,
    flight: Arc<Flight>,
}

impl<K: Eq + Hash + Clone, V: Clone> Drop for FlightGuard<'_, K, V> {
    fn drop(&mut self) {
        // `Drop` 的约束必须与结构体一致，所以注销逻辑放在 `TtlCache` 的方法里。
        self.cache.finish_flight(&self.key, &self.flight);
    }
}

/// 带 TTL 与容量上限的 LRU 缓存。
pub struct TtlCache<K, V> {
    ttl: Duration,
    capacity: usize,
    inner: Mutex<Inner<K, V>>,
    inflight: Mutex<HashMap<K, Arc<Flight>>>,
}

impl<K, V> TtlCache<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    /// 创建一个 TTL 为 `ttl`、最多容纳 `capacity` 项的缓存。
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            ttl,
            capacity: capacity.max(1),
            inner: Mutex::new(Inner::default()),
            inflight: Mutex::new(HashMap::new()),
        }
    }

    /// 缓存的默认 TTL。
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// 容量上限。
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// 读取缓存；命中会把它标记为「最近使用」。
    ///
    /// 顺带把当前这个键的过期条目删掉，避免死数据长期占着容量。
    pub fn get(&self, key: &K) -> Option<V> {
        let now = Instant::now();
        let mut inner = lock(&self.inner);
        let idx = *inner.index.get(key)?;
        let alive = inner.nodes[idx]
            .as_ref()
            .is_some_and(|node| node.expires_at > now);
        if !alive {
            inner.remove(idx);
            return None;
        }
        let value = inner.nodes[idx].as_ref().map(|node| node.value.clone())?;
        inner.touch(idx);
        Some(value)
    }

    /// 写入缓存（覆盖同键旧值并按默认 TTL 续期）。
    pub fn insert(&self, key: K, value: V) {
        self.insert_with_ttl(key, value, self.ttl);
    }

    /// 用指定 TTL 写入缓存。
    ///
    /// 主要给测试与「个别条目需要更短生命周期」的场景使用；常规写入请用
    /// [`TtlCache::insert`]。
    pub fn insert_with_ttl(&self, key: K, value: V, ttl: Duration) {
        let now = Instant::now();
        let expires_at = now + ttl;
        let mut inner = lock(&self.inner);

        // 只有写满容量时才需要付出 O(n)：先清掉已经过期的死条目，
        // 这样「有数据过期」的情况下不会误伤仍然有效的条目。
        if inner.len >= self.capacity {
            inner.sweep_expired(now);
        }

        if let Some(&idx) = inner.index.get(&key) {
            if let Some(node) = inner.nodes[idx].as_mut() {
                node.value = value;
                node.expires_at = expires_at;
            }
            inner.touch(idx);
            return;
        }

        while inner.len >= self.capacity {
            if !inner.evict_lru() {
                break;
            }
        }
        inner.push_node(key, value, expires_at);
    }

    /// 命中缓存则直接返回，否则执行 `load` 并写入缓存。
    ///
    /// 同一键的并发未命中会被合并成一次 `load`：第一个调用者成为 leader，其余调用者
    /// 等待 leader 结束后回查缓存。leader 失败或被取消时不会写入缓存，等待者会自己
    /// 接手重试（最多 [`MAX_COOPERATIVE_ATTEMPTS`] 轮）。
    ///
    /// 只有 `Ok` 结果会进入缓存。
    pub async fn get_or_try_insert_with<F, Fut, E>(&self, key: K, load: F) -> Result<V, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, E>>,
        E: Clone,
    {
        let mut pending = Some(load);

        for _ in 0..MAX_COOPERATIVE_ATTEMPTS {
            if let Some(hit) = self.get(&key) {
                return Ok(hit);
            }

            let (flight, is_leader) = self.join(key.clone());
            if is_leader {
                let guard = FlightGuard {
                    cache: self,
                    key: key.clone(),
                    flight: Arc::clone(&flight),
                };
                let load = pending.take().expect("只有 leader 会消耗加载闭包");
                let outcome = load().await;
                if let Ok(value) = &outcome {
                    self.insert(key, value.clone());
                }
                drop(guard);
                return outcome;
            }

            // 跟随者：leader 正常结束、报错或被取消都会唤醒我们。
            let _ = flight.wait().await;
        }

        // 极端情况下的兜底：不再协作，自己打一次上游。
        let load = pending.take().expect("兜底分支最多执行一次");
        load().await
    }

    /// 加入已有 flight 或新建一个；返回 `(flight, 是否成为 leader)`。
    fn join(&self, key: K) -> (Arc<Flight>, bool) {
        let mut inflight = lock(&self.inflight);
        if let Some(flight) = inflight.get(&key) {
            return (Arc::clone(flight), false);
        }
        let flight = Arc::new(Flight::default());
        inflight.insert(key, Arc::clone(&flight));
        (flight, true)
    }

    /// 注销某个 flight 并唤醒它的等待者。
    ///
    /// 先摘表再唤醒：被唤醒的等待者如果发现缓存里没有值，会自己成为新的 leader，
    /// 而不是加入一个已经结束的 flight。
    fn finish_flight(&self, key: &K, flight: &Arc<Flight>) {
        {
            let mut inflight = lock(&self.inflight);
            let mine = inflight
                .get(key)
                .is_some_and(|current| Arc::ptr_eq(current, flight));
            if mine {
                inflight.remove(key);
            }
        }
        flight.complete();
    }

    /// 当前有效条目数量（顺带清理过期条目）。
    pub fn len(&self) -> usize {
        let now = Instant::now();
        let mut inner = lock(&self.inner);
        inner.sweep_expired(now);
        inner.len
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 清空缓存内容（不打断正在进行的 singleflight）。
    pub fn clear(&self) {
        let mut inner = lock(&self.inner);
        *inner = Inner::default();
    }

    /// 正在进行中的 singleflight 数量（测试与诊断用）。
    #[cfg(test)]
    fn inflight_len(&self) -> usize {
        lock(&self.inflight).len()
    }

    /// 指定键上的等待者数量（测试用）。
    #[cfg(test)]
    fn waiter_len(&self, key: &K) -> usize {
        lock(&self.inflight)
            .get(key)
            .map(|flight| lock(&flight.state).waiters.len())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc as StdArc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn cache<K: Eq + Hash + Clone, V: Clone>(ttl: Duration, capacity: usize) -> TtlCache<K, V> {
        TtlCache::new(ttl, capacity)
    }

    /// 断言缓存的内部结构（索引、双向链表、槽位）自洽。
    fn assert_consistent<K: Eq + Hash + Clone, V: Clone>(cache: &TtlCache<K, V>) {
        lock(&cache.inner).assert_consistent();
    }

    #[test]
    fn stores_and_expires() {
        let cache: TtlCache<String, u32> = cache(Duration::from_millis(40), 8);
        cache.insert("a".into(), 1);
        assert_eq!(cache.get(&"a".to_string()), Some(1));
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(cache.get(&"a".to_string()), None);
        assert_eq!(cache.len(), 0, "过期条目不应再计入容量");
    }

    #[test]
    fn zero_ttl_is_never_returned() {
        let cache: TtlCache<u32, u32> = cache(Duration::from_secs(60), 4);
        cache.insert_with_ttl(1, 1, Duration::ZERO);
        assert_eq!(cache.get(&1), None, "TTL 为零的条目应立即失效");
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn respects_capacity() {
        let cache: TtlCache<u32, u32> = cache(Duration::from_secs(60), 2);
        cache.insert(1, 1);
        cache.insert(2, 2);
        cache.insert(3, 3);
        assert_eq!(cache.len(), 2);
        assert_consistent(&cache);
    }

    /// 真正的 LRU：被 `get` 访问过的键不会被优先淘汰。
    #[test]
    fn evicts_least_recently_used_key() {
        let cache: TtlCache<u32, u32> = cache(Duration::from_secs(60), 3);
        cache.insert(1, 10);
        cache.insert(2, 20);
        cache.insert(3, 30);
        assert_consistent(&cache);

        // 访问 1，让它从「最久未使用」变成「最近使用」。
        assert_eq!(cache.get(&1), Some(10));
        assert_consistent(&cache);

        cache.insert(4, 40);
        assert_eq!(cache.get(&2), None, "最久未使用的 2 应被淘汰");
        assert_eq!(cache.get(&1), Some(10));
        assert_eq!(cache.get(&3), Some(30));
        assert_eq!(cache.get(&4), Some(40));
        assert_eq!(cache.len(), 3);
        assert_consistent(&cache);
    }

    /// 连续多次淘汰后 LRU 顺序不能错乱（这是「链表摘除顺序写错」最容易漏掉的场景）。
    #[test]
    fn lru_order_survives_many_evictions() {
        let cache: TtlCache<u32, u32> = cache(Duration::from_secs(60), 3);
        for key in 1..=3 {
            cache.insert(key, key);
        }

        // `resident` 按「最久未使用 → 最近使用」排列。
        let mut resident = vec![1u32, 2, 3];
        for key in 4..=12u32 {
            // 访问当前最久未使用的键，把它变成最近使用。
            let touched = resident[0];
            assert_eq!(cache.get(&touched), Some(touched));
            // 于是下一个被淘汰的应该是原本的次久未使用项。
            let victim = resident[1];

            cache.insert(key, key);

            assert_eq!(cache.get(&victim), None, "第 {key} 轮应淘汰 {victim}");
            assert_eq!(cache.get(&key), Some(key));
            assert_eq!(cache.get(&touched), Some(touched));

            // 本轮的读写把顺序更新为：[剩下的旧项, key, touched]。
            resident = vec![resident[2], key, touched];
            assert_eq!(cache.len(), 3);
            assert_consistent(&cache);
        }
    }

    /// 覆盖同一个键不应引起任何淘汰，且会续期。
    #[test]
    fn overwriting_existing_key_keeps_capacity_intact() {
        let cache: TtlCache<u32, u32> = cache(Duration::from_secs(60), 2);
        cache.insert(1, 10);
        cache.insert(2, 20);
        cache.insert(1, 11);
        assert_eq!(cache.get(&1), Some(11));
        assert_eq!(cache.get(&2), Some(20));
        assert_eq!(cache.len(), 2);

        cache.insert(1, 12);
        cache.insert(3, 30);
        assert_eq!(cache.get(&2), None, "2 仍是最久未使用的键");
    }

    /// 过期条目先于有效条目被回收，不会挤掉还有效的数据。
    #[test]
    fn expired_entries_are_reclaimed_before_live_ones() {
        let cache: TtlCache<u32, u32> = cache(Duration::from_secs(60), 3);
        cache.insert(1, 10);
        cache.insert(2, 20);
        cache.insert_with_ttl(3, 30, Duration::ZERO); // 立即过期

        // 写满容量后插入新键：应先回收死条目 3，而不是淘汰 LRU 尾部的 1。
        cache.insert(4, 40);
        assert_eq!(cache.get(&1), Some(10), "有效条目不应被过期条目连累");
        assert_eq!(cache.get(&2), Some(20));
        assert_eq!(cache.get(&4), Some(40));
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn clear_drops_everything() {
        let cache: TtlCache<u32, u32> = cache(Duration::from_secs(60), 4);
        cache.insert(1, 1);
        cache.insert(2, 2);
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.get(&1), None);
    }

    #[test]
    fn capacity_is_at_least_one() {
        let cache: TtlCache<u32, u32> = cache(Duration::from_secs(60), 0);
        assert_eq!(cache.capacity(), 1);
        cache.insert(1, 1);
        cache.insert(2, 2);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(&2), Some(2));
    }

    #[tokio::test]
    async fn loads_on_miss_only() {
        let cache: TtlCache<u32, u32> = cache(Duration::from_secs(60), 4);
        let first: Result<u32, ()> = cache.get_or_try_insert_with(7, || async { Ok(41) }).await;
        assert_eq!(first, Ok(41));
        let second: Result<u32, ()> = cache.get_or_try_insert_with(7, || async { Ok(99) }).await;
        assert_eq!(second, Ok(41), "第二次应命中缓存");
    }

    #[tokio::test]
    async fn failures_are_not_cached() {
        let cache: TtlCache<u32, u32> = cache(Duration::from_secs(60), 4);
        let first = cache
            .get_or_try_insert_with(1, || async { Err::<u32, &'static str>("boom") })
            .await;
        assert_eq!(first, Err("boom"));
        assert_eq!(cache.len(), 0, "失败结果不能进缓存");

        let calls = AtomicUsize::new(0);
        let second = cache
            .get_or_try_insert_with(1, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok::<u32, &'static str>(7)
            })
            .await;
        assert_eq!(second, Ok(7));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// 三次并发未命中只应触发一次上游加载。
    #[tokio::test]
    async fn singleflight_coalesces_concurrent_misses() {
        let cache = StdArc::new(cache::<u32, u32>(Duration::from_secs(60), 8));
        let calls = StdArc::new(AtomicUsize::new(0));

        let mut tasks: Vec<tokio::task::JoinHandle<Result<u32, ()>>> = Vec::new();
        for _ in 0..3 {
            let cache = StdArc::clone(&cache);
            let calls = StdArc::clone(&calls);
            tasks.push(tokio::spawn(async move {
                cache
                    .get_or_try_insert_with(1, || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        Ok::<u32, ()>(42)
                    })
                    .await
            }));
        }

        for task in tasks {
            assert_eq!(task.await.expect("任务不应 panic"), Ok(42));
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "同一键的并发未命中应只打一次上游"
        );
    }

    /// leader 被取消（future 被 drop）时，等待者不能永久卡死。
    #[tokio::test]
    async fn cancelled_leader_lets_a_waiter_take_over() {
        let cache = StdArc::new(cache::<u32, u32>(Duration::from_secs(60), 8));

        // leader：加载过程一直挂起，直到被 abort。
        let leader: tokio::task::JoinHandle<Result<u32, ()>> = {
            let cache = StdArc::clone(&cache);
            tokio::spawn(async move {
                cache
                    .get_or_try_insert_with(1, || async {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        Ok::<u32, ()>(1)
                    })
                    .await
            })
        };

        while cache.inflight_len() == 0 {
            tokio::task::yield_now().await;
        }

        // 跟随者：排在 leader 后面等待。
        let follower: tokio::task::JoinHandle<Result<u32, ()>> = {
            let cache = StdArc::clone(&cache);
            tokio::spawn(async move {
                cache
                    .get_or_try_insert_with(1, || async { Ok::<u32, ()>(2) })
                    .await
            })
        };

        while cache.waiter_len(&1) == 0 {
            tokio::task::yield_now().await;
        }

        leader.abort();
        let _ = leader.await;

        let outcome = tokio::time::timeout(Duration::from_secs(5), follower)
            .await
            .expect("leader 被取消后等待者不应死锁")
            .expect("等待者任务不应 panic");
        assert_eq!(outcome, Ok(2), "等待者应接手并成功返回");
        assert_eq!(cache.get(&1), Some(2), "接手结果应写入缓存");
        assert_eq!(cache.inflight_len(), 0, "inflight 表必须清理干净");
    }
}
