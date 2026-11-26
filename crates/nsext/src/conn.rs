//! One daemon connection (`NsExtConn`) and the client side of the
//! protocol (`DbProxy*`).

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use ns_core::log::Severity;
use nsdb::{DbConnection, DbError, ExecResult};
use nspd::msg::{self, Msg};
use nspd::strerror;

use crate::Ctx;

/// `STATUS_BUFSIZE`.
pub(crate) const STATUS_BUFSIZE: usize = 4096;
/// `RSP_BUFSIZE`.
pub(crate) const RSP_BUFSIZE: usize = 32768;
/// `RESPBUFMAX`.
const RESPBUFMAX: usize = 256;
/// `MAX_SIZEDIGITS`.
const MAX_SIZEDIGITS: usize = 32;
/// The buffer `ns_db sp_returncode` passes (`Ns_DbSpReturnCode(.., 32)`).
const RETURNCODE_BUFSIZE: usize = 32;

/// The daemon socket: TCP for a remote daemon, a socket pair for a
/// local one.
enum Sock {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl Sock {
    fn try_clone(&self) -> io::Result<Self> {
        Ok(match self {
            Self::Tcp(s) => Self::Tcp(s.try_clone()?),
            Self::Unix(s) => Self::Unix(s.try_clone()?),
        })
    }

    fn set_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Tcp(s) => {
                s.set_read_timeout(t)?;
                s.set_write_timeout(t)
            }
            Self::Unix(s) => {
                s.set_read_timeout(t)?;
                s.set_write_timeout(t)
            }
        }
    }

    fn fd(&self) -> i32 {
        match self {
            Self::Tcp(s) => s.as_raw_fd(),
            Self::Unix(s) => s.as_raw_fd(),
        }
    }
}

impl Read for Sock {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(s) => s.read(buf),
            Self::Unix(s) => s.read(buf),
        }
    }
}

impl Write for Sock {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(s) => s.write(buf),
            Self::Unix(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Io {
    r: BufReader<Sock>,
    w: Sock,
}

/// Why a reply line could not be read.
enum LineErr {
    Io(io::Error),
    /// No newline within the buffer; the bytes read.
    TooLong(Vec<u8>),
}

/// Read one `\n`-terminated line of at most `max - 1` bytes (the reply
/// buffer `DbProxyGetString` peeks into).
fn read_line(r: &mut BufReader<Sock>, max: usize) -> Result<Vec<u8>, LineErr> {
    let mut line = Vec::new();
    loop {
        let buf = r.fill_buf().map_err(LineErr::Io)?;
        if buf.is_empty() {
            return Err(LineErr::Io(io::ErrorKind::UnexpectedEof.into()));
        }
        if let Some(i) = buf.iter().position(|&b| b == b'\n') {
            let fits = line.len() + i < max;
            line.extend_from_slice(&buf[..i]);
            r.consume(i + 1);
            return if fits {
                Ok(line)
            } else {
                Err(LineErr::TooLong(line))
            };
        }
        let n = buf.len();
        line.extend_from_slice(buf);
        r.consume(n);
        if line.len() >= max {
            return Err(LineErr::TooLong(line));
        }
    }
}

/// A connection to a proxy daemon (`NsExtConn`).  `io` is `None` once
/// the connection has been cleaned up after an error (`DbProxyCleanup`);
/// the handle is then disconnected.
pub struct ExtConn {
    io: Option<Io>,
    num: u32,
    ctx: Arc<Ctx>,
    ncols: usize,
}

impl std::fmt::Debug for ExtConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtConn")
            .field("num", &self.num)
            .field("connected", &self.io.is_some())
            .finish_non_exhaustive()
    }
}

fn no_exception() -> DbError {
    DbError::msg("")
}

impl ExtConn {
    /// `ExtOpenDb`.
    pub(crate) fn open(
        ctx: &Arc<Ctx>,
        datasource: &str,
        user: Option<&str>,
        password: Option<&str>,
    ) -> Result<Self, DbError> {
        let d = msg::ARG_TOKEN_DELIMITER;
        let open_data = format!(
            "{datasource}{d}{}{d}{}{d}{}",
            user.unwrap_or_default(),
            password.unwrap_or_default(),
            ctx.param.as_deref().unwrap_or_default()
        );
        let mut c = Self {
            io: None,
            num: ctx.next_conn_num(),
            ctx: Arc::clone(ctx),
            ncols: 0,
        };
        let r = c.start().and_then(|()| {
            if !c.is_alive() {
                return Err(no_exception());
            }
            if !c.send(Msg::Open, Some(open_data.as_bytes())) {
                return Err(no_exception());
            }
            c.check_status()?;
            let ident = c.identify().ok_or_else(no_exception)?;
            c.ctx.log(
                Severity::Notice,
                &format!(
                    "nsext: datasource opened: {datasource} with {}",
                    c.ctx.ident()
                ),
            );
            c.ctx.set_ident(&ident);
            Ok(())
        });
        match r {
            Ok(()) => Ok(c),
            Err(e) => {
                c.cleanup();
                Err(e)
            }
        }
    }

