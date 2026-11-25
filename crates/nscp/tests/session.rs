//! Control-port sessions against a live server (`nscp/nscp.c` EvalThread).

#![allow(clippy::unwrap_used)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nsd::{Nsd, NsdOptions};

struct Srv {
    nsd: Nsd,
    _home: tempfile::TempDir,
    port: u16,
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.nsd.shutdown();
    }
}

/// Boot a server with nscp and the given `ns_param user` entries.
fn boot(users: &[&str]) -> Srv {
    let home = tempfile::tempdir().unwrap();
    nsd::init_tcl::install_home(home.path(), true).unwrap();
    std::fs::create_dir_all(home.path().join("servers/s1/pages")).unwrap();
    let users = users
        .iter()
        .map(|u| format!("ns_param user \"{u}\""))
        .collect::<Vec<_>>()
        .join("\n");
    let cfg = format!(
        r#"
ns_section ns/parameters
ns_param home {home}
ns_param shutdowntimeout 5
ns_section ns/servers
ns_param s1 "S1"
ns_section ns/server/s1/modules
ns_param nssock nssock.so
ns_param nscp nscp.so
ns_section ns/server/s1/module/nssock
ns_param port 0
ns_param address 127.0.0.1
ns_param hostname localhost
ns_section ns/server/s1/module/nscp
ns_param address 127.0.0.1
ns_param port 0
ns_section ns/server/s1/module/nscp/users
{users}
"#,
        home = home.path().display()
    );
    let mut o = NsdOptions::from_text(&cfg);
    o.log_memory = true;
    let mut modules = nsd::builtin_modules();
    modules.register("nscp", Arc::new(nscp::init));
    o.modules = modules;
    let nsd = Nsd::new(o).unwrap();
    nsd.start().unwrap();
    let port = nsd.server("s1").unwrap().nsv.with("nscp", false, |a| {
        a.unwrap().get("port").unwrap().parse().unwrap()
    });
    Srv {
        nsd,
        _home: home,
        port,
    }
}

impl Srv {
    /// Send `input`, half-close, then read until the server closes.
    fn session(&self, input: &[u8]) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.write_all(input).unwrap();
        s.shutdown(std::net::Shutdown::Write).unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        String::from_utf8_lossy(&out).into_owned()
    }

    fn log(&self) -> Vec<String> {
        self.nsd.global.log.captured()
    }

    /// Wait until at least `n` "disconnected" lines have been logged.
    fn wait_disconnects(&self, n: usize) -> Vec<String> {
        let start = Instant::now();
        loop {
            let lines: Vec<String> = self
                .log()
                .into_iter()
                .filter(|l| l.contains("nscp: 127.0.0.1 disconnected"))
                .collect();
            if lines.len() >= n || start.elapsed() > Duration::from_secs(10) {
                return lines;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn admin() -> String {
    format!("admin:{}", ns_core::crypt::crypt("secret", "xy"))
}

#[test]
fn exit_says_goodbye_and_checks_args() {
    let srv = boot(&[&admin()]);
    let out = srv.session(b"admin\nsecret\nexit 1\nexpr {1 + 2}\nexit\nexpr 99\n");
    assert!(
        out.contains("wrong # args: should be \"exit\"\r\n"),
        "{out}"
    );
    // The session continued after the failed exit.
    assert!(out.contains("3\r\n"), "{out}");
    assert!(out.ends_with("\nGoodbye!\r\n"), "{out}");
    assert!(!out.contains("99"), "{out}");
}

#[test]
fn empty_password_entry_accepts_any_password() {
    // The stock config ships `ns_param user ":"`: Ns_Encrypt with an empty
    // salt yields a string that is empty up to its first NUL.
    let srv = boot(&["open:"]);
    let out = srv.session(b"open\nanything\nexpr {6 * 7}\nexit\n");
    assert!(out.contains("Welcome to s1"), "{out}");
    assert!(out.contains("42\r\n"), "{out}");
}

#[test]
fn partial_line_at_eof_is_not_evaluated() {
    let srv = boot(&[&admin()]);
    let out = srv.session(b"admin\nsecret\nnsv_set cp k v");
    assert!(out.contains("Welcome to s1"), "{out}");
    srv.wait_disconnects(1);
    let v = srv
        .nsd
        .server("s1")
        .unwrap()
        .nsv
        .with("cp", false, |a| a.and_then(|a| a.get("k").cloned()));
    assert_eq!(v, None);
}

#[test]
fn partial_password_at_eof_is_access_denied() {
    let srv = boot(&["open:"]);
    let out = srv.session(b"open\nanything");
    assert!(out.contains("Access denied!"), "{out}");
    assert!(!out.contains("Welcome"), "{out}");
    srv.wait_disconnects(1);
    assert!(srv
        .log()
        .iter()
        .any(|l| l.contains("nscp: login failed: '?'")));
}

#[test]
fn interrupt_and_eot_end_the_session() {
    let srv = boot(&[&admin()]);
    // Ctrl-C in telnet: IAC IP IAC DO TIMING-MARK.
    let out = srv.session(b"admin\nsecret\n\xff\xf4\xff\xfd\x06expr 1\n");
    assert!(out.contains("Welcome to s1"), "{out}");
    assert!(!out.contains("1\r\n"), "{out}");
    assert!(!out.contains("invalid"), "{out}");
    // IAC EOF.
    let out = srv.session(b"admin\nsecret\n\xff\xecexpr 1\n");
    assert!(!out.contains("1\r\n"), "{out}");
    // A lone EOT (Ctrl-D).
    let out = srv.session(b"admin\nsecret\n\x04expr 1\n");
    assert!(!out.contains("1\r\n"), "{out}");
    assert!(!out.contains("invalid"), "{out}");
}

#[test]
fn unsupported_telnet_command_drops_with_a_warning() {
    let srv = boot(&[&admin()]);
    // IAC AYT (are-you-there).
    let out = srv.session(b"admin\nsecret\n\xff\xf6expr 1\n");
    assert!(!out.contains("1\r\n"), "{out}");
    assert_eq!(srv.wait_disconnects(1).len(), 1);
    assert!(srv
        .log()
        .iter()
        .any(|l| l.contains("nscp: unsupported telnet IAC code received from client")));
}

#[test]
fn disconnected_is_logged_on_every_path() {
    let srv = boot(&[&admin()]);
    assert!(srv.session(b"admin\nwrong\n").contains("Access denied!"));
    assert_eq!(srv.wait_disconnects(1).len(), 1);
    srv.session(b"");
    assert_eq!(srv.wait_disconnects(2).len(), 2);
    srv.session(b"admin\nsecret\nexit\n");
    assert_eq!(srv.wait_disconnects(3).len(), 3);
}
