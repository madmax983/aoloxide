//! # ns-script
//!
//! The scripting seam of the AOLserver port.  AOLserver is built around an
//! embedded Tcl interpreter; this crate abstracts the operations the server
//! needs from *any* embedded scripting engine so that every `ns_*` command is
//! written once, against [`Interp`], and engines plug in behind
//! [`ScriptEngine`].  The first (and reference) engine is `ns-tcl`, which
//! links the real Tcl 8.6 library.  See `docs/adr/0002-scripting-trait.md`.
//!
//! Design rules:
//!
//! * Interpreters are **thread-bound** (`!Send`), exactly like Tcl interps:
//!   each connection thread owns its own interps.
//! * All [`Interp`] methods take `&self`; engines use interior mutability so
//!   that commands may re-enter the interpreter (a command evaluating a
//!   script that calls another command) without aliasing `&mut`.
//! * Values cross the boundary as UTF-8 strings.  Lists are built and split
//!   by the engine so quoting always matches the engine's syntax.
//! * Non-`OK` completion codes (`error`, `return`, `break`, `continue`) are
//!   carried by [`Exception`].

use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

/// Non-OK script completion codes (`TCL_ERROR`, `TCL_RETURN`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Code {
    /// An error (`TCL_ERROR`).
    Error,
    /// `return` was invoked (`TCL_RETURN`).
    Return,
    /// `break` was invoked (`TCL_BREAK`).
    Break,
    /// `continue` was invoked (`TCL_CONTINUE`).
    Continue,
    /// Any other application-defined code.
    Other(i32),
}

impl Code {
    /// The numeric Tcl code.
    pub const fn as_i32(self) -> i32 {
        match self {
            Self::Error => 1,
            Self::Return => 2,
            Self::Break => 3,
            Self::Continue => 4,
            Self::Other(n) => n,
        }
    }

    /// From a numeric Tcl code; `0` (OK) yields `None`.
    pub const fn from_i32(n: i32) -> Option<Self> {
        match n {
            0 => None,
            1 => Some(Self::Error),
            2 => Some(Self::Return),
            3 => Some(Self::Break),
            4 => Some(Self::Continue),
            n => Some(Self::Other(n)),
        }
    }
}

/// A non-OK completion: the code plus the interpreter result and, for
/// errors, the engine's error trace and machine-readable error code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct Exception {
    /// Completion code.
    pub code: Code,
    /// The interpreter result (the error message for errors).
    pub message: String,
    /// `errorInfo`-style stack trace, when available.
    pub error_info: Option<String>,
    /// `errorCode`-style machine-readable code, when set.
    pub error_code: Option<String>,
}

impl fmt::Display for Exception {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Exception {
    /// An error with `message`.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            code: Code::Error,
            message: message.into(),
            error_info: None,
            error_code: None,
        }
    }

    /// An exception with an arbitrary code.
    pub fn with_code(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            error_info: None,
            error_code: None,
        }
    }

    /// Attach an `errorCode`.
    #[must_use]
    pub fn error_code(mut self, code: impl Into<String>) -> Self {
        self.error_code = Some(code.into());
        self
    }

    /// True for [`Code::Error`].
    pub fn is_error(&self) -> bool {
        self.code == Code::Error
    }

    /// The best available description: the stack trace if present,
    /// otherwise the message.
    pub fn detail(&self) -> &str {
        self.error_info.as_deref().unwrap_or(&self.message)
    }
}

/// Result of evaluating a script or command.
pub type ScriptResult = Result<String, Exception>;

/// A command implementation.  `argv[0]` is the command name, as with Tcl's
/// `objv`.  Commands are shared across threads (they usually capture an
/// `Arc` of server state), but are always invoked on the interp's thread.
pub type Command = Arc<dyn Fn(&dyn Interp, &[String]) -> ScriptResult + Send + Sync>;

/// Build a [`Command`] from a closure.
pub fn command<F>(f: F) -> Command
where
    F: Fn(&dyn Interp, &[String]) -> ScriptResult + Send + Sync + 'static,
{
    Arc::new(f)
}

/// Per-interpreter typed storage (the Rust analogue of `Tcl_SetAssocData`).
///
/// Values are stored once per type and handed out by clone; store
/// `Rc<RefCell<T>>` for mutable state.  Never hold a borrow of a value
/// across a call that may re-enter the interpreter.
#[derive(Default)]
pub struct Extensions {
    map: RefCell<HashMap<TypeId, Box<dyn Any>>>,
}

