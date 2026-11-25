//! Named caches for `ns_cache` (`nsd/tclcache.c`).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use ns_core::cache::Cache;

/// A string cache plus the options it was created with.
#[derive(Debug)]
pub struct NamedCache {
    /// The cache.
    pub cache: Cache<String>,
    /// Default `-maxwait` for `ns_cache eval`.
    pub maxwait: Option<Duration>,
}

/// Registry of `ns_cache` caches (process-wide, as in AOLserver).
#[derive(Debug, Default)]
pub struct CacheRegistry {
    caches: Mutex<BTreeMap<String, Arc<NamedCache>>>,
}

impl CacheRegistry {
    /// Create a cache; returns `None` if the name is taken.
    pub fn create(
        &self,
        name: &str,
        size: usize,
        timeout: Option<Duration>,
        maxwait: Option<Duration>,
    ) -> Option<Arc<NamedCache>> {
        let mut m = self.caches.lock().unwrap_or_else(PoisonError::into_inner);
        if m.contains_key(name) {
            return None;
        }
        let c = Arc::new(NamedCache {
            cache: Cache::new(name, size, timeout),
            maxwait,
        });
        m.insert(name.to_owned(), Arc::clone(&c));
        Some(c)
    }

    /// Look up a cache.
    pub fn get(&self, name: &str) -> Option<Arc<NamedCache>> {
        self.caches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned()
    }

    /// Cache names in order.
    pub fn names(&self) -> Vec<String> {
        self.caches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_lookup() {
        let r = CacheRegistry::default();
        assert!(r.create("a", 10, None, None).is_some());
        assert!(r.create("a", 10, None, None).is_none());
        let c = r.get("a").expect("cache");
        c.cache.set("k", "v".into(), 1);
        assert_eq!(
            r.get("a").expect("cache").cache.get("k").as_deref(),
            Some("v")
        );
        assert_eq!(r.names(), vec!["a"]);
        assert!(r.get("b").is_none());
    }
}
