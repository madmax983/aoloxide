//! Conformance tests for the `ns_*` commands, run as Tcl scripts in a
//! server interp (no connection) and through live requests.

#![allow(clippy::unwrap_used)]

mod common;

use common::{Response, TestServer};

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
        Err(e) => assert!(
            e.contains(want),
            "script: {script}\n  error {e:?} lacks {want:?}"
        ),
    }
}

#[test]
fn ns_set_commands() {
    let srv = TestServer::start("", |_| {});
    ok(
        &srv,
        "set s [ns_set create hdr a 1 b 2]; ns_set size $s",
        "2",
    );
    ok(&srv, "set s [ns_set new x]; ns_set put $s K v; ns_set put $s k w; list [ns_set get $s K] [ns_set iget $s K] [ns_set find $s k] [ns_set ifind $s K]", "v v 1 0");
    ok(&srv, "set s [ns_set create n a 1 a 2]; list [ns_set unique $s a] [ns_set iunique $s A] [ns_set key $s 1] [ns_set value $s 1]", "0 0 a 2");
    ok(&srv, "set s [ns_set create n a 1]; ns_set update $s a 9; ns_set cput $s a 7; ns_set icput $s A 8; ns_set array $s", "a 9");
    ok(&srv, "set s [ns_set create n a 1 b 2 c 3]; ns_set delete $s 0; ns_set delkey $s c; ns_set array $s", "b 2");
    ok(
        &srv,
        "set s [ns_set create n a 1 b 2]; ns_set truncate $s 1; ns_set size $s",
        "1",
    );
    ok(&srv, "set s [ns_set create n a]; ns_set isnull $s 0", "1");
    ok(&srv, "set s [ns_set create nm]; ns_set name $s", "nm");
    ok(&srv, "set a [ns_set create a x 1]; set b [ns_set create b x 2 y 3]; ns_set merge $a $b; ns_set array $a", "x 1 y 3");
    ok(&srv, "set a [ns_set create a]; set b [ns_set create b y 3]; ns_set move $a $b; list [ns_set size $a] [ns_set size $b]", "1 0");
    ok(&srv, "set a [ns_set create a k v]; set c [ns_set copy $a]; ns_set put $c z 1; list [ns_set size $a] [ns_set size $c]", "1 2");
    ok(
        &srv,
        "set s [ns_set create t a.x 1 b.y 2 z 3]; llength [ns_set split $s]",
        "3",
    );
    ok(&srv, "set s [ns_set create -shared t a 1]; list [string index $s 0] [ns_set get $s a] [expr {[lsearch [ns_set list -shared] $s] >= 0}]", "s 1 1");
    ok(
        &srv,
        "set s [ns_set create t]; ns_set free $s; catch {ns_set size $s}",
        "1",
    );
    ok(&srv, "set s [ns_set create t]; ns_parseheader $s {Content-Type: text/html}; ns_parseheader $s {X-A: b} preserve; ns_set array $s", "content-type text/html X-A b");
    ok(&srv, "ns_set cleanup; llength [ns_set list]", "0");
    err(&srv, "ns_set get nosuch k", "no such set");
    err(&srv, "ns_set value [ns_set create t] 5", "beyond range");
    err(&srv, "ns_set value [ns_set create t] -1", "must be >= 0");
    err(&srv, "ns_set bogus", "bad option");
    err(
        &srv,
        "ns_parseheader [ns_set create t] {bad}",
        "invalid header",
    );
}

#[test]
fn nsv_and_var_commands() {
    let srv = TestServer::start("", |_| {});
    ok(
        &srv,
        "nsv_set a k v; list [nsv_get a k] [nsv_exists a k] [nsv_exists a x]",
        "v 1 0",
    );
    ok(&srv, "nsv_incr a n; nsv_incr a n 5", "6");
    ok(&srv, "nsv_append a s x y; nsv_append a s z", "xyz");
    ok(&srv, "nsv_lappend a l {a b}; nsv_lappend a l c", "{a b} c");
    ok(
        &srv,
        "nsv_array set arr {x 1 y 2}; lsort [nsv_array names arr]",
        "x y",
    );
    ok(&srv, "nsv_array reset arr {z 3}; list [nsv_array size arr] [nsv_array exists arr] [nsv_array get arr]", "1 1 {z 3}");
    ok(&srv, "expr {[lsearch [nsv_names] arr] >= 0}", "1");
    ok(&srv, "nsv_unset arr z; nsv_array size arr", "0");
    ok(
        &srv,
        "nsv_unset -nocomplain arr nope; nsv_unset arr; nsv_array exists arr",
        "0",
    );
    err(&srv, "nsv_get a missing", "no such key");
    err(&srv, "nsv_unset nosuch", "no such array");
    ok(
        &srv,
        "ns_var set x 1; list [ns_var get x] [ns_var exists x] [ns_var list]",
        "1 1 x",
    );
    ok(&srv, "ns_var unset x; ns_var exists x", "0");
    err(&srv, "ns_var get x", "no such variable");
}

