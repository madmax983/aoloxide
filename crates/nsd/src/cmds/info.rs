//! Server introspection and control: `ns_info`, `ns_server`, `ns_config*`,
//! `ns_library`, `ns_modulepath`, `ns_ictl`, `ns_pools`, `ns_limits`,
//! `ns_driver`, `ns_shutdown`, `ns_kill`, `ns_markfordelete`.

use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};

use ns_core::glob::string_match;
use ns_core::UrlFlags;
use ns_script::{wrong_args, Command, Exception, Interp, ScriptResult};

use super::set::enter_set;
use super::{bool_str, check_args, cmd, int_arg, server};
use crate::conn::set_ref;
use crate::global::{SERVER_NAME, SERVER_VERSION};
use crate::interp::{ns_interp, register_trace, run_traces, TraceAction, TraceWhen};
use crate::pool::{LimitsConfig, PoolConfig};

/// Commands defined here.
pub fn commands() -> Vec<(&'static str, Command)> {
    vec![
        ("ns_info", cmd(ns_info)),
        ("ns_server", cmd(ns_server)),
        ("ns_config", cmd(ns_config)),
        ("ns_configsection", cmd(ns_configsection)),
        ("ns_configsections", cmd(ns_configsections)),
        ("ns_library", cmd(ns_library)),
        ("ns_modulepath", cmd(ns_modulepath)),
        ("ns_ictl", cmd(ns_ictl)),
        ("ns_interp_ctl", cmd(ns_ictl)),
        ("ns_markfordelete", cmd(ns_markfordelete)),
        ("ns_pools", cmd(ns_pools)),
        ("ns_limits", cmd(ns_limits)),
        ("ns_driver", cmd(ns_driver)),
        ("ns_shutdown", cmd(ns_shutdown)),
        ("ns_kill", cmd(ns_kill)),
    ]
}

const INFO_OPTS: &[&str] = &[
    "address",
    "argv0",
    "boottime",
    "builddate",
    "callbacks",
    "config",
    "home",
    "hostname",
    "label",
    "locks",
    "log",
    "major",
    "minor",
    "name",
    "nsd",
    "pageroot",
    "patchlevel",
    "pid",
    "platform",
    "pools",
    "scheduled",
    "server",
    "servers",
    "sockcallbacks",
    "tag",
    "tcllib",
    "threads",
    "uptime",
    "version",
    "winnt",
];

fn ns_info(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (_, opt) = super::subcommand(argv, INFO_OPTS)?;
    let srv = server(interp)?;
    let g = &srv.global;
    Ok(match opt {
        "address" => g.info.address.clone(),
        "argv0" => g.info.argv0.clone(),
        "boottime" => g.info.boot_time.to_string(),
        "builddate" | "label" | "tag" => g.info.build.clone(),
        "callbacks" | "locks" | "sockcallbacks" | "threads" => String::new(),
        "config" => g
            .info
            .config_file
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        "home" => g.info.home.to_string_lossy().into_owned(),
        "hostname" => g.info.hostname.clone(),
        "log" => g
            .log
            .path()
            .map_or_else(|| "STDERR".into(), |p| p.to_string_lossy().into_owned()),
        "major" => "4".into(),
        "minor" => "5".into(),
        "name" => SERVER_NAME.into(),
        "nsd" => g.info.nsd.to_string_lossy().into_owned(),
        "pageroot" => srv.fastpath.pageroot.to_string_lossy().into_owned(),
        "patchlevel" | "version" => SERVER_VERSION.into(),
        "pid" => g.info.pid.to_string(),
        "platform" => std::env::consts::OS.into(),
        "pools" => interp.list_from(
            &g.pools
                .all()
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>(),
        ),
        "scheduled" => {
            let ev: Vec<String> = g
                .sched
                .events()
                .iter()
                .map(|e| {
                    interp.list_from(&[
                        e.id.to_string(),
                        u8::from(e.paused).to_string(),
                        e.next_run.to_string(),
                        e.last_start.to_string(),
                        e.last_end.to_string(),
                        e.desc.clone(),
                    ])
                })
                .collect();
            interp.list_from(&ev)
        }
        "server" => srv.name.clone(),
        "servers" => interp.list_from(&g.server_names()),
        "tcllib" => srv.tcl.library.to_string_lossy().into_owned(),
        "uptime" => g.uptime().to_string(),
        "winnt" => "0".into(),
        _ => String::new(),
    })
}

