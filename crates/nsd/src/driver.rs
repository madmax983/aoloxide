//! The socket driver (`nsd/driver.c`, the `nssock` module).
//!
//! Each driver listens on one address.  For every accepted socket an async
//! task reads the request ahead (request line and headers within the
//! `maxline` and `maxinput` limits, then the body), then hands it to the
//! virtual server's connection queue and waits for the connection thread to give
//! the socket back.  If the response allowed keep-alive, the task waits up
//! to `keepwait` for the next request; otherwise it closes gracefully,
//! draining input for up to `closewait`.  See
//! `docs/adr/0003-threading-model.md`.
//!
//! As in `SockReadLine`, a body longer than the request's `ns_limits`
//! `maxupload` is refused (413) before it is read, and a request line and
//! headers that do not end within `maxinput` are refused (400, AOLserver's
//! `E_RRANGE`).  A body (empty without a `Content-Length`) whose request
//! (line, headers, body and a trailing CRLF) reaches `maxinput` is spooled
//! to a temp file ([`crate::content::get_temp`]) as it arrives instead of
//! being buffered, with the file I/O on tokio's blocking pool; the
//! accounting is modelled in `verus/spool.rs`.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant, SystemTime};

use ns_core::log::Severity;
use ns_core::request::{parse_header, HeaderCase, Request};
use ns_core::{Config, NsSet};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch, Semaphore};

use crate::content::{get_temp, Content, TempFile};
use crate::global::{Global, NsError};
use crate::pool::Limits;
use crate::server::Server;

/// A client byte stream (plain TCP or TLS).
pub trait ConnStream: AsyncRead + AsyncWrite + Unpin + Send + 'static {
    /// The plain TCP socket, if this stream is one (`ns_conn sock`).
    fn tcp(&self) -> Option<&TcpStream>;
    /// Unwrap the plain TCP socket, or give the stream back
    /// (`ns_conn channel`).
    fn into_tcp(self: Box<Self>) -> Result<TcpStream, Box<dyn ConnStream>>;
}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> ConnStream for T {
    fn tcp(&self) -> Option<&TcpStream> {
        (self as &dyn std::any::Any).downcast_ref()
    }

    fn into_tcp(self: Box<Self>) -> Result<TcpStream, Box<dyn ConnStream>> {
        if (&*self as &dyn std::any::Any).is::<TcpStream>() {
            let any: Box<dyn std::any::Any> = self;
            Ok(*any
                .downcast::<TcpStream>()
                .unwrap_or_else(|_| unreachable!("checked above")))
        } else {
            Err(self)
        }
    }
}

/// Boxed future returned by [`StreamWrapper::wrap`].
pub type WrapFuture = Pin<Box<dyn Future<Output = io::Result<Box<dyn ConnStream>>> + Send>>;

/// Turns an accepted TCP stream into a [`ConnStream`] (e.g. a TLS
/// handshake).  The plain driver uses the identity wrapper.
pub trait StreamWrapper: Send + Sync {
    /// Wrap `tcp`.
    fn wrap(&self, tcp: TcpStream) -> WrapFuture;
}

/// The identity wrapper for plain HTTP.
#[derive(Debug, Default)]
pub struct PlainWrapper;

impl StreamWrapper for PlainWrapper {
    fn wrap(&self, tcp: TcpStream) -> WrapFuture {
        Box::pin(async move { Ok(Box::new(tcp) as Box<dyn ConnStream>) })
    }
}

/// Driver statistics (`ns_driver query`).
#[derive(Debug, Default)]
pub struct DriverStats {
    /// Sockets accepted.
    pub accepts: AtomicU64,
    /// Requests read.
    pub reads: AtomicU64,
    /// Requests queued.
    pub queued: AtomicU64,
    /// Read timeouts.
    pub timeouts: AtomicU64,
    /// Requests rejected for size.
    pub overflow: AtomicU64,
    /// Sockets dropped on error.
    pub dropped: AtomicU64,
}

/// Configuration and identity of a driver (`struct Driver`).
#[derive(Debug)]
pub struct DriverInfo {
    /// Driver name, e.g. `nssock`.
    pub name: String,
    /// Module name as configured.
    pub module: String,
    /// Owning virtual server (`None` for global, virtual-host drivers).
    pub server: Option<String>,
    /// `http` or `https`.
    pub protocol: &'static str,
    /// Host name used in the location.
    pub host: String,
    /// Address to bind.
    pub bind_address: String,
    port: AtomicU16,
    configured_port: u16,
    location_override: Option<String>,
    /// Send timeout.
    pub sendwait: Duration,
    /// Receive timeout.
    pub recvwait: Duration,
    /// Keep-alive idle timeout (zero disables keep-alive).
    pub keepwait: Duration,
    /// Graceful-close drain time.
    pub closewait: Duration,
    /// Max request/header line length.
    pub maxline: usize,
    /// The `maxheader` setting.  AOLserver 4.5 reads it but never uses it,
    /// and neither does aoloxide: [`DriverInfo::maxinput`] bounds the head.
    pub maxheader: usize,
    /// Max bytes of a request held in memory (`drvPtr->maxinput`): the
    /// request line and headers must end within it (else 400), and a body
    /// whose request (line, headers, body and a trailing CRLF) would reach
    /// it is spooled to a temp file instead.
    pub maxinput: usize,
    /// Max open sockets.
    pub maxsock: usize,
    /// Read buffer size.
    pub bufsize: usize,
    /// Whether the location is derived from the Host header.
    pub virtual_hosts: bool,
    /// Live statistics.
    pub stats: DriverStats,
}

