//! The socket callback thread (`nsd/sockcallback.c`) and listen callbacks
//! (`nsd/listen.c`).
//!
//! [`SockCallbacks::register`] (`Ns_SockCallback`) queues a callback for a
//! descriptor; a dedicated `-socks-` thread, started on first use, moves
//! queued registrations into its table, polls every registered descriptor
//! and runs the callbacks whose conditions are met.  A callback that
//! returns `false` is dropped from the table.  At shutdown the thread
//! calls every callback registered for [`SOCK_EXIT`] once, then exits.
//! Registrations are refused once shutdown is pending.  The state machine
//! is modelled in `verus/sockcallback.rs`.
//!
//! Callbacks run on the callback thread, so a callback that needs an
//! interp allocates one there (as `NsTclSockProc` does); interps never
//! cross threads.

use std::collections::{BTreeMap, HashMap};
use std::net::Ipv4Addr;
use std::os::fd::RawFd;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ns_core::log::Severity;

use crate::global::Global;
use crate::sock;

/// Readable (`NS_SOCK_READ`).
pub const SOCK_READ: u32 = 0x01;
/// Writable (`NS_SOCK_WRITE`).
pub const SOCK_WRITE: u32 = 0x02;
/// Exceptional condition, out-of-band data (`NS_SOCK_EXCEPTION`).
pub const SOCK_EXCEPTION: u32 = 0x04;
/// Server shutdown (`NS_SOCK_EXIT`).
pub const SOCK_EXIT: u32 = 0x08;
/// Client dropped (`NS_SOCK_DROP`).
pub const SOCK_DROP: u32 = 0x10;
/// Callback cancelled (`NS_SOCK_CANCEL`).
pub const SOCK_CANCEL: u32 = 0x20;
/// Any I/O condition (`NS_SOCK_ANY`).
pub const SOCK_ANY: u32 = SOCK_READ | SOCK_WRITE | SOCK_EXCEPTION;

/// A socket callback (`Ns_SockProc`): called on the callback thread with
/// the descriptor and the condition that occurred; return `false` to be
/// removed.
pub type SockProc = Box<dyn FnMut(RawFd, u32) -> bool + Send>;

/// A listen callback: called on the callback thread with each accepted
/// socket, which it owns.
pub type ListenProc = Arc<dyn Fn(RawFd, u32) -> bool + Send + Sync>;

/// One `ns_info sockcallbacks` entry (`NsGetSockCallbacks`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackInfo {
    /// The descriptor.
    pub fd: RawFd,
    /// Registered conditions (`SOCK_*`).
    pub when: u32,
    /// The proc description (`Ns_GetProcInfo`): name, then arguments.
    pub info: Vec<String>,
}

impl CallbackInfo {
    /// The condition names, as `ns_info sockcallbacks` lists them.
    pub fn when_names(&self) -> Vec<&'static str> {
        [
            (SOCK_READ, "read"),
            (SOCK_WRITE, "write"),
            (SOCK_EXCEPTION, "exception"),
            (SOCK_EXIT, "exit"),
        ]
        .iter()
        .filter(|(bit, _)| self.when & bit != 0)
        .map(|(_, n)| *n)
        .collect()
    }
}

/// A queued registration or cancel (`struct SockCallback` on the queue).
struct Queued {
    fd: RawFd,
    when: u32,
    proc_: Option<SockProc>,
    info: Vec<String>,
}

struct Entry {
    when: u32,
    proc_: SockProc,
    info: Vec<String>,
}

/// poll(2) events for each condition (`events[]`/`when[]` in
/// `SockCallbackThread`).
const CONDITIONS: [(libc::c_short, u32); 3] = [
    (libc::POLLIN, SOCK_READ),
    (libc::POLLOUT, SOCK_WRITE),
    (libc::POLLPRI, SOCK_EXCEPTION | SOCK_DROP),
];

/// The callback thread's table of active callbacks, keyed by descriptor.
#[derive(Default)]
struct Table {
    entries: BTreeMap<RawFd, Entry>,
}

