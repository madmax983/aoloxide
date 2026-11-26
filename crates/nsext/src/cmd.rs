//! `ns_ext` (`ExtCmd`) and the remote file copies it offers.

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;

use ns_script::{Exception, Interp, ScriptResult};
use nsdb::DbConnection;
use nspd::msg::{self, Msg};
use nspd::strerror;

use crate::conn::{ExtConn, RSP_BUFSIZE};
use crate::DRIVER_NAME;

/// `FILE_IOSIZE`: the chunk size of `cpto` and `cpfrom`.
const FILE_IOSIZE: usize = 1024;

fn err(m: impl Into<String>) -> Exception {
    Exception::error(m)
}

fn insufficient(argv0: &str, usage: &str) -> Exception {
    err(format!("insufficient args: should be \"{argv0} {usage}\""))
}

/// A result: a string, or a list built once the handle is released.
enum Out {
    Str(String),
    List(Vec<String>),
}

/// `ns_ext cmd dbId ?arg ...?`.
pub fn ns_ext(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    if argv.len() < 3 {
        return Err(insufficient(&argv[0], "command dbId (optional cmd arg)"));
    }
    let id = &argv[2];
    let not_ext = || err(format!("handle \"{id}\" is not of type \"{DRIVER_NAME}\""));
    let out = nsdb::with_tcl_handle(interp, id, |h| {
        if !h.pool().driver.name().contains(DRIVER_NAME) {
            return Err(not_ext());
        }
        h.with_connection(|c| {
            c.as_any_mut()
                .and_then(|a| a.downcast_mut::<ExtConn>())
                .map_or_else(|| Err(not_ext()), |e| ext_cmd(e, argv))
        })
        .unwrap_or_else(|| Err(err(format!("handle \"{id}\" is not connected"))))
    })??;
    Ok(match out {
        Out::Str(s) => s,
        Out::List(l) => interp.list_from(&l),
    })
}

fn ext_cmd(e: &mut ExtConn, argv: &[String]) -> Result<Out, Exception> {
    let argv0 = argv[0].as_str();
    let nargs = |n: usize, usage: &str| {
        if argv.len() == n {
            Ok(())
        } else {
            Err(insufficient(argv0, usage))
        }
    };
    let s = match argv[1].as_str() {
        "ping" => if e.is_alive() { "ok" } else { "error" }.to_owned(),
        "isremote" => if e.ctx().path.is_some() { "0" } else { "1" }.to_owned(),
        "number" => e.number().to_string(),
        "identify" => e.ctx().ident(),
        "dbtype" => e.ctx().db_type(),
        "resethandle" => if e.reset().is_ok() { "0" } else { "-1" }.to_owned(),
        "traceon" => {
            nargs(4, "traceon dbId filepath")?;
            let path = &argv[3];
            if !std::path::Path::new(path).is_absolute() {
                return Err(err(format!(
                    "filepath argument: \"{path}\" must be absolute."
                )));
            }
            if e.ping_cmd(Msg::TraceOn, Some(path.as_bytes()), "traceon") {
                format!("Trace is on, using file: {path}")
            } else {
                format!("Error: traceon command to file: {path} failed")
            }
        }
        "traceoff" => if e.ping_cmd(Msg::TraceOff, None, "traceoff") {
            "Trace is off."
        } else {
            "Error: traceoff command failed"
        }
        .to_owned(),
        "gettypes" => {
            let types = e
                .line_cmd(Msg::GetTypes, None, RSP_BUFSIZE, "gettypes")
                .ok_or_else(|| err("Error: gettypes command failed"))?;
            return Ok(Out::List(
                types
                    .split([' ', '\t'])
                    .filter(|w| !w.is_empty())
                    .map(str::to_owned)
                    .collect(),
            ));
        }
        "resultid" => e
            .line_cmd(Msg::ResultId, None, RSP_BUFSIZE, "resultid")
            .ok_or_else(|| err("Error: resultid command failed"))?,
        "resultrows" => e
            .line_cmd(Msg::ResultRows, None, RSP_BUFSIZE, "resultrows")
            .ok_or_else(|| err("Error: resultrows command failed"))?,
        "setmaxrows" => {
            nargs(4, "setmaxrows dbId rowcount")?;
            let n = &argv[3];
            if !msg::all_digits(n) {
                return Err(err(format!("rowcount argument: \"{n}\" must be numeric.")));
            }
            if !e.ping_cmd(Msg::SetMaxRows, Some(n.as_bytes()), "setmaxrows") {
                return Err(err("Error: setmaxrows command failed"));
            }
            format!("Max rows set to {n}")
        }
        "mktemp" => create_remote_tmp_file(e).map_err(|m| err(format!("mktemp error: {m}")))?,
        "cpto" => {
            nargs(5, "cpto dbId localSrcFile remoteDestFile")?;
            copy_to_remote(e, &argv[3], &argv[4]).map_err(|m| err(format!("cpto error: {m}")))?;
            String::new()
        }
        "cpfrom" => {
            nargs(5, "cpfrom dbId localDestFile remoteSrcFile")?;
            copy_from_remote(e, &argv[3], &argv[4])
                .map_err(|m| err(format!("cpfrom error: {m}")))?;
            String::new()
        }
        "rm" => {
            nargs(4, "rm dbId remoteFile")?;
            // A failure is the result, not an error.
            delete_remote(e, &argv[3]).map_or_else(|m| format!("rm error: {m}"), |()| String::new())
        }
        other => {
            return Err(err(format!(
                "unknown command \"{other}\": should be ping, gettypes, number, traceon, traceoff, \
                 identify, resultid, resultrows, setmaxrows, mktemp, rm, cpto, cpfrom."
            )))
        }
    };
    Ok(Out::Str(s))
}