    /// The connection number (`ns_ext number`).
    pub const fn number(&self) -> u32 {
        self.num
    }

    /// The driver settings.
    pub fn ctx(&self) -> &Arc<Ctx> {
        &self.ctx
    }

    fn log(&self, sev: Severity, m: &str) {
        self.ctx.log(sev, m);
    }

    /// `DbProxyStart`: `LocalProxy` or `NetProxy`.
    fn start(&mut self) -> Result<(), DbError> {
        let sock = match (&self.ctx.path, &self.ctx.host) {
            (Some(path), _) => self.local_proxy(path)?,
            (None, Some(host)) => {
                let port = self.ctx.port;
                let s = u16::try_from(port)
                    .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
                    .and_then(|p| TcpStream::connect((host.as_str(), p)));
                let Ok(s) = s else {
                    self.log(
                        Severity::Error,
                        &format!("nsext: connect failure: {host}:{port}"),
                    );
                    return Err(no_exception());
                };
                self.log(
                    Severity::Notice,
                    &format!("nsext: connect success: {host}:{port}"),
                );
                Sock::Tcp(s)
            }
            (None, None) => return Err(no_exception()),
        };
        let t = u64::try_from(self.ctx.timeout)
            .ok()
            .filter(|&t| t > 0)
            .map(Duration::from_secs);
        let r = sock.try_clone().and_then(|w| {
            sock.set_timeout(t)?;
            Ok(w)
        });
        match r {
            Ok(w) => {
                self.io = Some(Io {
                    r: BufReader::new(sock),
                    w,
                });
                Ok(())
            }
            Err(e) => {
                self.log(
                    Severity::Error,
                    &format!("nsext: failed to set up daemon socket: {}", strerror(&e)),
                );
                Err(no_exception())
            }
        }
    }

    /// `LocalProxy`: run the daemon on one end of a socket pair (its
    /// stdin and stdout) and wait for the process to exit 0: a daemon
    /// detaches and serves the connection from its child.
    fn local_proxy(&self, path: &std::path::Path) -> Result<Sock, DbError> {
        let Ok((ours, theirs)) = UnixStream::pair() else {
            self.log(
                Severity::Error,
                "nsext: failed to create input socket pipes",
            );
            return Err(no_exception());
        };
        let spawned = theirs.try_clone().and_then(|out| {
            Command::new(path)
                .stdin(Stdio::from(OwnedFd::from(theirs)))
                .stdout(Stdio::from(OwnedFd::from(out)))
                .spawn()
        });
        let Ok(mut child) = spawned else {
            self.log(
                Severity::Error,
                &format!("nsext: spawn failed for '{}'", path.display()),
            );
            return Err(no_exception());
        };
        match child.wait() {
            Ok(st) if st.success() => Ok(Sock::Unix(ours)),
            Ok(st) => {
                self.log(
                    Severity::Error,
                    &format!(
                        "nsext: proxy returned non-zero exit code: {}",
                        st.code().unwrap_or(-1)
                    ),
                );
                Err(no_exception())
            }
            Err(e) => {
                self.log(
                    Severity::Error,
                    &format!("nsext: waitpid failed: {}", strerror(&e)),
                );
                Err(no_exception())
            }
        }
    }

    /// `DbProxyStop`.
    fn stop(&mut self) {
        if self.io.take().is_some() {
            self.log(
                Severity::Debug,
                &format!("nsext: stopping db proxy daemon connection {}", self.num),
            );
        }
    }

    /// `DbProxyCleanup`: drop the daemon connection after an error; the
    /// handle is no longer connected.
    pub(crate) fn cleanup(&mut self) {
        self.stop();
    }