impl Table {
    /// Move queued updates into the table: a cancel removes the entry and
    /// runs its cancel proc; a registration replaces any entry for the same
    /// descriptor.
    fn apply(&mut self, queue: Vec<Queued>) {
        for q in queue {
            if q.when & SOCK_CANCEL != 0 {
                self.entries.remove(&q.fd);
                if let Some(mut p) = q.proc_ {
                    run_proc(&mut p, q.fd, SOCK_CANCEL);
                }
            } else if let Some(p) = q.proc_ {
                self.entries.insert(
                    q.fd,
                    Entry {
                        when: q.when,
                        proc_: p,
                        info: q.info,
                    },
                );
            }
        }
    }

    /// Drop entries with no I/O condition left: finished callbacks
    /// (`when == 0`), and registrations for [`SOCK_EXIT`] alone, which
    /// AOLserver frees without calling, leaking their argument.  Their
    /// procs are leaked here too, so whatever they own (a Tcl callback's
    /// descriptor) stays open.
    fn sweep(&mut self) {
        let gone: Vec<RawFd> = self
            .entries
            .iter()
            .filter(|(_, e)| e.when & SOCK_ANY == 0)
            .map(|(&fd, _)| fd)
            .collect();
        for fd in gone {
            if let Some(e) = self.entries.remove(&fd) {
                if e.when != 0 {
                    std::mem::forget(e.proc_);
                }
            }
        }
    }

    fn pollfds(&self) -> Vec<libc::pollfd> {
        self.entries
            .iter()
            .map(|(&fd, e)| libc::pollfd {
                fd,
                events: CONDITIONS
                    .iter()
                    .filter(|(_, w)| e.when & w != 0)
                    .fold(0, |ev, (p, _)| ev | p),
                revents: 0,
            })
            .collect()
    }

    /// Run the callbacks whose conditions are met; `ready` is
    /// [`Table::pollfds`] after polling.
    fn fire(&mut self, ready: &[libc::pollfd]) {
        for ((&fd, e), p) in self.entries.iter_mut().zip(ready) {
            for (ev, w) in CONDITIONS {
                if e.when & w != 0 && p.revents & ev != 0 && !run_proc(&mut e.proc_, fd, w) {
                    e.when = 0;
                }
            }
        }
    }

    /// Run every exit callback, then empty the table.
    fn exit_all(&mut self) {
        for (&fd, e) in &mut self.entries {
            if e.when & SOCK_EXIT != 0 {
                run_proc(&mut e.proc_, fd, SOCK_EXIT);
            }
        }
        self.entries.clear();
    }

    fn snapshot(&self) -> Vec<CallbackInfo> {
        self.entries
            .iter()
            .map(|(&fd, e)| CallbackInfo {
                fd,
                when: e.when,
                info: e.info.clone(),
            })
            .collect()
    }
}

/// Run a callback, treating a panic as "remove me".
fn run_proc(p: &mut SockProc, fd: RawFd, why: u32) -> bool {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| p(fd, why))).unwrap_or(false)
}

#[derive(Default)]
struct State {
    queue: Vec<Queued>,
    shutdown_pending: bool,
    running: bool,
    trigger: Option<RawFd>,
    snapshot: Vec<CallbackInfo>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    cond: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Listeners by port, then callbacks by local address (`portsTable`).
type AddrTable = Arc<Mutex<HashMap<Ipv4Addr, ListenProc>>>;

/// The socket callback system (one per process).
pub struct SockCallbacks {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
    global: Weak<Global>,
    ports: Mutex<HashMap<u16, AddrTable>>,
}

impl std::fmt::Debug for SockCallbacks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SockCallbacks").finish_non_exhaustive()
    }
}

/// Why a registration failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SockCallbackError {
    /// Shutdown is pending (`NS_ERROR` from `QueueSock`).
    #[error("socket callbacks are shutting down")]
    Shutdown,
    /// The callback thread could not be started.
    #[error("could not start the socket callback thread")]
    Thread,
    /// The listen address could not be resolved or bound.
    #[error("could not listen")]
    Listen,
    /// A listen callback is already registered for the address and port.
    #[error("listen callback already registered")]
    Duplicate,
}

