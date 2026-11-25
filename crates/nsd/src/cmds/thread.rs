//! Threads and synchronization from scripts (`nsd/tclthread.c`,
//! `nsd/tcljob.c`, `nsd/cls.c`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread::JoinHandle;

use ns_script::{wrong_args, Command, Exception, Interp, ScriptResult};

use super::{bool_str, check_args, cmd, conn, int_arg, server, time_arg};
use crate::job::DEFAULT_MAX_THREADS;
use crate::server::Server;
use crate::sync::{NsCond, NsCritSec, NsMutex, NsRwLock, NsSema, SyncRegistry};

/// Commands defined here.
pub fn commands() -> Vec<(&'static str, Command)> {
    vec![
        ("ns_thread", cmd(ns_thread)),
        ("ns_mutex", cmd(ns_mutex)),
        ("ns_cond", cmd(ns_cond)),
        ("ns_event", cmd(ns_cond)),
        ("ns_rwlock", cmd(ns_rwlock)),
        ("ns_sema", cmd(ns_sema)),
        ("ns_critsec", cmd(ns_critsec)),
        ("ns_sleep", cmd(ns_sleep)),
        ("ns_tls", cmd(ns_tls)),
        ("ns_cls", cmd(ns_cls)),
        ("ns_job", cmd(ns_job)),
    ]
}

type ThreadResult = Result<String, Exception>;

fn threads() -> &'static Mutex<HashMap<String, JoinHandle<ThreadResult>>> {
    static T: OnceLock<Mutex<HashMap<String, JoinHandle<ThreadResult>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn spawn_script(srv: &Arc<Server>, script: String, detached: bool) -> Result<String, Exception> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = format!("tid{}", NEXT.fetch_add(1, Ordering::Relaxed));
    let s = Arc::clone(srv);
    let h = std::thread::Builder::new()
        .name(format!("-tcl-{id}-"))
        .spawn(move || {
            let r = crate::interp::allocate(&s).and_then(|lease| {
                let r = lease.eval_global(&script);
                if let Err(e) = &r {
                    crate::interp::log_error(&s, &*lease, e);
                }
                r
            });
            crate::interp::thread_cleanup(&s.global);
            r
        })
        .map_err(|e| Exception::error(format!("could not create thread: {e}")))?;
    if !detached {
        threads()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.clone(), h);
    }
    Ok(id)
}

fn current_tid() -> String {
    format!("{:?}", std::thread::current().id())
}

fn ns_thread(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (_, opt) = super::subcommand(
        argv,
        &[
            "begin",
            "begindetached",
            "create",
            "wait",
            "join",
            "get",
            "getid",
            "id",
            "name",
            "yield",
        ],
    )?;
    match opt {
        "begin" | "create" | "begindetached" => {
            check_args(argv, 3, 3, 2, "script")?;
            spawn_script(&server(interp)?, argv[2].clone(), opt == "begindetached")
        }
        "wait" | "join" => {
            check_args(argv, 3, 3, 2, "tid")?;
            let h = threads()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&argv[2])
                .ok_or_else(|| Exception::error(format!("invalid thread id: {}", argv[2])))?;
            h.join()
                .map_err(|_| Exception::error("thread panicked"))?
                .or_else(|e| Ok(e.message))
        }
        "get" | "getid" | "id" => Ok(current_tid()),
        "name" => Ok(std::thread::current().name().unwrap_or("").to_owned()),
        _ => {
            std::thread::yield_now();
            Ok(String::new())
        }
    }
}

fn sync(interp: &dyn Interp) -> Result<Arc<Server>, Exception> {
    server(interp)
}

