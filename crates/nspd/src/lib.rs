//! # nspd
//!
//! The proxy-daemon side of the AOLserver external database driver
//! protocol (`nspd/main.c`, `listen.c`, `log.c`, `include/nspd.h`): a
//! daemon reads requests from `nsext` (the server's `External` driver)
//! and answers them from a database reached through a [`PdDb`]
//! implementation, the Rust form of the `Ns_PdDb*` functions a C daemon
//! linked against `libnspd` provided.
//!
//! Generic messages (`ping`, `traceon`, `traceoff` and the remote file
//! commands `openfile`, `readfile`, ...) are handled here, as in
//! `main.c`; the database messages are dispatched to the [`PdDb`].
//!
//! [`DriverDb`] serves any `nsdb` driver this way, and the `nssqlitepd`
//! binary serves SQLite databases with it:
//!
//! ```text
//! nssqlitepd          # one connection on stdin/stdout (nsext LocalDaemon)
//! nssqlitepd 8199     # listen on port 8199 (nsext RemoteHost/RemotePort)
//! ```

pub mod msg;
mod sqlite;

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;

pub use msg::Msg;
pub use sqlite::DriverDb;

/// `CMDMAX`: the largest request (name, length and argument).
pub const CMDMAX: usize = 32768;
/// `EXCEPTION_CODE_MAX`.
pub const EXCEPTION_CODE_MAX: usize = 32;
/// `EXCEPTION_MSG_MAX`.
pub const EXCEPTION_MSG_MAX: usize = 4096;
/// The read buffer of `PdReadFile`, the most one `readfile` returns.
const READ_MAX: usize = 4096;

/// Message priorities (`Ns_PdLogMsgType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// `LOG_ERR`.
    Error,
    /// `LOG_NOTICE`.
    Notice,
    /// Written to the trace file while tracing is on.
    Trace,
}

/// Where `Error` and `Notice` messages go (syslog in the daemons).
pub type Log = Arc<dyn Fn(Level, &str) + Send + Sync>;

/// How a request failed; the daemon replies with the error line instead
/// of `ok`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PdError {
    /// A database exception, sent as `code#message`
    /// (`Ns_PdSendException`); nsext makes it the handle's exception.
    Exception {
        /// Exception code (truncated to [`EXCEPTION_CODE_MAX`]).
        code: String,
        /// Message (truncated to [`EXCEPTION_MSG_MAX`]).
        message: String,
    },
    /// A plain error line, which nsext logs.
    Message(String),
    /// `silentError`: a failure nsext does not report.
    Silent,
}

impl PdError {
    /// An exception.
    pub fn exception(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Exception {
            code: code.into(),
            message: message.into(),
        }
    }

    /// The reply line.  Newlines would break the framing and become
    /// spaces (`Ns_PdSendException` does this for messages).
    pub fn line(&self) -> String {
        let s = match self {
            Self::Exception { code, message } => format!(
                "{}{}{}",
                truncate(code, EXCEPTION_CODE_MAX),
                msg::ARG_TOKEN_DELIMITER,
                truncate(message, EXCEPTION_MSG_MAX)
            ),
            Self::Message(m) => m.clone(),
            Self::Silent => msg::SILENT_ERROR_STATUS.to_owned(),
        };
        s.replace('\n', " ")
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut i = max;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    &s[..i]
}

/// What an `exec` or `spexec` returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdExec {
    /// Rows to fetch with `bindrow`/`getrow` (`exec_rows`).
    Rows,
    /// No rows (`exec_dml`).
    Dml,
}

/// The `open` argument, `datasource#user#password#param`
/// (`Ns_PdParseOpenArgs`): missing or empty fields are `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenArgs {
    /// The pool's datasource.
    pub datasource: Option<String>,
    /// The pool's user.
    pub user: Option<String>,
    /// The pool's password.
    pub password: Option<String>,
    /// The driver's `Param`.
    pub param: Option<String>,
}

