//! # nscgi
//!
//! The AOLserver CGI module (`nscgi/nscgi.c`).  Config
//! (`ns/server/S/module/nscgi`):
//!
//! * `map "METHOD /url ?path?"` — run CGIs for `/url`.  Without a path the
//!   script is found under the page root; with a directory path the first
//!   URL segment after `/url` names the script; with a file path that file
//!   is the script.  The remainder of the URL becomes `PATH_INFO`.
//! * `interps` — section mapping extensions to interpreters
//!   (`.pl /usr/bin/perl`, optionally `"/usr/bin/perl(envsection)"`).
//! * `environment` — section of extra environment variables.
//! * `systemenvironment` — inherit the server's environment.
//! * `limit`, `maxwait` — concurrency limit and wait seconds.
//!
//! Scripts named `nph-*` write the full HTTP response themselves.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ns_core::log::Severity;
use ns_core::{Config, NsSet};
use nsd::conn::lock_set;
use nsd::op::{register_request, RequestEntry};
use nsd::{ConnRef, Global, ModuleContext, NsError, Status};

/// A `map` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Map {
    /// Method.
    pub method: String,
    /// URL prefix.
    pub url: String,
    /// Directory or file, if given.
    pub path: Option<PathBuf>,
}

/// Parse a `map` value (`CgiRegister`).
pub fn parse_map(value: &str) -> Option<Map> {
    let mut it = value.split_whitespace();
    let method = it.next()?.to_owned();
    let url = it.next()?.trim_end_matches('/').to_owned();
    let path = it.next().map(PathBuf::from);
    Some(Map {
        method,
        url: if url.is_empty() { "/".into() } else { url },
        path,
    })
}

/// A resolved CGI invocation (`CgiInit`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Script path on disk.
    pub path: PathBuf,
    /// `SCRIPT_NAME`.
    pub name: String,
    /// `PATH_INFO`.
    pub path_info: String,
    /// Whether the script is non-parsed-header.
    pub nph: bool,
}

/// Resolve which script handles `url` under `map`.
pub fn resolve(map: &Map, url: &str, pageroot: &Path) -> Option<Target> {
    let under = |u: &str| ns_core::pathname::url_to_path(pageroot, u);
    let plen = map.url.len();
    let prefix_match = map.url == "/"
        || (url.starts_with(&map.url) && (url.len() == plen || url.as_bytes()[plen] == b'/'));
    let (path, name, path_info) = if prefix_match && map.url != "/" {
        match &map.path {
            None => {
                let rest = &url[plen..];
                let (script, info) = match rest.trim_start_matches('/').split_once('/') {
                    Some((s, i)) => (s, format!("/{i}")),
                    None => (rest.trim_start_matches('/'), String::new()),
                };
                let name = format!("{}/{script}", map.url);
                (under(&name), name, info)
            }
            Some(p) if p.is_dir() => {
                if url.len() == plen {
                    return None;
                }
                let rest = &url[plen + 1..];
                let (script, info) = match rest.split_once('/') {
                    Some((s, i)) => (s, format!("/{i}")),
                    None => (rest, String::new()),
                };
                (p.join(script), format!("{}/{script}", map.url), info)
            }
            Some(p) if p.is_file() => (p.clone(), map.url.clone(), url[plen..].to_owned()),
            Some(_) => return None,
        }
    } else {
        (under(url), url.to_owned(), String::new())
    };
    if !path.is_file() {
        return None;
    }
    let nph = path
        .file_name()
        .is_some_and(|f| f.to_string_lossy().starts_with("nph-"));
    Some(Target {
        path,
        name,
        path_info,
        nph,
    })
}

/// Module state.
pub struct Cgi {
    global: Arc<Global>,
    interps: Option<NsSet>,
    merge_env: Option<NsSet>,
    sysenv: bool,
    limit: usize,
    maxwait: Duration,
    running: Mutex<usize>,
    cond: Condvar,
}

impl std::fmt::Debug for Cgi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cgi")
            .field("limit", &self.limit)
            .finish_non_exhaustive()
    }
}

