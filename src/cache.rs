//! 线程安全的 TTL 缓存。
//!
//! 不用第三方缓存库：这里只需要「带过期时间 + 容量上限的 HashMap」，手写实现体积更小、
//! 行为更容易预测。缓存只存放已经成功解析的数据，失败结果不缓存，避免把一次网络抖动
//! 固化成一段时间内的错误。

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Slot<V> {
    value: V,
    expires_at: Instant,
}

/// 带 TTL 与容量上限的内存缓存。
pub struct TtlCache<K, V> {
    ttl: Duration,
    capacity: usize,
    inner: Mutex<HashMap<K, Slot<V>>>,
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
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// 缓存的 TTL。
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// 读取缓存，过期条目会被顺手删除。
    pub fn get(&self, key: &K) -> Option<V> {
        let mut guard = self.inner.lock().ok()?;
        match guard.get(key) {
            Some(slot) if slot.expires_at > Instant::now() => Some(slot.value.clone()),
            Some(_) => {
                guard.remove(key);
                None
            }
            None => None,
        }
    }

    /// 写入缓存。
    pub fn insert(&self, key: K, value: V) {
        let now = Instant::now();
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };
        guard.retain(|_, slot| slot.expires_at > now);
        if guard.len() >= self.capacity
            && !guard.contains_key(&key)
            && let Some(victim) = guard
                .iter()
                .min_by_key(|(_, slot)| slot.expires_at)
                .map(|(k, _)| k.clone())
        {
            guard.remove(&victim);
        }
        guard.insert(
            key,
            Slot {
                value,
                expires_at: now + self.ttl,
            },
        );
    }

    /// 命中缓存则直接返回，否则执行 `load` 并写入缓存。
    ///
    /// 只有 `Ok` 结果会进入缓存。
    pub async fn get_or_try_insert_with<F, Fut, E>(&self, key: K, load: F) -> Result<V, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, E>>,
    {
        if let Some(hit) = self.get(&key) {
            return Ok(hit);
        }
        let fresh = load().await?;
        self.insert(key, fresh.clone());
        Ok(fresh)
    }

    /// 当前有效条目数量（顺带清理过期条目）。
    pub fn len(&self) -> usize {
        let now = Instant::now();
        let Ok(mut guard) = self.inner.lock() else {
            return 0;
        };
        guard.retain(|_, slot| slot.expires_at > now);
        guard.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 清空缓存。
    pub fn clear(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_expires() {
        let cache: TtlCache<String, u32> = TtlCache::new(Duration::from_millis(40), 8);
        cache.insert("a".into(), 1);
        assert_eq!(cache.get(&"a".to_string()), Some(1));
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(cache.get(&"a".to_string()), None);
    }

    #[test]
    fn respects_capacity() {
        let cache: TtlCache<u32, u32> = TtlCache::new(Duration::from_secs(60), 2);
        cache.insert(1, 1);
        cache.insert(2, 2);
        cache.insert(3, 3);
        assert!(cache.len() <= 2);
    }

    #[tokio::test]
    async fn loads_on_miss_only() {
        let cache: TtlCache<u32, u32> = TtlCache::new(Duration::from_secs(60), 4);
        let first: Result<u32, ()> = cache.get_or_try_insert_with(7, || async { Ok(41) }).await;
        assert_eq!(first, Ok(41));
        let second: Result<u32, ()> = cache.get_or_try_insert_with(7, || async { Ok(99) }).await;
        assert_eq!(second, Ok(41), "第二次应命中缓存");
    }
}