impl fmt::Debug for Extensions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Extensions")
            .field("len", &self.map.borrow().len())
            .finish()
    }
}

impl Extensions {
    /// Store `value`, replacing any value of the same type.
    pub fn insert<T: 'static>(&self, value: T) {
        self.map
            .borrow_mut()
            .insert(TypeId::of::<T>(), Box::new(value));
    }

    /// Clone out the value of type `T`, if present.
    pub fn get<T: Clone + 'static>(&self) -> Option<T> {
        self.map
            .borrow()
            .get(&TypeId::of::<T>())
            .and_then(|b| b.downcast_ref::<T>())
            .cloned()
    }

    /// Remove and return the value of type `T`.
    pub fn remove<T: 'static>(&self) -> Option<T> {
        self.map
            .borrow_mut()
            .remove(&TypeId::of::<T>())
            .and_then(|b| b.downcast::<T>().ok())
            .map(|b| *b)
    }

    /// Get the shared cell of type `T`, creating it with `init` if absent.
    pub fn shared<T: 'static>(&self, init: impl FnOnce() -> T) -> Rc<RefCell<T>> {
        if let Some(v) = self.get::<Rc<RefCell<T>>>() {
            return v;
        }
        let v = Rc::new(RefCell::new(init()));
        self.insert(Rc::clone(&v));
        v
    }
}

/// The events a variable trace sees (`TCL_TRACE_*` flags).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TraceOps {
    /// The variable is being read (`TCL_TRACE_READS`).
    pub reads: bool,
    /// The variable is being written (`TCL_TRACE_WRITES`).
    pub writes: bool,
    /// The variable is being unset (`TCL_TRACE_UNSETS`).
    pub unsets: bool,
    /// An `array` subcommand is inspecting it (`TCL_TRACE_ARRAY`).
    pub array: bool,
    /// The trace is being removed after this call (`TCL_TRACE_DESTROYED`).
    pub destroyed: bool,
    /// The interp is being deleted (`TCL_INTERP_DESTROYED`); the callback
    /// then gets no interp.
    pub interp_destroyed: bool,
}

impl TraceOps {
    /// Reads, writes, unsets and array operations: everything `ns_share`
    /// traces.
    pub const ALL: Self = Self {
        reads: true,
        writes: true,
        unsets: true,
        array: true,
        destroyed: false,
        interp_destroyed: false,
    };
}

/// A variable trace callback (`Tcl_VarTraceProc`): called with the interp
/// (`None` while it is being deleted), the variable name as the script
/// used it, the array element (if any) and the events.  Traces are
/// thread-bound like their interp.
pub type VarTrace = Rc<dyn Fn(Option<&dyn Interp>, &str, Option<&str>, TraceOps)>;

/// Interrupts script evaluation in the thread that created it, the next
/// time that thread's engine checks for asynchronous events
/// (`Tcl_AsyncMark` on the handler `ns_ictl cancel` uses).  May be called
/// from any thread.
pub trait Canceller: Send + Sync {
    /// Request the cancellation.  Returns `false` if the thread has
    /// already released its engine resources.
    fn cancel(&self) -> bool;
}

/// An engine channel cut from its interp and thread (`Tcl_CutChannel`),
/// so that it can be handed to another thread (`ns_chan`).  Only the
/// engine that detached it can attach or close it again; a detached
/// channel that is dropped is leaked (it stays open), as in AOLserver.
pub struct DetachedChannel {
    engine: &'static str,
    name: String,
    id: usize,
    inner: Box<dyn Any + Send>,
}

impl fmt::Debug for DetachedChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DetachedChannel")
            .field("engine", &self.engine)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl DetachedChannel {
    /// Wrap an engine's detached channel.  `id` is the channel's identity
    /// (the same value [`Interp::channel_id`] reports once it is attached).
    pub fn new(engine: &'static str, name: String, id: usize, inner: Box<dyn Any + Send>) -> Self {
        Self {
            engine,
            name,
            id,
            inner,
        }
    }

    /// The engine that detached the channel.
    pub const fn engine(&self) -> &'static str {
        self.engine
    }

    /// The channel's name (`Tcl_GetChannelName`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The channel's identity.
    pub const fn id(&self) -> usize {
        self.id
    }

    /// The engine's own state, for the engine to downcast.
    pub fn into_inner(self) -> Box<dyn Any + Send> {
        self.inner
    }
}

fn unsupported_by(engine: &str, what: &str) -> Exception {
    Exception::error(format!("{what} is not supported by the {engine} engine"))
}

