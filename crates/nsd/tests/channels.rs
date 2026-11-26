//! Channels, shared variables and interp cancellation, run as Tcl scripts
//! in a live server: `ns_chan`, `ns_conn channel|sock|interp`,
//! `ns_adp_ctl channel`, `ns_adp_debug`, `ns_ictl threads|cancel` and
//! `ns_share`.

#![allow(clippy::unwrap_used)]

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use common::TestServer;

fn eval(srv: &TestServer, script: &str) -> Result<String, String> {
    let s = srv.nsd.server("s1").unwrap();
    nsd::interp::with_interp(s, |i| i.eval(script).map_err(|e| e.message))
        .map_err(|e| e.message)
        .and_then(|r| r)
}

fn ok(srv: &TestServer, script: &str, want: &str) {
    match eval(srv, script) {
        Ok(got) => assert_eq!(got, want, "script: {script}"),
        Err(e) => panic!("script failed: {script}\n  error: {e}"),
    }
}

fn err(srv: &TestServer, script: &str, want: &str) {
    match eval(srv, script) {
        Ok(got) => panic!("script should fail: {script}\n  got: {got}"),
        Err(e) => assert_eq!(e, want, "script: {script}"),
    }
}

fn wait_for(srv: &TestServer, script: &str) {
    let start = Instant::now();
    while eval(srv, script).as_deref() != Ok("1") {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out: {script}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

const LIB: &str = r#"
ns_register_proc GET /sock sock_proc
proc sock_proc {} {
    ns_return 200 text/plain [expr {[ns_conn sock] >= 0}]:[expr {[ns_conn interp] != 0}]
}
ns_register_proc GET /chan chan_proc
proc chan_proc {} {
    set c [ns_conn channel]
    fconfigure $c -translation binary
    set again [catch {ns_conn channel} m]
    puts -nonewline $c "HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\n\r\nraw:[ns_conn sock]:$again:$m"
    close $c
}
ns_register_proc GET /hold hold_proc
proc hold_proc {} {
    ns_chan create [ns_conn channel] held
}
ns_register_proc GET /release release_proc
proc release_proc {} {
    set c [ns_chan get held]
    fconfigure $c -translation binary
    puts -nonewline $c "HTTP/1.0 200 OK\r\n\r\nfrom another thread"
    close $c
    ns_return 200 text/plain released
}
ns_register_proc GET /spin spin_proc
proc spin_proc {} {
    regexp {\d+} [ns_thread id] tid
    nsv_set spin tid $tid
    set n 0
    if {[catch {while 1 {incr n}} m]} {
        ns_return 200 text/plain "$m:[expr {[lsearch [ns_ictl threads] $tid] >= 0}]"
    }
}
ns_register_proc GET /hits hits_proc
proc hits_proc {} {
    ns_share hits
    incr hits
    ns_return 200 text/plain $hits
}
proc my_debuginit {procs host port} {
    if {$host eq "fail"} {
        error "no debugger at $host"
    }
    nsv_set dbg args [list $procs $host $port]
}
"#;

fn start() -> TestServer {
    TestServer::start(
        "ns_section ns/server/s1/adp\nns_param enabledebug true\nns_param debuginit my_debuginit\n",
        |home| {
            let lib = home.join("servers/s1/modules/tcl");
            std::fs::create_dir_all(&lib).unwrap();
            std::fs::write(lib.join("chan.tcl"), LIB).unwrap();
            let pages = home.join("servers/s1/pages");
            std::fs::write(pages.join("inc.adp"), "<%= [expr 6*7] %> included").unwrap();
            std::fs::write(
                pages.join("tofile.adp"),
                "<% set f [open [ns_queryget out] w]; ns_adp_ctl channel $f %>to-file\
                 <% ns_adp_ctl channel {}; close $f; ns_return 200 text/plain done %>",
            )
            .unwrap();
            std::fs::write(
                pages.join("dbg.adp"),
                "<%= [ns_adp_debug p h 9] %>:<%= [ns_adp_debug] %>:<%= [string match *: $ns_adp_output] %>",
            )
            .unwrap();
            std::fs::write(pages.join("auto.adp"), "page").unwrap();
            std::fs::write(
                pages.join("dsrc.adp"),
                r"<% ns_adp_debug %>[<% ns_adp_puts -nonewline [string match /tmp/adp1.1.?????? [info script]] %>]<% set fh [open [info script]]; set t [read $fh]; close $fh; nsv_set dbg file [info script]; ns_adp_puts -nonewline [join [lrange [split $t \n] 0 5] |] %>/<% ns_adp_puts -nonewline a; return; ns_adp_puts b %>c",
            )
            .unwrap();
        },
    )
}

fn tmp(srv: &TestServer, name: &str) -> String {
    srv.home.path().join(name).to_string_lossy().into_owned()
}

#[test]
fn ns_chan_moves_channels_through_the_shared_table() {
    let srv = start();
    let f = tmp(&srv, "chan.txt");
    err(
        &srv,
        "ns_chan",
        "wrong # args: should be \"ns_chan command ?args?\"",
    );
    err(
        &srv,
        "ns_chan bogus",
        "bad option \"bogus\": must be cleanup, list, create, put, or get",
    );
    err(
        &srv,
        "ns_chan create x",
        "wrong # args: should be \"ns_chan create channel name\"",
    );
    err(
        &srv,
        "ns_chan get",
        "wrong # args: should be \"ns_chan get name\"",
    );
    err(
        &srv,
        "ns_chan put",
        "wrong # args: should be \"ns_chan put name\"",
    );
    err(
        &srv,
        "ns_chan list a b",
        "wrong # args: should be \"ns_chan list ?-shared?\"",
    );
    err(
        &srv,
        "ns_chan cleanup a b",
        "wrong # args: should be \"ns_chan cleanup ?-shared?\"",
    );
    err(
        &srv,
        "ns_chan create nosuch x",
        "can not find channel named \"nosuch\"",
    );
    err(&srv, "ns_chan get nosuch", "no such shared channel: nosuch");
    err(&srv, "ns_chan put nosuch", "no such shared channel: nosuch");
    // create cuts the channel from the interp.
    ok(
        &srv,
        &format!(
            "set f [open {f} w]; ns_chan create $f c1; list [ns_chan list -shared] [catch {{puts $f x}}] [ns_chan list]"
        ),
        "c1 1 {}",
    );
    err(
        &srv,
        &format!("set g [open {f}.2 w]; ns_chan create $g c1"),
        "channel with name \"c1\" already exists",
    );
    // get splices it into this interp; put hands it back.
    ok(
        &srv,
        "set g [ns_chan get c1]; puts -nonewline $g hello; list [string match file* $g] [ns_chan list] [ns_chan list -shared] [ns_chan put c1] [ns_chan list] [ns_chan list -shared]",
        "1 c1 {} {} {} c1",
    );
    err(&srv, "ns_chan put c1", "no such shared channel: c1");
    // cleanup -shared closes every shared channel.
    ok(&srv, "ns_chan cleanup -shared; ns_chan list -shared", "");
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "hello");
    // `put` to a name taken again since `get` overwrites the entry, as in
    // AOLserver, but closes (and so flushes) the displaced channel.
    let (f1, f2) = (format!("{f}.a"), format!("{f}.b"));
    ok(
        &srv,
        &format!(
            "set a [open {f1} w]; puts -nonewline $a one; ns_chan create $a c; set g [ns_chan get c]; \
             set b [open {f2} w]; puts -nonewline $b two; ns_chan create $b c; \
             list [ns_chan put c] [ns_chan list -shared] [file size {f2}]"
        ),
        "{} c 3",
    );
    ok(&srv, "ns_chan cleanup -shared", "");
    assert_eq!(std::fs::read_to_string(&f1).unwrap(), "one");
    // A channel closed behind ns_chan's back is a mismatch on put.
    ok(
        &srv,
        &format!("ns_chan create [open {f} r] c2; close [ns_chan get c2]; list [catch {{ns_chan put c2}} m] $m [ns_chan list]"),
        "1 {channel mismatch} {}",
    );
    // A channel registered in several interps cannot be cut.
    ok(
        &srv,
        &format!("set f [open {f} r]; interp create kid; interp share {{}} $f kid; list [catch {{ns_chan create $f c3}} m] $m"),
        "1 {channel is shared}",
    );
    ok(
        &srv,
        &format!("ns_chan create [open {f} r] c4; set g [ns_chan get c4]; interp create kid2; interp share {{}} $g kid2; list [catch {{ns_chan put c4}} m] $m"),
        "1 {channel is shared}",
    );
    // ns_chan cleanup (run by ns_cleanup) closes the channels this interp
    // got; the c4 channel above is gone with its interp's cleanup.
    ok(
        &srv,
        &format!("ns_chan create [open {f} r] c5; set g [ns_chan get c5]; ns_chan cleanup; list [ns_chan list] [lsearch [file channels] $g]"),
        "{} -1",
    );
    ok(&srv, "ns_chan list; ns_chan list -shared", "");
}

#[test]
fn ns_chan_hands_channels_to_other_threads() {
    let srv = start();
    let f = tmp(&srv, "thread.txt");
    ok(
        &srv,
        &format!("ns_chan create [open {f} w] t1; ns_chan list -shared"),
        "t1",
    );
    ok(
        &srv,
        "ns_thread wait [ns_thread begin {set c [ns_chan get t1]; puts -nonewline $c threaded; ns_chan put t1; ns_chan list -shared}]",
        "t1",
    );
    ok(&srv, "close [ns_chan get t1]", "");
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "threaded");
}