impl SockCallbacks {
    /// Create the callback system; the thread starts on first
    /// registration.  `global` is used for logging, the startup wait and
    /// thread interp cleanup.
    pub fn new(global: Weak<Global>) -> Self {
        Self {
            shared: Arc::default(),
            thread: Mutex::new(None),
            global,
            ports: Mutex::new(HashMap::new()),
        }
    }

    /// Register `proc_` to run when `fd` reaches any of the `when`
    /// conditions (`Ns_SockCallback`).  `info` describes the callback for
    /// `ns_info sockcallbacks`.  A later registration for the same
    /// descriptor replaces this one.
    pub fn register(
        &self,
        fd: RawFd,
        proc_: SockProc,
        when: u32,
        info: Vec<String>,
    ) -> Result<(), SockCallbackError> {
        self.queue(Queued {
            fd,
            when,
            proc_: Some(proc_),
            info,
        })
    }

    /// Remove the callback for `fd`, then run `proc_` (if any) on the
    /// callback thread with [`SOCK_CANCEL`] (`Ns_SockCancelCallbackEx`).
    pub fn cancel(&self, fd: RawFd, proc_: Option<SockProc>) -> Result<(), SockCallbackError> {
        self.queue(Queued {
            fd,
            when: SOCK_CANCEL,
            proc_,
            info: Vec::new(),
        })
    }

    /// `QueueSock`.
    fn queue(&self, q: Queued) -> Result<(), SockCallbackError> {
        let mut st = self.shared.lock();
        if st.shutdown_pending {
            return Err(SockCallbackError::Shutdown);
        }
        let mut trigger = None;
        let mut start = None;
        if !st.running {
            let [rd, wr] = sock::pair().map_err(|_| SockCallbackError::Thread)?;
            st.running = true;
            st.trigger = Some(wr);
            start = Some(rd);
        } else if st.queue.is_empty() {
            trigger = st.trigger;
        }
        st.queue.push(q);
        drop(st);
        if let Some(fd) = trigger {
            self.trigger(fd);
        }
        if let Some(rd) = start {
            let shared = Arc::clone(&self.shared);
            let global = self.global.clone();
            let spawned = std::thread::Builder::new()
                .name("-socks-".into())
                .spawn(move || callback_thread(&shared, &global, rd));
            if let Ok(h) = spawned {
                *self.thread.lock().unwrap_or_else(PoisonError::into_inner) = Some(h);
            } else {
                let mut st = self.shared.lock();
                st.running = false;
                st.queue.clear();
                if let Some(wr) = st.trigger.take() {
                    sock::close(wr);
                }
                sock::close(rd);
                return Err(SockCallbackError::Thread);
            }
        }
        Ok(())
    }

    /// Wake the callback thread (`CallbackTrigger`).
    fn trigger(&self, fd: RawFd) {
        if let Err(e) = sock::send_byte(fd) {
            self.log(
                Severity::Error,
                &format!("socks: trigger send() failed: {}", sock::posix_error(&e)),
            );
        }
    }

    fn log(&self, sev: Severity, msg: &str) {
        if let Some(g) = self.global.upgrade() {
            g.log(sev, msg);
        }
    }

    /// Whether the callback thread is running.
    pub fn running(&self) -> bool {
        self.shared.lock().running
    }

    /// The active callbacks (`NsGetSockCallbacks`), as of the callback
    /// thread's last pass over its table.
    pub fn callbacks(&self) -> Vec<CallbackInfo> {
        let st = self.shared.lock();
        if st.running {
            st.snapshot.clone()
        } else {
            Vec::new()
        }
    }

    /// Ask the callback thread to stop (`NsStartSockShutdown`).
    pub fn start_shutdown(&self) {
        let mut st = self.shared.lock();
        if st.running {
            st.shutdown_pending = true;
            if let Some(fd) = st.trigger {
                self.trigger(fd);
            }
        }
    }

