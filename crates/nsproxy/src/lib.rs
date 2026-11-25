//! # nsproxy
//!
//! The AOLserver proxy module (`nsproxy/nsproxylib.c`): pools of worker
//! processes, each running its own Tcl interp, for scripts that must not
//! share the server's address space.
//!
//! ```tcl
//! ns_proxy config mypool -max 2 -evaltimeout 5000
//! set h [ns_proxy get mypool]
//! ns_proxy eval $h {expr 6*7}
//! ns_proxy release $h
//! ```
//!
//! Subcommands, as in AOLserver 4.5:
//!
//! | command | effect |
//! |---|---|
//! | `get pool ?-timeout ms? ?-handles n?` | check out handles (list of ids) |
//! | `release handle` | return a handle, running the pool's `-reinit` |
//! | `eval handle script ?ms?` | `send` + `wait` + `recv` |
//! | `send handle script` / `wait handle ?ms?` / `recv handle` | asynchronous evaluation |
//! | `ping handle` | evaluate an empty script |
//! | `cleanup` | release every handle this interp holds |
//! | `config pool ?-opt val ...?` | set and return pool options |
//! | `active pool` | `{id script ...}` of handles currently evaluating |
//!
//! Handles are owned by the interp that got them and are released when the
//! interp is deallocated (after each connection), like `Ns_ProxyCleanup`.
//!
//! Each proxy is a state machine (`Idle → Busy → Done → Idle`); see
//! `verus/proxy.rs` for the proof that the transitions below are the only
//! ones and that the pool's `avail` accounting is preserved.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use ns_script::{command, wrong_args, Code, Exception, Interp, ScriptResult};
use nsd::{ModuleContext, NsError};

pub mod proto;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Extended errors (`Err` in `nsproxylib.c`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyErr {
    /// Handle is evaluating a script.
    Busy,
    /// Worker process has gone away.
    Dead,
    /// Waiting for handles would deadlock.
    Deadlock,
    /// Worker could not be started.
    Exec,
    /// Timed out waiting for a handle.
    GetTimeout,
    /// No script is being evaluated.
    Idle,
    /// Malformed worker response.
    Import,
    /// The `-init` script failed.
    Init,
    /// More handles requested than the pool allows.
    Range,
    /// Reading the result failed.
    Recv,
    /// Sending the script failed.
    Send,
    /// `recv` without `wait`.
    NoWait,
    /// Timed out waiting for the result.
    EvalTimeout,
}

impl ProxyErr {
    /// `(code, message)` as set by `ProxyError`.
    pub const fn describe(self) -> (&'static str, &'static str) {
        match self {
            Self::Busy => ("EBusy", "currently evaluating a script"),
            Self::Dead => ("EDead", "child process died"),
            Self::Deadlock => ("EDeadlock", "allocation deadlock"),
            Self::Exec => ("EExec", "could not create child process"),
            Self::GetTimeout => ("EGetTimeout", "timeout waiting for handle"),
            Self::Idle => ("EIdle", "no script evaluating"),
            Self::Import => ("EImport", "invalided response"),
            Self::Init => ("EInit", "init script failed"),
            Self::Range => ("ERange", "insufficient handles"),
            Self::Recv => ("ERecv", "result recv failed"),
            Self::Send => ("ESend", "script send failed"),
            Self::NoWait => ("ENoWait", "no wait for script result"),
            Self::EvalTimeout => ("EEvalTimeout", "timeout waiting for evaluation"),
        }
    }

    /// An exception with `prefix` + message and errorCode
    /// `NSPROXY code msg ?sysmsg?`.
    fn exception(self, prefix: &str, sysmsg: Option<&str>) -> Exception {
        let (code, msg) = self.describe();
        let mut ec = format!("NSPROXY {code} {{{msg}}}");
        if let Some(s) = sysmsg {
            ec.push_str(&format!(" {{{s}}}"));
        }
        Exception::error(format!("{prefix}{msg}")).error_code(ec)
    }
}

/// Pool options (`ns_proxy config`).  Timeouts are milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyConfig {
    /// Worker executable.
    pub exec: PathBuf,
    /// Script evaluated when a worker starts.
    pub init: Option<String>,
    /// Script evaluated when a handle is released.
    pub reinit: Option<String>,
    /// Minimum workers (recorded only, as in AOLserver).
    pub min: usize,
    /// Maximum handles.
    pub max: usize,
    /// `get` timeout.
    pub get_timeout: u64,
    /// `eval`/`wait` timeout.
    pub eval_timeout: u64,
    /// Send timeout.
    pub send_timeout: u64,
    /// Receive timeout.
    pub recv_timeout: u64,
    /// Time a closing worker is given to exit before it is killed.
    pub wait_timeout: u64,
}

