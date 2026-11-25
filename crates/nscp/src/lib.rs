//! # nscp
//!
//! The AOLserver control port (`nscp/nscp.c`): a telnet-style Tcl shell for
//! administrators.  After `login:`/`Password:` (passwords are `crypt(3)`
//! hashes from the `users` section), each line is evaluated in a server
//! interp and the result is written back.  Incomplete commands continue on
//! `>>>` prompts; `exit` ends the session.
//!
//! Config (`ns/server/S/module/nscp`): `address`, `port` (required),
//! `echopassword` (default on), `cpcmdlogging`; users in
//! `ns/server/S/module/nscp/users` as `ns_param user "name:encpass"`.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ns_core::log::Severity;
use ns_script::{command, Exception, Interp};
use nsd::callbacks::When;
use nsd::{ModuleContext, NsError, Server};

const IAC: u8 = 255;
const WILL: u8 = 251;
const WONT: u8 = 252;
const DO: u8 = 253;
const DONT: u8 = 254;
const ECHO: u8 = 1;

/// Control-port configuration.
#[derive(Debug, Clone)]
pub struct CpConfig {
    /// Users and their encrypted passwords.
    pub users: HashMap<String, String>,
    /// Echo the password as typed.
    pub echo_password: bool,
    /// Log every command.
    pub log_commands: bool,
}

const IP: u8 = 244;
const EOF: u8 = 236;
const EOT: u8 = 4;

/// Message of the [`io::ErrorKind::InvalidData`] error [`read_line`]
/// returns for a telnet command it does not handle (as logged by `GetLine`).
pub const UNSUPPORTED_IAC: &str = "unsupported telnet IAC code received from client";

/// Read one line, stripping telnet option negotiation and the CR
/// (`GetLine`).
///
/// Returns `None` when the remote dropped: end of input (even after a
/// partial line, which is never returned), `IAC EOF`, `IAC IP` (telnet's
/// Ctrl-C) or an EOT (Ctrl-D) at the start of a line.  Option negotiation
/// (`IAC WILL|WONT|DO|DONT opt`) is skipped; any other telnet command is
/// an [`io::ErrorKind::InvalidData`] error carrying [`UNSUPPORTED_IAC`].
pub fn read_line(r: &mut dyn Read) -> io::Result<Option<String>> {
    let mut out = Vec::new();
    loop {
        let Some(c) = read_byte(r)? else {
            return Ok(None);
        };
        match c {
            IAC => match read_byte(r)? {
                None | Some(EOF | IP) => return Ok(None),
                Some(WILL | WONT | DO | DONT) => {
                    if read_byte(r)?.is_none() {
                        return Ok(None);
                    }
                }
                Some(_) => {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, UNSUPPORTED_IAC));
                }
            },
            EOT if out.is_empty() => return Ok(None),
            b'\n' => break,
            b'\r' | 0 => {}
            c => out.push(c),
        }
    }
    Ok(Some(String::from_utf8_lossy(&out).into_owned()))
}

