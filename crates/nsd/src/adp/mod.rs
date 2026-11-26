//! AOLserver Dynamic Pages (`nsd/adpeval.c`, `nsd/adprequest.c`).
//!
//! Per-interp [`AdpState`] holds a stack of output buffers: the bottom one
//! is the page output (flushed to the connection), and `ns_adp_eval` /
//! `ns_adp_parse` push a buffer to capture output.  Included files share
//! their caller's buffer.  Control commands (`ns_adp_break`, `ns_adp_abort`,
//! `ns_adp_return`) set [`AdpException`] and fail, which stops block
//! execution; [`exec`] then clears `Return` at the file boundary.

pub mod parse;

use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError};

use ns_core::log::Severity;
use ns_script::{Exception, Interp, ScriptResult};

pub use parse::{AdpCode, Block, ParseOpts};

use crate::conn::{ConnRef, Status};
use crate::interp::{interp_conn, ns_interp, NsInterpRef};
use crate::op::{register_request, RequestEntry};
use crate::server::Server;

/// Pending ADP control exception.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AdpException {
    /// None.
    #[default]
    Ok,
    /// `ns_adp_break`: stop the page, flush output.
    Break,
    /// `ns_adp_abort`: stop the page, discard output.
    Abort,
    /// `ns_adp_return`: stop the current file only.
    Return,
}

/// An include frame (`AdpFrame`).
#[derive(Debug, Clone, Default)]
pub struct AdpFrame {
    /// File being executed (`None` for strings).
    pub file: Option<PathBuf>,
    /// `ns_adp_argv` arguments; `args[0]` is the file/string.
    pub args: Vec<String>,
    /// Directory for relative includes.
    pub cwd: PathBuf,
    /// Current line.
    pub line: usize,
    /// Identification string (`ns_adp_ident`).
    pub ident: String,
}

/// Per-interp ADP state (`itPtr->adp`).
#[derive(Debug, Default)]
pub struct AdpState {
    /// Output buffer stack; index 0 is the page buffer.
    pub outputs: Vec<String>,
    /// Include frames.
    pub frames: Vec<AdpFrame>,
    /// Pending control exception.
    pub exception: AdpException,
    /// Whether output goes to a connection.
    pub has_conn: bool,
    /// Something has already been flushed.
    pub flushed: bool,
    /// An error was logged (strict mode).
    pub errlogged: bool,
    /// Streaming enabled (`ns_adp_stream`).
    pub stream: bool,
    /// Auto-flush threshold.
    pub bufsize: usize,
    /// Response MIME type (`ns_adp_mimetype`).
    pub mimetype: Option<String>,
    /// Trim leading whitespace before the first flush.
    pub trim: bool,
    /// Gzip output.
    pub gzip: bool,
    /// Send `Expires: now`.
    pub expire: bool,
    /// Stop at the first error.
    pub strict: bool,
    /// Abort when a flush fails.
    pub autoabort: bool,
    /// Show errors in the page.
    pub display: bool,
    /// Last `ns_adp_eval` result captured into a variable.
    pub depth: usize,
    /// Channel that receives the output instead of the connection
    /// (`ns_adp_ctl channel`).
    pub chan: Option<String>,
    /// TclPro debugger nesting level (`debugLevel`).
    pub debug_level: i32,
    /// The debugger has been initialised (`debugInit`).
    pub debug_init: bool,
    /// Pattern of files to debug, from the `debug` query field
    /// (`debugFile`).
    pub debug_file: Option<String>,
}

const MAX_DEPTH: usize = 256;

fn state(interp: &dyn Interp) -> Result<NsInterpRef, Exception> {
    ns_interp(interp)
}

/// Append text to the current output buffer (`NsAdpAppend`), flushing to the
/// connection when streaming or past `bufsize`.
pub fn append(interp: &dyn Interp, text: &str) -> ScriptResult {
    let st = state(interp)?;
    let flush = {
        let mut s = st.borrow_mut();
        let a = &mut s.adp;
        if a.outputs.is_empty() {
            a.outputs.push(String::new());
        }
        let top = a.outputs.len() - 1;
        a.outputs[top].push_str(text);
        a.outputs.len() == 1
            && (a.has_conn || a.chan.is_some())
            && (a.stream || (a.bufsize > 0 && a.outputs[0].len() > a.bufsize))
    };
    if flush {
        flush_output(interp, true)?;
    }
    Ok(String::new())
}