impl OpenArgs {
    /// Parse as `Ns_PdParseOpenArgs` does: a field is set only when the
    /// delimiter after it is present (the param takes the rest).
    pub fn parse(s: &str) -> Self {
        let mut a = Self::default();
        let nonempty = |f: &str| (!f.is_empty()).then(|| f.to_owned());
        let Some((ds, rest)) = s.split_once(msg::ARG_TOKEN_DELIMITER) else {
            return a;
        };
        a.datasource = nonempty(ds);
        let Some((user, rest)) = rest.split_once(msg::ARG_TOKEN_DELIMITER) else {
            return a;
        };
        a.user = nonempty(user);
        let Some((pw, rest)) = rest.split_once(msg::ARG_TOKEN_DELIMITER) else {
            return a;
        };
        a.password = nonempty(pw);
        a.param = nonempty(rest);
        a
    }
}

fn no_sp() -> PdError {
    PdError::exception("NSPD", "stored procedures are not supported")
}

/// A daemon's database (the `Ns_PdDb*` functions of `nspd.h`).  One value
/// serves one connection from nsext; the daemon writes the replies.
pub trait PdDb {
    /// `open`: connect to the database (`Ns_PdDbOpen`).
    fn open(&mut self, args: &OpenArgs) -> Result<(), PdError>;
    /// `close` (`Ns_PdDbClose`); the daemon then stops reading.
    fn close(&mut self) -> Result<(), PdError>;
    /// `identify`: `<dbtype> Proxy Daemon vX.X` (`Ns_PdDbIdentify`);
    /// nsext reports the first word as the database type.
    fn identify(&mut self) -> String;
    /// `gettypes`: a space-separated list of type names.
    fn get_types(&mut self) -> String;
    /// `exec` (`Ns_PdDbExec`).
    fn exec(&mut self, sql: &str) -> Result<PdExec, PdError>;
    /// `bindrow`: the columns of the pending result (`Ns_PdDbBindRow`).
    fn bind_row(&mut self) -> Result<Vec<String>, PdError>;
    /// `getrow`: the next row, `None` at the end (`Ns_PdDbGetRow`).
    /// `ncols` is the size of the server's row set.
    fn get_row(&mut self, ncols: i64) -> Result<Option<Vec<Option<String>>>, PdError>;
    /// `flush`: discard pending rows (`Ns_PdDbFlush`).
    fn flush(&mut self) -> Result<(), PdError>;
    /// `cancel` (`Ns_PdDbCancel`).
    fn cancel(&mut self) -> Result<(), PdError> {
        self.flush()
    }
    /// `resultid`: the id of the last object an `exec` affected.
    fn result_id(&mut self) -> String {
        String::new()
    }
    /// `resultrows`: the number of rows the last `exec` affected.
    fn result_rows(&mut self) -> String {
        String::new()
    }
    /// `setmaxrows`: limit the rows a query returns.
    fn set_max_rows(&mut self, max: &str) -> Result<(), PdError> {
        let _ = max;
        Ok(())
    }
    /// `resethandle`: undo a failed transaction before the handle goes
    /// back to its pool (`Ns_PdDbResetHandle`).
    fn reset_handle(&mut self) -> Result<(), PdError> {
        Ok(())
    }
    /// `spstart` (`Ns_PdDbSpStart`).
    fn sp_start(&mut self, procname: &str) -> Result<(), PdError> {
        let _ = procname;
        Err(no_sp())
    }
    /// `spsetparam`: `name type in|out value` (`Ns_PdDbSpSetParam`).
    fn sp_set_param(&mut self, args: &str) -> Result<(), PdError> {
        let _ = args;
        Err(no_sp())
    }
    /// `spexec` (`Ns_PdDbSpExec`).
    fn sp_exec(&mut self) -> Result<PdExec, PdError> {
        Err(no_sp())
    }
    /// `spreturncode` (`Ns_PdDbSpReturnCode`).
    fn sp_return_code(&mut self) -> String {
        String::new()
    }
    /// `spgetparams`: output parameters as `(name, value)` pairs.
    fn sp_get_params(&mut self) -> Result<Vec<(String, Option<String>)>, PdError> {
        Err(no_sp())
    }
    /// The connection ended (`Ns_PdDbCleanup`).
    fn cleanup(&mut self) {}
}

