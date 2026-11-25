//! `nsd`: the AOLserver server binary (`nsd/main.c`, `nsd/nsmain.c`).

use std::ffi::CString;
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use nsd::{Nsd, NsdOptions};

fn set_ids(user: Option<&str>, group: Option<&str>) -> Result<()> {
    if let Some(g) = group {
        let c = CString::new(g)?;
        // SAFETY: c is a valid C string; getgrnam returns NULL or a
        // pointer to static storage we read immediately.
        let gr = unsafe { libc::getgrnam(c.as_ptr()) };
        let gid = if gr.is_null() {
            g.parse::<libc::gid_t>()
                .with_context(|| format!("no such group: {g}"))?
        } else {
            // SAFETY: gr is non-null.
            unsafe { (*gr).gr_gid }
        };
        // SAFETY: setgid has no memory-safety preconditions.
        if unsafe { libc::setgid(gid) } != 0 {
            bail!("setgid({gid}) failed: {}", std::io::Error::last_os_error());
        }
    }
    if let Some(u) = user {
        let c = CString::new(u)?;
        // SAFETY: as above for getpwnam.
        let pw = unsafe { libc::getpwnam(c.as_ptr()) };
        let uid = if pw.is_null() {
            u.parse::<libc::uid_t>()
                .with_context(|| format!("no such user: {u}"))?
        } else {
            // SAFETY: pw is non-null.
            unsafe { (*pw).pw_uid }
        };
        // SAFETY: setuid has no memory-safety preconditions.
        if unsafe { libc::setuid(uid) } != 0 {
            bail!("setuid({uid}) failed: {}", std::io::Error::last_os_error());
        }
    }
    Ok(())
}

fn daemonize() -> Result<()> {
    // SAFETY: called before any threads are created.
    match unsafe { libc::fork() } {
        -1 => bail!("fork failed: {}", std::io::Error::last_os_error()),
        0 => {
            // SAFETY: setsid in the child has no preconditions.
            unsafe { libc::setsid() };
            Ok(())
        }
        _ => std::process::exit(0),
    }
}

fn run() -> Result<()> {
    let args = aoloxide::parse_args(std::env::args().skip(1)).map_err(anyhow::Error::msg)?;
    if args.help {
        print!("{}", aoloxide::USAGE);
        return Ok(());
    }
    if args.version {
        println!(
            "{}/{} (aoloxide {})",
            nsd::global::SERVER_NAME,
            nsd::global::SERVER_VERSION,
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }
    if let Some(home) = &args.install_home {
        let n = aoloxide::install_home(home, false)?;
        println!("installed {n} files under {}", home.display());
        return Ok(());
    }
    let Some(config) = &args.config else {
        eprint!("{}", aoloxide::USAGE);
        bail!("required -t <config> option not specified");
    };
    let mut opts = NsdOptions::from_file(config)?;
    opts.args.clone_from(&args.rest);
    opts.server.clone_from(&args.server);
    opts.log_stderr = args.foreground;
    opts.modules = aoloxide::modules();
    if args.check {
        nsd::eval_config(&ns_tcl::TclEngine::new(), &opts.config_text, &opts.args)?;
        println!("config ok: {}", config.display());
        return Ok(());
    }
    if !args.foreground && !args.inittab {
        daemonize()?;
    }
    let server = Nsd::new(opts)?;
    set_ids(args.user.as_deref(), args.group.as_deref())?;
    server.install_signal_handlers();
    server.start()?;
    server.wait();
    server.shutdown();
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("nsd: {e:#}");
            ExitCode::FAILURE
        }
    }
}
