//! Connection thread pools (`nsd/pools.c`, `nsd/queue.c`) and request
//! limits (`nsd/limits.c`).
//!
//! A pool owns between `min` and `max` connection threads.  Queued work is
//! run by an idle thread; if none is idle and the pool is below `max`, a new
//! thread is started.  Idle threads above `min` exit after `timeout`
//! seconds, and threads exit after serving `maxconns` connections (when
//! non-zero).  The thread-accounting state machine
//! (`idle ≤ current ≤ max`, queue/idle exclusivity) is specified and proved
//! in `verus/pool.rs`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant};

/// Work queued on a pool.
pub type Work = Box<dyn FnOnce() + Send>;
/// Hook run by each connection thread before it exits.
pub type ThreadHook = Arc<dyn Fn() + Send + Sync>;

/// Pool tuning (`ns_pools set`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolConfig {
    /// Minimum threads kept alive.
    pub min: usize,
    /// Maximum threads.
    pub max: usize,
    /// Idle seconds before a thread above `min` exits.
    pub timeout: u64,
    /// Connections served before a thread exits (0 = never).
    pub maxconns: u64,
    /// Percent random variance on timeout/maxconns (kept for `ns_pools get`).
    pub spread: u32,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            min: 0,
            max: 10,
            timeout: 120,
            maxconns: 0,
            spread: 20,
        }
    }
}

impl PoolConfig {
    /// Validate as `ns_pools set` does.
    pub fn validate(&self) -> Result<(), String> {
        if self.max < 1 {
            return Err("maxthreads can't be less than 1".into());
        }
        if self.min > self.max {
            return Err("minthreads can't be larger than maxthreads".into());
        }
        if self.timeout < 1 {
            return Err("timeout cannot be less than 1".into());
        }
        if self.spread > 100 {
            return Err("spread must be between 0 and 100".into());
        }
        Ok(())
    }
}

/// Live thread accounting for a pool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolStats {
    /// Threads alive.
    pub current: usize,
    /// Threads waiting for work.
    pub idle: usize,
    /// Work items waiting for a thread.
    pub waiting: usize,
    /// Total work items ever queued.
    pub queued: u64,
}

struct PoolState {
    queue: VecDeque<Work>,
    current: usize,
    idle: usize,
    queued: u64,
    shutdown: bool,
}

/// A connection thread pool.
pub struct Pool {
    /// Pool name.
    pub name: String,
    config: RwLock<PoolConfig>,
    state: Mutex<PoolState>,
    cond: Condvar,
    hook: RwLock<Option<ThreadHook>>,
    next_thread: AtomicU64,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Pool {
    /// A new, empty pool.
    pub fn new(name: &str, config: PoolConfig) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_owned(),
            config: RwLock::new(config),
            state: Mutex::new(PoolState {
                queue: VecDeque::new(),
                current: 0,
                idle: 0,
                queued: 0,
                shutdown: false,
            }),
            cond: Condvar::new(),
            hook: RwLock::new(None),
            next_thread: AtomicU64::new(0),
        })
    }

    fn lock(&self) -> MutexGuard<'_, PoolState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Current configuration.
    pub fn config(&self) -> PoolConfig {
        *self.config.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Replace the configuration (validated).
    pub fn set_config(&self, c: PoolConfig) -> Result<(), String> {
        c.validate()?;
        *self.config.write().unwrap_or_else(PoisonError::into_inner) = c;
        Ok(())
    }

    /// Install the thread-exit hook.
    pub fn set_hook(&self, hook: Option<ThreadHook>) {
        *self.hook.write().unwrap_or_else(PoisonError::into_inner) = hook;
    }

    /// Thread accounting snapshot.
    pub fn stats(&self) -> PoolStats {
        let g = self.lock();
        PoolStats {
            current: g.current,
            idle: g.idle,
            waiting: g.queue.len(),
            queued: g.queued,
        }
    }

    fn spawn_thread(self: &Arc<Self>) {
        let me = Arc::clone(self);
        let n = self.next_thread.fetch_add(1, Ordering::Relaxed);
        let name = format!("-conn:{}:{n}-", self.name);
        if std::thread::Builder::new()
            .name(name)
            .spawn(move || me.thread_main())
            .is_err()
        {
            self.lock().current -= 1;
        }
    }

    /// Queue work (`NsQueueConn`).  Returns `false` if the pool is shut down.
    pub fn enqueue(self: &Arc<Self>, work: Work) -> bool {
        let max = self.config().max;
        let spawn = {
            let mut g = self.lock();
            if g.shutdown {
                return false;
            }
            g.queue.push_back(work);
            g.queued += 1;
            let spawn = g.idle < g.queue.len() && g.current < max;
            if spawn {
                g.current += 1;
            }
            spawn
        };
        if spawn {
            self.spawn_thread();
        }
        self.cond.notify_one();
        true
    }

    /// Start `min` threads (`NsStartPools`).
    pub fn start(self: &Arc<Self>) {
        let min = self.config().min;
        let n = {
            let mut g = self.lock();
            let n = min.saturating_sub(g.current);
            g.current += n;
            n
        };
        for _ in 0..n {
            self.spawn_thread();
        }
    }

    fn thread_main(self: Arc<Self>) {
        let mut served: u64 = 0;
        let mut g = self.lock();
        loop {
            let cfg = self.config();
            if let Some(work) = g.queue.pop_front() {
                drop(g);
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
                served += 1;
                g = self.lock();
                if cfg.maxconns > 0 && served >= cfg.maxconns {
                    break;
                }
                continue;
            }
            if g.shutdown {
                break;
            }
            g.idle += 1;
            let start = Instant::now();
            let (ng, timeout) = self
                .cond
                .wait_timeout(g, Duration::from_secs(cfg.timeout.max(1)))
                .unwrap_or_else(PoisonError::into_inner);
            g = ng;
            g.idle -= 1;
            if timeout.timed_out()
                && g.queue.is_empty()
                && g.current > cfg.min
                && start.elapsed() >= Duration::from_secs(cfg.timeout.max(1))
            {
                break;
            }
        }
        g.current -= 1;
        let respawn = !g.shutdown && !g.queue.is_empty() && g.idle == 0;
        if respawn {
            g.current += 1;
        }
        drop(g);
        self.cond.notify_all();
        if respawn {
            self.spawn_thread();
        }
        let hook = self
            .hook
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(h) = hook {
            h();
        }
    }

    /// Stop accepting work, let threads drain the queue, and wait up to
    /// `timeout` for them to exit.  Returns whether all exited.
    pub fn shutdown(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut g = self.lock();
        g.shutdown = true;
        self.cond.notify_all();
        while g.current > 0 {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            g = self
                .cond
                .wait_timeout(g, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            self.cond.notify_all();
        }
        true
    }
}

/// Per-URL request limits (`ns_limits`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitsConfig {
    /// Max concurrently running requests.
    pub maxrun: u32,
    /// Max requests waiting for a thread.
    pub maxwait: u32,
    /// Max request body size in bytes.
    pub maxupload: usize,
    /// Max seconds a request may wait in the queue.
    pub timeout: u32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            maxrun: 100,
            maxwait: 100,
            maxupload: 10 * 1024 * 1000,
            timeout: 60,
        }
    }
}