fn ns_server(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let opts = &[
        "active",
        "all",
        "connections",
        "keepalive",
        "pools",
        "queued",
        "threads",
        "waiting",
    ];
    let mut args: Vec<String> = vec![argv[0].clone()];
    let mut srv = server(interp)?;
    let mut pool_name = None;
    let mut i = 1;
    while i < argv.len() {
        match argv[i].as_str() {
            "-server" if i + 1 < argv.len() => {
                srv = srv
                    .global
                    .server(&argv[i + 1])
                    .ok_or_else(|| Exception::error(format!("no such server: {}", argv[i + 1])))?;
                i += 2;
            }
            "-pool" if i + 1 < argv.len() => {
                pool_name = Some(argv[i + 1].clone());
                i += 2;
            }
            a => {
                args.push(a.to_owned());
                i += 1;
            }
        }
    }
    let (_, opt) = super::subcommand(&args, opts)?;
    let pool = match pool_name {
        Some(n) => srv
            .global
            .pools
            .get(&n)
            .ok_or_else(|| Exception::error(format!("no such pool: {n}")))?,
        None => srv.global.pools.default_pool(),
    };
    let stats = pool.stats();
    let cfg = pool.config();
    Ok(match opt {
        "pools" => interp.list_from(
            &srv.global
                .pools
                .all()
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>(),
        ),
        "threads" => interp.list_from(&[
            "min".into(),
            cfg.min.to_string(),
            "max".into(),
            cfg.max.to_string(),
            "current".into(),
            stats.current.to_string(),
            "idle".into(),
            stats.idle.to_string(),
            "stopping".into(),
            "0".into(),
        ]),
        "waiting" => stats.waiting.to_string(),
        "queued" => stats.queued.to_string(),
        "connections" => srv.global.conn_count().to_string(),
        "keepalive" => "0".into(),
        _ => String::new(),
    })
}

fn ns_config(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let srv = server(interp)?;
    let mut i = 1;
    let (mut exact, mut kind) = (false, "");
    while i < argv.len() && argv[i].starts_with('-') {
        match argv[i].as_str() {
            "-exact" => exact = true,
            "-bool" => kind = "bool",
            "-int" => kind = "int",
            _ => break,
        }
        i += 1;
    }
    let rest = &argv[i..];
    if rest.len() < 2 || rest.len() > 3 {
        return Err(wrong_args(
            argv,
            1,
            "?-exact | -bool | -int? section key ?default?",
        ));
    }
    let c = &srv.global.config;
    let value = if exact {
        c.get_exact(&rest[0], &rest[1])
    } else {
        c.get(&rest[0], &rest[1])
    };
    let def = rest.get(2);
    match kind {
        "bool" => {
            let v = value.and_then(ns_core::text::parse_bool);
            match (v, def) {
                (Some(b), _) => Ok(bool_str(b)),
                (None, Some(d)) => Ok(bool_str(ns_script::parse_bool(d)?)),
                (None, None) => Ok(String::new()),
            }
        }
        "int" => {
            let v = value.and_then(ns_core::text::parse_int_prefix);
            match (v, def) {
                (Some(n), _) => Ok(n.to_string()),
                (None, Some(d)) => Ok(int_arg(d)?.to_string()),
                (None, None) => Ok(String::new()),
            }
        }
        _ => Ok(value
            .map(str::to_owned)
            .or_else(|| def.cloned())
            .unwrap_or_default()),
    }
}

