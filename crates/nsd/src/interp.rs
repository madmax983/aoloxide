//! Interpreter management (`nsd/tclinit.c`).
//!
//! Interpreters are cached per thread and per server, exactly like
//! AOLserver: [`allocate`] pops a cached interp (or creates one, running
//! `create` traces) and runs `allocate` traces (normally `ns_init`, which
//! applies any newer `ns_ictl save` script); dropping the [`InterpLease`]
//! runs `deallocate` traces (normally `ns_cleanup`) and returns the interp
//! to the cache, or destroys it if `ns_markfordelete` was called.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Deref;
use std::rc::{Rc, Weak};
use std::sync::{Arc, PoisonError};

use ns_core::log::Severity;
use ns_script::{Exception, Interp, ScriptResult};

use crate::adp::AdpState;
use crate::conn::{ConnCtx, ConnRef, SetRef, Status};
use crate::server::Server;

/// When an interp trace runs (`NS_TCL_TRACE_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TraceWhen {
    /// After creation.
    Create,
    /// Before deletion.
    Delete,
    /// Each time the interp is allocated.
    Allocate,
    /// Each time the interp is released.
    Deallocate,
    /// When attached to a connection.
    GetConn,
    /// When detached from a connection.
    FreeConn,
}

impl TraceWhen {
    /// Parse an `ns_ictl trace` name.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "create" => Self::Create,
            "delete" => Self::Delete,
            "allocate" => Self::Allocate,
            "deallocate" => Self::Deallocate,
            "getconn" => Self::GetConn,
            "freeconn" => Self::FreeConn,
            _ => return None,
        })
    }

    /// Name used by `ns_ictl gettraces`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Delete => "delete",
            Self::Allocate => "allocate",
            Self::Deallocate => "deallocate",
            Self::GetConn => "getconn",
            Self::FreeConn => "freeconn",
        }
    }
}

/// A native interp trace callback.
pub type NativeTrace = Arc<dyn Fn(&dyn Interp) -> ScriptResult + Send + Sync>;

/// What a trace does.
#[derive(Clone)]
pub enum TraceAction {
    /// Evaluate a script.
    Script(String),
    /// Call native code.
    Native(NativeTrace),
}

impl std::fmt::Debug for TraceAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Script(s) => write!(f, "Script({s:?})"),
            Self::Native(_) => f.write_str("Native"),
        }
    }
}

/// A registered interp trace.
#[derive(Debug, Clone)]
pub struct InterpTrace {
    /// When it runs.
    pub when: TraceWhen,
    /// What it does.
    pub action: TraceAction,
}

/// Per-interp server state (`struct NsInterp`).
pub struct NsInterp {
    /// The interp's server.
    pub server: Arc<Server>,
    /// The connection being served, if any.
    pub conn: Option<Weak<ConnCtx>>,
    /// Private sets.
    pub sets: HashMap<String, SetRef>,
    /// Next private set id.
    pub next_set: u64,
    /// ADP evaluation state.
    pub adp: AdpState,
    /// Epoch of the init script last applied.
    pub epoch: u64,
    /// `Ns_TclRegisterDefer` callbacks run by `ns_ictl cleanup`.
    pub defer: Vec<Box<dyn FnOnce(&dyn Interp)>>,
    /// Set ids handed out for the current connection's form, headers and
    /// output headers (`itPtr->nsconn`).
    pub conn_sets: HashMap<&'static str, String>,
}

impl std::fmt::Debug for NsInterp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NsInterp")
            .field("server", &self.server.name)
            .finish_non_exhaustive()
    }
}

/// Shared handle to [`NsInterp`].
pub type NsInterpRef = Rc<RefCell<NsInterp>>;

/// The [`NsInterp`] of `interp` (`NsGetInterpData`).
pub fn ns_interp(interp: &dyn Interp) -> Result<NsInterpRef, Exception> {
    interp
        .ext()
        .get::<NsInterpRef>()
        .ok_or_else(|| Exception::error("interp not associated with a server"))
}

/// The server of `interp`.
pub fn interp_server(interp: &dyn Interp) -> Result<Arc<Server>, Exception> {
    Ok(Arc::clone(&ns_interp(interp)?.borrow().server))
}

/// The connection `interp` is serving (`NsTclGetConn`).
pub fn interp_conn(interp: &dyn Interp) -> Result<ConnRef, Exception> {
    ns_interp(interp)?
        .borrow()
        .conn
        .as_ref()
        .and_then(Weak::upgrade)
        .ok_or_else(|| Exception::error("no connection"))
}

/// The connection, if any, without erroring.
pub fn try_conn(interp: &dyn Interp) -> Option<ConnRef> {
    interp_conn(interp).ok()
}

/// Private `ns_set`s of an interp with no server (see [`init_bare`]).
#[derive(Debug, Default)]
pub struct BareSets {
    /// Private sets.
    pub sets: HashMap<String, SetRef>,
    /// Next private set id.
    pub next_set: u64,
}

