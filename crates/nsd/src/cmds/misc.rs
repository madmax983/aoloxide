//! Miscellaneous commands: logging, time, encoding, HTML helpers, keyed
//! lists, loops, environment, HTTP client, sockets and channels
//! (`nsd/tclmisc.c`, `tclhttp.c`, `tclxkeylist.c`, `tclloop.c`, `tclenv.c`,
//! `tclsock.c`, `tclimg.c`).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ns_core::httptime::{gmtime, http_time, now, parse_http_time};
use ns_core::localtime::LocalTm;
use ns_core::log::Severity;
use ns_core::urlencode::{charset_encoding, decode_url, encode_url};
use ns_core::NsSet;
use ns_script::{
    binary_to_bytes, bytes_to_binary, wrong_args, Command, Exception, Interp, Options, ScriptResult,
};

use super::set::{enter_set, lookup_set};
use super::{bool_str, check_args, cmd, int_arg, server, time_arg};
use crate::conn::{lock_set, set_ref};

/// Commands defined here.
pub fn commands() -> Vec<(&'static str, Command)> {
    vec![
        ("ns_log", cmd(ns_log)),
        ("ns_logctl", cmd(ns_logctl)),
        ("ns_logroll", cmd(ns_logroll)),
        ("ns_time", cmd(ns_time)),
        ("ns_gmtime", cmd(|i, a| tm_cmd(i, a, true))),
        ("ns_localtime", cmd(|i, a| tm_cmd(i, a, false))),
        ("ns_fmttime", cmd(ns_fmttime)),
        ("ns_httptime", cmd(ns_httptime)),
        ("ns_parsehttptime", cmd(ns_parsehttptime)),
        ("ns_urlencode", cmd(|i, a| url_codec(i, a, true))),
        ("ns_urldecode", cmd(|i, a| url_codec(i, a, false))),
        ("ns_quotehtml", cmd(ns_quotehtml)),
        ("ns_striphtml", cmd(ns_striphtml)),
        ("ns_hrefs", cmd(ns_hrefs)),
        ("ns_crypt", cmd(ns_crypt)),
        ("ns_uuencode", cmd(ns_uuencode)),
        ("ns_uudecode", cmd(ns_uudecode)),
        ("ns_rand", cmd(ns_rand)),
        ("ns_guesstype", cmd(ns_guesstype)),
        ("ns_normalizepath", cmd(ns_normalizepath)),
        ("ns_parsequery", cmd(ns_parsequery)),
        ("ns_env", cmd(ns_env)),
        ("env", cmd(ns_env)),
        ("ns_charsets", cmd(ns_charsets)),
        ("ns_encodingforcharset", cmd(ns_encodingforcharset)),
        ("ns_addrbyhost", cmd(ns_addrbyhost)),
        ("ns_hostbyaddr", cmd(ns_hostbyaddr)),
        ("ns_gifsize", cmd(ns_gifsize)),
        ("ns_jpegsize", cmd(ns_jpegsize)),
        ("ns_for", cmd(|i, a| loop_cmd(i, a, "for", 5))),
        ("ns_foreach", cmd(|i, a| loop_cmd(i, a, "foreach", 0))),
        ("ns_while", cmd(|i, a| loop_cmd(i, a, "while", 3))),
        ("ns_loop_ctl", cmd(ns_loop_ctl)),
        ("keylget", cmd(keylget)),
        ("keylset", cmd(keylset)),
        ("keyldel", cmd(keyldel)),
        ("keylkeys", cmd(keylkeys)),
        ("ns_geturl", cmd(ns_geturl)),
        ("ns_http", cmd(ns_http)),
        ("ns_sockopen", cmd(ns_sockopen)),
        ("ns_sockblocking", cmd(|i, a| sock_blocking(i, a, true))),
        ("ns_socknonblocking", cmd(|i, a| sock_blocking(i, a, false))),
        ("ns_socknread", cmd(ns_socknread)),
        ("ns_sockcheck", cmd(ns_sockcheck)),
        ("ns_socklisten", cmd(ns_socklisten)),
        ("ns_sockaccept", cmd(unsupported)),
        ("ns_sockselect", cmd(unsupported)),
        ("ns_sockcallback", cmd(unsupported)),
        ("ns_socklistencallback", cmd(unsupported)),
        ("ns_socketpair", cmd(unsupported)),
        ("ns_chan", cmd(ns_chan)),
        ("ns_share", cmd(super::share::ns_share)),
        ("ns_quewait", cmd(unsupported)),
        ("ns_checkurl", cmd(ns_checkurl)),
        ("ns_puts", cmd(super::adp::ns_adp_puts)),
    ]
}

fn unsupported(_interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    Err(Exception::error(format!(
        "{}: not supported in this release of aoloxide",
        argv.first().map_or("", String::as_str)
    )))
}

fn ns_log(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 3, usize::MAX, 1, "severity string ?string ...?")?;
    let sev = Severity::parse(&argv[1]).ok_or_else(|| {
        Exception::error(format!(
            "unknown severity: \"{}\": should be notice, warning, error, fatal, bug, debug or dev",
            argv[1]
        ))
    })?;
    let msg = argv[2..].join(" ");
    if let Ok(s) = server(interp) {
        s.global.log(sev, &msg);
    } else {
        // No server (an `nsproxy` worker): `Ns_Log` writes to stderr.
        static LOG: OnceLock<ns_core::log::Log> = OnceLock::new();
        LOG.get_or_init(ns_core::log::Log::stderr).log(sev, &msg);
    }
    Ok(String::new())
}

