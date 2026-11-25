//! Thread-safe sized/timed LRU cache (`nsd/cache.c`, `Ns_Cache*`).
//!
//! * Entries carry a byte size; when `max_size > 0` the least recently used
//!   entries are evicted until the total fits.  An entry larger than the
//!   whole cache is not stored.
//! * An optional timeout expires entries on access.
//! * [`Cache::get_or_compute`] is single-flight: while one thread computes a
//!   value for a key, other threads asking for the same key wait for it
//!   instead of computing it again (the `ns_cache eval` behaviour built on
//!   `Ns_CacheWait`/`Ns_CacheBroadcast`).
//!
//! Size accounting (`total == Σ sizes ≤ max_size`) is specified and proved in
//! `verus/cache.rs`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::glob::string_match;

/// Cache statistics (`ns_cache_stats`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Successful lookups.
    pub hits: u64,
    /// Failed lookups.
    pub misses: u64,
    /// Explicit flushes of single entries or the whole cache.
    pub flushed: u64,
    /// Entries evicted to make room or expired.
    pub pruned: u64,
    /// Current entry count.
    pub entries: usize,
    /// Current total size in bytes.
    pub size: usize,
}

#[derive(Debug)]
struct Entry<V> {
    value: V,
    size: usize,
    tick: u64,
    expires: Option<Instant>,
}

#[derive(Debug)]
struct Inner<V> {
    map: HashMap<String, Entry<V>>,
    lru: BTreeMap<u64, String>,
    pending: HashSet<String>,
    total: usize,
    tick: u64,
    stats: CacheStats,
}

/// A named LRU cache.  See the module documentation.
#[derive(Debug)]
pub struct Cache<V> {
    name: String,
    max_size: usize,
    timeout: Option<Duration>,
    inner: Mutex<Inner<V>>,
    cond: Condvar,
}

/// Error from [`Cache::get_or_compute`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CacheError<E> {
    /// Waiting for another thread's computation exceeded `maxwait`.
    #[error("timeout waiting for cache entry")]
    Timeout,
    /// The compute function failed.
    #[error("compute failed")]
    Compute(E),
}