/// Raise an ADP control exception (used by `ns_adp_break`/`abort`/`return`).
pub fn raise(interp: &dyn Interp, ex: AdpException) -> ScriptResult {
    let st = state(interp)?;
    st.borrow_mut().adp.exception = ex;
    Err(Exception::error("adp exception raised").error_code("ADP EXCEPTION"))
}

/// Log an ADP error with file and line (`NsAdpLogError`), optionally
/// appending it to the output.
pub fn log_error(interp: &dyn Interp, e: &Exception) {
    let Ok(st) = state(interp) else {
        return;
    };
    let (server, file, line, display) = {
        let s = st.borrow();
        let f = s.adp.frames.last();
        (
            Arc::clone(&s.server),
            f.and_then(|f| f.file.clone()),
            f.map_or(0, |f| f.line),
            s.adp.display,
        )
    };
    let mut msg = String::from(e.detail());
    if let Some(f) = &file {
        msg.push_str(&format!(
            "\n    at line {} of adp file \"{}\"",
            line + 1,
            f.display()
        ));
    }
    crate::interp::log_error(&server, interp, &Exception::error(msg.clone()));
    if display {
        let _ = append(
            interp,
            &format!("<br><pre>\n{}\n</pre><br>", ns_core::text::quote_html(&msg)),
        );
    }
}

/// Execute parsed code in a new frame (`AdpExec`).
pub fn exec(interp: &dyn Interp, code: &AdpCode, frame: AdpFrame) -> ScriptResult {
    let st = state(interp)?;
    {
        let mut s = st.borrow_mut();
        if s.adp.depth >= MAX_DEPTH {
            return Err(Exception::error("adp stack overflow"));
        }
        s.adp.depth += 1;
        s.adp.frames.push(frame);
    }
    let mut result = Ok(String::new());
    for (block, line) in &code.blocks {
        if st.borrow().adp.exception != AdpException::Ok {
            break;
        }
        if let Some(f) = st.borrow_mut().adp.frames.last_mut() {
            f.line = *line;
        }
        let r = match block {
            Block::Text(t) => append(interp, t),
            Block::Script(s) => interp.eval(s),
        };
        if let Err(e) = r {
            if st.borrow().adp.exception == AdpException::Ok {
                let logged = st.borrow().adp.errlogged;
                if !logged {
                    log_error(interp, &e);
                }
                if st.borrow().adp.strict {
                    st.borrow_mut().adp.errlogged = true;
                    result = Err(e);
                    break;
                }
            }
        }
    }
    let mut s = st.borrow_mut();
    s.adp.frames.pop();
    s.adp.depth -= 1;
    if s.adp.exception == AdpException::Return {
        s.adp.exception = AdpException::Ok;
    }
    if s.adp.exception != AdpException::Ok {
        result = Ok(String::new());
    }
    result
}

/// Parse options for a server.
pub fn parse_opts(server: &Server) -> ParseOpts {
    ParseOpts {
        safe: server.adp.safe,
        single: server.adp.single,
    }
}

/// Parse `text` with the server's registered tags.
pub fn parse_text(server: &Server, text: &str) -> AdpCode {
    let tags = server
        .adp_tags
        .read()
        .unwrap_or_else(PoisonError::into_inner);
    parse::parse(text, &tags, parse_opts(server))
}

/// Load and parse a page, using the server's cache keyed by mtime and size.
pub fn load_page(server: &Server, path: &Path) -> Result<Arc<AdpCode>, Exception> {
    let meta = std::fs::metadata(path)
        .map_err(|e| Exception::error(format!("could not stat \"{}\": {e}", path.display())))?;
    let mtime = meta
        .modified()
        .map(ns_core::httptime::unix_time)
        .unwrap_or_default();
    let len = meta.len();
    if let Some((m, l, code)) = server
        .adp_pages
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(path)
    {
        if *m == mtime && *l == len {
            return Ok(Arc::clone(code));
        }
    }
    let bytes = std::fs::read(path)
        .map_err(|e| Exception::error(format!("could not read \"{}\": {e}", path.display())))?;
    let text = String::from_utf8_lossy(&bytes);
    let code = Arc::new(parse_text(server, &text));
    server
        .adp_pages
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(path.to_path_buf(), (mtime, len, Arc::clone(&code)));
    Ok(code)
}