fn ns_logctl(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (_, opt) = super::subcommand(
        argv,
        &[
            "severity", "count", "flush", "hold", "release", "truncate", "get", "peek",
        ],
    )?;
    let srv = server(interp)?;
    if opt != "severity" {
        return Ok(String::new());
    }
    check_args(argv, 3, 4, 2, "severity ?bool?")?;
    let sev = Severity::parse(&argv[2])
        .ok_or_else(|| Exception::error(format!("unknown severity: {}", argv[2])))?;
    let mut f = srv.global.log.filter();
    if let Some(v) = argv.get(3) {
        let on = ns_script::parse_bool(v)?;
        match sev {
            Severity::Notice => f.notice = on,
            Severity::Debug => f.debug = on,
            Severity::Dev => f.dev = on,
            _ => {}
        }
        srv.global.log.set_filter(f);
    }
    Ok(bool_str(f.enabled(sev)))
}

fn ns_logroll(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 1, 1, 1, "")?;
    let srv = server(interp)?;
    srv.global
        .log
        .roll(srv.global.params.log_max_backup)
        .map(|()| String::new())
        .map_err(|e| Exception::error(format!("could not roll server log: {e}")))
}

fn parse_ns_time(s: &str) -> Result<(i64, i64), Exception> {
    if let Some((a, b)) = s.split_once(':') {
        Ok((int_arg(a)?, int_arg(b)?))
    } else {
        Ok((int_arg(s)?, 0))
    }
}

fn fmt_ns_time(sec: i64, usec: i64) -> String {
    let (mut s, mut u) = (sec, usec);
    s += u.div_euclid(1_000_000);
    u = u.rem_euclid(1_000_000);
    if u == 0 {
        s.to_string()
    } else {
        format!("{s}:{u}")
    }
}

fn ns_time(_interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    if argv.len() == 1 {
        return Ok(now().to_string());
    }
    let (_, opt) = super::subcommand(
        argv,
        &[
            "adjust",
            "diff",
            "format",
            "get",
            "incr",
            "make",
            "seconds",
            "microseconds",
        ],
    )?;
    match opt {
        "get" => {
            let d = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            Ok(format!("{}:{}", d.as_secs(), d.subsec_micros()))
        }
        "make" => {
            check_args(argv, 3, 4, 2, "sec ?usec?")?;
            let u = argv.get(3).map_or(Ok(0), |u| int_arg(u))?;
            Ok(fmt_ns_time(int_arg(&argv[2])?, u))
        }
        "incr" => {
            check_args(argv, 4, 5, 2, "time sec ?usec?")?;
            let (s, u) = parse_ns_time(&argv[2])?;
            let du = argv.get(4).map_or(Ok(0), |u| int_arg(u))?;
            Ok(fmt_ns_time(s + int_arg(&argv[3])?, u + du))
        }
        "diff" => {
            check_args(argv, 4, 4, 2, "time1 time2")?;
            let (s1, u1) = parse_ns_time(&argv[2])?;
            let (s2, u2) = parse_ns_time(&argv[3])?;
            Ok(fmt_ns_time(s1 - s2, u1 - u2))
        }
        "adjust" => {
            check_args(argv, 3, 3, 2, "time")?;
            let (s, u) = parse_ns_time(&argv[2])?;
            Ok(fmt_ns_time(s, u))
        }
        "seconds" => {
            check_args(argv, 3, 3, 2, "time")?;
            Ok(parse_ns_time(&argv[2])?.0.to_string())
        }
        "microseconds" => {
            check_args(argv, 3, 3, 2, "time")?;
            Ok(parse_ns_time(&argv[2])?.1.to_string())
        }
        _ => {
            check_args(argv, 3, 3, 2, "time")?;
            let (s, u) = parse_ns_time(&argv[2])?;
            Ok(format!("{s}.{u:06}"))
        }
    }
}

fn day_of_year(year: i64, month: u32, day: u32) -> i64 {
    ns_core::httptime::days_from_civil(year, month, day)
        - ns_core::httptime::days_from_civil(year, 1, 1)
}

fn tm_cmd(interp: &dyn Interp, argv: &[String], gmt: bool) -> ScriptResult {
    check_args(argv, 1, 1, 1, "")?;
    let t = now();
    let (sec, min, hour, mday, mon, year, wday) = if gmt {
        let g = gmtime(t);
        (g.sec, g.min, g.hour, g.day, g.month, g.year, g.wday)
    } else {
        let l = LocalTm::from_unix(t);
        let wday = gmtime(t + l.gmtoff).wday;
        (l.sec, l.min, l.hour, l.day, l.month, l.year, wday)
    };
    Ok(interp.list_from(&[
        sec.to_string(),
        min.to_string(),
        hour.to_string(),
        mday.to_string(),
        (mon - 1).to_string(),
        (year - 1900).to_string(),
        wday.to_string(),
        day_of_year(year, mon, mday).to_string(),
        "0".into(),
    ]))
}

