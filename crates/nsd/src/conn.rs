//! Connections: the `Ns_Conn` API, output (`nsd/connio.c`) and canned
//! responses (`nsd/return.c`).
//!
//! A [`ConnCtx`] is created by a connection thread for each request and
//! shared (`Rc`) between the request pipeline and the scripting commands
//! that act on it.  It is thread-confined; the socket is written with
//! blocking calls into the async runtime.

use std::any::Any;
use std::cell::{Ref, RefCell, RefMut};
use std::fmt::Write as _;
use std::io::{self, Read};
use std::net::SocketAddr;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use encoding_rs::Encoding;
use ns_core::form::Form;
use ns_core::httptime::{http_time, now, parse_http_time, unix_time};
use ns_core::log::Severity;
use ns_core::request::Request;
use ns_core::NsSet;
use tokio::io::AsyncWriteExt;

use crate::content::Content;
use crate::driver::{DriverInfo, Sock};
use crate::interp::InterpLease;
use crate::pool::Limits;
use crate::server::Server;

/// A set shared between a connection and the scripts that reference it.
pub type SetRef = Arc<Mutex<NsSet>>;

/// Wrap a set for sharing.
pub fn set_ref(s: NsSet) -> SetRef {
    Arc::new(Mutex::new(s))
}

/// Lock a shared set.
pub fn lock_set(s: &SetRef) -> std::sync::MutexGuard<'_, NsSet> {
    s.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Result status of request procs, filters and authorization
/// (`NS_OK`, `NS_ERROR`, `NS_FILTER_BREAK`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Success / continue.
    Ok,
    /// Failure.
    Error,
    /// Stop running filters, continue the request.
    FilterBreak,
    /// Stop running filters, the response has been sent.
    FilterReturn,
    /// Authorization: credentials required.
    Unauthorized,
    /// Authorization: access denied.
    Forbidden,
}

/// Connection state flags (`NS_CONN_*`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ConnFlags {
    /// The socket has been released.
    pub closed: bool,
    /// Don't send headers (HTTP/0.9).
    pub skip_hdrs: bool,
    /// Don't send a body (HEAD).
    pub skip_body: bool,
    /// Headers have been queued/sent.
    pub sent_hdrs: bool,
    /// Keep the connection open after the response.
    pub keepalive: bool,
    /// Gzip output if possible.
    pub gzip: bool,
    /// Output uses chunked transfer encoding.
    pub chunk: bool,
    /// The request was rejected for capacity.
    pub overflow: bool,
}

/// Timestamps for a connection's lifecycle.
#[derive(Debug, Clone, Copy)]
pub struct ConnTimes {
    /// Socket accepted.
    pub accept: SystemTime,
    /// Request fully read and queued.
    pub queue: Instant,
    /// Processing started.
    pub run: Instant,
}

/// The reason phrase for a status code (`reasons[]` in return.c).
pub fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        102 => "Processing",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        203 => "Non-Authoritative Information",
        204 => "No Content",
        205 => "Reset Content",
        206 => "Partial Content",
        207 => "Multi-Status",
        300 => "Multiple Choices",
        301 => "Moved",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        305 => "Use Proxy",
        307 => "Temporary Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Request Entity Too Large",
        414 => "Request-URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Requested Range Not Satisfiable",
        417 => "Expectation Failed",
        422 => "Unprocessable Entity",
        423 => "Locked",
        424 => "Method Failure",
        425 => "Insufficient Space On Resource",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        507 => "Insufficient Storage",
        _ => "Unknown Reason",
    }
}

/// Maximum internal-redirect depth (`MAX_RECURSION`).
pub const MAX_RECURSION: u32 = 3;

