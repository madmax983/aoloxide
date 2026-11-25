//! # aoloxide
//!
//! AOLserver ported to Rust.  This crate assembles the `nsd` binary: the
//! server core plus every compiled-in module, and the command-line
//! handling of `nsd/nsmain.c`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use nsd::ModuleRegistry;

/// All modules shipped with aoloxide, keyed by the names used in
/// `ns/server/S/modules` (`nssock`, `nslog`, `nsperm`, `nscgi`, `nsssl`,
/// `nszlib`).
pub fn modules() -> ModuleRegistry {
    let mut r = nsd::builtin_modules();
    r.register("nslog", Arc::new(nslog::init));
    r.register("nsperm", Arc::new(nsperm::init));
    r.register("nscgi", Arc::new(nscgi::init));
    r.register("nsssl", Arc::new(nssl::init));
    r.register("nsopenssl", Arc::new(nssl::init));
    r.register("nszlib", Arc::new(nszlib::init));
    r
}

/// Create a server home with the Tcl libraries of the core and all
/// modules (`--install-home`).  Returns the number of files written.
pub fn install_home(home: &Path, overwrite: bool) -> std::io::Result<usize> {
    let mut n = nsd::init_tcl::install_home(home, overwrite)?;
    for (rel, contents) in nsperm::TCL_LIBRARY {
        let p = home.join(rel);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        if overwrite || !p.exists() {
            std::fs::write(&p, contents)?;
            n += 1;
        }
    }
    Ok(n)
}

/// Install nsperm's default data files for `server`.
pub fn install_perm_files(home: &Path, server: &str) -> std::io::Result<()> {
    let dir = home
        .join("servers")
        .join(server)
        .join("modules")
        .join("nsperm");
    std::fs::create_dir_all(&dir)?;
    for (name, contents) in nsperm::DATA_FILES {
        let p = dir.join(name);
        if !p.exists() {
            std::fs::write(p, contents)?;
        }
    }
    Ok(())
}

/// Parsed command line (`Ns_Main` options).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Args {
    /// `-f`: run in the foreground, log to stderr.
    pub foreground: bool,
    /// `-i`: inittab mode (foreground, log to file).
    pub inittab: bool,
    /// `-d`: debug (implies foreground).
    pub debug: bool,
    /// `-V`: print version and exit.
    pub version: bool,
    /// `-h`: print usage and exit.
    pub help: bool,
    /// `-t file`: config file.
    pub config: Option<PathBuf>,
    /// `-s server`: only start this server.
    pub server: Option<String>,
    /// `-u user`: run as this user.
    pub user: Option<String>,
    /// `-g group`: run as this group.
    pub group: Option<String>,
    /// `--install-home dir`: create a server home and exit.
    pub install_home: Option<PathBuf>,
    /// `-c`: check the config file and exit.
    pub check: bool,
    /// Remaining arguments passed to the config script as `argv`.
    pub rest: Vec<String>,
}

/// Usage text.
pub const USAGE: &str =
    "Usage: nsd [-h|V] [-i|f|d] [-c] [-u <user>] [-g <group>] [-s <server>] -t <file>\n\
       nsd --install-home <dir>\n\n\
  -h  help (this message)\n\
  -V  version and release information\n\
  -i  inittab mode (foreground, log to file)\n\
  -f  foreground mode (log to stderr)\n\
  -d  debug mode (foreground, debug logging)\n\
  -c  check the config file and exit\n\
  -u  run as <user>\n\
  -g  run as <group>\n\
  -s  start only <server>\n\
  -t  read config from <file> (REQUIRED)\n";

/// Parse arguments (excluding `argv[0]`).
pub fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let mut val = |name: &str| {
            it.next()
                .ok_or_else(|| format!("option {name} requires an argument"))
        };
        match arg.as_str() {
            "-f" => a.foreground = true,
            "-i" => a.inittab = true,
            "-d" => {
                a.debug = true;
                a.foreground = true;
            }
            "-V" => a.version = true,
            "-h" | "--help" => a.help = true,
            "-c" => a.check = true,
            "-t" => a.config = Some(PathBuf::from(val("-t")?)),
            "-s" => a.server = Some(val("-s")?),
            "-u" => a.user = Some(val("-u")?),
            "-g" => a.group = Some(val("-g")?),
            "--install-home" => a.install_home = Some(PathBuf::from(val("--install-home")?)),
            "--" => {
                a.rest.extend(it.by_ref());
                break;
            }
            s if s.starts_with('-') => return Err(format!("unknown option: {s}")),
            s => a.rest.push(s.to_owned()),
        }
    }
    if a.foreground && a.inittab {
        return Err("-f and -i are mutually exclusive".into());
    }
    Ok(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(v: &[&str]) -> Result<Args, String> {
        parse_args(v.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn args() {
        let a = p(&["-f", "-t", "c.tcl", "-s", "s1", "x"]).expect("args");
        assert!(a.foreground);
        assert_eq!(a.config, Some("c.tcl".into()));
        assert_eq!(a.server.as_deref(), Some("s1"));
        assert_eq!(a.rest, vec!["x"]);
        assert!(p(&["-d"]).expect("d").foreground);
        assert!(p(&["-t"]).is_err());
        assert!(p(&["-z"]).is_err());
        assert!(p(&["-f", "-i"]).is_err());
        assert_eq!(p(&["--", "-f"]).expect("rest").rest, vec!["-f"]);
        assert!(p(&[
            "-V",
            "-h",
            "-c",
            "-u",
            "u",
            "-g",
            "g",
            "--install-home",
            "/x"
        ])
        .is_ok());
    }

    #[test]
    fn modules_and_home() {
        let m = modules();
        for n in ["nssock", "nslog", "nsperm", "nscgi", "nsssl", "nszlib"] {
            assert!(m.get(n).is_some(), "{n}");
        }
        let d = tempfile::tempdir().expect("tmp");
        assert!(install_home(d.path(), false).expect("install") > 10);
        install_perm_files(d.path(), "s1").expect("perm");
        assert!(d.path().join("modules/tcl/nsperm/init.tcl").is_file());
        assert!(d.path().join("servers/s1/modules/nsperm/passwd").is_file());
    }
}
