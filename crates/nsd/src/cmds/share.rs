//! `ns_share` (`nsd/tclshare.c`): global variables shared by every interp
//! of a server.
//!
//! As in AOLserver each shared variable has a server-wide value, and each
//! interp that shares it keeps an ordinary global copy watched by a
//! variable trace: reads refresh the copy from the shared value, writes and
//! unsets update the shared value, and `array` operations rebuild the copy.
//! The share count goes up on every `ns_share` and down only when an
//! interp is deleted; the shared value goes away with its last sharer.
//!
//! The locking follows the C: a recursive critical section for the table
//! (`share.cs`) and one per value, both held across the interp callbacks.
//! The values themselves sit behind a plain mutex that is only held for
//! the data copy, never while the interp runs.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use ns_core::log::Severity;
use ns_script::{Exception, Interp, ScriptResult, TraceOps, VarTrace};

use super::server;
use crate::server::Server;
use crate::sync::NsCritSec;

/// `SHARE_SCALAR`.
const SCALAR: u8 = 0x1;
/// `SHARE_ARRAY`.
const ARRAY: u8 = 0x2;
/// `SHARE_TRACE`: an unset of our own is in progress.
const TRACE: u8 = 0x8;

/// Trace key of the trace `RegisterShare` sets (its client data is the
/// address of `RegisterShare`).
const MARKER: usize = 1;
/// Trace key of the traces re-established after an unset (client data
/// `NULL` in the C).
const NULL_KEY: usize = 0;

/// Server-wide `ns_share` state (`servPtr->share`).
#[derive(Debug, Default)]
pub struct ShareState {
    /// Serializes the table (`share.cs`).
    cs: NsCritSec,
    /// Shared values by globalized name (`share.vars`).
    vars: Mutex<HashMap<String, Arc<ShareVar>>>,
    /// `ns_share -init` scripts run (or running) by variable name
    /// (`share.inits`, `share.lock`).
    inits: Mutex<HashMap<String, bool>>,
    /// Signalled when an init script finishes (`share.cond`).
    cond: Condvar,
}

/// One shared value (`NsShareVar`).
#[derive(Debug, Default)]
struct ShareVar {
    /// Serializes access to the value (`lock`).
    lock: NsCritSec,
    data: Mutex<ShareData>,
}

#[derive(Debug, Default, Clone)]
struct ShareData {
    share_count: usize,
    flags: u8,
    scalar: Option<String>,
    array: Option<BTreeMap<String, String>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Holds a critical section until dropped (also on unwind).
struct Cs<'a>(&'a NsCritSec);

impl<'a> Cs<'a> {
    fn enter(cs: &'a NsCritSec) -> Self {
        cs.enter();
        Self(cs)
    }
}

impl Drop for Cs<'_> {
    fn drop(&mut self) {
        self.0.leave();
    }
}

/// `GetGlobalizedName`.
fn globalize(name: &str) -> String {
    if name.starts_with("::") {
        name.to_owned()
    } else {
        format!("::{name}")
    }
}

impl ShareState {
    fn var(&self, name: &str) -> Option<Arc<ShareVar>> {
        lock(&self.vars).get(name).cloned()
    }

    /// Names of the shared values (for tests and introspection).
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = lock(&self.vars).keys().cloned().collect();
        v.sort();
        v
    }
}

/// `ns_share ?-init script? varName ?varName ...?`.
pub fn ns_share(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    if argv.len() < 2 {
        return Err(Exception::error(format!(
            "wrong # args: should be \"{} ?-init script? varName ?varName ...?\"",
            argv[0]
        )));
    }
    let srv = server(interp)?;
    if argv[1] == "-init" {
        if argv.len() != 4 {
            return Err(Exception::error(format!(
                "wrong # args: should be \"{} -init script varName\"",
                argv[0]
            )));
        }
        share_var(interp, &srv, &argv[3])?;
        return init_share(interp, &srv, &argv[3], &argv[2]);
    }
    for v in &argv[1..] {
        share_var(interp, &srv, v)?;
    }
    Ok(String::new())
}