fn ns_configsection(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 2, 1, "section")?;
    let srv = server(interp)?;
    match srv.global.config.section(&argv[1]) {
        Some(s) => enter_set(interp, set_ref(s.clone()), false, false),
        None => Ok(String::new()),
    }
}

fn ns_configsections(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 1, 1, 1, "")?;
    let srv = server(interp)?;
    let mut ids = Vec::new();
    for s in srv.global.config.sections() {
        ids.push(enter_set(interp, set_ref(s.clone()), false, false)?);
    }
    Ok(interp.list_from(&ids))
}

fn ns_library(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 3, 1, "library ?module?")?;
    let srv = server(interp)?;
    let mut p = match argv[1].as_str() {
        "private" => srv.tcl.library.clone(),
        "shared" => srv
            .global
            .config
            .get(crate::global::PARAMS, "tcllibrary")
            .map_or_else(
                || srv.global.home_path(&["modules", "tcl"]),
                std::path::PathBuf::from,
            ),
        o => {
            return Err(Exception::error(format!(
                "unknown library \"{o}\": should be private or shared"
            )))
        }
    };
    if let Some(m) = argv.get(2) {
        p.push(m);
    }
    Ok(p.to_string_lossy().into_owned())
}

fn ns_modulepath(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, usize::MAX, 1, "server ?module ...?")?;
    let srv = server(interp)?;
    let mut p = srv.global.home_path(&["servers", &argv[1]]);
    if argv.len() > 2 {
        p.push("modules");
        for m in &argv[2..] {
            p.push(m);
        }
    }
    Ok(p.to_string_lossy().into_owned())
}

const ICTL_OPTS: &[&str] = &[
    "addmodule",
    "cleanup",
    "epoch",
    "get",
    "getmodules",
    "save",
    "update",
    "oncreate",
    "oncleanup",
    "oninit",
    "ondelete",
    "trace",
    "threads",
    "cancel",
    "runtraces",
    "gettraces",
    "package",
    "once",
    "markfordelete",
];

fn trace_when(s: &str) -> Result<TraceWhen, Exception> {
    TraceWhen::parse(s).ok_or_else(|| {
        Exception::error(format!(
            "bad when \"{s}\": must be create, delete, allocate, deallocate, getconn, or freeconn"
        ))
    })
}

