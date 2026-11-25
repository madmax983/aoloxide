//! # nslog
//!
//! The AOLserver access-log module (`nslog/nslog.c`).  Writes one line per
//! request in Common Log Format, or Combined Log Format (with referer and
//! user agent) when `logcombined` is on:
//!
//! ```text
//! 127.0.0.1 - user [25/Nov/2026:19:15:00 -0800] "GET / HTTP/1.0" 200 1234 "ref" "agent"
//! ```
//!
//! Config (`ns/server/S/module/nslog`): `file` (default `access.log` in the
//! module directory), `formattedtime`, `logcombined`, `logreqtime`,
//! `suppressquery`, `maxbuffer`, `maxbackup`, `rollfmt`, `rolllog`,
//! `rollhour`, `rollonsignal`, `extendedheaders`.  Adds `ns_accesslog`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use ns_core::localtime::{format_clf_time, LocalTm};
use ns_core::log::Severity;
use ns_script::{command, Exception};
use nsd::callbacks::When;
use nsd::conn::lock_set;
use nsd::sched::{Repeat, SchedFlags};
use nsd::{ConnCtx, ModuleContext, NsError};

/// Access-log options.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct LogOptions {
    /// Use `[dd/Mon/yyyy:hh:mm:ss +zzzz]` rather than `[epoch]`.
    pub formatted_time: bool,
    /// Append referer and user agent.
    pub combined: bool,
    /// Append the request processing time.
    pub reqtime: bool,
    /// Log only the URL, not the full request line with query.
    pub suppress_query: bool,
    /// Extra request headers to log.
    pub ext_headers: Vec<String>,
    /// Lines to buffer before writing (0 = write each line).
    pub max_buffer: usize,
    /// Backups to keep when rolling.
    pub max_backup: usize,
    /// `strftime` suffix for rolled files (instead of `.000` numbering).
    pub rollfmt: Option<String>,
}

impl Default for LogOptions {
    fn default() -> Self {
        Self {
            formatted_time: true,
            combined: true,
            reqtime: false,
            suppress_query: false,
            ext_headers: Vec::new(),
            max_buffer: 0,
            max_backup: 100,
            rollfmt: None,
        }
    }
}

/// A request's loggable fields, extracted from a connection.
#[derive(Debug, Clone, Default)]
pub struct Entry {
    /// Client address (or `X-Forwarded-For`).
    pub peer: String,
    /// Authenticated user.
    pub user: Option<String>,
    /// Request time (Unix seconds).
    pub time: i64,
    /// Request line.
    pub line: String,
    /// URL only.
    pub url: String,
    /// Response status.
    pub status: u16,
    /// Body bytes sent.
    pub sent: u64,
    /// Referer header.
    pub referer: Option<String>,
    /// User-Agent header.
    pub agent: Option<String>,
    /// Processing time in seconds.
    pub elapsed: f64,
    /// Extended header values, in `ext_headers` order.
    pub ext: Vec<Option<String>>,
}

/// Format an entry (`LogTrace`).
pub fn format_entry(e: &Entry, o: &LogOptions) -> String {
    let mut s = e.peer.clone();
    match &e.user {
        None => s.push_str(" - - "),
        Some(u) if u.contains(char::is_whitespace) => s.push_str(&format!(" - \"{u}\" ")),
        Some(u) => s.push_str(&format!(" - {u} ")),
    }
    if o.formatted_time {
        s.push_str(&format!(
            "[{}]",
            format_clf_time(&LocalTm::from_unix(e.time), true)
        ));
    } else {
        s.push_str(&format!("[{}]", e.time));
    }
    let req = if o.suppress_query { &e.url } else { &e.line };
    s.push_str(&format!(" \"{req}\" "));
    s.push_str(&format!(
        "{} {} ",
        if e.status == 0 { 200 } else { e.status },
        e.sent
    ));
    if o.combined {
        s.push_str(&format!(
            "\"{}\" \"{}\"",
            e.referer.as_deref().unwrap_or(""),
            e.agent.as_deref().unwrap_or("")
        ));
    }
    if o.reqtime {
        s.push_str(&format!(" {:.6}", e.elapsed));
    }
    for v in &e.ext {
        match v {
            Some(v) => s.push_str(&format!(" \"{v}\"")),
            None => s.push_str(" -"),
        }
    }
    s.push('\n');
    s
}

