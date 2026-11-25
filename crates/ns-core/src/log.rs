//! Server logging (`nsd/log.c`) and log rolling (`nsd/rollfile.c`).
//!
//! Lines look like AOLserver's:
//! `[25/Nov/2026:19:15:00][1234.conn:s1:0] Notice: message`.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use crate::localtime::{format_clf_time, LocalTm};

/// Log severities (`Ns_LogSeverity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// Informational.
    Notice,
    /// Something unexpected but recoverable.
    Warning,
    /// An error.
    Error,
    /// A fatal error; the server exits.
    Fatal,
    /// An internal bug.
    Bug,
    /// Debug output (enabled by `logdebug`).
    Debug,
    /// Developer output (enabled by `logdev`).
    Dev,
}

impl Severity {
    /// Parse a severity name as accepted by `ns_log` (case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "notice" => Self::Notice,
            "warning" => Self::Warning,
            "error" => Self::Error,
            "fatal" => Self::Fatal,
            "bug" => Self::Bug,
            "debug" => Self::Debug,
            "dev" => Self::Dev,
            _ => return None,
        })
    }

    /// Display name used in log lines.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Notice => "Notice",
            Self::Warning => "Warning",
            Self::Error => "Error",
            Self::Fatal => "Fatal",
            Self::Bug => "Bug",
            Self::Debug => "Debug",
            Self::Dev => "Dev",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which severities are written (`ns/parameters` `logdebug`, `logdev`,
/// `lognotice`, `debug`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogFilter {
    /// Write `Notice` lines.
    pub notice: bool,
    /// Write `Debug` lines.
    pub debug: bool,
    /// Write `Dev` lines.
    pub dev: bool,
}

impl Default for LogFilter {
    fn default() -> Self {
        Self {
            notice: true,
            debug: false,
            dev: false,
        }
    }
}

impl LogFilter {
    /// Whether `sev` passes this filter.
    pub const fn enabled(self, sev: Severity) -> bool {
        match sev {
            Severity::Notice => self.notice,
            Severity::Debug => self.debug,
            Severity::Dev => self.dev,
            _ => true,
        }
    }
}

/// Format one log line (without the trailing newline).
pub fn format_line(
    tm: &LocalTm,
    usec: Option<u32>,
    pid: u32,
    thread: &str,
    sev: Severity,
    msg: &str,
) -> String {
    let stamp = format_clf_time(tm, false);
    let usec = usec.map(|u| format!(".{u:06}")).unwrap_or_default();
    format!("[{stamp}{usec}][{pid}.{thread}] {sev}: {msg}")
}

enum Sink {
    Stderr,
    File { path: PathBuf, file: File },
    Memory(Vec<String>),
}

/// A thread-safe server log.
pub struct Log {
    filter: Mutex<LogFilter>,
    usec: bool,
    expanded: bool,
    sink: Mutex<Sink>,
}

impl fmt::Debug for Log {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Log")
            .field("usec", &self.usec)
            .finish_non_exhaustive()
    }
}

fn current_thread_name() -> String {
    std::thread::current()
        .name()
        .map_or_else(|| "-thread-".to_owned(), |n| format!("-{n}-"))
}

impl Log {
    /// Log to standard error.
    pub fn stderr() -> Self {
        Self {
            filter: Mutex::new(LogFilter::default()),
            usec: false,
            expanded: false,
            sink: Mutex::new(Sink::Stderr),
        }
    }

    /// Log into memory (for tests).
    pub fn memory() -> Self {
        Self {
            filter: Mutex::new(LogFilter {
                notice: true,
                debug: true,
                dev: true,
            }),
            usec: false,
            expanded: false,
            sink: Mutex::new(Sink::Memory(Vec::new())),
        }
    }

    /// Append to `path`, creating it.
    pub fn file(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            filter: Mutex::new(LogFilter::default()),
            usec: false,
            expanded: false,
            sink: Mutex::new(Sink::File {
                path: path.to_path_buf(),
                file,
            }),
        })
    }

    /// Configure microsecond timestamps and expanded (blank-line separated)
    /// output.
    #[must_use]
    pub fn with_options(mut self, usec: bool, expanded: bool) -> Self {
        self.usec = usec;
        self.expanded = expanded;
        self
    }

    /// Replace the severity filter.
    pub fn set_filter(&self, filter: LogFilter) {
        *self.filter.lock().unwrap_or_else(PoisonError::into_inner) = filter;
    }

    /// Current filter.
    pub fn filter(&self) -> LogFilter {
        *self.filter.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether `sev` would be written.
    pub fn enabled(&self, sev: Severity) -> bool {
        self.filter().enabled(sev)
    }

    /// Write a message (`Ns_Log`).
    pub fn log(&self, sev: Severity, msg: &str) {
        if !self.enabled(sev) {
            return;
        }
        let (tm, usec) = LocalTm::now();
        let line = format_line(
            &tm,
            self.usec.then_some(usec),
            std::process::id(),
            &current_thread_name(),
            sev,
            msg,
        );
        let mut sink = self.sink.lock().unwrap_or_else(PoisonError::into_inner);
        let sep = if self.expanded { "\n\n" } else { "\n" };
        match &mut *sink {
            Sink::Stderr => {
                let _ = write!(io::stderr(), "{line}{sep}");
            }
            Sink::File { file, .. } => {
                let _ = write!(file, "{line}{sep}");
            }
            Sink::Memory(v) => v.push(line),
        }
    }

    /// Lines captured by a memory log.
    pub fn captured(&self) -> Vec<String> {
        match &*self.sink.lock().unwrap_or_else(PoisonError::into_inner) {
            Sink::Memory(v) => v.clone(),
            _ => Vec::new(),
        }
    }

    /// Roll a file-backed log (`ns_logroll`): rename the current file to
    /// `.000` (shifting older backups) and reopen.
    pub fn roll(&self, max_backup: usize) -> io::Result<()> {
        let mut sink = self.sink.lock().unwrap_or_else(PoisonError::into_inner);
        if let Sink::File { path, file } = &mut *sink {
            file.flush()?;
            roll_file(path, max_backup)?;
            *file = OpenOptions::new().create(true).append(true).open(&*path)?;
        }
        Ok(())
    }

    /// Path of a file-backed log.
    pub fn path(&self) -> Option<PathBuf> {
        match &*self.sink.lock().unwrap_or_else(PoisonError::into_inner) {
            Sink::File { path, .. } => Some(path.clone()),
            _ => None,
        }
    }
}

