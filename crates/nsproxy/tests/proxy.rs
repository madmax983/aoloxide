//! `ns_proxy` against real worker processes (the `nsproxy` binary).

use ns_script::{command, Interp, ScriptEngine};

fn interp() -> Box<dyn Interp> {
    let i = ns_tcl::TclEngine::new().create_interp().expect("interp");
    i.register("ns_proxy", command(nsproxy::ns_proxy));
    i
}

/// A fresh pool using the worker binary built for this test run.
fn pool(i: &dyn Interp, name: &str, opts: &str) {
    let exec = env!("CARGO_BIN_EXE_nsproxy");
    i.eval(&format!(
        "ns_proxy config {name} -exec {{{exec}}} -evaltimeout 5000 {opts}"
    ))
    .expect("config");
}

fn err(i: &dyn Interp, script: &str) -> (String, String) {
    let e = i.eval(script).expect_err(script);
    (e.message, i.get_global("errorCode").unwrap_or_default())
}

#[test]
fn eval_in_worker_process() {
    let i = interp();
    pool(&*i, "p1", "");
    let h = i.eval("set h [ns_proxy get p1]").expect("get");
    assert_eq!(h, "p1-proxy-0");
    assert_eq!(i.eval("ns_proxy eval $h {expr {6*7}}").expect("eval"), "42");
    // A separate process: its pid differs from ours.
    let pid = i.eval("ns_proxy eval $h pid").expect("pid");
    assert_ne!(pid, std::process::id().to_string());
    // State persists between evals on one handle.
    i.eval("ns_proxy eval $h {set ::v 7}").expect("set");
    assert_eq!(i.eval("ns_proxy eval $h {set ::v}").expect("get v"), "7");
    // Errors carry the worker's errorCode.
    let (m, code) = err(&*i, "ns_proxy eval $h {error boom {} {MY CODE}}");
    assert_eq!((m.as_str(), code.as_str()), ("boom", "MY CODE"));
    assert_eq!(i.eval("ns_proxy ping $h").expect("ping"), "");
    assert_eq!(i.eval("ns_proxy release $h").expect("release"), "");
    let (m, _) = err(&*i, "ns_proxy eval $h {expr 1}");
    assert_eq!(m, "no such handle: p1-proxy-0");
    // The released proxy (and its worker) is reused.
    assert_eq!(
        i.eval("set h [ns_proxy get p1]").expect("get"),
        "p1-proxy-0"
    );
    assert_eq!(i.eval("ns_proxy eval $h {set ::v}").expect("kept"), "7");
    i.eval("ns_proxy cleanup").expect("cleanup");
}

#[test]
fn send_wait_recv_and_active() {
    let i = interp();
    pool(&*i, "p2", "");
    i.eval("set h [ns_proxy get p2]").expect("get");
    let (m, code) = err(&*i, "ns_proxy wait $h");
    assert_eq!(
        m,
        "wait for proxy \"p2-proxy-0\" failed: no script evaluating"
    );
    assert!(code.starts_with("NSPROXY EIdle"), "{code}");
    let (m, _) = err(&*i, "ns_proxy recv $h");
    assert!(m.ends_with("no script evaluating"), "{m}");
    i.eval("ns_proxy send $h {after 100; expr 1+1}")
        .expect("send");
    assert_eq!(
        i.eval("ns_proxy active p2").expect("active"),
        "p2-proxy-0 {after 100; expr 1+1}"
    );
    let (_, code) = err(&*i, "ns_proxy send $h {expr 2}");
    assert!(code.starts_with("NSPROXY EBusy"), "{code}");
    let (m, code) = err(&*i, "ns_proxy recv $h");
    assert_eq!(
        m,
        "could not receive result from proxy \"p2-proxy-0\": no wait for script result"
    );
    assert!(code.starts_with("NSPROXY ENoWait"), "{code}");
    i.eval("ns_proxy wait $h 5000").expect("wait");
    i.eval("ns_proxy wait $h")
        .expect("wait again is a no-op once done");
    assert_eq!(i.eval("ns_proxy recv $h").expect("recv"), "2");
    assert_eq!(i.eval("ns_proxy active p2").expect("active"), "");
    i.eval("ns_proxy release $h").expect("release");
}