/// Resolve `file` relative to the current frame's directory.
pub fn resolve(interp: &dyn Interp, file: &str) -> Result<PathBuf, Exception> {
    let p = Path::new(file);
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    let st = state(interp)?;
    let s = st.borrow();
    let base = s
        .adp
        .frames
        .last()
        .map_or_else(|| s.server.fastpath.pageroot.clone(), |f| f.cwd.clone());
    Ok(base.join(p))
}

/// Include a file into the current output (`NsAdpInclude`).
pub fn include(interp: &dyn Interp, file: &str, args: &[String]) -> ScriptResult {
    let path = resolve(interp, file)?;
    let st = state(interp)?;
    let server = Arc::clone(&st.borrow().server);
    let code = load_page(&server, &path)?;
    // TclPro debugging (AdpSource): count nested files once the debugger
    // runs, or start it for a file matching the page's debug pattern.
    let debug_file = {
        let mut s = st.borrow_mut();
        if s.adp.debug_level > 0 {
            s.adp.debug_level += 1;
            None
        } else if server.adp.debug {
            s.adp.debug_file.clone()
        } else {
            None
        }
    };
    if let Some(pat) = debug_file {
        let tail = path
            .file_name()
            .map(|t| t.to_string_lossy().into_owned())
            .unwrap_or_default();
        if ns_core::glob::string_match(&tail, &pat) {
            if let Some(conn) = crate::interp::try_conn(interp) {
                let q = conn.form().fields;
                let get = |k: &str| q.iget(k).map(str::to_owned);
                let (host, port, procs) = (get("dhost"), get("dport"), get("dprocs"));
                if let Err(e) =
                    debug_init(interp, host.as_deref(), port.as_deref(), procs.as_deref())
                {
                    let _ = conn.return_notice(200, "Debug Init Failed", &e.message);
                    st.borrow_mut().adp.exception = AdpException::Abort;
                    return Ok(String::new());
                }
            }
        }
    }
    let r = include_code(interp, &code, path, args);
    let mut s = st.borrow_mut();
    if s.adp.debug_level > 0 {
        s.adp.debug_level -= 1;
    }
    r
}

fn include_code(
    interp: &dyn Interp,
    code: &AdpCode,
    path: PathBuf,
    args: &[String],
) -> ScriptResult {
    let mut argv = vec![path.to_string_lossy().into_owned()];
    argv.extend_from_slice(args);
    let frame = AdpFrame {
        cwd: path.parent().map(Path::to_path_buf).unwrap_or_default(),
        file: Some(path),
        args: argv,
        line: 0,
        ident: String::new(),
    };
    exec(interp, code, frame)
}

/// Evaluate ADP text or a file, capturing and returning its output
/// (`NsAdpEval` / `NsAdpSource`).
pub fn eval_capture(
    interp: &dyn Interp,
    target: &str,
    is_file: bool,
    args: &[String],
) -> ScriptResult {
    let st = state(interp)?;
    st.borrow_mut().adp.outputs.push(String::new());
    if st.borrow().adp.outputs.len() == 1 {
        // No page buffer yet: push the capture buffer above an empty base.
        st.borrow_mut().adp.outputs.insert(0, String::new());
    }
    let r = if is_file {
        include(interp, target, args)
    } else {
        let server = Arc::clone(&st.borrow().server);
        let code = parse_text(&server, target);
        let mut argv = vec![target.to_owned()];
        argv.extend_from_slice(args);
        let cwd = st
            .borrow()
            .adp
            .frames
            .last()
            .map_or_else(|| server.fastpath.pageroot.clone(), |f| f.cwd.clone());
        exec(
            interp,
            &code,
            AdpFrame {
                file: None,
                args: argv,
                cwd,
                line: 0,
                ident: String::new(),
            },
        )
    };
    let out = st.borrow_mut().adp.outputs.pop().unwrap_or_default();
    r.map(|_| out)
}