fn ns_ictl(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (_, opt) = super::subcommand(argv, ICTL_OPTS)?;
    let srv = server(interp)?;
    let st = ns_interp(interp)?;
    match opt {
        "addmodule" => {
            check_args(argv, 3, 3, 2, "module")?;
            srv.tcl
                .modules
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(argv[2].clone());
            Ok(String::new())
        }
        "getmodules" => Ok(interp.list_from(&srv.tcl_modules())),
        "get" => Ok(srv
            .tcl
            .script
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()),
        "epoch" => Ok(srv.tcl.epoch.load(Ordering::SeqCst).to_string()),
        "save" => {
            check_args(argv, 3, 3, 2, "script")?;
            srv.tcl
                .script
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .clone_from(&argv[2]);
            let e = srv.tcl.epoch.fetch_add(1, Ordering::SeqCst) + 1;
            // This interp already has the state it saved.
            st.borrow_mut().epoch = e;
            Ok(String::new())
        }
        "update" => {
            let epoch = srv.tcl.epoch.load(Ordering::SeqCst);
            if st.borrow().epoch != epoch {
                let script = srv
                    .tcl
                    .script
                    .read()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone();
                st.borrow_mut().epoch = epoch;
                interp.eval_global(&script)?;
            }
            Ok(String::new())
        }
        "cleanup" => {
            let defer: Vec<_> = std::mem::take(&mut st.borrow_mut().defer);
            for d in defer {
                d(interp);
            }
            Ok(String::new())
        }
        "oncreate" | "oninit" | "oncleanup" | "ondelete" => {
            check_args(argv, 3, 3, 2, "script")?;
            let when = match opt {
                "oncleanup" => TraceWhen::Deallocate,
                "ondelete" => TraceWhen::Delete,
                _ => TraceWhen::Create,
            };
            register_trace(&srv, when, TraceAction::Script(argv[2].clone()));
            Ok(String::new())
        }
        "trace" => {
            check_args(argv, 4, 4, 2, "when script")?;
            register_trace(
                &srv,
                trace_when(&argv[2])?,
                TraceAction::Script(argv[3].clone()),
            );
            Ok(String::new())
        }
        "runtraces" => {
            check_args(argv, 3, 3, 2, "when")?;
            run_traces(&srv, interp, trace_when(&argv[2])?);
            Ok(String::new())
        }
        "gettraces" => {
            check_args(argv, 3, 3, 2, "when")?;
            let when = trace_when(&argv[2])?;
            let v: Vec<String> = srv
                .tcl
                .traces
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .filter(|t| t.when == when)
                .map(|t| match &t.action {
                    TraceAction::Script(s) => s.clone(),
                    TraceAction::Native(_) => "native".into(),
                })
                .collect();
            Ok(interp.list_from(&v))
        }
        "threads" => Ok(String::new()),
        "cancel" => Err(Exception::error(
            "ns_ictl cancel: interp cancellation is not supported",
        )),
        "package" => {
            if argv.len() < 3 {
                return Err(wrong_args(argv, 2, "option ?args?"));
            }
            match argv[2].as_str() {
                "names" => {
                    let pat = argv.get(3);
                    let names: Vec<String> = srv
                        .tcl
                        .packages
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .keys()
                        .filter(|n| pat.is_none_or(|p| string_match(n, p)))
                        .cloned()
                        .collect();
                    Ok(interp.list_from(&names))
                }
                "require" => {
                    let mut a = &argv[3..];
                    let exact = a.first().is_some_and(|s| s == "-exact");
                    if exact {
                        a = &a[1..];
                    }
                    let name = a
                        .first()
                        .ok_or_else(|| wrong_args(argv, 3, "?-exact? package ?version?"))?;
                    let mut words = vec!["package", "require"];
                    if exact {
                        words.push("-exact");
                    }
                    words.push(name);
                    if let Some(v) = a.get(1) {
                        words.push(v);
                    }
                    let version = interp.eval_words(&words)?;
                    let mut pk = srv
                        .tcl
                        .packages
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    match pk.get(name) {
                        Some(have) if *have != version => Err(Exception::error(format!(
                            "version conflict for package \"{name}\": have {have}, need {version}"
                        ))),
                        Some(_) => Ok(version),
                        None => {
                            pk.insert(name.clone(), version.clone());
                            drop(pk);
                            let script = interp.list_from(
                                &words.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>(),
                            );
                            register_trace(
                                &srv,
                                TraceWhen::Allocate,
                                TraceAction::Script(format!("catch {{{script}}}")),
                            );
                            Ok(version)
                        }
                    }
                }
                o => Err(Exception::error(format!(
                    "bad option \"{o}\": must be require or names"
                ))),
            }
        }
        "once" => {
            check_args(argv, 4, 4, 2, "name script")?;
            let first = srv
                .tcl
                .once
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(argv[2].clone());
            if first {
                if let Err(e) = interp.eval(&argv[3]) {
                    srv.tcl
                        .once
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&argv[2]);
                    return Err(e);
                }
            }
            Ok(bool_str(first))
        }
        "markfordelete" => ns_markfordelete(interp, &argv[1..]),
        _ => Ok(String::new()),
    }
}

fn ns_markfordelete(interp: &dyn Interp, _argv: &[String]) -> ScriptResult {
    interp.mark_for_delete();
    Ok(String::new())
}