impl DriverInfo {
    /// Read driver settings from `section` (`Ns_DriverInit`).
    pub fn from_config(
        config: &Config,
        section: &str,
        name: &str,
        module: &str,
        server: Option<&str>,
        ssl: bool,
        default_host: &str,
    ) -> Self {
        let int = |k: &str, d: i64| config.int_or(section, k, d);
        let secs = |k: &str, d: i64| Duration::from_secs(int(k, d).max(0) as u64);
        let socktimeout = int("socktimeout", 30).max(1);
        let (protocol, defport) = if ssl { ("https", 443) } else { ("http", 80) };
        let address = config.get(section, "address").map(str::to_owned);
        let host = config
            .get(section, "hostname")
            .map(str::to_owned)
            .or_else(|| address.clone())
            .unwrap_or_else(|| default_host.to_owned());
        let port = int("port", defport).clamp(0, i64::from(u16::MAX)) as u16;
        Self {
            name: name.to_owned(),
            module: module.to_owned(),
            server: server.map(str::to_owned),
            protocol,
            host,
            bind_address: address.unwrap_or_else(|| "0.0.0.0".to_owned()),
            port: AtomicU16::new(port),
            configured_port: port,
            location_override: config.get(section, "location").map(str::to_owned),
            sendwait: Duration::from_secs(int("sendwait", socktimeout).max(1) as u64),
            recvwait: Duration::from_secs(int("recvwait", socktimeout).max(1) as u64),
            keepwait: secs("keepwait", 30),
            closewait: secs("closewait", 2),
            maxline: int("maxline", 4 * 1024).max(256) as usize,
            maxheader: int("maxheader", 32 * 1024).max(1024) as usize,
            maxinput: int("maxinput", 1000 * 1024).max(2024) as usize,
            maxsock: int("maxsock", 100).max(1) as usize,
            bufsize: int("bufsize", 16_000).max(1024) as usize,
            virtual_hosts: server.is_none(),
            stats: DriverStats::default(),
        }
    }

    /// The bound port (resolved after bind when configured as 0).
    pub fn port(&self) -> u16 {
        self.port.load(Ordering::Relaxed)
    }

    /// `proto://host[:port]` (`Ns_ConnLocation` default).
    pub fn location(&self) -> String {
        if let Some(l) = &self.location_override {
            return l.clone();
        }
        let port = self.port();
        let def = if self.protocol == "https" { 443 } else { 80 };
        if port == def {
            format!("{}://{}", self.protocol, self.host)
        } else {
            format!("{}://{}:{}", self.protocol, self.host, port)
        }
    }

    /// Port as configured (before resolving 0).
    pub fn configured_port(&self) -> u16 {
        self.configured_port
    }
}

/// What a connection thread hands back to the driver.
pub struct SockReturn {
    /// The stream.
    pub stream: Box<dyn ConnStream>,
    /// Bytes read past the end of the request (pipelining).
    pub leftover: Vec<u8>,
    /// Keep the connection alive.
    pub keep: bool,
}

/// A client socket owned by a connection while it runs (`struct Sock`).
pub struct Sock {
    /// The stream.
    pub stream: Box<dyn ConnStream>,
    /// Pipelined bytes after the current request.
    pub leftover: Vec<u8>,
    /// The client's TCP descriptor (for polling while the stream is
    /// owned by the connection, as `ns_quewait` does).
    pub fd: Option<std::os::fd::RawFd>,
    reply: Option<oneshot::Sender<SockReturn>>,
}

impl std::fmt::Debug for Sock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sock").finish_non_exhaustive()
    }
}

impl Sock {
    /// Give the socket back to its driver (`NsSockClose`).
    pub fn release(mut self, keep: bool) {
        if let Some(tx) = self.reply.take() {
            let _ = tx.send(SockReturn {
                stream: self.stream,
                leftover: std::mem::take(&mut self.leftover),
                keep,
            });
        }
    }
}

/// A fully read request handed to a server's queue.
pub struct Incoming {
    /// The socket.
    pub sock: Sock,
    /// Parsed request line.
    pub request: Request,
    /// Request headers (case preserved).
    pub headers: NsSet,
    /// Request body, in memory or spooled.
    pub content: Content,
    /// Client address.
    pub peer: SocketAddr,
    /// Accepting driver.
    pub driver: Arc<DriverInfo>,
    /// When the socket was accepted.
    pub accept_time: SystemTime,
    /// When the request finished reading.
    pub read_time: Instant,
}

/// A running driver.
pub struct Driver {
    /// Configuration.
    pub info: Arc<DriverInfo>,
    servers: HashMap<String, Arc<Server>>,
    default_server: Arc<Server>,
    wrapper: Arc<dyn StreamWrapper>,
    listener: Mutex<Option<std::net::TcpListener>>,
    stop: watch::Sender<bool>,
    active: Arc<Semaphore>,
    global: Arc<Global>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Driver")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
enum ReadError {
    Eof,
    Timeout,
    Io,
    Status(u16),
    /// The body could not be spooled (`E_FDAGAIN`, `E_FDWRITE`, ...).
    Spool(io::Error),
}

impl From<io::Error> for ReadError {
    fn from(e: io::Error) -> Self {
        Self::Spool(e)
    }
}

/// Run temp-file I/O on tokio's blocking pool, so a slow disk does not
/// stall the other driver tasks on this worker thread.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(io::Error::other)?
}