/// The connection proper (`struct Conn`).
pub struct Conn {
    /// Unique id.
    pub id: u64,
    /// Parsed request line.
    pub request: Request,
    /// Request headers.
    pub headers: SetRef,
    /// Response headers.
    pub output_headers: SetRef,
    /// Basic-auth user.
    pub auth_user: Option<String>,
    /// Basic-auth password.
    pub auth_passwd: Option<String>,
    /// Request body, in memory or spooled to a temp file.  Released (the
    /// temp file returned to the pool) when the connection is dropped.
    pub content: Content,
    /// Read offset into `content` for `ns_conn read`-style consumers.
    pub content_offset: usize,
    /// State flags.
    pub flags: ConnFlags,
    /// Response status.
    pub status: u16,
    /// Response content type.
    pub content_type: Option<String>,
    /// Body bytes sent so far (excluding headers).
    pub n_content_sent: u64,
    /// Client address.
    pub peer: SocketAddr,
    /// Driver that accepted the connection.
    pub driver: Arc<DriverInfo>,
    /// Lifecycle times.
    pub times: ConnTimes,
    /// Encoding used to decode the URL.
    pub url_encoding: Option<&'static Encoding>,
    /// Encoding used to decode form data.
    pub query_encoding: Option<&'static Encoding>,
    /// Encoding for text output (`None` = UTF-8).
    pub output_encoding: Option<&'static Encoding>,
    /// Parsed form, cached.
    pub form: Option<Form>,
    /// Internal redirect depth.
    pub recursion: u32,
    /// The client socket while open.
    pub sock: Option<Sock>,
    /// Queued header bytes not yet written.
    pub obuf: Vec<u8>,
    /// Conn-local storage (`Ns_Cls`).
    pub cls: Vec<Option<Box<dyn Any>>>,
    /// Limits applied to this request.
    pub limits: Option<Arc<Limits>>,
    /// Pool name the request ran in.
    pub pool: String,
}

impl std::fmt::Debug for Conn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Conn")
            .field("id", &self.id)
            .field("method", &self.request.method)
            .field("url", &self.request.url)
            .finish_non_exhaustive()
    }
}

/// A connection plus its lazily-allocated interpreter.
pub struct ConnCtx {
    /// The virtual server.
    pub server: Arc<Server>,
    conn: RefCell<Conn>,
    interp: RefCell<Option<Rc<InterpLease>>>,
    at_close: RefCell<Vec<String>>,
}

impl std::fmt::Debug for ConnCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnCtx")
            .field("conn", &self.conn)
            .finish_non_exhaustive()
    }
}

/// Shared handle to a connection.
pub type ConnRef = Rc<ConnCtx>;

fn hdr_eq(set: &NsSet, name: &str, value: &str) -> bool {
    set.iget(name)
        .is_some_and(|v| v.trim().eq_ignore_ascii_case(value))
}

impl ConnCtx {
    /// Wrap a connection.
    pub fn new(server: Arc<Server>, conn: Conn) -> ConnRef {
        Rc::new(Self {
            server,
            conn: RefCell::new(conn),
            interp: RefCell::new(None),
            at_close: RefCell::new(Vec::new()),
        })
    }