fn pool_result(interp: &dyn Interp, c: &PoolConfig, s: crate::pool::PoolStats) -> String {
    interp.list_from(&[
        "minthreads".into(),
        c.min.to_string(),
        "maxthreads".into(),
        c.max.to_string(),
        "idle".into(),
        s.idle.to_string(),
        "current".into(),
        s.current.to_string(),
        "maxconns".into(),
        c.maxconns.to_string(),
        "queued".into(),
        s.queued.to_string(),
        "timeout".into(),
        c.timeout.to_string(),
        "spread".into(),
        c.spread.to_string(),
    ])
}

fn ns_pools(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (_, opt) = super::subcommand(argv, &["get", "set", "list", "register"])?;
    let srv = server(interp)?;
    let reg = &srv.global.pools;
    match opt {
        "list" => {
            let pat = argv.get(2);
            let v: Vec<String> = reg
                .all()
                .iter()
                .map(|p| p.name.clone())
                .filter(|n| pat.is_none_or(|p| string_match(n, p)))
                .collect();
            Ok(interp.list_from(&v))
        }
        "get" => {
            check_args(argv, 3, 3, 2, "pool")?;
            let p = reg
                .get(&argv[2])
                .ok_or_else(|| Exception::error(format!("no such pool: {}", argv[2])))?;
            Ok(pool_result(interp, &p.config(), p.stats()))
        }
        "set" => {
            if argv.len() < 3 || (argv.len() - 3) % 2 != 0 {
                return Err(wrong_args(argv, 2, "pool ?opt val opt val...?"));
            }
            let p = reg.get_or_create(&argv[2]);
            let mut c = p.config();
            for kv in argv[3..].chunks(2) {
                let v = int_arg(&kv[1])?;
                let i = ns_script::get_index(
                    &kv[0],
                    &[
                        "-maxthreads",
                        "-minthreads",
                        "-maxconns",
                        "-timeout",
                        "-spread",
                    ],
                    "cfg",
                )?;
                match i {
                    0 => c.max = v.max(0) as usize,
                    1 => c.min = v.max(0) as usize,
                    2 => {
                        if v < 0 {
                            return Err(Exception::error("maxconns cannot be less than 0"));
                        }
                        c.maxconns = v as u64;
                    }
                    3 => c.timeout = v.max(0) as u64,
                    _ => c.spread = v.max(0) as u32,
                }
            }
            p.set_config(c).map_err(Exception::error)?;
            Ok(pool_result(interp, &p.config(), p.stats()))
        }
        _ => {
            check_args(argv, 6, 6, 2, "pool server method url")?;
            reg.get(&argv[2])
                .ok_or_else(|| Exception::error(format!("no such pool: {}", argv[2])))?;
            let target = srv
                .global
                .server(&argv[3])
                .ok_or_else(|| Exception::error(format!("no such server: {}", argv[3])))?;
            target
                .url2pool
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .set(&argv[4], &argv[5], argv[2].clone(), UrlFlags::INHERIT);
            Ok(String::new())
        }
    }
}

fn limits_result(interp: &dyn Interp, l: &crate::pool::Limits) -> String {
    let c = l.config();
    let k = l.counters();
    interp.list_from(&[
        "nrunning".into(),
        k.nrunning.to_string(),
        "nwaiting".into(),
        k.nwaiting.to_string(),
        "ntimeout".into(),
        k.ntimeout.to_string(),
        "ndropped".into(),
        k.ndropped.to_string(),
        "noverflow".into(),
        k.noverflow.to_string(),
        "maxwait".into(),
        c.maxwait.to_string(),
        "maxupload".into(),
        c.maxupload.to_string(),
        "timeout".into(),
        c.timeout.to_string(),
        "maxrun".into(),
        c.maxrun.to_string(),
    ])
}