    /// Wait up to `timeout` for the callback thread to run its exit
    /// callbacks and stop (`NsWaitSockShutdown`).  Returns false (and
    /// logs a warning) on timeout.
    pub fn wait_shutdown(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut st = self.shared.lock();
        while st.running {
            let now = Instant::now();
            if now >= deadline {
                drop(st);
                self.log(
                    Severity::Warning,
                    "socks: timeout waiting for callback shutdown",
                );
                return false;
            }
            st = self
                .shared
                .cond
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        let wr = st.trigger.take();
        drop(st);
        let handle = self
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(h) = handle {
            let _ = h.join();
        }
        if let Some(fd) = wr {
            sock::close(fd);
        }
        true
    }

    /// Listen on `addr:port` (all addresses when `None`) and run `proc_`
    /// with each connection accepted for that address
    /// (`Ns_SockListenCallback`).  One listening socket per port serves
    /// every address registered on it.
    pub fn listen_callback(
        &self,
        addr: Option<&str>,
        port: u16,
        proc_: ListenProc,
    ) -> Result<(), SockCallbackError> {
        let sa = sock::get_sock_addr(addr, port).ok_or(SockCallbackError::Listen)?;
        if addr.is_some() {
            // Make sure we can bind to the specified interface.
            let mut probe = sa;
            probe.set_port(0);
            let fd = sock::bind(probe).map_err(|_| SockCallbackError::Listen)?;
            sock::close(fd);
        }
        let mut ports = self.ports.lock().unwrap_or_else(PoisonError::into_inner);
        let table = if let Some(t) = ports.get(&port) {
            Arc::clone(t)
        } else {
            let backlog = self.global.upgrade().map_or(32, |g| g.params.backlog);
            let lsock = sock::listen(None, port, backlog).map_err(|_| SockCallbackError::Listen)?;
            let _ = sock::set_blocking(lsock, false);
            let table: AddrTable = Arc::default();
            ports.insert(port, Arc::clone(&table));
            let t = Arc::clone(&table);
            let info = vec![
                format!("p:{:p}", listen_proc as fn(&AddrTable, RawFd, u32) -> bool),
                format!("a:{:p}", Arc::as_ptr(&table)),
            ];
            let _ = self.register(
                lsock,
                Box::new(move |fd, why| listen_proc(&t, fd, why)),
                SOCK_READ | SOCK_EXIT,
                info,
            );
            table
        };
        drop(ports);
        let mut t = table.lock().unwrap_or_else(PoisonError::into_inner);
        if t.contains_key(sa.ip()) {
            return Err(SockCallbackError::Duplicate);
        }
        t.insert(*sa.ip(), proc_);
        Ok(())
    }

    /// Whether a listen callback socket is open on `port`
    /// (`Ns_SockPortBound`).
    pub fn port_bound(&self, port: u16) -> bool {
        self.ports
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(&port)
    }
}

impl Drop for SockCallbacks {
    fn drop(&mut self) {
        self.start_shutdown();
    }
}

/// The listen socket's callback (`ListenProc`): accept, find the callback
/// for the local address (or `INADDR_ANY`) and hand it the connection.
fn listen_proc(table: &AddrTable, lsock: RawFd, why: u32) -> bool {
    if why == SOCK_EXIT {
        sock::close(lsock);
        return false;
    }
    if let Ok(new) = sock::accept(lsock) {
        let _ = sock::set_blocking(new, true);
        let proc_ = {
            let t = table.lock().unwrap_or_else(PoisonError::into_inner);
            sock::local_addr(new)
                .and_then(|a| t.get(&a).cloned())
                .or_else(|| t.get(&Ipv4Addr::UNSPECIFIED).cloned())
        };
        match proc_ {
            Some(p) => {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| p(new, why)));
            }
            None => sock::close(new),
        }
    }
    true
}

/// `Ns_WaitForStartup`, bounded by shutdown.
fn wait_for_startup(shared: &Shared, global: &Weak<Global>) {
    loop {
        let Some(g) = global.upgrade() else {
            return;
        };
        if g.state.started() || g.state.stopping() || shared.lock().shutdown_pending {
            return;
        }
        g.state.wait_started(Duration::from_millis(100));
    }
}

