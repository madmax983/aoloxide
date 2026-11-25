//! ADP commands (`nsd/adpcmds.c`, `nsd/adpparse.c` registration).

use std::sync::PoisonError;

use ns_script::{wrong_args, Command, Exception, Interp, ScriptResult};

use super::{bool_str, check_args, cmd, int_arg, server};
use crate::adp::{self, AdpException};
use crate::interp::ns_interp;
use crate::server::{AdpTag, AdpTagKind};

/// Commands defined here.
pub fn commands() -> Vec<(&'static str, Command)> {
    vec![
        ("ns_adp_abort", cmd(|i, a| raise(i, a, AdpException::Abort))),
        ("ns_adp_break", cmd(|i, a| raise(i, a, AdpException::Break))),
        (
            "ns_adp_return",
            cmd(|i, a| raise(i, a, AdpException::Return)),
        ),
        ("ns_adp_append", cmd(ns_adp_append)),
        ("ns_adp_puts", cmd(ns_adp_puts)),
        ("ns_adp_argc", cmd(ns_adp_argc)),
        ("ns_adp_argv", cmd(ns_adp_argv)),
        ("ns_adp_bind_args", cmd(ns_adp_bind_args)),
        ("ns_adp_close", cmd(ns_adp_close)),
        ("ns_adp_compress", cmd(ns_adp_compress)),
        ("ns_adp_ctl", cmd(ns_adp_ctl)),
        ("ns_adp_debug", cmd(ns_adp_debug)),
        ("ns_adp_dir", cmd(ns_adp_dir)),
        ("ns_adp_dump", cmd(ns_adp_dump)),
        ("ns_adp_eval", cmd(|i, a| eval_cmd(i, a, false))),
        ("ns_adp_safeeval", cmd(|i, a| eval_cmd(i, a, false))),
        ("ns_adp_parse", cmd(ns_adp_parse)),
        ("ns_adp_exception", cmd(ns_adp_exception)),
        ("ns_adp_flush", cmd(|i, a| flush(i, a))),
        ("ns_adp_ident", cmd(ns_adp_ident)),
        ("ns_adp_include", cmd(ns_adp_include)),
        ("_ns_adp_include", cmd(ns_adp_include)),
        ("ns_adp_mime", cmd(ns_adp_mimetype)),
        ("ns_adp_mimetype", cmd(ns_adp_mimetype)),
        ("ns_adp_registeradp", cmd(|i, a| register(i, a, "adp"))),
        ("ns_adp_registertag", cmd(|i, a| register(i, a, "adp"))),
        ("ns_adp_registerproc", cmd(|i, a| register(i, a, "proc"))),
        (
            "ns_adp_registerscript",
            cmd(|i, a| register(i, a, "script")),
        ),
        ("ns_register_adptag", cmd(|i, a| register(i, a, "script"))),
        ("ns_adp_stats", cmd(ns_adp_stats)),
        ("ns_adp_stream", cmd(ns_adp_stream)),
        ("ns_adp_tell", cmd(ns_adp_tell)),
        ("ns_adp_trunc", cmd(ns_adp_trunc)),
    ]
}

fn raise(interp: &dyn Interp, argv: &[String], ex: AdpException) -> ScriptResult {
    check_args(argv, 1, 2, 1, "?retval?")?;
    if let Some(v) = argv.get(1) {
        let _ = interp.set_global("_ns_adp_retval", v);
    }
    adp::raise(interp, ex)
}

fn ns_adp_append(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, usize::MAX, 1, "string ?string ...?")?;
    adp::append(interp, &argv[1..].concat())
}

/// `ns_adp_puts ?-nonewline? string`.
pub fn ns_adp_puts(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let nonl = argv.get(1).is_some_and(|a| a == "-nonewline");
    let a = if nonl { &argv[2..] } else { &argv[1..] };
    if a.len() != 1 {
        return Err(wrong_args(argv, 1, "?-nonewline? string"));
    }
    let mut s = a[0].clone();
    if !nonl {
        s.push('\n');
    }
    adp::append(interp, &s)
}

fn frame_args(interp: &dyn Interp) -> Result<Vec<String>, Exception> {
    let st = ns_interp(interp)?;
    let s = st.borrow();
    s.adp
        .frames
        .last()
        .map(|f| f.args.clone())
        .ok_or_else(|| Exception::error("no active adp"))
}

fn ns_adp_argc(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 1, 1, 1, "")?;
    Ok(frame_args(interp)?.len().to_string())
}

