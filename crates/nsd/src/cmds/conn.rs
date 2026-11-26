//! `ns_conn` (`nsd/conn.c`) and the response commands (`nsd/tclresp.c`).

use std::path::Path;
use std::time::UNIX_EPOCH;

use ns_script::{
    binary_to_bytes, bytes_to_binary, wrong_args, Command, Exception, Interp, Options, ScriptResult,
};

use super::set::{enter_set, lookup_set};
use super::{bool_str, cmd, conn, int_arg, io_ok, status_arg};
use crate::conn::{lock_set, set_ref, ConnRef};
use crate::content::Content;
use crate::interp::ns_interp;

/// Commands defined here.
pub fn commands() -> Vec<(&'static str, Command)> {
    vec![
        ("ns_conn", cmd(ns_conn)),
        ("ns_return", cmd(ns_return)),
        ("ns_respond", cmd(ns_respond)),
        ("ns_returnfile", cmd(ns_returnfile)),
        ("ns_returnfp", cmd(ns_returnfp)),
        ("ns_returnbadrequest", cmd(ns_returnbadrequest)),
        (
            "ns_returnnotfound",
            cmd(|i, a| simple(i, a, super::super::conn::ConnCtx::return_not_found)),
        ),
        (
            "ns_returnforbidden",
            cmd(|i, a| simple(i, a, super::super::conn::ConnCtx::return_forbidden)),
        ),
        (
            "ns_returnunauthorized",
            cmd(|i, a| simple(i, a, super::super::conn::ConnCtx::return_unauthorized)),
        ),
        ("ns_returnerror", cmd(ns_returnerror)),
        ("ns_returnnotice", cmd(ns_returnnotice)),
        ("ns_returnadminnotice", cmd(ns_returnnotice)),
        ("ns_returnredirect", cmd(ns_returnredirect)),
        ("ns_returnmoved", cmd(ns_returnmoved)),
        ("ns_write", cmd(ns_write)),
        ("ns_headers", cmd(ns_headers)),
        ("ns_startcontent", cmd(ns_startcontent)),
        ("ns_writecontent", cmd(ns_writecontent)),
        ("ns_conncptofp", cmd(ns_writecontent)),
        ("ns_connsendfp", cmd(ns_connsendfp)),
        ("ns_writefp", cmd(ns_connsendfp)),
        ("ns_internalredirect", cmd(ns_internalredirect)),
        ("ns_atclose", cmd(ns_atclose)),
    ]
}

/// Drop an optional leading `cnsN` connection id (the AOLserver 2/3 API).
fn strip_connid(argv: &[String]) -> Vec<String> {
    let mut v = argv.to_vec();
    if v.len() > 1 && v[1].starts_with("cns") && v[1][3..].chars().all(|c| c.is_ascii_digit()) {
        v.remove(1);
    }
    v
}

fn conn_set_id(interp: &dyn Interp, c: &ConnRef, which: &'static str) -> ScriptResult {
    let st = ns_interp(interp)?;
    if let Some(id) = st.borrow().conn_sets.get(which) {
        if st.borrow().sets.contains_key(id) {
            return Ok(id.clone());
        }
    }
    let set = match which {
        "headers" => c.headers(),
        "outputheaders" => c.output_headers(),
        _ => set_ref(c.form().fields),
    };
    let id = enter_set(interp, set, false, false)?;
    st.borrow_mut().conn_sets.insert(which, id.clone());
    Ok(id)
}

const CONN_OPTS: &[&str] = &[
    "authpassword",
    "authuser",
    "channel",
    "close",
    "contentavail",
    "content",
    "contentlength",
    "contentsentlength",
    "contentchannel",
    "copy",
    "driver",
    "encoding",
    "files",
    "fileoffset",
    "filelength",
    "fileheaders",
    "flags",
    "form",
    "headers",
    "host",
    "id",
    "isconnected",
    "keepalive",
    "location",
    "method",
    "outputheaders",
    "peeraddr",
    "peerport",
    "pool",
    "port",
    "protocol",
    "query",
    "request",
    "server",
    "sock",
    "start",
    "status",
    "url",
    "urlc",
    "urlencoding",
    "urlv",
    "version",
    "write_encoded",
    "gzip",
    "interp",
];