fn read_byte(r: &mut dyn Read) -> io::Result<Option<u8>> {
    let mut b = [0u8; 1];
    loop {
        match r.read(&mut b) {
            Ok(0) => return Ok(None),
            Ok(_) => return Ok(Some(b[0])),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

fn prompt(s: &mut TcpStream, p: &str, echo: bool) -> io::Result<Option<String>> {
    if !echo {
        s.write_all(&[IAC, WILL, ECHO, IAC, DONT, ECHO])?;
    }
    s.write_all(p.as_bytes())?;
    let line = read_line(s);
    if !echo {
        s.write_all(&[IAC, WONT, ECHO, IAC, DO, ECHO, b'\r', b'\n'])?;
    }
    line
}

/// Check credentials (`Login`).  Like `STREQ` on the `Ns_Encrypt` buffer,
/// the encrypted password is compared up to its first NUL, so an empty
/// stored password (an empty salt) accepts any password.
pub fn check_login(cfg: &CpConfig, user: &str, pass: &str) -> bool {
    cfg.users.get(user).is_some_and(|enc| {
        ns_core::crypt::crypt(pass, enc).split('\0').next() == Some(enc.as_str())
    })
}

/// One control-port connection (`EvalThread`): logs `connected`, runs the
/// login and the shell, and logs `disconnected` however they end.
fn session(server: &Arc<Server>, cfg: &CpConfig, mut s: TcpStream) -> io::Result<()> {
    let g = &server.global;
    let peer = s
        .peer_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_default();
    g.log(Severity::Notice, &format!("nscp: {peer} connected"));
    let mut result = login(server, cfg, &mut s).and_then(|user| match user {
        Some(user) => shell(server, cfg, &mut s, &user),
        None => Ok(()),
    });
    if let Err(e) = &result {
        if e.kind() == io::ErrorKind::InvalidData {
            // GetLine's unsupported-IAC warning; the remote counts as dropped.
            g.log(Severity::Warning, &format!("nscp: {e}"));
            result = Ok(());
        }
    }
    g.log(Severity::Notice, &format!("nscp: {peer} disconnected"));
    result
}

/// Prompt for and check the credentials, then greet the user (`Login`).
/// Returns the user name once logged in; `None` after "Access denied!".
fn login(server: &Server, cfg: &CpConfig, s: &mut TcpStream) -> io::Result<Option<String>> {
    let g = &server.global;
    let mut creds = None;
    if let Some(user) = prompt(s, "login: ", true)? {
        if let Some(pass) = prompt(s, "Password: ", cfg.echo_password)? {
            creds = Some((user.trim().to_owned(), pass.trim().to_owned()));
        }
    }
    let user = match creds {
        Some((user, pass)) if check_login(cfg, &user, &pass) => user,
        other => {
            let user = other.map_or_else(|| "?".to_owned(), |(u, _)| u);
            g.log(Severity::Warning, &format!("nscp: login failed: '{user}'"));
            s.write_all(b"Access denied!\n")?;
            return Ok(None);
        }
    };
    g.log(Severity::Notice, &format!("nscp: {user} logged in"));
    s.write_all(
        format!(
            "\nWelcome to {} running at {} (pid {})\n{}/{} (aoloxide) for {}\n",
            server.name,
            g.info.nsd.display(),
            g.info.pid,
            nsd::global::SERVER_NAME,
            nsd::global::SERVER_VERSION,
            std::env::consts::OS
        )
        .as_bytes(),
    )?;
    Ok(Some(user))
}

/// Evaluate complete commands until `exit` or the remote drops
/// (`EvalThread`'s loop).
fn shell(server: &Arc<Server>, cfg: &CpConfig, s: &mut TcpStream, user: &str) -> io::Result<()> {
    let g = &server.global;
    let lease = nsd::interp::allocate(server).map_err(|e| io::Error::other(e.message))?;
    let stop = Arc::new(AtomicBool::new(false));
    let st = Arc::clone(&stop);
    lease.register(
        "exit",
        command(move |_, argv| {
            // ExitCmd.
            if argv.len() != 1 {
                let name = argv.first().map_or("exit", String::as_str);
                return Err(Exception::error(format!(
                    "wrong # args: should be \"{name}\""
                )));
            }
            st.store(true, Ordering::SeqCst);
            Ok("\nGoodbye!".into())
        }),
    );
    lease.mark_for_delete();
    let mut ncmd = 0u64;
    let result = (|| -> io::Result<()> {
        while !stop.load(Ordering::SeqCst) {
            ncmd += 1;
            let mut script = String::new();
            let mut p = format!("{}:nscp {ncmd}> ", server.name);
            loop {
                s.write_all(p.as_bytes())?;
                let Some(line) = read_line(s)? else {
                    return Ok(());
                };
                script.push_str(&line);
                script.push('\n');
                if is_complete(&*lease, &script) {
                    break;
                }
                p = format!("{}:nscp {ncmd}>>> ", server.name);
            }
            let script = script.trim_end_matches('\n');
            if script.trim().is_empty() {
                ncmd -= 1;
                continue;
            }
            if cfg.log_commands {
                g.log(Severity::Notice, &format!("nscp: {user} {ncmd}: {script}"));
            }
            let out = match lease.eval(script) {
                Ok(r) => r,
                Err(e) => {
                    nsd::interp::log_error(server, &*lease, &e);
                    e.message
                }
            };
            s.write_all(format!("{out}\r\n").as_bytes())?;
            if cfg.log_commands {
                g.log(Severity::Notice, &format!("nscp: {user} {ncmd}: done"));
            }
        }
        Ok(())
    })();
    drop(lease);
    nsd::interp::thread_cleanup(g);
    result
}

fn is_complete(interp: &dyn Interp, script: &str) -> bool {
    interp
        .eval_words(&["info", "complete", script])
        .map_or(true, |r| r == "1")
}

/// Module init.
pub fn init(ctx: &ModuleContext<'_>) -> Result<(), NsError> {
    let server = Arc::clone(
        ctx.server
            .ok_or_else(|| NsError::Config("nscp must be loaded as a per-server module".into()))?,
    );
    let path = ctx.config_path();
    let c = &ctx.global.config;
    let (Some(addr), Some(port)) = (c.get(&path, "address"), c.get_int(&path, "port")) else {
        return Err(NsError::Config(format!(
            "nscp: address and port must be specified in config ({path})"
        )));
    };
    let mut users = HashMap::new();
    if let Some(sec) = c.section(&format!("{path}/users")) {
        for (k, v) in sec.iter() {
            match v.and_then(|v| v.split_once(':')) {
                Some((u, p)) if k.eq_ignore_ascii_case("user") => {
                    // "name:encpass[:...]" as in nscp.c.
                    let enc = p.split(':').next().unwrap_or("");
                    if users.insert(u.to_owned(), enc.to_owned()).is_some() {
                        ctx.global
                            .log(Severity::Error, &format!("nscp: duplicate user: {u}"));
                    }
                }
                _ => ctx.global.log(
                    Severity::Error,
                    &format!("nscp: invalid user entry: {}", v.unwrap_or("")),
                ),
            }
        }
    }
    if users.is_empty() {
        ctx.global
            .log(Severity::Warning, "nscp: no authorized users");
    }
    let cfg = Arc::new(CpConfig {
        users,
        echo_password: c.bool_or(&path, "echopassword", true),
        log_commands: c.bool_or(&path, "cpcmdlogging", false),
    });
    let listener = TcpListener::bind((addr, u16::try_from(port).unwrap_or(0)))
        .map_err(|e| NsError::Config(format!("nscp: could not listen on {addr}:{port}: {e}")))?;
    let bound = listener.local_addr()?;
    listener.set_nonblocking(true)?;
    ctx.global
        .log(Severity::Notice, &format!("nscp: listening on {bound}"));
    server.nsv.with("nscp", true, |a| {
        if let Some(a) = a {
            a.insert("port".into(), bound.port().to_string());
        }
    });
    let stop = Arc::new(AtomicBool::new(false));
    let st = Arc::clone(&stop);
    ctx.global.callbacks.register(
        When::Shutdown,
        Arc::new(move || st.store(true, Ordering::SeqCst)),
    );
    let next = Arc::new(AtomicU64::new(1));
    std::thread::Builder::new()
        .name("-nscp-".into())
        .spawn(move || {
            while !stop.load(Ordering::SeqCst) && !server.global.state.stopping() {
                match listener.accept() {
                    Ok((sock, _)) => {
                        let _ = sock.set_nonblocking(false);
                        let (srv, cfg) = (Arc::clone(&server), Arc::clone(&cfg));
                        let id = next.fetch_add(1, Ordering::Relaxed);
                        let _ = std::thread::Builder::new()
                            .name(format!("-nscp:{id}-"))
                            .spawn(move || {
                                if let Err(e) = session(&srv, &cfg, sock) {
                                    srv.global.log(
                                        Severity::Notice,
                                        &format!("nscp: session {id} ended: {e}"),
                                    );
                                }
                            });
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(e) => {
                        server
                            .global
                            .log(Severity::Error, &format!("nscp: accept: {e}"));
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(mut input: &[u8]) -> io::Result<Option<String>> {
        read_line(&mut input)
    }

    #[test]
    fn line_reading_strips_telnet() {
        let mut input: &[u8] = &[b'a', IAC, DO, ECHO, b'b', b'\r', b'\n', b'c', b'\n'];
        assert_eq!(read_line(&mut input).expect("r").as_deref(), Some("ab"));
        assert_eq!(read_line(&mut input).expect("r").as_deref(), Some("c"));
        assert_eq!(read_line(&mut input).expect("r"), None);
        // Negotiation replies (e.g. to the password prompt's WILL ECHO).
        let neg = [
            IAC, WILL, ECHO, IAC, WONT, ECHO, IAC, DONT, ECHO, b'x', b'\n',
        ];
        assert_eq!(line(&neg).expect("r").as_deref(), Some("x"));
    }

    #[test]
    fn partial_line_at_eof_is_a_drop() {
        assert_eq!(line(b"ns_shutdown").expect("r"), None);
        assert_eq!(line(&[b'a', IAC]).expect("r"), None);
        assert_eq!(line(&[b'a', IAC, DO]).expect("r"), None);
    }

    #[test]
    fn interrupt_eof_and_eot_are_drops() {
        // Ctrl-C: IAC IP IAC DO TIMING-MARK; nothing after it is read.
        assert_eq!(line(&[IAC, IP, IAC, DO, 6, b'x', b'\n']).expect("r"), None);
        assert_eq!(line(&[b'a', IAC, IP, b'\n']).expect("r"), None);
        assert_eq!(line(&[IAC, EOF, b'x', b'\n']).expect("r"), None);
        assert_eq!(line(&[EOT, b'x', b'\n']).expect("r"), None);
        // EOT inside a line is data.
        assert_eq!(
            line(&[b'a', EOT, b'\n']).expect("r").as_deref(),
            Some("a\u{4}")
        );
    }

    #[test]
    fn unsupported_iac_is_an_error() {
        for cmd in [IAC, 246 /* AYT */, 250 /* SB */] {
            let e = line(&[IAC, cmd, b'x', b'\n']).expect_err("unsupported");
            assert_eq!(e.kind(), io::ErrorKind::InvalidData);
            assert_eq!(e.to_string(), UNSUPPORTED_IAC);
        }
    }

    #[test]
    fn login_check() {
        let mut users = HashMap::new();
        users.insert("admin".to_owned(), ns_core::crypt::crypt("pw", "ab"));
        users.insert("open".to_owned(), String::new());
        let cfg = CpConfig {
            users,
            echo_password: true,
            log_commands: false,
        };
        assert!(check_login(&cfg, "admin", "pw"));
        assert!(!check_login(&cfg, "admin", "bad"));
        assert!(!check_login(&cfg, "ghost", "pw"));
        // An empty stored password accepts anything (STREQ up to NUL).
        assert!(check_login(&cfg, "open", ""));
        assert!(check_login(&cfg, "open", "whatever"));
    }
}
