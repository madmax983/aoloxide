//! Request, filter and trace registration (`nsd/tclrequest.c`).

use std::sync::{Arc, OnceLock, PoisonError};

use ns_core::log::Severity;
use ns_core::UrlFlags;
use ns_script::{wrong_args, Command, Exception, Interp, ScriptResult};

use super::{cmd, server};
use crate::adp::parse::tcl_quote;
use crate::conn::{ConnRef, Status};
use crate::op::{self, AuthRequest, Filter, FilterWhen, RequestEntry};

/// Commands defined here.
pub fn commands() -> Vec<(&'static str, Command)> {
    vec![
        ("ns_register_proc", cmd(ns_register_proc)),
        ("ns_unregister_proc", cmd(ns_unregister_proc)),
        ("ns_register_filter", cmd(ns_register_filter)),
        ("ns_register_trace", cmd(ns_register_trace)),
        ("ns_register_adp", cmd(ns_register_adp)),
        ("ns_unregister_adp", cmd(ns_unregister_proc)),
        ("ns_register_fastpath", cmd(ns_register_fastpath)),
        ("ns_register_proxy", cmd(ns_register_proxy)),
        ("ns_register_encoding", cmd(ns_register_encoding)),
        ("ns_requestauthorize", cmd(ns_requestauthorize)),
    ]
}

/// A Tcl proc registered as a request handler or filter (`struct Proc`).
#[derive(Debug)]
struct ScriptProc {
    name: String,
    args: Option<String>,
    nargs: OnceLock<i64>,
}

impl ScriptProc {
    fn new(name: &str, args: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_owned(),
            args: args.map(str::to_owned),
            nargs: OnceLock::new(),
        })
    }

    /// Number of formal arguments, or -1 if not a Tcl proc (`GetNumArgs`).
    fn nargs(&self, interp: &dyn Interp) -> i64 {
        *self.nargs.get_or_init(|| {
            interp
                .eval_words(&["info", "args", &self.name])
                .ok()
                .and_then(|a| interp.list_split(&a).ok())
                .map_or(-1, |v| v.len() as i64)
        })
    }

    fn conn_id(conn: &ConnRef) -> String {
        format!("cns{}", conn.id())
    }

    fn request_script(&self, interp: &dyn Interp, conn: &ConnRef) -> String {
        let mut s = tcl_quote(&self.name);
        let n = self.nargs(interp);
        if n != 0 {
            if n > 1 {
                s.push(' ');
                s.push_str(&Self::conn_id(conn));
            }
            s.push(' ');
            s.push_str(&tcl_quote(self.args.as_deref().unwrap_or("")));
        }
        s
    }

    fn filter_script(&self, interp: &dyn Interp, conn: &ConnRef, why: FilterWhen) -> String {
        let mut s = tcl_quote(&self.name);
        let n = self.nargs(interp);
        if n > 1 {
            if n > 2 {
                s.push(' ');
                s.push_str(&Self::conn_id(conn));
            }
            s.push(' ');
            s.push_str(&tcl_quote(self.args.as_deref().unwrap_or("")));
        }
        // As in ProcFilter: void traces (ns_register_trace) get no `why`.
        if why != FilterWhen::VoidTrace {
            s.push(' ');
            s.push_str(why.name());
        }
        s
    }
}

/// `ProcRequest`: invoke a registered Tcl proc for a connection.
fn proc_request(p: &ScriptProc, conn: &ConnRef) -> Status {
    let interp = match conn.interp() {
        Ok(i) => i,
        Err(e) => {
            conn.server.global.log(Severity::Error, &e.message);
            let _ = conn.return_internal_error();
            return Status::Ok;
        }
    };
    let script = p.request_script(&**interp, conn);
    if let Err(e) = interp.eval(&script) {
        crate::interp::log_error(&conn.server, &**interp, &e);
        if conn.reset_return() {
            let _ = conn.return_internal_error();
        }
    }
    Status::Ok
}

/// `ProcFilter`: invoke a Tcl filter proc and map its result.
fn proc_filter(p: &ScriptProc, conn: &ConnRef, why: FilterWhen) -> Status {
    let interp = match conn.interp() {
        Ok(i) => i,
        Err(e) => {
            conn.server.global.log(Severity::Error, &e.message);
            return Status::Error;
        }
    };
    let script = p.filter_script(&**interp, conn, why);
    let r = interp.eval(&script);
    if let Err(e) = &r {
        crate::interp::log_error(&conn.server, &**interp, e);
    }
    if why == FilterWhen::VoidTrace {
        return Status::Ok;
    }
    match r.as_deref() {
        Ok("filter_ok") => Status::Ok,
        Ok("filter_break") => Status::FilterBreak,
        Ok("filter_return") => Status::FilterReturn,
        Ok(other) => {
            conn.server.global.log(
                Severity::Warning,
                &format!("tclfilter: {} return invalid result: {other}", p.name),
            );
            Status::Error
        }
        Err(_) => Status::Error,
    }
}