/// Daemon settings.
#[derive(Clone)]
pub struct Options {
    /// Where `Error` and `Notice` messages go.
    pub log: Log,
    /// Directory of `createtmpfile` files (`/var/tmp/`).
    pub tmpdir: PathBuf,
    /// The largest request accepted ([`CMDMAX`]).
    pub max_request: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            log: Arc::new(|_, _| {}),
            tmpdir: PathBuf::from("/var/tmp/"),
            max_request: CMDMAX,
        }
    }
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("tmpdir", &self.tmpdir)
            .field("max_request", &self.max_request)
            .finish_non_exhaustive()
    }
}

/// `strerror` text of an I/O error (without Rust's `(os error N)`).
pub fn strerror(e: &io::Error) -> String {
    let s = e.to_string();
    match s.rfind(" (os error ") {
        Some(i) => s[..i].to_owned(),
        None => s,
    }
}

/// One connection's state (the statics of `main.c` and `log.c`).
struct Session<'a, D, W> {
    db: D,
    out: W,
    opts: &'a Options,
    read_input: bool,
    trace: bool,
    trace_file: Option<File>,
    files: HashMap<i32, File>,
}

impl<D: PdDb, W: Write> Session<'_, D, W> {
    /// `Ns_PdLog`.
    fn log(&mut self, level: Level, m: &str) {
        if level == Level::Trace {
            if self.trace {
                if let Some(f) = self.trace_file.as_mut() {
                    let _ = writeln!(f, "{m}");
                    let _ = f.flush();
                }
            }
        } else {
            (self.opts.log)(level, m);
        }
    }

    /// `Ns_PdSendData`.
    fn send_data(&mut self, data: &[u8]) -> io::Result<()> {
        self.out.write_all(data)
    }

    /// `Ns_PdSendString`: the string and a newline, in one write.
    fn send_string(&mut self, s: &str) -> io::Result<()> {
        let mut b = Vec::with_capacity(s.len() + 1);
        b.extend_from_slice(s.as_bytes());
        b.push(b'\n');
        self.send_data(&b)
    }

    fn send_ok(&mut self) -> io::Result<()> {
        self.send_string(msg::OK_STATUS)
    }

    /// `Ns_PdSendRowInfo`: each element's size and bytes, then `-1`.
    fn send_list<'s>(
        &mut self,
        items: impl IntoIterator<Item = Option<&'s [u8]>>,
    ) -> io::Result<()> {
        let mut b = Vec::new();
        for it in items {
            let data = it.unwrap_or_default();
            b.extend_from_slice(format!("{}\n", data.len()).as_bytes());
            b.extend_from_slice(data);
        }
        b.extend_from_slice(msg::END_DATA.as_bytes());
        self.send_data(&b)
    }

    /// A status reply: `ok` or the error line.
    fn send_status(&mut self, r: Result<(), PdError>) -> io::Result<()> {
        match r {
            Ok(()) => self.send_ok(),
            Err(e) => self.send_string(&e.line()),
        }
    }

    fn send_exec(&mut self, r: Result<PdExec, PdError>) -> io::Result<()> {
        match r {
            Ok(x) => {
                self.send_ok()?;
                self.send_string(match x {
                    PdExec::Rows => msg::EXEC_RET_ROWS,
                    PdExec::Dml => msg::EXEC_RET_DML,
                })
            }
            Err(e) => self.send_string(&e.line()),
        }
    }

    fn send_row(&mut self, r: Result<Option<Vec<Option<String>>>, PdError>) -> io::Result<()> {
        match r {
            Ok(row) => {
                self.send_ok()?;
                let row = row.unwrap_or_default();
                self.send_list(row.iter().map(|v| v.as_deref().map(str::as_bytes)))
            }
            Err(e) => self.send_string(&e.line()),
        }
    }

    /// `DispatchCmd`.
    fn dispatch(&mut self, cmd: &str, arg: &[u8]) -> io::Result<()> {
        let Some(m) = Msg::from_name(cmd) else {
            self.log(
                Level::Error,
                &format!("nspd: unknown dispatch message '{cmd}'"),
            );
            return Ok(());
        };
        let text = String::from_utf8_lossy(arg).into_owned();
        match m {
            Msg::Flush => {
                let r = self.db.flush();
                self.send_status(r)
            }
            Msg::Cancel => {
                let r = self.db.cancel();
                self.send_status(r)
            }
            Msg::Exec => {
                let r = self.db.exec(&text);
                self.send_exec(r)
            }
            Msg::BindRow => {
                let r = self.db.bind_row();
                match r {
                    Ok(cols) => {
                        self.send_ok()?;
                        self.send_list(cols.iter().map(|c| Some(c.as_bytes())))
                    }
                    Err(e) => self.send_string(&e.line()),
                }
            }
            Msg::GetRow => {
                let r = self.db.get_row(msg::atoi(&text));
                self.send_row(r)
            }
            Msg::GetTableInfo | Msg::TableList | Msg::BestRowId => {
                let name = match m {
                    Msg::GetTableInfo => "GetTableInfo",
                    Msg::TableList => "TableList",
                    _ => "BestRowId",
                };
                self.log(
                    Level::Error,
                    &format!("nspd: unsupported dispatch command: {name}"),
                );
                Ok(())
            }
            Msg::Close => {
                let r = self.db.close();
                self.read_input = false;
                self.send_status(r)
            }
            Msg::Open => {
                self.log(Level::Trace, &format!("Ns_PdParseOpenArgs: |{text}|"));
                let r = self.db.open(&OpenArgs::parse(&text));
                self.send_status(r)
            }
            Msg::Identify => {
                let s = self.db.identify();
                self.send_string(&s.replace('\n', " "))
            }
            Msg::GetTypes => {
                let s = self.db.get_types();
                self.send_string(&s.replace('\n', " "))
            }
            Msg::ResultId => {
                let s = self.db.result_id();
                self.send_string(&s.replace('\n', " "))
            }
            Msg::ResultRows => {
                let s = self.db.result_rows();
                self.send_string(&s.replace('\n', " "))
            }
            Msg::SetMaxRows => {
                let r = self.db.set_max_rows(&text);
                self.send_status(r)
            }
            Msg::ResetHandle => {
                let r = self.db.reset_handle();
                self.send_status(r)
            }
            Msg::SpStart => {
                let r = self.db.sp_start(&text);
                self.send_status(r)
            }
            Msg::SpSetParam => {
                let r = self.db.sp_set_param(&text);
                self.send_status(r)
            }
            Msg::SpExec => {
                let r = self.db.sp_exec();
                self.send_exec(r)
            }
            Msg::SpReturnCode => {
                let s = self.db.sp_return_code();
                self.send_string(&s.replace('\n', " "))
            }
            Msg::SpGetParams => match self.db.sp_get_params() {
                Ok(params) => {
                    self.send_ok()?;
                    self.send_list(params.iter().map(|(n, _)| Some(n.as_bytes())))?;
                    self.send_list(params.iter().map(|(_, v)| v.as_deref().map(str::as_bytes)))
                }
                Err(e) => self.send_string(&e.line()),
            },
            Msg::Ping => {
                self.log(Level::Trace, "nspd: responding to ping");
                self.send_ok()
            }
            Msg::TraceOn => self.trace_on(&text),
            Msg::TraceOff => self.trace_off(),
            Msg::OpenF => self.open_file(&text),
            Msg::CloseF => self.close_file(&text),
            Msg::ReadF => self.read_file(&text),
            Msg::WriteF => self.write_file(arg),
            Msg::DeleteF => self.delete_file(&text),
            Msg::CreateTmpF => self.create_tmp_file(),
        }
    }

    /// `PdTraceOn`: tracing stays on even if the file cannot be opened.
    fn trace_on(&mut self, file: &str) -> io::Result<()> {
        self.log(Level::Notice, "PdTraceOn: traceon:");
        self.trace = true;
        self.trace_file = None;
        match File::create(file) {
            Ok(f) => {
                self.trace_file = Some(f);
                self.send_ok()
            }
            Err(e) => {
                let m = format!("Unable to open trace file: {file} ({})", strerror(&e));
                self.log(Level::Error, &m);
                self.send_string(&m)
            }
        }
    }

    /// `PdTraceOff`.
    fn trace_off(&mut self) -> io::Result<()> {
        self.log(Level::Notice, "nspd: traceoff");
        self.trace = false;
        self.trace_file = None;
        self.send_ok()
    }

    fn file_error(&mut self, what: &str, m: String) -> io::Result<()> {
        self.log(Level::Error, &format!("nspd: {what}'{m}'"));
        self.send_string(&m)
    }

    /// `PdOpenFile`: `oflags#path` (`sscanf("%d#%s")`: the path ends at
    /// white space).
    fn open_file(&mut self, params: &str) -> io::Result<()> {
        self.log(Level::Trace, "PdOpenFile:");
        let parsed = msg::scan_int(params).and_then(|(flags, rest)| {
            let path = rest.strip_prefix('#')?.trim_start();
            let path = &path[..path.find(char::is_whitespace).unwrap_or(path.len())];
            (!path.is_empty()).then(|| (flags, path.to_owned()))
        });
        let Some((flags, path)) = parsed else {
            return self.file_error("", format!("Error parsing open parameters: {params}"));
        };
        match open_flags(&path, flags) {
            Ok(f) => {
                let fd = f.as_raw_fd();
                self.files.insert(fd, f);
                self.send_ok()?;
                self.send_string(&fd.to_string())
            }
            Err(e) => self.file_error(
                "",
                format!("Can't open file {path} (oflags={flags}): {}", strerror(&e)),
            ),
        }
    }

    /// `PdCloseFile`.
    fn close_file(&mut self, fd: &str) -> io::Result<()> {
        self.log(Level::Trace, "PdCloseFile:");
        let fd = msg::atoi(fd);
        if i32::try_from(fd)
            .ok()
            .and_then(|fd| self.files.remove(&fd))
            .is_some()
        {
            return self.send_ok();
        }
        let m = format!("Close error on file descriptor {fd}: Bad file descriptor");
        self.log(Level::Error, &format!("nspd: failed closing file '{m}'"));
        self.send_string(&m)
    }

    fn file(&mut self, fd: i64) -> io::Result<&mut File> {
        i32::try_from(fd)
            .ok()
            .and_then(|fd| self.files.get_mut(&fd))
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))
    }

    /// `PdReadFile`: `fd#offset#count`; replies `ok`, then the data as a
    /// one-element list.
    fn read_file(&mut self, params: &str) -> io::Result<()> {
        self.log(Level::Trace, "PdReadFile:");
        let Some((fd, offset, count)) = scan3(params) else {
            return self.file_error(
                "failed reading file: ",
                format!("Error parsing read parameters: {params}"),
            );
        };
        let want = usize::try_from(count).unwrap_or(0).min(READ_MAX);
        let mut buf = vec![0; want];
        let r = self.file(fd).and_then(|f| {
            f.seek(SeekFrom::Start(u64::try_from(offset).unwrap_or(0)))?;
            f.read(&mut buf)
        });
        match r {
            Ok(n) => {
                self.send_ok()?;
                self.send_list([Some(&buf[..n])])
            }
            Err(e) => {
                let m = format!("Read error on fd {fd} ({count} bytes): {}", strerror(&e));
                self.log(Level::Error, &format!("failed reading file: '{m}'"));
                self.send_string(&m)
            }
        }
    }

    /// `PdWriteFile`: `fd#offset#count#data`.
    fn write_file(&mut self, params: &[u8]) -> io::Result<()> {
        self.log(Level::Trace, "PdWriteFile:");
        let head = String::from_utf8_lossy(&params[..params.len().min(128)]).into_owned();
        let Some((fd, offset, count)) = scan3(&head) else {
            return self.file_error(
                "failed writing file: ",
                format!(
                    "Error parsing write parameters: {}",
                    String::from_utf8_lossy(params)
                ),
            );
        };
        // The data follow the third `#`.
        let data = params
            .iter()
            .enumerate()
            .filter(|(_, b)| **b == b'#')
            .nth(2)
            .map_or_else(|| &params[params.len()..], |(i, _)| &params[i + 1..]);
        let count_n = usize::try_from(count).unwrap_or(0);
        let data = &data[..data.len().min(count_n)];
        let r = self.file(fd).and_then(|f| {
            f.seek(SeekFrom::Start(u64::try_from(offset).unwrap_or(0)))?;
            f.write_all(data)?;
            if data.len() == count_n {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(libc::EINVAL))
            }
        });
        match r {
            Ok(()) => self.send_ok(),
            Err(e) => self.file_error(
                "failed writing file: ",
                format!("Write error to fd {fd} ({count} bytes): {}", strerror(&e)),
            ),
        }
    }

    /// `PdDeleteFile`.
    fn delete_file(&mut self, name: &str) -> io::Result<()> {
        self.log(Level::Trace, "PdDeleteFile:");
        match std::fs::remove_file(name) {
            Ok(()) => self.send_ok(),
            Err(e) => self.file_error(
                "failed deleting file: ",
                format!("Can't delete file {name}: {}", strerror(&e)),
            ),
        }
    }

    /// `PdCreateTmpFile`: `<tmpdir>/pdXXXXXX`, created empty (mode 0644).
    fn create_tmp_file(&mut self) -> io::Result<()> {
        self.log(Level::Trace, "PdCreateTmpFile:");
        let made = tempfile::Builder::new()
            .prefix("pd")
            .rand_bytes(6)
            .tempfile_in(&self.opts.tmpdir)
            .and_then(|t| t.keep().map_err(|e| e.error));
        match made {
            Ok((f, path)) => {
                use std::os::unix::fs::PermissionsExt;
                let _ = f.set_permissions(std::fs::Permissions::from_mode(0o644));
                drop(f);
                self.send_ok()?;
                self.send_string(&path.to_string_lossy())
            }
            Err(e) => {
                let m = format!(
                    "open/create of {}pdXXXXXX failed: {}",
                    self.opts.tmpdir.display(),
                    strerror(&e)
                );
                self.send_string(&m)?;
                self.log(
                    Level::Error,
                    &format!("nspd: failed to open/create temp file: '{m}'"),
                );
                Ok(())
            }
        }
    }
}

