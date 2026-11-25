//! The scheduler (`nsd/sched.c`): `Ns_ScheduleProc`, `Ns_ScheduleDaily`,
//! `Ns_ScheduleWeekly`, `Ns_Cancel`, `Ns_Pause`, `Ns_Resume`.
//!
//! A single scheduler thread runs due events.  Events flagged `thread` run
//! in a detached thread so they cannot delay the scheduler.  `once` events
//! are removed after running.

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ns_core::localtime::LocalTm;
use ns_core::log::Severity;

use crate::global::Global;

/// A scheduled procedure; receives its event id.
pub type SchedProc = Arc<dyn Fn(u64) + Send + Sync>;

/// Scheduling flags (`NS_SCHED_*`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SchedFlags {
    /// Run in a detached thread.
    pub thread: bool,
    /// Run only once.
    pub once: bool,
}

/// When an event repeats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repeat {
    /// Every `n` seconds.
    Interval(Duration),
    /// Every day at `hour:minute` local time.
    Daily {
        /// Hour 0..=23.
        hour: u32,
        /// Minute 0..=59.
        minute: u32,
    },
    /// Every week on `day` (0 = Sunday) at `hour:minute`.
    Weekly {
        /// Day of week 0..=6.
        day: u32,
        /// Hour 0..=23.
        hour: u32,
        /// Minute 0..=59.
        minute: u32,
    },
}

/// Public view of an event (`ns_info scheduled`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventInfo {
    /// Event id.
    pub id: u64,
    /// Flags.
    pub flags: SchedFlags,
    /// Repeat rule.
    pub repeat: Repeat,
    /// Whether paused.
    pub paused: bool,
    /// Whether currently running.
    pub running: bool,
    /// Description (e.g. the Tcl script).
    pub desc: String,
    /// Unix time of next run.
    pub next_run: i64,
    /// Unix time the last run started (0 if never).
    pub last_start: i64,
    /// Unix time the last run ended (0 if never).
    pub last_end: i64,
}

struct Event {
    proc_: SchedProc,
    flags: SchedFlags,
    repeat: Repeat,
    paused: bool,
    running: bool,
    cancelled: bool,
    desc: String,
    next: Instant,
    next_unix: i64,
    last_start: i64,
    last_end: i64,
}

#[derive(Default)]
struct Inner {
    events: BTreeMap<u64, Event>,
    next_id: u64,
    shutdown: bool,
}

/// The scheduler.
pub struct Scheduler {
    global: Weak<Global>,
    max_elapsed: Duration,
    inner: Arc<(Mutex<Inner>, Condvar)>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scheduler").finish_non_exhaustive()
    }
}

/// Seconds until the next occurrence of a daily/weekly time, given the
/// current Unix time and local UTC offset.
pub fn seconds_until(now: i64, gmtoff: i64, repeat: Repeat) -> i64 {
    let local = now + gmtoff;
    let into_day = local.rem_euclid(86_400);
    let wday = (local.div_euclid(86_400) + 4).rem_euclid(7);
    match repeat {
        Repeat::Interval(d) => d.as_secs().max(1) as i64,
        Repeat::Daily { hour, minute } => {
            let target = i64::from(hour) * 3600 + i64::from(minute) * 60;
            let mut delta = target - into_day;
            if delta <= 0 {
                delta += 86_400;
            }
            delta
        }
        Repeat::Weekly { day, hour, minute } => {
            let target = i64::from(day) * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60;
            let cur = wday * 86_400 + into_day;
            let mut delta = target - cur;
            if delta <= 0 {
                delta += 7 * 86_400;
            }
            delta
        }
    }
}

fn next_time(repeat: Repeat) -> (Instant, i64) {
    let now = ns_core::httptime::now();
    let off = LocalTm::from_unix(now).gmtoff;
    let secs = seconds_until(now, off, repeat);
    (
        Instant::now() + Duration::from_secs(secs as u64),
        now + secs,
    )
}