/// A named limits record with live counters.
#[derive(Debug)]
pub struct Limits {
    /// Name.
    pub name: String,
    config: RwLock<LimitsConfig>,
    counters: Mutex<LimitCounters>,
}

/// Live counters for a limits record.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LimitCounters {
    /// Requests running.
    pub nrunning: u32,
    /// Requests waiting.
    pub nwaiting: u32,
    /// Requests dropped for exceeding `maxupload`.
    pub ndropped: u32,
    /// Requests rejected for exceeding `maxwait`.
    pub noverflow: u32,
    /// Requests that timed out waiting.
    pub ntimeout: u32,
}

impl Limits {
    /// New limits.
    pub fn new(name: &str, config: LimitsConfig) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_owned(),
            config: RwLock::new(config),
            counters: Mutex::new(LimitCounters::default()),
        })
    }

    /// Configuration.
    pub fn config(&self) -> LimitsConfig {
        *self.config.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Replace configuration.
    pub fn set_config(&self, c: LimitsConfig) {
        *self.config.write().unwrap_or_else(PoisonError::into_inner) = c;
    }

    /// Counters snapshot.
    pub fn counters(&self) -> LimitCounters {
        *self.counters.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Try to admit a request into the wait queue.  Returns `false` (and
    /// counts an overflow) if `maxwait` is reached.
    pub fn admit(&self) -> bool {
        let max = self.config().maxwait;
        let mut c = self.counters.lock().unwrap_or_else(PoisonError::into_inner);
        if c.nwaiting >= max {
            c.noverflow += 1;
            false
        } else {
            c.nwaiting += 1;
            true
        }
    }

    /// A waiting request started running.
    pub fn start_run(&self) {
        let mut c = self.counters.lock().unwrap_or_else(PoisonError::into_inner);
        c.nwaiting = c.nwaiting.saturating_sub(1);
        c.nrunning += 1;
    }

    /// A running request finished.
    pub fn end_run(&self) {
        let mut c = self.counters.lock().unwrap_or_else(PoisonError::into_inner);
        c.nrunning = c.nrunning.saturating_sub(1);
    }

    /// Count a dropped (too large) request.
    pub fn drop_upload(&self) {
        self.counters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .ndropped += 1;
    }
}

/// Registry of pools and limits.
#[derive(Default)]
pub struct PoolRegistry {
    pools: Mutex<BTreeMap<String, Arc<Pool>>>,
    limits: Mutex<BTreeMap<String, Arc<Limits>>>,
    hook: RwLock<Option<ThreadHook>>,
}

impl std::fmt::Debug for PoolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolRegistry").finish_non_exhaustive()
    }
}