/// An embedded interpreter instance.  See the crate docs for the rules.
pub trait Interp {
    /// Engine name, e.g. `"tcl"`.
    fn engine(&self) -> &'static str;

    /// Evaluate `script` in the current call frame.
    fn eval(&self, script: &str) -> ScriptResult;

    /// Evaluate `script` at global level (`TCL_EVAL_GLOBAL`).
    fn eval_global(&self, script: &str) -> ScriptResult;

    /// Invoke a command from already-split words, without re-parsing
    /// (`Tcl_EvalObjv`).
    fn eval_words(&self, words: &[&str]) -> ScriptResult;

    /// Evaluate `script` in the frame named by `level` (`uplevel`).
    ///
    /// Native commands do not push a call frame, so from inside a command
    /// level `0` is the frame that invoked the command and level `1` is its
    /// caller — the same as `Tcl_EvalObjv("uplevel ...")` from C.
    fn uplevel(&self, level: &str, script: &str) -> ScriptResult {
        self.eval_words(&["uplevel", level, script])
    }

    /// Source a script file.
    fn eval_file(&self, path: &str) -> ScriptResult {
        self.eval_words(&["source", path])
    }

    /// Set a variable in the current frame.  `name` may be `arr(key)`.
    fn set_var(&self, name: &str, value: &str) -> Result<(), Exception>;

    /// Read a variable from the current frame.
    fn get_var(&self, name: &str) -> Option<String>;

    /// Unset a variable in the current frame (missing variables are fine).
    fn unset_var(&self, name: &str);

    /// Set a global variable.
    fn set_global(&self, name: &str, value: &str) -> Result<(), Exception>;

    /// Read a global variable.
    fn get_global(&self, name: &str) -> Option<String>;

    /// Set one element of an array variable in the current frame.
    fn set_array_elem(&self, array: &str, key: &str, value: &str) -> Result<(), Exception>;

    /// Link local variable `local` to `other` in frame `level` (`upvar`).
    fn upvar(&self, level: &str, other: &str, local: &str) -> Result<(), Exception> {
        self.eval_words(&["upvar", level, other, local]).map(|_| ())
    }

    /// Build a well-formed list from items (`Tcl_Merge`).
    fn list_join(&self, items: &[&str]) -> String;

    /// Split a list into items (`Tcl_SplitList`).
    fn list_split(&self, list: &str) -> Result<Vec<String>, Exception>;

    /// Register (or replace) a command.
    fn register(&self, name: &str, cmd: Command);

    /// Remove a command.
    fn unregister(&self, name: &str);

    /// Whether a command named `name` exists.
    fn command_exists(&self, name: &str) -> bool;

    /// The engine's last error trace (`errorInfo`).
    fn error_info(&self) -> String {
        self.get_global("errorInfo").unwrap_or_default()
    }

    /// Per-interp typed storage.
    fn ext(&self) -> &Extensions;

    /// Mark the interp for deletion when its owner next releases it
    /// (`ns_markfordelete`).
    fn mark_for_delete(&self);

    /// Whether [`Interp::mark_for_delete`] was called.
    fn marked_for_delete(&self) -> bool;

    /// Convenience: build a list from owned strings.
    fn list_from(&self, items: &[String]) -> String {
        let refs: Vec<&str> = items.iter().map(String::as_str).collect();
        self.list_join(&refs)
    }

    /// A number identifying the interp (`ns_conn interp`: the address of
    /// the `Tcl_Interp`).
    fn handle(&self) -> usize {
        std::ptr::from_ref(self).cast::<()>() as usize
    }

    /// Run any pending asynchronous handlers, discarding their result
    /// (`Tcl_AsyncInvoke(interp, TCL_OK)`): clears a stale `ns_ictl cancel`
    /// when the interp is allocated.
    fn invoke_async(&self) {}

    /// Read `part1` or the element `part1(part2)` (`Tcl_GetVar2Ex` with no
    /// flags).
    fn get_var2(&self, part1: &str, part2: Option<&str>) -> Option<String> {
        match part2 {
            Some(k) => self.get_var(&format!("{part1}({k})")),
            None => self.get_var(part1),
        }
    }

    /// Set `part1` or the element `part1(part2)` (`Tcl_SetVar2Ex` with no
    /// flags).
    fn set_var2(&self, part1: &str, part2: Option<&str>, value: &str) -> Result<(), Exception> {
        match part2 {
            Some(k) => self.set_array_elem(part1, k, value),
            None => self.set_var(part1, value),
        }
    }

