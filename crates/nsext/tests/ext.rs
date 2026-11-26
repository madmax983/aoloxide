//! `ns_db` through an `External` pool, and `ns_ext`, in a live server
//! talking to an in-process proxy daemon.

#![allow(clippy::unwrap_used)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use nsd::{Nsd, NsdOptions};
use nspd::{DriverDb, OpenArgs, Options, PdDb, PdError, PdExec};

struct Srv {
    nsd: Nsd,
    _home: tempfile::TempDir,
    port: u16,
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.nsd.shutdown();
    }
}

impl Srv {
    fn get(&self, url: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        s.write_all(format!("GET {url} HTTP/1.0\r\n\r\n").as_bytes())
            .unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        let r = String::from_utf8_lossy(&out).into_owned();
        r.split_once("\r\n\r\n")
            .map_or_else(|| r.clone(), |(_, b)| b.to_owned())
    }

    fn logs(&self) -> Vec<String> {
        self.nsd.global.log.captured()
    }
}

/// Boot a server whose Tcl library holds `tcl`; `/t?n=proc` runs `proc`.
fn boot(extra: &str, tcl: &str) -> Srv {
    nsext::register();
    let home = tempfile::tempdir().unwrap();
    nsd::init_tcl::install_home(home.path(), true).unwrap();
    let lib = home.path().join("servers/s1/modules/tcl");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::create_dir_all(home.path().join("servers/s1/pages")).unwrap();
    std::fs::write(
        lib.join("t.tcl"),
        format!(
            "ns_register_proc GET /t t_run\nproc t_run {{}} {{ ns_return 200 text/plain [[ns_queryget n]] }}\n{tcl}"
        ),
    )
    .unwrap();
    let cfg = format!(
        r#"
ns_section ns/parameters
ns_param home {home}
ns_section ns/servers
ns_param s1 "S1"
ns_section ns/server/s1/modules
ns_param nssock nssock.so
ns_param nsdb nsdb.so
ns_section ns/server/s1/module/nssock
ns_param port 0
ns_param address 127.0.0.1
ns_param hostname localhost
{extra}
"#,
        home = home.path().display()
    );
    let mut o = NsdOptions::from_text(&cfg);
    o.log_memory = true;
    let mut m = nsd::builtin_modules();
    m.register("nsdb", Arc::new(nsdb::init));
    m.register("nsext", Arc::new(nsext::init));
    o.modules = m;
    let nsd = Nsd::new(o).unwrap();
    nsd.start().unwrap();
    let port = nsd.drivers()[0].info.port();
    Srv {
        nsd,
        _home: home,
        port,
    }
}

fn serve<D: PdDb + 'static>(f: impl Fn() -> D + Send + Sync + 'static) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || nspd::serve(&l, move || Some(f()), &Options::default()));
    port
}

fn pools(port: u16, ds: &str) -> String {
    format!(
        "ns_section ns/db/drivers\n\
         ns_param external nsext.so\n\
         ns_param sqlite nssqlite.so\n\
         ns_section ns/db/driver/external\n\
         ns_param RemoteHost 127.0.0.1\n\
         ns_param RemotePort {port}\n\
         ns_param Param extra\n\
         ns_param TrimData true\n\
         ns_section ns/db/pools\n\
         ns_param main \"Main\"\n\
         ns_param lite \"Lite\"\n\
         ns_section ns/db/pool/main\n\
         ns_param driver external\n\
         ns_param datasource {ds}\n\
         ns_param user web\n\
         ns_param connections 2\n\
         ns_section ns/db/pool/lite\n\
         ns_param driver sqlite\n\
         ns_param datasource :memory:\n\
         ns_section ns/server/s1/db\n\
         ns_param pools *\n\
         ns_param defaultpool main\n"
    )
}