#[test]
fn cache_commands() {
    let srv = TestServer::start("", |_| {});
    ok(
        &srv,
        "ns_cache create c1 -size 1000 -timeout 60; ns_cache set c1 k v; ns_cache get c1 k",
        "v",
    );
    ok(
        &srv,
        "list [ns_cache get c1 k val] $val [ns_cache get c1 nope v2]",
        "1 v 0",
    );
    ok(&srv, "ns_cache incr c1 n; ns_cache incr c1 n 4", "5");
    ok(
        &srv,
        "ns_cache append c1 s a b; ns_cache lappend c1 l x; ns_cache lappend c1 l y",
        "x y",
    );
    ok(&srv, "set calls 0; ns_cache eval c1 e {incr calls; set r computed}; ns_cache eval c1 e {incr calls}; list [ns_cache get c1 e] $calls", "computed 1");
    ok(
        &srv,
        "ns_cache eval -force c1 e {set r recomputed}",
        "recomputed",
    );
    ok(&srv, "expr {[lsearch [ns_cache names] c1] >= 0}", "1");
    ok(&srv, "expr {[lsearch [ns_cache_names] c1] >= 0}", "1");
    ok(&srv, "lsort [ns_cache_keys c1 s*]", "s");
    ok(&srv, "ns_cache_flush c1 s", "1");
    ok(&srv, "lindex [ns_cache_size c1] 0", "1000");
    ok(
        &srv,
        "array set st [ns_cache_stats c1]; expr {$st(hits) > 0}",
        "1",
    );
    ok(&srv, "ns_cache flush c1 k; catch {ns_cache get c1 k}", "1");
    ok(&srv, "ns_cache flush c1; llength [ns_cache_keys c1]", "0");
    err(&srv, "ns_cache create c1", "duplicate cache");
    err(&srv, "ns_cache get nosuch k", "no such cache");
    err(&srv, "ns_cache eval c1 boom {error kaboom}", "kaboom");
}

#[test]
fn info_config_and_control() {
    let srv = TestServer::start(
        "ns_section ns/test\nns_param Flag on\nns_param num 42\nns_param str hello",
        |_| {},
    );
    ok(
        &srv,
        "list [ns_info name] [ns_info version] [ns_info major] [ns_info minor] [ns_info server]",
        "AOLserver 4.5.1 4 5 s1",
    );
    ok(&srv, "ns_info servers", "s1");
    ok(
        &srv,
        "expr {[ns_info uptime] >= 0 && [ns_info pid] > 0 && [ns_info boottime] > 0}",
        "1",
    );
    ok(
        &srv,
        "string match */servers/s1/pages [ns_info pageroot]",
        "1",
    );
    ok(&srv, "list [file isdirectory [ns_info home]] [string length [ns_info hostname]] [ns_info platform]", &format!("1 {} linux", nsd::global::Global::server(&srv.nsd.global, "s1").unwrap().global.info.hostname.len()));
    ok(&srv, "list [ns_config ns/test str] [ns_config ns/test missing dflt] [ns_config -bool ns/test flag] [ns_config -int ns/test num] [ns_config -exact ns/test flag x]", "hello dflt 1 42 x");
    ok(&srv, "ns_config -bool ns/test missing 0", "0");
    ok(
        &srv,
        "set s [ns_configsection ns/test]; ns_set get $s str",
        "hello",
    );
    ok(&srv, "expr {[llength [ns_configsections]] > 3}", "1");
    ok(&srv, "string match */modules/tcl [ns_library shared]", "1");
    ok(
        &srv,
        "string match */servers/s1/modules/tcl/foo [ns_library private foo]",
        "1",
    );
    ok(
        &srv,
        "string match */servers/s1/modules/m [ns_modulepath s1 m]",
        "1",
    );
    ok(&srv, "lindex [ns_pools list] 0", "default");
    ok(
        &srv,
        "array set p [ns_pools get default]; set p(maxthreads)",
        "4",
    );
    ok(
        &srv,
        "array set p [ns_pools set small -maxthreads 2 -minthreads 0]; set p(maxthreads)",
        "2",
    );
    ok(&srv, "ns_pools register small s1 GET /small; expr 1", "1");
    err(
        &srv,
        "ns_pools set bad -minthreads 9 -maxthreads 1",
        "minthreads can't be larger",
    );
    ok(
        &srv,
        "array set l [ns_limits set lim -maxupload 5]; set l(maxupload)",
        "5",
    );
    ok(
        &srv,
        "ns_limits register lim s1 POST /up; lsort [ns_limits list]",
        "default lim",
    );
    ok(&srv, "lindex [ns_driver list] 0", "nssock");
    ok(
        &srv,
        "array set d [ns_driver query nssock]; set d(name)",
        "nssock",
    );
    ok(
        &srv,
        "list [ns_server threads] [ns_server pools]",
        "{min 0 max 4 current 0 idle 0 stopping 0} {bad default small}",
    );
    ok(&srv, "expr {[ns_ictl epoch] > 0}", "1");
    ok(&srv, "expr {[string length [ns_ictl get]] > 100}", "1");
    ok(
        &srv,
        "ns_ictl once x {set ran 1}; ns_ictl once x {set ran 2}; set ran",
        "1",
    );
    ok(
        &srv,
        "expr {[llength [ns_ictl gettraces allocate]] >= 1}",
        "1",
    );
    ok(
        &srv,
        "ns_ictl package require Tcl",
        &eval(&srv, "package require Tcl").unwrap(),
    );
    err(&srv, "ns_ictl trace bogus x", "bad when");
    ok(&srv, "ns_markfordelete", "");
}