#[test]
fn ns_conn_sock_interp_and_channel() {
    let srv = start();
    assert_eq!(srv.get("/sock").body, "1:1");
    let r = srv.raw("GET /chan HTTP/1.0\r\n\r\n");
    assert_eq!(
        r,
        "HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\n\r\nraw:-1:1:bad file number"
    );
    err(&srv, "ns_conn sock", "no connection");
    // The socket outlives its request in the shared table, and another
    // connection thread answers on it.
    let mut held = TcpStream::connect(("127.0.0.1", srv.port)).unwrap();
    held.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    held.write_all(b"GET /hold HTTP/1.0\r\n\r\n").unwrap();
    wait_for(&srv, "expr {[lsearch [ns_chan list -shared] held] >= 0}");
    assert_eq!(srv.get("/release").body, "released");
    let mut out = String::new();
    held.read_to_string(&mut out).unwrap();
    assert_eq!(out, "HTTP/1.0 200 OK\r\n\r\nfrom another thread");
}

#[test]
fn adp_output_to_a_channel() {
    let srv = start();
    let out = tmp(&srv, "adp.out");
    let inc = srv.pages().join("inc.adp");
    ok(
        &srv,
        &format!(
            "set f [open {out} w]; ns_adp_ctl channel $f; ns_adp_include {}; ns_adp_ctl channel {{}}; close $f",
            inc.display()
        ),
        "",
    );
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "42 included");
    // Streaming flushes go to the channel too.
    ok(
        &srv,
        &format!(
            "set f [open {out} w]; ns_adp_ctl channel $f; ns_adp_puts -nonewline a; ns_adp_flush; ns_adp_puts -nonewline b; ns_adp_ctl channel {{}}; close $f"
        ),
        "",
    );
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "ab");
    err(
        &srv,
        "ns_adp_ctl channel",
        "wrong # args: should be \"ns_adp_ctl channel channel\"",
    );
    err(
        &srv,
        "ns_adp_ctl channel nosuch",
        "can not find channel named \"nosuch\"",
    );
    let e = eval(
        &srv,
        &format!("set f [open {out} r]; ns_adp_ctl channel $f"),
    )
    .unwrap_err();
    assert!(e.ends_with("\" not open for write"), "{e}");
    // From a page: the output goes to the file and the page answers itself.
    let out2 = tmp(&srv, "page.out");
    assert_eq!(srv.get(&format!("/tofile.adp?out={out2}")).body, "done");
    assert_eq!(std::fs::read_to_string(&out2).unwrap(), "to-file");
    // A write failure is reported by the flush, and with autoabort the
    // next flush is refused; the final flush still resets the ADP state
    // (NsAdpFlush runs NsAdpReset when not streaming), so the next
    // `ns_adp_ctl channel {}` is a no-op.
    let e = eval(
        &srv,
        &format!(
            "set f [open {out} w]; ns_adp_ctl channel $f; close $f; ns_adp_puts x; ns_adp_flush"
        ),
    )
    .unwrap_err();
    assert!(e.starts_with("can not find channel named"), "{e}");
    err(
        &srv,
        "ns_adp_ctl channel {}",
        "adp flush disabled: adp aborted",
    );
    ok(&srv, "ns_adp_ctl channel {}", "");
    // A failing final flush also resets: the channel is forgotten.
    let e = eval(
        &srv,
        &format!(
            "set f [open {out} w]; ns_adp_ctl channel $f; close $f; ns_adp_puts x; ns_adp_ctl channel {{}}"
        ),
    )
    .unwrap_err();
    assert!(e.starts_with("can not find channel named"), "{e}");
    ok(&srv, "ns_adp_ctl channel {}", "");
    err(&srv, "ns_adp_flush", "no adp output context");
}