fn ns_mutex(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (_, opt) = super::subcommand(
        argv,
        &["create", "destroy", "lock", "trylock", "unlock", "eval"],
    )?;
    let srv = sync(interp)?;
    let reg = &srv.global.sync;
    if opt == "create" {
        let h = argv
            .get(2)
            .cloned()
            .unwrap_or_else(|| reg.new_handle("mutex"));
        SyncRegistry::insert(&reg.mutexes, &h, Arc::new(NsMutex::new(&h)));
        return Ok(h);
    }
    check_args(argv, 3, 4, 2, "mutexId ?script?")?;
    let m = SyncRegistry::get(&reg.mutexes, &argv[2]).unwrap_or_else(|| {
        let m = Arc::new(NsMutex::new(&argv[2]));
        SyncRegistry::insert(&reg.mutexes, &argv[2], Arc::clone(&m));
        m
    });
    match opt {
        "destroy" => {
            SyncRegistry::remove(&reg.mutexes, &argv[2]);
            Ok(String::new())
        }
        "lock" => {
            m.lock();
            Ok(String::new())
        }
        "trylock" => Ok(if m.try_lock() { "0" } else { "-1" }.to_owned()),
        "unlock" => {
            m.unlock();
            Ok(String::new())
        }
        _ => {
            let script = argv
                .get(3)
                .ok_or_else(|| wrong_args(argv, 2, "mutexId script"))?;
            m.lock();
            let r = interp.eval(script);
            m.unlock();
            r
        }
    }
}

fn ns_cond(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (_, opt) = super::subcommand(
        argv,
        &[
            "abswait",
            "broadcast",
            "create",
            "destroy",
            "set",
            "signal",
            "timedwait",
            "wait",
        ],
    )?;
    let srv = sync(interp)?;
    let reg = &srv.global.sync;
    if opt == "create" {
        let h = reg.new_handle("cond");
        SyncRegistry::insert(&reg.conds, &h, Arc::new(NsCond::default()));
        return Ok(h);
    }
    check_args(argv, 3, 5, 2, "condId ?mutexId? ?timeout?")?;
    let c = SyncRegistry::get_or(&reg.conds, &argv[2]);
    match opt {
        "destroy" => {
            SyncRegistry::remove(&reg.conds, &argv[2]);
            Ok(String::new())
        }
        "broadcast" | "set" => {
            c.broadcast();
            Ok(String::new())
        }
        "signal" => {
            c.signal();
            Ok(String::new())
        }
        _ => {
            let mid = argv
                .get(3)
                .ok_or_else(|| wrong_args(argv, 2, "condId mutexId ?timeout?"))?;
            let m = SyncRegistry::get(&reg.mutexes, mid)
                .ok_or_else(|| Exception::error(format!("no such mutex: {mid}")))?;
            let timeout = match (opt, argv.get(4)) {
                ("abswait", Some(t)) => Some(crate::sync::until_unix(int_arg(t)?)),
                (_, Some(t)) if int_arg(t).is_ok_and(|n| n <= 0) => None,
                (_, Some(t)) => Some(time_arg(t)?),
                _ => None,
            };
            Ok(bool_str(c.wait(&m, timeout)))
        }
    }
}

fn ns_rwlock(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (_, opt) = super::subcommand(
        argv,
        &[
            "create",
            "destroy",
            "readlock",
            "readunlock",
            "writelock",
            "writeunlock",
            "unlock",
        ],
    )?;
    let srv = sync(interp)?;
    let reg = &srv.global.sync;
    if opt == "create" {
        let h = reg.new_handle("rwlock");
        SyncRegistry::insert(&reg.rwlocks, &h, Arc::new(NsRwLock::default()));
        return Ok(h);
    }
    check_args(argv, 3, 3, 2, "rwlockId")?;
    let l = SyncRegistry::get_or(&reg.rwlocks, &argv[2]);
    match opt {
        "destroy" => {
            SyncRegistry::remove(&reg.rwlocks, &argv[2]);
        }
        "readlock" => l.read_lock(),
        "writelock" => l.write_lock(),
        _ => l.unlock(),
    }
    Ok(String::new())
}

fn ns_sema(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (_, opt) = super::subcommand(argv, &["create", "destroy", "release", "wait"])?;
    let srv = sync(interp)?;
    let reg = &srv.global.sync;
    if opt == "create" {
        let count = argv.get(2).map_or(Ok(0), |c| int_arg(c))?;
        let h = reg.new_handle("sema");
        SyncRegistry::insert(&reg.semas, &h, Arc::new(NsSema::new(count)));
        return Ok(h);
    }
    check_args(argv, 3, 4, 2, "semaId ?count?")?;
    let s = SyncRegistry::get_or(&reg.semas, &argv[2]);
    match opt {
        "destroy" => {
            SyncRegistry::remove(&reg.semas, &argv[2]);
        }
        "release" => s.release(argv.get(3).map_or(Ok(1), |c| int_arg(c))?),
        _ => s.wait(),
    }
    Ok(String::new())
}

