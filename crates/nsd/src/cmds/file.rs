//! File-system commands (`nsd/tclfile.c`).

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use ns_script::{wrong_args, Command, Exception, Interp, ScriptResult};

use super::{check_args, cmd, int_arg, server};

/// Commands defined here.
pub fn commands() -> Vec<(&'static str, Command)> {
    vec![
        ("ns_mkdir", cmd(ns_mkdir)),
        ("ns_rmdir", cmd(ns_rmdir)),
        ("ns_unlink", cmd(ns_unlink)),
        ("ns_rename", cmd(ns_rename)),
        ("ns_cp", cmd(ns_cp)),
        ("ns_cpfp", cmd(ns_cpfp)),
        ("ns_link", cmd(ns_link)),
        ("ns_symlink", cmd(ns_symlink)),
        ("ns_chmod", cmd(ns_chmod)),
        ("ns_truncate", cmd(ns_truncate)),
        ("ns_ftruncate", cmd(ns_ftruncate)),
        ("ns_tmpnam", cmd(ns_tmpnam)),
        ("ns_mktemp", cmd(ns_mktemp)),
        ("ns_purgefiles", cmd(ns_purgefiles)),
        ("ns_rollfile", cmd(ns_rollfile)),
        ("ns_url2file", cmd(ns_url2file)),
    ]
}

fn io(op: &str, path: &str, e: std::io::Error) -> Exception {
    Exception::error(format!("{op} (\"{path}\") failed:  {e}"))
        .error_code(format!("POSIX {}", e.kind()))
}

fn ns_mkdir(_i: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 2, 1, "dir")?;
    fs::create_dir(&argv[1]).map_err(|e| io("mkdir", &argv[1], e))?;
    Ok(String::new())
}

fn ns_rmdir(_i: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 2, 1, "dir")?;
    fs::remove_dir(&argv[1]).map_err(|e| io("rmdir", &argv[1], e))?;
    Ok(String::new())
}

fn ns_unlink(_i: &dyn Interp, argv: &[String]) -> ScriptResult {
    let nocomplain = argv.get(1).is_some_and(|a| a == "-nocomplain");
    let file = argv
        .get(if nocomplain { 2 } else { 1 })
        .ok_or_else(|| wrong_args(argv, 1, "?-nocomplain? filename"))?;
    match fs::remove_file(file) {
        Ok(()) => Ok(String::new()),
        Err(e) if nocomplain && e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(io("unlink", file, e)),
    }
}

fn ns_rename(_i: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 3, 3, 1, "from to")?;
    fs::rename(&argv[1], &argv[2]).map_err(|e| io("rename", &argv[1], e))?;
    Ok(String::new())
}

fn ns_cp(_i: &dyn Interp, argv: &[String]) -> ScriptResult {
    let preserve = argv.get(1).is_some_and(|a| a == "-preserve");
    let a = if preserve { &argv[2..] } else { &argv[1..] };
    if a.len() != 2 {
        return Err(wrong_args(argv, 1, "?-preserve? srcfile dstfile"));
    }
    fs::copy(&a[0], &a[1]).map_err(|e| io("copy", &a[0], e))?;
    if preserve {
        if let Ok(m) = fs::metadata(&a[0]) {
            let _ = fs::set_permissions(&a[1], m.permissions());
        }
    }
    Ok(String::new())
}

fn ns_cpfp(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 3, 4, 1, "ichan ochan ?ncopy?")?;
    let mut words = vec!["fcopy", argv[1].as_str(), argv[2].as_str()];
    if let Some(n) = argv.get(3) {
        words.push("-size");
        words.push(n);
    }
    interp.eval_words(&words)
}

fn ns_link(_i: &dyn Interp, argv: &[String]) -> ScriptResult {
    let nocomplain = argv.get(1).is_some_and(|a| a == "-nocomplain");
    let a = if nocomplain { &argv[2..] } else { &argv[1..] };
    if a.len() != 2 {
        return Err(wrong_args(argv, 1, "?-nocomplain? filename1 filename2"));
    }
    match fs::hard_link(&a[0], &a[1]) {
        Ok(()) => Ok("1".into()),
        Err(_) if nocomplain => Ok("0".into()),
        Err(e) => Err(io("link", &a[0], e)),
    }
}