/// `strftime` in local time.
pub fn strftime_local(t: i64, fmt: &str) -> String {
    let Ok(cfmt) = std::ffi::CString::new(fmt) else {
        return String::new();
    };
    // SAFETY: zeroed tm is a valid all-integers struct; localtime_r fills it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let tt = t as libc::time_t;
    // SAFETY: pointers reference live locals.
    unsafe { libc::localtime_r(&raw const tt, &raw mut tm) };
    let mut buf = vec![0u8; 512];
    // SAFETY: buf is writable for its length; cfmt and tm are valid.
    let n = unsafe {
        libc::strftime(
            buf.as_mut_ptr().cast(),
            buf.len(),
            cfmt.as_ptr(),
            &raw const tm,
        )
    };
    buf.truncate(n);
    String::from_utf8_lossy(&buf).into_owned()
}

fn ns_fmttime(_interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 3, 1, "time ?fmt?")?;
    let t = int_arg(&argv[1])?;
    let fmt = argv.get(2).map_or("%c", String::as_str);
    Ok(strftime_local(t, fmt))
}

fn ns_httptime(_interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 2, 1, "time")?;
    Ok(http_time(int_arg(&argv[1])?))
}

fn ns_parsehttptime(_interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 2, 1, "httptime")?;
    parse_http_time(&argv[1])
        .map(|t| t.to_string())
        .ok_or_else(|| Exception::error(format!("invalid time: {}", argv[1])))
}

fn url_codec(_interp: &dyn Interp, argv: &[String], encode: bool) -> ScriptResult {
    let mut o = Options::new(argv, 1);
    let mut cs = None;
    while let Some(flag) = o.next_flag() {
        if flag == "-charset" {
            cs = Some(o.value(&flag)?);
        } else {
            return Err(wrong_args(argv, 1, "?-charset charset? string ?string...?"));
        }
    }
    let rest = o.rest();
    if rest.is_empty() {
        return Err(wrong_args(argv, 1, "?-charset charset? string ?string...?"));
    }
    let enc = cs.as_deref().and_then(charset_encoding);
    let parts: Vec<String> = rest
        .iter()
        .map(|s| {
            if encode {
                encode_url(s, enc)
            } else {
                decode_url(s, enc)
            }
        })
        .collect();
    Ok(parts.join(if encode { "/" } else { "" }))
}

fn ns_quotehtml(_interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 2, 1, "html")?;
    Ok(ns_core::text::quote_html(&argv[1]))
}

/// Remove HTML tags and decode common entities (`ns_striphtml`).
pub fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            '&' if !in_tag => {
                let mut ent = String::new();
                while let Some(&n) = chars.peek() {
                    if n == ';' || ent.len() > 8 || n.is_whitespace() {
                        break;
                    }
                    ent.push(n);
                    chars.next();
                }
                if chars.peek() == Some(&';') {
                    chars.next();
                }
                let rep = match ent.as_str() {
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "amp" => Some('&'),
                    "quot" => Some('"'),
                    "nbsp" => Some(' '),
                    e if e.starts_with('#') => e[1..].parse::<u32>().ok().and_then(char::from_u32),
                    _ => None,
                };
                if let Some(r) = rep {
                    out.push(r);
                }
            }
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn ns_striphtml(_interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 2, 1, "html")?;
    Ok(strip_html(&argv[1]))
}

/// Extract `href` attribute values from anchor tags (`ns_hrefs`).
pub fn hrefs(html: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(i) = lower[pos..].find('<').map(|i| pos + i) {
        let Some(j) = lower[i..].find('>').map(|j| i + j) else {
            break;
        };
        let tag = &lower[i + 1..j];
        if tag.starts_with('a') && tag[1..].starts_with(char::is_whitespace) {
            if let Some(h) = tag.find("href") {
                let rest = html[i + 1 + h + 4..j].trim_start();
                if let Some(v) = rest.strip_prefix('=') {
                    let v = v.trim_start();
                    let val = if let Some(q) = v.strip_prefix('"') {
                        q.split('"').next().unwrap_or("")
                    } else if let Some(q) = v.strip_prefix('\'') {
                        q.split('\'').next().unwrap_or("")
                    } else {
                        v.split_whitespace().next().unwrap_or("")
                    };
                    out.push(val.to_owned());
                }
            }
        }
        pos = j + 1;
    }
    out
}

fn ns_hrefs(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 2, 1, "html")?;
    Ok(interp.list_from(&hrefs(&argv[1])))
}

fn ns_crypt(_interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 3, 3, 1, "key salt")?;
    Ok(ns_core::crypt::crypt(&argv[1], &argv[2]))
}

fn ns_uuencode(_interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 2, 1, "string")?;
    Ok(ns_core::text::htuu_encode(&binary_to_bytes(&argv[1])))
}

fn ns_uudecode(_interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 2, 1, "string")?;
    let b = ns_core::text::htuu_decode(&argv[1]);
    Ok(String::from_utf8(b.clone()).unwrap_or_else(|_| bytes_to_binary(&b)))
}

