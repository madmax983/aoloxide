//! Socket commands (`nsd/tclsock.c`): `ns_sockopen`, `ns_socklisten`,
//! `ns_sockaccept`, `ns_socketpair`, `ns_sockselect`, `ns_sockcallback`,
//! `ns_socklistencallback`, `ns_quewait` and friends.
//!
//! Sockets are the engine's own channels (`Tcl_MakeTcpClientChannel`
//! through [`Interp::socket_channel`]), so scripts use them with
//! `gets`, `puts`, `read`, `fconfigure` and `close`.

use std::os::fd::RawFd;
use std::sync::{Arc, Weak};

use ns_script::{get_index, parse_bool, wrong_args, Command, Exception, Interp, ScriptResult};

use super::{bool_str, check_args, cmd, conn, int_arg, server, time_arg};
use crate::interp::{allocate, log_error};
use crate::server::Server;
use crate::sock;
use crate::sockcallback::{ListenProc, SOCK_EXCEPTION, SOCK_EXIT, SOCK_READ, SOCK_WRITE};

/// Commands defined here.
pub fn commands() -> Vec<(&'static str, Command)> {
    vec![
        ("ns_sockopen", cmd(ns_sockopen)),
        ("ns_sockblocking", cmd(|i, a| sock_blocking(i, a, true))),
        ("ns_socknonblocking", cmd(|i, a| sock_blocking(i, a, false))),
        ("ns_socknread", cmd(ns_socknread)),
        ("ns_sockcheck", cmd(ns_sockcheck)),
        ("ns_socklisten", cmd(ns_socklisten)),
        ("ns_sockaccept", cmd(ns_sockaccept)),
        ("ns_sockselect", cmd(ns_sockselect)),
        ("ns_socketpair", cmd(ns_socketpair)),
        ("ns_sockcallback", cmd(ns_sockcallback)),
        ("ns_socklistencallback", cmd(ns_socklistencallback)),
        ("ns_quewait", cmd(ns_quewait)),
    ]
}

fn posix(prefix: &str, e: &std::io::Error) -> Exception {
    Exception::error(format!("{prefix}{}", sock::posix_error(e)))
}

/// `EnterDupedSocks`: a channel for `fd` and one for a duplicate, as a
/// two-element list (read side, write side).
fn enter_duped(interp: &dyn Interp, fd: RawFd) -> ScriptResult {
    let first = interp.socket_channel(fd)?;
    let dup = sock::dup(fd).map_err(|e| posix("could not dup socket: ", &e))?;
    let second = interp.socket_channel(dup)?;
    Ok(interp.list_join(&[&first, &second]))
}

/// A port argument (`Tcl_GetIntFromObj`, truncated as `htons` does).
fn port_arg(s: &str) -> Result<i64, Exception> {
    int_arg(s)
}

fn ns_sockopen(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    const USAGE: &str =
        "?(-nonblock | -async) | -timeout seconds? ?-localhost host? ?-localport port? host port";
    let syntax = || wrong_args(argv, 1, USAGE);
    if argv.len() < 3 || argv.len() > 9 {
        return Err(syntax());
    }
    let mut lhost: Option<&str> = None;
    let mut lport = 0i64;
    let mut async_ = false;
    let mut timeout: i64 = -1;
    let mut first = 1;
    while first < argv.len() {
        let opt = argv[first].as_str();
        if !opt.starts_with('-') {
            break;
        }
        match opt {
            "-nonblock" | "-async" => {
                if timeout >= 0 {
                    return Err(syntax());
                }
                async_ = true;
            }
            "-localhost" => {
                first += 1;
                let h = argv.get(first).ok_or_else(syntax)?;
                if h.is_empty() {
                    return Err(Exception::error("invalid hostname: must not be empty"));
                }
                lhost = Some(h);
            }
            "-timeout" => {
                first += 1;
                if first >= argv.len() || async_ {
                    return Err(syntax());
                }
                timeout = int_arg(&argv[first])?;
            }
            "-localport" => {
                first += 1;
                let p = argv.get(first).ok_or_else(syntax)?;
                lport = port_arg(p)?;
                if lport < 0 {
                    return Err(Exception::error(format!("invalid port: {p}; must be > 0")));
                }
            }
            _ => return Err(Exception::error(format!("invalid option: \"{opt}\""))),
        }
        first += 1;
    }
    if argv.len() - first != 2 {
        return Err(syntax());
    }
    let host = &argv[first];
    if host.is_empty() {
        return Err(Exception::error("invalid hostname: must not be empty"));
    }
    let port = port_arg(&argv[first + 1])?;
    if port < 0 {
        return Err(Exception::error(format!(
            "invalid port: {}; must be > 0",
            argv[first + 1]
        )));
    }
    let (port, lport) = (port as u16, lport as u16);
    let r = if async_ {
        sock::connect(host, port, lhost, lport, true)
    } else if timeout < 0 {
        sock::connect(host, port, lhost, lport, false)
    } else {
        sock::timed_connect(
            host,
            port,
            lhost,
            lport,
            std::time::Duration::from_secs(timeout as u64),
        )
    };
    let fd = r.map_err(|e| {
        let why = if e.raw_os_error().is_some() {
            sock::posix_error(&e)
        } else {
            "reason unknown".to_owned()
        };
        Exception::error(format!(
            "can't connect to \"{host}:{}\"; {why}",
            argv[first + 1]
        ))
    })?;
    enter_duped(interp, fd)
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
    let fd = interp.channel_fd(&argv[1], false)?;
    let n = sock::nread(fd).map_err(|e| posix("ns_sockioctl failed: ", &e))?;
    Ok((n + interp.channel_buffered(&argv[1])?).to_string())
}