#[test]
fn time_text_and_codecs() {
    let srv = TestServer::start("", |_| {});
    ok(&srv, "ns_httptime 0", "Thu, 01 Jan 1970 00:00:00 GMT");
    ok(
        &srv,
        "ns_parsehttptime {Sun, 06 Nov 1994 08:49:37 GMT}",
        "784111777",
    );
    err(&srv, "ns_parsehttptime bogus", "invalid time");
    ok(&srv, "list [ns_time make 1 5] [ns_time incr 1 1 999999] [ns_time diff 5:5 2:6] [ns_time seconds 3:4] [ns_time microseconds 3:4] [ns_time format 3:4]", "1:5 2:999999 2:999999 3 4 3.000004");
    ok(
        &srv,
        "expr {[ns_time] > 1000000000 && [llength [split [ns_time get] :]] == 2}",
        "1",
    );
    ok(&srv, "llength [ns_gmtime]", "9");
    ok(&srv, "llength [ns_localtime]", "9");
    ok(&srv, "string length [ns_fmttime 0 %Y]", "4");
    ok(&srv, "ns_urlencode {a b&c}", "a+b%26c");
    ok(&srv, "ns_urlencode a/ b", "a%2f/b");
    ok(&srv, "ns_urldecode a+b%26c", "a b&c");
    ok(&srv, "ns_urlencode -charset iso-8859-1 \u{e9}", "%e9");
    ok(
        &srv,
        "ns_quotehtml {<a href=\"x\">&}",
        "&lt;a href=&#34;x&#34;&gt;&amp;",
    );
    ok(&srv, "ns_striphtml {<b>bold</b> &amp; more}", "bold & more");
    ok(
        &srv,
        "ns_hrefs {<a href=\"one\">1</a><a href=two>2</a>}",
        "one two",
    );
    ok(&srv, "ns_crypt test aa", "aaqPiZY5xR5l.");
    ok(&srv, "ns_uudecode [ns_uuencode hello]", "hello");
    ok(&srv, "ns_uuencode Aladdin:open", "QWxhZGRpbjpvcGVu");
    ok(&srv, "expr {[ns_rand 10] < 10 && [ns_rand] < 1.0}", "1");
    err(&srv, "ns_rand 0", "must be > 0");
    ok(
        &srv,
        "list [ns_guesstype x.gif] [ns_guesstype x.unknownext]",
        "image/gif */*",
    );
    ok(&srv, "ns_normalizepath /a/./b/../c//d", "/a/c/d");
    ok(
        &srv,
        "set s [ns_parsequery a=1&b=two+words]; list [ns_set get $s a] [ns_set get $s b]",
        "1 {two words}",
    );
    ok(&srv, "ns_encodingforcharset utf-8", "utf-8");
    ok(&srv, "expr {[lsearch [ns_charsets] iso-8859-1] >= 0}", "1");
    ok(&srv, "ns_addrbyhost localhost", "127.0.0.1");
    err(
        &srv,
        "ns_addrbyhost no-such-host.invalid",
        "could not lookup",
    );
    err(&srv, "ns_hostbyaddr not-an-ip", "could not lookup");
    ok(
        &srv,
        "ns_log notice hello from test; ns_log debug quiet",
        "",
    );
    err(&srv, "ns_log bogus x", "unknown severity");
    ok(
        &srv,
        "ns_logctl severity debug on; ns_logctl severity debug",
        "1",
    );
    assert!(srv
        .log()
        .iter()
        .any(|l| l.ends_with("Notice: hello from test")));
}

#[test]
fn keyed_lists_loops_and_env() {
    let srv = TestServer::start("", |_| {});
    ok(
        &srv,
        "keylset kl a 1 b.c 2; list [keylget kl a] [keylget kl b.c] [keylget kl b]",
        "1 2 {{c 2}}",
    );
    ok(&srv, "keylset kl a 1; keylget kl a v; set v", "1");
    ok(&srv, "keylset kl a 1 b 2; keyldel kl a; keylkeys kl", "b");
    ok(&srv, "set kl {}; keylget kl missing v", "0");
    err(&srv, "set kl {}; keylget kl missing", "not found");
    ok(
        &srv,
        "set n 0; ns_for {set i 0} {$i < 3} {incr i} {incr n}; set n",
        "3",
    );
    ok(
        &srv,
        "set out {}; ns_foreach x {a b} {append out $x}; set out",
        "ab",
    );
    ok(&srv, "set i 0; ns_while {$i < 4} {incr i}; set i", "4");
    ok(&srv, "ns_loop_ctl list", "");
    ok(
        &srv,
        "ns_env set AOLOXIDE_T 1; list [ns_env get AOLOXIDE_T] [ns_env exists AOLOXIDE_T]",
        "1 1",
    );
    ok(&srv, "ns_env unset AOLOXIDE_T; list [ns_env exists AOLOXIDE_T] [ns_env get -nocomplain AOLOXIDE_T]", "0 {}");
    err(
        &srv,
        "ns_env get AOLOXIDE_NOPE",
        "no such environment variable",
    );
    ok(&srv, "expr {[lsearch [ns_env names] PATH] >= 0}", "1");
}

