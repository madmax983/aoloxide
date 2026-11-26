//! # ns-tcl
//!
//! The Tcl 8.6 implementation of [`ns_script::ScriptEngine`].  It links the
//! system `libtcl8.6` exactly the way AOLserver embeds Tcl, so existing
//! `nsconfig.tcl` files, ADP pages and Tcl libraries run unchanged.
//!
//! ## Safety model
//!
//! * A [`TclInterp`] owns one `Tcl_Interp*` and is `!Send` (it contains raw
//!   pointers), so it can only be used on the thread that created it — the
//!   same rule Tcl imposes.
//! * Every registered command receives a heap-allocated `CmdData` as its
//!   client data, holding the command closure and a pointer back to the
//!   owning `TclInterp`.  The `TclInterp` lives in a `Box` whose address is
//!   stable for the interp's lifetime, and `Tcl_DeleteInterp` (run from
//!   `Drop`) frees every `CmdData` through the command delete callback
//!   before the box is released.
//! * Commands get `&TclInterp` (shared), so re-entrant evaluation never
//!   aliases a `&mut`.
//! * Panics inside commands are caught and turned into Tcl errors; they never
//!   unwind across the C boundary.

mod extra;
mod ffi;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::sync::Once;

use ns_script::{
    close_fd, Canceller, Code, Command, DetachedChannel, Exception, Extensions, Interp,
    ScriptEngine, ScriptResult, TraceOps, VarTrace,
};
use std::sync::Arc;

static INIT: Once = Once::new();
const FILE: &[u8] = b"ns-tcl\0";

/// Initialise the Tcl library once per process (`Tcl_FindExecutable`).
fn init_library() {
    INIT.call_once(|| {
        let argv0 = std::env::args().next().and_then(|a| CString::new(a).ok());
        // SAFETY: Tcl_FindExecutable accepts NULL or a valid C string; the
        // CString outlives the call.
        unsafe { ffi::Tcl_FindExecutable(argv0.as_ref().map_or(ptr::null(), |c| c.as_ptr())) };
    });
}