fn ns_sockcheck(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 2, 1, "sockId")?;
    let fd = interp.channel_fd(&argv[1], true)?;
    Ok(bool_str(sock::check(fd)))
}

/// `*` means every address.
fn addr_arg(s: &str) -> Option<&str> {
    (s != "*").then_some(s)
}

fn ns_socklisten(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 3, 3, 1, "address port")?;
    let port = port_arg(&argv[2])?;
    let backlog = server(interp).map_or(32, |s| s.global.params.backlog);
    let fd = sock::listen(addr_arg(&argv[1]), port as u16, backlog).map_err(|_| {
        Exception::error(format!("could not listen on \"{}:{}\"", argv[1], argv[2]))
    })?;
    interp.socket_channel(fd)
}

fn ns_sockaccept(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 2, 1, "sockId")?;
    let lsock = interp.channel_fd(&argv[1], false)?;
    let fd = sock::accept(lsock).map_err(|e| posix("accept failed: ", &e))?;
    enter_duped(interp, fd)
}

fn ns_socketpair(interp: &dyn Interp, _argv: &[String]) -> ScriptResult {
    let [a, b] = sock::pair().map_err(|e| posix("ns_sockpair failed:  ", &e))?;
    let first = match interp.socket_channel(a) {
        Ok(c) => c,
        Err(e) => {
            sock::close(b);
            return Err(e);
        }
    };
    let second = interp.socket_channel(b)?;
    Ok(interp.list_join(&[&first, &second]))
}

// ---- ns_sockselect (NsTclSelectObjCmd) ----

/// poll(2) results that `select(2)` reports as readable, writable and
/// exceptional (the kernel's `POLLIN_SET`, `POLLOUT_SET`, `POLLEX_SET`).
const READ_SET: libc::c_short = libc::POLLIN | libc::POLLHUP | libc::POLLERR;
const WRITE_SET: libc::c_short = libc::POLLOUT | libc::POLLERR;
const EXCEPT_SET: libc::c_short = libc::POLLPRI;

/// `GetSet`: the descriptors of a list of channels, in reverse list order
/// as AOLserver walks them.
fn fd_set(interp: &dyn Interp, list: &[String], write: bool) -> Result<Vec<RawFd>, Exception> {
    list.iter()
        .rev()
        .map(|c| interp.channel_fd(c, write))
        .collect()
}