/// A request body as it is read ahead: buffered in memory or spooled to a
/// temp file (`connPtr->content` / `connPtr->tfd`).  `mem.len() + disk`
/// is the bytes read so far (`connPtr->avail`); see `verus/spool.rs`.
/// File I/O runs on the blocking pool ([`blocking`]).
struct Body {
    mem: Vec<u8>,
    file: Option<TempFile>,
    disk: usize,
}

impl Drop for Body {
    /// An abandoned spool file is released (rewound and truncated) on the
    /// blocking pool too.
    fn drop(&mut self) {
        if let Some(f) = self.file.take() {
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                drop(rt.spawn_blocking(move || drop(f)));
            }
        }
    }
}

impl Body {
    /// An in-memory body expecting about `capacity` bytes.
    fn memory(capacity: usize) -> Self {
        Self {
            mem: Vec::with_capacity(capacity),
            file: None,
            disk: 0,
        }
    }

    /// A body spooled to a new temp file.
    async fn spooled() -> io::Result<Self> {
        Ok(Self {
            mem: Vec::new(),
            file: Some(blocking(get_temp).await?),
            disk: 0,
        })
    }

    /// Bytes read so far.
    fn read(&self) -> usize {
        self.mem.len() + self.disk
    }

    const fn spooling(&self) -> bool {
        self.file.is_some()
    }

    /// Store body bytes in the chosen sink, as `SockReadContent` writes
    /// each read to the temp file.
    async fn push(&mut self, data: &[u8]) -> io::Result<()> {
        match self.file.take() {
            Some(mut f) => {
                let data = data.to_vec();
                let n = data.len();
                self.file = Some(
                    blocking(move || {
                        std::io::Write::write_all(f.file_mut(), &data)?;
                        Ok(f)
                    })
                    .await?,
                );
                self.disk += n;
            }
            None => self.mem.extend_from_slice(data),
        }
        Ok(())
    }

    /// Move a growing in-memory body to a temp file (chunked bodies).
    async fn spill(&mut self) -> io::Result<()> {
        let mem = std::mem::take(&mut self.mem);
        let n = mem.len();
        let t = blocking(move || {
            let mut t = get_temp()?;
            std::io::Write::write_all(t.file_mut(), &mem)?;
            Ok(t)
        })
        .await?;
        self.disk = n;
        self.file = Some(t);
        Ok(())
    }

    /// The finished body, a spooled one truncated and rewound for reading.
    async fn finish(mut self) -> io::Result<Content> {
        match self.file.take() {
            Some(mut f) => {
                let len = self.disk;
                blocking(move || {
                    f.file_mut().set_len(len as u64)?;
                    std::io::Seek::seek(f.file_mut(), std::io::SeekFrom::Start(0))?;
                    Ok(Content::spooled(f, len))
                })
                .await
            }
            None => Ok(Content::memory(std::mem::take(&mut self.mem))),
        }
    }
}

/// Move up to `need` bytes from the front of `buf` into `body`.  Returns
/// the bytes taken (`take` in `verus/spool.rs`).
async fn take_body(buf: &mut Vec<u8>, body: &mut Body, need: usize) -> io::Result<usize> {
    let n = need.min(buf.len());
    if n > 0 {
        body.push(&buf[..n]).await?;
        buf.drain(..n);
    }
    Ok(n)
}

/// Whether a body of `len` bytes after `head` request bytes is spooled:
/// `roff + contentLength + 2 >= maxinput` (room for a trailing CRLF).
const fn spools(head: usize, len: usize, maxinput: usize) -> bool {
    head.saturating_add(len).saturating_add(2) >= maxinput
}

fn find_header_end(buf: &[u8]) -> Option<(usize, usize)> {
    let crlf = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, i + 4));
    let lf = buf
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|i| (i, i + 2));
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

async fn read_more(
    stream: &mut Box<dyn ConnStream>,
    buf: &mut Vec<u8>,
    wait: Duration,
    chunk: usize,
) -> Result<(), ReadError> {
    let mut tmp = vec![0u8; chunk];
    match tokio::time::timeout(wait, stream.read(&mut tmp)).await {
        Err(_) => Err(ReadError::Timeout),
        Ok(Err(_)) => Err(ReadError::Io),
        Ok(Ok(0)) => Err(ReadError::Eof),
        Ok(Ok(n)) => {
            buf.extend_from_slice(&tmp[..n]);
            Ok(())
        }
    }
}

struct ReadRequest {
    request: Request,
    headers: NsSet,
    content: Content,
}