impl Scheduler {
    /// Create a scheduler (the thread starts on first use).
    pub fn new(global: Weak<Global>, max_elapsed: Duration) -> Self {
        Self {
            global,
            max_elapsed,
            inner: Arc::new((Mutex::new(Inner::default()), Condvar::new())),
            thread: Mutex::new(None),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn ensure_thread(&self) {
        let mut t = self.thread.lock().unwrap_or_else(PoisonError::into_inner);
        if t.is_some() {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let global = self.global.clone();
        let max_elapsed = self.max_elapsed;
        *t = std::thread::Builder::new()
            .name("-sched-".into())
            .spawn(move || sched_thread(&inner, &global, max_elapsed))
            .ok();
    }

    /// Schedule `proc_` (`Ns_ScheduleProcEx`).  Returns the event id.
    pub fn schedule(&self, proc_: SchedProc, flags: SchedFlags, repeat: Repeat, desc: &str) -> u64 {
        let (next, next_unix) = next_time(repeat);
        let id = {
            let mut g = self.lock();
            g.next_id += 1;
            let id = g.next_id;
            g.events.insert(
                id,
                Event {
                    proc_,
                    flags,
                    repeat,
                    paused: false,
                    running: false,
                    cancelled: false,
                    desc: desc.to_owned(),
                    next,
                    next_unix,
                    last_start: 0,
                    last_end: 0,
                },
            );
            id
        };
        self.ensure_thread();
        self.inner.1.notify_all();
        id
    }

    /// Cancel an event (`Ns_Cancel`/`Ns_UnscheduleProc`).  A running event
    /// finishes but is not rescheduled.  Returns whether it existed.
    pub fn cancel(&self, id: u64) -> bool {
        let mut g = self.lock();
        let found = match g.events.get_mut(&id) {
            Some(e) if e.running => {
                e.cancelled = true;
                true
            }
            Some(_) => {
                g.events.remove(&id);
                true
            }
            None => false,
        };
        drop(g);
        self.inner.1.notify_all();
        found
    }

    /// Pause (`true`) or resume (`false`) an event.  Returns whether the
    /// state changed.
    pub fn set_paused(&self, id: u64, paused: bool) -> bool {
        let mut g = self.lock();
        let changed = g.events.get_mut(&id).is_some_and(|e| {
            let ch = e.paused != paused;
            e.paused = paused;
            if !paused {
                let (n, u) = next_time(e.repeat);
                e.next = n;
                e.next_unix = u;
            }
            ch
        });
        drop(g);
        self.inner.1.notify_all();
        changed
    }

    /// Snapshot of all events.
    pub fn events(&self) -> Vec<EventInfo> {
        self.lock()
            .events
            .iter()
            .map(|(id, e)| EventInfo {
                id: *id,
                flags: e.flags,
                repeat: e.repeat,
                paused: e.paused,
                running: e.running,
                desc: e.desc.clone(),
                next_run: e.next_unix,
                last_start: e.last_start,
                last_end: e.last_end,
            })
            .collect()
    }

    /// Stop the scheduler thread and drop all events.
    pub fn shutdown(&self, timeout: Duration) {
        {
            let mut g = self.lock();
            g.shutdown = true;
            g.events.clear();
        }
        self.inner.1.notify_all();
        let handle = self
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(h) = handle {
            let deadline = Instant::now() + timeout;
            while !h.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if h.is_finished() {
                let _ = h.join();
            }
        }
    }
}

fn run_event(
    inner: &Arc<(Mutex<Inner>, Condvar)>,
    global: &Weak<Global>,
    id: u64,
    max_elapsed: Duration,
) {
    let (proc_, desc) = {
        let mut g = inner.0.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(e) = g.events.get_mut(&id) else {
            return;
        };
        e.running = true;
        e.last_start = ns_core::httptime::now();
        (Arc::clone(&e.proc_), e.desc.clone())
    };
    let start = Instant::now();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| proc_(id)));
    let elapsed = start.elapsed();
    if elapsed > max_elapsed {
        if let Some(g) = global.upgrade() {
            g.log(
                Severity::Warning,
                &format!(
                    "sched: excessive time taken by proc {id} ({desc}): {:.3}s",
                    elapsed.as_secs_f64()
                ),
            );
        }
    }
    let mut g = inner.0.lock().unwrap_or_else(PoisonError::into_inner);
    let remove = g.events.get_mut(&id).is_some_and(|e| {
        e.running = false;
        e.last_end = ns_core::httptime::now();
        if e.flags.once || e.cancelled {
            true
        } else {
            let (n, u) = next_time(e.repeat);
            e.next = n;
            e.next_unix = u;
            false
        }
    });
    if remove {
        g.events.remove(&id);
    }
    drop(g);
    inner.1.notify_all();
}

fn sched_thread(
    inner: &Arc<(Mutex<Inner>, Condvar)>,
    global: &Weak<Global>,
    max_elapsed: Duration,
) {
    let (lock, cond) = &**inner;
    let mut g = lock.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        if g.shutdown {
            return;
        }
        let now = Instant::now();
        let due: Vec<(u64, bool)> = g
            .events
            .iter()
            .filter(|(_, e)| !e.paused && !e.running && e.next <= now)
            .map(|(id, e)| (*id, e.flags.thread))
            .collect();
        if due.is_empty() {
            let next = g
                .events
                .values()
                .filter(|e| !e.paused && !e.running)
                .map(|e| e.next)
                .min();
            g = match next {
                Some(t) => {
                    cond.wait_timeout(g, t.saturating_duration_since(now))
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
                None => cond.wait(g).unwrap_or_else(PoisonError::into_inner),
            };
            continue;
        }
        drop(g);
        for (id, threaded) in due {
            if threaded {
                // Mark running before the thread starts so it is not
                // dispatched twice.
                if let Some(e) = lock
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .events
                    .get_mut(&id)
                {
                    e.running = true;
                }
                let inner = Arc::clone(inner);
                let global = global.clone();
                let _ = std::thread::Builder::new()
                    .name(format!("-sched:{id}-"))
                    .spawn(move || {
                        if let Some(e) = inner
                            .0
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .events
                            .get_mut(&id)
                        {
                            e.running = false;
                        }
                        run_event(&inner, &global, id, max_elapsed);
                    });
            } else {
                run_event(inner, global, id, max_elapsed);
            }
        }
        g = lock.lock().unwrap_or_else(PoisonError::into_inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn next_occurrence() {
        // Thursday 1970-01-01 00:00 UTC, offset 0.
        assert_eq!(
            seconds_until(
                0,
                0,
                Repeat::Daily {
                    hour: 1,
                    minute: 30
                }
            ),
            5400
        );
        assert_eq!(
            seconds_until(
                5400,
                0,
                Repeat::Daily {
                    hour: 1,
                    minute: 30
                }
            ),
            86_400
        );
        // Next Sunday 00:00 from Thursday 00:00 is 3 days.
        assert_eq!(
            seconds_until(
                0,
                0,
                Repeat::Weekly {
                    day: 0,
                    hour: 0,
                    minute: 0
                }
            ),
            3 * 86_400
        );
        // Same weekday, later time.
        assert_eq!(
            seconds_until(
                0,
                0,
                Repeat::Weekly {
                    day: 4,
                    hour: 2,
                    minute: 0
                }
            ),
            7200
        );
        assert_eq!(
            seconds_until(0, 3600, Repeat::Daily { hour: 0, minute: 0 }),
            86_400 - 3600
        );
        assert_eq!(
            seconds_until(0, 0, Repeat::Interval(Duration::from_secs(0))),
            1
        );
    }

    #[test]
    fn interval_once_cancel_pause() {
        let s = Scheduler::new(Weak::new(), Duration::from_secs(5));
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = Arc::clone(&n);
        let id = s.schedule(
            Arc::new(move |_| {
                n2.fetch_add(1, Ordering::SeqCst);
            }),
            SchedFlags {
                once: true,
                thread: false,
            },
            Repeat::Interval(Duration::from_secs(1)),
            "once",
        );
        assert_eq!(s.events().len(), 1);
        std::thread::sleep(Duration::from_millis(1300));
        assert_eq!(n.load(Ordering::SeqCst), 1);
        assert!(
            s.events().iter().all(|e| e.id != id),
            "once events are removed"
        );

        let m = Arc::new(AtomicUsize::new(0));
        let m2 = Arc::clone(&m);
        let id = s.schedule(
            Arc::new(move |_| {
                m2.fetch_add(1, Ordering::SeqCst);
            }),
            SchedFlags {
                once: false,
                thread: true,
            },
            Repeat::Interval(Duration::from_secs(1)),
            "threaded",
        );
        assert!(s.set_paused(id, true));
        assert!(!s.set_paused(id, true));
        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(m.load(Ordering::SeqCst), 0, "paused events do not run");
        assert!(s.set_paused(id, false));
        std::thread::sleep(Duration::from_millis(1300));
        assert!(m.load(Ordering::SeqCst) >= 1);
        assert!(s.cancel(id));
        assert!(!s.cancel(9999));
        s.shutdown(Duration::from_secs(2));
        assert_eq!(s.events().len(), 0);
    }
}