fn ns_limits(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (_, opt) = super::subcommand(argv, &["get", "set", "list", "register"])?;
    let srv = server(interp)?;
    let reg = &srv.global.pools;
    match opt {
        "list" => {
            let pat = argv.get(2);
            let v: Vec<String> = reg
                .limits_names()
                .into_iter()
                .filter(|n| pat.is_none_or(|p| string_match(n, p)))
                .collect();
            Ok(interp.list_from(&v))
        }
        "get" => {
            check_args(argv, 3, 3, 2, "limit")?;
            let l = reg
                .limits(&argv[2])
                .ok_or_else(|| Exception::error(format!("no such limits: {}", argv[2])))?;
            Ok(limits_result(interp, &l))
        }
        "set" => {
            if argv.len() < 3 || (argv.len() - 3) % 2 != 0 {
                return Err(wrong_args(argv, 2, "limit ?opt val opt val...?"));
            }
            let l = reg.limits_or_create(&argv[2]);
            let mut c: LimitsConfig = l.config();
            for kv in argv[3..].chunks(2) {
                let v = int_arg(&kv[1])?.max(0);
                match ns_script::get_index(
                    &kv[0],
                    &["-maxrun", "-maxwait", "-maxupload", "-timeout"],
                    "cfg",
                )? {
                    0 => c.maxrun = v as u32,
                    1 => c.maxwait = v as u32,
                    2 => c.maxupload = v as usize,
                    _ => c.timeout = v as u32,
                }
            }
            l.set_config(c);
            Ok(limits_result(interp, &l))
        }
        _ => {
            check_args(argv, 6, 6, 2, "limit server method url")?;
            reg.limits(&argv[2])
                .ok_or_else(|| Exception::error(format!("no such limits: {}", argv[2])))?;
            let target = srv
                .global
                .server(&argv[3])
                .ok_or_else(|| Exception::error(format!("no such server: {}", argv[3])))?;
            target
                .url2limits
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .set(&argv[4], &argv[5], argv[2].clone(), UrlFlags::INHERIT);
            Ok(String::new())
        }
    }
}

fn ns_driver(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (_, opt) = super::subcommand(argv, &["list", "query"])?;
    let srv = server(interp)?;
    let drivers = srv
        .global
        .drivers
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    if opt == "list" {
        return Ok(interp.list_from(&drivers.iter().map(|d| d.module.clone()).collect::<Vec<_>>()));
    }
    check_args(argv, 3, 3, 2, "driver")?;
    let d = drivers
        .iter()
        .find(|d| d.module == argv[2] || d.name == argv[2])
        .ok_or_else(|| Exception::error(format!("no such driver: {}", argv[2])))?;
    let s = &d.stats;
    let v = |x: &std::sync::atomic::AtomicU64| x.load(Ordering::Relaxed).to_string();
    Ok(interp.list_from(&[
        "name".into(),
        d.name.clone(),
        "location".into(),
        d.location(),
        "port".into(),
        d.port().to_string(),
        "accepts".into(),
        v(&s.accepts),
        "reads".into(),
        v(&s.reads),
        "queued".into(),
        v(&s.queued),
        "timeout".into(),
        v(&s.timeouts),
        "overflow".into(),
        v(&s.overflow),
        "dropped".into(),
        v(&s.dropped),
    ]))
}

fn ns_shutdown(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 1, 3, 1, "?-restart? ?timeout?")?;
    let srv = server(interp)?;
    let g = Arc::clone(&srv.global);
    g.state.request_stop();
    Ok(g.params.shutdown_timeout.as_secs().to_string())
}

fn ns_kill(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let nocomplain = argv.get(1).is_some_and(|a| a == "-nocomplain");
    let a = if nocomplain { &argv[1..] } else { argv };
    if a.len() != 3 {
        return Err(wrong_args(argv, 1, "?-nocomplain? pid signal"));
    }
    let _ = interp;
    let pid = int_arg(&a[1])? as libc::pid_t;
    let sig = int_arg(&a[2])? as libc::c_int;
    // SAFETY: kill(2) has no memory-safety preconditions.
    let rc = unsafe { libc::kill(pid, sig) };
    if rc != 0 && !nocomplain {
        return Err(Exception::error(format!(
            "kill {pid} {sig} failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_when_parsing() {
        assert!(trace_when("allocate").is_ok());
        assert!(trace_when("bogus").is_err());
    }
}
