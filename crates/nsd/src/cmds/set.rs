//! `ns_set` and `ns_parseheader` (`nsd/tclset.c`).
//!
//! Set ids encode their kind in the first character, as in AOLserver:
//! `d` dynamic private, `t` static private (e.g. connection headers),
//! `s` shared dynamic, `p` shared static.

use std::sync::{Arc, PoisonError};

use ns_core::request::{parse_header, HeaderCase};
use ns_core::NsSet;
use ns_script::{wrong_args, Command, Exception, Interp, ScriptResult};

use super::{cmd, int_arg, server};
use crate::conn::{lock_set, set_ref, SetRef};
use crate::interp::ns_interp;

/// Commands defined here.
pub fn commands() -> Vec<(&'static str, Command)> {
    vec![
        ("ns_set", cmd(ns_set)),
        ("ns_parseheader", cmd(ns_parseheader)),
    ]
}

/// Enter `set` into the interp and return its id (`Ns_TclEnterSet`).
pub fn enter_set(
    interp: &dyn Interp,
    set: SetRef,
    dynamic: bool,
    shared: bool,
) -> Result<String, Exception> {
    let st = ns_interp(interp)?;
    if shared {
        let srv = server(interp)?;
        let kind = if dynamic { 's' } else { 'p' };
        let mut table = srv
            .shared_sets
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        loop {
            let id = format!("{kind}{}", srv.next_set_id());
            if !table.contains_key(&id) {
                table.insert(id.clone(), set);
                return Ok(id);
            }
        }
    }
    let kind = if dynamic { 'd' } else { 't' };
    let mut s = st.borrow_mut();
    loop {
        let id = format!("{kind}{}", s.next_set);
        s.next_set += 1;
        if !s.sets.contains_key(&id) {
            s.sets.insert(id.clone(), set);
            return Ok(id);
        }
    }
}

fn is_shared(id: &str) -> bool {
    id.starts_with('s') || id.starts_with('p')
}

/// Look up a set by id (`Ns_TclGetSet2`); `remove` deletes the id.
pub fn lookup_set(interp: &dyn Interp, id: &str, remove: bool) -> Result<SetRef, Exception> {
    let found = if is_shared(id) {
        let srv = server(interp)?;
        let mut t = srv
            .shared_sets
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if remove {
            t.remove(id)
        } else {
            t.get(id).cloned()
        }
    } else {
        let st = ns_interp(interp)?;
        let mut s = st.borrow_mut();
        if remove {
            s.sets.remove(id)
        } else {
            s.sets.get(id).cloned()
        }
    };
    found.ok_or_else(|| Exception::error(format!("no such set: {id}")))
}

/// Remove all private dynamic sets (`ns_set cleanup`).
pub fn cleanup_sets(interp: &dyn Interp) -> Result<(), Exception> {
    let st = ns_interp(interp)?;
    st.borrow_mut().sets.clear();
    Ok(())
}

const OPTS: &[&str] = &[
    "array", "cleanup", "copy", "cput", "create", "delete", "delkey", "find", "free", "get",
    "icput", "idelete", "idelkey", "ifind", "iget", "isnull", "iunique", "key", "list", "merge",
    "move", "name", "new", "print", "purge", "put", "size", "split", "truncate", "unique",
    "update", "value",
];

fn ns_set(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (_, opt) = super::subcommand(argv, OPTS)?;
    let opt = if opt == "create" { "new" } else { opt };
    match opt {
        "cleanup" => {
            cleanup_sets(interp)?;
            Ok(String::new())
        }
        "list" => {
            let ids: Vec<String> = match argv.get(2).map(String::as_str) {
                None => ns_interp(interp)?.borrow().sets.keys().cloned().collect(),
                Some("-shared") => server(interp)?
                    .shared_sets
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .keys()
                    .cloned()
                    .collect(),
                Some(_) => return Err(wrong_args(argv, 2, "?-shared?")),
            };
            Ok(interp.list_from(&ids))
        }
        "new" | "copy" | "split" => {
            let mut i = 2;
            let mut shared = false;
            if argv
                .get(2)
                .is_some_and(|a| a == "-shared" || a == "-persist")
            {
                shared = true;
                i += 1;
            }
            match opt {
                "new" => {
                    let name = argv.get(i).map(String::as_str);
                    i += 1;
                    let mut set = NsSet::new(name);
                    while i < argv.len() {
                        let key = &argv[i];
                        let val = argv.get(i + 1).map(String::as_str);
                        set.put(key, val);
                        i += 2;
                    }
                    enter_set(interp, set_ref(set), true, shared)
                }
                "copy" => {
                    let id = argv
                        .get(i)
                        .ok_or_else(|| wrong_args(argv, 2, "?-shared? setId"))?;
                    let src = lookup_set(interp, id, false)?;
                    let copy = lock_set(&src).clone();
                    enter_set(interp, set_ref(copy), true, shared)
                }
                _ => {
                    let id = argv
                        .get(i)
                        .ok_or_else(|| wrong_args(argv, 2, "?-shared? setId ?splitChar"))?;
                    let src = lookup_set(interp, id, false)?;
                    let sep = argv
                        .get(i + 1)
                        .and_then(|s| s.chars().next())
                        .unwrap_or('.');
                    let parts = lock_set(&src).split(sep);
                    let mut ids = Vec::new();
                    for p in parts {
                        ids.push(enter_set(interp, set_ref(p), true, shared)?);
                    }
                    Ok(interp.list_from(&ids))
                }
            }
        }
        _ => {
            if argv.len() < 3 {
                return Err(wrong_args(argv, 2, "setId ?args?"));
            }
            let set = lookup_set(interp, &argv[2], false)?;
            set_op(interp, argv, opt, &set)
        }
    }
}