/// Default worker: `nsproxy` next to the running binary (`${bindir}`).
pub fn default_exec() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("nsproxy")))
        .unwrap_or_else(|| PathBuf::from("nsproxy"))
}

impl Default for ProxyConfig {
    /// AOLserver's defaults (`GetPool`).
    fn default() -> Self {
        Self {
            exec: default_exec(),
            init: None,
            reinit: None,
            min: 0,
            max: 5,
            get_timeout: 500,
            eval_timeout: 500,
            send_timeout: 100,
            recv_timeout: 100,
            wait_timeout: 100,
        }
    }
}

/// A running worker process (`Proc`).
struct Worker {
    child: Option<Child>,
    stdin: Option<BufWriter<ChildStdin>>,
    replies: Receiver<Option<proto::Reply>>,
    linger: Duration,
    pool: String,
}

impl Worker {
    fn exec(cfg: &ProxyConfig, pool: &str, id: &str) -> Result<Self, String> {
        let mut child = Command::new(&cfg.exec)
            .args([pool, id])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| e.to_string())?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            return Err("no pipes".into());
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            // `None` marks a response that could not be decoded.
            while let Ok(Some(m)) = proto::read_msg(&mut r) {
                if tx.send(proto::Reply::decode(&m)).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child: Some(child),
            stdin: Some(BufWriter::new(stdin)),
            replies: rx,
            linger: Duration::from_millis(cfg.wait_timeout),
            pool: pool.to_owned(),
        })
    }

    fn send(&mut self, script: &str) -> std::io::Result<()> {
        let w = self
            .stdin
            .as_mut()
            .ok_or_else(|| std::io::Error::other("closed"))?;
        proto::write_msg(w, script.as_bytes())?;
        w.flush()
    }
}

impl Drop for Worker {
    /// `Close`: close the worker's stdin so it exits, and hand the process
    /// to a background thread that reaps it (`CloseThread`).
    fn drop(&mut self) {
        drop(self.stdin.take());
        if let Some(mut child) = self.child.take() {
            let (linger, pool) = (self.linger, std::mem::take(&mut self.pool));
            std::thread::spawn(move || {
                reap(&mut child, linger, &pool);
            });
        }
    }
}

/// How [`reap`] disposed of a closing worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reaped {
    /// Exited within `-waittimeout` of its stdin closing.
    Exited,
    /// Exited after `SIGTERM`.
    Terminated,
    /// Exited after `SIGKILL`.
    Killed,
    /// Still running after `SIGKILL`; left unreaped.
    Zombie,
}

/// The server log of the process, once the module is loaded.
static LOG_GLOBAL: Mutex<Option<std::sync::Weak<nsd::Global>>> = Mutex::new(None);

/// `Ns_Log(Warning, ...)`: the server log if reachable, else stderr.
fn warn(msg: &str) {
    let g = lock(&LOG_GLOBAL)
        .as_ref()
        .and_then(std::sync::Weak::upgrade);
    match g {
        Some(g) => g.log(ns_core::log::Severity::Warning, msg),
        None => eprintln!("Warning: {msg}"),
    }
}