/// `sscanf("%d#%d#%d")`.
fn scan3(s: &str) -> Option<(i64, i64, i64)> {
    let (a, r) = msg::scan_int(s)?;
    let (b, r) = msg::scan_int(r.strip_prefix('#')?)?;
    let (c, _) = msg::scan_int(r.strip_prefix('#')?)?;
    Some((a, b, c))
}

/// `open(path, oflags, 0)` with the C open flags nsext sends.
fn open_flags(path: &str, flags: i64) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let flags = i32::try_from(flags).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let mut o = std::fs::OpenOptions::new();
    match flags & libc::O_ACCMODE {
        libc::O_WRONLY => o.write(true),
        libc::O_RDWR => o.read(true).write(true),
        _ => o.read(true),
    };
    o.custom_flags(flags & !libc::O_ACCMODE).mode(0o644);
    o.open(path)
}

/// `GetMsg`: read one request.  `None` ends the session (end of input,
/// a read error or a malformed request, which is logged).
fn get_msg<R: BufRead>(
    input: &mut R,
    max: usize,
    log: &dyn Fn(Level, &str),
) -> Option<(String, Vec<u8>)> {
    let mut head = Vec::new();
    let name = match read_line(input, max, &mut head) {
        Ok(Some(n)) => n,
        Ok(None) => return None,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof
            ) =>
        {
            log(
                Level::Error,
                "nspd: protocol error: no newline terminator for command name",
            );
            return None;
        }
        Err(e) => {
            log(
                Level::Error,
                &format!(
                    "nspd: read error while reading {max} bytes: '{}'",
                    strerror(&e)
                ),
            );
            return None;
        }
    };
    let Ok(Some(size)) = read_line(input, max, &mut head) else {
        log(
            Level::Error,
            &format!("nspd: protocol error: no newline terminator for arglen of command: '{name}'"),
        );
        return None;
    };
    let argsize = msg::atoi(&size);
    let room = max.saturating_sub(head.len());
    let Some(n) = usize::try_from(argsize).ok().filter(|&n| n <= room) else {
        log(
            Level::Error,
            &format!(
                "nspd: arglen of {argsize} for {name} command is greater than configured max {max}"
            ),
        );
        return None;
    };
    let mut arg = vec![0; n];
    input.read_exact(&mut arg).ok()?;
    Some((name, arg))
}