/// Read one request.  `limits` gives the request's `ns_limits`
/// (`NsGetRequestLimits`) once its line and headers are read; a body over
/// their `maxupload` is refused and counted as dropped.
async fn read_request(
    stream: &mut Box<dyn ConnStream>,
    buf: &mut Vec<u8>,
    info: &DriverInfo,
    first_wait: Duration,
    limits: &(dyn Fn(&Request, &NsSet) -> Arc<Limits> + Send + Sync),
) -> Result<ReadRequest, ReadError> {
    let mut wait = first_wait;
    // Skip leading blank lines between pipelined requests.
    let (hdr_end, body_start) = loop {
        while buf.first().is_some_and(|b| *b == b'\r' || *b == b'\n') {
            buf.remove(0);
        }
        if let Some(p) = find_header_end(buf) {
            break p;
        }
        // An HTTP/0.9 request has a single line and no headers.
        if let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&buf[..nl]);
            if !line.trim().is_empty() && ns_core::request::find_version(line.trim()).is_none() {
                break (nl, nl + 1);
            }
            if nl > info.maxline {
                return Err(ReadError::Status(414));
            }
        } else if buf.len() > info.maxline {
            return Err(ReadError::Status(414));
        }
        // `E_RRANGE`: the request line and headers must end within the
        // first `maxinput` bytes (`maxheader` is read but unused, as in
        // AOLserver).
        if buf.len() >= info.maxinput {
            return Err(ReadError::Status(400));
        }
        read_more(stream, buf, wait, info.bufsize).await?;
        wait = info.recvwait;
    };
    let head = String::from_utf8_lossy(&buf[..hdr_end]).into_owned();
    let mut lines = head.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));
    let line = lines.next().unwrap_or("");
    if line.len() > info.maxline {
        return Err(ReadError::Status(414));
    }
    let request = Request::parse(line, None).ok_or(ReadError::Status(400))?;
    let mut headers = NsSet::named("headers");
    if request.major > 0 {
        for l in lines {
            if l.len() > info.maxline {
                return Err(ReadError::Status(400));
            }
            if l.is_empty() {
                break;
            }
            parse_header(&mut headers, l, HeaderCase::Preserve)
                .map_err(|_| ReadError::Status(400))?;
        }
    }
    // A head that ends past `maxinput` was read in one large read here;
    // `SockReadLine` never reads past `maxinput`.
    if body_start > info.maxinput {
        return Err(ReadError::Status(400));
    }
    buf.drain(..body_start);
    let chunked = headers
        .iget("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let length = match headers.iget("content-length") {
        Some(v) => Some(
            v.trim()
                .parse::<usize>()
                .map_err(|_| ReadError::Status(400))?,
        ),
        None => None,
    };
    let expects_continue = headers
        .iget("expect")
        .is_some_and(|v| v.eq_ignore_ascii_case("100-continue"));
    let limits = limits(&request, &headers);
    let maxupload = limits.config().maxupload;
    let too_large = || {
        limits.drop_upload();
        ReadError::Status(413)
    };
    // As `SockReadLine`, a request without a `Content-Length` has an empty
    // body that still spools when its head alone reaches `maxinput`.
    let len = length.filter(|_| !chunked).unwrap_or(0);
    if len > maxupload {
        return Err(too_large());
    }
    let mut body = if spools(body_start, len, info.maxinput) {
        Body::spooled().await?
    } else {
        Body::memory(len)
    };
    if chunked {
        if expects_continue {
            let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await;
        }
        loop {
            let nl = loop {
                if let Some(p) = buf.windows(2).position(|w| w == b"\r\n") {
                    break p;
                }
                read_more(stream, buf, info.recvwait, info.bufsize).await?;
            };
            let size_line = String::from_utf8_lossy(&buf[..nl]).into_owned();
            let size_hex = size_line.split(';').next().unwrap_or("").trim();
            let size = usize::from_str_radix(size_hex, 16).map_err(|_| ReadError::Status(400))?;
            buf.drain(..nl + 2);
            if size == 0 {
                // Discard trailers up to the blank line.
                loop {
                    if let Some(p) = buf.windows(2).position(|w| w == b"\r\n") {
                        let blank = p == 0;
                        buf.drain(..p + 2);
                        if blank {
                            break;
                        }
                    } else {
                        read_more(stream, buf, info.recvwait, info.bufsize).await?;
                    }
                }
                break;
            }
            if body.read().saturating_add(size) > maxupload {
                return Err(too_large());
            }
            if !body.spooling() && spools(body_start, body.read() + size, info.maxinput) {
                body.spill().await?;
            }
            let mut need = size;
            loop {
                need -= take_body(buf, &mut body, need).await?;
                if need == 0 {
                    break;
                }
                read_more(stream, buf, info.recvwait, info.bufsize).await?;
            }
            while buf.len() < 2 {
                read_more(stream, buf, info.recvwait, info.bufsize).await?;
            }
            buf.drain(..2);
        }
    } else {
        if expects_continue && buf.len() < len {
            let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await;
        }
        loop {
            let owed = len - body.read();
            take_body(buf, &mut body, owed).await?;
            if body.read() == len {
                break;
            }
            read_more(stream, buf, info.recvwait, info.bufsize).await?;
        }
    }
    Ok(ReadRequest {
        request,
        headers,
        content: body.finish().await?,
    })
}

async fn write_error(stream: &mut Box<dyn ConnStream>, status: u16) {
    let body = format!(
        "<HTML><HEAD><TITLE>{status} {r}</TITLE></HEAD><BODY><H2>{r}</H2></BODY></HTML>\n",
        r = crate::conn::reason(status)
    );
    let msg = format!(
        "HTTP/1.0 {status} {}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        crate::conn::reason(status),
        body.len()
    );
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.write_all(msg.as_bytes())).await;
}

/// Close `stream`: flush and shut down the write side (bounded by
/// `sendwait`, as `DriverClose`/`NsSSLFlush` are bounded by the socket send
/// timeout), then drain input for up to `closewait`.  A peer that stops
/// reading cannot hold the task (and its `maxsock` permit) forever.
async fn graceful_close(mut stream: Box<dyn ConnStream>, sendwait: Duration, closewait: Duration) {
    if tokio::time::timeout(sendwait, stream.shutdown())
        .await
        .is_err()
    {
        return;
    }
    if closewait.is_zero() {
        return;
    }
    let deadline = tokio::time::Instant::now() + closewait;
    let mut sink = [0u8; 1024];
    loop {
        match tokio::time::timeout_at(deadline, stream.read(&mut sink)).await {
            Ok(Ok(n)) if n > 0 => {}
            _ => break,
        }
    }
}