/// Wait up to `ms` for `child` to exit (`WaitFd` on the result pipe).
fn exited_within(child: &mut Child, ms: Duration) -> bool {
    let deadline = Instant::now() + ms;
    loop {
        if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// `Kill`: log and signal a worker that won't die.
fn kill_worker(child: &mut Child, pool: &str, sig: i32) {
    warn(&format!(
        "[{pool}]: process {} won't die - sending sig {sig}",
        child.id()
    ));
    if sig == libc::SIGKILL {
        let _ = child.kill();
    } else if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: kill(2) with a pid we spawned and have not yet reaped.
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

/// `CloseThread`: give a closing worker `linger` to exit, then `SIGTERM`
/// it, then `SIGKILL` it, waiting `linger` after each; a worker that
/// survives even that is logged as a zombie.
fn reap(child: &mut Child, linger: Duration, pool: &str) -> Reaped {
    if exited_within(child, linger) {
        return Reaped::Exited;
    }
    kill_worker(child, pool, libc::SIGTERM);
    if exited_within(child, linger) {
        return Reaped::Terminated;
    }
    kill_worker(child, pool, libc::SIGKILL);
    if exited_within(child, linger) {
        return Reaped::Killed;
    }
    warn(&format!("zombie: {}", child.id()));
    Reaped::Zombie
}

/// Proxy evaluation state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Ready for a script.
    Idle,
    /// A script was sent.
    Busy,
    /// The result is ready to be received.
    Done,
}

/// A proxy handle (`Proxy`).
struct Proxy {
    id: String,
    pool: Arc<Pool>,
    worker: Option<Worker>,
    state: State,
    script: String,
    /// The received reply, or why there is none.
    reply: Option<Result<proto::Reply, ProxyErr>>,
}

struct PoolInner {
    cfg: ProxyConfig,
    /// Handles that may still be handed out (may go negative after
    /// `-max` is lowered).
    avail: i64,
    /// Free proxies (`firstPtr`).
    free: VecDeque<Proxy>,
    waiting: bool,
    /// Handles held plus wanted by interps in `get` (`wthave + wtwant`;
    /// C keeps the two separately but only ever reads their sum).
    waiting_total: usize,
    next_id: u64,
    /// Scripts of checked-out proxies that are evaluating, for `active`.
    active: HashMap<String, String>,
}

/// A proxy pool.
pub struct Pool {
    /// Pool name.
    pub name: String,
    inner: Mutex<PoolInner>,
    cond: Condvar,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Pool {
    /// Current configuration.
    pub fn config(&self) -> ProxyConfig {
        lock(&self.inner).cfg.clone()
    }
}

/// `GetPool`: find or create a pool.
pub fn get_pool(name: &str) -> Arc<Pool> {
    static POOLS: OnceLock<Mutex<HashMap<String, Arc<Pool>>>> = OnceLock::new();
    let mut pools = lock(POOLS.get_or_init(Mutex::default));
    Arc::clone(pools.entry(name.to_owned()).or_insert_with(|| {
        let cfg = ProxyConfig::default();
        Arc::new(Pool {
            name: name.to_owned(),
            inner: Mutex::new(PoolInner {
                avail: i64::try_from(cfg.max).unwrap_or(i64::MAX),
                cfg,
                free: VecDeque::new(),
                waiting: false,
                waiting_total: 0,
                next_id: 0,
                active: HashMap::new(),
            }),
            cond: Condvar::new(),
        })
    }))
}

impl Proxy {
    fn cfg(&self) -> ProxyConfig {
        self.pool.config()
    }

    /// `Reset`: a proxy left mid-evaluation has its worker closed.
    fn reset(&mut self) {
        if self.state != State::Idle {
            self.worker = None;
            self.state = State::Idle;
        }
        self.script.clear();
        self.reply = None;
        lock(&self.pool.inner).active.remove(&self.id);
    }

    /// `Send`.
    fn send(&mut self, script: &str) -> Result<(), Exception> {
        let prefix = || {
            format!(
                "could not send script \"{script}\" to proxy \"{}\": ",
                self.id
            )
        };
        let Some(w) = self.worker.as_mut() else {
            return Err(ProxyErr::Dead.exception(&prefix(), None));
        };
        if self.state != State::Idle {
            return Err(ProxyErr::Busy.exception(&prefix(), None));
        }
        self.state = State::Busy;
        script.clone_into(&mut self.script);
        if let Err(e) = w.send(script) {
            let p = prefix();
            self.reset();
            return Err(ProxyErr::Send.exception(&p, Some(&e.to_string())));
        }
        lock(&self.pool.inner)
            .active
            .insert(self.id.clone(), script.to_owned());
        Ok(())
    }

    /// `Wait`: `ms` of `None` uses the pool's `-evaltimeout`.
    fn wait(&mut self, ms: Option<u64>) -> Result<(), Exception> {
        let prefix = || format!("wait for proxy \"{}\" failed: ", self.id);
        match self.state {
            State::Idle => return Err(ProxyErr::Idle.exception(&prefix(), None)),
            State::Done => return Ok(()),
            State::Busy => {}
        }
        let ms = ms.unwrap_or_else(|| self.cfg().eval_timeout);
        let Some(w) = self.worker.as_ref() else {
            return Err(ProxyErr::EvalTimeout.exception(&prefix(), None));
        };
        let got = if ms == 0 {
            match w.replies.try_recv() {
                Ok(r) => Some(r.ok_or(ProxyErr::Import)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(ProxyErr::Recv)),
            }
        } else {
            match w.replies.recv_timeout(Duration::from_millis(ms)) {
                Ok(r) => Some(r.ok_or(ProxyErr::Import)),
                Err(RecvTimeoutError::Timeout) => None,
                // EOF is "readable" to poll(); recv then fails.
                Err(RecvTimeoutError::Disconnected) => Some(Err(ProxyErr::Recv)),
            }
        };
        match got {
            Some(r) => {
                self.reply = Some(r);
                self.state = State::Done;
                Ok(())
            }
            None => Err(ProxyErr::EvalTimeout.exception(&prefix(), None)),
        }
    }

    /// `Recv`.
    fn recv(&mut self) -> ScriptResult {
        let prefix = format!("could not receive result from proxy \"{}\": ", self.id);
        match self.state {
            State::Idle => return Err(ProxyErr::Idle.exception(&prefix, None)),
            State::Busy => return Err(ProxyErr::NoWait.exception(&prefix, None)),
            State::Done => {}
        }
        let out = match self.reply.take() {
            Some(Ok(r)) => {
                self.state = State::Idle;
                r.into_result()
            }
            Some(Err(e)) => Err(e.exception(&prefix, None)),
            None => Err(ProxyErr::Recv.exception(&prefix, None)),
        };
        // A failed receive leaves the state at Done, so reset closes it.
        self.reset();
        out
    }

    /// `Eval`: `script` of `None` is a ping.
    fn eval(&mut self, script: Option<&str>, ms: Option<u64>) -> ScriptResult {
        self.send(script.unwrap_or(""))?;
        self.wait(ms)?;
        self.recv()
    }

    /// `Close` after a failed check: reset (dropping any `active` entry
    /// and returning to `Idle`) and close the worker even if it is idle.
    fn close(&mut self) {
        self.reset();
        self.worker = None;
    }

    /// `Check`: ping a live worker (restarting it if the ping fails), or
    /// start one and run the pool's `-init` script.
    fn check(&mut self) -> Result<(), Exception> {
        if self.worker.is_some() && self.eval(None, None).is_err() {
            self.close();
        }
        if self.worker.is_none() {
            let cfg = self.cfg();
            match Worker::exec(&cfg, &self.pool.name, &self.id) {
                Ok(w) => self.worker = Some(w),
                Err(e) => return Err(ProxyErr::Exec.exception("", Some(&e))),
            }
            if let Some(init) = &cfg.init {
                if self.eval(Some(init), None).is_err() {
                    self.close();
                    return Err(ProxyErr::Init.exception("", None));
                }
            }
        }
        Ok(())
    }
}

/// `PutProxy`: back on the free list, or freed if `avail` went negative.
fn put_proxy(p: Proxy) {
    let pool = Arc::clone(&p.pool);
    let mut inner = lock(&pool.inner);
    inner.avail += 1;
    inner.active.remove(&p.id);
    if inner.avail > 0 {
        inner.free.push_back(p);
    } else {
        drop(p);
    }
    drop(inner);
    pool.cond.notify_all();
}

/// `ReleaseProxy`: reset, run `-reinit`, return to the pool.
fn release_proxy(mut p: Proxy) -> ScriptResult {
    p.reset();
    let result = match p.cfg().reinit {
        Some(reinit) => p.eval(Some(&reinit), None),
        None => Ok(String::new()),
    };
    put_proxy(p);
    result
}

/// Per-interp handle table (`InterpData`).
#[derive(Default)]
struct InterpData {
    ids: HashMap<String, Proxy>,
    /// Handles held per pool (`cnts`).
    cnts: HashMap<String, usize>,
}

impl InterpData {
    fn release(&mut self, id: &str) -> Option<ScriptResult> {
        let p = self.ids.remove(id)?;
        if let Some(n) = self.cnts.get_mut(&p.pool.name) {
            *n = n.saturating_sub(1);
        }
        Some(release_proxy(p))
    }

    /// `ReleaseHandles`.
    fn release_all(&mut self) {
        let ids: Vec<String> = self.ids.keys().cloned().collect();
        for id in ids {
            let _ = self.release(&id);
        }
    }
}

impl Drop for InterpData {
    /// `DeleteData`: an interp being deleted returns its handles.
    fn drop(&mut self) {
        self.release_all();
    }
}

type InterpDataRef = Rc<RefCell<InterpData>>;

fn interp_data(interp: &dyn Interp) -> InterpDataRef {
    if let Some(d) = interp.ext().get::<InterpDataRef>() {
        return d;
    }
    let d = InterpDataRef::default();
    interp.ext().insert(Rc::clone(&d));
    d
}

/// `Ns_ProxyCleanup`: release every handle held by `interp`.
pub fn cleanup(interp: &dyn Interp) {
    if let Some(d) = interp.ext().get::<InterpDataRef>() {
        d.borrow_mut().release_all();
    }
}

/// `PopProxy` loop of `GetObjCmd`: wait to be the exclusive waiter, then
/// take `nwant` proxies.  On failure, also returns how many were still
/// wanted (C's `nwant`, which picks "handle" or "handles").
fn pop_proxies(
    pool: &Arc<Pool>,
    nhave: usize,
    nwant: usize,
    ms: u64,
) -> Result<Vec<Proxy>, (ProxyErr, usize)> {
    let deadline = Instant::now() + Duration::from_millis(ms);
    let mut inner = lock(&pool.inner);
    let (mut nhave, mut nwant, mut waiter) = (nhave, nwant, false);
    let mut got: Vec<Proxy> = Vec::new();
    let mut err = None;
    while err.is_none() && nwant > 0 {
        // nhave + nwant is constant within an iteration.
        inner.waiting_total += nhave + nwant;
        let max = inner.cfg.max;
        let mut timed_out = false;
        if nhave + nwant > max {
            err = Some(ProxyErr::Range);
        } else if !waiter {
            if inner.waiting_total > max {
                err = Some(ProxyErr::Deadlock);
            } else if !inner.waiting {
                inner.waiting = true;
                waiter = true;
            } else {
                let now = Instant::now();
                if now >= deadline {
                    timed_out = true;
                } else {
                    let (g, r) = pool
                        .cond
                        .wait_timeout(inner, deadline - now)
                        .unwrap_or_else(PoisonError::into_inner);
                    inner = g;
                    timed_out = r.timed_out();
                }
            }
        }
        if timed_out {
            err = Some(ProxyErr::GetTimeout);
        }
        if err.is_none() && waiter {
            if inner.avail > 0 {
                inner.avail -= 1;
                let p = if let Some(p) = inner.free.pop_front() {
                    p
                } else {
                    let id = format!("{}-proxy-{}", pool.name, inner.next_id);
                    inner.next_id += 1;
                    Proxy {
                        id,
                        pool: Arc::clone(pool),
                        worker: None,
                        state: State::Idle,
                        script: String::new(),
                        reply: None,
                    }
                };
                got.push(p);
                nhave += 1;
                nwant -= 1;
            } else {
                let now = Instant::now();
                let to = if now >= deadline {
                    true
                } else {
                    let (g, r) = pool
                        .cond
                        .wait_timeout(inner, deadline - now)
                        .unwrap_or_else(PoisonError::into_inner);
                    inner = g;
                    r.timed_out()
                };
                if to {
                    err = Some(ProxyErr::GetTimeout);
                }
            }
        }
        inner.waiting_total -= nhave + nwant;
    }
    if waiter {
        inner.waiting = false;
    }
    let (out, dropped) = match err {
        None => (Ok(got), Vec::new()),
        Some(e) => (Err((e, nwant)), give_back(&mut inner, got)),
    };
    drop(inner);
    drop(dropped);
    pool.cond.notify_all();
    out
}

/// A failed `get` returns the proxies it took, front first.  Like
/// `put_proxy`, a proxy goes back on the free list only while capacity
/// remains (`-max` may have been lowered while the lock was dropped for
/// `check`); the rest are returned for the caller to free outside the lock.
fn give_back(inner: &mut PoolInner, got: Vec<Proxy>) -> Vec<Proxy> {
    let mut dropped = Vec::new();
    for p in got.into_iter().rev() {
        inner.avail += 1;
        if inner.avail > 0 {
            inner.free.push_front(p);
        } else {
            dropped.push(p);
        }
    }
    dropped
}

fn get_cmd(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    const USAGE: &str = "pool ?-opt val -opt val ...?";
    if argv.len() < 3 || argv.len() % 2 != 1 {
        return Err(wrong_args(argv, 2, USAGE));
    }
    let pool = get_pool(&argv[2]);
    let data = interp_data(interp);
    let nhave = data.borrow().cnts.get(&pool.name).copied().unwrap_or(0);
    let (mut nwant, mut nwant_str, mut ms) = (1usize, "1".to_owned(), pool.config().get_timeout);
    for kv in argv[3..].chunks(2) {
        let flags = ["-timeout", "-handles"];
        let flag = ns_script::get_index(&kv[0], &flags, "flags")?;
        let n = nsd::cmds::int_arg(&kv[1])?;
        let Ok(n) = u64::try_from(n) else {
            // As in C, which prints `objv[2]`: the pool name.
            return Err(Exception::error(format!(
                "invalid {}: {}",
                flags[flag], argv[2]
            )));
        };
        if flag == 0 {
            ms = n;
        } else {
            nwant_str.clone_from(&kv[1]);
            nwant = usize::try_from(n).unwrap_or(usize::MAX);
        }
    }
    let fail = |e: Exception, n: usize| {
        let msg = format!(
            "could not get {nwant_str} handle{} from pool \"{}\": {}",
            if n > 1 { "s" } else { "" },
            pool.name,
            e.message
        );
        Exception { message: msg, ..e }
    };
    let mut got = pop_proxies(&pool, nhave, nwant, ms)
        .map_err(|(e, left)| fail(e.exception("", None), left))?;
    // Check (ping or start) each proxy outside the pool lock.
    let mut bad = None;
    for p in &mut got {
        if let Err(e) = p.check() {
            bad = Some(e);
            break;
        }
    }
    if let Some(e) = bad {
        let dropped = give_back(&mut lock(&pool.inner), got);
        drop(dropped);
        pool.cond.notify_all();
        // Every handle was taken, so C's remaining `nwant` is 0.
        return Err(fail(e, 0));
    }
    let mut d = data.borrow_mut();
    *d.cnts.entry(pool.name.clone()).or_default() += got.len();
    let ids: Vec<String> = got.iter().map(|p| p.id.clone()).collect();
    for p in got {
        d.ids.insert(p.id.clone(), p);
    }
    drop(d);
    Ok(interp.list_from(&ids))
}

fn config_cmd(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    const FLAGS: [&str; 10] = [
        "-init",
        "-reinit",
        "-min",
        "-max",
        "-exec",
        "-getimeout",
        "-evaltimeout",
        "-sendtimeout",
        "-recvtimeout",
        "-waittimeout",
    ];
    if argv.len() < 3 || argv.len() % 2 != 1 {
        return Err(wrong_args(argv, 2, "pool ?-opt val -opt val ...?"));
    }
    let pool = get_pool(&argv[2]);
    let mut inner = lock(&pool.inner);
    let max = i64::try_from(inner.cfg.max).unwrap_or(i64::MAX);
    let nrun = max - inner.avail;
    for kv in argv[3..].chunks(2) {
        // `-gettimeout` is accepted as an alias of C's `-getimeout`.
        let flag = if kv[0] == "-gettimeout" {
            5
        } else {
            ns_script::get_index(&kv[0], &FLAGS, "flags")?
        };
        let v = &kv[1];
        let set_opt = |v: &str| (!v.is_empty()).then(|| v.to_owned());
        match flag {
            0 => inner.cfg.init = set_opt(v),
            1 => inner.cfg.reinit = set_opt(v),
            4 => inner.cfg.exec = PathBuf::from(v),
            _ => {
                let n = nsd::cmds::int_arg(v)?;
                let Ok(n) = u64::try_from(n) else {
                    return Err(Exception::error(format!("invalid {}: {v}", FLAGS[flag])));
                };
                let c = &mut inner.cfg;
                match flag {
                    2 => c.min = usize::try_from(n).unwrap_or(usize::MAX),
                    3 => c.max = usize::try_from(n).unwrap_or(usize::MAX),
                    5 => c.get_timeout = n,
                    6 => c.eval_timeout = n,
                    7 => c.send_timeout = n,
                    8 => c.recv_timeout = n,
                    _ => c.wait_timeout = n,
                }
            }
        }
    }
    // Adjust limits and dump lingering proxies.
    if inner.cfg.min > inner.cfg.max {
        inner.cfg.min = inner.cfg.max;
    }
    inner.avail = i64::try_from(inner.cfg.max).unwrap_or(i64::MAX) - nrun;
    let stale: Vec<Proxy> = inner.free.drain(..).collect();
    let c = inner.cfg.clone();
    drop(inner);
    drop(stale);
    pool.cond.notify_all();
    Ok(interp.list_from(&[
        "-exec".into(),
        c.exec.to_string_lossy().into_owned(),
        "-init".into(),
        c.init.unwrap_or_default(),
        "-reinit".into(),
        c.reinit.unwrap_or_default(),
        "-max".into(),
        c.max.to_string(),
        "-min".into(),
        c.min.to_string(),
        "-getimeout".into(),
        c.get_timeout.to_string(),
        "-evaltimeout".into(),
        c.eval_timeout.to_string(),
        "-sendtimeout".into(),
        c.send_timeout.to_string(),
        "-recvtimeout".into(),
        c.recv_timeout.to_string(),
        "-waittimeout".into(),
        c.wait_timeout.to_string(),
    ]))
}

fn timeout_arg(s: Option<&String>) -> Result<Option<u64>, Exception> {
    s.map(|s| nsd::cmds::int_arg(s).map(|n| u64::try_from(n).ok()))
        .transpose()
        .map(Option::flatten)
}

/// The `ns_proxy` command.
pub fn ns_proxy(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (_, opt) = nsd::cmds::subcommand(
        argv,
        &[
            "get", "release", "eval", "cleanup", "config", "ping", "active", "send", "wait", "recv",
        ],
    )?;
    let argc = argv.len();
    let with_proxy = |f: &mut dyn FnMut(&mut Proxy) -> ScriptResult| -> ScriptResult {
        let data = interp_data(interp);
        let mut d = data.borrow_mut();
        let p = d
            .ids
            .get_mut(&argv[2])
            .ok_or_else(|| Exception::error(format!("no such handle: {}", argv[2])))?;
        f(p)
    };
    match opt {
        "get" => get_cmd(interp, argv),
        "config" => config_cmd(interp, argv),
        "release" | "ping" => {
            if argc != 3 {
                return Err(wrong_args(argv, 2, "handle"));
            }
            if opt == "ping" {
                return with_proxy(&mut |p| p.eval(None, None));
            }
            let data = interp_data(interp);
            let r = data.borrow_mut().release(&argv[2]);
            r.unwrap_or_else(|| Err(Exception::error(format!("no such handle: {}", argv[2]))))
        }
        "cleanup" => {
            if argc != 2 {
                return Err(wrong_args(argv, 2, ""));
            }
            cleanup(interp);
            Ok(String::new())
        }
        "send" => {
            if argc != 4 {
                return Err(wrong_args(argv, 2, "handle script"));
            }
            with_proxy(&mut |p| p.send(&argv[3]).map(|()| String::new()))
        }
        "wait" => {
            if argc != 3 && argc != 4 {
                return Err(wrong_args(argv, 2, "handle ?timeout?"));
            }
            let ms = timeout_arg(argv.get(3))?;
            with_proxy(&mut |p| p.wait(ms).map(|()| String::new()))
        }
        "recv" => {
            if argc != 3 {
                return Err(wrong_args(argv, 2, "handle"));
            }
            with_proxy(&mut Proxy::recv)
        }
        "eval" => {
            if argc != 4 && argc != 5 {
                return Err(wrong_args(argv, 2, "handle script ?timeout?"));
            }
            let ms = timeout_arg(argv.get(4))?;
            with_proxy(&mut |p| p.eval(Some(&argv[3]), ms))
        }
        _ => {
            if argc != 3 {
                return Err(wrong_args(argv, 2, "pool"));
            }
            let pool = get_pool(&argv[2]);
            let inner = lock(&pool.inner);
            let mut active: Vec<(&String, &String)> = inner.active.iter().collect();
            active.sort();
            let out: Vec<String> = active
                .into_iter()
                .flat_map(|(id, s)| [id.clone(), s.clone()])
                .collect();
            drop(inner);
            Ok(interp.list_from(&out))
        }
    }
}

/// Move the protocol off fds 0 and 1 (`Ns_ProxyMain`) "to avoid protocol
/// errors with scripts accessing stdin and stdout": the pipes are
/// duplicated to private descriptors, stdin is reopened on `/dev/null` and
/// stdout becomes a copy of stderr.  The private descriptors are
/// close-on-exec so commands a script `exec`s cannot hold the pipes open.
fn private_stdio() -> std::io::Result<(File, File)> {
    use std::os::fd::{FromRawFd, OwnedFd};
    let dup = |fd: i32| -> std::io::Result<OwnedFd> {
        // SAFETY: fcntl on a descriptor number; the result, if valid, is a
        // new descriptor we own.
        let n = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `n` is a freshly created descriptor owned by nobody else.
        Ok(unsafe { OwnedFd::from_raw_fd(n) })
    };
    let (rfd, wfd) = (dup(0)?, dup(1)?);
    let null = std::fs::File::open("/dev/null")?;
    // SAFETY: dup2 onto the standard descriptors, which this process owns;
    // `null` and fd 2 are valid for the duration of the calls.
    unsafe {
        if libc::dup2(std::os::fd::AsRawFd::as_raw_fd(&null), 0) < 0 || libc::dup2(2, 1) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok((File::from(rfd), File::from(wfd)))
}

/// The worker process main loop (`Ns_ProxyMain`): read scripts from the
/// server, evaluate them in an interp with the core commands
/// (`Ns_TclCreateInterp`), write the replies back.
pub fn worker_main(engine: &dyn ns_script::ScriptEngine) -> i32 {
    let Ok((rfd, wfd)) = private_stdio() else {
        return 1;
    };
    let Ok(interp) = engine.create_interp() else {
        return 1;
    };
    nsd::interp::init_bare(&*interp);
    let mut input = BufReader::new(rfd);
    let mut output = BufWriter::new(wfd);
    while let Ok(Some(script)) = proto::read_msg(&mut input) {
        let script = String::from_utf8_lossy(&script);
        let reply = if script.is_empty() {
            proto::Reply::ok(String::new())
        } else {
            match interp.eval_global(&script) {
                Ok(r) => proto::Reply::ok(r),
                Err(e) => proto::Reply::from_exception(e),
            }
        };
        if proto::write_msg(&mut output, &reply.encode()).is_err() || output.flush().is_err() {
            break;
        }
    }
    0
}

impl proto::Reply {
    /// The reply for an exception.
    pub fn from_exception(e: Exception) -> Self {
        Self {
            code: e.code.as_i32(),
            error_code: e.error_code.unwrap_or_default(),
            error_info: e.error_info.unwrap_or_default(),
            result: e.message,
        }
    }

    /// Back to a script result (`Import`).
    pub fn into_result(self) -> ScriptResult {
        let Some(code) = Code::from_i32(self.code) else {
            return Ok(self.result);
        };
        Err(Exception {
            code,
            message: self.result,
            error_info: (!self.error_info.is_empty()).then_some(self.error_info),
            error_code: (!self.error_code.is_empty()).then_some(self.error_code),
        })
    }
}

/// Module init: adds `ns_proxy` and the deallocate trace that releases
/// handles (`Ns_ProxyCleanup`).
pub fn init(ctx: &ModuleContext<'_>) -> Result<(), NsError> {
    let server = ctx
        .server
        .ok_or_else(|| NsError::Config("nsproxy must be loaded as a per-server module".into()))?;
    *lock(&LOG_GLOBAL) = Some(Arc::downgrade(ctx.global));
    server.add_command("ns_proxy", command(ns_proxy));
    nsd::interp::register_trace(
        server,
        nsd::interp::TraceWhen::Deallocate,
        nsd::interp::TraceAction::Native(Arc::new(|interp| {
            cleanup(interp);
            Ok(String::new())
        })),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn bare_proxy(pool: &Arc<Pool>, id: &str) -> Proxy {
        Proxy {
            id: id.to_owned(),
            pool: Arc::clone(pool),
            worker: None,
            state: State::Idle,
            script: String::new(),
            reply: None,
        }
    }

    #[test]
    fn give_back_frees_proxies_beyond_capacity() {
        let pool = get_pool("unit-give-back");
        let mut inner = lock(&pool.inner);
        // Two proxies were taken, then `-max` was lowered under them.
        inner.avail = -1;
        let got = vec![bare_proxy(&pool, "a"), bare_proxy(&pool, "b")];
        let dropped = give_back(&mut inner, got);
        assert_eq!(inner.avail, 1);
        // "b" brings avail to 0 and is freed; "a" brings it to 1 and stays.
        let free: Vec<&str> = inner.free.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(free, ["a"]);
        assert_eq!(
            dropped.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            ["b"]
        );
        // With capacity, every proxy goes back in its original order.
        inner.free.clear();
        inner.avail = 0;
        let got = vec![bare_proxy(&pool, "c"), bare_proxy(&pool, "d")];
        assert!(give_back(&mut inner, got).is_empty());
        let free: Vec<&str> = inner.free.iter().map(|p| p.id.as_str()).collect();
        assert_eq!((free, inner.avail), (vec!["c", "d"], 2));
        inner.free.clear();
    }

    fn spawn(script: &str) -> Child {
        Command::new("/bin/sh")
            .args(["-c", script])
            .spawn()
            .expect("spawn sh")
    }

    #[test]
    fn reap_escalates_from_sigterm_to_sigkill() {
        let ms = Duration::from_millis;
        let mut c = spawn("exit 0");
        assert_eq!(reap(&mut c, ms(2000), "p"), Reaped::Exited);
        let mut c = spawn("exec sleep 30");
        assert_eq!(reap(&mut c, ms(50), "p"), Reaped::Terminated);
        assert_eq!(c.wait().expect("status").signal(), Some(libc::SIGTERM));
        let mut c = spawn("trap '' TERM; exec sleep 30");
        assert_eq!(reap(&mut c, ms(50), "p"), Reaped::Killed);
        assert_eq!(c.wait().expect("status").signal(), Some(libc::SIGKILL));
    }

    #[test]
    fn error_codes_match_aolserver() {
        let e = ProxyErr::GetTimeout.exception("x: ", None);
        assert_eq!(e.message, "x: timeout waiting for handle");
        assert_eq!(
            e.error_code.as_deref(),
            Some("NSPROXY EGetTimeout {timeout waiting for handle}")
        );
        let e = ProxyErr::Exec.exception("", Some("No such file"));
        assert_eq!(
            e.error_code.as_deref(),
            Some("NSPROXY EExec {could not create child process} {No such file}")
        );
        for e in [
            ProxyErr::Busy,
            ProxyErr::Dead,
            ProxyErr::Deadlock,
            ProxyErr::Idle,
            ProxyErr::Import,
            ProxyErr::Init,
            ProxyErr::Range,
            ProxyErr::Recv,
            ProxyErr::Send,
            ProxyErr::NoWait,
            ProxyErr::EvalTimeout,
        ] {
            assert!(e.describe().0.starts_with('E'));
        }
    }

    #[test]
    fn defaults_match_aolserver() {
        let c = ProxyConfig::default();
        assert_eq!(
            (c.max, c.min, c.get_timeout, c.eval_timeout),
            (5, 0, 500, 500)
        );
        assert_eq!(
            (c.send_timeout, c.recv_timeout, c.wait_timeout),
            (100, 100, 100)
        );
    }

    #[test]
    fn reply_into_result() {
        assert_eq!(proto::Reply::ok("v".into()).into_result(), Ok("v".into()));
        let e = proto::Reply::from_exception(Exception::error("boom").error_code("X Y"))
            .into_result()
            .expect_err("err");
        assert_eq!(
            (e.code, e.message.as_str(), e.error_code.as_deref()),
            (Code::Error, "boom", Some("X Y"))
        );
        let r = proto::Reply {
            code: 3,
            error_code: String::new(),
            error_info: String::new(),
            result: String::new(),
        };
        assert_eq!(r.into_result().expect_err("break").code, Code::Break);
    }
}