/// One `\n`-terminated header line, appended to `head`.  `Ok(None)` at
/// end of input; an error if the line does not fit in `max` bytes.
fn read_line<R: BufRead>(
    input: &mut R,
    max: usize,
    head: &mut Vec<u8>,
) -> io::Result<Option<String>> {
    let start = head.len();
    loop {
        let buf = input.fill_buf()?;
        if buf.is_empty() {
            return if head.len() == start {
                Ok(None)
            } else {
                Err(io::ErrorKind::UnexpectedEof.into())
            };
        }
        let (n, done) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (buf.len(), false),
        };
        head.extend_from_slice(&buf[..n]);
        input.consume(n);
        if head.len() > max {
            return Err(io::ErrorKind::InvalidData.into());
        }
        if done {
            let line = &head[start..head.len() - 1];
            return Ok(Some(String::from_utf8_lossy(line).into_owned()));
        }
    }
}

/// `PdMainLoop`: serve requests from `input`, replying on `output`,
/// until `close`, end of input or a protocol error.  `init` makes the
/// database (`Ns_PdDbInit`); `None` is an initialisation failure.
pub fn main_loop<D: PdDb>(
    input: impl Read,
    output: impl Write,
    init: impl FnOnce() -> Option<D>,
    opts: &Options,
) {
    (opts.log)(Level::Notice, "nspd: starting");
    let Some(db) = init() else {
        (opts.log)(Level::Error, "nspd: initialization failure");
        return;
    };
    let mut input = BufReader::new(input);
    let mut s = Session {
        db,
        out: output,
        opts,
        read_input: true,
        trace: false,
        trace_file: None,
        files: HashMap::new(),
    };
    while s.read_input {
        let log = |l: Level, m: &str| (opts.log)(l, m);
        let Some((cmd, arg)) = get_msg(&mut input, opts.max_request, &log) else {
            break;
        };
        if let Err(e) = s.dispatch(&cmd, &arg).and_then(|()| s.out.flush()) {
            s.log(
                Level::Error,
                &format!("nspd: error '{}' writing reply to '{cmd}'", strerror(&e)),
            );
            break;
        }
    }
    s.db.cleanup();
}