/// Build the CGI/1.1 environment for a request.
pub fn environment(conn: &ConnRef, t: &Target) -> Vec<(String, String)> {
    let mut env = Vec::new();
    let mut put = |k: &str, v: &str| env.push((k.to_owned(), v.to_owned()));
    let server = &conn.server;
    let c = conn.conn();
    put("GATEWAY_INTERFACE", "CGI/1.1");
    put(
        "SERVER_SOFTWARE",
        &format!(
            "{}/{}",
            nsd::global::SERVER_NAME,
            nsd::global::SERVER_VERSION
        ),
    );
    put("SERVER_ROOT", &server.global.info.home.to_string_lossy());
    put("DOCUMENT_ROOT", &server.fastpath.pageroot.to_string_lossy());
    put(
        "SERVER_PROTOCOL",
        &format!("HTTP/{}.{}", c.request.major, c.request.minor),
    );
    put("SERVER_NAME", &c.driver.host);
    put("SERVER_PORT", &c.driver.port().to_string());
    put(
        "HTTPS",
        if c.driver.protocol == "https" {
            "on"
        } else {
            "off"
        },
    );
    put("REQUEST_URI", &c.request.url);
    put("SCRIPT_NAME", &t.name);
    put("SCRIPT_FILENAME", &t.path.to_string_lossy());
    put("PATH_INFO", &t.path_info);
    if !t.path_info.is_empty() {
        put(
            "PATH_TRANSLATED",
            &ns_core::pathname::url_to_path(&server.fastpath.pageroot, &t.path_info)
                .to_string_lossy(),
        );
    }
    put("REQUEST_METHOD", &c.request.method);
    put("QUERY_STRING", c.request.query.as_deref().unwrap_or(""));
    put("REMOTE_ADDR", &c.peer.ip().to_string());
    put("REMOTE_HOST", &c.peer.ip().to_string());
    if let Some(u) = &c.auth_user {
        put("AUTH_TYPE", "Basic");
        put("REMOTE_USER", u);
    }
    let h = lock_set(&c.headers);
    put("CONTENT_TYPE", h.iget("content-type").unwrap_or(""));
    put(
        "CONTENT_LENGTH",
        &if c.content.is_empty() {
            String::new()
        } else {
            c.content.len().to_string()
        },
    );
    for (k, v) in h.iter() {
        let key = format!("HTTP_{}", k.to_ascii_uppercase().replace('-', "_"));
        if key != "HTTP_CONTENT_TYPE" && key != "HTTP_CONTENT_LENGTH" && key != "HTTP_AUTHORIZATION"
        {
            put(&key, v.unwrap_or(""));
        }
    }
    env
}