fn ns_sockselect(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    const USAGE: &str = "?-timeout sec? rfds wfds efds";
    if argv.len() != 6 && argv.len() != 4 {
        return Err(wrong_args(argv, 1, USAGE));
    }
    let (mut timeout, arg) = if argv.len() == 4 {
        (None, 1)
    } else {
        if argv[1] != "-timeout" {
            return Err(wrong_args(argv, 1, USAGE));
        }
        (Some(time_arg(&argv[2])?), 3)
    };
    // Channels with buffered input are ready without polling.
    let mut buffered = Vec::new();
    let mut rlist = Vec::new();
    for c in interp.list_split(&argv[arg])? {
        if interp.channel_buffered(&c)? > 0 {
            buffered.push(c);
        } else {
            rlist.push(c);
        }
    }
    if !buffered.is_empty() {
        timeout = Some(std::time::Duration::ZERO);
    }
    let wlist = interp.list_split(&argv[arg + 1])?;
    let elist = interp.list_split(&argv[arg + 2])?;
    let rfds = fd_set(interp, &rlist, false)?;
    let wfds = fd_set(interp, &wlist, true)?;
    let efds = fd_set(interp, &elist, false)?;
    if buffered.is_empty()
        && rfds.is_empty()
        && wfds.is_empty()
        && efds.is_empty()
        && timeout.is_none()
    {
        return Ok(String::new());
    }
    let mut pfds: Vec<libc::pollfd> = Vec::new();
    for (fds, events) in [
        (&rfds, libc::POLLIN),
        (&wfds, libc::POLLOUT),
        (&efds, libc::POLLPRI),
    ] {
        pfds.extend(fds.iter().map(|&fd| libc::pollfd {
            fd,
            events,
            revents: 0,
        }));
    }
    let n = sock::poll(&mut pfds, timeout).map_err(|e| posix("select failed: ", &e))?;
    // `AppendReadyFiles`: ready channels, walking each list backwards.
    let ready = |list: &[String], from: usize, set: libc::c_short, out: &mut Vec<String>| {
        for (i, c) in list.iter().rev().enumerate() {
            if n > 0 && pfds[from + i].revents & set != 0 {
                out.push(c.clone());
            }
        }
    };
    let mut r = buffered;
    ready(&rlist, 0, READ_SET, &mut r);
    let mut w = Vec::new();
    ready(&wlist, rfds.len(), WRITE_SET, &mut w);
    let mut e = Vec::new();
    ready(&elist, rfds.len() + wfds.len(), EXCEPT_SET, &mut e);
    let r = interp.list_from(&r);
    let w = interp.list_from(&w);
    let e = interp.list_from(&e);
    Ok(interp.list_join(&[&r, &w, &e]))
}

// ---- ns_sockcallback (NsTclSockCallbackObjCmd, NsTclSockProc) ----

/// The state of one `ns_sockcallback` (`TclSockCallback`).
struct TclSockCallback {
    server: Weak<Server>,
    fd: RawFd,
    /// The callback thread's channel for `fd`, made on first use.
    chan: Option<String>,
    when: u32,
    script: String,
    done: bool,
}

impl TclSockCallback {
    /// Release the descriptor: through its channel if one was made.
    fn finish(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        match (self.chan.take(), self.server.upgrade()) {
            (Some(chan), Some(srv)) => {
                srv.global.engine.release_held(&chan);
            }
            (Some(_), None) => {}
            (None, _) => sock::close(self.fd),
        }
    }

    /// `NsTclSockProc`: run the script with the channel and condition
    /// appended; a non-true result (or an exit) removes the callback.
    fn call(&mut self, why: u32) -> bool {
        let mut why = why;
        if why != SOCK_EXIT || self.when & SOCK_EXIT != 0 {
            why = self.run_script(why);
        }
        if why == SOCK_EXIT {
            self.finish();
            return false;
        }
        true
    }

    fn run_script(&mut self, why: u32) -> u32 {
        let Some(srv) = self.server.upgrade() else {
            return SOCK_EXIT;
        };
        let lease = match allocate(&srv) {
            Ok(l) => l,
            Err(e) => {
                srv.global.log(ns_core::log::Severity::Error, &e.message);
                return SOCK_EXIT;
            }
        };
        let chan = if let Some(c) = &self.chan {
            c.clone()
        } else if let Ok(c) = srv.global.engine.hold_socket(self.fd) {
            self.chan = Some(c.clone());
            c
        } else {
            // The engine closed the descriptor.
            self.done = true;
            srv.global.log(
                ns_core::log::Severity::Error,
                &format!("could not make channel for sock: {}", self.fd),
            );
            return SOCK_EXIT;
        };
        if let Err(e) = lease.attach_held_channel(&chan) {
            log_error(&srv, &*lease, &e);
            return SOCK_EXIT;
        }
        let w = match why {
            SOCK_READ => "r",
            SOCK_WRITE => "w",
            SOCK_EXCEPTION => "e",
            _ => "x",
        };
        let script = format!("{} {}", self.script, lease.list_join(&[&chan, w]));
        match lease.eval(&script) {
            Err(e) => log_error(&srv, &*lease, &e),
            Ok(r) => {
                if !parse_bool(&r).unwrap_or(false) {
                    return SOCK_EXIT;
                }
            }
        }
        why
    }
}

impl Drop for TclSockCallback {
    fn drop(&mut self) {
        // A cancelled or replaced callback still owns its descriptor.
        self.finish();
    }
}

/// `r`, `w`, `e` and `x` flags.
fn parse_when(s: &str) -> Result<u32, Exception> {
    let bad = || {
        Exception::error(format!(
            "invalid when specification \"{s}\": should be one or more of r, w, e, or x"
        ))
    };
    let mut when = 0;
    for c in s.chars() {
        when |= match c {
            'r' => SOCK_READ,
            'w' => SOCK_WRITE,
            'e' => SOCK_EXCEPTION,
            'x' => SOCK_EXIT,
            _ => return Err(bad()),
        };
    }
    if when == 0 {
        return Err(bad());
    }
    Ok(when)
}