/// Send buffered output to the connection (`NsAdpFlush`).
pub fn flush_output(interp: &dyn Interp, stream: bool) -> ScriptResult {
    let st = state(interp)?;
    let (mut buf, ex, trim, flushed, gzip, expire, autoabort, has_conn, mimetype, chan) = {
        let mut s = st.borrow_mut();
        let a = &mut s.adp;
        if a.outputs.is_empty() {
            a.outputs.push(String::new());
        }
        (
            std::mem::take(&mut a.outputs[0]),
            a.exception,
            a.trim,
            a.flushed,
            a.gzip,
            a.expire,
            a.autoabort,
            a.has_conn,
            a.mimetype.clone(),
            a.chan.clone(),
        )
    };
    if !has_conn && chan.is_none() {
        return Err(Exception::error("no adp output context"));
    }
    if trim && !flushed {
        buf = buf.trim_start().to_owned();
    }
    if ex == AdpException::Abort {
        return Err(Exception::error("adp flush disabled: adp aborted"));
    }
    if buf.is_empty() && stream {
        return Ok(String::new());
    }
    let mut result: ScriptResult = Ok(String::new());
    if let Some(ch) = chan {
        if let Err(e) = interp.channel_write(&ch, buf.as_bytes()) {
            result = Err(e);
        }
        return finish_flush(&st, result, autoabort);
    }
    let conn = interp_conn(interp)?;
    if conn.is_closed() {
        result = Err(Exception::error("adp flush failed: connection closed"));
    } else {
        if gzip {
            conn.conn_mut().flags.gzip = true;
        }
        if !flushed && expire {
            conn.cond_set_header("Expires", "now");
        }
        if !conn.headers_sent() {
            if let Some(m) = mimetype {
                conn.set_type(Some(&m));
            }
        }
        if conn.flush(buf.as_bytes(), stream).is_err() {
            result = Err(Exception::error("adp flush failed: connection flush error"));
        }
    }
    finish_flush(&st, result, autoabort)
}

/// The end of `NsAdpFlush`: mark output flushed and raise an abort if a
/// failed flush should stop the page.
fn finish_flush(st: &NsInterpRef, result: ScriptResult, autoabort: bool) -> ScriptResult {
    st.borrow_mut().adp.flushed = true;
    if result.is_err() && autoabort {
        st.borrow_mut().adp.exception = AdpException::Abort;
    }
    result
}

/// `NsAdpReset` after a final flush with no connection: the state the
/// next execution starts from (output context and options from the
/// server).
pub fn reset(interp: &dyn Interp) -> Result<(), Exception> {
    let st = state(interp)?;
    let mut s = st.borrow_mut();
    let server = Arc::clone(&s.server);
    let cfg = &server.adp;
    let mut outputs = std::mem::take(&mut s.adp.outputs);
    match outputs.first_mut() {
        Some(base) => base.clear(),
        None => outputs.push(String::new()),
    }
    let fresh = AdpState {
        outputs,
        frames: std::mem::take(&mut s.adp.frames),
        depth: s.adp.depth,
        bufsize: cfg.bufsize,
        trim: cfg.trim,
        gzip: cfg.gzip,
        expire: cfg.expire,
        strict: cfg.strict,
        autoabort: cfg.autoabort,
        display: cfg.display,
        ..AdpState::default()
    };
    s.adp = fresh;
    Ok(())
}

/// `NsAdpDebug`: initialise the TclPro debugger once per interp by
/// calling the server's `debuginit` proc with the procs to instrument
/// and the debugger's host and port.  The interp is marked for deletion.
pub fn debug_init(
    interp: &dyn Interp,
    host: Option<&str>,
    port: Option<&str>,
    procs: Option<&str>,
) -> Result<(), Exception> {
    let st = state(interp)?;
    if st.borrow().adp.debug_init {
        return Ok(());
    }
    interp.mark_for_delete();
    let proc_ = st.borrow().server.adp.debuginit.clone();
    let script = interp.list_join(&[
        &proc_,
        procs.unwrap_or_default(),
        host.unwrap_or_default(),
        port.unwrap_or_default(),
    ]);
    if let Err(e) = interp.eval(&script) {
        log_error(interp, &e);
        return Err(e);
    }
    link_output(interp, &st);
    let mut s = st.borrow_mut();
    s.adp.debug_init = true;
    s.adp.debug_level = 1;
    Ok(())
}