#[test]
fn adp_debug_calls_the_debuginit_proc() {
    let srv = start();
    err(
        &srv,
        "ns_adp_debug a b c d",
        "wrong # args: should be \"ns_adp_debug ?procs? ?host? ?port?\"",
    );
    err(
        &srv,
        "ns_adp_debug p fail 1",
        "could not initialize debugger",
    );
    assert_eq!(srv.get("/dbg.adp").body, "1:1:1");
    ok(&srv, "nsv_get dbg args", "p h 9");
    // enabledebug: a GET with ?debug=pattern starts the debugger for
    // matching files, with dhost/dport/dprocs from the query.
    assert_eq!(srv.get("/auto.adp?debug=au*&dhost=hh&dport=7").body, "page");
    ok(&srv, "nsv_get dbg args", "{} hh 7");
    let r = srv.get("/auto.adp?debug=*&dhost=fail");
    assert_eq!(r.status, 200);
    assert!(r.body.contains("Debug Init Failed"), "{}", r.body);
    assert!(r.body.contains("no debugger at fail"), "{}", r.body);
    assert!(
        srv.log().iter().any(|l| l.contains("no debugger at fail")),
        "error logged"
    );
    // Once the debugger runs, each script block is sourced from a temp
    // file with a level/chunk/file header (AdpDebug), which is removed
    // afterwards; `return` ends only that block.
    let page = srv.pages().join("dsrc.adp");
    assert_eq!(
        srv.get("/dsrc.adp").body,
        format!(
            "[1]#|# level: 1|# chunk: 2|# file:  {}|#|/ac",
            page.display()
        )
    );
    ok(&srv, "file exists [nsv_get dbg file]", "0");
    // Without the debuginit proc, the default is TclPro's, which is not
    // loaded.
    let plain = TestServer::start("", |_| {});
    err(&plain, "ns_adp_debug", "could not initialize debugger");
}

