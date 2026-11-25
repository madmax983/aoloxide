//! Script-visible synchronization objects (`nsd/tclthread.c`): mutexes,
//! condition variables, read/write locks, semaphores and recursive critical
//! sections.  Scripts lock and unlock in separate commands, so these are
//! *manual* primitives rather than RAII guards.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A manually locked mutex (`Ns_Mutex`).
#[derive(Debug, Default)]
pub struct NsMutex {
    locked: Mutex<bool>,
    cv: Condvar,
    /// Name given at creation.
    pub name: String,
}

impl NsMutex {
    /// Named mutex.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            ..Self::default()
        }
    }

    /// Block until acquired.
    pub fn lock(&self) {
        let mut g = lock(&self.locked);
        while *g {
            g = self.cv.wait(g).unwrap_or_else(PoisonError::into_inner);
        }
        *g = true;
    }

    /// Acquire without blocking; returns success.
    pub fn try_lock(&self) -> bool {
        let mut g = lock(&self.locked);
        if *g {
            false
        } else {
            *g = true;
            true
        }
    }

    /// Release.
    pub fn unlock(&self) {
        *lock(&self.locked) = false;
        self.cv.notify_one();
    }

    /// Whether currently locked.
    pub fn is_locked(&self) -> bool {
        *lock(&self.locked)
    }
}

/// A condition variable used with an [`NsMutex`] (`Ns_Cond`).
#[derive(Debug, Default)]
pub struct NsCond {
    cv: Condvar,
}

impl NsCond {
    /// Atomically release `m`, wait for a signal (or `timeout`), then
    /// re-acquire `m`.  Returns `false` on timeout.
    pub fn wait(&self, m: &NsMutex, timeout: Option<Duration>) -> bool {
        let mut g = lock(&m.locked);
        *g = false;
        m.cv.notify_one();
        let signalled = match timeout {
            None => {
                g = self.cv.wait(g).unwrap_or_else(PoisonError::into_inner);
                true
            }
            Some(t) => {
                let (ng, res) = self
                    .cv
                    .wait_timeout(g, t)
                    .unwrap_or_else(PoisonError::into_inner);
                g = ng;
                !res.timed_out()
            }
        };
        while *g {
            g = m.cv.wait(g).unwrap_or_else(PoisonError::into_inner);
        }
        *g = true;
        signalled
    }

    /// Wake one waiter.
    pub fn signal(&self) {
        self.cv.notify_one();
    }

    /// Wake all waiters.
    pub fn broadcast(&self) {
        self.cv.notify_all();
    }
}

/// A manually locked reader/writer lock (`Ns_RWLock`).
#[derive(Debug, Default)]
pub struct NsRwLock {
    state: Mutex<(usize, bool)>,
    cv: Condvar,
}

impl NsRwLock {
    /// Acquire shared.
    pub fn read_lock(&self) {
        let mut g = lock(&self.state);
        while g.1 {
            g = self.cv.wait(g).unwrap_or_else(PoisonError::into_inner);
        }
        g.0 += 1;
    }

    /// Acquire exclusive.
    pub fn write_lock(&self) {
        let mut g = lock(&self.state);
        while g.1 || g.0 > 0 {
            g = self.cv.wait(g).unwrap_or_else(PoisonError::into_inner);
        }
        g.1 = true;
    }

    /// Release whichever lock the caller holds.
    pub fn unlock(&self) {
        let mut g = lock(&self.state);
        if g.1 {
            g.1 = false;
        } else if g.0 > 0 {
            g.0 -= 1;
        }
        drop(g);
        self.cv.notify_all();
    }
}

/// A counting semaphore (`Ns_Sema`).
#[derive(Debug, Default)]
pub struct NsSema {
    count: Mutex<i64>,
    cv: Condvar,
}

impl NsSema {
    /// Semaphore with an initial count.
    pub fn new(count: i64) -> Self {
        Self {
            count: Mutex::new(count),
            cv: Condvar::new(),
        }
    }

    /// Decrement, blocking while zero.
    pub fn wait(&self) {
        let mut g = lock(&self.count);
        while *g <= 0 {
            g = self.cv.wait(g).unwrap_or_else(PoisonError::into_inner);
        }
        *g -= 1;
    }

    /// Increment by `n`.
    pub fn release(&self, n: i64) {
        *lock(&self.count) += n;
        self.cv.notify_all();
    }

    /// Current count.
    pub fn count(&self) -> i64 {
        *lock(&self.count)
    }
}

/// A recursive critical section (`Ns_Cs`).
#[derive(Debug, Default)]
pub struct NsCritSec {
    state: Mutex<(Option<ThreadId>, usize)>,
    cv: Condvar,
}

impl NsCritSec {
    /// Enter (re-entrant for the owning thread).
    pub fn enter(&self) {
        let me = std::thread::current().id();
        let mut g = lock(&self.state);
        loop {
            match g.0 {
                None => {
                    *g = (Some(me), 1);
                    return;
                }
                Some(owner) if owner == me => {
                    g.1 += 1;
                    return;
                }
                Some(_) => g = self.cv.wait(g).unwrap_or_else(PoisonError::into_inner),
            }
        }
    }