/// A fast non-cryptographic random number (xorshift64*).
pub fn rand_u64() -> u64 {
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut s = STATE.load(Ordering::Relaxed);
    if s == 0 {
        s = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
            | 1;
    }
    // xorshift64*
    s ^= s >> 12;
    s ^= s << 25;
    s ^= s >> 27;
    STATE.store(s, Ordering::Relaxed);
    s.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

fn ns_rand(_interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 1, 2, 1, "?maximum?")?;
    let r = rand_u64();
    match argv.get(1) {
        Some(m) => {
            let max = int_arg(m)?;
            if max <= 0 {
                return Err(Exception::error(format!(
                    "invalid max \"{m}\": must be > 0"
                )));
            }
            Ok((r % max as u64).to_string())
        }
        None => Ok(format!("{}", (r >> 11) as f64 / (1u64 << 53) as f64)),
    }
}

fn ns_guesstype(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 2, 1, "filename")?;
    Ok(server(interp)?
        .global
        .mime
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .guess(&argv[1])
        .to_owned())
}

fn ns_normalizepath(_interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 2, 1, "path")?;
    Ok(ns_core::pathname::normalize_path(&argv[1]))
}

fn ns_parsequery(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 2, 1, "querystring")?;
    let mut set = NsSet::default();
    ns_core::form::parse_query_into(&mut set, &argv[1], None);
    enter_set(interp, set_ref(set), true, false)
}

fn ns_env(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (_, opt) = super::subcommand(argv, &["exists", "names", "get", "set", "unset"])?;
    let srv = server(interp)?;
    let _g = srv
        .global
        .env_lock
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    match opt {
        "names" => Ok(interp.list_from(
            &std::env::vars_os()
                .map(|(k, _)| k.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
        )),
        "exists" => {
            check_args(argv, 3, 3, 2, "name")?;
            Ok(bool_str(std::env::var_os(&argv[2]).is_some()))
        }
        "get" => {
            let nocomplain = argv.get(2).is_some_and(|a| a == "-nocomplain");
            let name = argv
                .get(if nocomplain { 3 } else { 2 })
                .ok_or_else(|| wrong_args(argv, 2, "?-nocomplain? name"))?;
            match std::env::var(name) {
                Ok(v) => Ok(v),
                Err(_) if nocomplain => Ok(String::new()),
                Err(_) => Err(Exception::error(format!(
                    "no such environment variable: {name}"
                ))),
            }
        }
        "set" => {
            check_args(argv, 4, 4, 2, "name value")?;
            // SAFETY: serialised by env_lock; AOLserver mutates the
            // environment the same way.
            unsafe { std::env::set_var(&argv[2], &argv[3]) };
            Ok(String::new())
        }
        _ => {
            let nocomplain = argv.get(2).is_some_and(|a| a == "-nocomplain");
            let name = argv
                .get(if nocomplain { 3 } else { 2 })
                .ok_or_else(|| wrong_args(argv, 2, "?-nocomplain? name"))?;
            if std::env::var_os(name).is_none() && !nocomplain {
                return Err(Exception::error(format!(
                    "no such environment variable: {name}"
                )));
            }
            // SAFETY: serialised by env_lock.
            unsafe { std::env::remove_var(name) };
            Ok(String::new())
        }
    }
}

fn ns_charsets(interp: &dyn Interp, _argv: &[String]) -> ScriptResult {
    let names = [
        "utf-8",
        "iso-8859-1",
        "iso-8859-2",
        "iso-8859-5",
        "iso-8859-7",
        "iso-8859-15",
        "windows-1250",
        "windows-1251",
        "windows-1252",
        "shift_jis",
        "euc-jp",
        "iso-2022-jp",
        "euc-kr",
        "gb2312",
        "big5",
        "koi8-r",
    ];
    Ok(interp.list_join(&names))
}

fn ns_encodingforcharset(_interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 2, 1, "charset")?;
    let cs = argv[1].trim().to_ascii_lowercase();
    if cs == "utf-8" || cs == "utf8" {
        return Ok("utf-8".into());
    }
    Ok(charset_encoding(&cs)
        .map(|e| match e.name() {
            "windows-1252" if cs.contains("8859-1") || cs == "latin1" => "iso8859-1".to_owned(),
            n => n.to_ascii_lowercase().replace("iso-", "iso"),
        })
        .unwrap_or_default())
}

fn ns_addrbyhost(_interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let all = argv.get(1).is_some_and(|a| a == "-all");
    let host = argv
        .get(if all { 2 } else { 1 })
        .ok_or_else(|| wrong_args(argv, 1, "?-all? host"))?;
    let addrs = ns_core::dns::addrs_by_host(host);
    if addrs.is_empty() {
        return Err(Exception::error(format!("could not lookup {host}")));
    }
    Ok(if all {
        addrs.join(" ")
    } else {
        addrs[0].clone()
    })
}

fn ns_hostbyaddr(_interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 2, 1, "address")?;
    ns_core::dns::host_by_addr(&argv[1])
        .ok_or_else(|| Exception::error(format!("could not lookup {}", argv[1])))
}

/// Width and height of a GIF.
pub fn gif_size(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 10 || !b.starts_with(b"GIF") {
        return None;
    }
    Some((
        u32::from(u16::from_le_bytes([b[6], b[7]])),
        u32::from(u16::from_le_bytes([b[8], b[9]])),
    ))
}