#[test]
fn ictl_cancel_interrupts_a_running_script() {
    let srv = start();
    err(
        &srv,
        "ns_ictl cancel",
        "wrong # args: should be \"ns_ictl cancel tid\"",
    );
    err(
        &srv,
        "ns_ictl cancel abc",
        "expected integer but got \"abc\"",
    );
    err(
        &srv,
        "ns_ictl cancel 999999",
        "no such active thread: 999999",
    );
    err(
        &srv,
        "ns_ictl threads extra",
        "wrong # args: should be \"ns_ictl threads\"",
    );
    err(
        &srv,
        "ns_ictl cancel ThreadId(999999)",
        "no such active thread: ThreadId(999999)",
    );
    ok(
        &srv,
        "regexp {\\d+} [ns_thread id] me; expr {[lsearch [ns_ictl threads] $me] >= 0}",
        "1",
    );
    let port = srv.port;
    let client = std::thread::spawn(move || {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        s.write_all(b"GET /spin HTTP/1.0\r\n\r\n").unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    });
    wait_for(&srv, "nsv_exists spin tid");
    ok(&srv, "ns_ictl cancel [nsv_get spin tid]", "");
    let out = client.join().unwrap();
    assert!(out.ends_with("\r\n\r\nasync cancel:1"), "{out}");
    // A cancel pending when the thread is idle is cleared when it next
    // allocates an interp.
    ok(&srv, "ns_ictl cancel [nsv_get spin tid]", "");
    assert_eq!(srv.get("/sock").body, "1:1");
}