fn ns_critsec(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (_, opt) = super::subcommand(argv, &["create", "destroy", "enter", "eval", "leave"])?;
    let srv = sync(interp)?;
    let reg = &srv.global.sync;
    if opt == "create" {
        let h = reg.new_handle("critsec");
        SyncRegistry::insert(&reg.critsecs, &h, Arc::new(NsCritSec::default()));
        return Ok(h);
    }
    check_args(argv, 3, 4, 2, "csId ?script?")?;
    let cs = SyncRegistry::get_or(&reg.critsecs, &argv[2]);
    match opt {
        "destroy" => {
            SyncRegistry::remove(&reg.critsecs, &argv[2]);
            Ok(String::new())
        }
        "enter" => {
            cs.enter();
            Ok(String::new())
        }
        "leave" => {
            cs.leave();
            Ok(String::new())
        }
        _ => {
            let script = argv
                .get(3)
                .ok_or_else(|| wrong_args(argv, 2, "csId script"))?;
            cs.enter();
            let r = interp.eval(script);
            cs.leave();
            r
        }
    }
}

fn ns_sleep(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    check_args(argv, 2, 2, 1, "timespec")?;
    let d = time_arg(&argv[1])?;
    let _ = interp;
    std::thread::sleep(d);
    Ok(String::new())
}

thread_local! {
    static TLS: RefCell<HashMap<u64, String>> = RefCell::new(HashMap::new());
}

fn ns_tls(_interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let (_, opt) = super::subcommand(argv, &["alloc", "get", "set"])?;
    match opt {
        "alloc" => Ok(NEXT.fetch_add(1, Ordering::Relaxed).to_string()),
        "get" => {
            check_args(argv, 3, 3, 2, "slot")?;
            let k = int_arg(&argv[2])? as u64;
            Ok(TLS.with(|t| t.borrow().get(&k).cloned().unwrap_or_default()))
        }
        _ => {
            check_args(argv, 4, 4, 2, "slot value")?;
            let k = int_arg(&argv[2])? as u64;
            TLS.with(|t| t.borrow_mut().insert(k, argv[3].clone()));
            Ok(argv[3].clone())
        }
    }
}

fn ns_cls(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let (_, opt) = super::subcommand(argv, &["alloc", "get", "set"])?;
    if opt == "alloc" {
        return Ok(NEXT.fetch_add(1, Ordering::Relaxed).to_string());
    }
    check_args(argv, 3, 4, 2, "slot ?value?")?;
    let slot = int_arg(&argv[2])?.max(0) as usize;
    let c = conn(interp)?;
    let mut cc = c.conn_mut();
    if cc.cls.len() <= slot {
        cc.cls.resize_with(slot + 1, || None);
    }
    if opt == "set" {
        let v = argv
            .get(3)
            .ok_or_else(|| wrong_args(argv, 2, "slot value"))?;
        cc.cls[slot] = Some(Box::new(v.clone()));
        Ok(v.clone())
    } else {
        Ok(cc.cls[slot]
            .as_ref()
            .and_then(|b| b.downcast_ref::<String>())
            .cloned()
            .unwrap_or_default())
    }
}