#[test]
fn eval_timeout_restarts_worker() {
    let i = interp();
    pool(&*i, "p3", "");
    i.eval("set h [ns_proxy get p3]").expect("get");
    let pid = i.eval("ns_proxy eval $h pid").expect("pid");
    let (m, code) = err(&*i, "ns_proxy eval $h {after 2000} 50");
    assert_eq!(
        m,
        "wait for proxy \"p3-proxy-0\" failed: timeout waiting for evaluation"
    );
    assert!(code.starts_with("NSPROXY EEvalTimeout"), "{code}");
    // Still busy until released.
    let (_, code) = err(&*i, "ns_proxy eval $h {expr 1}");
    assert!(code.starts_with("NSPROXY EBusy"), "{code}");
    i.eval("ns_proxy release $h").expect("release");
    i.eval("set h [ns_proxy get p3]").expect("get");
    let pid2 = i.eval("ns_proxy eval $h pid").expect("pid");
    assert_ne!(pid, pid2, "a busy proxy's worker is closed on release");
    i.eval("ns_proxy release $h").expect("release");
}

#[test]
fn worker_death() {
    let i = interp();
    pool(&*i, "p4", "");
    i.eval("set h [ns_proxy get p4]").expect("get");
    let (m, code) = err(&*i, "ns_proxy eval $h {exit 3}");
    assert!(m.ends_with("result recv failed"), "{m}");
    assert!(code.starts_with("NSPROXY ERecv"), "{code}");
    let (m, code) = err(&*i, "ns_proxy eval $h {expr 1}");
    assert_eq!(
        m,
        "could not send script \"expr 1\" to proxy \"p4-proxy-0\": child process died"
    );
    assert!(code.starts_with("NSPROXY EDead"), "{code}");
    i.eval("ns_proxy release $h").expect("release");
    // `get` restarts a dead worker.
    i.eval("set h [ns_proxy get p4]").expect("get");
    assert_eq!(i.eval("ns_proxy eval $h {expr 2}").expect("eval"), "2");
}

#[test]
fn init_and_reinit_scripts() {
    let i = interp();
    pool(&*i, "p5", "-init {set ::n 1} -reinit {incr ::n 10}");
    i.eval("set h [ns_proxy get p5]").expect("get");
    assert_eq!(i.eval("ns_proxy eval $h {set ::n}").expect("init ran"), "1");
    assert_eq!(i.eval("ns_proxy release $h").expect("reinit result"), "11");
    i.eval("set h [ns_proxy get p5]").expect("get");
    assert_eq!(
        i.eval("ns_proxy eval $h {set ::n}").expect("reinit ran"),
        "11"
    );
    i.eval("ns_proxy release $h").expect("release");

    pool(&*i, "p5bad", "-init {error nope}");
    let (m, code) = err(&*i, "ns_proxy get p5bad");
    assert_eq!(
        m,
        "could not get 1 handle from pool \"p5bad\": init script failed"
    );
    assert!(code.starts_with("NSPROXY EInit"), "{code}");

    i.eval("ns_proxy config p5exec -exec /nonexistent/nsproxy")
        .expect("config");
    let (m, code) = err(&*i, "ns_proxy get p5exec");
    assert_eq!(
        m,
        "could not get 1 handle from pool \"p5exec\": could not create child process"
    );
    assert!(code.starts_with("NSPROXY EExec"), "{code}");
    // A failed get returns its handles: the pool is not leaked.
    let (_, code) = err(&*i, "ns_proxy get p5exec -handles 5");
    assert!(code.starts_with("NSPROXY EExec"), "{code}");
}