fn ns_conn(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let argv = strip_connid(argv);
    let (_, opt) = super::subcommand(&argv, CONN_OPTS)?;
    if opt == "isconnected" {
        return Ok(bool_str(super::conn(interp).is_ok_and(|c| !c.is_closed())));
    }
    let c = conn(interp)?;
    let arg = |i: usize| argv.get(i).map(String::as_str);
    Ok(match opt {
        "authpassword" => c.conn().auth_passwd.clone().unwrap_or_default(),
        "authuser" => c.conn().auth_user.clone().unwrap_or_default(),
        "close" => {
            c.close();
            String::new()
        }
        "content" => {
            if argv.len() != 2 && argv.len() != 4 {
                return Err(wrong_args(&argv, 2, "?off len?"));
            }
            let cc = c.conn();
            let slice = if argv.len() == 4 {
                let (off, len) = content_indices(&cc.content, &argv[2], &argv[3])?;
                cc.content.read_at(off, len).map_err(content_error)?
            } else {
                // `Tcl_SetResult(interp, Ns_ConnContent(conn), TCL_STATIC)`:
                // a C string, so the body ends at its first NUL.
                let all = cc.content.bytes().unwrap_or_default();
                let end = all.iter().position(|&b| b == 0).unwrap_or(all.len());
                all[..end].to_vec()
            };
            match cc.query_encoding {
                Some(e) => e.decode_without_bom_handling(&slice).0.into_owned(),
                None => String::from_utf8(slice).unwrap_or_else(|e| bytes_to_binary(e.as_bytes())),
            }
        }
        "contentavail" => {
            let cc = c.conn();
            (cc.content.len() - cc.content_offset.min(cc.content.len())).to_string()
        }
        "contentchannel" => content_channel(interp, &c)?,
        "contentlength" => c.conn().content.len().to_string(),
        "contentsentlength" => c.conn().n_content_sent.to_string(),
        "copy" => {
            if argv.len() != 5 {
                return Err(wrong_args(&argv, 2, "off len chan"));
            }
            let (off, len) = content_indices(&c.conn().content, &argv[2], &argv[3])?;
            copy_content(interp, &c, off, len, &argv[4])?;
            String::new()
        }
        "channel" | "sock" | "interp" => {
            return Err(Exception::error(format!(
                "ns_conn {opt}: not supported by the {} engine bridge",
                interp.engine()
            )))
        }
        "driver" => c.conn().driver.name.clone(),
        "encoding" | "urlencoding" => {
            if let Some(cs) = arg(2) {
                let enc = ns_core::urlencode::charset_encoding(cs);
                if enc.is_none() && !cs.eq_ignore_ascii_case("utf-8") {
                    return Err(Exception::error(format!("no such encoding: {cs}")));
                }
                let mut cc = c.conn_mut();
                if opt == "encoding" {
                    cc.output_encoding = enc;
                } else {
                    cc.url_encoding = enc;
                    cc.query_encoding = enc;
                    cc.form = None;
                }
            }
            let cc = c.conn();
            let e = if opt == "encoding" {
                cc.output_encoding
            } else {
                cc.url_encoding
            };
            e.map_or("utf-8", |e| e.name()).to_ascii_lowercase()
        }
        "files" => {
            let names: Vec<String> = c.form().files.iter().map(|f| f.name.clone()).collect();
            interp.list_from(&names)
        }
        "fileoffset" | "filelength" | "fileheaders" => {
            let name = arg(2).ok_or_else(|| wrong_args(&argv, 2, "file"))?;
            let form = c.form();
            let f = form
                .file(name)
                .ok_or_else(|| Exception::error(format!("no such file: {name}")))?;
            match opt {
                "fileoffset" => f.offset.to_string(),
                "filelength" => f.length.to_string(),
                _ => enter_set(interp, set_ref(f.headers.clone()), false, false)?,
            }
        }
        "flags" => {
            let f = c.conn().flags;
            let mut n = if c.conn().content.file_content() {
                0x80
            } else {
                0
            };
            for (b, v) in [
                (f.closed, 0x1),
                (f.skip_hdrs, 0x2),
                (f.skip_body, 0x4),
                (f.sent_hdrs, 0x10),
                (f.keepalive, 0x20),
                (f.overflow, 0x200),
                (f.gzip, 0x800),
                (f.chunk, 0x1000),
            ] {
                if b {
                    n |= v;
                }
            }
            n.to_string()
        }
        "form" => conn_set_id(interp, &c, "form")?,
        "headers" => conn_set_id(interp, &c, "headers")?,
        "outputheaders" => conn_set_id(interp, &c, "outputheaders")?,
        "host" => c.conn().request.host.clone().unwrap_or_default(),
        "id" => c.id().to_string(),
        "keepalive" => {
            if let Some(v) = arg(2) {
                c.conn_mut().flags.keepalive = ns_script::parse_bool(v)?;
            }
            bool_str(c.conn().flags.keepalive)
        }
        "gzip" => {
            if let Some(v) = arg(2) {
                c.conn_mut().flags.gzip = ns_script::parse_bool(v)?;
            }
            bool_str(c.conn().flags.gzip)
        }
        "location" => c.location(),
        "method" => c.method(),
        "peeraddr" => c.conn().peer.ip().to_string(),
        "peerport" => c.conn().peer.port().to_string(),
        "pool" => c.conn().pool.clone(),
        "port" => {
            let cc = c.conn();
            if cc.request.port != 0 {
                cc.request.port.to_string()
            } else {
                cc.driver.port().to_string()
            }
        }
        "protocol" => c.conn().request.protocol.clone().unwrap_or_default(),
        "query" => c.conn().request.query.clone().unwrap_or_default(),
        "request" => c.conn().request.line.clone(),
        "server" => c.server.name.clone(),
        "start" => {
            let t = c
                .conn()
                .times
                .accept
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            format!("{}:{}", t.as_secs(), t.subsec_micros())
        }
        "status" => {
            if let Some(s) = arg(2) {
                c.set_status(status_arg(s)?);
            }
            c.status().to_string()
        }
        "url" => c.url(),
        "urlc" => c.conn().request.urlv.len().to_string(),
        "urlv" => {
            let v = c.conn().request.urlv.clone();
            match arg(2) {
                Some(i) => {
                    let i = int_arg(i)?;
                    usize::try_from(i)
                        .ok()
                        .and_then(|i| v.get(i).cloned())
                        .unwrap_or_default()
                }
                None => interp.list_from(&v),
            }
        }
        "version" => {
            let r = &c.conn().request;
            format!("{}.{}", r.major, r.minor)
        }
        "write_encoded" => bool_str(c.conn().output_encoding.is_some()),
        _ => String::new(),
    })
}