fn ns_symlink(_i: &dyn Interp, argv: &[String]) -> ScriptResult {
    let nocomplain = argv.get(1).is_some_and(|a| a == "-nocomplain");
    let a = if nocomplain { &argv[2..] } else { &argv[1..] };
    if a.len() != 2 {
        return Err(wrong_args(argv, 1, "?-nocomplain? filename1 filename2"));
    }
    match std::os::unix::fs::symlink(&a[0], &a[1]) {
        Ok(()) => Ok("1".into()),
        Err(_) if nocomplain => Ok("0".into()),
        Err(e) => Err(io("symlink", &a[0], e)),
    }
}

fn ns_chmod(_i: &dyn Interp, argv: &[String]) -> ScriptResult {
    use std::os::unix::fs::PermissionsExt;
    check_args(argv, 3, 3, 1, "filename mode")?;
    let mode = u32::from_str_radix(argv[2].trim_start_matches("0o"), 8)
        .or_else(|_| int_arg(&argv[2]).map(|m| m as u32))
        .map_err(|_| Exception::error(format!("invalid mode: {}", argv[2])))?;
    fs::set_permissions(&argv[1], fs::Permissions::from_mode(mode))
        .map_err(|e| io("chmod", &argv[1], e))?;
    Ok(String::new())
}

fn ns_truncate(_i: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 3, 1, "file ?length?")?;
    let len = argv.get(2).map_or(Ok(0), |l| int_arg(l))?.max(0) as u64;
    let f = fs::OpenOptions::new()
        .write(true)
        .open(&argv[1])
        .map_err(|e| io("open", &argv[1], e))?;
    f.set_len(len).map_err(|e| io("truncate", &argv[1], e))?;
    Ok(String::new())
}

fn ns_ftruncate(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 3, 1, "fileId ?length?")?;
    let len = argv.get(2).map_or("0", String::as_str);
    interp.eval_words(&["chan", "truncate", &argv[1], len])
}

fn unique_name(dir: &Path, prefix: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("{prefix}{}{n}", std::process::id()))
        .to_string_lossy()
        .into_owned()
}

fn ns_tmpnam(_i: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 1, 1, 1, "")?;
    Ok(unique_name(&std::env::temp_dir(), "ns"))
}

fn ns_mktemp(_i: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 1, 2, 1, "?template?")?;
    let Some(t) = argv.get(1) else {
        return Ok(unique_name(&std::env::temp_dir(), "nsd-"));
    };
    let base = t.trim_end_matches('X');
    let nx = t.len() - base.len();
    loop {
        let mut r = String::new();
        while r.len() < nx.max(1) {
            r.push_str(&format!("{:016x}", super::misc::rand_u64()));
        }
        let suffix = &r[..nx.max(1)];
        let candidate = format!("{base}{suffix}");
        if !Path::new(&candidate).exists() {
            return Ok(candidate);
        }
    }
}

fn ns_purgefiles(_i: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 3, 3, 1, "file backupMax")?;
    let max = int_arg(&argv[2])?.max(0) as usize;
    ns_core::log::purge_files(Path::new(&argv[1]), max).map_err(|e| io("purge", &argv[1], e))?;
    Ok(String::new())
}

fn ns_rollfile(_i: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 3, 3, 1, "file backupMax")?;
    let max = int_arg(&argv[2])?;
    if !(0..=999).contains(&max) {
        return Err(Exception::error(format!("invalid max: {max}")));
    }
    ns_core::log::roll_file(Path::new(&argv[1]), max as usize)
        .map_err(|e| io("roll", &argv[1], e))?;
    Ok(String::new())
}

fn ns_url2file(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 2, 1, "url")?;
    let srv = server(interp)?;
    Ok(crate::fastpath::url_to_file(&srv, &argv[1])
        .to_string_lossy()
        .into_owned())
}