#[test]
fn handle_limits() {
    let i = interp();
    pool(&*i, "p6", "-max 2 -gettimeout 50");
    let (m, code) = err(&*i, "ns_proxy get p6 -handles 3");
    assert_eq!(
        m,
        "could not get 3 handles from pool \"p6\": insufficient handles"
    );
    assert!(code.starts_with("NSPROXY ERange"), "{code}");
    let hs = i
        .eval("set hs [ns_proxy get p6 -handles 2]")
        .expect("get 2");
    assert_eq!(hs, "p6-proxy-0 p6-proxy-1");
    // Holding 2 of 2, this interp cannot get more.
    let (_, code) = err(&*i, "ns_proxy get p6");
    assert!(code.starts_with("NSPROXY ERange"), "{code}");
    // Another thread times out waiting.
    let other = std::thread::spawn(|| {
        let j = interp();
        let e = j.eval("ns_proxy get p6 -timeout 50").expect_err("timeout");
        (e.message, j.get_global("errorCode").unwrap_or_default())
    })
    .join()
    .expect("thread");
    assert_eq!(
        other.0,
        "could not get 1 handle from pool \"p6\": timeout waiting for handle"
    );
    assert!(other.1.starts_with("NSPROXY EGetTimeout"), "{}", other.1);
    // ...but succeeds once a handle is released.
    let waiter = std::thread::spawn(|| {
        let j = interp();
        j.eval("ns_proxy get p6 -timeout 5000").map(|_| ())
    });
    std::thread::sleep(std::time::Duration::from_millis(100));
    i.eval("ns_proxy release [lindex $hs 0]").expect("release");
    waiter
        .join()
        .expect("thread")
        .expect("got handle after release");
    // The waiter's interp was dropped with its thread, returning its handle.
    i.eval("ns_proxy cleanup").expect("cleanup");
    assert_eq!(
        i.eval("llength [ns_proxy get p6 -handles 2]")
            .expect("all back"),
        "2"
    );
    i.eval("ns_proxy cleanup").expect("cleanup");
    let (m, _) = err(&*i, "ns_proxy get p6 -handles -1");
    assert_eq!(m, "invalid -handles: -1");
    assert_eq!(i.eval("ns_proxy get p6 -handles 0").expect("zero"), "");
}

#[test]
fn config_and_usage() {
    let i = interp();
    let c = i
        .eval("ns_proxy config p7 -min 9 -max 3 -init x")
        .expect("config");
    let items = i.list_split(&c).expect("list");
    assert_eq!(
        items.iter().step_by(2).cloned().collect::<Vec<_>>(),
        [
            "-exec",
            "-init",
            "-reinit",
            "-max",
            "-min",
            "-gettimeout",
            "-evaltimeout",
            "-sendtimeout",
            "-recvtimeout",
            "-waittimeout"
        ]
    );
    assert_eq!(items[3], "x");
    assert_eq!(
        (items[7].as_str(), items[9].as_str()),
        ("3", "3"),
        "min clamped to max"
    );
    assert_eq!(items[11], "500");
    let (m, _) = err(&*i, "ns_proxy config p7 -max -1");
    assert_eq!(m, "invalid -max: -1");
    assert!(err(&*i, "ns_proxy config p7 -bogus 1")
        .0
        .contains("bad flags"));
    assert!(err(&*i, "ns_proxy config p7 -max")
        .0
        .starts_with("wrong # args"));
    for s in [
        "ns_proxy",
        "ns_proxy get",
        "ns_proxy release",
        "ns_proxy ping",
        "ns_proxy cleanup x",
        "ns_proxy send h",
        "ns_proxy wait",
        "ns_proxy recv",
        "ns_proxy eval h",
        "ns_proxy active",
    ] {
        assert!(err(&*i, s).0.starts_with("wrong # args"), "{s}");
    }
    assert!(err(&*i, "ns_proxy bogus").0.contains("bad option"));
    assert_eq!(
        err(&*i, "ns_proxy release nosuch").0,
        "no such handle: nosuch"
    );
    assert_eq!(err(&*i, "ns_proxy recv nosuch").0, "no such handle: nosuch");
}

#[test]
fn allocation_deadlock_is_detected() {
    let i = interp();
    pool(&*i, "p8", "-max 2");
    i.eval("set h [ns_proxy get p8]").expect("main holds 1");
    let (tx, rx) = std::sync::mpsc::channel();
    let other = std::thread::spawn(move || {
        let j = interp();
        j.eval("ns_proxy get p8").expect("other holds 1");
        tx.send(()).expect("signal");
        // Becomes the exclusive waiter for a second handle.
        j.eval("ns_proxy get p8 -timeout 5000").map(|_| ())
    });
    rx.recv().expect("other got its first handle");
    std::thread::sleep(std::time::Duration::from_millis(100));
    // Both interps holding 1 and wanting 1 more of 2 would deadlock.
    let (m, code) = err(&*i, "ns_proxy get p8 -timeout 5000");
    assert_eq!(
        m,
        "could not get 1 handle from pool \"p8\": allocation deadlock"
    );
    assert!(code.starts_with("NSPROXY EDeadlock"), "{code}");
    i.eval("ns_proxy release $h").expect("release");
    other
        .join()
        .expect("thread")
        .expect("waiter got the released handle");
}