impl Driver {
    /// Create and bind a driver.  `servers` maps Host header values to
    /// servers for virtual hosting; `default_server` handles the rest.
    pub fn bind(
        global: &Arc<Global>,
        info: DriverInfo,
        servers: HashMap<String, Arc<Server>>,
        default_server: Arc<Server>,
        wrapper: Arc<dyn StreamWrapper>,
    ) -> Result<Arc<Self>, NsError> {
        let addr = format!("{}:{}", info.bind_address, info.configured_port);
        let listener = std::net::TcpListener::bind(&addr)
            .map_err(|e| NsError::Config(format!("{}: could not bind {addr}: {e}", info.module)))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        info.port.store(port, Ordering::Relaxed);
        let maxsock = info.maxsock;
        let (stop, _) = watch::channel(false);
        global.log(
            Severity::Notice,
            &format!(
                "{}: listening on {}:{port} ({})",
                info.module,
                info.bind_address,
                info.location()
            ),
        );
        Ok(Arc::new(Self {
            info: Arc::new(info),
            servers,
            default_server,
            wrapper,
            listener: Mutex::new(Some(listener)),
            stop,
            active: Arc::new(Semaphore::new(maxsock)),
            global: Arc::clone(global),
            task: Mutex::new(None),
        }))
    }

    /// The server a request belongs to (virtual hosting by `Host`).
    fn select_server(&self, headers: &NsSet) -> Arc<Server> {
        if self.servers.is_empty() {
            return Arc::clone(&self.default_server);
        }
        headers
            .iget("host")
            .and_then(|h| {
                let h = h.trim().to_ascii_lowercase();
                self.servers
                    .get(&h)
                    .or_else(|| self.servers.get(h.split(':').next().unwrap_or(&h)))
            })
            .cloned()
            .unwrap_or_else(|| Arc::clone(&self.default_server))
    }