/// Width and height of a JPEG.
pub fn jpeg_size(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 4 || b[0] != 0xff || b[1] != 0xd8 {
        return None;
    }
    let mut i = 2;
    while i + 9 < b.len() {
        if b[i] != 0xff {
            i += 1;
            continue;
        }
        let marker = b[i + 1];
        let len = usize::from(u16::from_be_bytes([b[i + 2], b[i + 3]]));
        if (0xc0..=0xcf).contains(&marker) && marker != 0xc4 && marker != 0xc8 && marker != 0xcc {
            let h = u32::from(u16::from_be_bytes([b[i + 5], b[i + 6]]));
            let w = u32::from(u16::from_be_bytes([b[i + 7], b[i + 8]]));
            return Some((w, h));
        }
        i += 2 + len;
    }
    None
}

fn img(argv: &[String], f: fn(&[u8]) -> Option<(u32, u32)>, kind: &str) -> ScriptResult {
    check_args(argv, 2, 2, 1, "file")?;
    let data = std::fs::read(&argv[1])
        .map_err(|e| Exception::error(format!("could not read \"{}\": {e}", argv[1])))?;
    f(&data)
        .map(|(w, h)| format!("{w} {h}"))
        .ok_or_else(|| Exception::error(format!("invalid {kind} file: {}", argv[1])))
}

fn ns_gifsize(_interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    img(argv, gif_size, "gif")
}

fn ns_jpegsize(_interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    img(argv, jpeg_size, "jpeg")
}

fn loop_cmd(interp: &dyn Interp, argv: &[String], tcl: &str, n: usize) -> ScriptResult {
    if n > 0 && argv.len() != n {
        return Err(wrong_args(
            argv,
            1,
            if tcl == "for" {
                "start test next command"
            } else {
                "test command"
            },
        ));
    }
    let mut words: Vec<&str> = vec![tcl];
    words.extend(argv[1..].iter().map(String::as_str));
    interp.eval_words(&words)
}

fn ns_loop_ctl(_interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (_, opt) = super::subcommand(
        argv,
        &[
            "list", "info", "cancel", "eval", "install", "pause", "resume",
        ],
    )?;
    if opt == "list" {
        Ok(String::new())
    } else {
        Err(Exception::error(format!(
            "no such loop id: {}",
            argv.get(2).map_or("", String::as_str)
        )))
    }
}

// ---- TclX keyed lists ----

fn keyl_find(interp: &dyn Interp, list: &str, key: &str) -> Result<Option<String>, Exception> {
    let (head, rest) = key
        .split_once('.')
        .map_or((key, None), |(h, r)| (h, Some(r)));
    for e in interp.list_split(list)? {
        let kv = interp.list_split(&e)?;
        if kv.len() == 2 && kv[0] == head {
            return match rest {
                Some(r) => keyl_find(interp, &kv[1], r),
                None => Ok(Some(kv[1].clone())),
            };
        }
    }
    Ok(None)
}

fn keyl_set(interp: &dyn Interp, list: &str, key: &str, value: &str) -> Result<String, Exception> {
    let (head, rest) = key
        .split_once('.')
        .map_or((key, None), |(h, r)| (h, Some(r)));
    let mut items = interp.list_split(list)?;
    let mut found = false;
    for e in &mut items {
        let kv = interp.list_split(e)?;
        if kv.len() == 2 && kv[0] == head {
            let v = match rest {
                Some(r) => keyl_set(interp, &kv[1], r, value)?,
                None => value.to_owned(),
            };
            *e = interp.list_join(&[head, &v]);
            found = true;
            break;
        }
    }
    if !found {
        let v = match rest {
            Some(r) => keyl_set(interp, "", r, value)?,
            None => value.to_owned(),
        };
        items.push(interp.list_join(&[head, &v]));
    }
    Ok(interp.list_from(&items))
}

fn keylget(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 4, 1, "listvar ?key? ?retvar?")?;
    let list = interp.get_var(&argv[1]).unwrap_or_default();
    let Some(key) = argv.get(2) else {
        let keys: Vec<String> = interp
            .list_split(&list)?
            .iter()
            .filter_map(|e| interp.list_split(e).ok().and_then(|kv| kv.first().cloned()))
            .collect();
        return Ok(interp.list_from(&keys));
    };
    let v = keyl_find(interp, &list, key)?;
    match argv.get(3) {
        Some(var) => {
            if let Some(v) = &v {
                if !var.is_empty() {
                    interp.set_var(var, v)?;
                }
            }
            Ok(bool_str(v.is_some()))
        }
        None => v.ok_or_else(|| Exception::error(format!("key \"{key}\" not found in keyed list"))),
    }
}

fn keylset(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    if argv.len() < 4 || argv.len() % 2 != 0 {
        return Err(wrong_args(argv, 1, "listvar key value ?key value...?"));
    }
    let mut list = interp.get_var(&argv[1]).unwrap_or_default();
    for kv in argv[2..].chunks(2) {
        list = keyl_set(interp, &list, &kv[0], &kv[1])?;
    }
    interp.set_var(&argv[1], &list)?;
    Ok(String::new())
}

