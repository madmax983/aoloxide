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
    let matches: Vec<usize> = table
        .iter()
        .enumerate()
        .filter(|(_, t)| !name.is_empty() && t.starts_with(name))
        .map(|(i, _)| i)
        .collect();
    if matches.len() == 1 {
        return Ok(matches[0]);
    }
    let kind = if matches.len() > 1 {
        "ambiguous"
    } else {
        "bad"
    };
    let mut msg = format!("{kind} {what} \"{name}\": must be ");
    for (i, t) in table.iter().enumerate() {
        if i + 1 == table.len() && table.len() > 1 {
            msg.push_str(", or ");
        } else if i > 0 {
            msg.push_str(", ");
        }
        msg.push_str(t);
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

    /// An engine with no channels, to exercise the trait defaults.
    struct Bare(Extensions);

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
        fn set_var(&self, _: &str, _: &str) -> Result<(), Exception> {
            Ok(())
        }
        fn get_var(&self, _: &str) -> Option<String> {
            None
        }
        fn unset_var(&self, _: &str) {}
        fn set_global(&self, _: &str, _: &str) -> Result<(), Exception> {
            Ok(())
        }
        fn get_global(&self, _: &str) -> Option<String> {
            None
        }
        fn set_array_elem(&self, _: &str, _: &str, _: &str) -> Result<(), Exception> {
            Ok(())
        }
        fn list_join(&self, items: &[&str]) -> String {
            items.join(" ")
        }
        fn list_split(&self, list: &str) -> Result<Vec<String>, Exception> {
            Ok(list.split_whitespace().map(str::to_owned).collect())
        }
        fn register(&self, _: &str, _: Command) {}
        fn unregister(&self, _: &str) {}
        fn command_exists(&self, _: &str) -> bool {
            false
        }
        fn ext(&self) -> &Extensions {
            &self.0
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
            Ok(Box::new(Bare(Extensions::default())))
        }
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