    /// Start accepting (`NsStartDrivers`).
    pub fn start(self: &Arc<Self>) -> Result<(), NsError> {
        let std_listener = self
            .listener
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .ok_or_else(|| NsError::Config("driver already started".into()))?;
        let me = Arc::clone(self);
        let handle = self.global.runtime.spawn(async move {
            let listener = match TcpListener::from_std(std_listener) {
                Ok(l) => l,
                Err(e) => {
                    me.global.log(Severity::Error, &format!("{}: listener: {e}", me.info.module));
                    return;
                }
            };
            let mut stop = me.stop.subscribe();
            loop {
                let permit = tokio::select! {
                    p = Arc::clone(&me.active).acquire_owned() => match p { Ok(p) => p, Err(_) => break },
                    _ = stop.changed() => break,
                };
                let accepted = tokio::select! {
                    a = listener.accept() => a,
                    _ = stop.changed() => break,
                };
                match accepted {
                    Ok((tcp, peer)) => {
                        me.info.stats.accepts.fetch_add(1, Ordering::Relaxed);
                        let _ = tcp.set_nodelay(true);
                        let me2 = Arc::clone(&me);
                        tokio::spawn(async move {
                            me2.handle(tcp, peer).await;
                            drop(permit);
                        });
                    }
                    Err(e) => {
                        me.global.log(Severity::Warning, &format!("{}: accept: {e}", me.info.module));
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        });
        *self.task.lock().unwrap_or_else(PoisonError::into_inner) = Some(handle);
        Ok(())
    }

    async fn handle(self: Arc<Self>, tcp: TcpStream, peer: SocketAddr) {
        let accept_time = SystemTime::now();
        let fd = std::os::fd::AsRawFd::as_raw_fd(&tcp);
        let Ok(Ok(mut stream)) =
            tokio::time::timeout(self.info.recvwait, self.wrapper.wrap(tcp)).await
        else {
            self.info.stats.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let mut buf: Vec<u8> = Vec::new();
        let mut first = true;
        let mut stop = self.stop.subscribe();
        loop {
            let wait = if first {
                self.info.recvwait
            } else {
                self.info.keepwait
            };
            first = false;
            let limits = |req: &Request, headers: &NsSet| {
                self.select_server(headers)
                    .limits_for(&req.method, &req.url)
            };
            let read = tokio::select! {
                r = read_request(&mut stream, &mut buf, &self.info, wait, &limits) => r,
                _ = stop.changed() => Err(ReadError::Eof),
            };
            let req = match read {
                Ok(r) => r,
                Err(ReadError::Status(code)) => {
                    if code == 413 {
                        self.info.stats.overflow.fetch_add(1, Ordering::Relaxed);
                    }
                    write_error(&mut stream, code).await;
                    graceful_close(stream, self.info.sendwait, self.info.closewait).await;
                    return;
                }
                Err(ReadError::Timeout) => {
                    self.info.stats.timeouts.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                Err(ReadError::Spool(e)) => {
                    self.global.log(
                        Severity::Error,
                        &format!("{}: could not spool request content: {e}", self.info.module),
                    );
                    return;
                }
                Err(ReadError::Eof | ReadError::Io) => return,
            };
            self.info.stats.reads.fetch_add(1, Ordering::Relaxed);
            let server = self.select_server(&req.headers);
            let (tx, rx) = oneshot::channel();
            let incoming = Incoming {
                sock: Sock {
                    stream,
                    leftover: std::mem::take(&mut buf),
                    fd: Some(fd),
                    reply: Some(tx),
                },
                request: req.request,
                headers: req.headers,
                content: req.content,
                peer,
                driver: Arc::clone(&self.info),
                accept_time,
                read_time: Instant::now(),
            };
            self.info.stats.queued.fetch_add(1, Ordering::Relaxed);
            if let Err(mut inc) = crate::queue::queue_conn(&server, incoming) {
                write_error(&mut inc.sock.stream, 503).await;
                return;
            }
            let Ok(ret) = rx.await else {
                return;
            };
            stream = ret.stream;
            buf = ret.leftover;
            if !ret.keep || *stop.borrow() {
                graceful_close(stream, self.info.sendwait, self.info.closewait).await;
                return;
            }
        }
    }

    /// Stop accepting new connections (`NsStopDrivers`).
    pub fn stop(&self) {
        let _ = self.stop.send(true);
        self.listener
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }

    /// Wait for the accept loop to finish.
    pub fn wait(&self, timeout: Duration) {
        let task = self
            .task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(t) = task {
            let _ = self
                .global
                .runtime
                .block_on(async { tokio::time::timeout(timeout, t).await });
        }
    }
}

/// A factory that creates [`StreamWrapper`]s for TLS drivers; registered by
/// the nsssl module.
pub type WrapperFactory =
    Arc<dyn Fn(&Config, &str) -> Result<Arc<dyn StreamWrapper>, NsError> + Send + Sync>;

/// Global registry of stream-wrapper factories by module name.
#[derive(Default)]
pub struct WrapperRegistry {
    map: RwLock<HashMap<String, WrapperFactory>>,
}

impl std::fmt::Debug for WrapperRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WrapperRegistry").finish_non_exhaustive()
    }
}

impl WrapperRegistry {
    /// Register a factory.
    pub fn register(&self, name: &str, f: WrapperFactory) {
        self.map
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name.to_owned(), f);
    }

    /// Look up a factory.
    pub fn get(&self, name: &str) -> Option<WrapperFactory> {
        self.map
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_tcp_streams_unwrap() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(async {
            let other: Box<dyn ConnStream> = Box::new(tokio::io::duplex(8).0);
            assert!((*other).tcp().is_none());
            assert!(<dyn ConnStream>::into_tcp(other).is_err());
            let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let c = TcpStream::connect(l.local_addr().expect("addr"))
                .await
                .expect("connect");
            let plain: Box<dyn ConnStream> = Box::new(c);
            assert!((*plain).tcp().is_some());
            assert!(<dyn ConnStream>::into_tcp(plain).is_ok());
        });
    }

    fn info() -> DriverInfo {
        let mut c = Config::new();
        c.put("ns/module/nssock", "port", "8000");
        c.put("ns/module/nssock", "hostname", "example.com");
        c.put("ns/module/nssock", "maxinput", "4096");
        DriverInfo::from_config(
            &c,
            "ns/module/nssock",
            "nssock",
            "nssock",
            Some("s1"),
            false,
            "localhost",
        )
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt")
    }

    async fn parse(input: &[u8], i: &DriverInfo) -> Result<(ReadRequest, Vec<u8>), ReadError> {
        let limits = Limits::new("t", crate::pool::LimitsConfig::default());
        parse_with(input, i, &limits, input.len().max(1)).await
    }

    /// Read a request sent in `piece`-byte writes under `limits`.
    async fn parse_with(
        input: &[u8],
        i: &DriverInfo,
        limits: &Arc<Limits>,
        piece: usize,
    ) -> Result<(ReadRequest, Vec<u8>), ReadError> {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let data = input.to_vec();
        let writer = tokio::spawn(async move {
            for p in data.chunks(piece) {
                if client.write_all(p).await.is_err() {
                    break;
                }
            }
        });
        let mut s: Box<dyn ConnStream> = Box::new(server);
        let mut buf = Vec::new();
        let l = Arc::clone(limits);
        let r = read_request(&mut s, &mut buf, i, Duration::from_secs(1), &move |_, _| {
            Arc::clone(&l)
        })
        .await;
        writer.abort();
        Ok((r?, buf))
    }

    fn limits(maxupload: usize) -> Arc<Limits> {
        Limits::new(
            "t",
            crate::pool::LimitsConfig {
                maxupload,
                ..crate::pool::LimitsConfig::default()
            },
        )
    }

    fn post(len: usize, body: &[u8]) -> Vec<u8> {
        let mut v = format!("POST /up HTTP/1.1\r\nContent-Length: {len}\r\n\r\n").into_bytes();
        v.extend_from_slice(body);
        v
    }

    fn pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn small_bodies_stay_in_memory() {
        let i = info();
        let head = post(1000, b"").len();
        // The largest body that, with its request and a CRLF, stays under
        // maxinput.
        let n = i.maxinput - head - 3;
        let body = pattern(n);
        let (r, _) = rt()
            .block_on(parse_with(&post(n, &body), &i, &limits(1 << 20), 1000))
            .expect("read");
        assert!(!r.content.on_disk());
        assert_eq!(r.content.bytes().expect("bytes"), &body[..]);
    }

    #[test]
    fn large_bodies_are_spooled() {
        let i = info();
        let head = post(1000, b"").len();
        let n = i.maxinput - head - 2;
        assert!(spools(head, n, i.maxinput) && !spools(head, n - 1, i.maxinput));
        for len in [n, 100_000] {
            let body = pattern(len);
            let mut input = post(len, &body);
            input.extend_from_slice(b"GET /next HTTP/1.0\r\n\r\n");
            let (r, left) = rt()
                .block_on(parse_with(&input, &i, &limits(1 << 20), 777))
                .expect("read");
            assert!(spools(post(len, b"").len(), len, i.maxinput));
            assert!(r.content.file_content(), "{len}");
            assert_eq!(r.content.len(), len);
            assert_eq!(r.content.read_at(0, len).expect("read"), body);
            assert!(left.starts_with(b"GET /next"), "{len}");
        }
    }

    #[test]
    fn chunked_bodies_spill_to_disk() {
        let i = info();
        let mut input = b"POST /c HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        let body = pattern(9000);
        for c in body.chunks(1500) {
            input.extend_from_slice(format!("{:x}\r\n", c.len()).as_bytes());
            input.extend_from_slice(c);
            input.extend_from_slice(b"\r\n");
        }
        input.extend_from_slice(b"0\r\n\r\n");
        let (r, _) = rt()
            .block_on(parse_with(&input, &i, &limits(1 << 20), 333))
            .expect("read");
        assert!(r.content.file_content());
        assert_eq!(r.content.to_vec(), body);
        // Over maxupload: refused part-way, and counted as dropped.
        let l = limits(4000);
        assert!(matches!(
            rt().block_on(parse_with(&input, &i, &l, 333)),
            Err(ReadError::Status(413))
        ));
        assert_eq!(l.counters().ndropped, 1);
    }

    /// A GET whose request line and headers are exactly `total` bytes.
    fn get_head(total: usize) -> Vec<u8> {
        let pad = "p".repeat(total - b"GET / HTTP/1.0\r\nX-Pad: \r\n\r\n".len());
        format!("GET / HTTP/1.0\r\nX-Pad: {pad}\r\n\r\n").into_bytes()
    }

    #[test]
    fn head_is_bounded_by_maxinput() {
        let i = info();
        for piece in [1, 1000, 100_000] {
            // A head ending at maxinput is read; one byte more is E_RRANGE.
            let (r, _) = rt()
                .block_on(parse_with(
                    &get_head(i.maxinput),
                    &i,
                    &limits(1 << 20),
                    piece,
                ))
                .expect("read");
            assert_eq!(r.request.url, "/");
            assert!(
                matches!(
                    rt().block_on(parse_with(
                        &get_head(i.maxinput + 1),
                        &i,
                        &limits(1 << 20),
                        piece
                    )),
                    Err(ReadError::Status(400))
                ),
                "{piece}"
            );
            // Many short headers past maxinput.
            let mut many = b"GET / HTTP/1.0\r\n".to_vec();
            for n in 0..70 {
                many.extend_from_slice(format!("X-H{n}: {}\r\n", "v".repeat(90)).as_bytes());
            }
            many.extend_from_slice(b"\r\n");
            assert!(many.len() > i.maxinput);
            assert!(matches!(
                rt().block_on(parse_with(&many, &i, &limits(1 << 20), piece)),
                Err(ReadError::Status(400))
            ));
        }
        // maxheader is read but unused, as in AOLserver.
        let mut c = Config::new();
        c.put("ns/module/nssock", "maxheader", "1024");
        let i =
            DriverInfo::from_config(&c, "ns/module/nssock", "nssock", "nssock", None, false, "h");
        assert_eq!(i.maxheader, 1024);
        let mut big = b"GET / HTTP/1.0\r\n".to_vec();
        for n in 0..10 {
            big.extend_from_slice(format!("X-H{n}: {}\r\n", "v".repeat(4000)).as_bytes());
        }
        big.extend_from_slice(b"\r\n");
        assert!(big.len() > 32 * 1024);
        let (r, _) = rt().block_on(parse(&big, &i)).expect("read");
        assert_eq!(r.headers.len(), 10);
    }

    #[test]
    fn bodiless_request_spools_when_its_head_reaches_maxinput() {
        let i = info();
        // roff + 0 + 2 >= maxinput: an empty spooled body (NS_CONN_FILECONTENT).
        let (r, _) = rt()
            .block_on(parse(&get_head(i.maxinput - 2), &i))
            .expect("read");
        assert!(r.content.file_content());
        assert_eq!(r.content.len(), 0);
        let (r, _) = rt()
            .block_on(parse(&get_head(i.maxinput - 3), &i))
            .expect("read");
        assert!(!r.content.on_disk());
        // Chunked bodies start from the same decision.
        let mut input = get_head(i.maxinput - 2);
        input.truncate(input.len() - 2);
        input.extend_from_slice(b"Transfer-Encoding: chunked\r\n\r\n");
        let i2 = DriverInfo {
            maxinput: input.len() + 1,
            ..info()
        };
        input.extend_from_slice(b"3\r\nabc\r\n0\r\n\r\n");
        let (r, _) = rt().block_on(parse(&input, &i2)).expect("read");
        assert!(r.content.file_content());
        assert_eq!(r.content.to_vec(), b"abc");
    }

    #[test]
    fn maxupload_refuses_before_reading() {
        let i = info();
        let l = limits(10);
        assert!(matches!(
            rt().block_on(parse_with(&post(11, b""), &i, &l, 100)),
            Err(ReadError::Status(413))
        ));
        assert_eq!(l.counters().ndropped, 1);
        let (r, _) = rt()
            .block_on(parse_with(&post(10, b"0123456789"), &i, &l, 100))
            .expect("read");
        assert_eq!(r.content.to_vec(), b"0123456789");
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(24))]

        /// The read-ahead invariants of `verus/spool.rs`: the body is read
        /// exactly, in the sink `spools` chooses, however it is split, and
        /// pipelined bytes are left alone.
        #[test]
        fn read_ahead_accounting(len in 0usize..12_000, piece in 1usize..5000, extra in 0usize..64) {
            let i = info();
            let head = post(len, b"").len();
            let body = pattern(len);
            let mut input = post(len, &body);
            input.extend(std::iter::repeat_n(b'x', extra));
            let (r, left) = rt()
                .block_on(parse_with(&input, &i, &limits(1 << 20), piece))
                .expect("read");
            proptest::prop_assert_eq!(r.content.on_disk(), spools(head, len, i.maxinput));
            proptest::prop_assert_eq!(r.content.len(), len);
            proptest::prop_assert_eq!(r.content.to_vec(), body);
            proptest::prop_assert!(left.len() <= extra);
            proptest::prop_assert!(left.iter().all(|&b| b == b'x'));
        }
    }