fn ns_job(interp: &dyn Interp, argv: &[String]) -> ScriptResult {
    let (_, opt) = super::subcommand(
        argv,
        &[
            "cancel",
            "create",
            "delete",
            "genid",
            "jobs",
            "joblist",
            "threadlist",
            "queue",
            "queues",
            "queuelist",
            "wait",
            "waitany",
        ],
    )?;
    let srv = server(interp)?;
    let reg = &srv.global.jobs;
    match opt {
        "create" => {
            let mut i = 2;
            let mut desc = String::new();
            if argv.get(i).is_some_and(|a| a.starts_with("-desc")) {
                desc = argv.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            let name = argv
                .get(i)
                .ok_or_else(|| wrong_args(argv, 2, "?-desc description? queueId ?maxThreads?"))?;
            let max = argv
                .get(i + 1)
                .map_or(Ok(DEFAULT_MAX_THREADS as i64), |m| int_arg(m))?;
            let g = Arc::clone(&srv.global);
            reg.create(
                name,
                &desc,
                max.max(1) as usize,
                Some(Arc::new(move || crate::interp::thread_cleanup(&g))),
            )?;
            Ok(name.clone())
        }
        "delete" => {
            check_args(argv, 3, 3, 2, "queueId")?;
            reg.delete(&argv[2])?;
            Ok(String::new())
        }
        "genid" => Ok(reg.gen_id()),
        "queues" => Ok(interp.list_from(&reg.names())),
        "queuelist" => {
            let v: Vec<String> = reg
                .all()
                .iter()
                .map(|q| {
                    let i = q.info();
                    interp.list_from(&[
                        "name".into(),
                        i.name,
                        "desc".into(),
                        i.desc,
                        "maxthreads".into(),
                        i.max_threads.to_string(),
                        "numrunning".into(),
                        i.running.to_string(),
                        "req".into(),
                        i.req.into(),
                    ])
                })
                .collect();
            Ok(interp.list_from(&v))
        }
        "threadlist" => Ok(String::new()),
        "queue" => {
            let detached = argv.get(2).is_some_and(|a| a == "-detached");
            let a = if detached { &argv[3..] } else { &argv[2..] };
            if a.len() != 2 {
                return Err(wrong_args(argv, 2, "?-detached? queueId script"));
            }
            let q = reg.get(&a[0])?;
            let s = Arc::clone(&srv);
            let script = a[1].clone();
            let work_script = script.clone();
            q.submit(
                reg.gen_id(),
                &script,
                detached,
                Box::new(move || {
                    let lease = crate::interp::allocate(&s)?;
                    let r = lease.eval_global(&work_script);
                    if let Err(e) = &r {
                        if detached {
                            crate::interp::log_error(&s, &*lease, e);
                        }
                    }
                    r
                }),
            )
        }
        "wait" => {
            let mut i = 2;
            let mut timeout = None;
            if argv.get(i).is_some_and(|a| a == "-timeout") {
                timeout = Some(time_arg(argv.get(i + 1).map_or("", String::as_str))?);
                i += 2;
            }
            if argv.len() != i + 2 {
                return Err(wrong_args(
                    argv,
                    2,
                    "?-timeout seconds:microseconds? queueId jobId",
                ));
            }
            let q = reg.get(&argv[i])?;
            match q.wait(&argv[i + 1], timeout)? {
                Ok(v) => Ok(v),
                Err(e) => {
                    if let Some(ec) = &e.error_code {
                        let _ = interp.set_global("errorCode", ec);
                    }
                    Err(e)
                }
            }
        }
        "waitany" => {
            let mut i = 2;
            let mut timeout = None;
            if argv.get(i).is_some_and(|a| a == "-timeout") {
                timeout = Some(time_arg(argv.get(i + 1).map_or("", String::as_str))?);
                i += 2;
            }
            let q = reg.get(
                argv.get(i)
                    .ok_or_else(|| wrong_args(argv, 2, "?-timeout t? queueId"))?,
            )?;
            q.wait_any(timeout)?;
            Ok(String::new())
        }
        "cancel" => {
            check_args(argv, 4, 4, 2, "queueId jobId")?;
            Ok(bool_str(reg.get(&argv[2])?.cancel(&argv[3])?))
        }
        "jobs" => {
            check_args(argv, 3, 3, 2, "queueId")?;
            Ok(interp.list_from(&reg.get(&argv[2])?.job_ids()))
        }
        _ => {
            check_args(argv, 3, 3, 2, "queueId")?;
            let v: Vec<String> = reg
                .get(&argv[2])?
                .jobs()
                .into_iter()
                .map(|j| {
                    interp.list_from(&[
                        "id".into(),
                        j.id,
                        "state".into(),
                        j.state.as_str().into(),
                        "results".into(),
                        j.results,
                        "script".into(),
                        j.script,
                        "code".into(),
                        j.code,
                        "type".into(),
                        if j.detached {
                            "detached"
                        } else {
                            "nondetached"
                        }
                        .into(),
                        "req".into(),
                        j.req.into(),
                        "time".into(),
                        format!("{:.6}", j.time),
                        "starttime".into(),
                        j.start.to_string(),
                        "endtime".into(),
                        j.end.to_string(),
                    ])
                })
                .collect();
            Ok(interp.list_from(&v))
        }
    }
}