#[test]
fn file_commands() {
    let srv = TestServer::start("", |_| {});
    let d = tempfile::tempdir().unwrap();
    let base = d.path().display().to_string();
    ok(
        &srv,
        &format!("ns_mkdir {base}/x; file isdirectory {base}/x"),
        "1",
    );
    ok(&srv, &format!("set f [open {base}/a w]; puts -nonewline $f hello; close $f; ns_cp {base}/a {base}/b; ns_rename {base}/b {base}/c; file exists {base}/c"), "1");
    ok(
        &srv,
        &format!("ns_link {base}/a {base}/l; ns_symlink {base}/a {base}/s; file type {base}/s"),
        "link",
    );
    ok(
        &srv,
        &format!("ns_truncate {base}/c 2; file size {base}/c"),
        "2",
    );
    ok(&srv, &format!("ns_chmod {base}/c 0600; format %o [expr {{[file attributes {base}/c -permissions] & 0777}}]"), "600");
    ok(
        &srv,
        &format!("ns_unlink {base}/c; ns_unlink -nocomplain {base}/c; file exists {base}/c"),
        "0",
    );
    err(&srv, &format!("ns_unlink {base}/c"), "failed");
    ok(
        &srv,
        &format!("ns_rmdir {base}/x; file exists {base}/x"),
        "0",
    );
    ok(&srv, &format!("set f [open {base}/log w]; close $f; ns_rollfile {base}/log 3; file exists {base}/log.000"), "1");
    ok(
        &srv,
        &format!("ns_purgefiles {base}/log 0; file exists {base}/log.000"),
        "0",
    );
    ok(&srv, "expr {[string length [ns_tmpnam]] > 0}", "1");
    ok(
        &srv,
        &format!("string match {base}/tmp* [ns_mktemp {base}/tmpXXXXXX]"),
        "1",
    );
    ok(
        &srv,
        "string match */servers/s1/pages/a/b [ns_url2file /a/../a/b]",
        "1",
    );
    ok(&srv, &format!("set i [open {base}/a r]; set o [open {base}/z w]; ns_cpfp $i $o; close $i; close $o; file size {base}/z"), "5");
    ok(
        &srv,
        &format!("set f [open {base}/z r+]; ns_ftruncate $f 1; close $f; file size {base}/z"),
        "1",
    );
}

#[test]
fn threads_sync_and_jobs() {
    let srv = TestServer::start("", |_| {});
    ok(
        &srv,
        "set t [ns_thread begin {expr {6*7}}]; ns_thread wait $t",
        "42",
    );
    ok(
        &srv,
        "set t [ns_thread begindetached {nsv_set th done 1}]; after 300; nsv_get th done",
        "1",
    );
    ok(&srv, "expr {[string length [ns_thread id]] > 0}", "1");
    ok(&srv, "set m [ns_mutex create]; ns_mutex lock $m; set r [ns_mutex trylock $m]; ns_mutex unlock $m; list $r [ns_mutex trylock $m]", "-1 0");
    ok(
        &srv,
        "set m [ns_mutex create]; ns_mutex eval $m {set x inside}",
        "inside",
    );
    ok(&srv, "set m [ns_mutex create]; set c [ns_cond create]; ns_mutex lock $m; set r [ns_cond timedwait $c $m 0.05]; ns_mutex unlock $m; set r", "0");
    ok(
        &srv,
        "set c [ns_cond create]; ns_cond broadcast $c; ns_cond signal $c; ns_cond destroy $c",
        "",
    );
    ok(&srv, "set rw [ns_rwlock create]; ns_rwlock readlock $rw; ns_rwlock readunlock $rw; ns_rwlock writelock $rw; ns_rwlock writeunlock $rw", "");
    ok(
        &srv,
        "set s [ns_sema create 1]; ns_sema wait $s; ns_sema release $s 2; ns_sema wait $s",
        "",
    );
    ok(&srv, "set cs [ns_critsec create]; ns_critsec enter $cs; ns_critsec enter $cs; ns_critsec leave $cs; ns_critsec leave $cs; ns_critsec eval $cs {set y 1}", "1");
    ok(&srv, "ns_sleep 0.01", "");
    ok(
        &srv,
        "set slot [ns_tls alloc]; ns_tls set $slot val; ns_tls get $slot",
        "val",
    );
    ok(
        &srv,
        "ns_job create q1 2; set j [ns_job queue q1 {expr {1+2}}]; ns_job wait q1 $j",
        "3",
    );
    ok(
        &srv,
        "set j [ns_job queue q1 {error jobfail}]; catch {ns_job wait q1 $j} msg; set msg",
        "jobfail",
    );
    ok(
        &srv,
        "ns_job queue -detached q1 {nsv_set jobs d 1}; after 300; nsv_get jobs d",
        "1",
    );
    ok(
        &srv,
        "set j [ns_job queue q1 {after 50; set x 1}]; ns_job waitany q1; ns_job wait q1 $j",
        "1",
    );
    ok(&srv, "expr {[lsearch [ns_job queues] q1] >= 0}", "1");
    ok(&srv, "set j [ns_job queue q1 {after 100}]; set l [ns_job joblist q1]; ns_job wait q1 $j; llength $l", "1");
    ok(
        &srv,
        "llength [ns_job queuelist]",
        &eval(&srv, "llength [ns_job queues]").unwrap(),
    );
    ok(&srv, "string match job* [ns_job genid]", "1");
    err(&srv, "ns_job create q1", "already exists");
    err(&srv, "ns_job wait nosuch j", "no such queue");
    ok(&srv, "ns_job delete q1", "");
}