/// Check `off len` against the content (`GetIndices`).
fn content_indices(content: &Content, off: &str, len: &str) -> Result<(usize, usize), Exception> {
    let total = i64::try_from(content.len()).unwrap_or(i64::MAX);
    let o = int_arg(off)?;
    let l = int_arg(len)?;
    if o < 0 || o > total {
        return Err(Exception::error(format!("invalid offset: {off}")));
    }
    if l < 0 || l > total - o {
        return Err(Exception::error(format!("invalid length: {len}")));
    }
    // Both are within 0..=content.len() here.
    Ok((o as usize, l as usize))
}

fn content_error(e: std::io::Error) -> Exception {
    Exception::error(format!("could not get content: {e}"))
}

/// Bytes copied to a channel per write by `ns_conn copy`.
const COPY_CHUNK: usize = 64 * 1024;

/// Write `len` content bytes from `off` to a channel (`ns_conn copy`),
/// a piece at a time so a spooled body is never wholly in memory.
fn copy_content(
    interp: &dyn Interp,
    c: &ConnRef,
    off: usize,
    len: usize,
    chan: &str,
) -> Result<(), Exception> {
    interp.eval_words(&["fconfigure", chan, "-translation", "binary"])?;
    let mut pos = off;
    let end = off + len;
    while pos < end {
        let n = (end - pos).min(COPY_CHUNK);
        let data = c.conn().content.read_at(pos, n).map_err(content_error)?;
        interp.eval_words(&["puts", "-nonewline", chan, &bytes_to_binary(&data)])?;
        pos += n;
    }
    Ok(())
}

/// `ns_conn contentchannel`: a readable channel on a temp file holding the
/// content (`Ns_ConnContentFd`, copying in-memory content there first).
/// As in AOLserver the result is empty when no temp file can be had.  The
/// channel is opened through `/dev/fd`, so closing it leaves the
/// connection's file open.
fn content_channel(interp: &dyn Interp, c: &ConnRef) -> ScriptResult {
    let Ok(fd) = c.content_fd() else {
        return Ok(String::new());
    };
    interp.eval_words(&["open", &format!("/dev/fd/{fd}"), "r"])
}

/// Read up to `len` bytes (all if `None`) from a script channel.
fn read_channel(interp: &dyn Interp, chan: &str, len: Option<i64>) -> Result<Vec<u8>, Exception> {
    interp.eval_words(&["fconfigure", chan, "-translation", "binary"])?;
    let data = match len {
        Some(n) if n >= 0 => interp.eval_words(&["read", chan, &n.to_string()])?,
        _ => interp.eval_words(&["read", chan])?,
    };
    Ok(binary_to_bytes(&data))
}