fn ns_sockcallback(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 4, 4, 1, "sockId script when")?;
    let when = parse_when(&argv[3])?;
    let fd = interp.channel_fd(&argv[1], when & SOCK_WRITE != 0)?;
    let srv = server(interp)?;
    // The callback thread gets its own descriptor, so this interp's
    // cleanup can close the channel.
    let fd = sock::dup(fd).map_err(|e| posix("could not dup socket: ", &e))?;
    let mut cb = TclSockCallback {
        server: Arc::downgrade(&srv),
        fd,
        chan: None,
        when,
        script: argv[2].clone(),
        done: false,
    };
    let info = vec!["ns:tclsockcallback".to_owned(), argv[2].clone()];
    let registered = srv.global.socks.register(
        fd,
        Box::new(move |_, why| cb.call(why)),
        when | SOCK_EXIT,
        info,
    );
    // On failure the dropped callback closes the duplicate.
    registered.map_err(|_| Exception::error("could not register callback"))?;
    Ok(String::new())
}

/// `SockListenCallback`: run the script with a read and a write channel
/// for the accepted socket, in a server interp on the callback thread.
fn listen_script(server: &Weak<Server>, script: &str, fd: RawFd) -> bool {
    let Some(srv) = server.upgrade() else {
        sock::close(fd);
        return true;
    };
    let lease = match allocate(&srv) {
        Ok(l) => l,
        Err(e) => {
            srv.global.log(ns_core::log::Severity::Error, &e.message);
            sock::close(fd);
            return true;
        }
    };
    let r = enter_duped(&*lease, fd).and_then(|chans| {
        let words = lease.list_split(&chans)?;
        let refs: Vec<&str> = words.iter().map(String::as_str).collect();
        lease.eval(&format!("{script} {}", lease.list_join(&refs)))
    });
    if let Err(e) = r {
        log_error(&srv, &*lease, &e);
    }
    true
}

fn ns_socklistencallback(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 4, 4, 1, "address port script")?;
    let port = port_arg(&argv[2])?;
    let srv = server(interp)?;
    let weak = Arc::downgrade(&srv);
    let script = argv[3].clone();
    let proc_: ListenProc = Arc::new(move |fd, _| listen_script(&weak, &script, fd));
    srv.global
        .socks
        .listen_callback(addr_arg(&argv[1]), port as u16, proc_)
        .map_err(|_| Exception::error("could not register callback"))?;
    Ok(String::new())
}

// ---- ns_quewait (NsTclQueWaitObjCmd, WaitCallback) ----

fn ns_quewait(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 5, 5, 1, "sockId event timeout script")?;
    let c = conn(interp)?;
    interp.channel_buffered(&argv[1])?;
    let idx = get_index(&argv[2], &["readable", "writable"], "event")?;
    let (when, write) = if idx == 0 {
        (SOCK_READ, false)
    } else {
        (SOCK_WRITE, true)
    };
    let fd = interp.channel_fd(&argv[1], write)?;
    let timeout = time_arg(&argv[3])?;
    let script = argv[4].clone();
    let proc_ = Box::new(move |conn: &crate::conn::ConnRef, _fd: RawFd, why: u32| {
        let Ok(interp) = conn.interp() else {
            return;
        };
        let s = if why == 0 {
            "timeout"
        } else if why & SOCK_READ != 0 {
            "readable"
        } else if why & SOCK_WRITE != 0 {
            "writable"
        } else {
            "dropped"
        };
        if let Err(e) = interp.eval(&format!("{script} {}", interp.list_join(&[s]))) {
            log_error(&conn.server, &**interp, &e);
        }
    });
    crate::quewait::queue_wait(&c, fd, proc_, when, timeout)
        .map_err(|_| Exception::error("could not register sock wait"))?;
    Ok(String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn when_flags() {
        assert_eq!(
            parse_when("rwex"),
            Ok(SOCK_READ | SOCK_WRITE | SOCK_EXCEPTION | SOCK_EXIT)
        );
        assert_eq!(parse_when("r"), Ok(SOCK_READ));
        assert!(parse_when("").is_err());
        assert!(parse_when("rz")
            .expect_err("bad")
            .message
            .contains("should be one or more of r, w, e, or x"));
        assert_eq!(addr_arg("*"), None);
        assert_eq!(addr_arg("127.0.0.1"), Some("127.0.0.1"));
    }
}