#[test]
fn scheduler_and_callbacks() {
    let srv = TestServer::start("", |_| {});
    ok(
        &srv,
        "set id [ns_schedule_proc -once 1 {nsv_set sched once 1}]; expr {$id > 0}",
        "1",
    );
    std::thread::sleep(std::time::Duration::from_millis(1500));
    ok(&srv, "nsv_get sched once", "1");
    ok(&srv, "set id [ns_schedule_proc 1000 {set x 1}]; list [ns_pause $id] [ns_resume $id] [ns_cancel $id] [ns_cancel $id]", "1 1 1 0");
    ok(
        &srv,
        "set id [ns_schedule_daily 3 30 {set x 1}]; ns_unschedule_proc $id",
        "1",
    );
    ok(
        &srv,
        "set id [ns_schedule_weekly 1 3 30 {set x 1}]; expr {[llength [ns_info scheduled]] >= 1}",
        "1",
    );
    err(&srv, "ns_schedule_daily 25 0 {x}", "invalid hour");
    err(&srv, "ns_schedule_weekly 9 0 0 {x}", "invalid day");
    ok(
        &srv,
        "ns_after 1 {nsv_set sched after 1}",
        &eval(
            &srv,
            "expr {[lindex [lindex [ns_info scheduled] end] 0] + 1}",
        )
        .unwrap(),
    );
    ok(
        &srv,
        "ns_atshutdown {nsv_set cb shut 1}; ns_atexit {set x 1}; ns_atsignal {set y 1}",
        "",
    );
}

#[test]
fn http_client_and_sockets() {
    let srv = TestServer::start("", |home| {
        std::fs::write(home.join("servers/s1/pages/c.txt"), "client ok").unwrap();
    });
    let url = format!("http://127.0.0.1:{}/c.txt", srv.port);
    ok(&srv, &format!("ns_geturl {url}"), "client ok");
    ok(
        &srv,
        &format!("ns_geturl {url} hdrs; ns_set iget $hdrs content-type"),
        "text/plain",
    );
    ok(
        &srv,
        &format!("set id [ns_http queue GET {url}]; ns_http wait -status st $id"),
        "client ok",
    );
    ok(&srv, &format!("set id [ns_http queue GET {url}]; ns_http wait -result body -status st $id; list $st $body"), "200 {client ok}");
    ok(&srv, &format!("ns_http run GET {url}"), "client ok");
    ok(&srv, "ns_http cleanup", "");
    err(&srv, "ns_geturl https://example.invalid/", "only http://");
    ok(&srv, &format!("set fds [ns_sockopen 127.0.0.1 {}]; set f [lindex $fds 1]; fconfigure $f -translation binary; puts -nonewline $f \"GET /c.txt HTTP/1.0\\r\\n\\r\\n\"; flush $f; set r [read [lindex $fds 0]]; close $f; string match *client* $r", srv.port), "1");
    ok(&srv, "ns_chan list", "");
    err(&srv, "ns_chan create x y", "not supported");
    ok(&srv, "ns_checkurl GET /c.txt", "1");
}