fn simple(
    interp: &dyn Interp,
    argv: &[String],
    f: impl FnOnce(&ConnRef) -> std::io::Result<()>,
) -> ScriptResult {
    let argv = strip_connid(argv);
    if argv.len() != 1 {
        return Err(wrong_args(&argv, 1, "?connid?"));
    }
    let c = conn(interp)?;
    io_ok(f(&c))
}

fn ns_return(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let mut argv = strip_connid(argv);
    let binary = argv.get(1).is_some_and(|a| a == "-binary");
    if binary {
        argv.remove(1);
    }
    if argv.len() != 4 {
        return Err(wrong_args(
            &argv,
            1,
            "?-binary? ?connid? status type string",
        ));
    }
    let c = conn(interp)?;
    let status = status_arg(&argv[1])?;
    if binary {
        io_ok(c.return_data(status, &binary_to_bytes(&argv[3]), Some(&argv[2])))
    } else {
        io_ok(c.return_char_data(status, &argv[3], Some(&argv[2])))
    }
}

fn ns_respond(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let c = conn(interp)?;
    let mut o = Options::new(argv, 1);
    let (mut status, mut ty, mut string, mut file, mut fileid, mut length, mut headers) =
        (200u16, None, None, None, None, None, None);
    while let Some(flag) = o.next_flag() {
        let v = o.value(&flag)?;
        match flag.to_ascii_lowercase().as_str() {
            "-status" => status = status_arg(&v)?,
            "-type" => ty = Some(v),
            "-string" => string = Some(v),
            "-file" => file = Some(v),
            "-fileid" => fileid = Some(v),
            "-length" => length = Some(int_arg(&v)?),
            "-headers" => headers = Some(v),
            _ => {
                return Err(wrong_args(
                    argv,
                    1,
                    "?-status status? ?-type type? { ?-string string? | ?-file filename? | ?-fileid fileid? } ?-length length? ?-headers setid?",
                ))
            }
        }
    }
    if let Some(h) = headers {
        let set = lookup_set(interp, &h, false)?;
        let src = lock_set(&set).clone();
        let out = c.output_headers();
        lock_set(&out).merge(&src);
    }
    if let Some(s) = string {
        io_ok(c.return_char_data(status, &s, ty.as_deref()))
    } else if let Some(f) = file {
        io_ok(c.return_file(status, ty.as_deref(), Path::new(&f)))
    } else if let Some(ch) = fileid {
        let data = read_channel(interp, &ch, length)?;
        io_ok(c.return_data(status, &data, ty.as_deref()))
    } else {
        io_ok(c.return_status(status))
    }
}

fn ns_returnfile(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let argv = strip_connid(argv);
    if argv.len() != 4 {
        return Err(wrong_args(&argv, 1, "?connid? status type file"));
    }
    let c = conn(interp)?;
    io_ok(c.return_file(status_arg(&argv[1])?, Some(&argv[2]), Path::new(&argv[3])))
}

fn ns_returnfp(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let argv = strip_connid(argv);
    if argv.len() != 5 {
        return Err(wrong_args(&argv, 1, "?connid? status type fileId len"));
    }
    let c = conn(interp)?;
    let data = read_channel(interp, &argv[3], Some(int_arg(&argv[4])?))?;
    io_ok(c.return_data(status_arg(&argv[1])?, &data, Some(&argv[2])))
}

fn ns_returnbadrequest(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let argv = strip_connid(argv);
    if argv.len() > 2 {
        return Err(wrong_args(&argv, 1, "?connid? reason"));
    }
    let c = conn(interp)?;
    io_ok(c.return_bad_request(argv.get(1).map(String::as_str)))
}

fn ns_returnerror(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let argv = strip_connid(argv);
    if argv.len() != 3 {
        return Err(wrong_args(&argv, 1, "?connid? status message"));
    }
    let c = conn(interp)?;
    io_ok(c.return_notice(status_arg(&argv[1])?, "Request Error", &argv[2]))
}

fn ns_returnnotice(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let argv = strip_connid(argv);
    if argv.len() != 4 && argv.len() != 3 {
        return Err(wrong_args(&argv, 1, "?connid? status title message"));
    }
    let c = conn(interp)?;
    let status = status_arg(&argv[1])?;
    if argv.len() == 3 {
        io_ok(c.return_admin_notice(status, &argv[2], ""))
    } else {
        io_ok(c.return_notice(status, &argv[2], &argv[3]))
    }
}

fn ns_returnredirect(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let argv = strip_connid(argv);
    if argv.len() != 2 {
        return Err(wrong_args(&argv, 1, "?connid? location"));
    }
    io_ok(conn(interp)?.return_redirect(Some(&argv[1])))
}

