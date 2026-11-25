//! `ns_cache` and the legacy `ns_cache_*` commands (`nsd/tclcache.c`).

use std::sync::Arc;

use ns_core::cache::CacheError;
use ns_script::{wrong_args, Command, Exception, Interp, ScriptResult};

use super::{bool_str, check_args, cmd, int_arg, server, time_arg};
use crate::tclcache::NamedCache;

/// Commands defined here.
pub fn commands() -> Vec<(&'static str, Command)> {
    vec![
        ("ns_cache", cmd(ns_cache)),
        ("ns_cache_names", cmd(ns_cache_names)),
        ("ns_cache_keys", cmd(ns_cache_keys)),
        ("ns_cache_flush", cmd(ns_cache_flush)),
        ("ns_cache_size", cmd(ns_cache_size)),
        ("ns_cache_stats", cmd(ns_cache_stats)),
    ]
}

fn get_cache(interp: &dyn Interp, name: &str) -> Result<Arc<NamedCache>, Exception> {
    server(interp)?
        .global
        .caches
        .get(name)
        .ok_or_else(|| Exception::error(format!("no such cache: {name}")))
}

fn ns_cache(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (_, opt) = super::subcommand(
        argv,
        &[
            "create", "names", "eval", "set", "get", "incr", "append", "lappend", "flush",
        ],
    )?;
    if opt == "names" {
        return ns_cache_names(interp, &argv[1..]);
    }
    if argv.len() < 3 {
        return Err(wrong_args(argv, 2, "cache ?args ...?"));
    }
    if opt == "create" {
        let (mut size, mut timeout, mut maxwait) = (0usize, None, None);
        let mut i = 3;
        while i < argv.len() {
            let flag = ns_script::get_index(
                &argv[i],
                &["-timeout", "-size", "-thread", "-server", "-maxwait"],
                "flag",
            )?;
            if flag == 2 {
                i += 1;
                continue;
            }
            let v = argv
                .get(i + 1)
                .ok_or_else(|| wrong_args(argv, 2, "?-flag val -flag val...?"))?;
            match flag {
                0 => timeout = Some(time_arg(v)?),
                1 => size = int_arg(v)?.max(0) as usize,
                4 => maxwait = Some(time_arg(v)?),
                _ => {}
            }
            i += 2;
        }
        let srv = server(interp)?;
        if srv
            .global
            .caches
            .create(&argv[2], size, timeout, maxwait)
            .is_none()
        {
            return Err(Exception::error(format!(
                "duplicate cache name: {}",
                argv[2]
            )));
        }
        return Ok(String::new());
    }
    if opt == "eval" {
        return cache_eval(interp, argv);
    }
    let c = get_cache(interp, &argv[2])?;
    let cache = &c.cache;
    match opt {
        "get" => {
            check_args(argv, 4, 5, 3, "key ?valueVar?")?;
            let v = cache.get(&argv[3]);
            match argv.get(4) {
                Some(var) => {
                    if let Some(v) = &v {
                        interp.set_var(var, v)?;
                    }
                    Ok(bool_str(v.is_some()))
                }
                None => v.ok_or_else(|| Exception::error(format!("no such key: {}", argv[3]))),
            }
        }
        "set" => {
            check_args(argv, 5, 5, 3, "key value")?;
            cache.set(&argv[3], argv[4].clone(), argv[4].len());
            Ok(argv[4].clone())
        }
        "incr" => {
            check_args(argv, 4, 5, 3, "key ?incr?")?;
            let by = argv.get(4).map_or(Ok(1), |s| int_arg(s))?;
            let mut err = None;
            let v = cache.update(&argv[3], |cur| {
                let n = match cur.as_deref().map(int_arg) {
                    Some(Ok(n)) => n,
                    Some(Err(e)) => {
                        err = Some(e);
                        0
                    }
                    None => 0,
                };
                let s = (n + by).to_string();
                let l = s.len();
                (s, l)
            });
            err.map_or(Ok(v), Err)
        }
        "append" | "lappend" => {
            check_args(argv, 5, usize::MAX, 3, "key str ?str ...?")?;
            let items = &argv[4..];
            let lappend = opt == "lappend";
            let joined = if lappend {
                interp.list_from(items)
            } else {
                items.concat()
            };
            Ok(cache.update(&argv[3], |cur| {
                let mut s = cur.unwrap_or_default();
                if lappend && !s.is_empty() {
                    s.push(' ');
                }
                s.push_str(&joined);
                let l = s.len();
                (s, l)
            }))
        }
        "flush" => {
            if argv.len() == 3 {
                cache.flush(None);
            } else {
                for k in &argv[3..] {
                    cache.remove(k);
                }
            }
            Ok(String::new())
        }
        _ => Err(Exception::error(format!(
            "unknown ns_cache option \"{opt}\""
        ))),
    }
}