fn ns_register_proc(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let usage = "?-noinherit? method url proc ?args?";
    let (noinherit, a) = match argv.get(1).map(String::as_str) {
        Some("-noinherit") => (true, &argv[2..]),
        _ => (false, &argv[1..]),
    };
    if a.len() < 3 || a.len() > 4 {
        return Err(wrong_args(argv, 1, usage));
    }
    let srv = server(interp)?;
    let p = ScriptProc::new(&a[2], a.get(3).map(String::as_str));
    let desc = p.name.clone();
    op::register_request(
        &srv,
        &a[0],
        &a[1],
        RequestEntry {
            proc_: Arc::new(move |c| proc_request(&p, c)),
            desc,
        },
        UrlFlags {
            noinherit,
            recurse: false,
        },
    );
    Ok(String::new())
}

fn ns_unregister_proc(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (noinherit, recurse, a) = match argv.get(1).map(String::as_str) {
        Some("-noinherit") => (true, false, &argv[2..]),
        Some("-recurse") => (false, true, &argv[2..]),
        _ => (false, false, &argv[1..]),
    };
    if a.len() != 2 {
        return Err(wrong_args(argv, 1, "?-noinherit? method url"));
    }
    let srv = server(interp)?;
    op::unregister_request(&srv, &a[0], &a[1], noinherit, recurse);
    Ok(String::new())
}

fn ns_register_filter(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    if argv.len() < 5 || argv.len() > 6 {
        return Err(wrong_args(argv, 1, "when method url script ?arg?"));
    }
    let whens = interp.list_split(&argv[1])?;
    let mut mask = 0;
    for w in &whens {
        let when = FilterWhen::parse(w).ok_or_else(|| {
            Exception::error(format!(
                "bad when \"{w}\": must be read, write, prequeue, preauth, postauth, or trace"
            ))
        })?;
        mask |= when.bit();
    }
    if mask == 0 {
        return Err(Exception::error("blank filter when specification"));
    }
    register_filter(interp, mask, &argv[2..])
}

fn register_filter(interp: &dyn Interp, mask: u32, a: &[String]) -> ScriptResult {
    let srv = server(interp)?;
    let p = ScriptProc::new(&a[2], a.get(3).map(String::as_str));
    let desc = p.name.clone();
    op::register_filter(
        &srv,
        Filter {
            method: a[0].clone(),
            url: a[1].clone(),
            when: mask,
            proc_: Arc::new(move |c, why| proc_filter(&p, c, why)),
            desc,
        },
    );
    Ok(String::new())
}

fn ns_register_trace(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    if argv.len() < 4 || argv.len() > 5 {
        return Err(wrong_args(argv, 1, "method urlPattern script ?arg?"));
    }
    register_filter(interp, FilterWhen::VoidTrace.bit(), &argv[1..])
}

fn ns_register_adp(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (noinherit, a) = match argv.get(1).map(String::as_str) {
        Some("-noinherit") => (true, &argv[2..]),
        _ => (false, &argv[1..]),
    };
    if a.len() != 3 {
        return Err(wrong_args(argv, 1, "?-noinherit? method url file"));
    }
    let srv = server(interp)?;
    crate::adp::register_file(&srv, &a[0], &a[1], &a[2], noinherit);
    Ok(String::new())
}

fn ns_register_fastpath(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (noinherit, a) = match argv.get(1).map(String::as_str) {
        Some("-noinherit") => (true, &argv[2..]),
        _ => (false, &argv[1..]),
    };
    if a.len() != 2 {
        return Err(wrong_args(argv, 1, "?-noinherit? method url"));
    }
    let srv = server(interp)?;
    crate::fastpath::register(
        &srv,
        &a[0],
        &a[1],
        UrlFlags {
            noinherit,
            recurse: false,
        },
    );
    Ok(String::new())
}

fn ns_register_proxy(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    if argv.len() < 4 || argv.len() > 5 {
        return Err(wrong_args(argv, 1, "method protocol script ?args?"));
    }
    let srv = server(interp)?;
    let p = ScriptProc::new(&argv[3], argv.get(4).map(String::as_str));
    let desc = p.name.clone();
    op::register_proxy(
        &srv,
        &argv[1],
        &argv[2],
        RequestEntry {
            proc_: Arc::new(move |c| proc_request(&p, c)),
            desc,
        },
    );
    Ok(String::new())
}

fn ns_register_encoding(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (noinherit, a) = match argv.get(1).map(String::as_str) {
        Some("-noinherit") => (true, &argv[2..]),
        _ => (false, &argv[1..]),
    };
    if a.len() != 3 {
        return Err(wrong_args(argv, 1, "?-noinherit? method url charset"));
    }
    let srv = server(interp)?;
    srv.url2encoding
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .set(
            &a[0],
            &a[1],
            a[2].clone(),
            UrlFlags {
                noinherit,
                recurse: false,
            },
        );
    Ok(String::new())
}

fn ns_requestauthorize(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    if argv.len() != 5 && argv.len() != 6 {
        return Err(wrong_args(
            argv,
            1,
            "method url authuser authpasswd ?ipaddr?",
        ));
    }
    let srv = server(interp)?;
    let req = AuthRequest {
        server: srv.name.clone(),
        method: argv[1].clone(),
        url: argv[2].clone(),
        user: Some(argv[3].clone()),
        passwd: Some(argv[4].clone()),
        peer: argv.get(5).cloned().unwrap_or_default(),
    };
    Ok(match op::authorize_request(&srv, &req) {
        Status::Ok => "OK",
        Status::Forbidden => "FORBIDDEN",
        Status::Unauthorized => "UNAUTHORIZED",
        _ => "ERROR",
    }
    .to_owned())
}