    /// Trace variable `name` (`Tcl_TraceVar2`).  The callback sees every
    /// event in `ops`, and always the unset that destroys the trace; `key`
    /// is reported by [`Interp::var_trace_keys`] to recognise the trace.
    fn trace_var(
        &self,
        name: &str,
        ops: TraceOps,
        key: usize,
        cb: VarTrace,
    ) -> Result<(), Exception> {
        let _ = (name, ops, key, cb);
        Err(unsupported_by(self.engine(), "variable tracing"))
    }

    /// The keys of the [`Interp::trace_var`] traces on `name`, most
    /// recently created first (`Tcl_VarTraceInfo`).
    fn var_trace_keys(&self, name: &str) -> Vec<usize> {
        let _ = name;
        Vec::new()
    }

    /// The identity of the channel `name` registered in this interp, or the
    /// engine's "can not find channel" error (`Tcl_GetChannel`).
    fn channel_id(&self, name: &str) -> Result<usize, Exception> {
        let _ = name;
        Err(unsupported_by(self.engine(), "channel access"))
    }

    /// Whether channel `name` is registered in more than one interp
    /// (`Tcl_IsChannelShared`).
    fn channel_is_shared(&self, name: &str) -> Result<bool, Exception> {
        let _ = name;
        Err(unsupported_by(self.engine(), "channel access"))
    }

    /// Check that channel `name` is open for writing (`write`) or reading
    /// (`Ns_TclGetOpenChannel` with `check`).
    fn channel_check_mode(&self, name: &str, write: bool) -> Result<(), Exception> {
        let _ = (name, write);
        Err(unsupported_by(self.engine(), "channel access"))
    }

    /// Cut channel `name` from this interp and thread (`UnspliceChannel`:
    /// clear its handlers, unregister it, `Tcl_CutChannel`).
    fn channel_detach(&self, name: &str) -> Result<DetachedChannel, Exception> {
        let _ = name;
        Err(unsupported_by(self.engine(), "channel transfer"))
    }

    /// Splice a detached channel into this thread and register it in this
    /// interp (`SpliceChannel`); returns its name.
    fn channel_attach(&self, chan: DetachedChannel) -> Result<String, Exception> {
        let _ = chan;
        Err(unsupported_by(self.engine(), "channel transfer"))
    }

    /// Unregister channel `name` from this interp if it is still the
    /// channel `id` (`Tcl_UnregisterChannel`); the last registration
    /// closes it.
    fn channel_unregister(&self, name: &str, id: usize) {
        let _ = (name, id);
    }

    /// Splice a detached channel into this thread and close it
    /// (`ns_chan cleanup -shared`).
    fn channel_close_detached(&self, chan: DetachedChannel) {
        let _ = chan;
    }

    /// Write raw bytes to channel `name`, without encoding conversion
    /// (`Tcl_Write`).
    fn channel_write(&self, name: &str, data: &[u8]) -> Result<(), Exception> {
        let _ = (name, data);
        Err(unsupported_by(self.engine(), "channel access"))
    }

    /// Wrap a connected socket as a blocking channel registered in this
    /// interp (`Tcl_MakeTcpClientChannel`); returns its name.  The channel
    /// owns the socket.
    fn channel_from_tcp(&self, stream: std::net::TcpStream) -> Result<String, Exception> {
        drop(stream);
        Err(unsupported_by(self.engine(), "socket channels"))
    }

    /// The OS descriptor behind channel `chan` (`Ns_TclGetOpenFd`): the
    /// channel must be open for writing when `write` is true, otherwise
    /// for reading.  Engines without OS-level channels return an error.
    fn channel_fd(&self, chan: &str, write: bool) -> Result<i32, Exception> {
        let _ = write;
        Err(Exception::error(format!(
            "channel \"{chan}\": descriptors are not supported by the {} engine",
            self.engine()
        )))
    }

    /// Bytes of input the engine has buffered for `chan`
    /// (`Tcl_InputBuffered`).
    fn channel_buffered(&self, chan: &str) -> Result<usize, Exception> {
        Err(Exception::error(format!(
            "channel \"{chan}\": buffers are not supported by the {} engine",
            self.engine()
        )))
    }

    /// Wrap the connected socket `fd` in a new binary-mode channel
    /// registered in this interp and return its name (AOLserver's
    /// `EnterSock`).  The channel owns `fd`; on failure `fd` is closed.
    fn socket_channel(&self, fd: i32) -> ScriptResult {
        close_fd(fd);
        Err(Exception::error("could not open socket"))
    }