fn keyldel(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 3, 3, 1, "listvar key")?;
    let list = interp.get_var(&argv[1]).unwrap_or_default();
    let items: Vec<String> = interp
        .list_split(&list)?
        .into_iter()
        .filter(|e| {
            interp
                .list_split(e)
                .ok()
                .and_then(|kv| kv.first().cloned())
                .as_deref()
                != Some(argv[2].as_str())
        })
        .collect();
    interp.set_var(&argv[1], &interp.list_from(&items))?;
    Ok(String::new())
}

fn keylkeys(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 3, 1, "listvar ?key?")?;
    let mut list = interp.get_var(&argv[1]).unwrap_or_default();
    if let Some(k) = argv.get(2) {
        list = keyl_find(interp, &list, k)?
            .ok_or_else(|| Exception::error(format!("key \"{k}\" not found")))?;
    }
    let keys: Vec<String> = interp
        .list_split(&list)?
        .iter()
        .filter_map(|e| interp.list_split(e).ok().and_then(|kv| kv.first().cloned()))
        .collect();
    Ok(interp.list_from(&keys))
}

// ---- HTTP client (ns_geturl, ns_http) ----

/// A completed HTTP client response.
#[derive(Debug, Clone, Default)]
pub struct HttpResponse {
    /// Status code.
    pub status: u16,
    /// Response headers.
    pub headers: NsSet,
    /// Body.
    pub body: Vec<u8>,
}

/// Perform a blocking HTTP/1.0 request (plain HTTP only).
pub fn http_request(
    method: &str,
    url: &str,
    headers: &NsSet,
    body: &[u8],
    timeout: Duration,
) -> Result<HttpResponse, Exception> {
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        Exception::error(format!("invalid url \"{url}\": only http:// is supported"))
    })?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let addr = if hostport.contains(':') {
        hostport.to_owned()
    } else {
        format!("{hostport}:80")
    };
    let sa = addr
        .to_socket_addrs()
        .map_err(|e| Exception::error(format!("could not resolve {hostport}: {e}")))?
        .next()
        .ok_or_else(|| Exception::error(format!("could not resolve {hostport}")))?;
    let mut s = TcpStream::connect_timeout(&sa, timeout)
        .map_err(|e| Exception::error(format!("connect to {addr} failed: {e}")))?;
    let _ = s.set_read_timeout(Some(timeout));
    let _ = s.set_write_timeout(Some(timeout));
    let mut req = format!("{method} {path} HTTP/1.0\r\n");
    if headers.ifind("host").is_none() {
        req.push_str(&format!("Host: {hostport}\r\n"));
    }
    if headers.ifind("user-agent").is_none() {
        req.push_str(&format!(
            "User-Agent: {}/{}\r\n",
            crate::global::SERVER_NAME,
            crate::global::SERVER_VERSION
        ));
    }
    for (k, v) in headers.iter() {
        req.push_str(&format!("{k}: {}\r\n", v.unwrap_or("")));
    }
    if !body.is_empty() && headers.ifind("content-length").is_none() {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    let io = |e: std::io::Error| Exception::error(format!("http i/o error: {e}"));
    s.write_all(req.as_bytes()).map_err(io)?;
    s.write_all(body).map_err(io)?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).map_err(io)?;
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, i + 4))
        .or_else(|| {
            raw.windows(2)
                .position(|w| w == b"\n\n")
                .map(|i| (i, i + 2))
        })
        .ok_or_else(|| Exception::error("invalid http response"))?;
    let head = String::from_utf8_lossy(&raw[..split.0]).into_owned();
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Exception::error(format!("invalid http status line: {status_line}")))?;
    let mut hdrs = NsSet::named("headers");
    for l in lines {
        let _ =
            ns_core::request::parse_header(&mut hdrs, l, ns_core::request::HeaderCase::Preserve);
    }
    Ok(HttpResponse {
        status,
        headers: hdrs,
        body: raw[split.1..].to_vec(),
    })
}

fn body_string(b: &[u8]) -> String {
    String::from_utf8(b.to_vec()).unwrap_or_else(|_| bytes_to_binary(b))
}

fn ns_geturl(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 3, 1, "url ?headersSetIdVar?")?;
    let mut url = argv[1].clone();
    if url.starts_with('/') {
        if let Ok(c) = super::conn(interp) {
            url = format!("{}{url}", c.location());
        }
    }
    let mut resp = http_request("GET", &url, &NsSet::default(), &[], Duration::from_secs(30))?;
    for _ in 0..5 {
        if !(301..=303).contains(&resp.status) {
            break;
        }
        let Some(loc) = resp.headers.iget("location").map(str::to_owned) else {
            break;
        };
        resp = http_request("GET", &loc, &NsSet::default(), &[], Duration::from_secs(30))?;
    }
    if let Some(var) = argv.get(2) {
        let id = enter_set(interp, set_ref(resp.headers.clone()), true, false)?;
        interp.set_var(var, &id)?;
    }
    Ok(body_string(&resp.body))
}

type HttpJob = JoinHandle<Result<HttpResponse, Exception>>;