fn ns_adp_argv(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 1, 3, 1, "?index? ?default?")?;
    let args = frame_args(interp)?;
    match argv.get(1) {
        None => Ok(interp.list_from(&args)),
        Some(i) => {
            let i = int_arg(i)?;
            Ok(usize::try_from(i)
                .ok()
                .and_then(|i| args.get(i).cloned())
                .or_else(|| argv.get(2).cloned())
                .unwrap_or_default())
        }
    }
}

fn ns_adp_bind_args(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, usize::MAX, 1, "var1 ?var2 ...?")?;
    let args = frame_args(interp)?;
    let vars = &argv[1..];
    if args.len().saturating_sub(1) != vars.len() {
        return Err(Exception::error(format!(
            "invalid #variables: should be {} but got {}",
            args.len().saturating_sub(1),
            vars.len()
        )));
    }
    for (v, a) in vars.iter().zip(args.iter().skip(1)) {
        interp.set_var(v, a)?;
    }
    Ok(String::new())
}

fn ns_adp_close(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 1, 1, 1, "")?;
    adp::flush_output(interp, false)?;
    super::conn(interp)?.close();
    Ok(String::new())
}

fn ns_adp_compress(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 1, 2, 1, "?boolean?")?;
    let on = argv.get(1).map_or(Ok(true), |b| ns_script::parse_bool(b))?;
    ns_interp(interp)?.borrow_mut().adp.gzip = on;
    Ok(String::new())
}

fn ns_adp_ctl(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let opts = &[
        "bufsize",
        "channel",
        "autoabort",
        "detailerror",
        "displayerror",
        "expire",
        "gzip",
        "nocache",
        "safe",
        "singlescript",
        "stricterror",
        "trace",
        "trimspace",
        "stream",
    ];
    let (_, opt) = super::subcommand(argv, opts)?;
    let st = ns_interp(interp)?;
    let mut s = st.borrow_mut();
    let a = &mut s.adp;
    if opt == "bufsize" {
        let old = a.bufsize;
        if let Some(v) = argv.get(2) {
            a.bufsize = int_arg(v)?.max(0) as usize;
        }
        return Ok(old.to_string());
    }
    if opt == "channel" {
        return Err(Exception::error(
            "ns_adp_ctl channel: output channels are not supported",
        ));
    }
    let slot: Option<&mut bool> = match opt {
        "autoabort" => Some(&mut a.autoabort),
        "displayerror" => Some(&mut a.display),
        "expire" => Some(&mut a.expire),
        "gzip" => Some(&mut a.gzip),
        "stricterror" => Some(&mut a.strict),
        "trimspace" => Some(&mut a.trim),
        "stream" => Some(&mut a.stream),
        _ => None,
    };
    match slot {
        Some(flag) => {
            let old = *flag;
            if let Some(v) = argv.get(2) {
                *flag = ns_script::parse_bool(v)?;
            }
            Ok(bool_str(old))
        }
        None => Ok("0".into()),
    }
}

fn ns_adp_debug(_interp: &dyn Interp, _argv: &[String]) -> ScriptResult {
    Err(Exception::error(
        "ns_adp_debug: the TclPro debugger is not supported",
    ))
}

fn ns_adp_dir(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 1, 1, 1, "")?;
    let st = ns_interp(interp)?;
    let s = st.borrow();
    Ok(s.adp
        .frames
        .last()
        .map_or_else(|| s.server.fastpath.pageroot.clone(), |f| f.cwd.clone())
        .to_string_lossy()
        .into_owned())
}

fn ns_adp_dump(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 1, 1, 1, "")?;
    let st = ns_interp(interp)?;
    let s = st.borrow();
    Ok(s.adp.outputs.last().cloned().unwrap_or_default())
}

fn eval_cmd(interp: &dyn Interp, argv: &[String], file: bool) -> ScriptResult {
    let mut i = 1;
    let mut file = file;
    while i < argv.len() && argv[i].starts_with('-') {
        match argv[i].as_str() {
            "-safe" | "-string" => {}
            "-file" => file = true,
            _ => break,
        }
        i += 1;
    }
    let target = argv
        .get(i)
        .ok_or_else(|| wrong_args(argv, 1, "?-safe? page ?args ...?"))?;
    adp::eval_capture(interp, target, file, &argv[i + 1..])
}

fn ns_adp_parse(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    eval_cmd(interp, argv, false)
}