/// Shared handle to [`BareSets`].
pub type BareSetsRef = Rc<RefCell<BareSets>>;

/// Prepare an interp created outside any virtual server, as
/// `CreateInterp(NULL)` does for `Ns_TclCreateInterp` (used by `nsproxy`
/// workers): add every core command (`NsTclAddCmds`).  As in AOLserver the
/// server-independent commands (`ns_time`, `ns_fmttime`, private `ns_set`s,
/// `ns_log` to stderr, ...) work; commands that need a server fail with
/// "interp not associated with a server".
pub fn init_bare(interp: &dyn Interp) {
    interp.ext().insert::<BareSetsRef>(Rc::default());
    for (name, cmd) in crate::cmds::core_commands() {
        interp.register(name, Arc::clone(cmd));
    }
}

thread_local! {
    static CACHE: RefCell<HashMap<String, Vec<Box<dyn Interp>>>> = RefCell::new(HashMap::new());
}

/// Run every trace registered for `when` on `interp`.
pub fn run_traces(server: &Server, interp: &dyn Interp, when: TraceWhen) {
    let traces: Vec<InterpTrace> = server
        .tcl
        .traces
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter(|t| t.when == when)
        .cloned()
        .collect();
    for t in traces {
        let r = match &t.action {
            TraceAction::Script(s) => interp.eval_global(s),
            TraceAction::Native(f) => f(interp),
        };
        if let Err(e) = r {
            log_error(server, interp, &e);
        }
    }
}

/// Register an interp trace (`Ns_TclRegisterTrace`).
pub fn register_trace(server: &Server, when: TraceWhen, action: TraceAction) {
    server
        .tcl
        .traces
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .push(InterpTrace { when, action });
}

fn create(server: &Arc<Server>) -> Result<Box<dyn Interp>, Exception> {
    let lock = server.global.params.tcl_init_lock.then(|| {
        static INIT: std::sync::Mutex<()> = std::sync::Mutex::new(());
        INIT.lock().unwrap_or_else(PoisonError::into_inner)
    });
    let interp = server.global.engine.create_interp()?;
    interp
        .ext()
        .insert::<NsInterpRef>(Rc::new(RefCell::new(NsInterp {
            server: Arc::clone(server),
            conn: None,
            sets: HashMap::new(),
            next_set: 0,
            adp: AdpState::default(),
            epoch: 0,
            defer: Vec::new(),
            conn_sets: HashMap::new(),
        })));
    for (name, cmd) in crate::cmds::core_commands() {
        interp.register(name, Arc::clone(cmd));
    }
    let extra = server
        .tcl
        .commands
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    for (name, cmd) in extra {
        interp.register(&name, cmd);
    }
    run_traces(server, &*interp, TraceWhen::Create);
    drop(lock);
    Ok(interp)
}

fn destroy(server: &Server, interp: Box<dyn Interp>) {
    run_traces(server, &*interp, TraceWhen::Delete);
    drop(interp);
}

/// An allocated interpreter; returned to the thread cache on drop.
pub struct InterpLease {
    interp: Option<Box<dyn Interp>>,
    server: Arc<Server>,
}

impl std::fmt::Debug for InterpLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InterpLease")
            .field("server", &self.server.name)
            .finish()
    }
}

impl Deref for InterpLease {
    type Target = dyn Interp;

    fn deref(&self) -> &Self::Target {
        // The option is only emptied in Drop.
        self.interp
            .as_deref()
            .unwrap_or_else(|| unreachable!("interp taken"))
    }
}

impl InterpLease {
    /// The interp's server.
    pub fn server(&self) -> &Arc<Server> {
        &self.server
    }
}

impl Drop for InterpLease {
    fn drop(&mut self) {
        let Some(interp) = self.interp.take() else {
            return;
        };
        run_traces(&self.server, &*interp, TraceWhen::Deallocate);
        if interp.marked_for_delete() {
            destroy(&self.server, interp);
        } else {
            let name = self.server.name.clone();
            CACHE.with(|c| c.borrow_mut().entry(name).or_default().push(interp));
        }
    }
}

/// Allocate an interp for `server` on this thread (`Ns_TclAllocateInterp`).
pub fn allocate(server: &Arc<Server>) -> Result<InterpLease, Exception> {
    let cached = CACHE.with(|c| c.borrow_mut().get_mut(&server.name).and_then(Vec::pop));
    let interp = match cached {
        Some(i) => i,
        None => create(server)?,
    };
    run_traces(server, &*interp, TraceWhen::Allocate);
    Ok(InterpLease {
        interp: Some(interp),
        server: Arc::clone(server),
    })
}

/// Attach a connection to an allocated interp.
pub fn attach_conn(lease: &InterpLease, conn: Weak<ConnCtx>) {
    if let Ok(st) = ns_interp(&**lease) {
        st.borrow_mut().conn = Some(conn);
    }
    run_traces(&lease.server, &**lease, TraceWhen::GetConn);
}