fn http_jobs() -> &'static Mutex<HashMap<String, HttpJob>> {
    static J: OnceLock<Mutex<HashMap<String, HttpJob>>> = OnceLock::new();
    J.get_or_init(|| Mutex::new(HashMap::new()))
}

fn ns_http(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let (_, opt) = super::subcommand(argv, &["cancel", "cleanup", "queue", "run", "wait"])?;
    match opt {
        "cleanup" => Ok(String::new()),
        "cancel" => {
            check_args(argv, 3, 3, 2, "id")?;
            http_jobs()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&argv[2]);
            Ok(String::new())
        }
        "queue" | "run" => {
            let mut o = Options::new(argv, 2);
            let mut timeout = Duration::from_secs(30);
            let mut body = Vec::new();
            let mut headers = NsSet::default();
            while let Some(flag) = o.next_flag() {
                let v = o.value(&flag)?;
                match flag.as_str() {
                    "-timeout" => timeout = time_arg(&v)?,
                    "-body" => body = binary_to_bytes(&v),
                    "-headers" => headers = lock_set(&lookup_set(interp, &v, false)?).clone(),
                    _ => {
                        return Err(wrong_args(
                            argv,
                            2,
                            "?-timeout t? ?-body b? ?-headers set? method url",
                        ))
                    }
                }
            }
            let rest = o.rest();
            if rest.len() < 2 {
                return Err(wrong_args(argv, 2, "?flags? method url ?body?"));
            }
            let method = rest[0].to_ascii_uppercase();
            let url = rest[1].clone();
            if let Some(b) = rest.get(2) {
                body = binary_to_bytes(b);
            }
            let h =
                std::thread::spawn(move || http_request(&method, &url, &headers, &body, timeout));
            if opt == "run" {
                let r = h
                    .join()
                    .map_err(|_| Exception::error("http thread panicked"))??;
                return Ok(body_string(&r.body));
            }
            let id = format!("http{}", NEXT.fetch_add(1, Ordering::Relaxed));
            http_jobs()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(id.clone(), h);
            Ok(id)
        }
        _ => {
            let mut o = Options::new(argv, 2);
            let (mut result_var, mut status_var, mut headers_set) = (None, None, None);
            while let Some(flag) = o.next_flag() {
                let v = o.value(&flag)?;
                match flag.as_str() {
                    "-result" => result_var = Some(v),
                    "-status" => status_var = Some(v),
                    "-headers" => headers_set = Some(v),
                    "-timeout" | "-elapsed" | "-file" => {}
                    _ => {
                        return Err(wrong_args(
                            argv,
                            2,
                            "?-result var? ?-status var? ?-headers set? id",
                        ))
                    }
                }
            }
            let id = o.rest().first().ok_or_else(|| wrong_args(argv, 2, "id"))?;
            let h = http_jobs()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(id)
                .ok_or_else(|| Exception::error(format!("no such request: {id}")))?;
            let r = h
                .join()
                .map_err(|_| Exception::error("http thread panicked"))??;
            if let Some(s) = headers_set {
                let set = lookup_set(interp, &s, false)?;
                lock_set(&set).merge(&r.headers);
            }
            if let Some(v) = status_var {
                interp.set_var(&v, &r.status.to_string())?;
            }
            let body = body_string(&r.body);
            match result_var {
                Some(v) => {
                    interp.set_var(&v, &body)?;
                    Ok("1".into())
                }
                None => Ok(body),
            }
        }
    }
}

// ---- sockets & channels (via the engine's own socket channels) ----

fn ns_sockopen(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let mut a: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
    let nonblock = a.first() == Some(&"-nonblock");
    if nonblock {
        a.remove(0);
    }
    let mut timeout = None;
    if a.first() == Some(&"-timeout") && a.len() > 1 {
        timeout = Some(a[1].to_owned());
        a.drain(..2);
    }
    if a.len() != 2 {
        return Err(wrong_args(
            argv,
            1,
            "?-nonblock|-timeout seconds? host port",
        ));
    }
    let _ = timeout;
    let mut words = vec!["socket"];
    if nonblock {
        words.push("-async");
    }
    words.push(a[0]);
    words.push(a[1]);
    let chan = interp.eval_words(&words)?;
    Ok(interp.list_join(&[&chan, &chan]))
}

fn sock_blocking(interp: &dyn Interp, argv: &[String], blocking: bool) -> ScriptResult {
    check_args(argv, 2, 2, 1, "sockId")?;
    interp.eval_words(&[
        "fconfigure",
        &argv[1],
        "-blocking",
        if blocking { "1" } else { "0" },
    ])?;
    Ok(String::new())
}

fn ns_socknread(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 2, 1, "sockId")?;
    interp.eval_words(&["chan", "pending", "input", &argv[1]])
}

fn ns_sockcheck(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 2, 1, "sockId")?;
    let err = interp.eval_words(&["fconfigure", &argv[1], "-error"])?;
    Ok(bool_str(err.is_empty()))
}

fn ns_socklisten(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 3, 3, 1, "address port")?;
    let _ = interp;
    Err(Exception::error(
        "ns_socklisten: use the engine's [socket -server] with a callback; blocking listen sockets are not supported",
    ))
}

