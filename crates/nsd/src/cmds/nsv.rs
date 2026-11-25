//! Shared variables: `nsv_*` (`nsd/tclvar.c`) and the legacy `ns_var`.

use std::sync::PoisonError;

use ns_core::glob::string_match;
use ns_script::{wrong_args, Command, Exception, Interp, ScriptResult};

use super::{bool_str, check_args, cmd, int_arg, server};

/// Commands defined here.
pub fn commands() -> Vec<(&'static str, Command)> {
    vec![
        ("nsv_get", cmd(nsv_get)),
        ("nsv_set", cmd(nsv_set)),
        ("nsv_exists", cmd(nsv_exists)),
        ("nsv_incr", cmd(nsv_incr)),
        ("nsv_append", cmd(nsv_append)),
        ("nsv_lappend", cmd(nsv_lappend)),
        ("nsv_names", cmd(nsv_names)),
        ("nsv_array", cmd(nsv_array)),
        ("nsv_unset", cmd(nsv_unset)),
        ("ns_var", cmd(ns_var)),
    ]
}

fn no_key(array: &str, key: &str) -> Exception {
    Exception::error(format!("no such key: {key}")).error_code(format!("NSV NOKEY {array} {key}"))
}

fn nsv_get(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 3, 3, 1, "array key")?;
    let srv = server(interp)?;
    srv.nsv
        .with(&argv[1], false, |a| {
            a.and_then(|a| a.get(&argv[2]).cloned())
        })
        .ok_or_else(|| no_key(&argv[1], &argv[2]))
}

fn nsv_set(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 3, 4, 1, "array key ?value?")?;
    if argv.len() == 3 {
        return nsv_get(interp, argv);
    }
    let srv = server(interp)?;
    srv.nsv.with(&argv[1], true, |a| {
        if let Some(a) = a {
            a.insert(argv[2].clone(), argv[3].clone());
        }
    });
    Ok(argv[3].clone())
}

fn nsv_exists(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 3, 3, 1, "array key")?;
    let srv = server(interp)?;
    Ok(bool_str(srv.nsv.with(&argv[1], false, |a| {
        a.is_some_and(|a| a.contains_key(&argv[2]))
    })))
}

fn nsv_incr(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 3, 4, 1, "array key ?increment?")?;
    let by = argv.get(3).map_or(Ok(1), |s| int_arg(s))?;
    let srv = server(interp)?;
    srv.nsv.with(&argv[1], true, |a| {
        let a = a.ok_or_else(|| Exception::error("nsv array missing"))?;
        let cur = match a.get(&argv[2]) {
            Some(v) => int_arg(v)?,
            None => 0,
        };
        let n = cur + by;
        a.insert(argv[2].clone(), n.to_string());
        Ok(n.to_string())
    })
}

fn nsv_append(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 4, usize::MAX, 1, "array key string ?string ...?")?;
    let srv = server(interp)?;
    Ok(srv.nsv.with(&argv[1], true, |a| {
        let a = a.expect("created");
        let v = a.entry(argv[2].clone()).or_default();
        for s in &argv[3..] {
            v.push_str(s);
        }
        v.clone()
    }))
}

fn nsv_lappend(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 4, usize::MAX, 1, "array key value ?value ...?")?;
    let srv = server(interp)?;
    let cur = srv.nsv.with(&argv[1], false, |a| {
        a.and_then(|a| a.get(&argv[2]).cloned())
    });
    let mut items = match cur {
        Some(c) => interp.list_split(&c)?,
        None => Vec::new(),
    };
    items.extend(argv[3..].iter().cloned());
    let joined = interp.list_from(&items);
    srv.nsv.with(&argv[1], true, |a| {
        if let Some(a) = a {
            a.insert(argv[2].clone(), joined.clone());
        }
    });
    Ok(joined)
}

