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
            && a.has_conn
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
    let server = Arc::clone(&state(interp)?.borrow().server);
    let code = load_page(&server, &path)?;
    let mut argv = vec![path.to_string_lossy().into_owned()];
    argv.extend_from_slice(args);
    let frame = AdpFrame {
        cwd: path.parent().map(Path::to_path_buf).unwrap_or_default(),
        file: Some(path),
        args: argv,
        line: 0,
        ident: String::new(),
    };
    exec(interp, &code, frame)
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
    let (mut buf, ex, trim, flushed, gzip, expire, autoabort, has_conn, mimetype) = {
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
        )
    };
    if !has_conn {
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
    let conn = interp_conn(interp)?;
    let mut result: ScriptResult = Ok(String::new());
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
    st.borrow_mut().adp.flushed = true;
    if result.is_err() && autoabort {
        st.borrow_mut().adp.exception = AdpException::Abort;
    }
    result
}

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