impl<V: Clone> Cache<V> {
    /// Create a cache.  `max_size == 0` means unbounded.
    pub fn new(name: &str, max_size: usize, timeout: Option<Duration>) -> Self {
        Self {
            name: name.to_owned(),
            max_size,
            timeout,
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                lru: BTreeMap::new(),
                pending: HashSet::new(),
                total: 0,
                tick: 0,
                stats: CacheStats::default(),
            }),
            cond: Condvar::new(),
        }
    }

    /// Cache name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Configured maximum size (0 = unbounded).
    pub fn max_size(&self) -> usize {
        self.max_size
    }

    /// Configured entry timeout.
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    fn lock(&self) -> MutexGuard<'_, Inner<V>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn remove_locked(inner: &mut Inner<V>, key: &str) -> Option<Entry<V>> {
        let e = inner.map.remove(key)?;
        inner.lru.remove(&e.tick);
        inner.total -= e.size;
        Some(e)
    }

    fn lookup_locked(inner: &mut Inner<V>, key: &str) -> Option<V> {
        let now = Instant::now();
        let expired = inner
            .map
            .get(key)
            .is_some_and(|e| e.expires.is_some_and(|t| t <= now));
        if expired {
            Self::remove_locked(inner, key);
            inner.stats.pruned += 1;
        }
        inner.tick += 1;
        let tick = inner.tick;
        let Inner {
            map, lru, stats, ..
        } = inner;
        if let Some(e) = map.get_mut(key) {
            lru.remove(&e.tick);
            e.tick = tick;
            lru.insert(tick, key.to_owned());
            stats.hits += 1;
            Some(e.value.clone())
        } else {
            stats.misses += 1;
            None
        }
    }

    /// Look up `key`, refreshing its recency.
    pub fn get(&self, key: &str) -> Option<V> {
        let mut inner = self.lock();
        Self::lookup_locked(&mut inner, key)
    }

    fn insert_locked(&self, inner: &mut Inner<V>, key: &str, value: V, size: usize) -> bool {
        Self::remove_locked(inner, key);
        if self.max_size > 0 && size > self.max_size {
            return false;
        }
        while self.max_size > 0 && inner.total + size > self.max_size {
            let Some((_, victim)) = inner.lru.pop_first() else {
                break;
            };
            if let Some(e) = inner.map.remove(&victim) {
                inner.total -= e.size;
                inner.stats.pruned += 1;
            }
        }
        inner.tick += 1;
        let tick = inner.tick;
        inner.lru.insert(tick, key.to_owned());
        inner.total += size;
        inner.map.insert(
            key.to_owned(),
            Entry {
                value,
                size,
                tick,
                expires: self.timeout.map(|t| Instant::now() + t),
            },
        );
        true
    }

    /// Store `value` (of `size` bytes) under `key`, evicting LRU entries as
    /// needed.  Returns `false` if the value is larger than the cache.
    pub fn set(&self, key: &str, value: V, size: usize) -> bool {
        let mut inner = self.lock();
        self.insert_locked(&mut inner, key, value, size)
    }

    /// Remove `key`; returns whether it existed (`ns_cache flush cache key`).
    pub fn remove(&self, key: &str) -> bool {
        let mut inner = self.lock();
        let found = Self::remove_locked(&mut inner, key).is_some();
        if found {
            inner.stats.flushed += 1;
        }
        found
    }

    /// Remove every entry whose key matches `pattern` (all if `None`).
    /// Returns the number removed.
    pub fn flush(&self, pattern: Option<&str>) -> usize {
        let mut inner = self.lock();
        let keys: Vec<String> = inner
            .map
            .keys()
            .filter(|k| pattern.is_none_or(|p| string_match(k, p)))
            .cloned()
            .collect();
        for k in &keys {
            Self::remove_locked(&mut inner, k);
        }
        inner.stats.flushed += keys.len() as u64;
        keys.len()
    }

    /// Keys, optionally filtered by glob `pattern`, most recent last.
    pub fn keys(&self, pattern: Option<&str>) -> Vec<String> {
        let inner = self.lock();
        inner
            .lru
            .values()
            .filter(|k| pattern.is_none_or(|p| string_match(k, p)))
            .cloned()
            .collect()
    }

    /// Current statistics.
    pub fn stats(&self) -> CacheStats {
        let inner = self.lock();
        CacheStats {
            entries: inner.map.len(),
            size: inner.total,
            ..inner.stats
        }
    }

    /// Atomically transform the value at `key` (e.g. `ns_cache incr`).
    /// `f` receives the current value (if any) and returns the new value and
    /// its size.
    pub fn update<F>(&self, key: &str, f: F) -> V
    where
        F: FnOnce(Option<V>) -> (V, usize),
    {
        let mut inner = self.lock();
        let cur = Self::lookup_locked(&mut inner, key);
        let (v, size) = f(cur);
        self.insert_locked(&mut inner, key, v.clone(), size);
        v
    }

    /// Single-flight fetch: return the cached value or compute it with `f`
    /// (returning value and size).  Concurrent callers for the same key wait
    /// up to `maxwait` for the first caller's result.
    pub fn get_or_compute<E, F>(
        &self,
        key: &str,
        maxwait: Option<Duration>,
        f: F,
    ) -> Result<V, CacheError<E>>
    where
        F: FnOnce() -> Result<(V, usize), E>,
    {
        let deadline = maxwait.map(|d| Instant::now() + d);
        let mut inner = self.lock();
        loop {
            if let Some(v) = Self::lookup_locked(&mut inner, key) {
                return Ok(v);
            }
            if !inner.pending.contains(key) {
                break;
            }
            match deadline {
                None => {
                    inner = self
                        .cond
                        .wait(inner)
                        .unwrap_or_else(PoisonError::into_inner);
                }
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return Err(CacheError::Timeout);
                    }
                    inner = self
                        .cond
                        .wait_timeout(inner, d - now)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
            }
        }
        inner.pending.insert(key.to_owned());
        drop(inner);
        let result = f();
        let mut inner = self.lock();
        inner.pending.remove(key);
        let out = match result {
            Ok((v, size)) => {
                self.insert_locked(&mut inner, key, v.clone(), size);
                Ok(v)
            }
            Err(e) => Err(CacheError::Compute(e)),
        };
        drop(inner);
        self.cond.notify_all();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn get_set_remove() {
        let c: Cache<String> = Cache::new("t", 0, None);
        assert_eq!(c.get("a"), None);
        assert!(c.set("a", "1".into(), 1));
        assert_eq!(c.get("a").as_deref(), Some("1"));
        assert!(c.remove("a"));
        assert!(!c.remove("a"));
        let s = c.stats();
        assert_eq!((s.hits, s.misses, s.flushed, s.entries), (1, 1, 1, 0));
        assert_eq!(c.name(), "t");
    }

    #[test]
    fn lru_eviction() {
        let c: Cache<i32> = Cache::new("t", 10, None);
        c.set("a", 1, 4);
        c.set("b", 2, 4);
        assert_eq!(c.get("a"), Some(1)); // a is now most recent
        c.set("c", 3, 4); // evicts b
        assert_eq!(c.get("b"), None);
        assert_eq!(c.get("a"), Some(1));
        assert_eq!(c.get("c"), Some(3));
        assert!(!c.set("huge", 9, 11));
        assert_eq!(c.stats().size, 8);
        assert_eq!(c.max_size(), 10);
    }

    #[test]
    fn timeout_expires() {
        let c: Cache<i32> = Cache::new("t", 0, Some(Duration::from_millis(20)));
        c.set("a", 1, 1);
        assert_eq!(c.get("a"), Some(1));
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(c.get("a"), None);
        assert_eq!(c.stats().pruned, 1);
    }

    #[test]
    fn flush_and_keys() {
        let c: Cache<i32> = Cache::new("t", 0, None);
        for k in ["foo1", "foo2", "bar"] {
            c.set(k, 0, 1);
        }
        assert_eq!(c.keys(Some("foo*")).len(), 2);
        assert_eq!(c.flush(Some("foo*")), 2);
        assert_eq!(c.keys(None), vec!["bar"]);
        assert_eq!(c.flush(None), 1);
    }

    #[test]
    fn update_increments() {
        let c: Cache<i64> = Cache::new("t", 0, None);
        assert_eq!(c.update("n", |v| (v.unwrap_or(0) + 1, 8)), 1);
        assert_eq!(c.update("n", |v| (v.unwrap_or(0) + 5, 8)), 6);
    }

    #[test]
    fn single_flight() {
        let c: Arc<Cache<i32>> = Arc::new(Cache::new("t", 0, None));
        let calls = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let c = Arc::clone(&c);
                let calls = Arc::clone(&calls);
                std::thread::spawn(move || {
                    c.get_or_compute::<(), _>("k", None, || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(30));
                        Ok((42, 1))
                    })
                })
            })
            .collect();
        for h in handles {
            assert_eq!(h.join().expect("join"), Ok(42));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn compute_error_and_wait_timeout() {
        let c: Arc<Cache<i32>> = Arc::new(Cache::new("t", 0, None));
        assert_eq!(
            c.get_or_compute("k", None, || Err("bad")),
            Err(CacheError::Compute("bad"))
        );
        let c2 = Arc::clone(&c);
        let slow = std::thread::spawn(move || {
            c2.get_or_compute::<(), _>("s", None, || {
                std::thread::sleep(Duration::from_millis(200));
                Ok((1, 1))
            })
        });
        std::thread::sleep(Duration::from_millis(30));
        let r = c.get_or_compute::<(), _>("s", Some(Duration::from_millis(10)), || Ok((2, 1)));
        assert_eq!(r, Err(CacheError::Timeout));
        assert_eq!(slow.join().expect("join"), Ok(1));
    }

    proptest! {
        #[test]
        fn size_never_exceeds_max(ops in proptest::collection::vec((0u8..6, 1usize..8), 0..60)) {
            let c: Cache<u8> = Cache::new("p", 16, None);
            for (k, sz) in ops {
                c.set(&k.to_string(), k, sz);
                let s = c.stats();
                prop_assert!(s.size <= 16);
            }
        }
    }
}