    /// The failure of a read or write (`DbProxyTimedIO`).
    fn io_failed(&mut self, e: &io::Error, write: bool) {
        let fd = self.io.as_ref().map_or(-1, |io| io.w.fd());
        match e.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => self.log(
                Severity::Warning,
                &format!(
                    "nsext: exceeded proxy i/o timeout ({} seconds)",
                    self.ctx.timeout
                ),
            ),
            io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::InvalidInput => self.log(
                Severity::Warning,
                "nsext: connection dropped by external proxy daemon",
            ),
            _ => self.log(
                Severity::Error,
                &format!(
                    "nsext: socket {} {fd} failed: {} (code {})",
                    if write { "write to" } else { "read from" },
                    strerror(e),
                    e.raw_os_error().unwrap_or(0)
                ),
            ),
        }
        self.cleanup();
    }

    /// `DbProxySend`.
    pub(crate) fn send(&mut self, m: Msg, arg: Option<&[u8]>) -> bool {
        if arg.is_none() && m.requires_arg() {
            self.log(
                Severity::Bug,
                &format!(
                    "nsext: '{}' message requires argument (connection {})",
                    m.name(),
                    self.num
                ),
            );
            return false;
        }
        let buf = msg::encode(m, arg);
        let Some(io) = self.io.as_mut() else {
            return false;
        };
        if let Err(e) = io.w.write_all(&buf) {
            self.io_failed(&e, true);
            self.log(
                Severity::Error,
                &format!(
                    "nsext: error sending buffer({}): {}",
                    String::from_utf8_lossy(&buf),
                    strerror(&e)
                ),
            );
            return false;
        }
        if self.ctx.io_trace {
            self.log(
                Severity::Notice,
                &format!("SENT |{}|", String::from_utf8_lossy(&buf)),
            );
        }
        true
    }

    /// `DbProxyGetString`: one reply line of fewer than `max` bytes.
    pub(crate) fn get_string(&mut self, max: usize) -> Option<String> {
        let io = self.io.as_mut()?;
        match read_line(&mut io.r, max) {
            Ok(line) => {
                let s = String::from_utf8_lossy(&line).into_owned();
                if self.ctx.io_trace {
                    self.log(Severity::Notice, &format!("REC |{s}|"));
                }
                Some(s)
            }
            Err(LineErr::Io(e)) => {
                self.io_failed(&e, false);
                None
            }
            Err(LineErr::TooLong(mut b)) => {
                b.truncate(max.saturating_sub(1));
                self.log(
                    Severity::Error,
                    &format!(
                        "nsext: protocol error: no record separator in '{}'",
                        String::from_utf8_lossy(&b)
                    ),
                );
                self.cleanup();
                None
            }
        }
    }

    /// `DbProxyGetPingReply`: anything but `ok` breaks the connection.
    pub(crate) fn ping_reply(&mut self) -> bool {
        let Some(s) = self.get_string(STATUS_BUFSIZE) else {
            return false;
        };
        if s.eq_ignore_ascii_case(msg::OK_STATUS) {
            return true;
        }
        self.log(
            Severity::Error,
            &format!(
                "nsext: protocol error on ping: received|{s}| expected|{}|",
                msg::OK_STATUS
            ),
        );
        self.cleanup();
        false
    }

    /// `DbProxyIsAlive`.
    pub(crate) fn is_alive(&mut self) -> bool {
        self.send(Msg::Ping, None) && self.ping_reply()
    }

    /// `DbProxyCheckStatus`: `ok`, or the exception `code#message` (a
    /// line without `#` is only logged, `silentError` is not reported).
    pub(crate) fn check_status(&mut self) -> Result<(), DbError> {
        let s = self.get_string(STATUS_BUFSIZE).ok_or_else(no_exception)?;
        if s.eq_ignore_ascii_case(msg::OK_STATUS) {
            return Ok(());
        }
        if s.contains(msg::SILENT_ERROR_STATUS) {
            if self.ctx.verbose {
                self.log(
                    Severity::Debug,
                    &format!("nsext: silent error string '{s}'"),
                );
            }
            return Err(no_exception());
        }
        let Some((code, message)) = s.split_once(msg::ARG_TOKEN_DELIMITER) else {
            self.log(
                Severity::Error,
                &format!("nsext: database error message: '{s}'"),
            );
            return Err(no_exception());
        };
        if message.is_empty() {
            self.log(
                Severity::Error,
                &format!("nsext: invalid exception status string: '{code}'"),
            );
            return Err(no_exception());
        }
        self.log(
            Severity::Debug,
            &format!("nsext: received exception code={code} msg={message}"),
        );
        Err(DbError {
            code: code.to_owned(),
            message: message.to_owned(),
        })
    }

    /// `DbProxyGetList`: size-prefixed elements up to `-1`.  An empty
    /// element is a NULL value.  `None` after an error, which breaks the
    /// connection.
    pub(crate) fn get_list(&mut self) -> Option<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        loop {
            let size = self.get_string(MAX_SIZEDIGITS)?;
            if !msg::all_digits(&size) {
                self.log(
                    Severity::Error,
                    &format!("nsext: protocol error: number expected, got '{size}'"),
                );
                self.cleanup();
                return None;
            }
            let n = msg::atoi(&size);
            if n > self.ctx.max_element_size {
                self.log(
                    Severity::Error,
                    &format!("nsext: exceeded element size limit of {n}"),
                );
                self.cleanup();
                return None;
            }
            if n == -1 {
                return Some(out);
            }
            let Ok(n) = usize::try_from(n) else {
                self.log(
                    Severity::Error,
                    &format!("nsext: protocol error: number expected, got '{size}'"),
                );
                self.cleanup();
                return None;
            };
            let mut data = vec![0; n];
            let io = self.io.as_mut()?;
            if let Err(e) = io.r.read_exact(&mut data) {
                self.io_failed(&e, false);
                self.log(
                    Severity::Error,
                    &format!("nsext: read error: {}", strerror(&e)),
                );
                return None;
            }
            if self.ctx.io_trace {
                self.log(
                    Severity::Notice,
                    &format!("REC |{}|", String::from_utf8_lossy(&data)),
                );
            }
            out.push(data);
        }
    }

    /// A value from a row: `TrimData` strips trailing spaces, and an
    /// empty element is NULL.
    fn value(&self, data: &[u8]) -> Option<String> {
        if data.is_empty() {
            return None;
        }
        let mut s = String::from_utf8_lossy(data).into_owned();
        if self.ctx.trim_data {
            s.truncate(s.trim_end_matches(' ').len());
        }
        Some(s)
    }

    /// A request answered by a status alone.
    fn status_cmd(&mut self, m: Msg, arg: Option<&[u8]>) -> Result<(), DbError> {
        if !self.send(m, arg) {
            return Err(no_exception());
        }
        self.check_status()
    }

    /// `exec_rows` / `exec_dml` after an `ok` (`ExtExec`, `ExtSpExec`).
    fn exec_reply(&mut self) -> Result<ExecResult, DbError> {
        self.check_status()?;
        match self.get_string(RESPBUFMAX).as_deref() {
            Some(msg::EXEC_RET_ROWS) => Ok(ExecResult::Rows(Vec::new())),
            Some(msg::EXEC_RET_DML) => Ok(ExecResult::Dml(0)),
            _ => Err(no_exception()),
        }
    }

    /// A request answered by one line (`DbProxyIdentify`, ...): `what`
    /// names it in the error logged when it fails.
    pub(crate) fn line_cmd(
        &mut self,
        m: Msg,
        arg: Option<&[u8]>,
        max: usize,
        what: &str,
    ) -> Option<String> {
        let r = if self.send(m, arg) {
            self.get_string(max)
        } else {
            None
        };
        if r.is_none() {
            self.log(
                Severity::Error,
                &format!("nsext: '{what}' command to proxy daemon failed"),
            );
        }
        r
    }

    /// `DbProxyIdentify`.
    pub(crate) fn identify(&mut self) -> Option<String> {
        self.line_cmd(Msg::Identify, None, RSP_BUFSIZE, "identify")
    }

    /// A request answered by a ping reply (`setmaxrows`, `traceon`,
    /// `traceoff`): `what` names it in the error logged.
    pub(crate) fn ping_cmd(&mut self, m: Msg, arg: Option<&[u8]>, what: &str) -> bool {
        let ok = self.send(m, arg) && self.ping_reply();
        if !ok {
            self.log(
                Severity::Error,
                &format!("nsext: '{what}' command to proxy daemon failed"),
            );
        }
        ok
    }
}

