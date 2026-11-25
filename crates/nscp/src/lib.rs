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
use ns_script::{command, Interp};
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

/// Read one line, stripping telnet IAC sequences and the CR (`GetLine`).
/// Returns `None` at EOF.
pub fn read_line(r: &mut dyn Read) -> io::Result<Option<String>> {
    let mut out = Vec::new();
    let mut b = [0u8; 1];
    loop {
        if r.read(&mut b)? == 0 {
            return Ok(if out.is_empty() {
                None
            } else {
                Some(String::from_utf8_lossy(&out).into_owned())
            });
        }
        match b[0] {
            IAC => {
                let mut skip = [0u8; 2];
                r.read_exact(&mut skip)?;
            }
            b'\n' => break,
            b'\r' | 0 => {}
            c => out.push(c),
        }
    }
    Ok(Some(String::from_utf8_lossy(&out).into_owned()))
}

fn prompt(s: &mut TcpStream, p: &str, echo: bool) -> io::Result<Option<String>> {
    if !echo {
        s.write_all(&[IAC, WILL, ECHO, IAC, DONT, ECHO])?;
    }
    s.write_all(p.as_bytes())?;
    let line = read_line(s)?;
    if !echo {
        s.write_all(&[IAC, WONT, ECHO, IAC, DO, ECHO, b'\r', b'\n'])?;
    }
    Ok(line)
}

/// Check credentials (`Login`).
pub fn check_login(cfg: &CpConfig, user: &str, pass: &str) -> bool {
    cfg.users
        .get(user)
        .is_some_and(|enc| ns_core::crypt::crypt(pass, enc) == *enc)
}

fn session(server: &Arc<Server>, cfg: &CpConfig, mut s: TcpStream, id: u64) -> io::Result<()> {
    let g = &server.global;
    let peer = s
        .peer_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_default();
    g.log(Severity::Notice, &format!("nscp: {peer} connected"));
    let user = prompt(&mut s, "login: ", true)?.unwrap_or_default();
    let pass = prompt(&mut s, "Password: ", cfg.echo_password)?.unwrap_or_default();
    let (user, pass) = (user.trim().to_owned(), pass.trim().to_owned());
    if !check_login(cfg, &user, &pass) {
        g.log(Severity::Warning, &format!("nscp: login failed: '{user}'"));
        s.write_all(b"Access denied!\n")?;
        return Ok(());
    }
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
    let lease = nsd::interp::allocate(server).map_err(|e| io::Error::other(e.message))?;
    let stop = Arc::new(AtomicBool::new(false));
    let st = Arc::clone(&stop);
    lease.register(
        "exit",
        command(move |_, _| {
            st.store(true, Ordering::SeqCst);
            Ok(String::new())
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
                let Some(line) = read_line(&mut s)? else {
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
    g.log(
        Severity::Notice,
        &format!("nscp: {peer} disconnected (session {id})"),
    );
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
                                if let Err(e) = session(&srv, &cfg, sock, id) {
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

    #[test]
    fn line_reading_strips_telnet() {
        let mut input: &[u8] = &[b'a', IAC, DO, ECHO, b'b', b'\r', b'\n', b'c'];
        assert_eq!(read_line(&mut input).expect("r").as_deref(), Some("ab"));
        assert_eq!(read_line(&mut input).expect("r").as_deref(), Some("c"));
        assert_eq!(read_line(&mut input).expect("r"), None);
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
    }
}
