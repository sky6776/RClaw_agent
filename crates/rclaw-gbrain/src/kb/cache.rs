//! 搜索缓存模块 (P4-006~P4-010)
//!
//! 进程内 LRU/TTL 缓存，支持 query embedding、分词、召回结果、rerank 结果缓存。
//! 缓存 key 包含 index_version，索引变更时自动失效。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{debug, trace};

/// 缓存条目
struct CacheEntry<T> {
    value: T,
    created_at: Instant,
    last_accessed: Instant,
    ttl: Duration,
}

impl<T> CacheEntry<T> {
    fn is_expired(&self) -> bool {
        self.created_at.elapsed() > self.ttl
    }
}

/// 简单的 LRU + TTL 缓存
pub struct SearchCache<T> {
    entries: Mutex<HashMap<String, CacheEntry<T>>>,
    max_entries: usize,
    default_ttl: Duration,
}

impl<T: Clone> SearchCache<T> {
    pub fn new(max_entries: usize, ttl_secs: u64) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            max_entries,
            default_ttl: Duration::from_secs(ttl_secs),
        }
    }

    /// 获取缓存值（过期返回 None）
    pub fn get(&self, key: &str) -> Option<T> {
        let mut entries = self.entries.lock().ok()?;
        match entries.get_mut(key) {
            Some(entry) if !entry.is_expired() => {
                entry.last_accessed = Instant::now();
                trace!("cache hit: key={}", key);
                Some(entry.value.clone())
            }
            _ => {
                trace!("cache miss: key={}", key);
                entries.remove(key);
                None
            }
        }
    }

    /// 写入缓存（超过容量时淘汰最近最少使用的条目）
    pub fn set(&self, key: String, value: T) {
        if let Ok(mut entries) = self.entries.lock() {
            if let Some(entry) = entries.get_mut(&key) {
                entry.value = value;
                entry.created_at = Instant::now();
                entry.last_accessed = entry.created_at;
                entry.ttl = self.default_ttl;
                return;
            }

            if entries.len() >= self.max_entries {
                // 当前 max_entries 较小（默认 100-200），O(n) 扫描可接受。
                // TODO: 若将来 max_entries 增长至数千以上，应改用 OrderedDict 或
                // 基于时间轮的数据结构，避免每次写入都做全量扫描。
                // 淘汰最近最少使用的条目，而不是最早创建的条目。
                if let Some(oldest_key) = entries
                    .iter()
                    .min_by_key(|(_, v)| v.last_accessed)
                    .map(|(k, _)| k.clone())
                {
                    debug!(
                        "SearchCache LRU 淘汰: key={}, 当前条目数={}",
                        oldest_key,
                        entries.len()
                    );
                    entries.remove(&oldest_key);
                }
            }
            entries.insert(
                key,
                CacheEntry {
                    value,
                    created_at: Instant::now(),
                    last_accessed: Instant::now(),
                    ttl: self.default_ttl,
                },
            );
        }
    }

    /// 根据 index_version 清理陈旧缓存
    pub fn invalidate_by_prefix(&self, prefix: &str) {
        if let Ok(mut entries) = self.entries.lock() {
            let before = entries.len();
            entries.retain(|k, _| !k.starts_with(prefix));
            let removed = before - entries.len();
            if removed > 0 {
                debug!("cache invalidation: prefix={}, removed={}", prefix, removed);
            }
        }
    }

    /// 获取当前条目数
    pub fn len(&self) -> usize {
        self.entries.lock().map(|e| e.len()).unwrap_or(0)
    }

    /// 是否为空
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 构建缓存 key（必须包含 index_version）
pub fn make_cache_key(
    query_normalized: &str,
    library_ids: &[i64],
    index_version: i64,
    cache_type: &str,
) -> String {
    let libs = library_ids
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{}:{}:v{}:{}",
        cache_type, query_normalized, index_version, libs
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_get_set() {
        let cache: SearchCache<String> = SearchCache::new(10, 60);
        cache.set("key1".into(), "value1".into());
        assert_eq!(cache.get("key1"), Some("value1".into()));
        assert!(cache.get("key2").is_none());
    }

    #[test]
    fn test_cache_eviction() {
        let cache: SearchCache<i32> = SearchCache::new(2, 60);
        cache.set("a".into(), 1);
        cache.set("b".into(), 2);
        cache.set("c".into(), 3); // 触发淘汰
                                  // 淘汰最旧的条目
        let count = [
            cache.get("a").is_some(),
            cache.get("b").is_some(),
            cache.get("c").is_some(),
        ]
        .iter()
        .filter(|x| **x)
        .count();
        assert!(count <= 2);
    }

    #[test]
    fn test_invalidate_by_prefix() {
        let cache: SearchCache<i32> = SearchCache::new(10, 60);
        cache.set("lib1:query1".into(), 1);
        cache.set("lib1:query2".into(), 2);
        cache.set("lib2:query1".into(), 3);
        cache.invalidate_by_prefix("lib1:");
        assert!(cache.get("lib1:query1").is_none());
        assert!(cache.get("lib1:query2").is_none());
        assert!(cache.get("lib2:query1").is_some());
    }
}