impl DbConnection for ExtConn {
    fn exec(&mut self, sql: &str) -> Result<ExecResult, DbError> {
        if !self.send(Msg::Exec, Some(sql.as_bytes())) {
            return Err(no_exception());
        }
        self.exec_reply()
    }

    /// `ExtBindRow`; a result with no columns is an error.
    fn bind_row(&mut self) -> Option<Result<Vec<String>, DbError>> {
        Some((|| {
            self.status_cmd(Msg::BindRow, None)?;
            let cols = self.get_list().ok_or_else(no_exception)?;
            if cols.is_empty() {
                return Err(no_exception());
            }
            self.ncols = cols.len();
            Ok(cols
                .iter()
                .map(|c| String::from_utf8_lossy(c).into_owned())
                .collect())
        })())
    }

    fn next_row(&mut self) -> Result<Option<Vec<Option<String>>>, DbError> {
        self.next_row_n(self.ncols)
    }

    /// `ExtGetRow`: the set size is sent; an empty list ends the data.
    fn next_row_n(&mut self, ncols: usize) -> Result<Option<Vec<Option<String>>>, DbError> {
        self.status_cmd(Msg::GetRow, Some(ncols.to_string().as_bytes()))?;
        let row = self.get_list().ok_or_else(no_exception)?;
        if row.is_empty() {
            return Ok(None);
        }
        Ok(Some(row.iter().map(|v| self.value(v)).collect()))
    }