fn nsv_names(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 1, 2, 1, "?pattern?")?;
    let srv = server(interp)?;
    let pat = argv.get(1);
    let names: Vec<String> = srv
        .nsv
        .names()
        .into_iter()
        .filter(|n| pat.is_none_or(|p| string_match(n, p)))
        .collect();
    Ok(interp.list_from(&names))
}

fn nsv_array(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (_, opt) = super::subcommand(argv, &["set", "reset", "get", "names", "size", "exists"])?;
    if argv.len() < 3 {
        return Err(wrong_args(argv, 2, "array ?arg?"));
    }
    let srv = server(interp)?;
    let arr = &argv[2];
    match opt {
        "set" | "reset" => {
            check_args(argv, 4, 4, 2, "array valueList")?;
            let items = interp.list_split(&argv[3])?;
            if items.len() % 2 != 0 {
                return Err(Exception::error("invalid list"));
            }
            srv.nsv.with(arr, true, |a| {
                if let Some(a) = a {
                    if opt == "reset" {
                        a.clear();
                    }
                    for kv in items.chunks(2) {
                        a.insert(kv[0].clone(), kv[1].clone());
                    }
                }
            });
            Ok(String::new())
        }
        "get" | "names" => {
            let pat = argv.get(3);
            let pairs = srv.nsv.with(arr, false, |a| {
                a.map(|a| {
                    a.iter()
                        .filter(|(k, _)| pat.is_none_or(|p| string_match(k, p)))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
            });
            let mut out = Vec::new();
            for (k, v) in pairs {
                out.push(k);
                if opt == "get" {
                    out.push(v);
                }
            }
            Ok(interp.list_from(&out))
        }
        "size" => Ok(srv
            .nsv
            .with(arr, false, |a| a.map_or(0, |a| a.len()))
            .to_string()),
        _ => Ok(bool_str(srv.nsv.with(arr, false, |a| a.is_some()))),
    }
}

fn nsv_unset(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let nocomplain = argv.get(1).is_some_and(|a| a == "-nocomplain");
    let a = if nocomplain { &argv[1..] } else { argv };
    if a.len() < 2 || a.len() > 3 {
        return Err(wrong_args(argv, 1, "?-nocomplain? array ?key?"));
    }
    let srv = server(interp)?;
    let ok = match a.get(2) {
        None => srv.nsv.remove_array(&a[1]),
        Some(k) => srv
            .nsv
            .with(&a[1], false, |m| m.is_some_and(|m| m.remove(k).is_some())),
    };
    if !ok && !nocomplain {
        return Err(match a.get(2) {
            Some(k) => no_key(&a[1], k),
            None => Exception::error(format!("no such array: {}", a[1])),
        });
    }
    Ok(String::new())
}

fn ns_var(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (_, opt) = super::subcommand(argv, &["exists", "get", "list", "set", "unset"])?;
    let srv = server(interp)?;
    let mut vars = srv.vars.lock().unwrap_or_else(PoisonError::into_inner);
    match opt {
        "list" => {
            let mut names: Vec<String> = vars.keys().cloned().collect();
            names.sort();
            Ok(interp.list_from(&names))
        }
        "exists" => {
            check_args(argv, 3, 3, 2, "var")?;
            Ok(bool_str(vars.contains_key(&argv[2])))
        }
        "get" => {
            check_args(argv, 3, 3, 2, "var")?;
            vars.get(&argv[2])
                .cloned()
                .ok_or_else(|| Exception::error(format!("no such variable \"{}\"", argv[2])))
        }
        "set" => {
            check_args(argv, 4, 4, 2, "var value")?;
            vars.insert(argv[2].clone(), argv[3].clone());
            Ok(argv[3].clone())
        }
        _ => {
            check_args(argv, 3, 3, 2, "var")?;
            vars.remove(&argv[2])
                .map(|_| String::new())
                .ok_or_else(|| Exception::error(format!("no such variable \"{}\"", argv[2])))
        }
    }
}