/// Convert Tcl's modified UTF-8 (NUL as `C0 80`) to a Rust string.
fn tcl_bytes_to_string(bytes: &[u8]) -> String {
    if bytes.windows(2).any(|w| w == [0xc0, 0x80]) {
        let mut v = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if i + 1 < bytes.len() && bytes[i] == 0xc0 && bytes[i + 1] == 0x80 {
                v.push(0);
                i += 2;
            } else {
                v.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8_lossy(&v).into_owned()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// Read the string representation of `obj`.
///
/// # Safety
/// `obj` must be a live `Tcl_Obj`.
unsafe fn obj_string(obj: *mut ffi::Tcl_Obj) -> String {
    let mut len: c_int = 0;
    // SAFETY: caller guarantees obj is live; Tcl returns a pointer to at
    // least `len` bytes valid until the object changes.
    unsafe {
        let p = ffi::Tcl_GetStringFromObj(obj, &raw mut len);
        if p.is_null() {
            return String::new();
        }
        let slice = std::slice::from_raw_parts(p.cast::<u8>(), usize::try_from(len).unwrap_or(0));
        tcl_bytes_to_string(slice)
    }
}

fn new_obj(s: &str) -> *mut ffi::Tcl_Obj {
    let len = c_int::try_from(s.len()).unwrap_or(c_int::MAX);
    // SAFETY: the pointer/length pair describes `s`, which Tcl copies.
    unsafe { ffi::Tcl_NewStringObj(s.as_ptr().cast::<c_char>(), len) }
}

fn incr(obj: *mut ffi::Tcl_Obj) {
    // SAFETY: obj is a live object; FILE is NUL-terminated.
    unsafe { ffi::Tcl_DbIncrRefCount(obj, FILE.as_ptr().cast(), 0) };
}

fn decr(obj: *mut ffi::Tcl_Obj) {
    // SAFETY: obj is live and its refcount was incremented by us.
    unsafe { ffi::Tcl_DbDecrRefCount(obj, FILE.as_ptr().cast(), 0) };
}

fn cstring(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

thread_local! {
    /// Channels held by this thread rather than by an interp
    /// (`Tcl_RegisterChannel(NULL, chan)`), by name.
    static HELD: RefCell<HashMap<String, usize>> = RefCell::new(HashMap::new());
}

/// The name of a live channel.
///
/// # Safety
/// `chan` must be a live channel.
unsafe fn channel_name(chan: ffi::Tcl_Channel) -> String {
    // SAFETY: Tcl returns a NUL-terminated name owned by the live channel.
    unsafe {
        let p = ffi::Tcl_GetChannelName(chan);
        if p.is_null() {
            String::new()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

/// Wrap socket `fd` in a TCP client channel in binary mode, closing `fd`
/// if Tcl cannot.
fn make_socket_channel(interp: *mut ffi::Tcl_Interp, fd: i32) -> Option<ffi::Tcl_Channel> {
    // SAFETY: Tcl takes ownership of the descriptor, passed by value as
    // the channel's client data (as `Tcl_MakeTcpClientChannel` expects).
    let chan = unsafe { ffi::Tcl_MakeTcpClientChannel(fd as isize as ffi::ClientData) };
    if chan.is_null() {
        close_fd(fd);
        return None;
    }
    // SAFETY: chan is live; the option strings are NUL-terminated.
    unsafe {
        ffi::Tcl_SetChannelOption(interp, chan, c"-translation".as_ptr(), c"binary".as_ptr());
    }
    Some(chan)
}

/// The Tcl engine.  Cheap to share; interps are created per thread.
#[derive(Debug, Default, Clone, Copy)]
pub struct TclEngine;

impl TclEngine {
    /// Create the engine (initialising libtcl on first use).
    pub fn new() -> Self {
        init_library();
        Self
    }

    /// Create a concrete [`TclInterp`].
    pub fn create(&self) -> Result<Box<TclInterp>, Exception> {
        init_library();
        // SAFETY: library initialised; Tcl_CreateInterp has no preconditions.
        let raw = unsafe { ffi::Tcl_CreateInterp() };
        if raw.is_null() {
            return Err(Exception::error("Tcl_CreateInterp failed"));
        }
        let interp = Box::new(TclInterp {
            raw,
            ext: Extensions::default(),
            delete: Cell::new(false),
        });
        // SAFETY: raw is a live interp owned by `interp`.
        if unsafe { ffi::Tcl_Init(raw) } != ffi::TCL_OK {
            let msg = interp.result();
            return Err(Exception::error(format!("Tcl_Init failed: {msg}")));
        }
        Ok(interp)
    }
}

impl ScriptEngine for TclEngine {
    fn name(&self) -> &'static str {
        "tcl"
    }

    fn version(&self) -> String {
        let (mut ma, mut mi, mut pa, mut ty) = (0, 0, 0, 0);
        // SAFETY: all pointers reference live locals.
        unsafe { ffi::Tcl_GetVersion(&raw mut ma, &raw mut mi, &raw mut pa, &raw mut ty) };
        format!("{ma}.{mi}.{pa}")
    }

    fn create_interp(&self) -> Result<Box<dyn Interp>, Exception> {
        Ok(self.create()?)
    }

    fn finalize_thread(&self) {
        HELD.with(|h| h.borrow_mut().clear());
        extra::release_thread_canceller();
        // SAFETY: called by a thread that is done with all its interps.
        unsafe { ffi::Tcl_FinalizeThread() };
    }

    fn thread_canceller(&self) -> Option<Arc<dyn Canceller>> {
        extra::thread_canceller()
    }

    fn hold_socket(&self, fd: i32) -> Result<String, Exception> {
        init_library();
        let chan = make_socket_channel(ptr::null_mut(), fd)
            .ok_or_else(|| Exception::error(format!("could not make channel for sock: {fd}")))?;
        // SAFETY: chan is live; a NULL interp registers a thread-level
        // reference that keeps the channel open.
        unsafe { ffi::Tcl_RegisterChannel(ptr::null_mut(), chan) };
        // SAFETY: chan is live.
        let name = unsafe { channel_name(chan) };
        HELD.with(|h| h.borrow_mut().insert(name.clone(), chan as usize));
        Ok(name)
    }

    fn release_held(&self, chan: &str) -> bool {
        let Some(c) = HELD.with(|h| h.borrow_mut().remove(chan)) else {
            return false;
        };
        // SAFETY: the held reference kept the channel alive until now; this
        // drops it (closing the channel unless an interp still has it).
        unsafe { ffi::Tcl_UnregisterChannel(ptr::null_mut(), c as ffi::Tcl_Channel) };
        true
    }
}

struct CmdData {
    owner: *const TclInterp,
    cmd: Command,
}

unsafe extern "C" fn cmd_trampoline(
    cd: *mut c_void,
    raw: *mut ffi::Tcl_Interp,
    objc: c_int,
    objv: *const *mut ffi::Tcl_Obj,
) -> c_int {
    // SAFETY: cd is the CmdData we registered; it lives until the command's
    // delete proc runs, which cannot happen while the command executes.
    let data = unsafe { &*cd.cast::<CmdData>() };
    let n = usize::try_from(objc).unwrap_or(0);
    // SAFETY: Tcl passes objc live objects in objv.
    let args: Vec<String> = (0..n)
        .map(|i| unsafe { obj_string(*objv.add(i)) })
        .collect();
    // SAFETY: owner points into a Box that outlives every command (see crate docs).
    let owner = unsafe { &*data.owner };
    let outcome = catch_unwind(AssertUnwindSafe(|| (data.cmd)(owner, &args)));
    let set = |s: &str| {
        // SAFETY: raw is the live interp invoking us.
        unsafe { ffi::Tcl_SetObjResult(raw, new_obj(s)) };
    };
    match outcome {
        Ok(Ok(s)) => {
            set(&s);
            ffi::TCL_OK
        }
        Ok(Err(e)) => {
            set(&e.message);
            if let Some(code) = &e.error_code {
                // SAFETY: raw is live; Tcl takes ownership of the new object.
                unsafe { ffi::Tcl_SetObjErrorCode(raw, new_obj(code)) };
            }
            e.code.as_i32()
        }
        Err(_) => {
            set(&format!(
                "internal error: command \"{}\" panicked",
                args.first().map_or("", String::as_str)
            ));
            ffi::TCL_ERROR
        }
    }
}

unsafe extern "C" fn cmd_delete(cd: *mut c_void) {
    // SAFETY: cd was produced by Box::into_raw in `register` and is freed
    // exactly once, here.
    drop(unsafe { Box::from_raw(cd.cast::<CmdData>()) });
}

/// A Tcl interpreter.  See the crate docs for the safety model.
pub struct TclInterp {
    raw: *mut ffi::Tcl_Interp,
    ext: Extensions,
    delete: Cell<bool>,
}

impl std::fmt::Debug for TclInterp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TclInterp")
            .field("raw", &self.raw)
            .finish_non_exhaustive()
    }
}

impl Drop for TclInterp {
    fn drop(&mut self) {
        // SAFETY: we own raw; deleting it runs the delete procs for all
        // registered commands, freeing their CmdData.
        unsafe { ffi::Tcl_DeleteInterp(self.raw) };
    }
}

impl TclInterp {
    /// The raw `Tcl_Interp*`, for callers that need the C API directly.
    pub fn as_raw(&self) -> *mut c_void {
        self.raw.cast()
    }

    fn result(&self) -> String {
        // SAFETY: raw is live; the result object is owned by the interp.
        unsafe { obj_string(ffi::Tcl_GetObjResult(self.raw)) }
    }

    fn global_obj(&self, name: &str) -> Option<String> {
        let n = cstring(name);
        // SAFETY: raw live, n valid for the call.
        let obj =
            unsafe { ffi::Tcl_GetVar2Ex(self.raw, n.as_ptr(), ptr::null(), ffi::TCL_GLOBAL_ONLY) };
        // SAFETY: non-null obj is owned by the variable and live here.
        (!obj.is_null()).then(|| unsafe { obj_string(obj) })
    }

    fn finish(&self, code: c_int) -> ScriptResult {
        let msg = self.result();
        match Code::from_i32(code) {
            None => Ok(msg),
            Some(Code::Error) => Err(Exception {
                code: Code::Error,
                message: msg,
                error_info: self.global_obj("errorInfo"),
                error_code: self.global_obj("errorCode"),
            }),
            Some(c) => Err(Exception::with_code(c, msg)),
        }
    }

    fn eval_flags(&self, script: &str, flags: c_int) -> ScriptResult {
        let len =
            c_int::try_from(script.len()).map_err(|_| Exception::error("script too large"))?;
        // SAFETY: raw live; script pointer/length valid for the call.
        let code =
            unsafe { ffi::Tcl_EvalEx(self.raw, script.as_ptr().cast::<c_char>(), len, flags) };
        self.finish(code)
    }

    fn set_var_flags(&self, name: &str, value: &str, flags: c_int) -> Result<(), Exception> {
        let n = cstring(name);
        // SAFETY: raw live; Tcl takes a reference to the new value object.
        let r = unsafe {
            ffi::Tcl_SetVar2Ex(
                self.raw,
                n.as_ptr(),
                ptr::null(),
                new_obj(value),
                flags | ffi::TCL_LEAVE_ERR_MSG,
            )
        };
        if r.is_null() {
            Err(Exception::error(self.result()))
        } else {
            Ok(())
        }
    }

    /// `Tcl_GetChannel`: the channel and its open mode.
    fn get_channel(&self, name: &str) -> Result<(ffi::Tcl_Channel, c_int), Exception> {
        let n = cstring(name);
        let mut mode: c_int = 0;
        // SAFETY: raw live, n valid, mode a live local.
        let chan = unsafe { ffi::Tcl_GetChannel(self.raw, n.as_ptr(), &raw mut mode) };
        if chan.is_null() {
            let msg = self.result();
            // SAFETY: raw is live.
            unsafe { ffi::Tcl_ResetResult(self.raw) };
            return Err(Exception::error(msg));
        }
        Ok((chan, mode))
    }

    fn get_var_flags(&self, name: &str, flags: c_int) -> Option<String> {
        let n = cstring(name);
        // SAFETY: raw live, n valid.
        let obj = unsafe { ffi::Tcl_GetVar2Ex(self.raw, n.as_ptr(), ptr::null(), flags) };
        // SAFETY: obj, when non-null, is live.
        (!obj.is_null()).then(|| unsafe { obj_string(obj) })
    }
}

impl Interp for TclInterp {
    fn engine(&self) -> &'static str {
        "tcl"
    }

    fn eval(&self, script: &str) -> ScriptResult {
        self.eval_flags(script, 0)
    }

    fn eval_global(&self, script: &str) -> ScriptResult {
        self.eval_flags(script, ffi::TCL_EVAL_GLOBAL)
    }

    fn eval_words(&self, words: &[&str]) -> ScriptResult {
        let objs: Vec<*mut ffi::Tcl_Obj> = words.iter().map(|w| new_obj(w)).collect();
        for &o in &objs {
            incr(o);
        }
        let n = c_int::try_from(objs.len()).unwrap_or(c_int::MAX);
        // SAFETY: raw live; objv holds n live, referenced objects.
        let code = unsafe { ffi::Tcl_EvalObjv(self.raw, n, objs.as_ptr(), 0) };
        let out = self.finish(code);
        for o in objs {
            decr(o);
        }
        out
    }

    fn set_var(&self, name: &str, value: &str) -> Result<(), Exception> {
        self.set_var_flags(name, value, 0)
    }

    fn get_var(&self, name: &str) -> Option<String> {
        self.get_var_flags(name, 0)
    }

    fn unset_var(&self, name: &str) {
        let n = cstring(name);
        // SAFETY: raw live, n valid.
        unsafe { ffi::Tcl_UnsetVar2(self.raw, n.as_ptr(), ptr::null(), 0) };
    }

    fn set_global(&self, name: &str, value: &str) -> Result<(), Exception> {
        self.set_var_flags(name, value, ffi::TCL_GLOBAL_ONLY)
    }

    fn get_global(&self, name: &str) -> Option<String> {
        self.get_var_flags(name, ffi::TCL_GLOBAL_ONLY)
    }

    fn set_array_elem(&self, array: &str, key: &str, value: &str) -> Result<(), Exception> {
        let a = cstring(array);
        let k = cstring(key);
        // SAFETY: raw live; a/k valid; Tcl references the value object.
        let r = unsafe {
            ffi::Tcl_SetVar2Ex(
                self.raw,
                a.as_ptr(),
                k.as_ptr(),
                new_obj(value),
                ffi::TCL_LEAVE_ERR_MSG,
            )
        };
        if r.is_null() {
            Err(Exception::error(self.result()))
        } else {
            Ok(())
        }
    }

    fn list_join(&self, items: &[&str]) -> String {
        let objs: Vec<*mut ffi::Tcl_Obj> = items.iter().map(|w| new_obj(w)).collect();
        let n = c_int::try_from(objs.len()).unwrap_or(c_int::MAX);
        // SAFETY: objs are fresh objects that the list takes references to.
        let list = unsafe { ffi::Tcl_NewListObj(n, objs.as_ptr()) };
        incr(list);
        // SAFETY: list is live.
        let s = unsafe { obj_string(list) };
        decr(list);
        s
    }

    fn list_split(&self, list: &str) -> Result<Vec<String>, Exception> {
        let obj = new_obj(list);
        incr(obj);
        let mut n: c_int = 0;
        let mut elems: *mut *mut ffi::Tcl_Obj = ptr::null_mut();
        // SAFETY: obj live; out-pointers reference locals.
        let code =
            unsafe { ffi::Tcl_ListObjGetElements(self.raw, obj, &raw mut n, &raw mut elems) };
        let out = if code == ffi::TCL_OK {
            let count = usize::try_from(n).unwrap_or(0);
            // SAFETY: Tcl returned `count` live element pointers owned by obj.
            Ok((0..count)
                .map(|i| unsafe { obj_string(*elems.add(i)) })
                .collect())
        } else {
            Err(Exception::error(self.result()))
        };
        decr(obj);
        out
    }

    fn register(&self, name: &str, cmd: Command) {
        let data = Box::into_raw(Box::new(CmdData {
            owner: std::ptr::from_ref(self),
            cmd,
        }));
        let n = cstring(name);
        // SAFETY: raw live; ownership of `data` passes to Tcl, which frees it
        // through cmd_delete when the command is deleted or replaced.
        unsafe {
            ffi::Tcl_CreateObjCommand(
                self.raw,
                n.as_ptr(),
                cmd_trampoline,
                data.cast(),
                Some(cmd_delete),
            );
        }
    }

    fn unregister(&self, name: &str) {
        let n = cstring(name);
        // SAFETY: raw live; deleting a missing command is harmless.
        unsafe { ffi::Tcl_DeleteCommand(self.raw, n.as_ptr()) };
    }

    fn command_exists(&self, name: &str) -> bool {
        let n = cstring(name);
        // Tcl_CmdInfo is 8 pointer-sized fields; over-allocate for safety.
        let mut info = [0usize; 16];
        // SAFETY: raw live; info buffer is large enough for Tcl_CmdInfo.
        unsafe { ffi::Tcl_GetCommandInfo(self.raw, n.as_ptr(), info.as_mut_ptr().cast()) != 0 }
    }

    fn ext(&self) -> &Extensions {
        &self.ext
    }

    fn mark_for_delete(&self) {
        self.delete.set(true);
    }

    fn marked_for_delete(&self) -> bool {
        self.delete.get()
    }

    fn handle(&self) -> usize {
        self.raw as usize
    }

    fn invoke_async(&self) {
        self.invoke_async_impl();
    }

    fn get_var2(&self, part1: &str, part2: Option<&str>) -> Option<String> {
        let a = cstring(part1);
        let k = part2.map(cstring);
        // SAFETY: raw live; a/k valid for the call.
        let obj = unsafe {
            ffi::Tcl_GetVar2Ex(
                self.raw,
                a.as_ptr(),
                k.as_ref().map_or(ptr::null(), |k| k.as_ptr()),
                0,
            )
        };
        // SAFETY: obj, when non-null, is live.
        (!obj.is_null()).then(|| unsafe { obj_string(obj) })
    }

    fn set_var2(&self, part1: &str, part2: Option<&str>, value: &str) -> Result<(), Exception> {
        let a = cstring(part1);
        let k = part2.map(cstring);
        // SAFETY: raw live; a/k valid; Tcl references the value object.
        let r = unsafe {
            ffi::Tcl_SetVar2Ex(
                self.raw,
                a.as_ptr(),
                k.as_ref().map_or(ptr::null(), |k| k.as_ptr()),
                new_obj(value),
                ffi::TCL_LEAVE_ERR_MSG,
            )
        };
        if r.is_null() {
            Err(Exception::error(self.result()))
        } else {
            Ok(())
        }
    }

    fn trace_var(
        &self,
        name: &str,
        ops: TraceOps,
        key: usize,
        cb: VarTrace,
    ) -> Result<(), Exception> {
        self.trace_var_impl(name, ops, key, cb)
    }

    fn var_trace_keys(&self, name: &str) -> Vec<usize> {
        self.var_trace_keys_impl(name)
    }

    fn channel_id(&self, name: &str) -> Result<usize, Exception> {
        self.channel_id_impl(name)
    }

    fn channel_is_shared(&self, name: &str) -> Result<bool, Exception> {
        self.channel_is_shared_impl(name)
    }

    fn channel_check_mode(&self, name: &str, write: bool) -> Result<(), Exception> {
        self.channel_check_mode_impl(name, write)
    }

    fn channel_detach(&self, name: &str) -> Result<DetachedChannel, Exception> {
        self.channel_detach_impl(name)
    }

    fn channel_attach(&self, chan: DetachedChannel) -> Result<String, Exception> {
        self.channel_attach_impl(chan)
    }

    fn channel_unregister(&self, name: &str, id: usize) {
        self.channel_unregister_impl(name, id);
    }

    fn channel_close_detached(&self, chan: DetachedChannel) {
        Self::channel_close_detached_impl(chan);
    }

    fn channel_write(&self, name: &str, data: &[u8]) -> Result<(), Exception> {
        self.channel_write_impl(name, data)
    }

    fn channel_from_tcp(&self, stream: std::net::TcpStream) -> Result<String, Exception> {
        self.channel_from_tcp_impl(stream)
    }

    fn channel_fd(&self, chan: &str, write: bool) -> Result<i32, Exception> {
        let (c, mode) = self.get_channel(chan)?;
        let dir = if write {
            ffi::TCL_WRITABLE
        } else {
            ffi::TCL_READABLE
        };
        if mode & dir == 0 {
            return Err(Exception::error(format!(
                "channel \"{chan}\" not open for {}",
                if write { "write" } else { "read" }
            )));
        }
        let mut data: ffi::ClientData = ptr::null_mut();
        // SAFETY: c is live; data is a live out-pointer.
        if unsafe { ffi::Tcl_GetChannelHandle(c, dir, &raw mut data) } != ffi::TCL_OK {
            return Err(Exception::error(format!(
                "could not get handle for channel: {chan}"
            )));
        }
        Ok(data as isize as i32)
    }

    fn channel_buffered(&self, chan: &str) -> Result<usize, Exception> {
        let (c, _) = self.get_channel(chan)?;
        // SAFETY: c is live.
        let n = unsafe { ffi::Tcl_InputBuffered(c) };
        Ok(usize::try_from(n).unwrap_or(0))
    }

    fn socket_channel(&self, fd: i32) -> ScriptResult {
        let chan = make_socket_channel(self.raw, fd)
            .ok_or_else(|| Exception::error("could not open socket"))?;
        // SAFETY: raw and chan are live; the interp takes a reference.
        unsafe { ffi::Tcl_RegisterChannel(self.raw, chan) };
        // SAFETY: chan is live.
        Ok(unsafe { channel_name(chan) })
    }

    fn attach_held_channel(&self, chan: &str) -> Result<(), Exception> {
        let c = HELD
            .with(|h| h.borrow().get(chan).copied())
            .ok_or_else(|| Exception::error(format!("can not find channel named \"{chan}\"")))?;
        // SAFETY: the thread's hold keeps c alive; registering a channel
        // already registered in this interp is a no-op.
        unsafe { ffi::Tcl_RegisterChannel(self.raw, c as ffi::Tcl_Channel) };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ns_script::command;

    #[allow(clippy::unnecessary_box_returns)] // stable address required
    fn interp() -> Box<TclInterp> {
        TclEngine::new().create().expect("interp")
    }

    #[test]
    fn eval_and_errors() {
        let i = interp();
        assert_eq!(i.eval("expr {6*7}"), Ok("42".into()));
        let e = i.eval("error boom {} {MY CODE}").expect_err("error");
        assert_eq!(e.code, Code::Error);
        assert_eq!(e.message, "boom");
        assert_eq!(e.error_code.as_deref(), Some("MY CODE"));
        assert!(e.error_info.expect("info").contains("boom"));
        assert_eq!(i.eval("set x {a b}"), Ok("a b".into()));
    }

    #[test]
    fn variables() {
        let i = interp();
        i.set_var("v", "1").expect("set");
        assert_eq!(i.get_var("v").as_deref(), Some("1"));
        i.set_var("arr(k)", "x").expect("set");
        i.set_array_elem("arr", "j", "y").expect("set");
        assert_eq!(i.eval("array get arr k").expect("eval"), "k x");
        assert_eq!(i.get_var("arr(j)").as_deref(), Some("y"));
        i.unset_var("v");
        assert_eq!(i.get_var("v"), None);
        i.set_global("g", "gv").expect("set");
        assert_eq!(
            i.eval("proc p {} {global g; return $g}; p").expect("eval"),
            "gv"
        );
        assert_eq!(i.get_global("g").as_deref(), Some("gv"));
        assert!(i.set_var("arr", "scalar-over-array").is_err());
    }

    #[test]
    fn lists() {
        let i = interp();
        let l = i.list_join(&["a", "b c", "", "{"]);
        assert_eq!(i.list_split(&l).expect("split"), vec!["a", "b c", "", "{"]);
        assert!(i.list_split("a {b").is_err());
        assert_eq!(i.list_from(&["x".into()]), "x");
    }

    #[test]
    fn commands_and_reentrancy() {
        let i = interp();
        i.register(
            "twice",
            command(|interp, argv| {
                let inner = interp.eval(&argv[1])?;
                Ok(format!("{inner}{inner}"))
            }),
        );
        i.register(
            "fail",
            command(|_, _| Err(Exception::error("nope").error_code("NS FAIL"))),
        );
        i.register(
            "brk",
            command(|_, _| Err(Exception::with_code(Code::Break, ""))),
        );
        i.register("boom", command(|_, _| panic!("kaboom")));
        assert!(i.command_exists("twice"));
        assert!(!i.command_exists("nosuch"));
        assert_eq!(i.eval("twice {twice {set z ab}}"), Ok("abababab".into()));
        let e = i.eval("fail").expect_err("fail");
        assert_eq!(
            (e.message.as_str(), e.error_code.as_deref()),
            ("nope", Some("NS FAIL"))
        );
        assert_eq!(
            i.eval("set n 0; foreach x {1 2 3} {incr n; brk}; set n"),
            Ok("1".into())
        );
        let e = i.eval("boom").expect_err("panic becomes error");
        assert!(e.message.contains("panicked"));
        i.unregister("twice");
        assert!(!i.command_exists("twice"));
        // Re-registering replaces and frees the old CmdData.
        i.register("fail", command(|_, _| Ok("ok".into())));
        assert_eq!(i.eval("fail"), Ok("ok".into()));
    }

    #[test]
    fn eval_words_and_uplevel() {
        let i = interp();
        assert_eq!(i.eval_words(&["string", "length", "a b{"]), Ok("4".into()));
        i.eval("proc outer {} { set local 5; inner }")
            .expect("proc");
        i.register(
            "inner",
            command(|interp, _| interp.uplevel("0", "set local")),
        );
        assert_eq!(i.eval("outer"), Ok("5".into()));
        // Native commands have no frame of their own: level 1 is the
        // proc's caller (global here), exactly as for C commands.
        i.register(
            "inner",
            command(|interp, _| interp.uplevel("1", "info level")),
        );
        assert_eq!(i.eval("outer"), Ok("0".into()));
        assert_eq!(i.eval_global("set ::top 1"), Ok("1".into()));
    }

    #[test]
    fn binary_and_nul_safe() {
        let i = interp();
        i.set_var("b", "a\0b").expect("set");
        assert_eq!(i.get_var("b").as_deref(), Some("a\0b"));
        assert_eq!(i.eval("string length $b"), Ok("3".into()));
        assert_eq!(tcl_bytes_to_string(&[b'x', 0xc0, 0x80, b'y']), "x\0y");
    }

    #[test]
    fn engine_and_ext() {
        let e = TclEngine::new();
        assert_eq!(ScriptEngine::name(&e), "tcl");
        assert!(e.version().starts_with("8."));
        let i = e.create_interp().expect("interp");
        assert_eq!(i.engine(), "tcl");
        i.ext().insert(7u8);
        assert_eq!(i.ext().get::<u8>(), Some(7));
        assert!(!i.marked_for_delete());
        i.mark_for_delete();
        assert!(i.marked_for_delete());
        let _ = i.error_info();
        let _ = i
            .eval_file("/nonexistent/file.tcl")
            .expect_err("missing file");
    }

    #[test]
    #[allow(clippy::needless_collect)] // spawn all threads before joining
    fn interps_per_thread() {
        let handles: Vec<_> = (0..4)
            .map(|n| {
                std::thread::spawn(move || {
                    let e = TclEngine::new();
                    let i = e.create().expect("interp");
                    let r = i.eval(&format!("expr {{{n} + 1}}")).expect("eval");
                    drop(i);
                    e.finalize_thread();
                    r
                })
            })
            .collect();
        let mut got: Vec<String> = handles
            .into_iter()
            .map(|h| h.join().expect("join"))
            .collect();
        got.sort();
        assert_eq!(got, vec!["1", "2", "3", "4"]);
    }

    #[test]
    fn socket_and_held_channels() {
        use std::os::fd::IntoRawFd;
        let e = TclEngine::new();
        let i = interp();
        let (a, b) = std::os::unix::net::UnixStream::pair().expect("pair");
        let (fa, fb) = (a.into_raw_fd(), b.into_raw_fd());
        let ca = i.socket_channel(fa).expect("channel");
        assert_eq!(i.channel_fd(&ca, true), Ok(fa));
        assert_eq!(i.channel_fd(&ca, false), Ok(fa));
        assert_eq!(
            i.eval_words(&["fconfigure", &ca, "-translation"]),
            Ok("lf lf".into())
        );
        let e1 = i.channel_fd("nosuch", false).expect_err("missing");
        assert_eq!(e1.message, "can not find channel named \"nosuch\"");
        assert!(i.channel_buffered("nosuch").is_err());
        let f = i.eval("open [info nameofexecutable]").expect("open");
        let e2 = i.channel_fd(&f, true).expect_err("read-only");
        assert_eq!(e2.message, format!("channel \"{f}\" not open for write"));
        i.eval_words(&["close", &f]).expect("close");
        // A held channel outlives the interps it is attached to.
        let h = e.hold_socket(fb).expect("hold");
        i.attach_held_channel(&h).expect("attach");
        i.attach_held_channel(&h).expect("attach twice is a no-op");
        i.eval(&format!("puts {ca} one; puts {ca} two; flush {ca}"))
            .expect("write");
        assert_eq!(i.eval_words(&["gets", &h]), Ok("one".into()));
        assert_eq!(i.channel_buffered(&h), Ok(4));
        i.eval_words(&["close", &h]).expect("detach");
        assert!(i.channel_fd(&h, false).is_err(), "detached from the interp");
        let j = interp();
        j.attach_held_channel(&h).expect("attach to another interp");
        assert_eq!(j.eval_words(&["gets", &h]), Ok("two".into()));
        drop(j);
        assert!(e.release_held(&h));
        assert!(!e.release_held(&h));
        assert!(i.attach_held_channel(&h).is_err());
        // Releasing closed the socket: the peer sees end of file.
        assert_eq!(i.eval_words(&["read", &ca]), Ok(String::new()));
        i.eval_words(&["close", &ca]).expect("close");
    }
}