impl PoolRegistry {
    /// Set the hook installed on every pool's threads.
    pub fn set_thread_hook(&self, hook: ThreadHook) {
        *self.hook.write().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&hook));
        for p in self
            .pools
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
        {
            p.set_hook(Some(Arc::clone(&hook)));
        }
    }

    /// Get or create a pool.
    pub fn get_or_create(&self, name: &str) -> Arc<Pool> {
        let mut m = self.pools.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(p) = m.get(name) {
            return Arc::clone(p);
        }
        let p = Pool::new(name, PoolConfig::default());
        p.set_hook(
            self.hook
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        );
        m.insert(name.to_owned(), Arc::clone(&p));
        p
    }

    /// Look up a pool.
    pub fn get(&self, name: &str) -> Option<Arc<Pool>> {
        self.pools
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned()
    }

    /// The default pool.
    pub fn default_pool(&self) -> Arc<Pool> {
        self.get_or_create("default")
    }

    /// All pools.
    pub fn all(&self) -> Vec<Arc<Pool>> {
        self.pools
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    /// Get or create a limits record.
    pub fn limits_or_create(&self, name: &str) -> Arc<Limits> {
        Arc::clone(
            self.limits
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(name.to_owned())
                .or_insert_with(|| Limits::new(name, LimitsConfig::default())),
        )
    }

    /// Look up limits.
    pub fn limits(&self, name: &str) -> Option<Arc<Limits>> {
        self.limits
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned()
    }

    /// Limits names.
    pub fn limits_names(&self) -> Vec<String> {
        self.limits
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }

    /// Start every pool's minimum threads.
    pub fn start_all(&self) {
        for p in self.all() {
            p.start();
        }
    }

    /// Shut down every pool.
    pub fn shutdown_all(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        self.all()
            .iter()
            .all(|p| p.shutdown(deadline.saturating_duration_since(Instant::now())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn runs_work_and_bounds_threads() {
        let p = Pool::new(
            "t",
            PoolConfig {
                max: 3,
                ..PoolConfig::default()
            },
        );
        let n = Arc::new(AtomicUsize::new(0));
        let exits = Arc::new(AtomicUsize::new(0));
        let e2 = Arc::clone(&exits);
        p.set_hook(Some(Arc::new(move || {
            e2.fetch_add(1, Ordering::SeqCst);
        })));
        for _ in 0..20 {
            let n = Arc::clone(&n);
            assert!(p.enqueue(Box::new(move || {
                std::thread::sleep(Duration::from_millis(10));
                n.fetch_add(1, Ordering::SeqCst);
            })));
            let s = p.stats();
            assert!(s.current <= 3 && s.idle <= s.current);
        }
        assert!(p.shutdown(Duration::from_secs(10)));
        assert_eq!(n.load(Ordering::SeqCst), 20, "queue drained before exit");
        assert!(!p.enqueue(Box::new(|| {})));
        assert_eq!(p.stats().current, 0);
        assert!(exits.load(Ordering::SeqCst) >= 1);
        assert_eq!(p.stats().queued, 20);
    }

    #[test]
    fn maxconns_recycles_threads() {
        let p = Pool::new(
            "m",
            PoolConfig {
                max: 1,
                maxconns: 2,
                ..PoolConfig::default()
            },
        );
        let n = Arc::new(AtomicUsize::new(0));
        for _ in 0..5 {
            let n = Arc::clone(&n);
            p.enqueue(Box::new(move || {
                n.fetch_add(1, Ordering::SeqCst);
            }));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while n.load(Ordering::SeqCst) < 5 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(n.load(Ordering::SeqCst), 5);
        assert!(p.shutdown(Duration::from_secs(5)));
    }

    #[test]
    fn min_threads_and_config() {
        let p = Pool::new(
            "min",
            PoolConfig {
                min: 2,
                max: 4,
                ..PoolConfig::default()
            },
        );
        p.start();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(p.stats().current, 2);
        assert!(p
            .set_config(PoolConfig {
                min: 5,
                max: 4,
                ..PoolConfig::default()
            })
            .is_err());
        assert!(PoolConfig {
            max: 0,
            ..PoolConfig::default()
        }
        .validate()
        .is_err());
        assert!(PoolConfig {
            timeout: 0,
            ..PoolConfig::default()
        }
        .validate()
        .is_err());
        assert!(PoolConfig {
            spread: 101,
            ..PoolConfig::default()
        }
        .validate()
        .is_err());
        assert!(p.shutdown(Duration::from_secs(5)));
    }

    #[test]
    fn limits_admission() {
        let l = Limits::new(
            "l",
            LimitsConfig {
                maxwait: 1,
                ..LimitsConfig::default()
            },
        );
        assert!(l.admit());
        assert!(!l.admit());
        l.start_run();
        assert!(l.admit());
        l.end_run();
        l.drop_upload();
        let c = l.counters();
        assert_eq!(
            (c.nrunning, c.nwaiting, c.noverflow, c.ndropped),
            (0, 1, 1, 1)
        );
        let r = PoolRegistry::default();
        assert_eq!(r.default_pool().name, "default");
        assert_eq!(r.limits_or_create("default").config().maxrun, 100);
        assert!(r.limits("nope").is_none());
        assert_eq!(r.limits_names(), vec!["default"]);
        r.set_thread_hook(Arc::new(|| {}));
        r.start_all();
        assert!(r.shutdown_all(Duration::from_secs(2)));
    }
}