struct State {
    path: PathBuf,
    file: Option<File>,
    buffer: String,
    lines: usize,
}

/// An open access log.
pub struct AccessLog {
    /// Options.
    pub opts: LogOptions,
    state: Mutex<State>,
}

impl std::fmt::Debug for AccessLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessLog")
            .field("opts", &self.opts)
            .finish_non_exhaustive()
    }
}

impl AccessLog {
    /// Open (append) the log at `path`.
    pub fn open(path: &Path, opts: LogOptions) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            opts,
            state: Mutex::new(State {
                path: path.to_path_buf(),
                file: Some(file),
                buffer: String::new(),
                lines: 0,
            }),
        })
    }

    /// Current log path.
    pub fn path(&self) -> PathBuf {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .path
            .clone()
    }

    fn flush_locked(st: &mut State) -> std::io::Result<()> {
        if st.buffer.is_empty() {
            return Ok(());
        }
        if let Some(f) = st.file.as_mut() {
            f.write_all(st.buffer.as_bytes())?;
            f.flush()?;
        }
        st.buffer.clear();
        st.lines = 0;
        Ok(())
    }

    /// Append a formatted line.
    pub fn write_line(&self, line: &str) -> std::io::Result<()> {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        st.buffer.push_str(line);
        st.lines += 1;
        if self.opts.max_buffer == 0 || st.lines > self.opts.max_buffer {
            Self::flush_locked(&mut st)?;
        }
        Ok(())
    }

    /// Log an entry.
    pub fn log(&self, e: &Entry) -> std::io::Result<()> {
        self.write_line(&format_entry(e, &self.opts))
    }

    /// Flush buffered lines.
    pub fn flush(&self) -> std::io::Result<()> {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        Self::flush_locked(&mut st)
    }

    /// Roll the log (`LogRoll`): rename with `rollfmt` or numbered backups.
    pub fn roll(&self) -> std::io::Result<()> {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        Self::flush_locked(&mut st)?;
        st.file = None;
        let path = st.path.clone();
        if path.exists() {
            if let Some(fmt) = &self.opts.rollfmt {
                let suffix = strftime(ns_core::httptime::now(), fmt);
                let mut dest = path.clone().into_os_string();
                dest.push(format!(".{suffix}"));
                std::fs::rename(&path, dest)?;
            } else {
                ns_core::log::roll_file(&path, self.opts.max_backup.min(999))?;
            }
        }
        st.file = Some(OpenOptions::new().create(true).append(true).open(&path)?);
        Ok(())
    }

    /// Switch to a new file.
    pub fn reopen(&self, path: &Path) -> std::io::Result<()> {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        Self::flush_locked(&mut st)?;
        st.file = Some(OpenOptions::new().create(true).append(true).open(path)?);
        st.path = path.to_path_buf();
        Ok(())
    }

    /// Close (flushing) at shutdown.
    pub fn close(&self) {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = Self::flush_locked(&mut st);
        st.file = None;
    }
}

fn strftime(t: i64, fmt: &str) -> String {
    let tm = LocalTm::from_unix(t);
    // Minimal strftime for roll suffixes.
    let mut out = String::new();
    let mut chars = fmt.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str(&tm.year.to_string()),
            Some('m') => out.push_str(&format!("{:02}", tm.month)),
            Some('d') => out.push_str(&format!("{:02}", tm.day)),
            Some('H') => out.push_str(&format!("{:02}", tm.hour)),
            Some('M') => out.push_str(&format!("{:02}", tm.min)),
            Some('S') => out.push_str(&format!("{:02}", tm.sec)),
            Some('%') => out.push('%'),
            Some(o) => {
                out.push('%');
                out.push(o);
            }
            None => out.push('%'),
        }
    }
    out
}