    /// Borrow the connection.
    pub fn conn(&self) -> Ref<'_, Conn> {
        self.conn.borrow()
    }

    /// Mutably borrow the connection.  Never hold across script evaluation.
    pub fn conn_mut(&self) -> RefMut<'_, Conn> {
        self.conn.borrow_mut()
    }

    /// Unique connection id.
    pub fn id(&self) -> u64 {
        self.conn().id
    }

    /// Request method.
    pub fn method(&self) -> String {
        self.conn().request.method.clone()
    }

    /// Request URL.
    pub fn url(&self) -> String {
        self.conn().request.url.clone()
    }

    /// Request headers.
    pub fn headers(&self) -> SetRef {
        Arc::clone(&self.conn().headers)
    }

    /// Response headers.
    pub fn output_headers(&self) -> SetRef {
        Arc::clone(&self.conn().output_headers)
    }

    /// Case-insensitive request header lookup.
    pub fn header(&self, name: &str) -> Option<String> {
        lock_set(&self.conn().headers).iget(name).map(str::to_owned)
    }

    /// The server's interpreter for this connection (`Ns_GetConnInterp`).
    pub fn interp(self: &Rc<Self>) -> Result<Rc<InterpLease>, ns_script::Exception> {
        if let Some(i) = self.interp.borrow().as_ref() {
            return Ok(Rc::clone(i));
        }
        let lease = Rc::new(crate::interp::allocate(&self.server)?);
        crate::interp::attach_conn(&lease, Rc::downgrade(self));
        *self.interp.borrow_mut() = Some(Rc::clone(&lease));
        Ok(lease)
    }

    /// Whether an interpreter has been allocated.
    pub fn has_interp(&self) -> bool {
        self.interp.borrow().is_some()
    }

    /// Release the interpreter (`NsFreeConnInterp`).
    pub fn free_interp(&self) {
        let lease = self.interp.borrow_mut().take();
        if let Some(l) = lease {
            crate::interp::detach_conn(&l);
        }
    }

    /// Register a script to run when the connection closes (`ns_atclose`).
    pub fn at_close(&self, script: String) {
        self.at_close.borrow_mut().push(script);
    }

    /// The connection's location, e.g. `http://host:port`
    /// (`Ns_ConnLocation`).
    pub fn location(&self) -> String {
        let proc_ = self.server.location_proc();
        if let Some(p) = proc_ {
            if let Some(loc) = p(self) {
                return loc;
            }
        }
        let c = self.conn();
        if c.driver.virtual_hosts {
            if let Some(host) = lock_set(&c.headers).iget("host") {
                return format!("{}://{}", c.driver.protocol, host.trim());
            }
        }
        c.driver.location()
    }

    /// Whether the response headers have been sent.
    pub fn headers_sent(&self) -> bool {
        self.conn().flags.sent_hdrs
    }

    /// Whether the connection is closed.
    pub fn is_closed(&self) -> bool {
        self.conn().flags.closed
    }

    // ---- header helpers (Ns_ConnSetHeaders & friends) ----

    /// Append an output header.
    pub fn set_header(&self, key: &str, value: &str) {
        lock_set(&self.conn().output_headers).put_str(key, value);
    }

    /// Replace an output header (case-insensitively).
    pub fn update_header(&self, key: &str, value: &str) {
        lock_set(&self.conn().output_headers).iupdate(key, Some(value));
    }

    /// Set an output header unless already present (`Ns_ConnCondSetHeaders`).
    pub fn cond_set_header(&self, key: &str, value: &str) {
        let c = self.conn();
        let mut h = lock_set(&c.output_headers);
        if h.ifind(key).is_none() {
            h.put_str(key, value);
        }
    }

    /// Set the response status.
    pub fn set_status(&self, status: u16) {
        self.conn_mut().status = status;
    }

    /// Response status.
    pub fn status(&self) -> u16 {
        self.conn().status
    }

    /// Set the response type (`Ns_ConnSetType`).  A `text/` type without a
    /// charset gets the server's output charset appended and selects the
    /// output encoding.
    pub fn set_type(&self, ty: Option<&str>) {
        let Some(ty) = ty else {
            self.conn_mut().content_type = None;
            return;
        };
        let mut ty = ty.to_owned();
        let lower = ty.to_ascii_lowercase();
        let charset = lower
            .find("charset=")
            .map(|i| ty[i + 8..].trim().trim_matches('"').to_owned());
        let enc = if let Some(cs) = &charset {
            ns_core::urlencode::charset_encoding(cs)
        } else if lower.starts_with("text/") {
            if let Some(def) = &self.server.charsets.output_charset {
                let _ = write!(ty, "; charset={def}");
                ns_core::urlencode::charset_encoding(def)
            } else {
                None
            }
        } else {
            None
        };
        let mut c = self.conn_mut();
        if charset.is_some() || lower.starts_with("text/") {
            c.output_encoding = enc;
        }
        c.content_type = Some(ty);
    }

    /// Set the standard response headers (`Ns_ConnSetRequiredHeaders`).
    pub fn set_required_headers(&self, ty: Option<&str>, length: Option<u64>) {
        self.cond_set_header("MIME-Version", "1.0");
        self.cond_set_header("Date", &http_time(now()));
        let mut server = String::new();
        if self.server.opts.aolpress {
            server.push_str("NaviServer/2.0 ");
        }
        let _ = write!(
            server,
            "{}/{}",
            crate::global::SERVER_NAME,
            crate::global::SERVER_VERSION
        );
        self.cond_set_header("Server", &server);
        if ty.is_some() && ty.map(str::to_owned) != self.conn().content_type {
            self.set_type(ty);
        }
        let cur = self.conn().content_type.clone();
        if let Some(t) = cur {
            self.update_header("Content-Type", &t);
        }
        if let Some(len) = length {
            self.update_header("Content-Length", &len.to_string());
        }
    }

    fn check_keep(&self, status: u16) -> bool {
        let c = self.conn();
        if c.driver.keepwait.is_zero() || c.flags.overflow {
            return false;
        }
        let m = c.request.method.as_str();
        if m != "GET" && m != "HEAD" {
            return false;
        }
        let headers = lock_set(&c.headers);
        let wants = if c.request.major > 1 || (c.request.major == 1 && c.request.minor >= 1) {
            !hdr_eq(&headers, "connection", "close")
        } else {
            hdr_eq(&headers, "connection", "keep-alive")
        };
        drop(headers);
        if !wants {
            return false;
        }
        if status == 304 || status == 204 {
            return true;
        }
        if (200..300).contains(&status) || (300..400).contains(&status) || status >= 400 {
            let out = lock_set(&c.output_headers);
            if hdr_eq(&out, "transfer-encoding", "chunked") {
                return true;
            }
            return out.iget("content-length").is_some();
        }
        false
    }

    /// Build the status line and headers (`Ns_ConnConstructHeaders`).
    pub fn construct_headers(&self) -> Vec<u8> {
        let status = self.status();
        let (major, minor) = {
            let c = self.conn();
            let p = &self.server.global.params;
            match c.request.major.cmp(&p.http_major) {
                std::cmp::Ordering::Greater => (p.http_major, p.http_minor),
                std::cmp::Ordering::Equal => (c.request.major, c.request.minor.min(p.http_minor)),
                std::cmp::Ordering::Less => (c.request.major, c.request.minor),
            }
        };
        let mut out = format!("HTTP/{major}.{minor} {status} {}\r\n", reason(status));
        if !self.conn().flags.keepalive && self.check_keep(status) {
            self.conn_mut().flags.keepalive = true;
        }
        let keep = if self.conn().flags.keepalive {
            "keep-alive"
        } else {
            "close"
        };
        self.cond_set_header("Connection", keep);
        let c = self.conn();
        for (k, v) in lock_set(&c.output_headers).iter() {
            if let Some(v) = v {
                let _ = write!(out, "{k}: {v}\r\n");
            }
        }
        out.push_str("\r\n");
        out.into_bytes()
    }

    /// Queue the headers for sending with the first body write
    /// (`Ns_ConnQueueHeaders`).
    pub fn queue_headers(&self, status: u16) {
        if self.conn().flags.sent_hdrs {
            return;
        }
        self.set_status(status);
        if !self.conn().flags.skip_hdrs {
            let h = self.construct_headers();
            self.conn_mut().obuf.extend_from_slice(&h);
        }
        self.conn_mut().flags.sent_hdrs = true;
    }

    /// Write queued headers plus `bufs` to the client (`Ns_ConnSend`).
    /// Returns the number of body bytes written.
    pub fn send(self: &Rc<Self>, bufs: &[&[u8]]) -> io::Result<usize> {
        let (mut out, sendwait) = {
            let mut c = self.conn_mut();
            if c.sock.is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "connection closed",
                ));
            }
            (std::mem::take(&mut c.obuf), c.driver.sendwait)
        };
        let body: usize = bufs.iter().map(|b| b.len()).sum();
        for b in bufs {
            out.extend_from_slice(b);
        }
        if !out.is_empty() {
            let runtime = self.server.global.runtime.clone();
            let mut sock = self.conn_mut().sock.take();
            let result = if let Some(s) = sock.as_mut() {
                send_bounded(&runtime, s, &out, sendwait)
            } else {
                Ok(())
            };
            let mut c = self.conn_mut();
            c.sock = sock;
            if let Err(e) = result {
                c.flags.keepalive = false;
                return Err(e);
            }
            c.n_content_sent += body as u64;
        }
        if crate::op::run_filters(self, crate::op::FilterWhen::Write) != Status::Ok {
            return Err(io::Error::other("write filter failed"));
        }
        Ok(body)
    }

    /// Write raw bytes, sending headers first if needed (`Ns_WriteConn`).
    pub fn write(self: &Rc<Self>, data: &[u8]) -> io::Result<()> {
        if !self.headers_sent() {
            let status = self.status();
            self.queue_headers(status);
        }
        let skip = self.conn().flags.skip_body;
        if skip {
            self.send(&[])?;
        } else {
            self.send(&[data])?;
        }
        Ok(())
    }

    fn encode_output(&self, data: &[u8]) -> Vec<u8> {
        let enc = self.conn().output_encoding;
        match enc {
            Some(e) if e != encoding_rs::UTF_8 => {
                let s = String::from_utf8_lossy(data);
                e.encode(&s).0.into_owned()
            }
            _ => data.to_vec(),
        }
    }

    /// Send text output with encoding and optional gzip (`Ns_ConnFlush`).
    pub fn flush(self: &Rc<Self>, data: &[u8], stream: bool) -> io::Result<()> {
        let mut buf = self.encode_output(data);
        let gzip_ok = !stream
            && self.conn().flags.gzip
            && self.server.opts.gzip
            && buf.len() > self.server.opts.gzip_min
            && self
                .header("accept-encoding")
                .is_some_and(|a| a.contains("gzip"));
        if gzip_ok {
            if let Ok(z) = self.server.global.gzip(&buf, self.server.opts.gzip_level) {
                buf = z;
                self.cond_set_header("Content-Encoding", "gzip");
            }
        }
        self.flush_direct(&buf, stream)
    }

    /// Send output bypassing encoding and compression
    /// (`Ns_ConnFlushDirect`).
    pub fn flush_direct(self: &Rc<Self>, data: &[u8], stream: bool) -> io::Result<()> {
        if !self.headers_sent() {
            let len = if stream {
                if self.server.opts.chunked && self.conn().request.version > 1.0 {
                    self.conn_mut().flags.chunk = true;
                }
                None
            } else {
                Some(data.len() as u64)
            };
            let ty = self.conn().content_type.clone();
            self.set_required_headers(ty.as_deref(), len);
            if self.conn().flags.chunk {
                self.cond_set_header("Transfer-Encoding", "chunked");
            }
            let status = self.status();
            self.queue_headers(status);
        }
        let (skip, chunk) = {
            let c = self.conn();
            (c.flags.skip_body, c.flags.chunk)
        };
        if skip {
            self.send(&[])?;
        } else if !chunk {
            self.send(&[data])?;
        } else {
            let hdr = format!("{:x}\r\n", data.len());
            let mut parts: Vec<&[u8]> = Vec::new();
            if !data.is_empty() {
                parts.push(hdr.as_bytes());
                parts.push(data);
                parts.push(b"\r\n");
            }
            if !stream {
                parts.push(b"0\r\n\r\n");
            }
            self.send(&parts)?;
        }
        if !stream {
            self.close();
        }
        Ok(())
    }

    /// Release the socket to the driver (keep-alive or close) and run
    /// `ns_atclose` scripts (`Ns_ConnClose`).
    pub fn close(&self) {
        let sock = {
            let mut c = self.conn_mut();
            if c.flags.closed {
                return;
            }
            c.flags.closed = true;
            c.sock.take()
        };
        if let Some(mut s) = sock {
            // Flush any queued-but-unsent headers, bounded by sendwait like
            // every other write (Ns_SockSend); a failed write drops
            // keep-alive.
            let (pending, sendwait) = {
                let mut c = self.conn_mut();
                (std::mem::take(&mut c.obuf), c.driver.sendwait)
            };
            if !pending.is_empty() {
                let rt = self.server.global.runtime.clone();
                if send_bounded(&rt, &mut s, &pending, sendwait).is_err() {
                    self.conn_mut().flags.keepalive = false;
                }
            }
            let keep = self.conn().flags.keepalive;
            s.release(keep);
        }
        let scripts: Vec<String> = std::mem::take(&mut *self.at_close.borrow_mut());
        if !scripts.is_empty() {
            if let Some(i) = self.interp.borrow().as_ref().map(Rc::clone) {
                for s in scripts.iter().rev() {
                    if let Err(e) = i.eval(s) {
                        self.server
                            .global
                            .log(Severity::Error, &format!("atclose: {}", e.detail()));
                    }
                }
            }
        }
    }

    // ---- canned responses (return.c) ----

    /// `Ns_ConnReturnData`: binary data, no encoding.
    pub fn return_data(
        self: &Rc<Self>,
        status: u16,
        data: &[u8],
        ty: Option<&str>,
    ) -> io::Result<()> {
        self.set_status(status);
        self.set_type(ty);
        self.flush_direct(data, false)
    }

    /// `Ns_ConnReturnCharData`: text, encoded with the output charset.
    pub fn return_char_data(
        self: &Rc<Self>,
        status: u16,
        data: &str,
        ty: Option<&str>,
    ) -> io::Result<()> {
        self.set_status(status);
        self.set_type(ty);
        self.flush(data.as_bytes(), false)
    }

    /// `Ns_ConnReturnHtml`.
    pub fn return_html(self: &Rc<Self>, status: u16, html: &str) -> io::Result<()> {
        self.return_char_data(status, html, Some("text/html"))
    }

    /// Internal redirect for a status code configured in
    /// `ns/server/S/redirects` (`ReturnRedirect`).  Returns `None` if no
    /// redirect applies.
    pub fn return_redirect_for(self: &Rc<Self>, status: u16) -> Option<io::Result<()>> {
        let url = self.server.redirect_for(status)?;
        if self.conn().recursion >= MAX_RECURSION {
            self.server.global.log(
                Severity::Error,
                &format!("return: failed to redirect '{status}': exceeded recursion limit of {MAX_RECURSION}"),
            );
            return None;
        }
        Some(self.redirect(&url).map(|_| ()))
    }

    /// `Ns_ConnReturnNotice`: a small HTML page.
    pub fn return_notice(
        self: &Rc<Self>,
        status: u16,
        title: &str,
        notice: &str,
    ) -> io::Result<()> {
        let mut s = format!(
            "<!DOCTYPE HTML PUBLIC \"-//IETF//DTD HTML 2.0//EN\">\n<HTML>\n<HEAD>\n<TITLE>{title}</TITLE>\n</HEAD>\n<BODY>\n<H2>{title}</H2>\n"
        );
        if !notice.is_empty() {
            let _ = writeln!(s, "{notice}");
        }
        if self.server.opts.notice_detail {
            let _ = writeln!(
                s,
                "<P ALIGN=RIGHT><SMALL><I>{}/{} on {}</I></SMALL></P>",
                crate::global::SERVER_NAME,
                crate::global::SERVER_VERSION,
                self.location()
            );
        }
        if status >= 400 {
            while s.len() < self.server.limits.errorminsize {
                s.push_str("                    ");
            }
        }
        s.push_str("\n</BODY></HTML>\n");
        self.return_html(status, &s)
    }

    /// `Ns_ConnReturnAdminNotice` (same as a notice).
    pub fn return_admin_notice(
        self: &Rc<Self>,
        status: u16,
        title: &str,
        notice: &str,
    ) -> io::Result<()> {
        self.return_notice(status, title, notice)
    }

    /// `Ns_ConnReturnRedirect`: 302 to `url` (relative URLs get the
    /// connection location prepended).
    pub fn return_redirect(self: &Rc<Self>, url: Option<&str>) -> io::Result<()> {
        match url {
            Some(u) => {
                let loc = if u.starts_with('/') {
                    format!("{}{u}", self.location())
                } else {
                    u.to_owned()
                };
                self.update_header("Location", &loc);
                self.return_notice(
                    302,
                    "Redirection",
                    &format!("<A HREF=\"{loc}\">The requested URL has moved here.</A>"),
                )
            }
            None => self.return_notice(204, "No Content", ""),
        }
    }

    /// `ns_returnmoved`: 301 redirect.
    pub fn return_moved(self: &Rc<Self>, url: &str) -> io::Result<()> {
        let loc = if url.starts_with('/') {
            format!("{}{url}", self.location())
        } else {
            url.to_owned()
        };
        self.update_header("Location", &loc);
        self.return_notice(
            301,
            "Moved",
            &format!("<A HREF=\"{loc}\">The requested URL has moved here.</A>"),
        )
    }

    /// `Ns_ConnReturnStatus`: headers only.
    pub fn return_status(self: &Rc<Self>, status: u16) -> io::Result<()> {
        if let Some(r) = self.return_redirect_for(status) {
            return r;
        }
        self.set_required_headers(None, Some(0));
        self.queue_headers(status);
        let r = self.send(&[]).map(|_| ());
        self.close();
        r
    }

    /// `Ns_ConnReturnOk`.
    pub fn return_ok(self: &Rc<Self>) -> io::Result<()> {
        self.return_status(200)
    }

    /// `Ns_ConnReturnNoResponse` / 204.
    pub fn return_no_response(self: &Rc<Self>) -> io::Result<()> {
        self.return_status(204)
    }

    /// `Ns_ConnReturnNotModified`.
    pub fn return_not_modified(self: &Rc<Self>) -> io::Result<()> {
        self.return_status(304)
    }

    fn canned(self: &Rc<Self>, status: u16, title: &str, msg: &str) -> io::Result<()> {
        if let Some(r) = self.return_redirect_for(status) {
            return r;
        }
        self.return_notice(status, title, msg)
    }

    /// `Ns_ConnReturnNotFound`.
    pub fn return_not_found(self: &Rc<Self>) -> io::Result<()> {
        self.canned(
            404,
            "Not Found",
            "The requested URL was not found on this server.",
        )
    }

    /// `Ns_ConnReturnBadRequest`.
    pub fn return_bad_request(self: &Rc<Self>, reason: Option<&str>) -> io::Result<()> {
        let mut msg = String::from("The HTTP request presented by your browser is invalid.");
        if let Some(r) = reason {
            let _ = write!(msg, "<P>\n{r}");
        }
        self.canned(400, "Invalid Request", &msg)
    }

    /// `Ns_ConnReturnUnauthorized`.
    pub fn return_unauthorized(self: &Rc<Self>) -> io::Result<()> {
        self.update_header(
            "WWW-Authenticate",
            &format!("Basic realm=\"{}\"", self.server.opts.realm),
        );
        self.canned(
            401,
            "Access Denied",
            "The requested URL cannot be accessed because a valid username and password are required.",
        )
    }

    /// `Ns_ConnReturnForbidden`.
    pub fn return_forbidden(self: &Rc<Self>) -> io::Result<()> {
        self.canned(
            403,
            "Forbidden",
            "The requested URL cannot be accessed by this server.",
        )
    }

    /// `Ns_ConnReturnInternalError`.
    pub fn return_internal_error(self: &Rc<Self>) -> io::Result<()> {
        self.update_header("Connection", "close");
        self.conn_mut().flags.keepalive = false;
        self.canned(
            500,
            "Server Error",
            "The requested URL cannot be accessed due to a system error on this server.",
        )
    }

    /// `Ns_ConnReturnNotImplemented`.
    pub fn return_not_implemented(self: &Rc<Self>) -> io::Result<()> {
        self.canned(
            501,
            "Not Implemented",
            "The requested URL or method is not implemented by this server.",
        )
    }

    /// `Ns_ConnReturnServiceUnavailable`.
    pub fn return_unavailable(self: &Rc<Self>) -> io::Result<()> {
        self.canned(
            503,
            "Service Unavailable",
            "The server is temporarily unable to service your request. Please try again later.",
        )
    }

    /// `Ns_ConnReturnEntityTooLarge`.
    pub fn return_too_large(self: &Rc<Self>) -> io::Result<()> {
        self.canned(
            413,
            "Request Entity Too Large",
            "The request content is larger than the server allows.",
        )
    }

    /// Stream `len` bytes from `reader` (`ReturnOpen`).
    pub fn return_reader(
        self: &Rc<Self>,
        status: u16,
        ty: Option<&str>,
        reader: &mut dyn Read,
        len: Option<u64>,
    ) -> io::Result<()> {
        self.set_type(ty);
        let cur_ty = self.conn().content_type.clone();
        self.set_required_headers(cur_ty.as_deref(), len);
        self.queue_headers(status);
        let mut remaining = len;
        let mut buf = vec![0u8; 64 * 1024];
        let mut sent_any = false;
        loop {
            let want = remaining.map_or(buf.len(), |r| {
                buf.len().min(usize::try_from(r).unwrap_or(usize::MAX))
            });
            if want == 0 {
                break;
            }
            let n = reader.read(&mut buf[..want])?;
            if n == 0 {
                break;
            }
            if self.conn().flags.skip_body {
                break;
            }
            self.send(&[&buf[..n]])?;
            sent_any = true;
            if let Some(r) = remaining.as_mut() {
                *r -= n as u64;
            }
        }
        if !sent_any {
            self.send(&[])?;
        }
        self.close();
        Ok(())
    }

    /// Return a file, honouring `If-Modified-Since` (`Ns_ConnReturnFile`).
    pub fn return_file(
        self: &Rc<Self>,
        status: u16,
        ty: Option<&str>,
        path: &Path,
    ) -> io::Result<()> {
        let meta = match std::fs::metadata(path) {
            Ok(m) if m.is_file() => m,
            _ => return self.return_not_found(),
        };
        let mtime = meta.modified().map(unix_time).unwrap_or_default();
        if status == 200 && self.server.opts.modsince {
            if let Some(ims) = self
                .header("if-modified-since")
                .and_then(|h| parse_http_time(&h))
            {
                if mtime <= ims {
                    self.update_header("Last-Modified", &http_time(mtime));
                    return self.return_not_modified();
                }
            }
        }
        self.update_header("Last-Modified", &http_time(mtime));
        let guessed;
        let ty = if let Some(t) = ty {
            t
        } else {
            guessed = self
                .server
                .global
                .mime
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .guess(&path.to_string_lossy())
                .to_owned();
            &guessed
        };
        let mut f = std::fs::File::open(path)?;
        self.return_reader(status, Some(ty), &mut f, Some(meta.len()))
    }

    /// Internal redirect to another URL (`Ns_ConnRedirect`).
    pub fn redirect(self: &Rc<Self>, url: &str) -> io::Result<Status> {
        {
            let mut c = self.conn_mut();
            c.recursion += 1;
            let enc = c.url_encoding;
            c.request.set_url(url, enc);
            c.form = None;
        }
        let status = crate::op::authorize(self);
        match status {
            Status::Ok => Ok(crate::op::run_request(self)),
            Status::Forbidden => self.return_forbidden().map(|()| Status::Ok),
            Status::Unauthorized => self.return_unauthorized().map(|()| Status::Ok),
            _ => self.return_internal_error().map(|()| Status::Ok),
        }
    }

    /// Reset the pending response so an error page can be sent
    /// (`Ns_ConnResetReturn`).  Fails if headers were already sent.
    pub fn reset_return(&self) -> bool {
        if self.headers_sent() {
            return false;
        }
        let c = self.conn();
        lock_set(&c.output_headers).trunc(0);
        true
    }

    /// The parsed form (`Ns_ConnGetQuery`), parsed on first use.
    pub fn form(&self) -> Form {
        if let Some(f) = &self.conn().form {
            return f.clone();
        }
        let form = {
            let c = self.conn();
            let ct = lock_set(&c.headers).iget("content-type").map(str::to_owned);
            let enc = c.query_encoding.or(c.url_encoding);
            // As `Ns_ConnGetQuery`: content that cannot be mapped parses as
            // an empty form.
            ns_core::form::parse_form(
                &c.request.method,
                c.request.query.as_deref(),
                ct.as_deref(),
                c.content.bytes().unwrap_or_default(),
                enc,
            )
        };
        self.conn_mut().form = Some(form.clone());
        form
    }

    /// The descriptor of a temp file holding the request body
    /// (`Ns_ConnContentFd`), copying an in-memory body there first.  The
    /// file belongs to the connection.  Failures are logged.
    pub fn content_fd(&self) -> io::Result<std::os::unix::io::RawFd> {
        let r = self.conn_mut().content.fd();
        self.log_content_error(r)
    }

    /// A `dup` of [`ConnRef::content_fd`]'s descriptor, sharing its offset
    /// (for a CGI's standard input).  Failures are logged.
    pub fn content_file(&self) -> io::Result<std::fs::File> {
        let r = self.conn_mut().content.dup_fd();
        self.log_content_error(r)
    }

    fn log_content_error<T>(&self, r: io::Result<T>) -> io::Result<T> {
        if let Err(e) = &r {
            self.server.global.log(
                Severity::Error,
                &format!("conn[{}]: could not get fd: {e}", self.id()),
            );
        }
        r
    }

    /// Discard the cached form (`ns_conn clearquery`/encoding changes).
    pub fn clear_form(&self) {
        self.conn_mut().form = None;
    }
}

/// Write and flush `data` to the client within `sendwait` (`Ns_SockSend`
/// with the driver's send timeout).
fn send_bounded(
    runtime: &tokio::runtime::Handle,
    sock: &mut Sock,
    data: &[u8],
    sendwait: Duration,
) -> io::Result<()> {
    runtime.block_on(async {
        match tokio::time::timeout(sendwait, async {
            sock.stream.write_all(data).await?;
            sock.stream.flush().await
        })
        .await
        {
            Ok(r) => r,
            Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "send timeout")),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons() {
        assert_eq!(reason(200), "OK");
        assert_eq!(reason(404), "Not Found");
        assert_eq!(reason(999), "Unknown Reason");
    }

    #[test]
    fn header_eq() {
        let mut s = NsSet::default();
        s.put_str("Connection", " Keep-Alive ");
        assert!(hdr_eq(&s, "connection", "keep-alive"));
        assert!(!hdr_eq(&s, "connection", "close"));
    }
}