/// `ns_cache eval ?-timeout t? ?-force? cache key script`.
fn cache_eval(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let mut i = 2;
    let mut timeout = None;
    let mut force = false;
    while i < argv.len() && argv[i].starts_with('-') {
        match argv[i].as_str() {
            "-timeout" => {
                timeout = Some(time_arg(argv.get(i + 1).map_or("", String::as_str))?);
                i += 2;
            }
            "-force" => {
                force = true;
                i += 1;
            }
            "--" => {
                i += 1;
                break;
            }
            _ => break,
        }
    }
    if argv.len() != i + 3 {
        return Err(wrong_args(
            argv,
            2,
            "?-timeout t? ?-force? cache key script",
        ));
    }
    let c = get_cache(interp, &argv[i])?;
    let (key, script) = (&argv[i + 1], &argv[i + 2]);
    if force {
        c.cache.remove(key);
    }
    c.cache
        .get_or_compute(key, timeout.or(c.maxwait), || {
            interp.eval(script).map(|v| {
                let l = v.len();
                (v, l)
            })
        })
        .map_err(|e| match e {
            CacheError::Timeout => {
                Exception::error("timeout waiting for value").error_code("NS_TIMEOUT")
            }
            CacheError::Compute(e) => e,
        })
}

fn ns_cache_names(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 1, 1, 1, "")?;
    Ok(interp.list_from(&server(interp)?.global.caches.names()))
}

fn ns_cache_keys(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 3, 1, "cache ?pattern?")?;
    let c = get_cache(interp, &argv[1])?;
    Ok(interp.list_from(&c.cache.keys(argv.get(2).map(String::as_str))))
}

fn ns_cache_flush(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 3, 1, "cache ?key?")?;
    let c = get_cache(interp, &argv[1])?;
    Ok(match argv.get(2) {
        Some(k) => bool_str(c.cache.remove(k)),
        None => c.cache.flush(None).to_string(),
    })
}

fn ns_cache_size(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 2, 1, "cache")?;
    let c = get_cache(interp, &argv[1])?;
    Ok(interp.list_from(&[
        c.cache.max_size().to_string(),
        c.cache.stats().size.to_string(),
    ]))
}

fn ns_cache_stats(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let name = argv
        .iter()
        .skip(1)
        .find(|a| !a.starts_with('-'))
        .ok_or_else(|| wrong_args(argv, 1, "?-contents? ?-reset? cache"))?;
    let c = get_cache(interp, name)?;
    let s = c.cache.stats();
    let total = s.hits + s.misses;
    let rate = (s.hits * 100).checked_div(total).unwrap_or(0);
    Ok(interp.list_from(&[
        "maxsize".into(),
        c.cache.max_size().to_string(),
        "size".into(),
        s.size.to_string(),
        "entries".into(),
        s.entries.to_string(),
        "flushed".into(),
        s.flushed.to_string(),
        "hits".into(),
        s.hits.to_string(),
        "missed".into(),
        s.misses.to_string(),
        "hitrate".into(),
        rate.to_string(),
        "expired".into(),
        s.pruned.to_string(),
        "pruned".into(),
        s.pruned.to_string(),
    ]))
}