    fn flush(&mut self) {
        let _ = self.try_flush();
    }

    /// `ExtFlush`.
    fn try_flush(&mut self) -> Result<(), DbError> {
        self.status_cmd(Msg::Flush, None)
    }

    /// `Ns_ExtCancel`.
    fn cancel(&mut self) -> Result<(), DbError> {
        self.status_cmd(Msg::Cancel, None)
    }

    /// `ExtResetHandle`: the reply must start with `ok`.
    fn reset(&mut self) -> Result<(), DbError> {
        self.send(Msg::ResetHandle, None);
        let s = self.get_string(STATUS_BUFSIZE).ok_or_else(no_exception)?;
        if s.get(..2)
            .is_some_and(|p| p.eq_ignore_ascii_case(msg::OK_STATUS))
        {
            return Ok(());
        }
        self.log(
            Severity::Error,
            &format!(
                "nsext: protocol error: received|{s}| expected|{}|",
                msg::OK_STATUS
            ),
        );
        self.cleanup();
        Err(no_exception())
    }

    fn connected(&self) -> bool {
        self.io.is_some()
    }

    fn db_type(&self) -> Option<String> {
        Some(self.ctx.db_type())
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }

    fn supports_sp(&self) -> bool {
        true
    }

    /// `ExtSpStart`.
    fn sp_start(&mut self, procname: &str) -> Result<(), DbError> {
        self.status_cmd(Msg::SpStart, Some(procname.as_bytes()))
    }

    /// `ExtSpSetParam`.
    fn sp_setparam(&mut self, args: &str) -> Result<(), DbError> {
        self.status_cmd(Msg::SpSetParam, Some(args.as_bytes()))
    }

    /// `ExtSpExec`.
    fn sp_exec(&mut self) -> Result<ExecResult, DbError> {
        if !self.send(Msg::SpExec, None) {
            return Err(no_exception());
        }
        self.exec_reply()
    }

    /// `ExtSpReturnCode`.
    fn sp_returncode(&mut self) -> Result<String, DbError> {
        self.line_cmd(Msg::SpReturnCode, None, RETURNCODE_BUFSIZE, "returncode")
            .ok_or_else(no_exception)
    }

    /// `ExtSpGetParams`: the names, then the values.  Both lists are read
    /// even when the first is empty, keeping the connection in step.
    fn sp_getparams(&mut self) -> Result<Vec<(String, Option<String>)>, DbError> {
        self.status_cmd(Msg::SpGetParams, None)?;
        let names = self.get_list().ok_or_else(no_exception)?;
        let Some(values) = self.get_list() else {
            self.log(Severity::Error, "nsext: rowlist did not arrive");
            return Err(no_exception());
        };
        if names.is_empty() {
            return Err(no_exception());
        }
        Ok(names
            .iter()
            .zip(&values)
            .map(|(n, v)| (String::from_utf8_lossy(n).into_owned(), self.value(v)))
            .collect())
    }
}

impl Drop for ExtConn {
    /// `ExtCloseDb`: send `close` and stop the daemon connection.
    fn drop(&mut self) {
        if self.io.is_none() {
            return;
        }
        let r = self.status_cmd(Msg::Close, None);
        if r.is_err() {
            self.log(
                Severity::Error,
                &format!("nsext: error closing connection: {}", self.num),
            );
        }
        self.stop();
    }
}