    #[test]
    fn config_and_location() {
        let i = info();
        assert_eq!(i.location(), "http://example.com:8000");
        assert_eq!(i.maxinput, 4096);
        assert_eq!(i.maxline, 4096);
        assert_eq!(i.keepwait, Duration::from_secs(30));
        assert!(!i.virtual_hosts);
        i.port.store(80, Ordering::Relaxed);
        assert_eq!(i.location(), "http://example.com");
    }

    #[test]
    fn reads_simple_and_pipelined() {
        let i = info();
        let (r, left) = rt()
            .block_on(parse(
                b"GET /a HTTP/1.1\r\nHost: x\r\n\r\nGET /b HTTP/1.1\r\n\r\n",
                &i,
            ))
            .expect("read");
        assert_eq!(r.request.url, "/a");
        assert_eq!(r.headers.get("Host"), Some("x"));
        assert!(left.starts_with(b"GET /b"));
    }

    #[test]
    fn reads_body_and_chunked() {
        let i = info();
        let (r, _) = rt()
            .block_on(parse(
                b"POST /p HTTP/1.0\r\nContent-Length: 5\r\n\r\nhelloEXTRA",
                &i,
            ))
            .expect("read");
        assert_eq!(r.content.to_vec(), b"hello");
        let (r, _) = rt()
            .block_on(parse(
                b"POST /c HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6;x=y\r\n world\r\n0\r\nT: v\r\n\r\n",
                &i,
            ))
            .expect("read");
        assert_eq!(r.content.to_vec(), b"hello world");
    }