const SCRIPT: &str = r#"
proc t_db {} {
    set db [ns_db gethandle main]
    set out {}
    ns_db dml $db "create table if not exists t (id integer, name text)"
    ns_db dml $db "delete from t"
    ns_db dml $db "insert into t values (1, 'alice  ')"
    ns_db dml $db "insert into t values (2, null)"
    set row [ns_db select $db "select id, name from t order by id"]
    while {[ns_db getrow $db $row]} {
        lappend out "[ns_set get $row id]=[ns_set get $row name]"
    }
    lappend out [ns_set get [ns_db 1row $db "select count(*) as n from t"] n]
    lappend out [ns_db 0or1row $db "select * from t where id = 9"]
    lappend out [ns_db exec $db "select 1"] [ns_set size [ns_db bindrow $db]] [ns_db exec $db "update t set id = id"]
    lappend out [ns_db flush $db] [ns_db cancel $db] [ns_db resethandle $db]
    lappend out [catch {ns_db dml $db "select * from nosuch"} m] [string match {Database operation "dml" failed (exception *no such table: nosuch")} $m]
    lappend out [ns_db exception $db]
    lappend out [catch {ns_db sp_start $db p} m] $m
    lappend out [ns_db dbtype $db] [ns_db driver $db] [ns_db connected $db]
    ns_db releasehandle $db
    return $out
}
proc t_ext {} {
    set db [ns_db gethandle main]
    set out {}
    lappend out [ns_ext ping $db] [ns_ext isremote $db] [string is integer [ns_ext number $db]]
    lappend out [ns_ext identify $db] [ns_ext dbtype $db] [ns_ext gettypes $db]
    ns_db dml $db "create table if not exists r (a)"
    ns_db dml $db "insert into r values (1), (2), (3)"
    lappend out [ns_ext resultrows $db] [ns_ext resultid $db]
    lappend out [ns_ext setmaxrows $db 2]
    set row [ns_db select $db "select a from r"]
    set n 0
    while {[ns_db getrow $db $row]} { incr n }
    lappend out $n [ns_ext setmaxrows $db 0]
    lappend out [ns_ext resethandle $db]
    lappend out [ns_ext traceon $db [ns_ext mktemp $db]] [ns_ext traceoff $db]
    lappend out [catch {ns_ext traceon $db rel} m] $m
    lappend out [catch {ns_ext traceon $db} m] $m
    lappend out [catch {ns_ext setmaxrows $db x} m] $m
    lappend out [catch {ns_ext setmaxrows $db} m] $m
    lappend out [catch {ns_ext bogus $db} m] $m
    lappend out [catch {ns_ext ping} m] $m
    lappend out [catch {ns_ext ping nsdbz} m] $m
    lappend out [catch {ns_ext cpto $db} m] $m [catch {ns_ext cpfrom $db} m] $m [catch {ns_ext rm $db} m] $m
    ns_db releasehandle $db
    set lite [ns_db gethandle lite]
    lappend out [catch {ns_ext ping $lite} m] $m
    ns_db releasehandle $lite
    return $out
}
proc t_files {} {
    set db [ns_db gethandle main]
    set out {}
    set remote [ns_ext mktemp $db]
    set src [tdir]/src
    set dst [tdir]/dst
    set f [open $src w]
    fconfigure $f -translation binary
    puts -nonewline $f [string repeat "0123456789" 250]
    close $f
    lappend out [ns_ext cpto $db $src $remote]
    lappend out [ns_ext cpfrom $db $dst $remote]
    set f [open $dst]
    lappend out [string length [read $f]]
    close $f
    lappend out [ns_ext rm $db $remote] [ns_ext rm $db $remote]
    lappend out [catch {ns_ext cpto $db /nonexistent/x $remote} m] $m
    lappend out [catch {ns_ext cpfrom $db $dst /nonexistent/y} m] $m
    lappend out [catch {ns_ext cpfrom $db /nonexistent/dir/z $remote} m] $m
    lappend out [catch {ns_ext cpto $db $src /nonexistent/y} m] $m
    file delete $src $dst
    ns_db releasehandle $db
    return $out
}
"#;

#[test]
fn ns_db_through_a_remote_daemon() {
    let port = serve(DriverDb::sqlite);
    let dir = tempfile::tempdir().unwrap();
    let ds = dir.path().join("main.db").display().to_string();
    let srv = boot(&pools(port, &ds), SCRIPT);
    let out = srv.get("/t?n=t_db");
    assert_eq!(
        out,
        "1=alice 2= 2 {} NS_ROWS 1 NS_DML {} {} 0 1 1 {Unknown {no such table: nosuch}} 1 \
         {Database operation \"sp_start\" failed (exception NSPD, \"stored procedures are not supported\")} \
         SQLite External 1",
        "{:?}",
        srv.logs()
    );
    let logs = srv.logs();
    assert!(
        logs.iter()
            .any(|l| l.contains("nsext: connect success: 127.0.0.1:")),
        "{logs:?}"
    );
    assert!(logs.iter().any(|l| l.contains("nsext: module started")));
}

#[test]
fn ns_ext_subcommands() {
    let port = serve(DriverDb::sqlite);
    let dir = tempfile::tempdir().unwrap();
    let ds = dir.path().join("ext.db").display().to_string();
    let srv = boot(&pools(port, &ds), SCRIPT);
    let out = srv.get("/t?n=t_ext");
    let want = [
        "ok 1 1 {SQLite Proxy Daemon v4.5} SQLite {integer real text blob null} 3 {} {Max rows set to 2} 2",
        "{Max rows set to 0} 0",
        "TRACE {Trace is off.}",
        "1 {filepath argument: \"rel\" must be absolute.}",
        "1 {insufficient args: should be \"ns_ext traceon dbId filepath\"}",
        "1 {rowcount argument: \"x\" must be numeric.}",
        "1 {insufficient args: should be \"ns_ext setmaxrows dbId rowcount\"}",
        "1 {unknown command \"bogus\": should be ping, gettypes, number, traceon, traceoff, identify, resultid, resultrows, setmaxrows, mktemp, rm, cpto, cpfrom.}",
        "1 {insufficient args: should be \"ns_ext command dbId (optional cmd arg)\"}",
        "1 {invalid database id:  \"nsdbz\"}",
        "1 {insufficient args: should be \"ns_ext cpto dbId localSrcFile remoteDestFile\"}",
        "1 {insufficient args: should be \"ns_ext cpfrom dbId localDestFile remoteSrcFile\"}",
        "1 {insufficient args: should be \"ns_ext rm dbId remoteFile\"}",
        "1 {handle \"nsdb0\" is not of type \"External\"}",
    ];
    let got = out.replace(
        &out[out.find("{Trace is on").unwrap_or(0)..]
            .split_once('}')
            .map_or(String::new(), |(a, _)| format!("{a}}}")),
        "TRACE",
    );
    assert_eq!(got, want.join(" "), "{out} {:?}", srv.logs());
}

#[test]
fn remote_file_copies() {
    let port = serve(DriverDb::sqlite);
    let dir = tempfile::tempdir().unwrap();
    let ds = dir.path().join("f.db").display().to_string();
    let tcl = format!(
        "proc tdir {{}} {{ return {{{}}} }}\n{SCRIPT}",
        dir.path().display()
    );
    let srv = boot(&pools(port, &ds), &tcl);
    let out = srv.get("/t?n=t_files");
    assert!(
        out.starts_with("{} {} 2500 {} {rm error: Can't delete file /"),
        "{out} {:?}",
        srv.logs()
    );
    for m in [
        "{cpto error: open on /nonexistent/x failed: No such file or directory}",
        "{cpfrom error: Can't open file /nonexistent/y (oflags=0): No such file or directory}",
        "{cpfrom error: open/create on /nonexistent/dir/z failed: No such file or directory}",
        "{cpto error: Can't open file /nonexistent/y (oflags=2): No such file or directory}",
    ] {
        assert!(out.contains(m), "{m}: {out}");
    }
}

/// A daemon with stored procedures: `spexec` runs the procedure name as
/// SQL, and every `in` parameter is echoed back as an output.
struct SpDb {
    inner: DriverDb,
    proc_sql: String,
    params: Vec<(String, Option<String>)>,
}

impl PdDb for SpDb {
    fn open(&mut self, a: &OpenArgs) -> Result<(), PdError> {
        self.inner.open(a)
    }
    fn close(&mut self) -> Result<(), PdError> {
        self.inner.close()
    }
    fn identify(&mut self) -> String {
        "SpLite Proxy Daemon v1".into()
    }
    fn get_types(&mut self) -> String {
        self.inner.get_types()
    }
    fn exec(&mut self, sql: &str) -> Result<PdExec, PdError> {
        self.inner.exec(sql)
    }
    fn bind_row(&mut self) -> Result<Vec<String>, PdError> {
        self.inner.bind_row()
    }
    fn get_row(&mut self, n: i64) -> Result<Option<Vec<Option<String>>>, PdError> {
        self.inner.get_row(n)
    }
    fn flush(&mut self) -> Result<(), PdError> {
        self.inner.flush()
    }
    fn sp_start(&mut self, p: &str) -> Result<(), PdError> {
        p.clone_into(&mut self.proc_sql);
        self.params.clear();
        Ok(())
    }
    fn sp_set_param(&mut self, a: &str) -> Result<(), PdError> {
        let w: Vec<&str> = a.split(' ').collect();
        if w.len() != 4 {
            return Err(PdError::exception("SP", format!("bad param {a}")));
        }
        self.params.push((w[0].to_owned(), Some(w[3].to_owned())));
        Ok(())
    }
    fn sp_exec(&mut self) -> Result<PdExec, PdError> {
        let sql = self.proc_sql.clone();
        self.inner.exec(&sql)
    }
    fn sp_return_code(&mut self) -> String {
        self.params.len().to_string()
    }
    fn sp_get_params(&mut self) -> Result<Vec<(String, Option<String>)>, PdError> {
        Ok(self.params.clone())
    }
}

#[test]
fn stored_procedures_through_ns_db() {
    let port = serve(|| SpDb {
        inner: DriverDb::sqlite(),
        proc_sql: String::new(),
        params: Vec::new(),
    });
    let script = r#"
proc t_sp {} {
    set db [ns_db gethandle main]
    set out {}
    lappend out [ns_db sp_start $db "select 7 as seven"]
    lappend out [ns_db sp_setparam $db @a int in 5] [ns_db sp_setparam $db @b varchar out x]
    lappend out [ns_db sp_exec $db]
    set row [ns_db bindrow $db]
    lappend out [ns_db getrow $db $row] [ns_set get $row seven] [ns_db getrow $db $row]
    lappend out [ns_db sp_returncode $db]
    set p [ns_db sp_getparams $db]
    lappend out [ns_set size $p] [ns_set key $p 0] [ns_set value $p 0] [ns_set key $p 1] [ns_set value $p 1]
    lappend out [catch {ns_db sp_setparam $db @c int sideways 1} m] $m
    lappend out [ns_db sp_start $db "create table s (a)"] [ns_db sp_exec $db]
    lappend out [catch {ns_db sp_getparams $db} m] $m
    lappend out [ns_db sp_start $db "select * from nosuch"] [catch {ns_db sp_exec $db} m] $m
    ns_db releasehandle $db
    set lite [ns_db gethandle lite]
    lappend out [catch {ns_db sp_exec $lite} m] $m
    ns_db releasehandle $lite
    return $out
}
"#;
    let dir = tempfile::tempdir().unwrap();
    let ds = dir.path().join("sp.db").display().to_string();
    let srv = boot(&pools(port, &ds), script);
    let out = srv.get("/t?n=t_sp");
    assert_eq!(
        out,
        "0 1 1 NS_ROWS 1 7 0 2 2 @a 5 @b x 1 {bad option \"sideways\": must be in, or out} \
         0 NS_DML 1 {Database operation \"sp_getparams\" failed} \
         0 1 {Database operation \"sp_exec\" failed (exception Unknown, \"no such table: nosuch\")} \
         1 {ns_db sp_exec: stored procedures are not supported by the sqlite driver}",
        "{:?}",
        srv.logs()
    );
}