/// Detach the connection from an interp.
pub fn detach_conn(lease: &InterpLease) {
    run_traces(&lease.server, &**lease, TraceWhen::FreeConn);
    if let Ok(st) = ns_interp(&**lease) {
        let mut s = st.borrow_mut();
        s.conn = None;
        s.adp = AdpState::default();
        let ids: Vec<String> = s.conn_sets.drain().map(|(_, v)| v).collect();
        for id in ids {
            s.sets.remove(&id);
        }
    }
}

/// Destroy every interp cached by this thread and release engine
/// resources.  Called by connection, job and scheduler threads on exit.
pub fn thread_cleanup(global: &crate::global::Global) {
    let all: HashMap<String, Vec<Box<dyn Interp>>> =
        CACHE.with(|c| std::mem::take(&mut *c.borrow_mut()));
    for (name, interps) in all {
        let server = global.server(&name);
        for i in interps {
            match &server {
                Some(s) => destroy(s, i),
                None => drop(i),
            }
        }
    }
    global.engine.finalize_thread();
}

/// Run the server's Tcl bootstrap (`NsTclInitServer`): evaluate the init
/// file in a fresh interp, which sources the Tcl libraries, saves the
/// resulting procs/namespaces with `ns_ictl save`, and registers the
/// standard traces.
pub fn init_server(server: &Arc<Server>) -> Result<(), Exception> {
    let lease = allocate(server)?;
    let file = server.tcl.initfile.to_string_lossy().into_owned();
    if server.tcl.initfile.is_file() {
        if let Err(e) = lease.eval_file(&file) {
            log_error(server, &*lease, &e);
        }
    } else {
        server.global.log(
            Severity::Warning,
            &format!("tcl: init file not found: {file}; using built-in bootstrap"),
        );
        if let Err(e) = lease.eval_global(crate::init_tcl::INIT_TCL) {
            log_error(server, &*lease, &e);
        }
    }
    drop(lease);
    Ok(())
}

/// Log a script error with connection details (`Ns_TclLogError`).
pub fn log_error(server: &Server, interp: &dyn Interp, e: &Exception) {
    let mut msg = e.detail().to_owned();
    if let Some(conn) = try_conn(interp) {
        let c = conn.conn();
        msg = format!(
            "{msg}\n    while processing connection #{}:\n        {}",
            c.id, c.request.line
        );
    }
    server.global.log(Severity::Error, &msg);
}

/// Evaluate `proc_` for a connection (`Ns_TclRequest`).
pub fn tcl_request(conn: &ConnRef, proc_: &str) -> Status {
    let interp = match conn.interp() {
        Ok(i) => i,
        Err(e) => {
            conn.server.global.log(Severity::Error, &e.message);
            let _ = conn.return_internal_error();
            return Status::Ok;
        }
    };
    if let Err(e) = interp.eval(proc_) {
        log_error(&conn.server, &**interp, &e);
        if conn.reset_return() {
            let _ = conn.return_internal_error();
        }
    }
    Status::Ok
}

/// Allocate an interp, run `f`, and release it.
pub fn with_interp<R>(
    server: &Arc<Server>,
    f: impl FnOnce(&dyn Interp) -> R,
) -> Result<R, Exception> {
    let lease = allocate(server)?;
    Ok(f(&*lease))
}

/// Evaluate a script in a server interp, logging errors (used by
/// scheduled procs, callbacks and jobs).
pub fn eval_logged(server: &Arc<Server>, script: &str) -> ScriptResult {
    let lease = allocate(server)?;
    let r = lease.eval_global(script);
    if let Err(e) = &r {
        log_error(server, &*lease, e);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_names() {
        for w in [
            TraceWhen::Create,
            TraceWhen::Delete,
            TraceWhen::Allocate,
            TraceWhen::Deallocate,
            TraceWhen::GetConn,
            TraceWhen::FreeConn,
        ] {
            assert_eq!(TraceWhen::parse(w.name()), Some(w));
        }
        assert_eq!(TraceWhen::parse("x"), None);
        assert!(format!("{:?}", TraceAction::Script("a".into())).contains('a'));
    }

    #[test]
    fn bare_interp_has_server_independent_commands() {
        use ns_script::ScriptEngine;
        let i = ns_tcl::TclEngine::new().create_interp().expect("interp");
        init_bare(&*i);
        assert_eq!(
            i.eval("string is integer -strict [ns_time]")
                .expect("ns_time"),
            "1"
        );
        let s = i.eval("ns_set create t a 1").expect("ns_set");
        assert_eq!(i.eval(&format!("ns_set get {s} a")).expect("get"), "1");
        assert_eq!(i.eval("ns_set list").expect("list"), s);
        i.eval("ns_set cleanup").expect("cleanup");
        assert_eq!(i.eval("ns_set list").expect("list"), "");
        i.eval("ns_log notice bare").expect("ns_log to stderr");
        let e = i.eval("ns_info server").expect_err("no server");
        assert_eq!(e.message, "interp not associated with a server");
    }
}
