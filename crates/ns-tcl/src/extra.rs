//! Channel transfer, variable traces and asynchronous cancellation: the
//! parts of the Tcl C API that `ns_chan`, `ns_share`, `ns_conn channel`
//! and `ns_ictl cancel` need (`nsd/tclfile.c`, `tclshare.c`, `conn.c`,
//! `tclinit.c`).

use std::cell::RefCell;
use std::ffi::CStr;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::os::raw::{c_char, c_int, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::sync::{Arc, Mutex, PoisonError};

use ns_script::{Canceller, DetachedChannel, Exception, Interp, TraceOps, VarTrace};

use crate::{cstring, ffi, init_library, TclInterp};

/// The engine name carried by detached channels.
const ENGINE: &str = "tcl";

/// A `Tcl_Channel` cut from its thread.  Tcl's `Tcl_CutChannel` /
/// `Tcl_SpliceChannel` exist precisely to move a channel between threads,
/// so the pointer may be sent once it is cut.
struct CutChannel(usize);

fn c_str(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: Tcl returns NUL-terminated strings that live at least until
    // the next call on the same object.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

impl TclInterp {
    /// `Tcl_GetChannel`, leaving Tcl's error message on failure.
    fn get_channel(&self, name: &str) -> Result<(ffi::Tcl_Channel, c_int), Exception> {
        let n = cstring(name);
        let mut mode: c_int = 0;
        // SAFETY: raw is live; n and mode outlive the call.
        let chan = unsafe { ffi::Tcl_GetChannel(self.raw, n.as_ptr(), &raw mut mode) };
        if chan.is_null() {
            Err(Exception::error(self.result()))
        } else {
            Ok((chan, mode))
        }
    }

    /// `Tcl_PosixError`: the message for the current `errno`, also setting
    /// `errorCode`.
    fn posix_error(&self) -> String {
        // SAFETY: raw is live; Tcl returns a static message.
        c_str(unsafe { ffi::Tcl_PosixError(self.raw) })
    }

    pub(crate) fn channel_id_impl(&self, name: &str) -> Result<usize, Exception> {
        Ok(self.get_channel(name)?.0 as usize)
    }

    pub(crate) fn channel_is_shared_impl(&self, name: &str) -> Result<bool, Exception> {
        let (chan, _) = self.get_channel(name)?;
        // SAFETY: chan is a live channel of this thread.
        Ok(unsafe { ffi::Tcl_IsChannelShared(chan) } != 0)
    }

    pub(crate) fn channel_check_mode_impl(&self, name: &str, write: bool) -> Result<(), Exception> {
        let (_, mode) = self.get_channel(name)?;
        let want = if write {
            ffi::TCL_WRITABLE
        } else {
            ffi::TCL_READABLE
        };
        if mode & want == 0 {
            return Err(Exception::error(format!(
                "channel \"{name}\" not open for {}",
                if write { "write" } else { "read" }
            )));
        }
        Ok(())
    }

    pub(crate) fn channel_detach_impl(&self, name: &str) -> Result<DetachedChannel, Exception> {
        let (chan, _) = self.get_channel(name)?;
        // SAFETY: chan is live.  The NULL registration keeps it open while
        // it is unregistered from this interp (`UnspliceChannel`); after
        // the cut no thread owns it until it is spliced again.
        let cname = unsafe {
            ffi::Tcl_ClearChannelHandlers(chan);
            ffi::Tcl_RegisterChannel(ptr::null_mut(), chan);
            ffi::Tcl_UnregisterChannel(self.raw, chan);
            let cname = c_str(ffi::Tcl_GetChannelName(chan));
            ffi::Tcl_CutChannel(chan);
            cname
        };
        Ok(DetachedChannel::new(
            ENGINE,
            cname,
            chan as usize,
            Box::new(CutChannel(chan as usize)),
        ))
    }

    pub(crate) fn channel_attach_impl(&self, chan: DetachedChannel) -> Result<String, Exception> {
        let name = chan.name().to_owned();
        let cut = take_cut(chan)?;
        let chan = cut.0 as ffi::Tcl_Channel;
        // Tcl_RegisterChannel panics on a duplicate name; refuse instead
        // (the channel stays cut, and is leaked).
        if self.get_channel(&name).is_ok() {
            return Err(Exception::error(format!(
                "channel \"{name}\" already exists in this interp"
            )));
        }
        // SAFETY: raw is live; clear the lookup error.
        unsafe { ffi::Tcl_ResetResult(self.raw) };
        // SAFETY: the channel was cut by `channel_detach_impl` and is owned
        // by nobody; splice it into this thread, register it here and drop
        // the NULL registration (`SpliceChannel`).
        unsafe {
            ffi::Tcl_SpliceChannel(chan);
            ffi::Tcl_RegisterChannel(self.raw, chan);
            ffi::Tcl_UnregisterChannel(ptr::null_mut(), chan);
            Ok(c_str(ffi::Tcl_GetChannelName(chan)))
        }
    }

    pub(crate) fn channel_unregister_impl(&self, name: &str, id: usize) {
        if let Ok((chan, _)) = self.get_channel(name) {
            if chan as usize == id {
                // SAFETY: chan is registered in this interp.
                unsafe { ffi::Tcl_UnregisterChannel(self.raw, chan) };
            }
        }
        // SAFETY: raw is live; clear the lookup error.
        unsafe { ffi::Tcl_ResetResult(self.raw) };
    }

    pub(crate) fn channel_close_detached_impl(chan: DetachedChannel) {
        if let Ok(cut) = take_cut(chan) {
            let chan = cut.0 as ffi::Tcl_Channel;
            // SAFETY: as in attach; dropping the only (NULL) registration
            // closes the channel.
            unsafe {
                ffi::Tcl_SpliceChannel(chan);
                ffi::Tcl_UnregisterChannel(ptr::null_mut(), chan);
            }
        }
    }

    pub(crate) fn channel_write_impl(&self, name: &str, data: &[u8]) -> Result<(), Exception> {
        let (chan, _) = self.get_channel(name)?;
        let mut rest = data;
        while !rest.is_empty() {
            let len = c_int::try_from(rest.len()).unwrap_or(c_int::MAX);
            // SAFETY: chan live; rest describes `len` readable bytes.
            let wrote = unsafe { ffi::Tcl_Write(chan, rest.as_ptr().cast(), len) };
            if wrote < 0 {
                return Err(Exception::error(format!(
                    "write failed: {}",
                    self.posix_error()
                )));
            }
            rest = &rest[usize::try_from(wrote).unwrap_or(0).min(rest.len())..];
        }
        Ok(())
    }

    pub(crate) fn channel_from_tcp_impl(
        &self,
        stream: std::net::TcpStream,
    ) -> Result<String, Exception> {
        stream
            .set_nonblocking(false)
            .map_err(|e| Exception::error(e.to_string()))?;
        let fd = stream.into_raw_fd();
        // SAFETY: Tcl takes ownership of the descriptor (closing it when the
        // channel closes); the client data is the fd itself, as in
        // `MakeConnChannel`.
        let chan = unsafe { ffi::Tcl_MakeTcpClientChannel(fd as isize as *mut c_void) };
        if chan.is_null() {
            // SAFETY: Tcl did not take the descriptor; it is ours to close.
            drop(unsafe { OwnedFd::from_raw_fd(fd) });
            return Err(Exception::error(self.posix_error()));
        }
        // SAFETY: chan is a new channel of this thread.
        unsafe {
            ffi::Tcl_RegisterChannel(self.raw, chan);
            Ok(c_str(ffi::Tcl_GetChannelName(chan)))
        }
    }

    pub(crate) fn invoke_async_impl(&self) {
        // SAFETY: raw is live; invoking handlers at a safe point is what
        // Tcl_AsyncInvoke is for.
        unsafe { ffi::Tcl_AsyncInvoke(self.raw, ffi::TCL_OK) };
    }

    pub(crate) fn trace_var_impl(
        &self,
        name: &str,
        ops: TraceOps,
        key: usize,
        cb: VarTrace,
    ) -> Result<(), Exception> {
        let mut flags = ffi::TCL_TRACE_UNSETS;
        if ops.reads {
            flags |= ffi::TCL_TRACE_READS;
        }
        if ops.writes {
            flags |= ffi::TCL_TRACE_WRITES;
        }
        if ops.array {
            flags |= ffi::TCL_TRACE_ARRAY;
        }
        let data = Box::into_raw(Box::new(TraceData {
            owner: ptr::from_ref(self),
            key,
            cb,
        }));
        let n = cstring(name);
        // SAFETY: raw live; `data` is freed by the trampoline when Tcl
        // destroys the trace (it always sees the destroying unset).
        let code = unsafe {
            ffi::Tcl_TraceVar2(
                self.raw,
                n.as_ptr(),
                ptr::null(),
                flags,
                var_trampoline,
                data.cast(),
            )
        };
        if code == ffi::TCL_OK {
            Ok(())
        } else {
            // SAFETY: Tcl did not keep `data`.
            drop(unsafe { Box::from_raw(data) });
            Err(Exception::error(self.result()))
        }
    }

    pub(crate) fn var_trace_keys_impl(&self, name: &str) -> Vec<usize> {
        let n = cstring(name);
        let mut keys = Vec::new();
        let mut prev: ffi::ClientData = ptr::null_mut();
        loop {
            // SAFETY: raw live; prev is null or a TraceData of ours.
            let cd = unsafe {
                ffi::Tcl_VarTraceInfo2(self.raw, n.as_ptr(), ptr::null(), 0, var_trampoline, prev)
            };
            if cd.is_null() {
                break;
            }
            // SAFETY: only `trace_var_impl` registers var_trampoline, always
            // with a live TraceData.
            keys.push(unsafe { &*cd.cast::<TraceData>() }.key);
            prev = cd;
        }
        keys
    }
}

fn take_cut(chan: DetachedChannel) -> Result<CutChannel, Exception> {
    if chan.engine() != ENGINE {
        return Err(Exception::error(format!(
            "channel \"{}\" belongs to the {} engine",
            chan.name(),
            chan.engine()
        )));
    }
    chan.into_inner()
        .downcast::<CutChannel>()
        .map(|b| *b)
        .map_err(|_| Exception::error("not a Tcl channel"))
}

struct TraceData {
    owner: *const TclInterp,
    key: usize,
    cb: VarTrace,
}

unsafe extern "C" fn var_trampoline(
    cd: ffi::ClientData,
    _raw: *mut ffi::Tcl_Interp,
    name1: *const c_char,
    name2: *const c_char,
    flags: c_int,
) -> *mut c_char {
    let data = cd.cast::<TraceData>();
    let ops = TraceOps {
        reads: flags & ffi::TCL_TRACE_READS != 0,
        writes: flags & ffi::TCL_TRACE_WRITES != 0,
        unsets: flags & ffi::TCL_TRACE_UNSETS != 0,
        array: flags & ffi::TCL_TRACE_ARRAY != 0,
        destroyed: flags & ffi::TCL_TRACE_DESTROYED != 0,
        interp_destroyed: flags & ffi::TCL_INTERP_DESTROYED != 0,
    };
    let n1 = c_str(name1);
    let n2 = (!name2.is_null()).then(|| c_str(name2));
    {
        // SAFETY: cd is the TraceData registered with this trace; it is
        // freed below, only once Tcl says the trace is destroyed.
        let d = unsafe { &*data };
        // While the interp is being deleted its owner is being dropped, so
        // no reference to it is made.
        // SAFETY: otherwise the owner Box outlives every trace (see the
        // crate docs).
        let owner: Option<&dyn Interp> = if ops.interp_destroyed {
            None
        } else {
            Some(unsafe { &*d.owner })
        };
        let _ = catch_unwind(AssertUnwindSafe(|| (d.cb)(owner, &n1, n2.as_deref(), ops)));
    }
    if ops.destroyed {
        // SAFETY: Tcl will not call this trace again.
        drop(unsafe { Box::from_raw(data) });
    }
    ptr::null_mut()
}

const CANCEL_MSG: &[u8] = b"async cancel\0";

/// `AsyncCancel`: abort the running script with "async cancel".
unsafe extern "C" fn async_cancel(
    _cd: ffi::ClientData,
    interp: *mut ffi::Tcl_Interp,
    code: c_int,
) -> c_int {
    if interp.is_null() {
        // Invoked from the event loop with no interp (`Tcl_DoOneEvent`):
        // there is no script to cancel.
        return code;
    }
    // SAFETY: interp is live; CANCEL_MSG is static (TCL_STATIC).
    unsafe {
        ffi::Tcl_ResetResult(interp);
        ffi::Tcl_SetResult(
            interp,
            CANCEL_MSG.as_ptr().cast_mut().cast(),
            ptr::null_mut(),
        );
    }
    ffi::TCL_ERROR
}

/// The per-thread async cancel handler (`TclData.cancel`).
struct TclCanceller {
    /// The `Tcl_AsyncHandler`, or 0 once the thread released it.
    handler: Mutex<usize>,
}

impl Canceller for TclCanceller {
    fn cancel(&self) -> bool {
        let h = self.handler.lock().unwrap_or_else(PoisonError::into_inner);
        if *h == 0 {
            return false;
        }
        // SAFETY: the handler is live while the slot is non-zero; it is
        // cleared under this lock before the handler is deleted.
        unsafe { ffi::Tcl_AsyncMark(*h as ffi::Tcl_AsyncHandler) };
        true
    }
}

struct CancelSlot(Option<Arc<TclCanceller>>);

impl Drop for CancelSlot {
    fn drop(&mut self) {
        // A thread exiting without `finalize_thread`: Tcl's own thread data
        // may already be gone, so the handler is abandoned, not deleted.
        if let Some(c) = self.0.take() {
            *c.handler.lock().unwrap_or_else(PoisonError::into_inner) = 0;
        }
    }
}

thread_local! {
    static CANCEL: RefCell<CancelSlot> = const { RefCell::new(CancelSlot(None)) };
}

/// The calling thread's canceller, creating its async handler on first use.
pub(crate) fn thread_canceller() -> Option<Arc<dyn Canceller>> {
    init_library();
    CANCEL
        .try_with(|slot| {
            let mut slot = slot.borrow_mut();
            if let Some(c) = &slot.0 {
                return Arc::clone(c) as Arc<dyn Canceller>;
            }
            // SAFETY: the library is initialised; the handler belongs to
            // this thread and is deleted by `release_thread_canceller`.
            let h = unsafe { ffi::Tcl_AsyncCreate(async_cancel, ptr::null_mut()) };
            let c = Arc::new(TclCanceller {
                handler: Mutex::new(h as usize),
            });
            slot.0 = Some(Arc::clone(&c));
            c as Arc<dyn Canceller>
        })
        .ok()
}

/// Delete the calling thread's async handler (before
/// `Tcl_FinalizeThread`).
pub(crate) fn release_thread_canceller() {
    let _ = CANCEL.try_with(|slot| {
        if let Some(c) = slot.borrow_mut().0.take() {
            let mut h = c.handler.lock().unwrap_or_else(PoisonError::into_inner);
            if *h != 0 {
                // SAFETY: created by this thread and not yet deleted.
                unsafe { ffi::Tcl_AsyncDelete(*h as ffi::Tcl_AsyncHandler) };
                *h = 0;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::{Read, Write};
    use std::rc::Rc;

    use ns_script::{Interp, ScriptEngine, TraceOps};

    use crate::{TclEngine, TclInterp};

    #[allow(clippy::unnecessary_box_returns)] // stable address required
    fn interp() -> Box<TclInterp> {
        TclEngine::new().create().expect("interp")
    }

    fn tmpfile(tag: &str) -> String {
        std::env::temp_dir()
            .join(format!("ns-tcl-{tag}-{}", std::process::id()))
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn channel_moves_between_threads() {
        let path = tmpfile("move");
        let i = interp();
        let ch = i.eval(&format!("open {path} w")).expect("open");
        let id = i.channel_id(&ch).expect("id");
        assert!(!i.channel_is_shared(&ch).expect("shared"));
        i.channel_check_mode(&ch, true).expect("writable");
        let e = i.channel_check_mode(&ch, false).expect_err("not readable");
        assert_eq!(e.message, format!("channel \"{ch}\" not open for read"));
        i.channel_write(&ch, "héllo ".as_bytes()).expect("write");
        let det = i.channel_detach(&ch).expect("detach");
        assert_eq!(
            (det.name(), det.id(), det.engine()),
            (ch.as_str(), id, "tcl")
        );
        let e = i.channel_id(&ch).expect_err("gone");
        assert_eq!(e.message, format!("can not find channel named \"{ch}\""));
        // Another thread writes to it and closes it.
        std::thread::spawn(move || {
            let j = TclEngine::new().create().expect("interp");
            let name = j.channel_attach(det).expect("attach");
            j.eval(&format!("puts -nonewline {name} world; close {name}"))
                .expect("use");
            drop(j);
            TclEngine::new().finalize_thread();
        })
        .join()
        .expect("join");
        assert_eq!(
            std::fs::read(&path).expect("read"),
            "héllo world".as_bytes()
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn channel_unregister_and_close_detached() {
        let path = tmpfile("close");
        let i = interp();
        let ch = i.eval(&format!("open {path} w")).expect("open");
        let id = i.channel_id(&ch).expect("id");
        // A stale identity leaves the channel alone.
        i.channel_unregister(&ch, id + 1);
        assert!(i.channel_id(&ch).is_ok());
        i.channel_unregister(&ch, id);
        assert!(i.channel_id(&ch).is_err());
        i.channel_unregister("nosuch", 0);
        let ch = i.eval(&format!("open {path} w")).expect("open");
        i.eval(&format!("puts -nonewline {ch} buffered"))
            .expect("puts");
        let det = i.channel_detach(&ch).expect("detach");
        i.channel_close_detached(det);
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "buffered");
        // Shared between two interps of one thread.
        let ch = i.eval(&format!("open {path} r")).expect("open");
        i.eval(&format!("interp create kid; interp share {{}} {ch} kid"))
            .expect("share");
        assert!(i.channel_is_shared(&ch).expect("shared"));
        assert!(i.channel_write("nosuch", b"x").is_err());
        assert!(i.channel_detach("nosuch").is_err());
        assert!(i.channel_is_shared("nosuch").is_err());
        assert!(i.channel_check_mode("nosuch", true).is_err());
        i.channel_check_mode(&ch, false).expect("readable");
        let e = i.channel_check_mode(&ch, true).expect_err("not writable");
        assert!(e.message.ends_with("not open for write"), "{}", e.message);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_failures_report_the_posix_error() {
        let i = interp();
        let r = i.eval("lindex [chan pipe] 0").expect("pipe");
        let e = i.channel_write(&r, b"x").expect_err("read end");
        assert!(e.message.starts_with("write failed: "), "{}", e.message);
    }

    #[test]
    fn foreign_detached_channels_are_refused() {
        let i = interp();
        let det = ns_script::DetachedChannel::new("other", "c".into(), 1, Box::new(()));
        let e = i.channel_attach(det).expect_err("foreign");
        assert!(e.message.contains("other engine"), "{}", e.message);
        let det = ns_script::DetachedChannel::new("tcl", "c".into(), 1, Box::new(()));
        assert!(i.channel_attach(det).is_err());
        i.channel_close_detached(ns_script::DetachedChannel::new(
            "x",
            "c".into(),
            1,
            Box::new(()),
        ));
        assert!(format!(
            "{:?}",
            ns_script::DetachedChannel::new("x", "c".into(), 1, Box::new(()))
        )
        .contains("DetachedChannel"));
    }

    #[test]
    fn tcp_channels() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = l.local_addr().expect("addr");
        let client = std::thread::spawn(move || {
            let mut s = std::net::TcpStream::connect(addr).expect("connect");
            s.write_all(b"ping\n").expect("write");
            let mut b = String::new();
            s.read_to_string(&mut b).expect("read");
            b
        });
        let (sock, _) = l.accept().expect("accept");
        let i = interp();
        let ch = i.channel_from_tcp(sock).expect("channel");
        assert!(ch.starts_with("sock"), "{ch}");
        assert_eq!(
            i.eval(&format!("gets {ch} line; puts {ch} pong:$line; close {ch}"))
                .expect("io"),
            ""
        );
        assert_eq!(client.join().expect("join"), "pong:ping\r\n");
    }

    #[test]
    fn variable_traces() {
        let i = interp();
        let log: Rc<RefCell<Vec<String>>> = Rc::default();
        let l = Rc::clone(&log);
        i.trace_var(
            "::v",
            TraceOps::ALL,
            7,
            Rc::new(move |interp, n1, n2, ops| {
                l.borrow_mut().push(format!(
                    "{n1}:{}:{}{}{}{}{}:{}",
                    n2.unwrap_or("-"),
                    u8::from(ops.reads),
                    u8::from(ops.writes),
                    u8::from(ops.unsets),
                    u8::from(ops.array),
                    u8::from(ops.destroyed),
                    u8::from(interp.is_some())
                ));
            }),
        )
        .expect("trace");
        assert_eq!(i.var_trace_keys("::v"), vec![7]);
        i.eval("set ::v(a) 1; set ::v(a); array names ::v; unset ::v")
            .expect("ops");
        assert_eq!(
            *log.borrow(),
            vec![
                "::v:a:01000:1",
                "::v:a:10000:1",
                "::v:-:00010:1",
                "::v:-:00101:1"
            ]
        );
        assert_eq!(i.var_trace_keys("::v"), Vec::<usize>::new());
        // set_var2/get_var2 address elements without parsing the name.
        i.set_var2("::w", Some("a(b"), "x").expect("set");
        assert_eq!(i.get_var2("::w", Some("a(b")).as_deref(), Some("x"));
        assert_eq!(i.get_var2("::w", None), None);
        i.set_var2("::s", None, "y").expect("set");
        assert_eq!(i.get_var2("::s", None).as_deref(), Some("y"));
        assert!(i.set_var2("::s", Some("k"), "z").is_err());
        assert!(i
            .trace_var("::nosuchns::x", TraceOps::ALL, 0, Rc::new(|_, _, _, _| {}))
            .is_err());
        // A trace still present when the interp is deleted sees no interp.
        let l = Rc::clone(&log);
        i.trace_var(
            "::z",
            TraceOps::ALL,
            1,
            Rc::new(move |interp, _, _, ops| {
                if ops.interp_destroyed {
                    l.borrow_mut().push(format!("gone:{}", interp.is_none()));
                }
            }),
        )
        .expect("trace");
        i.set_var2("::z", None, "1").expect("set");
        drop(i);
        assert_eq!(log.borrow().last().map(String::as_str), Some("gone:true"));
    }

    #[test]
    fn async_cancel_aborts_running_scripts() {
        let e = TclEngine::new();
        let i = e.create().expect("interp");
        let c = e.thread_canceller().expect("canceller");
        let again = e.thread_canceller().expect("same");
        assert!(again.cancel());
        let err = i.eval("set n 0; while 1 {incr n}").expect_err("cancelled");
        assert_eq!(err.message, "async cancel");
        // A pending cancel is cleared by invoke_async.
        assert!(c.cancel());
        i.invoke_async();
        assert_eq!(i.eval("expr 1+1"), Ok("2".into()));
        assert_ne!(i.handle(), 0);
        // Cancelling from another thread.
        let c2 = std::sync::Arc::clone(&c);
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            c2.cancel()
        });
        let err = i.eval("while 1 {}").expect_err("cancelled");
        assert_eq!(err.message, "async cancel");
        assert!(t.join().expect("join"));
        // The event loop services async handlers with no interp: there is
        // nothing to cancel then.
        assert!(c.cancel());
        // SAFETY: invoking this thread's handlers with no interp is what
        // Tcl_DoOneEvent does.
        assert_eq!(
            unsafe { crate::ffi::Tcl_AsyncInvoke(std::ptr::null_mut(), 0) },
            0
        );
        assert_eq!(i.eval("expr 2+2"), Ok("4".into()));
        drop(i);
        e.finalize_thread();
        assert!(!c.cancel(), "released with the thread");
        // A thread that exits without finalize abandons its handler.
        let c3 = std::thread::spawn(|| TclEngine::new().thread_canceller().expect("c"))
            .join()
            .expect("join");
        assert!(!c3.cancel());
    }
}