/// `SockCallbackThread`.
fn callback_thread(shared: &Arc<Shared>, global: &Weak<Global>, trig: RawFd) {
    let log = |sev: Severity, msg: &str| {
        if let Some(g) = global.upgrade() {
            g.log(sev, msg);
        }
    };
    wait_for_startup(shared, global);
    log(Severity::Notice, "socks: starting");
    let mut table = Table::default();
    loop {
        let (queue, stop) = {
            let mut st = shared.lock();
            (std::mem::take(&mut st.queue), st.shutdown_pending)
        };
        table.apply(queue);
        table.sweep();
        shared.lock().snapshot = table.snapshot();
        if stop {
            break;
        }
        let mut pfds = vec![libc::pollfd {
            fd: trig,
            events: libc::POLLIN,
            revents: 0,
        }];
        pfds.extend(table.pollfds());
        let n = match sock::poll(&mut pfds, None) {
            Ok(n) => n,
            Err(e) => {
                log(
                    Severity::Error,
                    &format!("socks: poll() failed: {}", sock::posix_error(&e)),
                );
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        if pfds[0].revents & libc::POLLIN != 0 {
            if let Err(e) = sock::recv_byte(trig) {
                log(
                    Severity::Error,
                    &format!("socks: trigger read() failed: {}", sock::posix_error(&e)),
                );
            }
        }
        if n > 0 {
            table.fire(&pfds[1..]);
        }
    }
    log(Severity::Notice, "socks: shutdown pending");
    table.exit_all();
    log(Severity::Notice, "socks: shutdown complete");
    sock::close(trig);
    if let Some(g) = global.upgrade() {
        crate::interp::thread_cleanup(&g);
    }
    let mut st = shared.lock();
    st.running = false;
    st.snapshot.clear();
    drop(st);
    shared.cond.notify_all();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    fn counting(tx: mpsc::Sender<(RawFd, u32)>, keep: bool) -> SockProc {
        Box::new(move |fd, why| {
            if why == SOCK_READ {
                let _ = sock::recv_byte(fd);
            }
            let _ = tx.send((fd, why));
            keep
        })
    }

    fn recv(rx: &mpsc::Receiver<(RawFd, u32)>) -> (RawFd, u32) {
        rx.recv_timeout(Duration::from_secs(10)).expect("callback")
    }

    #[test]
    fn register_fire_cancel_and_shutdown() {
        let cbs = SockCallbacks::new(Weak::new());
        assert!(!cbs.running());
        assert_eq!(cbs.callbacks(), Vec::new());
        let [a, b] = sock::pair().expect("pair");
        let (tx, rx) = mpsc::channel();
        cbs.register(
            a,
            counting(tx.clone(), true),
            SOCK_READ | SOCK_EXIT,
            vec!["t".into()],
        )
        .expect("register");
        assert!(cbs.running());
        sock::send_byte(b).expect("send");
        let (fd, why) = recv(&rx);
        assert_eq!((fd, why), (a, SOCK_READ));
        // Writable fires immediately; returning false removes the callback.
        let [c, d] = sock::pair().expect("pair");
        cbs.register(c, counting(tx.clone(), false), SOCK_WRITE, vec![])
            .expect("register");
        assert_eq!(recv(&rx), (c, SOCK_WRITE));
        let deadline = Instant::now() + Duration::from_secs(10);
        while cbs.callbacks().iter().any(|i| i.fd == c) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let snap = cbs.callbacks();
        assert_eq!(snap.len(), 1, "{snap:?}");
        assert_eq!(snap[0].fd, a);
        assert_eq!(snap[0].when_names(), vec!["read", "exit"]);
        assert_eq!(snap[0].info, vec!["t".to_owned()]);
        // Cancel runs the cancel proc on the callback thread.
        cbs.cancel(a, Some(counting(tx.clone(), true)))
            .expect("cancel");
        assert_eq!(recv(&rx), (a, SOCK_CANCEL));
        cbs.cancel(a, None).expect("cancel nothing");
        // An exit callback runs once at shutdown.
        cbs.register(d, counting(tx, true), SOCK_READ | SOCK_EXIT, vec![])
            .expect("register");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cbs.callbacks().iter().any(|i| i.fd == d) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        cbs.start_shutdown();
        assert!(cbs.wait_shutdown(Duration::from_secs(10)));
        assert_eq!(recv(&rx), (d, SOCK_EXIT));
        assert!(rx.try_recv().is_err());
        assert!(!cbs.running());
        assert_eq!(
            cbs.register(b, Box::new(|_, _| true), SOCK_READ, vec![]),
            Err(SockCallbackError::Shutdown)
        );
        for fd in [a, b, c, d] {
            sock::close(fd);
        }
        assert!(format!("{cbs:?}").contains("SockCallbacks"));
    }

    #[test]
    fn exit_only_registrations_are_swept_and_panics_remove() {
        let cbs = SockCallbacks::new(Weak::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let [a, b] = sock::pair().expect("pair");
        let c1 = Arc::clone(&calls);
        cbs.register(
            a,
            Box::new(move |_, _| {
                c1.fetch_add(1, Ordering::SeqCst);
                true
            }),
            SOCK_EXIT,
            vec![],
        )
        .expect("register");
        let c2 = Arc::clone(&calls);
        cbs.register(
            b,
            Box::new(move |_, _| {
                c2.fetch_add(1, Ordering::SeqCst);
                panic!("boom")
            }),
            SOCK_WRITE | SOCK_EXIT,
            vec![],
        )
        .expect("register");
        let deadline = Instant::now() + Duration::from_secs(10);
        while calls.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        while !cbs.callbacks().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(cbs.callbacks(), Vec::new());
        cbs.start_shutdown();
        assert!(cbs.wait_shutdown(Duration::from_secs(10)));
        // The exit-only callback was swept unrun and leaked, as AOLserver
        // leaks its argument; the panicking one ran once and was dropped.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(Arc::strong_count(&calls), 2);
        sock::close(a);
        sock::close(b);
        // Shutdown with nothing running is a no-op.
        let idle = SockCallbacks::new(Weak::new());
        idle.start_shutdown();
        assert!(idle.wait_shutdown(Duration::from_millis(10)));
    }

    #[test]
    fn listen_callbacks_dispatch_by_address() {
        let cbs = SockCallbacks::new(Weak::new());
        let probe = sock::listen(Some("127.0.0.1"), 0, 5).expect("probe");
        let port = sock::local_port(probe).expect("port");
        sock::close(probe);
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let proc_: ListenProc = Arc::new(move |fd, why| {
            let _ = tx
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .send((fd, why));
            sock::close(fd);
            true
        });
        cbs.listen_callback(Some("127.0.0.1"), port, Arc::clone(&proc_))
            .expect("listen");
        assert!(cbs.port_bound(port));
        assert!(!cbs.port_bound(port.wrapping_add(1)));
        assert_eq!(
            cbs.listen_callback(Some("127.0.0.1"), port, Arc::clone(&proc_)),
            Err(SockCallbackError::Duplicate)
        );
        assert_eq!(
            cbs.listen_callback(Some("no.such.host.invalid"), port, Arc::clone(&proc_)),
            Err(SockCallbackError::Listen)
        );
        assert_eq!(
            cbs.listen_callback(Some("192.0.2.1"), port, Arc::clone(&proc_)),
            Err(SockCallbackError::Listen),
            "an address this host does not have"
        );
        let c = sock::connect("127.0.0.1", port, None, 0, false).expect("connect");
        let (_, why) = rx.recv_timeout(Duration::from_secs(10)).expect("accepted");
        assert_eq!(why, SOCK_READ);
        sock::close(c);
        // A second listener on the same port shares the socket and
        // dispatches by local address; INADDR_ANY catches the rest.
        let (tx2, rx2) = mpsc::channel::<RawFd>();
        let tx2 = Mutex::new(tx2);
        cbs.listen_callback(
            None,
            port,
            Arc::new(move |fd, _| {
                let _ = tx2.lock().unwrap_or_else(PoisonError::into_inner).send(fd);
                sock::close(fd);
                true
            }),
        )
        .expect("any");
        let c = sock::connect("127.0.0.2", port, None, 0, false).expect("connect .2");
        rx2.recv_timeout(Duration::from_secs(10))
            .expect("any accepted");
        sock::close(c);
        cbs.start_shutdown();
        assert!(cbs.wait_shutdown(Duration::from_secs(10)));
        assert!(sock::connect("127.0.0.1", port, None, 0, false).is_err());
    }

    #[test]
    fn table_model() {
        let mut t = Table::default();
        let noop = || -> SockProc { Box::new(|_, _| true) };
        t.apply(vec![
            Queued {
                fd: 3,
                when: SOCK_READ,
                proc_: Some(noop()),
                info: vec![],
            },
            Queued {
                fd: 3,
                when: SOCK_WRITE | SOCK_EXCEPTION,
                proc_: Some(noop()),
                info: vec![],
            },
            Queued {
                fd: 4,
                when: SOCK_CANCEL,
                proc_: None,
                info: vec![],
            },
        ]);
        assert_eq!(t.entries.len(), 1);
        let p = t.pollfds();
        assert_eq!(p[0].events, libc::POLLOUT | libc::POLLPRI);
        t.exit_all();
        assert!(t.entries.is_empty());
    }

    /// What the model callbacks of [`table_matches_model`] record.
    #[derive(Default)]
    struct Log {
        calls: Vec<(usize, u32)>,
        dropped: Vec<usize>,
        misuse: bool,
    }

    fn lock(log: &Mutex<Log>) -> MutexGuard<'_, Log> {
        log.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records the drop of callback `id`.
    struct Guard(usize, Arc<Mutex<Log>>);

    impl Drop for Guard {
        fn drop(&mut self) {
            lock(&self.1).dropped.push(self.0);
        }
    }

    /// A callback that logs its calls and returns `keep` from I/O
    /// conditions; a call after returning false is misuse.
    fn model_proc(id: usize, keep: bool, log: &Arc<Mutex<Log>>) -> SockProc {
        let g = Guard(id, Arc::clone(log));
        let mut finished = false;
        Box::new(move |_, why| {
            let mut l = lock(&g.1);
            l.misuse |= finished;
            l.calls.push((g.0, why));
            if why & (SOCK_ANY | SOCK_DROP) != 0 {
                finished = !keep;
            }
            keep
        })
    }

    #[derive(Debug, Clone)]
    enum Op {
        Reg(RawFd, u32, bool),
        Cancel(RawFd, bool),
        Apply,
        Sweep,
        Fire(Vec<u8>),
        Exit,
    }

    fn op() -> impl proptest::strategy::Strategy<Value = Op> {
        use proptest::prelude::*;
        prop_oneof![
            (0..4, 1u32..16, any::<bool>()).prop_map(|(fd, w, k)| Op::Reg(fd, w, k)),
            (0..4, any::<bool>()).prop_map(|(fd, p)| Op::Cancel(fd, p)),
            Just(Op::Apply),
            Just(Op::Sweep),
            proptest::collection::vec(0u8..8, 4).prop_map(Op::Fire),
            Just(Op::Exit),
        ]
    }

    fn revents(mask: u8) -> libc::c_short {
        [libc::POLLIN, libc::POLLOUT, libc::POLLPRI]
            .iter()
            .enumerate()
            .filter(|(i, _)| mask & (1 << i) != 0)
            .fold(0, |r, (_, e)| r | e)
    }

    /// One registration or cancel in the reference model.
    struct ModelQ {
        fd: RawFd,
        when: u32,
        id: Option<usize>,
        keep: bool,
    }

    proptest::proptest! {
        /// The callback table against a reference model of
        /// `SockCallbackThread` (the accounting of `verus/sockcallback.rs`):
        /// cancels and replacements drop the old callback, the sweep drops
        /// finished callbacks and leaks exit-only ones, callbacks run only
        /// for registered conditions and never after returning false, and
        /// every accepted callback ends up dropped or leaked.
        #[test]
        fn table_matches_model(ops in proptest::collection::vec(op(), 0..40)) {
            let log = Arc::new(Mutex::new(Log::default()));
            let mut t = Table::default();
            let mut queue = Vec::new();
            let mut m_queue: Vec<ModelQ> = Vec::new();
            let mut m_table: BTreeMap<RawFd, (usize, u32, bool)> = BTreeMap::new();
            let mut calls = Vec::new();
            let mut dropped = Vec::new();
            let mut leaked = Vec::new();
            let mut next = 0;
            for op in ops {
                match op {
                    Op::Reg(fd, when, keep) => {
                        queue.push(Queued { fd, when, proc_: Some(model_proc(next, keep, &log)), info: vec![] });
                        m_queue.push(ModelQ { fd, when, id: Some(next), keep });
                        next += 1;
                    }
                    Op::Cancel(fd, with_proc) => {
                        let id = with_proc.then_some(next);
                        queue.push(Queued { fd, when: SOCK_CANCEL, proc_: id.map(|i| model_proc(i, true, &log)), info: vec![] });
                        m_queue.push(ModelQ { fd, when: SOCK_CANCEL, id, keep: true });
                        next += usize::from(with_proc);
                    }
                    Op::Apply => {
                        t.apply(std::mem::take(&mut queue));
                        for q in std::mem::take(&mut m_queue) {
                            if q.when & SOCK_CANCEL != 0 {
                                if let Some((old, _, _)) = m_table.remove(&q.fd) {
                                    dropped.push(old);
                                }
                                if let Some(id) = q.id {
                                    calls.push((id, SOCK_CANCEL));
                                    dropped.push(id);
                                }
                            } else if let Some(id) = q.id {
                                if let Some((old, _, _)) = m_table.insert(q.fd, (id, q.when, q.keep)) {
                                    dropped.push(old);
                                }
                            }
                        }
                    }
                    Op::Sweep => {
                        t.sweep();
                        m_table.retain(|_, &mut (id, when, _)| {
                            if when & SOCK_ANY != 0 {
                                return true;
                            }
                            if when == 0 { dropped.push(id) } else { leaked.push(id) }
                            false
                        });
                    }
                    Op::Fire(masks) => {
                        let mut p = t.pollfds();
                        for (i, pfd) in p.iter_mut().enumerate() {
                            pfd.revents = revents(masks[i % masks.len()]);
                        }
                        t.fire(&p);
                        for (i, e) in m_table.values_mut().enumerate() {
                            let r = revents(masks[i % masks.len()]);
                            for (ev, w) in CONDITIONS {
                                if e.1 & w != 0 && r & ev != 0 {
                                    calls.push((e.0, w));
                                    if !e.2 {
                                        e.1 = 0;
                                    }
                                }
                            }
                        }
                    }
                    Op::Exit => {
                        t.exit_all();
                        for (id, when, _) in std::mem::take(&mut m_table).into_values() {
                            if when & SOCK_EXIT != 0 {
                                calls.push((id, SOCK_EXIT));
                            }
                            dropped.push(id);
                        }
                    }
                }
                let got: Vec<(RawFd, u32)> = t.entries.iter().map(|(&fd, e)| (fd, e.when)).collect();
                let want: Vec<(RawFd, u32)> = m_table.iter().map(|(&fd, e)| (fd, e.1)).collect();
                proptest::prop_assert_eq!(got, want);
                let l = lock(&log);
                proptest::prop_assert!(!l.misuse);
                proptest::prop_assert_eq!(&l.calls, &calls);
                let mut a = l.dropped.clone();
                let mut b = dropped.clone();
                a.sort_unstable();
                b.sort_unstable();
                proptest::prop_assert_eq!(a, b);
            }
            drop(t);
            drop(queue);
            // Accounting: every accepted callback was dropped or leaked,
            // never both.
            let mut all = lock(&log).dropped.clone();
            all.extend(&leaked);
            all.sort_unstable();
            proptest::prop_assert_eq!(all, (0..next).collect::<Vec<_>>());
        }
    }
}
