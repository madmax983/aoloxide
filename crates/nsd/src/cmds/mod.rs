//! The `ns_*` script commands (`nsd/tclcmds.c` and friends), written once
//! against [`ns_script::Interp`].

pub mod adp;
pub mod cache;
pub mod conn;
pub mod file;
pub mod info;
pub mod misc;
pub mod nsv;
pub mod register;
pub mod sched;
pub mod set;
pub mod share;
pub mod thread;

use std::sync::{Arc, OnceLock};

use ns_script::{command, wrong_args, Command, Exception, Interp, ScriptResult};

use crate::conn::ConnRef;
use crate::server::Server;

/// A plain command function.
pub type CmdFn = fn(&dyn Interp, &[String]) -> ScriptResult;

/// Wrap a function as a [`Command`].
pub fn cmd(f: CmdFn) -> Command {
    command(f)
}

/// Every core command, built once.
pub fn core_commands() -> &'static [(&'static str, Command)] {
    static CMDS: OnceLock<Vec<(&'static str, Command)>> = OnceLock::new();
    CMDS.get_or_init(|| {
        let mut v = Vec::new();
        v.extend(set::commands());
        v.extend(conn::commands());
        v.extend(info::commands());
        v.extend(register::commands());
        v.extend(nsv::commands());
        v.extend(cache::commands());
        v.extend(sched::commands());
        v.extend(thread::commands());
        v.extend(misc::commands());
        v.extend(file::commands());
        v.extend(adp::commands());
        v
    })
}

/// The interp's server.
pub fn server(interp: &dyn Interp) -> Result<Arc<Server>, Exception> {
    crate::interp::interp_server(interp)
}

/// The interp's connection.
pub fn conn(interp: &dyn Interp) -> Result<ConnRef, Exception> {
    crate::interp::interp_conn(interp)
}

/// Require `min..=max` arguments (counting `argv[0]`).
pub fn check_args(
    argv: &[String],
    min: usize,
    max: usize,
    nfixed: usize,
    usage: &str,
) -> Result<(), Exception> {
    if argv.len() < min || argv.len() > max {
        Err(wrong_args(argv, nfixed, usage))
    } else {
        Ok(())
    }
}

/// Map an I/O result to a script result.
pub fn io_ok(r: std::io::Result<()>) -> ScriptResult {
    r.map(|()| String::new())
        .map_err(|e| Exception::error(format!("i/o error: {e}")))
}

/// Tcl-style boolean result.
pub fn bool_str(b: bool) -> String {
    if b { "1" } else { "0" }.to_owned()
}

/// Parse a Tcl integer argument.
pub fn int_arg(s: &str) -> Result<i64, Exception> {
    ns_script::parse_int(s)
}

/// Parse a status code argument.
pub fn status_arg(s: &str) -> Result<u16, Exception> {
    let n = int_arg(s)?;
    u16::try_from(n).map_err(|_| Exception::error(format!("invalid status code \"{s}\"")))
}

/// Subcommand dispatch helper: resolves `argv[1]` against `opts`.
pub fn subcommand<'a>(argv: &'a [String], opts: &[&'a str]) -> Result<(usize, &'a str), Exception> {
    if argv.len() < 2 {
        return Err(wrong_args(argv, 1, "option ?arg arg ...?"));
    }
    let i = ns_script::get_index(&argv[1], opts, "option")?;
    Ok((i, opts[i]))
}

/// Parse a time argument: `sec` or `sec:usec` or a float.
pub fn time_arg(s: &str) -> Result<std::time::Duration, Exception> {
    let t = s.trim();
    if let Some((a, b)) = t.split_once(':') {
        let secs = int_arg(a)?.max(0) as u64;
        let usec = int_arg(b)?.max(0) as u64;
        return Ok(std::time::Duration::from_secs(secs) + std::time::Duration::from_micros(usec));
    }
    let f = ns_script::parse_double(t)?;
    if f < 0.0 {
        return Ok(std::time::Duration::ZERO);
    }
    Ok(std::time::Duration::from_secs_f64(f))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        let argv: Vec<String> = vec!["x".into(), "y".into()];
        assert!(check_args(&argv, 2, 2, 1, "y").is_ok());
        assert!(check_args(&argv, 3, 3, 1, "y z").is_err());
        assert_eq!(bool_str(true), "1");
        assert_eq!(status_arg("404"), Ok(404));
        assert!(status_arg("70000").is_err());
        assert_eq!(time_arg("1:500000").expect("t").as_millis(), 1500);
        assert_eq!(time_arg("0.25").expect("t").as_millis(), 250);
        assert_eq!(time_arg("-1").expect("t"), std::time::Duration::ZERO);
        assert!(subcommand(&argv, &["yes", "no"]).is_ok());
        assert!(io_ok(Err(std::io::Error::other("x"))).is_err());
        assert!(!core_commands().is_empty());
    }
}