#[test]
fn ns_share_links_globals_across_interps() {
    let srv = start();
    err(
        &srv,
        "ns_share",
        "wrong # args: should be \"ns_share ?-init script? varName ?varName ...?\"",
    );
    err(
        &srv,
        "ns_share -init x",
        "wrong # args: should be \"ns_share -init script varName\"",
    );
    err(
        &srv,
        "ns_share a(b)",
        "can't share a(b): must share whole arrays",
    );
    err(
        &srv,
        "ns_share {a b}",
        "wrong # args: should be \"info exists varName\"error sharing ::a b can't determine existence of variable",
    );
    // AOLserver called Ns_Fatal when the trace could not be set.
    let e = eval(&srv, "ns_share ::nosuch::x").unwrap_err();
    assert!(
        e.starts_with("ns_share: could not trace: ::nosuch::x"),
        "{e}"
    );
    // An existing global seeds the shared value.
    ok(&srv, "set ::sv 5; ns_share sv; set sv", "5");
    let in_thread = |script: &str| format!("ns_thread wait [ns_thread begin {{{script}}}]");
    ok(&srv, &in_thread("ns_share sv; set sv"), "5");
    ok(&srv, &in_thread("ns_share sv; set sv 7"), "7");
    ok(&srv, "ns_share sv; set sv", "7");
    ok(&srv, "proc shp {} {ns_share sv; incr sv}; shp", "8");
    ok(&srv, &in_thread("ns_share ::sv; set ::sv"), "8");
    // Arrays.
    ok(&srv, "ns_share arr; set arr(a) 1; set arr(b) 2", "2");
    ok(
        &srv,
        &in_thread("ns_share arr; lsort [array names arr]"),
        "a b",
    );
    ok(
        &srv,
        &in_thread("ns_share arr; unset arr(a); set arr(c) 3; array size arr"),
        "2",
    );
    ok(&srv, "ns_share arr; lsort [array get arr]", "2 3 b c");
    ok(&srv, "ns_share arr; set arr(b)", "2");
    ok(
        &srv,
        "set ::seed(x) 1; ns_share seed; array get seed",
        "x 1",
    );
    // Unsetting the variable unsets the shared value; the variable stays
    // shared.
    ok(&srv, "ns_share sv; unset sv; info exists sv", "0");
    ok(&srv, &in_thread("ns_share sv; info exists sv"), "0");
    ok(&srv, "ns_share sv; set sv again; set sv", "again");
    ok(&srv, &in_thread("ns_share sv; set sv"), "again");
    ok(&srv, "ns_share arr; unset arr; info exists arr", "0");
    // -init runs once per server.
    ok(&srv, "ns_share -init {set ::iv 10} iv", "10");
    ok(&srv, "ns_share -init {set ::iv 20} iv; set iv", "10");
    ok(
        &srv,
        &in_thread("ns_share -init {set ::iv 30} iv; set iv"),
        "10",
    );
    let names = srv.nsd.server("s1").unwrap().share.names();
    assert!(names.contains(&"::iv".to_owned()), "{names:?}");
    // The usual idiom: each request shares the variable again after
    // ns_cleanup unset its copy, and the shared value survives.
    assert_eq!(srv.get("/hits").body, "1");
    assert_eq!(srv.get("/hits").body, "2");
    assert_eq!(srv.get("/hits").body, "3");
}