/// Serve each connection accepted on `listener` on its own thread, with
/// a database from `factory` (`PdListen`; AOLserver forked a process per
/// connection).  Returns only if `accept` fails.
pub fn serve<D, F>(listener: &TcpListener, factory: F, opts: &Options) -> io::Result<()>
where
    D: PdDb,
    F: Fn() -> Option<D> + Send + Sync + 'static,
{
    let factory = Arc::new(factory);
    loop {
        let (sock, _) = listener.accept()?;
        let (f, o) = (Arc::clone(&factory), opts.clone());
        std::thread::Builder::new()
            .name("nspd-conn".into())
            .spawn(move || serve_conn(&sock, &*f, &o))?;
    }
}

fn serve_conn<D: PdDb>(sock: &TcpStream, factory: &dyn Fn() -> Option<D>, opts: &Options) {
    match sock.try_clone() {
        Ok(w) => main_loop(sock, w, factory, opts),
        Err(e) => (opts.log)(
            Level::Error,
            &format!("nspd: dup2 failed: {}", strerror(&e)),
        ),
    }
}

/// `PdPort`: a port argument, `None` unless `0 < port < 65535`.
pub fn parse_port(arg: &str) -> Option<u16> {
    let p = msg::atoi(arg);
    (p > 0 && p < 65535)
        .then(|| u16::try_from(p).ok())
        .flatten()
}

#[cfg(test)]
mod tests;