/// `InitShare`: evaluate the init script once per server and variable;
/// other callers wait until it has run.
fn init_share(interp: &dyn Interp, srv: &Server, name: &str, script: &str) -> ScriptResult {
    let st = &srv.share;
    let mut inits = lock(&st.inits);
    if inits.contains_key(name) {
        while !inits.get(name).copied().unwrap_or(true) {
            inits = st.cond.wait(inits).unwrap_or_else(PoisonError::into_inner);
        }
        return Ok(String::new());
    }
    inits.insert(name.to_owned(), false);
    drop(inits);
    let r = interp.eval(script);
    lock(&st.inits).insert(name.to_owned(), true);
    st.cond.notify_all();
    r
}

/// `ShareVar`: declare `var_name` shared, creating the shared value from
/// the interp's global variable the first time.
fn share_var(interp: &dyn Interp, srv: &Arc<Server>, var_name: &str) -> Result<(), Exception> {
    if let Some(p) = var_name.find('(') {
        if var_name[p..].contains(')') {
            return Err(Exception::error(format!(
                "can't share {var_name}: must share whole arrays"
            )));
        }
    }
    let gname = globalize(var_name);
    let st = &srv.share;
    let _cs = Cs::enter(&st.cs);
    let var = if let Some(v) = st.var(&gname) {
        v
    } else {
        let v = Arc::new(ShareVar::default());
        // Tcl_AppendResult: the message follows the `info exists` error.
        let exists = interp.eval(&format!("info exists {gname}")).map_err(|e| {
            Exception::error(format!(
                "{}error sharing {gname} can't determine existence of variable",
                e.message
            ))
        })?;
        if exists == "1" {
            let mut d = ShareData::default();
            if let Some(value) = interp.get_var2(&gname, None) {
                d.scalar = Some(value);
                d.flags = SCALAR;
            } else if let Ok(list) = interp.eval(&format!("array get {gname}")) {
                let items = interp.list_split(&list).unwrap_or_default();
                let mut a = BTreeMap::new();
                for pair in items.chunks(2) {
                    a.insert(pair[0].clone(), pair.get(1).cloned().unwrap_or_default());
                }
                d.array = Some(a);
                d.flags = ARRAY;
            }
            let _ = interp.eval(&format!("unset {gname}"));
            *lock(&v.data) = d;
        }
        lock(&st.vars).insert(gname.clone(), Arc::clone(&v));
        v
    };
    lock(&var.data).share_count += 1;
    register_share(interp, srv, &gname, &var)?;
    let _ = interp.eval(&format!("global {var_name}"));
    Ok(())
}

/// `RegisterShare`: the first time this interp shares the variable, copy
/// the shared value in and trace it.
fn register_share(
    interp: &dyn Interp,
    srv: &Arc<Server>,
    gname: &str,
    var: &ShareVar,
) -> Result<(), Exception> {
    // Tcl_VarTraceInfo walk: stop at our marker (traced already) or at a
    // trace with NULL client data (taken as "no trace", as the C does).
    let traced = interp
        .var_trace_keys(gname)
        .into_iter()
        .find(|k| *k == MARKER || *k == NULL_KEY)
        == Some(MARKER);
    if traced {
        return Ok(());
    }
    let d = lock(&var.data).clone();
    if d.flags & SCALAR != 0 {
        interp.set_global(gname, d.scalar.as_deref().unwrap_or_default())?;
    } else if d.flags & ARRAY != 0 {
        for (k, v) in d.array.iter().flatten() {
            interp.set_var2(gname, Some(k), v)?;
        }
    }
    interp
        .trace_var(gname, TraceOps::ALL, MARKER, share_trace(srv))
        .map_err(|e| Exception::error(format!("ns_share: could not trace: {gname}: {e}")))
}