    /// Register a channel held by this thread (see
    /// [`ScriptEngine::hold_socket`]) in this interp, so scripts can use it
    /// by name (`Tcl_RegisterChannel(interp, chan)`).
    fn attach_held_channel(&self, chan: &str) -> Result<(), Exception> {
        Err(Exception::error(format!(
            "can not find channel named \"{chan}\""
        )))
    }
}

/// Close a descriptor whose ownership was handed to an engine that cannot
/// use it.
pub fn close_fd(fd: i32) {
    if fd >= 0 {
        // SAFETY: callers pass a descriptor they own and will not use again.
        drop(unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) });
    }
}

/// A factory for interpreters.
pub trait ScriptEngine: Send + Sync {
    /// Engine name.
    fn name(&self) -> &'static str;

    /// Engine version string, e.g. the Tcl patch level.
    fn version(&self) -> String;

    /// Create a fresh interpreter on the calling thread.
    fn create_interp(&self) -> Result<Box<dyn Interp>, Exception>;

    /// Release per-thread engine resources (`Tcl_FinalizeThread`).  Called
    /// by threads that created interps, just before they exit.
    fn finalize_thread(&self) {}

    /// The calling thread's [`Canceller`], created on first use
    /// (`Tcl_AsyncCreate`), or `None` if the engine cannot interrupt
    /// evaluation.
    fn thread_canceller(&self) -> Option<Arc<dyn Canceller>> {
        None
    }

    /// Wrap the connected socket `fd` in a binary-mode channel held by the
    /// calling thread rather than by an interp (`Tcl_RegisterChannel(NULL,
    /// chan)`), and return its name.  The channel survives the interps it
    /// is attached to with [`Interp::attach_held_channel`] until
    /// [`ScriptEngine::release_held`].  The channel owns `fd`; on failure
    /// `fd` is closed.
    fn hold_socket(&self, fd: i32) -> Result<String, Exception> {
        close_fd(fd);
        Err(Exception::error(format!(
            "could not make channel for sock: {fd}"
        )))
    }

    /// Drop this thread's hold on channel `chan`
    /// (`Tcl_UnregisterChannel(NULL, chan)`), closing it unless an interp
    /// still has it registered.  Returns whether the thread held it.
    fn release_held(&self, chan: &str) -> bool {
        let _ = chan;
        false
    }
}

/// Standard "wrong # args" error in Tcl's format.
pub fn wrong_args(argv: &[String], nfixed: usize, usage: &str) -> Exception {
    let mut msg = String::from("wrong # args: should be \"");
    for (i, a) in argv.iter().take(nfixed).enumerate() {
        if i > 0 {
            msg.push(' ');
        }
        msg.push_str(a);
    }
    if !usage.is_empty() {
        msg.push(' ');
        msg.push_str(usage);
    }
    msg.push('"');
    Exception::error(msg)
}

/// Resolve `name` against a table of options with unique-prefix matching,
/// like `Tcl_GetIndexFromObj`.
pub fn get_index(name: &str, table: &[&str], what: &str) -> Result<usize, Exception> {
    if let Some(i) = table.iter().position(|t| *t == name) {
        return Ok(i);
    }
    // An empty key abbreviates every entry but never selects one.
    let matches: Vec<usize> = table
        .iter()
        .enumerate()
        .filter(|(_, t)| t.starts_with(name))
        .map(|(i, _)| i)
        .collect();
    if matches.len() == 1 && !name.is_empty() {
        return Ok(matches[0]);
    }
    let kind = if matches.len() > 1 {
        "ambiguous"
    } else {
        "bad"
    };
    let mut msg = format!("{kind} {what} \"{name}\": ");
    let Some((first, rest)) = table.split_first() else {
        msg.push_str("no valid options");
        return Err(Exception::error(msg));
    };
    // `Tcl_GetIndexFromObjStruct`: "a or b", "a, b, or c".
    msg.push_str("must be ");
    msg.push_str(first);
    let mut shown = 0;
    for (i, t) in rest.iter().enumerate() {
        if i + 1 == rest.len() {
            msg.push_str(if shown > 0 { ", or " } else { " or " });
            msg.push_str(t);
        } else if !t.is_empty() {
            msg.push_str(", ");
            msg.push_str(t);
            shown += 1;
        }
    }
    Err(Exception::error(msg))
}

/// Parse a Tcl integer (decimal, `0x` hex, leading `+`/`-`).
pub fn parse_int(s: &str) -> Result<i64, Exception> {
    let t = s.trim();
    let (neg, body) = match t.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let v = if let Some(h) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        i64::from_str_radix(h, 16)
    } else {
        body.parse::<i64>()
    }
    .map_err(|_| Exception::error(format!("expected integer but got \"{s}\"")))?;
    Ok(if neg { -v } else { v })
}