fn numbered(file: &Path, n: usize) -> PathBuf {
    let mut s = file.as_os_str().to_owned();
    s.push(format!(".{n:03}"));
    PathBuf::from(s)
}

/// Roll `file` to `file.000`, shifting `file.NNN` to `file.NNN+1` and
/// discarding the backup at `max` (`Ns_RollFile`).
pub fn roll_file(file: &Path, max: usize) -> io::Result<()> {
    if max > 999 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "max must be < 999",
        ));
    }
    if numbered(file, 0).exists() {
        let mut num = 0;
        while num < max && numbered(file, num).exists() {
            num += 1;
        }
        let last = num.min(max.saturating_sub(1));
        let last_path = numbered(file, last);
        if last_path.exists() {
            fs::remove_file(&last_path)?;
        }
        let mut n = last;
        while n > 0 {
            let from = numbered(file, n - 1);
            if from.exists() {
                fs::rename(&from, numbered(file, n))?;
            }
            n -= 1;
        }
    }
    if file.exists() {
        fs::rename(file, numbered(file, 0))?;
    }
    Ok(())
}

/// Delete all but the `max` most recently modified files whose name starts
/// with the file name of `file` (`Ns_PurgeFiles`).
pub fn purge_files(file: &Path, max: usize) -> io::Result<usize> {
    let dir = file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let tail = file
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid path"))?
        .to_string_lossy()
        .into_owned();
    let mut files = Vec::new();
    for ent in fs::read_dir(dir)? {
        let ent = ent?;
        if ent.file_name().to_string_lossy().starts_with(&tail) {
            let mtime = ent.metadata()?.modified()?;
            files.push((mtime, ent.path()));
        }
    }
    files.sort_by_key(|b| std::cmp::Reverse(b.0));
    let mut removed = 0;
    for (_, p) in files.into_iter().skip(max) {
        fs::remove_file(p)?;
        removed += 1;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_parse_and_filter() {
        assert_eq!(Severity::parse("ERROR"), Some(Severity::Error));
        assert_eq!(Severity::parse("nope"), None);
        let f = LogFilter::default();
        assert!(f.enabled(Severity::Notice));
        assert!(!f.enabled(Severity::Debug));
        assert!(f.enabled(Severity::Fatal));
        assert_eq!(Severity::Warning.to_string(), "Warning");
    }

    #[test]
    fn line_format() {
        let tm = LocalTm {
            year: 2026,
            month: 11,
            day: 5,
            hour: 7,
            min: 8,
            sec: 9,
            gmtoff: 0,
        };
        assert_eq!(
            format_line(&tm, None, 42, "-main-", Severity::Notice, "hi"),
            "[05/Nov/2026:07:08:09][42.-main-] Notice: hi"
        );
        assert_eq!(
            format_line(&tm, Some(12), 1, "t", Severity::Error, "x"),
            "[05/Nov/2026:07:08:09.000012][1.t] Error: x"
        );
    }

    #[test]
    fn memory_log_captures_and_filters() {
        let log = Log::memory();
        log.log(Severity::Notice, "one");
        log.set_filter(LogFilter {
            notice: false,
            ..LogFilter::default()
        });
        log.log(Severity::Notice, "two");
        log.log(Severity::Error, "three");
        let lines = log.captured();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with("Notice: one"));
        assert!(lines[1].ends_with("Error: three"));
    }

    #[test]
    fn rolling() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("server.log");
        let log = Log::file(&path).expect("open");
        log.log(Severity::Notice, "first");
        log.roll(3).expect("roll");
        log.log(Severity::Notice, "second");
        log.roll(3).expect("roll");
        assert!(numbered(&path, 0).exists());
        assert!(numbered(&path, 1).exists());
        let old = fs::read_to_string(numbered(&path, 1)).expect("read");
        assert!(old.contains("first"));
        for _ in 0..5 {
            log.roll(3).expect("roll");
        }
        assert!(!numbered(&path, 3).exists());
        assert!(numbered(&path, 2).exists());
        assert_eq!(log.path(), Some(path.clone()));
        let removed = purge_files(&path, 2).expect("purge");
        assert!(removed >= 1);
        assert!(roll_file(&path, 1000).is_err());
    }
}