fn set_op(interp: &dyn Interp, argv: &[String], opt: &str, set: &SetRef) -> ScriptResult {
    match opt {
        "array" | "size" | "name" | "print" | "free" => {
            if argv.len() != 3 {
                return Err(wrong_args(argv, 2, "setId"));
            }
            match opt {
                "array" => Ok(interp.list_from(&lock_set(set).to_pairs())),
                "size" => Ok(lock_set(set).len().to_string()),
                "name" => Ok(lock_set(set).name().unwrap_or("").to_owned()),
                "print" => {
                    eprint!("{}", lock_set(set));
                    Ok(String::new())
                }
                _ => {
                    lookup_set(interp, &argv[2], true)?;
                    Ok(String::new())
                }
            }
        }
        "find" | "ifind" | "get" | "iget" | "delkey" | "idelkey" | "unique" | "iunique" => {
            if argv.len() != 4 {
                return Err(wrong_args(argv, 2, "setId key"));
            }
            let key = &argv[3];
            let mut s = lock_set(set);
            Ok(match opt {
                "find" => s.find(key).map_or(-1, |i| i as i64).to_string(),
                "ifind" => s.ifind(key).map_or(-1, |i| i as i64).to_string(),
                "get" => s.get(key).unwrap_or("").to_owned(),
                "iget" => s.iget(key).unwrap_or("").to_owned(),
                "delkey" => {
                    s.delete_key(key);
                    String::new()
                }
                "idelkey" => {
                    s.idelete_key(key);
                    String::new()
                }
                "unique" => super::bool_str(s.unique(key)),
                _ => super::bool_str(s.iunique(key)),
            })
        }
        "value" | "isnull" | "key" | "delete" | "idelete" | "truncate" => {
            if argv.len() != 4 {
                return Err(wrong_args(argv, 2, "setId index"));
            }
            let i = int_arg(&argv[3])?;
            let mut s = lock_set(set);
            if i < 0 {
                return Err(Exception::error(format!(
                    "invalid index \"{}\": must be >= 0",
                    argv[3]
                )));
            }
            let i = i as usize;
            if i >= s.len() && opt != "truncate" {
                return Err(Exception::error(format!(
                    "invalid index \"{}\": beyond range of set fields",
                    argv[3]
                )));
            }
            Ok(match opt {
                "value" => s.value(i).flatten().unwrap_or("").to_owned(),
                "isnull" => super::bool_str(s.value(i).is_some_and(|v| v.is_none())),
                "key" => s.key(i).unwrap_or("").to_owned(),
                "truncate" => {
                    s.trunc(i);
                    String::new()
                }
                _ => {
                    s.delete(i);
                    String::new()
                }
            })
        }
        "put" | "update" | "cput" | "icput" => {
            if argv.len() != 5 {
                return Err(wrong_args(argv, 2, "setId key value"));
            }
            let (key, val) = (&argv[3], &argv[4]);
            let mut s = lock_set(set);
            let idx = match opt {
                "update" => s.update(key, Some(val)),
                "icput" => s.ifind(key).unwrap_or_else(|| s.put(key, Some(val))),
                "cput" => s.find(key).unwrap_or_else(|| s.put(key, Some(val))),
                _ => s.put(key, Some(val)),
            };
            Ok(idx.to_string())
        }
        "merge" | "move" => {
            if argv.len() != 4 {
                return Err(wrong_args(argv, 2, "setTo setFrom"));
            }
            let from = lookup_set(interp, &argv[3], false)?;
            if Arc::ptr_eq(set, &from) {
                return Ok(argv[2].clone());
            }
            let mut to = lock_set(set);
            let mut f = lock_set(&from);
            if opt == "merge" {
                to.merge(&f);
            } else {
                to.move_from(&mut f);
            }
            Ok(argv[2].clone())
        }
        "purge" => Ok(String::new()),
        _ => Err(Exception::error(format!("unknown ns_set option \"{opt}\""))),
    }
}

fn ns_parseheader(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    if argv.len() != 3 && argv.len() != 4 {
        return Err(Exception::error(format!(
            "wrong # of args: should be \"{} set header ?tolower|toupper|preserve?\"",
            argv[0]
        )));
    }
    let set = lookup_set(interp, &argv[1], false)?;
    let disp = match argv.get(3).map(String::as_str) {
        None | Some("tolower") => HeaderCase::ToLower,
        Some("toupper") => HeaderCase::ToUpper,
        Some("preserve") => HeaderCase::Preserve,
        Some(o) => {
            return Err(Exception::error(format!(
                "unknown case disposition \"{o}\":  should be toupper, tolower, or preserve"
            )))
        }
    };
    let mut guard = lock_set(&set);
    parse_header(&mut guard, &argv[2], disp)
        .map(|()| String::new())
        .map_err(|_| Exception::error(format!("invalid header:  {}", argv[2])))
}