/// Make `ns_adp_output` show the ADP output buffer (`Tcl_LinkVar`
/// read-only): every read refreshes it.
fn link_output(interp: &dyn Interp, st: &NsInterpRef) {
    let weak = std::rc::Rc::downgrade(st);
    let current = move || {
        weak.upgrade()
            .and_then(|s| s.borrow().adp.outputs.first().cloned())
            .unwrap_or_default()
    };
    let _ = interp.set_global("ns_adp_output", &current());
    let r = interp.trace_var(
        "::ns_adp_output",
        ns_script::TraceOps {
            reads: true,
            writes: true,
            ..ns_script::TraceOps::default()
        },
        LINK_KEY,
        std::rc::Rc::new(move |i, _, _, ops| {
            if let Some(i) = i {
                if ops.reads || ops.writes {
                    let _ = i.set_global("ns_adp_output", &current());
                }
            }
        }),
    );
    if let Err(e) = r {
        log_error(interp, &e);
    }
}

/// Trace key of the `ns_adp_output` link.
const LINK_KEY: usize = 0x6164_7000;

/// Serve an ADP file for a connection (`Ns_AdpRequest`).
pub fn adp_request(conn: &ConnRef, file: &str) -> Status {
    let path = {
        let p = Path::new(file);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            conn.server.fastpath.pageroot.join(p)
        }
    };
    if !path.is_file() {
        let _ = conn.return_not_found();
        return Status::Ok;
    }
    let interp = match conn.interp() {
        Ok(i) => i,
        Err(e) => {
            conn.server.global.log(Severity::Error, &e.message);
            let _ = conn.return_internal_error();
            return Status::Ok;
        }
    };
    let ty = {
        let m = conn
            .server
            .global
            .mime
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let t = m.guess(&path.to_string_lossy()).to_owned();
        if t == "*/*" {
            ns_core::mimetypes::TEXT_HTML.to_owned()
        } else {
            t
        }
    };
    conn.set_type(Some(&ty));
    conn.set_status(200);
    let server = Arc::clone(&conn.server);
    if let Ok(st) = ns_interp(&**interp) {
        let cfg = &server.adp;
        let mut s = st.borrow_mut();
        s.adp = AdpState {
            outputs: vec![String::new()],
            has_conn: true,
            bufsize: cfg.bufsize,
            trim: cfg.trim,
            gzip: cfg.gzip,
            expire: cfg.expire,
            strict: cfg.strict,
            autoabort: cfg.autoabort,
            display: cfg.display,
            debug_file: (cfg.debug && conn.method() == "GET")
                .then(|| conn.form().fields.iget("debug").map(str::to_owned))
                .flatten(),
            ..AdpState::default()
        };
    }
    let file_s = path.to_string_lossy().into_owned();
    let start = server
        .adp
        .startpage
        .clone()
        .unwrap_or_else(|| file_s.clone());
    let args = if server.adp.startpage.is_some() {
        vec![file_s]
    } else {
        Vec::new()
    };
    let r = include(&**interp, &start, &args);
    if let Err(e) = &r {
        log_error(&**interp, e);
    }
    let ex = ns_interp(&**interp)
        .map(|s| s.borrow().adp.exception)
        .unwrap_or_default();
    if ex != AdpException::Abort && !conn.is_closed() {
        if let Err(e) = flush_output(&**interp, false) {
            crate::interp::log_error(&server, &**interp, &e);
        }
    }
    if let Ok(st) = ns_interp(&**interp) {
        st.borrow_mut().adp = AdpState::default();
    }
    Status::Ok
}

/// Register ADP processing for `method url` (`ns/server/S/adp` `map`).
pub fn register_map(server: &Arc<Server>, method: &str, url: &str) {
    register_request(
        server,
        method,
        url,
        RequestEntry {
            proc_: Arc::new(|conn: &ConnRef| {
                let url = conn.url();
                let path = crate::fastpath::url_to_file(&conn.server, &url);
                adp_request(conn, &path.to_string_lossy())
            }),
            desc: "ns:adp".into(),
        },
        ns_core::UrlFlags::INHERIT,
    );
}

/// Register a specific ADP file for `method url` (`ns_register_adp`).
pub fn register_file(server: &Arc<Server>, method: &str, url: &str, file: &str, noinherit: bool) {
    let file = file.to_owned();
    register_request(
        server,
        method,
        url,
        RequestEntry {
            proc_: Arc::new(move |conn: &ConnRef| adp_request(conn, &file)),
            desc: "ns:adpfile".into(),
        },
        ns_core::UrlFlags {
            noinherit,
            recurse: false,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state() {
        let s = AdpState::default();
        assert_eq!(s.exception, AdpException::Ok);
        assert_eq!(s.outputs.len(), 0);
    }
}