/// `ns_chan cleanup|list|create|put|get` (`NsTclChanObjCmd`): move
/// channels between interps and threads through the server's shared
/// table.  See `verus/chan.rs` for the ownership invariant.
fn ns_chan(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    if argv.len() < 2 {
        return Err(wrong_args(argv, 1, "command ?args?"));
    }
    let opts = ["cleanup", "list", "create", "put", "get"];
    let opt = opts[ns_script::get_index(&argv[1], &opts, "option")?];
    let srv = server(interp)?;
    let st = crate::interp::ns_interp(interp)?;
    let lock_chans = || srv.chans.lock().unwrap_or_else(PoisonError::into_inner);
    match opt {
        "create" => {
            if argv.len() != 4 {
                return Err(wrong_args(argv, 1, "create channel name"));
            }
            if interp.channel_is_shared(&argv[2])? {
                return Err(Exception::error("channel is shared"));
            }
            let mut table = lock_chans();
            if table.contains_key(&argv[3]) {
                return Err(Exception::error(format!(
                    "channel with name \"{}\" already exists",
                    argv[3]
                )));
            }
            let det = interp.channel_detach(&argv[2])?;
            table.insert(argv[3].clone(), det);
            Ok(String::new())
        }
        "get" => {
            if argv.len() != 3 {
                return Err(wrong_args(argv, 1, "get name"));
            }
            let name = &argv[2];
            let det = lock_chans()
                .remove(name)
                .ok_or_else(|| Exception::error(format!("no such shared channel: {name}")))?;
            let id = det.id();
            let cname = interp.channel_attach(det)?;
            st.borrow_mut().chans.insert(
                name.clone(),
                crate::interp::RegChan {
                    name: cname.clone(),
                    id,
                },
            );
            Ok(cname)
        }
        "put" => {
            if argv.len() != 3 {
                return Err(wrong_args(argv, 1, "put name"));
            }
            let name = &argv[2];
            let reg = st
                .borrow()
                .chans
                .get(name)
                .cloned()
                .ok_or_else(|| Exception::error(format!("no such shared channel: {name}")))?;
            if interp.channel_id(&reg.name).ok() != Some(reg.id) {
                st.borrow_mut().chans.remove(name);
                return Err(Exception::error("channel mismatch"));
            }
            // Deviation: a channel registered in another interp too (e.g.
            // with `interp share`) is not cut from under it.
            if interp.channel_is_shared(&reg.name)? {
                return Err(Exception::error("channel is shared"));
            }
            let det = interp.channel_detach(&reg.name)?;
            st.borrow_mut().chans.remove(name);
            lock_chans().insert(name.clone(), det);
            Ok(String::new())
        }
        _ => {
            if argv.len() != 2 && argv.len() != 3 {
                return Err(wrong_args(argv, 1, &format!("{opt} ?-shared?")));
            }
            let shared = argv.len() == 3;
            if opt == "list" {
                let names: Vec<String> = if shared {
                    lock_chans().keys().cloned().collect()
                } else {
                    st.borrow().chans.keys().cloned().collect()
                };
                return Ok(interp.list_from(&names));
            }
            if shared {
                let all = std::mem::take(&mut *lock_chans());
                for det in all.into_values() {
                    interp.channel_close_detached(det);
                }
            } else {
                let all = std::mem::take(&mut st.borrow_mut().chans);
                for reg in all.into_values() {
                    interp.channel_unregister(&reg.name, reg.id);
                }
            }
            Ok(String::new())
        }
    }
}

fn ns_checkurl(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 3, 3, 1, "method url")?;
    let srv = server(interp)?;
    Ok(bool_str(
        crate::op::get_request(&srv, &argv[1], &argv[2]).is_some(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_and_hrefs() {
        assert_eq!(strip_html("<b>a &amp; b</b>&lt;&#65;"), "a & b<A");
        assert_eq!(
            hrefs(r#"<a href="x.html">x</a> <A HREF='y'>y</A> <img src=z> <a  href = w >"#),
            vec!["x.html", "y", "w"]
        );
    }

    #[test]
    fn ns_time_math() {
        assert_eq!(fmt_ns_time(1, 1_500_000), "2:500000");
        assert_eq!(fmt_ns_time(5, -1), "4:999999");
        assert_eq!(parse_ns_time("3:4"), Ok((3, 4)));
        assert_eq!(day_of_year(2024, 3, 1), 60);
    }

    #[test]
    fn images() {
        let gif = b"GIF89a\x10\x00\x20\x00rest";
        assert_eq!(gif_size(gif), Some((16, 32)));
        assert_eq!(gif_size(b"nope"), None);
        let jpeg = [
            0xff, 0xd8, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00, 0x40, 0x00, 0x80, 0x03, 0, 0, 0,
        ];
        assert_eq!(jpeg_size(&jpeg), Some((128, 64)));
        assert_eq!(jpeg_size(b"\x00\x00"), None);
    }

    #[test]
    fn randomness_and_strftime() {
        assert_ne!(rand_u64(), rand_u64());
        assert_eq!(strftime_local(0, "%Y").len(), 4);
        assert!(http_request(
            "GET",
            "https://x/",
            &NsSet::default(),
            &[],
            Duration::from_secs(1)
        )
        .is_err());
    }
}