/// Parse a Tcl boolean.
pub fn parse_bool(s: &str) -> Result<bool, Exception> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "t" | "y" => Ok(true),
        "0" | "false" | "no" | "off" | "f" | "n" => Ok(false),
        other => other
            .parse::<f64>()
            .map(|f| f != 0.0)
            .map_err(|_| Exception::error(format!("expected boolean value but got \"{s}\""))),
    }
}

/// Parse a Tcl double.
pub fn parse_double(s: &str) -> Result<f64, Exception> {
    s.trim()
        .parse::<f64>()
        .map_err(|_| Exception::error(format!("expected floating-point number but got \"{s}\"")))
}

/// Simple `-flag value` / `-switch` option parser used by many commands.
///
/// ```
/// use ns_script::Options;
/// let argv: Vec<String> = ["cmd", "-timeout", "5", "-force", "x"].iter().map(|s| s.to_string()).collect();
/// let mut o = Options::new(&argv, 1);
/// let mut timeout = None;
/// let mut force = false;
/// while let Some(flag) = o.next_flag() {
///     match flag.as_str() {
///         "-timeout" => timeout = Some(o.value(&flag).unwrap()),
///         "-force" => force = true,
///         _ => panic!(),
///     }
/// }
/// assert_eq!(timeout.as_deref(), Some("5"));
/// assert!(force);
/// assert_eq!(o.rest(), &["x".to_string()]);
/// ```
#[derive(Debug)]
pub struct Options<'a> {
    argv: &'a [String],
    pos: usize,
}

impl<'a> Options<'a> {
    /// Start parsing `argv` at index `start`.
    pub const fn new(argv: &'a [String], start: usize) -> Self {
        Self { argv, pos: start }
    }

    /// Next `-flag`, or `None` at the first non-flag argument or `--`.
    pub fn next_flag(&mut self) -> Option<String> {
        let a = self.argv.get(self.pos)?;
        if a == "--" {
            self.pos += 1;
            return None;
        }
        if a.starts_with('-') && a.len() > 1 && parse_int(a).is_err() {
            self.pos += 1;
            Some(a.clone())
        } else {
            None
        }
    }

    /// The value following `flag`.
    pub fn value(&mut self, flag: &str) -> Result<String, Exception> {
        let v = self
            .argv
            .get(self.pos)
            .ok_or_else(|| Exception::error(format!("missing value for {flag}")))?;
        self.pos += 1;
        Ok(v.clone())
    }

    /// Remaining positional arguments.
    pub fn rest(&self) -> &'a [String] {
        &self.argv[self.pos.min(self.argv.len())..]
    }

    /// Index of the next unparsed argument.
    pub const fn position(&self) -> usize {
        self.pos
    }
}

/// Convert a Tcl string that holds binary data (one char per byte, as
/// produced by a Tcl byte array) back into bytes.  Chars above U+00FF are
/// encoded as UTF-8.
pub fn binary_to_bytes(s: &str) -> Vec<u8> {
    if s.chars().all(|c| u32::from(c) <= 0xff) {
        s.chars().map(|c| c as u8).collect()
    } else {
        s.as_bytes().to_vec()
    }
}