    /// Leave once.
    pub fn leave(&self) {
        let mut g = lock(&self.state);
        if g.1 > 0 {
            g.1 -= 1;
            if g.1 == 0 {
                g.0 = None;
                drop(g);
                self.cv.notify_one();
            }
        }
    }
}

/// Registry of named sync objects, keyed by script handle.
#[derive(Debug, Default)]
pub struct SyncRegistry {
    next: AtomicU64,
    /// Mutexes.
    pub mutexes: Mutex<HashMap<String, Arc<NsMutex>>>,
    /// Condition variables.
    pub conds: Mutex<HashMap<String, Arc<NsCond>>>,
    /// Read/write locks.
    pub rwlocks: Mutex<HashMap<String, Arc<NsRwLock>>>,
    /// Semaphores.
    pub semas: Mutex<HashMap<String, Arc<NsSema>>>,
    /// Critical sections.
    pub critsecs: Mutex<HashMap<String, Arc<NsCritSec>>>,
}

impl SyncRegistry {
    /// A fresh handle string `ns:<kind>:<n>`.
    pub fn new_handle(&self, kind: &str) -> String {
        format!("ns:{kind}:{}", self.next.fetch_add(1, Ordering::Relaxed))
    }

    /// Look up or (when `create`) create an object in `map`.
    pub fn get_or<T: Default>(map: &Mutex<HashMap<String, Arc<T>>>, handle: &str) -> Arc<T> {
        Arc::clone(lock(map).entry(handle.to_owned()).or_default())
    }

    /// Look up an existing object.
    pub fn get<T>(map: &Mutex<HashMap<String, Arc<T>>>, handle: &str) -> Option<Arc<T>> {
        lock(map).get(handle).cloned()
    }

    /// Insert an object under `handle`.
    pub fn insert<T>(map: &Mutex<HashMap<String, Arc<T>>>, handle: &str, v: Arc<T>) {
        lock(map).insert(handle.to_owned(), v);
    }

    /// Remove an object.
    pub fn remove<T>(map: &Mutex<HashMap<String, Arc<T>>>, handle: &str) -> bool {
        lock(map).remove(handle).is_some()
    }
}

/// Absolute deadline helper for `ns_cond abswait`.
pub fn until_unix(epoch_secs: i64) -> Duration {
    let now = ns_core::httptime::now();
    Duration::from_secs((epoch_secs - now).max(0) as u64)
}

/// Sleep that wakes early when `stop` returns true (polled every 50ms).
pub fn interruptible_sleep(d: Duration, stop: impl Fn() -> bool) {
    let end = Instant::now() + d;
    while Instant::now() < end && !stop() {
        std::thread::sleep((end - Instant::now()).min(Duration::from_millis(50)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutex_and_cond() {
        let m = Arc::new(NsMutex::new("m"));
        let c = Arc::new(NsCond::default());
        let flag = Arc::new(Mutex::new(false));
        m.lock();
        assert!(!m.try_lock());
        let (m2, c2, f2) = (Arc::clone(&m), Arc::clone(&c), Arc::clone(&flag));
        let h = std::thread::spawn(move || {
            m2.lock();
            *f2.lock().expect("lock") = true;
            c2.signal();
            m2.unlock();
        });
        while !*flag.lock().expect("lock") {
            assert!(c.wait(&m, Some(Duration::from_secs(5))));
        }
        assert!(m.is_locked());
        m.unlock();
        h.join().expect("join");
        m.lock();
        assert!(!c.wait(&m, Some(Duration::from_millis(20))), "times out");
        m.unlock();
        c.broadcast();
        assert_eq!(m.name, "m");
    }

    #[test]
    fn rwlock_sema_critsec() {
        let rw = NsRwLock::default();
        rw.read_lock();
        rw.read_lock();
        rw.unlock();
        rw.unlock();
        rw.write_lock();
        rw.unlock();

        let s = Arc::new(NsSema::new(0));
        let s2 = Arc::clone(&s);
        let h = std::thread::spawn(move || s2.wait());
        s.release(2);
        h.join().expect("join");
        assert_eq!(s.count(), 1);

        let cs = NsCritSec::default();
        cs.enter();
        cs.enter();
        cs.leave();
        cs.leave();
        cs.leave(); // extra leave is harmless
    }

    #[test]
    fn registry() {
        let r = SyncRegistry::default();
        let h = r.new_handle("mutex");
        assert!(h.starts_with("ns:mutex:"));
        SyncRegistry::insert(&r.mutexes, &h, Arc::new(NsMutex::new(&h)));
        assert!(SyncRegistry::get(&r.mutexes, &h).is_some());
        let _ = SyncRegistry::get_or(&r.semas, "x");
        assert!(SyncRegistry::remove(&r.semas, "x"));
        assert!(!SyncRegistry::remove(&r.semas, "x"));
        assert_eq!(until_unix(0), Duration::ZERO);
        interruptible_sleep(Duration::from_secs(5), || true);
    }
}