fn ns_adp_exception(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 1, 2, 1, "?varName?")?;
    let ex = ns_interp(interp)?.borrow().adp.exception;
    let name = match ex {
        AdpException::Ok => "ok",
        AdpException::Break => "break",
        AdpException::Abort => "abort",
        AdpException::Return => "return",
    };
    if let Some(v) = argv.get(1) {
        interp.set_var(v, name)?;
    }
    Ok(bool_str(ex != AdpException::Ok))
}

fn flush(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 1, 1, 1, "")?;
    adp::flush_output(interp, true)
}

fn ns_adp_ident(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 1, 2, 1, "?ident?")?;
    let st = ns_interp(interp)?;
    let mut s = st.borrow_mut();
    let f = s
        .adp
        .frames
        .last_mut()
        .ok_or_else(|| Exception::error("no active adp"))?;
    if let Some(v) = argv.get(1) {
        f.ident.clone_from(v);
    }
    Ok(f.ident.clone())
}

fn ns_adp_include(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let mut i = 1;
    while i < argv.len() && argv[i].starts_with('-') {
        match argv[i].as_str() {
            "-nocache" | "-sync" => i += 1,
            "-cache" => i += 2,
            _ => break,
        }
    }
    let file = argv
        .get(i)
        .ok_or_else(|| wrong_args(argv, 1, "?-cache ttl | -nocache? file ?args ...?"))?;
    adp::include(interp, file, &argv[i + 1..])
}

fn ns_adp_mimetype(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 1, 2, 1, "?mimetype?")?;
    let st = ns_interp(interp)?;
    if let Some(t) = argv.get(1) {
        st.borrow_mut().adp.mimetype = Some(t.clone());
        if let Ok(c) = super::conn(interp) {
            c.set_type(Some(t));
        }
    }
    let cur = st.borrow().adp.mimetype.clone();
    Ok(cur
        .or_else(|| {
            super::conn(interp)
                .ok()
                .and_then(|c| c.conn().content_type.clone())
        })
        .unwrap_or_default())
}

fn register(interp: &dyn Interp, argv: &[String], kind: &str) -> ScriptResult {
    if argv.len() != 3 && argv.len() != 4 {
        return Err(wrong_args(argv, 1, "tag ?endtag? string"));
    }
    let srv = server(interp)?;
    let name = argv[1].to_lowercase();
    let endtag = (argv.len() == 4).then(|| argv[2].to_lowercase());
    let text = argv[argv.len() - 1].clone();
    let kind = match kind {
        "adp" => AdpTagKind::Adp(text),
        "proc" => AdpTagKind::Proc(text),
        _ => AdpTagKind::Script(text),
    };
    srv.adp_tags
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(name.clone(), AdpTag { name, endtag, kind });
    srv.adp_pages
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
    Ok(String::new())
}

fn ns_adp_stats(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 1, 1, 1, "")?;
    let srv = server(interp)?;
    let pages = srv.adp_pages.lock().unwrap_or_else(PoisonError::into_inner);
    let mut out = Vec::new();
    for (path, (mtime, size, code)) in pages.iter() {
        out.push(path.to_string_lossy().into_owned());
        out.push(interp.list_from(&[
            "mtime".into(),
            mtime.to_string(),
            "size".into(),
            size.to_string(),
            "blocks".into(),
            code.blocks.len().to_string(),
            "scripts".into(),
            code.nscripts().to_string(),
        ]));
    }
    Ok(interp.list_from(&out))
}

fn ns_adp_stream(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 1, 1, 1, "")?;
    ns_interp(interp)?.borrow_mut().adp.stream = true;
    adp::flush_output(interp, true)
}

fn ns_adp_tell(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 1, 1, 1, "")?;
    Ok(ns_interp(interp)?
        .borrow()
        .adp
        .outputs
        .last()
        .map_or(0, String::len)
        .to_string())
}

fn ns_adp_trunc(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 1, 2, 1, "?length?")?;
    let len = argv.get(1).map_or(Ok(0), |l| int_arg(l))?;
    if len < 0 {
        return Err(Exception::error(format!("invalid length: {len}")));
    }
    let st = ns_interp(interp)?;
    let mut s = st.borrow_mut();
    if let Some(top) = s.adp.outputs.last_mut() {
        let mut l = (len as usize).min(top.len());
        while !top.is_char_boundary(l) {
            l -= 1;
        }
        top.truncate(l);
    }
    Ok(String::new())
}