/// Extract an [`Entry`] from a finished connection.
pub fn entry_for(conn: &ConnCtx, opts: &LogOptions) -> Entry {
    let c = conn.conn();
    let h = lock_set(&c.headers);
    Entry {
        peer: h
            .iget("x-forwarded-for")
            .map_or_else(|| c.peer.ip().to_string(), str::to_owned),
        user: c.auth_user.clone(),
        time: ns_core::httptime::now(),
        line: c.request.line.clone(),
        url: c.request.url.clone(),
        status: c.status,
        sent: c.n_content_sent,
        referer: h.iget("referer").map(str::to_owned),
        agent: h.iget("user-agent").map(str::to_owned),
        elapsed: c.times.run.elapsed().as_secs_f64(),
        ext: opts
            .ext_headers
            .iter()
            .map(|k| h.iget(k).map(str::to_owned))
            .collect(),
    }
}

/// Module init (`Ns_ModuleInit` for nslog).
pub fn init(ctx: &ModuleContext<'_>) -> Result<(), NsError> {
    let server = ctx
        .server
        .ok_or_else(|| NsError::Config("nslog must be loaded as a per-server module".into()))?;
    let path = ctx.config_path();
    let c = &ctx.global.config;
    let file = c.string_or(&path, "file", "access.log");
    let file = if Path::new(&file).is_absolute() {
        PathBuf::from(file)
    } else {
        let dir = server.module_path(Some(ctx.name), &[]);
        std::fs::create_dir_all(&dir)?;
        dir.join(file)
    };
    let opts = LogOptions {
        formatted_time: c.bool_or(&path, "formattedtime", true),
        combined: c.bool_or(&path, "logcombined", true),
        reqtime: c.bool_or(&path, "logreqtime", false),
        suppress_query: c.bool_or(&path, "suppressquery", false),
        ext_headers: c
            .get(&path, "extendedheaders")
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        max_buffer: c.int_or(&path, "maxbuffer", 0).max(0) as usize,
        max_backup: c
            .get_int(&path, "maxbackup")
            .filter(|&n| n >= 1)
            .unwrap_or(100) as usize,
        rollfmt: c.get(&path, "rollfmt").map(str::to_owned),
    };
    let log = Arc::new(AccessLog::open(&file, opts)?);
    let g = Arc::clone(ctx.global);
    let l = Arc::clone(&log);
    nsd::op::register_trace(
        server,
        Arc::new(move |conn| {
            let e = entry_for(conn, &l.opts);
            if let Err(err) = l.log(&e) {
                g.log(
                    Severity::Error,
                    &format!("nslog: failed to flush log: {err}"),
                );
            }
        }),
    );
    if c.bool_or(&path, "rolllog", true) {
        let hour = c
            .get_int(&path, "rollhour")
            .filter(|h| (0..=23).contains(h))
            .unwrap_or(0) as u32;
        let l = Arc::clone(&log);
        let g = Arc::clone(ctx.global);
        ctx.global.sched.schedule(
            Arc::new(move |_| {
                if let Err(e) = l.roll() {
                    g.log(Severity::Error, &format!("nslog: roll failed: {e}"));
                }
            }),
            SchedFlags::default(),
            Repeat::Daily { hour, minute: 0 },
            "nslog roll",
        );
    }
    if c.bool_or(&path, "rollonsignal", false) {
        let l = Arc::clone(&log);
        ctx.global.callbacks.register(
            When::Signal,
            Arc::new(move || {
                let _ = l.roll();
            }),
        );
    }
    let l = Arc::clone(&log);
    ctx.global
        .callbacks
        .register(When::Shutdown, Arc::new(move || l.close()));
    let l = Arc::clone(&log);
    server.add_command(
        "ns_accesslog",
        command(move |_, argv| match argv.get(1).map(String::as_str) {
            Some("file") => {
                if let Some(p) = argv.get(2) {
                    l.reopen(Path::new(p))
                        .map_err(|e| Exception::error(format!("could not open \"{p}\": {e}")))?;
                }
                Ok(l.path().to_string_lossy().into_owned())
            }
            Some("roll") => {
                l.roll()
                    .map_err(|e| Exception::error(format!("roll failed: {e}")))?;
                Ok(String::new())
            }
            Some("flush") => {
                l.flush()
                    .map_err(|e| Exception::error(format!("flush failed: {e}")))?;
                Ok(String::new())
            }
            _ => Err(Exception::error(format!(
                "wrong # args: should be \"{} file ?path? | roll | flush\"",
                argv[0]
            ))),
        }),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> Entry {
        Entry {
            peer: "10.0.0.1".into(),
            user: None,
            time: 0,
            line: "GET /a?b=1 HTTP/1.0".into(),
            url: "/a".into(),
            status: 200,
            sent: 42,
            referer: Some("http://r/".into()),
            agent: Some("curl".into()),
            elapsed: 0.5,
            ext: vec![Some("x".into()), None],
        }
    }

    #[test]
    fn common_and_combined() {
        let o = LogOptions {
            formatted_time: false,
            combined: false,
            ..LogOptions::default()
        };
        let mut e = entry();
        e.ext.clear();
        assert_eq!(
            format_entry(&e, &o),
            "10.0.0.1 - - [0] \"GET /a?b=1 HTTP/1.0\" 200 42 \n"
        );
        let o = LogOptions {
            formatted_time: false,
            suppress_query: true,
            reqtime: true,
            ..LogOptions::default()
        };
        e.user = Some("bob smith".into());
        e.ext = vec![Some("x".into()), None];
        assert_eq!(
            format_entry(&e, &o),
            "10.0.0.1 - \"bob smith\" [0] \"/a\" 200 42 \"http://r/\" \"curl\" 0.500000 \"x\" -\n"
        );
        let o = LogOptions::default();
        let line = format_entry(&entry(), &o);
        assert!(line.contains("/1970:") || line.contains("/1969:"), "{line}");
    }

    #[test]
    fn buffering_and_rolling() {
        let d = tempfile::tempdir().expect("tmp");
        let p = d.path().join("access.log");
        let log = AccessLog::open(
            &p,
            LogOptions {
                max_buffer: 2,
                ..LogOptions::default()
            },
        )
        .expect("open");
        log.write_line("a\n").expect("w");
        log.write_line("b\n").expect("w");
        assert_eq!(std::fs::read_to_string(&p).expect("r"), "");
        log.write_line("c\n").expect("w");
        assert_eq!(std::fs::read_to_string(&p).expect("r"), "a\nb\nc\n");
        log.write_line("d\n").expect("w");
        log.roll().expect("roll");
        assert_eq!(
            std::fs::read_to_string(d.path().join("access.log.000")).expect("r"),
            "a\nb\nc\nd\n"
        );
        log.log(&entry()).expect("log");
        log.close();
        assert!(std::fs::read_to_string(&p)
            .expect("r")
            .starts_with("10.0.0.1"));
        let p2 = d.path().join("other.log");
        log.reopen(&p2).expect("reopen");
        assert_eq!(log.path(), p2);
        let rl = AccessLog::open(
            &p2,
            LogOptions {
                rollfmt: Some("%Y".into()),
                ..LogOptions::default()
            },
        )
        .expect("open");
        rl.write_line("z\n").expect("w");
        rl.roll().expect("roll");
        assert!(std::fs::read_dir(d.path()).expect("dir").any(|e| e
            .expect("e")
            .file_name()
            .to_string_lossy()
            .starts_with("other.log.")));
        assert_eq!(strftime(0, "%%x%q"), "%x%q");
    }
}