fn ns_returnmoved(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let argv = strip_connid(argv);
    if argv.len() != 2 {
        return Err(wrong_args(&argv, 1, "?connid? location"));
    }
    io_ok(conn(interp)?.return_moved(&argv[1]))
}

fn ns_write(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let argv = strip_connid(argv);
    if argv.len() != 2 {
        return Err(wrong_args(&argv, 1, "?connid? string"));
    }
    let c = conn(interp)?;
    // As in AOLserver: encode text when an output encoding is active,
    // otherwise write the string's byte-array form.
    let enc = c.conn().output_encoding;
    let data = match enc {
        Some(e) => e.encode(&argv[1]).0.into_owned(),
        None => binary_to_bytes(&argv[1]),
    };
    Ok(bool_str(c.write(&data).is_ok()))
}

fn ns_headers(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let argv = strip_connid(argv);
    if argv.len() < 2 || argv.len() > 4 {
        return Err(wrong_args(&argv, 1, "?connid? status ?type len?"));
    }
    let c = conn(interp)?;
    let status = status_arg(&argv[1])?;
    let len = match argv.get(3) {
        Some(l) => Some(int_arg(l)?.max(0) as u64),
        None => None,
    };
    c.set_required_headers(argv.get(2).map(String::as_str), len);
    c.queue_headers(status);
    io_ok(c.send(&[]).map(|_| ()))
}

fn ns_startcontent(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let c = conn(interp)?;
    let mut o = Options::new(argv, 1);
    let mut charset = None;
    let mut ty = None;
    while let Some(flag) = o.next_flag() {
        match flag.as_str() {
            "-charset" => charset = Some(o.value(&flag)?),
            "-type" => ty = Some(o.value(&flag)?),
            _ => return Err(wrong_args(argv, 1, "?-charset charset|-type type?")),
        }
    }
    let enc = match (charset, ty) {
        (Some(cs), _) => ns_core::urlencode::charset_encoding(&cs),
        (None, Some(t)) => t
            .to_ascii_lowercase()
            .split_once("charset=")
            .and_then(|(_, cs)| ns_core::urlencode::charset_encoding(cs.trim())),
        _ => c.server.charsets.output_encoding,
    };
    c.conn_mut().output_encoding = enc;
    Ok(String::new())
}

fn ns_writecontent(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let argv = strip_connid(argv);
    let mut o = Options::new(&argv, 1);
    let mut bytes = None;
    while let Some(flag) = o.next_flag() {
        if flag == "-bytes" {
            bytes = Some(int_arg(&o.value(&flag)?)?.max(0) as usize);
        } else {
            return Err(wrong_args(&argv, 1, "?-bytes tocopy? channel"));
        }
    }
    let chan = o
        .rest()
        .first()
        .ok_or_else(|| wrong_args(&argv, 1, "?-bytes tocopy? channel"))?
        .clone();
    let c = conn(interp)?;
    let (start, end) = {
        let mut cc = c.conn_mut();
        let start = cc.content_offset.min(cc.content.len());
        let end = bytes.map_or(cc.content.len(), |b| {
            start.saturating_add(b).min(cc.content.len())
        });
        cc.content_offset = end;
        (start, end)
    };
    copy_content(interp, &c, start, end - start, &chan)?;
    Ok(String::new())
}

fn ns_connsendfp(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let argv = strip_connid(argv);
    if argv.len() < 2 || argv.len() > 3 {
        return Err(wrong_args(&argv, 1, "?connid? fp len"));
    }
    let c = conn(interp)?;
    let len = match argv.get(2) {
        Some(l) => Some(int_arg(l)?),
        None => None,
    };
    let data = read_channel(interp, &argv[1], len)?;
    io_ok(c.write(&data))
}

fn ns_internalredirect(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let argv = strip_connid(argv);
    if argv.len() != 2 {
        return Err(wrong_args(&argv, 1, "?connid? location"));
    }
    let c = conn(interp)?;
    c.redirect(&argv[1])
        .map(|_| String::new())
        .map_err(|e| Exception::error(format!("redirect failed: {e}")))
}

fn ns_atclose(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    if argv.len() < 2 {
        return Err(wrong_args(argv, 1, "script ?args?"));
    }
    let c = conn(interp)?;
    let script = if argv.len() == 2 {
        argv[1].clone()
    } else {
        interp.list_from(&argv[1..])
    };
    c.at_close(script);
    Ok(String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connid_stripping() {
        let v: Vec<String> = ["ns_return", "cns12", "200"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert_eq!(strip_connid(&v), vec!["ns_return", "200"]);
        let v: Vec<String> = ["ns_return", "cnsx", "200"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert_eq!(strip_connid(&v).len(), 3);
    }
}