    #[test]
    fn limits_and_errors() {
        let i = info();
        let big = format!("POST / HTTP/1.0\r\nContent-Length: {}\r\n\r\n", 10_240_001);
        assert!(matches!(
            rt().block_on(parse(big.as_bytes(), &i)),
            Err(ReadError::Status(413))
        ));
        // A spooled body cut short by the client.
        assert!(matches!(
            rt().block_on(parse(&post(10_000, b"abc"), &i)),
            Err(ReadError::Eof)
        ));
        let long = format!("GET /{} HTTP/1.0\r\n\r\n", "a".repeat(5000));
        assert!(matches!(
            rt().block_on(parse(long.as_bytes(), &i)),
            Err(ReadError::Status(414))
        ));
        assert!(matches!(
            rt().block_on(parse(b"GET / HTTP/1.0\r\nbad header\r\n\r\n", &i)),
            Err(ReadError::Status(400))
        ));
        assert!(matches!(
            rt().block_on(parse(b"GET / HTTP/1.0\r\n", &i)),
            Err(ReadError::Eof)
        ));
        assert!(matches!(
            rt().block_on(parse(b"POST / HTTP/1.0\r\nContent-Length: x\r\n\r\n", &i)),
            Err(ReadError::Status(400))
        ));
    }

    #[test]
    fn http09() {
        let i = info();
        let (r, _) = rt().block_on(parse(b"GET /old\r\n", &i)).expect("read");
        assert_eq!(r.request.major, 0);
        assert_eq!(r.headers.len(), 0);
    }

    /// A stream whose peer never reads: writes and shutdown stay pending.
    struct Stuck;

    impl AsyncRead for Stuck {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Pending
        }
    }

    impl AsyncWrite for Stuck {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            std::task::Poll::Pending
        }
        fn poll_flush(
            self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Pending
        }
        fn poll_shutdown(
            self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Pending
        }
    }

    #[test]
    fn graceful_close_is_bounded_by_sendwait() {
        let done = rt().block_on(async {
            tokio::time::timeout(
                Duration::from_secs(3),
                graceful_close(
                    Box::new(Stuck),
                    Duration::from_millis(100),
                    Duration::from_millis(100),
                ),
            )
            .await
        });
        assert!(done.is_ok(), "graceful_close hung on a stuck shutdown");
    }

    #[test]
    fn header_end() {
        assert_eq!(find_header_end(b"a\r\n\r\nb"), Some((1, 5)));
        assert_eq!(find_header_end(b"a\n\nb"), Some((1, 3)));
        assert_eq!(find_header_end(b"abc"), None);
    }
}