#[test]
fn conn_commands_over_http() {
    let srv = TestServer::start("", |home| {
        let lib = home.join("servers/s1/modules/tcl");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(
            lib.join("t.tcl"),
            r#"
ns_register_proc GET /c conn_info
proc conn_info {} {
    set r [list [ns_conn method] [ns_conn url] [ns_conn urlc] [ns_conn urlv] [ns_conn query] \
        [ns_conn version] [ns_conn protocol] [ns_conn peeraddr] [ns_conn isconnected] \
        [ns_conn authuser] [ns_conn authpassword] [ns_conn contentlength] [ns_conn driver] \
        [ns_conn server] [string match http://* [ns_conn location]] [ns_conn status] \
        [ns_set iget [ns_conn headers] x-a] [expr {[ns_conn id] > 0}] [ns_conn encoding] [ns_conn pool]]
    ns_return 200 text/plain $r
}
ns_register_proc POST /form form_info
proc form_info {} {
    set f [ns_conn form]
    ns_return 200 text/plain "[ns_set get $f a],[ns_conn content],[ns_queryget a],[ns_queryexists b]"
}
ns_register_proc GET /respond respond_proc
proc respond_proc {} {
    set h [ns_set create h X-R yes]
    ns_respond -status 202 -type text/x-test -string resp -headers $h
}
ns_register_proc GET /headers headers_proc
proc headers_proc {} {
    ns_headers 200 text/plain 5
    ns_write hello
}
ns_register_proc GET /status status_proc
proc status_proc {} { ns_returnnotice 418 Teapot short-and-stout }
ns_register_proc GET /bad bad_proc
proc bad_proc {} { ns_returnbadrequest "because" }
ns_register_proc GET /err err_proc
proc err_proc {} { ns_returnerror 409 conflicted }
ns_register_proc GET /unauth unauth_proc
proc unauth_proc {} { ns_returnunauthorized }
ns_register_proc GET /moved moved_proc
proc moved_proc {} { ns_returnmoved /new }
ns_register_proc GET /file file_proc
proc file_proc {} { ns_returnfile 200 text/x-file [ns_url2file /f.txt] }
ns_register_proc GET /fp fp_proc
proc fp_proc {} { set f [open [ns_url2file /f.txt]]; ns_returnfp 200 text/plain $f 4; close $f }
ns_register_proc GET /binary binary_proc
proc binary_proc {} { ns_return -binary 200 application/octet-stream [binary format c3 {1 2 3}] }
ns_register_proc GET /args args_proc myarg
proc args_proc {arg} { ns_return 200 text/plain "arg=$arg" }
ns_register_proc GET /connid connid_proc extra
proc connid_proc {conn arg} { ns_return $conn 200 text/plain "c=[string match cns* $conn] $arg" }
ns_register_filter postauth GET /filtered/* f_one
proc f_one {why} { ns_set put [ns_conn outputheaders] X-Filter $why; return filter_ok }
ns_register_filter trace GET /filtered/* f_trace
proc f_trace {why} { nsv_set trace last [ns_conn url]; return filter_ok }
ns_register_trace GET /filtered/* t_void
proc t_void {} { nsv_incr trace void }
ns_register_proc GET /filtered/x filtered_proc
proc filtered_proc {} { ns_return 200 text/plain filtered }
ns_register_proc GET /atclose atclose_proc
proc atclose_proc {} { ns_atclose {nsv_set ac done 1}; ns_return 200 text/plain ok }
ns_register_proc GET /cls cls_proc
proc cls_proc {} { set s [ns_cls alloc]; ns_cls set $s v; ns_return 200 text/plain [ns_cls get $s] }
ns_register_proc GET /unreg unreg_proc
proc unreg_proc {} { ns_return 200 text/plain unreg }
ns_register_fastpath GET /fp2
ns_register_adp GET /adpfile page.adp
ns_register_proxy GET http proxy_proc
proc proxy_proc {} { ns_return 200 text/plain "proxied [ns_conn host]" }
"#,
        )
        .unwrap();
        let p = home.join("servers/s1/pages");
        std::fs::write(p.join("f.txt"), "file contents").unwrap();
        std::fs::write(p.join("page.adp"), "adp <%= [ns_conn url] %>").unwrap();
    });
    let r = Response::parse(
        &srv.raw("GET /c?x=1 HTTP/1.1\r\nHost: h\r\nX-A: z\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(r.status, 200, "{r:?} {:?}", srv.log());
    assert_eq!(
        r.body,
        "GET /c 1 c x=1 1.1 {} 127.0.0.1 1 {} {} 0 nssock s1 1 200 z 1 utf-8 default"
    );

    let body = "a=1&b=2";
    let r = Response::parse(&srv.raw(&format!(
        "POST /form HTTP/1.0\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )));
    assert_eq!(r.body, "1,a=1&b=2,1,1", "{:?}", srv.log());

    let r = srv.get("/respond");
    assert_eq!(
        (r.status, r.body.as_str(), r.header("x-r")),
        (202, "resp", Some("yes"))
    );
    assert_eq!(r.header("content-type"), Some("text/x-test"));
    let r = srv.get("/headers");
    assert_eq!((r.status, r.body.as_str()), (200, "hello"));
    let r = srv.get("/status");
    assert_eq!(r.status, 418);
    assert!(r.body.contains("short-and-stout"));
    assert_eq!(srv.get("/bad").status, 400);
    assert_eq!(srv.get("/err").status, 409);
    assert_eq!(srv.get("/unauth").status, 401);
    let r = srv.get("/moved");
    assert_eq!(r.status, 301);
    assert!(r.header("location").unwrap().ends_with("/new"));
    let r = srv.get("/file");
    assert_eq!(
        (r.body.as_str(), r.header("content-type")),
        ("file contents", Some("text/x-file"))
    );
    assert_eq!(srv.get("/fp").body, "file");
    let raw = srv.raw("GET /binary HTTP/1.0\r\n\r\n");
    assert!(raw.as_bytes().ends_with(&[1, 2, 3]));
    assert_eq!(srv.get("/args").body, "arg=myarg");
    assert_eq!(srv.get("/connid").body, "c=1 extra");
    let r = srv.get("/filtered/x");
    assert_eq!(
        (r.body.as_str(), r.header("x-filter")),
        ("filtered", Some("postauth"))
    );
    std::thread::sleep(std::time::Duration::from_millis(100));
    ok(
        &srv,
        "list [nsv_get trace last] [nsv_get trace void]",
        "/filtered/x 1",
    );
    assert_eq!(srv.get("/atclose").body, "ok");
    std::thread::sleep(std::time::Duration::from_millis(100));
    ok(&srv, "nsv_get ac done", "1");
    assert_eq!(srv.get("/cls").body, "v");
    assert_eq!(srv.get("/unreg").body, "unreg");
    ok(&srv, "ns_unregister_proc GET /unreg", "");
    assert_eq!(srv.get("/unreg").status, 404);
    assert_eq!(srv.get("/fp2").status, 404);
    assert_eq!(srv.get("/adpfile").body, "adp /adpfile");
    let r = Response::parse(&srv.raw("GET http://example.com/x HTTP/1.0\r\n\r\n"));
    assert_eq!(r.body, "proxied example.com");
    ok(&srv, "ns_requestauthorize GET / u p 127.0.0.1", "OK");
}

#[test]
fn adp_commands() {
    let srv = TestServer::start("", |home| {
        let lib = home.join("servers/s1/modules/tcl");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(
            lib.join("tags.tcl"),
            "proc mytag_proc {a} { return tag:$a }\n\
             proc up {text atts} { string toupper $text }\n\
             ns_register_proc GET /evaltest evaltest_proc\n\
             proc evaltest_proc {} { ns_return 200 text/plain [set ::evaltest] }\n",
        )
        .unwrap();
        let p = home.join("servers/s1/pages");
        let w = |n: &str, c: &str| std::fs::write(p.join(n), c).unwrap();
        w(
            "argv.adp",
            "<% ns_adp_bind_args a b %><%= $a-$b %>|<%= [ns_adp_argc] %>",
        );
        w("main.adp", "<% ns_adp_include argv.adp x y %>");
        w("eval.adp", "<%= [ns_adp_parse -string {in<%= [expr 1+1] %>}] %>/<%= [ns_adp_eval {e<% ns_adp_puts -nonewline v %>}] %>");
        w("tags.adp", "[<upper>abc</upper>]");
        w("tags2.adp", "<mytag a=1>");
        w("mime.adp", "<% ns_adp_mimetype text/plain %>plain");
        w(
            "trunc.adp",
            "abcdef<% ns_adp_trunc 2 %>X<%= [ns_adp_tell] %>",
        );
        w("ret.adp", "<% ns_adp_include r2.adp %>after");
        w("r2.adp", "in<% ns_adp_return %>never");
        w("abort.adp", "gone<% ns_adp_abort %>");
        w("misc.adp", "<%= [ns_adp_exception] %><%= [file tail [ns_adp_dir]] %><% ns_adp_ident myid %><%= [ns_adp_ident] %><%= [string length [ns_adp_dump]] %>");
        w(
            "ctl.adp",
            "<%= [ns_adp_ctl bufsize 10] %><%= [ns_adp_ctl stricterror] %>",
        );
        w("close.adp", "first<% ns_adp_close %>second");
        w("stream.adp", "<% ns_adp_stream %>s1<% ns_adp_flush %>s2");
        w("compress.adp", "<% ns_adp_compress 1 %>z");
        w(
            "dbg.adp",
            "<% catch {ns_adp_debug} m %><%= [string match *not* $m] %>",
        );
    });
    // Tags are applied at parse time, so register them before the pages run.
    ok(&srv, "ns_adp_registerproc mytag mytag_proc", "");
    // Script tags are called with the enclosed text and an attribute set.
    ok(&srv, "ns_adp_registerscript upper /upper up", "");
    // ns_eval saves state for every interp (via init.tcl's job queue).
    ok(
        &srv,
        "ns_eval -sync {set ::evaltest propagated}",
        "propagated",
    );
    assert_eq!(srv.get("/evaltest").body, "propagated");
    assert_eq!(srv.get("/main.adp").body, "x-y|3");
    assert_eq!(srv.get("/eval.adp").body, "in2/ev");
    assert_eq!(srv.get("/tags.adp").body, "[ABC]");
    assert_eq!(srv.get("/tags2.adp").body, "tag:1");
    let r = srv.get("/mime.adp");
    assert_eq!(
        (r.body.as_str(), r.header("content-type")),
        ("plain", Some("text/plain"))
    );
    assert_eq!(srv.get("/trunc.adp").body, "abX3");
    assert_eq!(srv.get("/ret.adp").body, "inafter");
    // Deviation (docs/porting.md): an abort with nothing sent yields 500
    // instead of an empty reply.
    assert_eq!(srv.get("/abort.adp").status, 500);
    assert_eq!(srv.get("/misc.adp").body, "0pagesmyid10");
    assert_eq!(srv.get("/ctl.adp").body, "10240000");
    assert_eq!(srv.get("/close.adp").body, "first");
    let raw = srv.raw("GET /stream.adp HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    assert!(
        raw.contains("Transfer-Encoding: chunked") && raw.contains("s1") && raw.contains("s2"),
        "{raw}"
    );
    assert_eq!(srv.get("/compress.adp").body, "z");
    assert_eq!(srv.get("/dbg.adp").body, "1");
    ok(&srv, "expr {[llength [ns_adp_stats]] > 0}", "1");
    err(&srv, "ns_adp_argc", "no active adp");
    err(&srv, "ns_adp_flush", "no adp output context");
}

/// Open descriptors on (unlinked) `nstmp.*` spool files.
fn open_spool_files() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .filter(|l| l.to_string_lossy().contains("/nstmp."))
        .count()
}

fn post(srv: &TestServer, url: &str, ty: &str, body: &str) -> Response {
    Response::parse(&srv.raw(&format!(
        "POST {url} HTTP/1.0\r\nContent-Type: {ty}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )))
}

#[test]
fn spooled_request_content() {
    let srv = TestServer::start(
        "ns_section ns/server/s1/module/nssock\nns_param maxinput 4096\n",
        |home| {
            let lib = home.join("servers/s1/modules/tcl");
            std::fs::create_dir_all(&lib).unwrap();
            std::fs::write(
                lib.join("t.tcl"),
                r#"
ns_register_proc POST /spool spool_proc
proc spool_proc {} {
    set ch [ns_conn contentchannel]
    fconfigure $ch -translation binary
    set data [read $ch]
    close $ch
    set ch2 [ns_conn contentchannel]
    set n2 [string length [read $ch2]]
    close $ch2
    ns_return 200 text/plain [list [ns_conn contentlength] [expr {[ns_conn flags] & 0x80}] \
        [string length $data] $n2 [string equal $data [ns_conn content]] [ns_queryget a] \
        [ns_conn content 2 3] [ns_conn contentavail]]
}
ns_register_proc POST /indices indices_proc
proc indices_proc {} {
    set n [ns_conn contentlength]
    set r {}
    foreach args [list [list -1 1] [list [expr {$n + 1}] 0] [list 0 [expr {$n + 1}]] [list 1 -1] [list 0]] {
        catch {ns_conn content {*}$args} m
        lappend r $m
    }
    catch {ns_conn copy 0 1} m
    lappend r [string match "wrong # args*off len chan*" $m] [string length [ns_conn content $n 0]]
    ns_return 200 text/plain [join $r |]
}
ns_register_proc POST /copy copy_proc
proc copy_proc {} {
    set tmp [ns_tmpnam]
    set f [open $tmp w]
    ns_conn copy 2 [expr {[ns_conn contentlength] - 2}] $f
    ns_conn copy 0 0 $f
    close $f
    set f [open $tmp]
    set head [read $f 4]
    close $f
    set sz [file size $tmp]
    set f [open $tmp w]
    ns_writecontent $f
    close $f
    set wsz [file size $tmp]
    file delete $tmp
    ns_return 200 text/plain [list $sz $head $wsz [ns_conn contentavail]]
}
ns_register_proc POST /upload upload_proc
proc upload_proc {} {
    set form [ns_getform]
    set tmp [ns_getformfile f]
    set f [open $tmp]
    fconfigure $f -translation binary
    set head [read $f 5]
    close $f
    ns_return 200 text/plain [list [ns_set get $form a] [file size $tmp] [ns_conn filelength f] \
        [expr {[ns_conn flags] & 0x80}] [ns_set get $form f.content-type] $head]
}
"#,
            )
            .unwrap();
        },
    );
    let form = "application/x-www-form-urlencoded";
    // A body whose request reaches maxinput is spooled, and every content
    // accessor sees it.
    let big = format!("a=1&b={}", "x".repeat(6000));
    let r = post(&srv, "/spool", form, &big);
    assert_eq!(r.status, 200, "{r:?} {:?}", srv.log());
    assert_eq!(r.body, "6006 128 6006 6006 1 1 1&b 6006");
    // In-memory content: contentchannel copies it to a temp file.
    let r = post(&srv, "/spool", form, "a=2&b=y");
    assert_eq!(r.body, "7 0 7 7 1 2 2&b 7");
    // GetIndices.
    let r = post(&srv, "/indices", form, &big);
    assert_eq!(
        r.body,
        "invalid offset: -1|invalid offset: 6007|invalid length: 6007|invalid length: -1|\
         wrong # args: should be \"ns_conn content ?off len?\"|1|0",
        "{:?}",
        srv.log()
    );
    let r = post(&srv, "/copy", form, &big);
    assert_eq!(r.body, "6004 1&b= 6006 0");
    // A multipart upload whose file part makes the body spill to disk.
    let file: String = (0..8000u32)
        .map(|i| char::from(b'a' + (i % 26) as u8))
        .collect();
    let body = format!(
        "--XyZ\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nval\r\n\
         --XyZ\r\nContent-Disposition: form-data; name=\"f\"; filename=\"big.txt\"\r\n\
         Content-Type: text/x-big\r\n\r\n{file}\r\n--XyZ--\r\n"
    );
    let r = post(&srv, "/upload", "multipart/form-data; boundary=XyZ", &body);
    assert_eq!(
        r.body,
        "val 8000 8000 128 text/x-big abcde",
        "{:?}",
        srv.log()
    );
    // Spool files go back to the pool when the connection is done: every
    // open spool file is a pooled one.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while open_spool_files() > nsd::content::pooled_temps() {
        assert!(std::time::Instant::now() < deadline, "spool files leaked");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(nsd::content::pooled_temps() >= 1);
    // maxupload, not maxinput, refuses a body.
    ok(
        &srv,
        "ns_limits set small -maxupload 5000; ns_limits register small s1 POST /spool; expr 1",
        "1",
    );
    let r = post(&srv, "/spool", form, &big);
    assert_eq!(r.status, 413);
    ok(
        &srv,
        "array set l [ns_limits get small]; set l(ndropped)",
        "1",
    );
    let r = post(&srv, "/spool", form, "a=3&b=z");
    assert_eq!(r.status, 200, "{r:?} {:?}", srv.log());
}

#[test]
fn unsupported_commands_fail_clearly() {
    let srv = TestServer::start("", |_| {});
    for c in [
        "ns_share x",
        "ns_sockselect",
        "ns_sockcallback",
        "ns_quewait",
        "ns_ictl cancel 1",
    ] {
        err(&srv, c, "not supported");
    }
    err(&srv, "ns_conn url", "no connection");
    ok(&srv, "ns_conn isconnected", "0");
}