fn share_trace(srv: &Arc<Server>) -> VarTrace {
    let srv = Arc::clone(srv);
    Rc::new(move |interp, name1, name2, ops| trace_proc(&srv, interp, name1, name2, ops))
}

/// Re-trace `name` after an unset (the shared property is sticky).
fn retrace(srv: &Arc<Server>, interp: &dyn Interp, name: &str) {
    if let Err(e) = interp.trace_var(name, TraceOps::ALL, NULL_KEY, share_trace(srv)) {
        srv.global.log(
            Severity::Error,
            &format!("ns_share: could not trace: {name}: {}", e.message),
        );
    }
}

/// `ShareUnsetVar`: unset our copy without touching the shared value,
/// then trace it again.
fn share_unset_var(srv: &Arc<Server>, interp: &dyn Interp, name: &str, var: &ShareVar) {
    lock(&var.data).flags |= TRACE;
    interp.unset_var(name);
    retrace(srv, interp, name);
    lock(&var.data).flags &= !TRACE;
}

/// `ShareTraceProc`: keep the interp's copy and the shared value in step.
fn trace_proc(
    srv: &Arc<Server>,
    interp: Option<&dyn Interp>,
    name1: &str,
    name2: Option<&str>,
    ops: TraceOps,
) {
    let name = globalize(name1);
    let st = &srv.share;
    let cs = Cs::enter(&st.cs);
    // Not in the table: an upvar alias of the shared variable (or a
    // variable whose value is gone).  Punt, which also lets init.tcl's
    // ns_cleanupvars unset the copy without unsetting the shared value.
    let Some(var) = st.var(&name) else {
        return;
    };
    let mut destroyed = false;
    let mut bail = false;
    {
        let mut d = lock(&var.data);
        if ops.interp_destroyed {
            d.share_count = d.share_count.saturating_sub(1);
            if d.share_count == 0 {
                destroyed = true;
                lock(&st.vars).remove(&name);
            } else {
                bail = true;
            }
        }
        if d.flags & TRACE != 0 {
            bail = true;
        }
    }
    drop(cs);
    if bail {
        return;
    }
    let _vl = Cs::enter(&var.lock);
    if let Some(interp) = interp {
        if ops.array && lock(&var.data).flags & ARRAY != 0 {
            share_unset_var(srv, interp, &name, &var);
            let arr = lock(&var.data).array.clone().unwrap_or_default();
            for (k, v) in &arr {
                let _ = interp.set_var2(&name, Some(k), v);
            }
        }
        if ops.writes {
            let value = interp.get_var2(&name, name2).unwrap_or_default();
            let mut d = lock(&var.data);
            if let Some(k) = name2 {
                if d.flags == 0 {
                    d.array = Some(BTreeMap::new());
                    d.flags = ARRAY;
                }
                d.array
                    .get_or_insert_with(BTreeMap::new)
                    .insert(k.to_owned(), value);
            } else {
                d.scalar = Some(value);
            }
        }
        if ops.reads {
            let value = {
                let d = lock(&var.data);
                match name2 {
                    Some(k) => d.array.as_ref().and_then(|a| a.get(k)).cloned(),
                    None => d.scalar.clone(),
                }
            };
            if let Some(v) = value {
                let _ = interp.set_var2(&name, name2, &v);
            }
        }
    }
    if ops.unsets {
        {
            let mut d = lock(&var.data);
            if let Some(k) = name2 {
                if let Some(a) = d.array.as_mut() {
                    a.remove(k);
                }
            } else if d.flags & ARRAY != 0 {
                d.array = None;
                d.flags &= !ARRAY;
            } else if d.scalar.is_some() {
                d.scalar = None;
                d.flags &= !SCALAR;
            }
        }
        if !destroyed {
            if let Some(interp) = interp {
                retrace(srv, interp, &name);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globalized_names() {
        assert_eq!(globalize("x"), "::x");
        assert_eq!(globalize("::a::b"), "::a::b");
    }
}