/// A reply that must be `ok`; otherwise the reply is the error.
fn expect_ok(e: &mut ExtConn, what: &str) -> Result<(), String> {
    let r = e
        .get_string(RSP_BUFSIZE)
        .ok_or_else(|| format!("Can't get {what} status response from Proxy Daemon"))?;
    if r.eq_ignore_ascii_case(msg::OK_STATUS) {
        Ok(())
    } else {
        Err(r)
    }
}

/// `DbProxyCreateRemoteTmpFile`.
fn create_remote_tmp_file(e: &mut ExtConn) -> Result<String, String> {
    if !e.send(Msg::CreateTmpF, None) {
        return Err("Can't send CreateTmpF command to Proxy Daemon".into());
    }
    expect_ok(e, "CreateTmpF")?;
    e.get_string(RSP_BUFSIZE)
        .ok_or_else(|| "Can't get CreateTmpF name response from Proxy Daemon".into())
}

/// `openfile flags#path`: the remote descriptor.
fn open_remote(e: &mut ExtConn, flags: i32, path: &str, send_err: &str) -> Result<String, String> {
    let params = format!("{flags}{}{path}", msg::ARG_TOKEN_DELIMITER);
    if !e.send(Msg::OpenF, Some(params.as_bytes())) {
        return Err(send_err.to_owned());
    }
    expect_ok(e, "OpenF")?;
    e.get_string(RSP_BUFSIZE)
        .ok_or_else(|| "Can't get OpenF fd response from Proxy Daemon".into())
}

/// `closefile fd`, which ends every copy that opened a remote file.
fn close_remote(e: &mut ExtConn, fd: &str) -> Result<(), String> {
    if !e.send(Msg::CloseF, Some(fd.as_bytes())) {
        return Err("Can't send CloseF command to Proxy Daemon".into());
    }
    expect_ok(e, "CloseF")
}

/// `DbProxyCopyToRemoteFile`.
fn copy_to_remote(e: &mut ExtConn, src: &str, remote: &str) -> Result<(), String> {
    let mut local =
        File::open(src).map_err(|x| format!("open on {src} failed: {}", strerror(&x)))?;
    let fd = open_remote(
        e,
        libc::O_RDWR,
        remote,
        "Can't send OpenFcommand to Proxy Daemon",
    )?;
    let d = msg::ARG_TOKEN_DELIMITER;
    let mut loc = 0usize;
    let mut buf = [0u8; FILE_IOSIZE];
    let copied = loop {
        let n = match local.read(&mut buf) {
            Ok(0) => break Ok(()),
            Ok(n) => n,
            Err(x) => break Err(format!("read on {src} failed: {}", strerror(&x))),
        };
        let mut out = format!("{fd}{d}{loc}{d}{n}{d}").into_bytes();
        out.extend_from_slice(&buf[..n]);
        if !e.send(Msg::WriteF, Some(&out)) {
            break Err(format!("Remote write failure, file offset={loc}"));
        }
        if let Err(m) = expect_ok(e, "WriteF") {
            break Err(m);
        }
        loc += n;
    };
    // Deviation: a failed copy stays failed when the close succeeds.
    let closed = close_remote(e, &fd);
    copied.and(closed)
}

/// `DbProxyCopyFromRemoteFile`.
fn copy_from_remote(e: &mut ExtConn, dest: &str, remote: &str) -> Result<(), String> {
    let mut local = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(dest)
        .map_err(|x| format!("open/create on {dest} failed: {}", strerror(&x)))?;
    let fd = open_remote(
        e,
        libc::O_RDONLY,
        remote,
        "Can't send OpenF command to Proxy Daemon",
    )?;
    let d = msg::ARG_TOKEN_DELIMITER;
    let mut loc = 0usize;
    let copied = loop {
        let req = format!("{fd}{d}{loc}{d}{FILE_IOSIZE}");
        if !e.send(Msg::ReadF, Some(req.as_bytes())) {
            break Err("Can't send ReadF command to Proxy Daemon".to_owned());
        }
        if let Err(m) = expect_ok(e, "ReadF") {
            break Err(m);
        }
        let Some(list) = e.get_list().filter(|l| !l.is_empty()) else {
            break Err("Can't read data from Proxy Daemon".to_owned());
        };
        let data = &list[0];
        if data.is_empty() {
            break Ok(());
        }
        if let Err(x) = local.write_all(data) {
            break Err(format!("Local write to {dest} failed: {}", strerror(&x)));
        }
        loc += data.len();
    };
    drop(local);
    let closed = close_remote(e, &fd);
    copied.and(closed)
}

/// `DbProxyDeleteRemoteFile`.
fn delete_remote(e: &mut ExtConn, path: &str) -> Result<(), String> {
    if !e.send(Msg::DeleteF, Some(path.as_bytes())) {
        return Err("Unable to send DeleteF command Proxy Daemon".into());
    }
    let r = e
        .get_string(RSP_BUFSIZE)
        .ok_or("Unable to get DeleteF status response from Proxy Daemon")?;
    if r.eq_ignore_ascii_case(msg::OK_STATUS) {
        Ok(())
    } else {
        Err(r)
    }
}
