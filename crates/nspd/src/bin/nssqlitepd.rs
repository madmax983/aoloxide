//! `nssqlitepd`: an external-driver proxy daemon serving SQLite databases
//! (`Ns_PdMain`).  The pool's datasource is the database file.
//!
//! ```text
//! nssqlitepd          # serve one nsext connection on stdin/stdout
//! nssqlitepd <port>   # listen on <port>, one thread per connection
//! ```
//!
//! As in AOLserver, the daemon detaches (forks, the parent exits 0 and the
//! child starts a new session) unless its parent is `init`, changes to
//! `/`, and logs to syslog under its program name.

use std::ffi::CString;
use std::sync::Arc;

use nspd::{DriverDb, Level, Options};

fn syslog(level: Level, m: &str) {
    let prio = match level {
        Level::Notice => libc::LOG_NOTICE,
        Level::Error | Level::Trace => libc::LOG_ERR,
    };
    if let Ok(c) = CString::new(m.replace('\0', " ")) {
        // SAFETY: both pointers are valid NUL-terminated strings and the
        // format consumes exactly one string argument.
        unsafe { libc::syslog(prio, c"%s".as_ptr(), c.as_ptr()) };
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // SAFETY: called before any thread is started.
    unsafe {
        if libc::getppid() != 1 {
            if libc::fork() > 0 {
                libc::_exit(0);
            }
            libc::setsid();
        }
    }
    let bin = args.first().map_or("nsextd", String::as_str);
    let ident = bin.rsplit('/').next().unwrap_or(bin);
    let _ = std::env::set_current_dir("/");
    if let Ok(id) = CString::new(ident) {
        // openlog keeps the pointer: leak it for the life of the process.
        let id: &'static std::ffi::CStr = Box::leak(id.into_boxed_c_str());
        // SAFETY: `id` lives for the rest of the process.
        unsafe {
            libc::openlog(
                id.as_ptr(),
                libc::LOG_PID | libc::LOG_CONS,
                libc::LOG_DAEMON,
            );
        }
    }
    let opts = Options {
        log: Arc::new(syslog),
        ..Options::default()
    };
    if let Some(arg) = args.get(1) {
        let Some(port) = nspd::parse_port(arg) else {
            syslog(Level::Error, &format!("nspd: invalid port '{arg}'"));
            std::process::exit(1);
        };
        let listener = match std::net::TcpListener::bind(("0.0.0.0", port)) {
            Ok(l) => l,
            Err(e) => {
                syslog(Level::Error, &format!("nspd: bind failed: {e}"));
                std::process::exit(1);
            }
        };
        if let Err(e) = nspd::serve(&listener, || Some(DriverDb::sqlite()), &opts) {
            syslog(Level::Error, &format!("nspd: accept failed: {e}"));
            std::process::exit(1);
        }
    } else {
        nspd::main_loop(
            std::io::stdin().lock(),
            std::io::stdout().lock(),
            || Some(DriverDb::sqlite()),
            &opts,
        );
    }
    // SAFETY: closelog has no preconditions.
    unsafe { libc::closelog() };
}