impl Cgi {
    fn acquire(&self) -> bool {
        if self.limit == 0 {
            return true;
        }
        let deadline = Instant::now() + self.maxwait;
        let mut n = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        while *n >= self.limit {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            n = self
                .cond
                .wait_timeout(n, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        *n += 1;
        true
    }

    fn release(&self) {
        if self.limit > 0 {
            *self.running.lock().unwrap_or_else(PoisonError::into_inner) -= 1;
            self.cond.notify_one();
        }
    }

    fn command(&self, conn: &ConnRef, t: &Target) -> Command {
        let interp = self.interps.as_ref().and_then(|set| {
            let ext = t.path.extension()?.to_string_lossy();
            set.iget(&format!(".{ext}")).map(str::to_owned)
        });
        let (exec, interp_env) = match interp {
            Some(i) => match i.split_once('(') {
                Some((prog, rest)) => (
                    Some(prog.trim().to_owned()),
                    Some(rest.trim_end_matches(')').to_owned()),
                ),
                None => (Some(i), None),
            },
            None => (None, None),
        };
        let mut cmd = match &exec {
            Some(prog) => {
                let mut c = Command::new(prog);
                c.arg(&t.path);
                c
            }
            None => Command::new(&t.path),
        };
        if let Some(dir) = t.path.parent() {
            cmd.current_dir(dir);
        }
        if !self.sysenv {
            cmd.env_clear();
            if let Ok(p) = std::env::var("PATH") {
                cmd.env("PATH", p);
            }
        }
        for (k, v) in environment(conn, t) {
            cmd.env(k, v);
        }
        if let Some(env) = &self.merge_env {
            for (k, v) in env.iter() {
                cmd.env(k, v.unwrap_or(""));
            }
        }
        if let Some(sec) = interp_env.and_then(|s| self.global.config.section(&s).cloned()) {
            for (k, v) in sec.iter() {
                cmd.env(k, v.unwrap_or(""));
            }
        }
        if let Some(q) = conn
            .conn()
            .request
            .query
            .as_deref()
            .filter(|q| !q.contains('='))
        {
            for a in q.split('+') {
                cmd.arg(ns_core::urlencode::decode_url(a, None));
            }
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    /// Run a CGI for `conn` (`CgiRequest`).
    pub fn run(&self, conn: &ConnRef, t: &Target) -> std::io::Result<()> {
        if !self.acquire() {
            self.global.log(Severity::Error, "nscgi: wait timeout");
            return conn.return_unavailable();
        }
        let r = self.exec(conn, t);
        self.release();
        r
    }

    fn exec(&self, conn: &ConnRef, t: &Target) -> std::io::Result<()> {
        let mut child = match self.command(conn, t).spawn() {
            Ok(c) => c,
            Err(e) => {
                self.global.log(
                    Severity::Error,
                    &format!("nscgi: could not exec {}: {e}", t.path.display()),
                );
                return conn.return_internal_error();
            }
        };
        let body = conn.conn().content.to_vec();
        let stdin = child.stdin.take();
        let writer = std::thread::spawn(move || {
            if let Some(mut s) = stdin {
                let _ = s.write_all(&body);
            }
        });
        let stderr = child.stderr.take();
        let g = Arc::clone(&self.global);
        let errlog = std::thread::spawn(move || {
            if let Some(e) = stderr {
                for line in BufReader::new(e).lines().map_while(Result::ok) {
                    g.log(Severity::Notice, &format!("nscgi: {line}"));
                }
            }
        });
        let Some(stdout) = child.stdout.take() else {
            return conn.return_internal_error();
        };
        let mut out = BufReader::new(stdout);
        let result = if t.nph {
            Self::copy_raw(conn, &mut out)
        } else {
            Self::copy_parsed(conn, &mut out)
        };
        let _ = writer.join();
        let _ = errlog.join();
        let _ = child.wait();
        result
    }

    fn copy_raw(conn: &ConnRef, out: &mut dyn Read) -> std::io::Result<()> {
        conn.conn_mut().flags.sent_hdrs = true;
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = out.read(&mut buf)?;
            if n == 0 {
                break;
            }
            conn.send(&[&buf[..n]])?;
        }
        conn.close();
        Ok(())
    }

    fn copy_parsed(
        conn: &ConnRef,
        out: &mut BufReader<std::process::ChildStdout>,
    ) -> std::io::Result<()> {
        let mut status = 200u16;
        let hdrs = conn.output_headers();
        let mut line = String::new();
        let mut last: Option<usize> = None;
        loop {
            line.clear();
            if out.read_line(&mut line)? == 0 {
                break;
            }
            let l = line.trim_end_matches(['\r', '\n']);
            if l.is_empty() {
                break;
            }
            if l.starts_with(char::is_whitespace) {
                if let Some(i) = last {
                    let mut h = lock_set(&hdrs);
                    let prev = h.value(i).flatten().unwrap_or("").to_owned();
                    h.put_value(i, Some(&format!("{prev}\n{l}")));
                }
                continue;
            }
            let Some((k, v)) = l.split_once(':') else {
                continue;
            };
            let v = v.trim_start();
            if k.eq_ignore_ascii_case("status") {
                status = v
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(200);
            } else if k.eq_ignore_ascii_case("location") {
                status = 302;
                let loc = if v.starts_with('/') {
                    format!("{}{v}", conn.location())
                } else {
                    v.to_owned()
                };
                last = Some(lock_set(&hdrs).put_str(k, &loc));
            } else if k.eq_ignore_ascii_case("content-type") {
                conn.set_type(Some(v));
                last = None;
            } else {
                last = Some(lock_set(&hdrs).put_str(k, v));
            }
        }
        let ty = conn.conn().content_type.clone();
        conn.set_required_headers(ty.as_deref(), None);
        conn.queue_headers(status);
        let mut buf = vec![0u8; 16 * 1024];
        let mut wrote = false;
        loop {
            let n = out.read(&mut buf)?;
            if n == 0 {
                break;
            }
            conn.write(&buf[..n])?;
            wrote = true;
        }
        if !wrote {
            conn.send(&[])?;
        }
        conn.conn_mut().flags.keepalive = false;
        conn.close();
        Ok(())
    }
}

/// Module init.
pub fn init(ctx: &ModuleContext<'_>) -> Result<(), NsError> {
    let server = ctx
        .server
        .ok_or_else(|| NsError::Config("nscgi must be loaded as a per-server module".into()))?;
    let path = ctx.config_path();
    let c = &ctx.global.config;
    // `interps foo` names section ns/interps/foo; `environment bar` names
    // ns/environment/bar (as in nscgi.c).
    let section = |key: &str, prefix: &str| {
        c.get(&path, key)
            .and_then(|s| c.section(&Config::make_path(None, None, &[prefix, s])))
            .cloned()
    };
    let cgi = Arc::new(Cgi {
        global: Arc::clone(ctx.global),
        interps: section("interps", "interps"),
        merge_env: section("environment", "environment"),
        sysenv: c.bool_or(&path, "systemenvironment", false),
        limit: c.int_or(&path, "limit", 0).max(0) as usize,
        maxwait: Duration::from_secs(c.int_or(&path, "maxwait", 30).max(0) as u64),
        running: Mutex::new(0),
        cond: Condvar::new(),
    });
    let maps: Vec<Map> = c
        .section(&path)
        .map(|s| {
            s.iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case("map"))
                .filter_map(|(_, v)| v.and_then(parse_map))
                .collect()
        })
        .unwrap_or_default();
    for m in maps {
        let cgi = Arc::clone(&cgi);
        let map = m.clone();
        register_request(
            server,
            &m.method,
            &m.url,
            RequestEntry {
                proc_: Arc::new(move |conn| {
                    let url = conn.url();
                    match resolve(&map, &url, &conn.server.fastpath.pageroot) {
                        Some(t) => {
                            let _ = cgi.run(conn, &t);
                        }
                        None => {
                            let _ = conn.return_not_found();
                        }
                    }
                    Status::Ok
                }),
                desc: format!("nscgi {}", m.url),
            },
            ns_core::UrlFlags::INHERIT,
        );
        ctx.global.log(
            Severity::Notice,
            &format!(
                "nscgi: {} {}{}",
                m.method,
                m.url,
                m.path
                    .as_ref()
                    .map(|p| format!(" -> {}", p.display()))
                    .unwrap_or_default()
            ),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps() {
        assert_eq!(
            parse_map("GET /cgi-bin /usr/lib/cgi-bin"),
            Some(Map {
                method: "GET".into(),
                url: "/cgi-bin".into(),
                path: Some("/usr/lib/cgi-bin".into())
            })
        );
        assert_eq!(parse_map("POST /cgi/").map(|m| m.url), Some("/cgi".into()));
        assert_eq!(parse_map("GET"), None);
    }

    #[test]
    fn resolution() {
        let d = tempfile::tempdir().expect("tmp");
        let bin = d.path().join("bin");
        std::fs::create_dir_all(&bin).expect("mkdir");
        std::fs::write(bin.join("s.sh"), "#!/bin/sh\n").expect("w");
        std::fs::write(bin.join("nph-x"), "").expect("w");
        let m = Map {
            method: "GET".into(),
            url: "/cgi-bin".into(),
            path: Some(bin.clone()),
        };
        let t = resolve(&m, "/cgi-bin/s.sh/extra/info", d.path()).expect("target");
        assert_eq!(t.path, bin.join("s.sh"));
        assert_eq!(t.name, "/cgi-bin/s.sh");
        assert_eq!(t.path_info, "/extra/info");
        assert!(!t.nph);
        assert!(resolve(&m, "/cgi-bin/nph-x", d.path()).expect("nph").nph);
        assert!(resolve(&m, "/cgi-bin", d.path()).is_none());
        assert!(resolve(&m, "/cgi-bin/missing", d.path()).is_none());
        let pages = d.path().join("pages");
        std::fs::create_dir_all(pages.join("cgi")).expect("mkdir");
        std::fs::write(pages.join("cgi/p.cgi"), "").expect("w");
        let m2 = Map {
            method: "GET".into(),
            url: "/cgi".into(),
            path: None,
        };
        let t = resolve(&m2, "/cgi/p.cgi/x", &pages).expect("target");
        assert_eq!(t.path, pages.join("cgi/p.cgi"));
        assert_eq!(t.path_info, "/x");
        let f = Map {
            method: "GET".into(),
            url: "/one".into(),
            path: Some(bin.join("s.sh")),
        };
        assert_eq!(
            resolve(&f, "/one/rest", d.path()).expect("file").path_info,
            "/rest"
        );
    }
}