/// Inverse of [`binary_to_bytes`]: one char per byte.
pub fn bytes_to_binary(b: &[u8]) -> String {
    b.iter().map(|&c| char::from(c)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn codes_roundtrip() {
        for c in [
            Code::Error,
            Code::Return,
            Code::Break,
            Code::Continue,
            Code::Other(7),
        ] {
            assert_eq!(Code::from_i32(c.as_i32()), Some(c));
        }
        assert_eq!(Code::from_i32(0), None);
    }

    #[test]
    fn exceptions() {
        let e = Exception::error("boom").error_code("NS X");
        assert!(e.is_error());
        assert_eq!(e.to_string(), "boom");
        assert_eq!(e.detail(), "boom");
        assert_eq!(e.error_code.as_deref(), Some("NS X"));
        let b = Exception::with_code(Code::Break, "");
        assert!(!b.is_error());
    }

    #[test]
    fn extensions_store_by_type() {
        let ext = Extensions::default();
        ext.insert(5u32);
        assert_eq!(ext.get::<u32>(), Some(5));
        assert_eq!(ext.get::<i64>(), None);
        let cell = ext.shared(|| vec![1]);
        cell.borrow_mut().push(2);
        assert_eq!(*ext.shared(Vec::<i32>::new).borrow(), vec![1, 2]);
        assert_eq!(ext.remove::<u32>(), Some(5));
        assert_eq!(ext.remove::<u32>(), None);
        assert!(format!("{ext:?}").contains("len"));
    }

    #[test]
    fn wrong_args_format() {
        let argv = sv(&["ns_set", "get", "x", "y", "z"]);
        assert_eq!(
            wrong_args(&argv, 2, "setId key").message,
            "wrong # args: should be \"ns_set get setId key\""
        );
    }

    #[test]
    fn index_lookup() {
        let t = ["create", "copy", "delete"];
        assert_eq!(get_index("copy", &t, "option"), Ok(1));
        assert_eq!(get_index("d", &t, "option"), Ok(2));
        let e = get_index("c", &t, "option").expect_err("ambiguous");
        assert!(e
            .message
            .starts_with("ambiguous option \"c\": must be create, copy, or delete"));
        let e = get_index("zz", &t, "option").expect_err("bad");
        assert!(e.message.starts_with("bad option"));
        // Tcl_GetIndexFromObj's wording: no comma before "or" with two
        // choices, an empty key is ambiguous among several.
        let msg = |k: &str, t: &[&str]| get_index(k, t, "option").expect_err(k).message;
        assert_eq!(
            msg("sideways", &["in", "out"]),
            "bad option \"sideways\": must be in or out"
        );
        assert_eq!(msg("x", &["a"]), "bad option \"x\": must be a");
        assert_eq!(msg("", &["a"]), "bad option \"\": must be a");
        assert_eq!(
            msg("", &["in", "out"]),
            "ambiguous option \"\": must be in or out"
        );
        assert_eq!(
            msg("x", &["a", "", "c"]),
            "bad option \"x\": must be a or c"
        );
        assert_eq!(msg("x", &[]), "bad option \"x\": no valid options");
        assert_eq!(get_index("a", &["a"], "option"), Ok(0));
    }

    #[test]
    fn scalar_parsing() {
        assert_eq!(parse_int(" 42 "), Ok(42));
        assert_eq!(parse_int("-0x10"), Ok(-16));
        assert!(parse_int("4x").is_err());
        assert_eq!(parse_bool("On"), Ok(true));
        assert_eq!(parse_bool("0"), Ok(false));
        assert_eq!(parse_bool("2.5"), Ok(true));
        assert!(parse_bool("maybe").is_err());
        assert!((parse_double("1.5").expect("f") - 1.5).abs() < f64::EPSILON);
        assert!(parse_double("x").is_err());
    }

    #[test]
    fn options_parser() {
        let argv = sv(&["cmd", "-a", "1", "--", "-notflag"]);
        let mut o = Options::new(&argv, 1);
        assert_eq!(o.next_flag().as_deref(), Some("-a"));
        assert_eq!(o.value("-a").as_deref(), Ok("1"));
        assert_eq!(o.next_flag(), None);
        assert_eq!(o.rest(), &sv(&["-notflag"])[..]);
        let argv = sv(&["cmd", "-5"]);
        let mut o = Options::new(&argv, 1);
        assert_eq!(o.next_flag(), None, "negative numbers are not flags");
        let argv = sv(&["cmd", "-x"]);
        let mut o = Options::new(&argv, 1);
        o.next_flag();
        assert!(o.value("-x").is_err());
        assert_eq!(o.position(), 2);
    }

    #[test]
    fn binary_conversion() {
        let bytes = vec![0u8, 0x7f, 0x80, 0xff];
        assert_eq!(binary_to_bytes(&bytes_to_binary(&bytes)), bytes);
        assert_eq!(binary_to_bytes("é€"), "é€".as_bytes());
    }

    /// A minimal engine with only the required methods, to exercise the
    /// trait's defaults.
    #[derive(Default)]
    struct Bare {
        vars: RefCell<HashMap<String, String>>,
        ext: Extensions,
    }

    impl Interp for Bare {
        fn engine(&self) -> &'static str {
            "bare"
        }
        fn eval(&self, _: &str) -> ScriptResult {
            Ok(String::new())
        }
        fn eval_global(&self, _: &str) -> ScriptResult {
            Ok(String::new())
        }
        fn eval_words(&self, _: &[&str]) -> ScriptResult {
            Ok(String::new())
        }
        fn set_var(&self, name: &str, value: &str) -> Result<(), Exception> {
            self.vars.borrow_mut().insert(name.into(), value.into());
            Ok(())
        }
        fn get_var(&self, name: &str) -> Option<String> {
            self.vars.borrow().get(name).cloned()
        }
        fn unset_var(&self, name: &str) {
            self.vars.borrow_mut().remove(name);
        }
        fn set_global(&self, name: &str, value: &str) -> Result<(), Exception> {
            self.set_var(name, value)
        }
        fn get_global(&self, name: &str) -> Option<String> {
            self.get_var(name)
        }
        fn set_array_elem(&self, array: &str, key: &str, value: &str) -> Result<(), Exception> {
            self.set_var(&format!("{array}({key})"), value)
        }
        fn list_join(&self, items: &[&str]) -> String {
            items.join(" ")
        }
        fn list_split(&self, list: &str) -> Result<Vec<String>, Exception> {
            Ok(list.split(' ').map(str::to_owned).collect())
        }
        fn register(&self, _: &str, _: Command) {}
        fn unregister(&self, _: &str) {}
        fn command_exists(&self, _: &str) -> bool {
            false
        }
        fn ext(&self) -> &Extensions {
            &self.ext
        }
        fn mark_for_delete(&self) {}
        fn marked_for_delete(&self) -> bool {
            false
        }
    }

    struct BareEngine;

    impl ScriptEngine for BareEngine {
        fn name(&self) -> &'static str {
            "bare"
        }
        fn version(&self) -> String {
            "0".into()
        }
        fn create_interp(&self) -> Result<Box<dyn Interp>, Exception> {
            Ok(Box::new(Bare::default()))
        }
    }

    #[test]
    fn trait_defaults() {
        let e = BareEngine;
        assert!(e.thread_canceller().is_none());
        e.finalize_thread();
        let i = e.create_interp().expect("interp");
        assert_ne!(i.handle(), 0);
        i.invoke_async();
        i.set_var2("a", Some("k"), "1").expect("set");
        i.set_var2("s", None, "2").expect("set");
        assert_eq!(i.get_var2("a", Some("k")).as_deref(), Some("1"));
        assert_eq!(i.get_var2("s", None).as_deref(), Some("2"));
        let e = i
            .trace_var("v", TraceOps::ALL, 0, Rc::new(|_, _, _, _| {}))
            .expect_err("no traces");
        assert_eq!(
            e.message,
            "variable tracing is not supported by the bare engine"
        );
        assert_eq!(i.var_trace_keys("v"), Vec::<usize>::new());
        assert!(i.channel_id("c").is_err());
        assert!(i.channel_is_shared("c").is_err());
        assert!(i.channel_check_mode("c", true).is_err());
        assert!(i.channel_detach("c").is_err());
        let d = DetachedChannel::new("bare", "c".into(), 3, Box::new(5u8));
        assert_eq!((d.engine(), d.name(), d.id()), ("bare", "c", 3));
        assert!(format!("{d:?}").contains("\"c\""));
        assert_eq!(d.into_inner().downcast::<u8>().ok().map(|b| *b), Some(5));
        let d = DetachedChannel::new("bare", "c".into(), 3, Box::new(()));
        assert!(i.channel_attach(d).is_err());
        i.channel_unregister("c", 0);
        i.channel_close_detached(DetachedChannel::new("bare", "c".into(), 3, Box::new(())));
        assert!(i.channel_write("c", b"x").is_err());
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let s = std::net::TcpStream::connect(l.local_addr().expect("addr")).expect("connect");
        assert!(i.channel_from_tcp(s).is_err());
        assert!(TraceOps::ALL.unsets && !TraceOps::default().reads);
    }

    fn pipe() -> [i32; 2] {
        use std::os::fd::IntoRawFd;
        let (a, b) = std::os::unix::net::UnixStream::pair().expect("pair");
        [a.into_raw_fd(), b.into_raw_fd()]
    }

    #[test]
    fn channel_defaults_without_engine_support() {
        let e = BareEngine;
        let i = e.create_interp().expect("interp");
        assert!(i
            .channel_fd("c", true)
            .expect_err("fd")
            .message
            .contains("not supported by the bare engine"));
        assert!(i.channel_buffered("c").is_err());
        assert!(i.attach_held_channel("c").is_err());
        let [a, b] = pipe();
        assert_eq!(
            i.socket_channel(a).expect_err("socket").message,
            "could not open socket"
        );
        assert!(e.hold_socket(b).is_err());
        assert!(!e.release_held("c"));
        e.finalize_thread();
        close_fd(-1);
        assert_eq!(i.list_from(&["a".into(), "b".into()]), "a b");
    }
}
