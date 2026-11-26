#![allow(clippy::unwrap_used)]

use super::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;

use nsdb::ExecResult;
use nspd::{DriverDb, Options};

type Logs = Arc<Mutex<Vec<String>>>;

fn logger() -> (Logger, Logs) {
    let logs: Logs = Arc::default();
    let l = Arc::clone(&logs);
    let log: Logger = Arc::new(move |sev, m| l.lock().unwrap().push(format!("{sev:?}: {m}")));
    (log, logs)
}

fn has(logs: &Logs, m: &str) -> bool {
    logs.lock().unwrap().iter().any(|l| l.contains(m))
}

fn ctx(host: Option<&str>, port: u16, log: &Logger) -> Ctx {
    Ctx {
        path: None,
        host: host.map(str::to_owned),
        port: i64::from(port),
        param: Some("prm".into()),
        timeout: 2,
        io_trace: false,
        trim_data: false,
        max_element_size: DEFAULT_MAX_ELEMENT_SIZE,
        verbose: true,
        conn_num: AtomicU32::new(0),
        ident: Mutex::new(String::new()),
        log: Arc::clone(log),
    }
}

/// An in-process SQLite daemon listening on a free port.
fn daemon() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || nspd::serve(&l, || Some(DriverDb::sqlite()), &Options::default()));
    port
}

type Seen = Arc<Mutex<Vec<String>>>;

/// A daemon that answers each request with the next scripted reply (an
/// empty reply sends nothing), then answers only `close`.
fn fake(script: Vec<(&'static str, Vec<u8>)>) -> (u16, Seen) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let seen: Seen = Arc::default();
    let s2 = Arc::clone(&seen);
    std::thread::spawn(move || {
        let (s, _) = l.accept().unwrap();
        let mut w = s.try_clone().unwrap();
        let mut r = BufReader::new(s);
        let next = |r: &mut BufReader<std::net::TcpStream>| {
            let mut name = String::new();
            let mut len = String::new();
            if r.read_line(&mut name).ok()? == 0 || r.read_line(&mut len).ok()? == 0 {
                return None;
            }
            let mut arg = vec![0; len.trim().parse().ok()?];
            r.read_exact(&mut arg).ok()?;
            let name = name.trim().to_owned();
            s2.lock()
                .unwrap()
                .push(format!("{name} {}", String::from_utf8_lossy(&arg)));
            Some(name)
        };
        for (want, reply) in script {
            let Some(got) = next(&mut r) else { return };
            assert_eq!(got, want);
            if w.write_all(&reply).is_err() {
                return;
            }
        }
        while let Some(got) = next(&mut r) {
            if got == "close" {
                let _ = w.write_all(b"ok\n");
            }
        }
    });
    (port, seen)
}

fn opened(mut rest: Vec<(&'static str, Vec<u8>)>) -> Vec<(&'static str, Vec<u8>)> {
    let mut v = vec![
        ("ping", b"ok\n".to_vec()),
        ("open", b"ok\n".to_vec()),
        ("identify", b"Fake Proxy Daemon v1\n".to_vec()),
    ];
    v.append(&mut rest);
    v
}

fn open_fake(
    script: Vec<(&'static str, Vec<u8>)>,
    f: impl FnOnce(&mut Ctx),
) -> (ExtConn, Seen, Logs) {
    let (port, seen) = fake(opened(script));
    let (log, logs) = logger();
    let mut c = ctx(Some("127.0.0.1"), port, &log);
    f(&mut c);
    let conn = ExtConn::open(&Arc::new(c), "ds", Some("u"), None).unwrap();
    (conn, seen, logs)
}

#[test]
fn sqlite_through_the_daemon() {
    let port = daemon();
    let (log, logs) = logger();
    let dir = tempfile::tempdir().unwrap();
    let ds = dir.path().join("x.db").display().to_string();
    let drv = ExtDriver::new(Some(ctx(Some("127.0.0.1"), port, &log)));
    assert_eq!((drv.name(), drv.db_type()), ("External", "External"));
    let mut c = drv.open(&ds, Some("u"), Some("p")).unwrap();
    assert!(c.connected() && c.supports_sp());
    assert_eq!(c.db_type().as_deref(), Some("SQLite"));
    assert_eq!(drv.ctx().unwrap().ident(), "SQLite Proxy Daemon v4.5");
    assert!(has(
        &logs,
        &format!("Notice: nsext: datasource opened: {ds} with ")
    ));
    assert!(has(&logs, "nsext: connect success: 127.0.0.1:"));
    assert_eq!(c.exec("create table t (a, b)").unwrap(), ExecResult::Dml(0));
    c.exec("insert into t values (1, 'x  '), (2, null)")
        .unwrap();
    assert_eq!(
        c.exec("select a, b from t").unwrap(),
        ExecResult::Rows(vec![])
    );
    assert_eq!(c.bind_row().unwrap().unwrap(), vec!["a", "b"]);
    assert_eq!(
        c.next_row().unwrap(),
        Some(vec![Some("1".into()), Some("x  ".into())])
    );
    assert_eq!(c.next_row_n(2).unwrap(), Some(vec![Some("2".into()), None]));
    assert_eq!(c.next_row().unwrap(), None);
    c.flush();
    assert!(c.try_flush().is_ok() && c.cancel().is_ok() && c.reset().is_ok());
    let e = c.exec("select * from nosuch").unwrap_err();
    assert!(e.message.contains("no such table"), "{e:?}");
    assert_ne!(e.code, "");
    let e = c.sp_start("p").unwrap_err();
    assert_eq!(
        (e.code.as_str(), e.message.as_str()),
        ("NSPD", "stored procedures are not supported")
    );
    assert!(c.bind_row().unwrap().is_err(), "no rows pending");
    assert!(c.as_any_mut().is_some());
    drop(c);
    assert!(has(&logs, "nsext: stopping db proxy daemon connection 0"));
}

#[test]
fn status_replies() {
    let (mut c, seen, logs) = open_fake(
        vec![
            ("exec", b"silentError\n".to_vec()),
            ("exec", b"plain failure\n".to_vec()),
            ("exec", b"CODE#\n".to_vec()),
            ("exec", b"CODE#bad thing\n".to_vec()),
            ("exec", b"OK\nexec_bogus\n".to_vec()),
            ("exec", b"ok\nexec_rows\n".to_vec()),
            ("bindrow", b"ok\n-1\n".to_vec()),
            ("getrow", b"ok\n3\nab 0\n2\n  -1\n".to_vec()),
            ("flush", b"nope#x\n".to_vec()),
            ("cancel", b"ok\n".to_vec()),
        ],
        |c| c.trim_data = true,
    );
    assert_eq!(c.exec("a").unwrap_err().message, "");
    assert!(has(
        &logs,
        "Debug: nsext: silent error string 'silentError'"
    ));
    assert_eq!(c.exec("b").unwrap_err().message, "");
    assert!(has(
        &logs,
        "Error: nsext: database error message: 'plain failure'"
    ));
    assert_eq!(c.exec("c").unwrap_err().message, "");
    assert!(has(
        &logs,
        "Error: nsext: invalid exception status string: 'CODE'"
    ));
    let e = c.exec("d").unwrap_err();
    assert_eq!((e.code.as_str(), e.message.as_str()), ("CODE", "bad thing"));
    assert!(has(
        &logs,
        "Debug: nsext: received exception code=CODE msg=bad thing"
    ));
    assert!(c.exec("e").is_err());
    assert_eq!(c.exec("f").unwrap(), ExecResult::Rows(vec![]));
    assert!(c.bind_row().unwrap().is_err(), "a row with no columns");
    assert_eq!(
        c.next_row_n(3).unwrap(),
        Some(vec![Some("ab".into()), None, Some(String::new())])
    );
    assert_eq!(c.try_flush().unwrap_err().code, "nope");
    assert!(c.cancel().is_ok());
    assert!(c.connected());
    drop(c);
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[1], "open ds#u##prm");
    assert_eq!(seen[10], "getrow 3");
    assert_eq!(seen.last().unwrap(), "close ");
}

#[test]
fn protocol_errors_drop_the_connection() {
    for (reply, msg) in [
        (
            &b"ok\nx\n"[..],
            "nsext: protocol error: number expected, got 'x'",
        ),
        (
            b"ok\n99999\n",
            "nsext: exceeded element size limit of 99999",
        ),
        (
            b"ok\n-5\n",
            "nsext: protocol error: number expected, got '-5'",
        ),
        (
            b"ok\n123456789012345678901234567890123\n",
            "nsext: protocol error: no record separator in '1234567890123456789012345678901'",
        ),
    ] {
        let (mut c, _, logs) = open_fake(vec![("bindrow", reply.to_vec())], |_| {});
        assert!(c.bind_row().unwrap().is_err());
        assert!(!c.connected(), "{msg}");
        assert!(has(&logs, msg), "{msg}: {:?}", logs.lock().unwrap());
        // A cleaned-up connection sends nothing more.
        assert!(c.exec("x").is_err());
    }
    // A reset reply that is not `ok`.
    let (mut c, _, logs) = open_fake(vec![("resethandle", b"nope\n".to_vec())], |_| {});
    assert!(c.reset().is_err() && !c.connected());
    assert!(has(
        &logs,
        "nsext: protocol error: received|nope| expected|ok|"
    ));
    // A reply that ends early.
    let (mut c2, _, logs2) = open_fake(vec![("getrow", b"ok\n5\nab".to_vec())], |c| c.timeout = 1);
    assert!(c2.next_row_n(1).is_err() && !c2.connected());
    assert!(has(&logs2, "nsext: exceeded proxy i/o timeout (1 seconds)"));
    assert!(has(&logs2, "nsext: read error: "));
}

#[test]
fn timeouts_and_dropped_daemons() {
    // No reply at all.
    let (mut c, _, logs) = open_fake(vec![("exec", Vec::new())], |c| c.timeout = 1);
    assert!(c.exec("x").is_err() && !c.connected());
    assert!(has(
        &logs,
        "Warning: nsext: exceeded proxy i/o timeout (1 seconds)"
    ));
    // Tracing, then a request that is never answered.
    let (port, _) = fake(opened(vec![]));
    let (log, logs) = logger();
    let mut cx = ctx(Some("127.0.0.1"), port, &log);
    cx.io_trace = true;
    let mut c = ExtConn::open(&Arc::new(cx), "ds", None, None).unwrap();
    assert!(has(&logs, "Notice: SENT |ping\n0\n|"));
    assert!(has(&logs, "Notice: REC |ok|"));
    // A request the daemon never answers.
    assert!(c.sp_returncode().is_err());
    assert!(has(
        &logs,
        "nsext: 'returncode' command to proxy daemon failed"
    ));
}

#[test]
fn stored_procedures() {
    let (mut c, seen, _) = open_fake(
        vec![
            ("spstart", b"ok\n".to_vec()),
            ("spsetparam", b"ok\n".to_vec()),
            ("spexec", b"ok\nexec_dml\n".to_vec()),
            ("spexec", b"ok\nexec_rows\n".to_vec()),
            ("spreturncode", b"0\n".to_vec()),
            ("spgetparams", b"ok\n2\n@a2\n@b-1\n1\n50\n-1\n".to_vec()),
            ("spgetparams", b"ok\n-1\n-1\n".to_vec()),
            ("spgetparams", b"ok\n2\n@azz\n".to_vec()),
        ],
        |_| {},
    );
    assert!(c.sp_start("proc").is_ok());
    assert!(c.sp_setparam("@a int in 1").is_ok());
    assert_eq!(c.sp_exec().unwrap(), ExecResult::Dml(0));
    assert_eq!(c.sp_exec().unwrap(), ExecResult::Rows(vec![]));
    assert_eq!(c.sp_returncode().unwrap(), "0");
    assert_eq!(
        c.sp_getparams().unwrap(),
        vec![("@a".into(), Some("5".into())), ("@b".into(), None)]
    );
    assert!(c.sp_getparams().is_err(), "no parameters");
    assert!(c.connected(), "both empty lists were read");
    assert!(c.sp_getparams().is_err());
    assert!(!c.connected());
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[3], "spstart proc");
    assert_eq!(seen[4], "spsetparam @a int in 1");
}

#[test]
fn opening_fails() {
    let (log, logs) = logger();
    // Nothing listens on the port.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let drv = ExtDriver::new(Some(ctx(Some("127.0.0.1"), port, &log)));
    assert!(drv.open("ds", None, None).is_err());
    assert!(has(
        &logs,
        &format!("Error: nsext: connect failure: 127.0.0.1:{port}")
    ));
    // A driver whose config was unusable never opens.
    assert!(ExtDriver::new(None).open("ds", None, None).is_err());
    // Bad replies while opening.
    for script in [
        vec![("ping", b"no\n".to_vec())],
        vec![
            ("ping", b"ok\n".to_vec()),
            ("open", b"E#cannot open\n".to_vec()),
        ],
        vec![
            ("ping", b"ok\n".to_vec()),
            ("open", b"ok\n".to_vec()),
            ("identify", Vec::new()),
        ],
    ] {
        let (port, _) = fake(script);
        let mut c = ctx(Some("127.0.0.1"), port, &log);
        c.timeout = 1;
        assert!(ExtConn::open(&Arc::new(c), "ds", None, None).is_err());
    }
    assert!(has(
        &logs,
        "nsext: protocol error on ping: received|no| expected|ok|"
    ));
    assert!(has(
        &logs,
        "nsext: 'identify' command to proxy daemon failed"
    ));
    // A local daemon that cannot be spawned, or exits with an error.
    let mut c = ctx(None, 0, &log);
    c.path = Some("/nonexistent/nspd".into());
    assert!(ExtConn::open(&Arc::new(c), "ds", None, None).is_err());
    assert!(has(&logs, "nsext: spawn failed for '/nonexistent/nspd'"));
    let mut c = ctx(None, 0, &log);
    c.path = Some("/bin/false".into());
    assert!(ExtConn::open(&Arc::new(c), "ds", None, None).is_err());
    assert!(has(&logs, "nsext: proxy returned non-zero exit code: 1"));
    // One that exits 0 but serves nothing.
    let mut c = ctx(None, 0, &log);
    c.path = Some("/bin/true".into());
    assert!(ExtConn::open(&Arc::new(c), "ds", None, None).is_err());
    assert!(has(
        &logs,
        "nsext: connection dropped by external proxy daemon"
    ));
}

#[test]
fn config() {
    let (log, logs) = logger();
    let mut cfg = ns_core::config::Config::new();
    let dc = |cfg: &ns_core::config::Config, name: &str| {
        let d = DriverConfig {
            name,
            path: format!("ns/db/driver/{name}"),
            config: cfg,
            home: Some(std::path::Path::new("/srv/ns")),
            log: &log,
        };
        driver_init(&d)
    };
    // Neither a daemon nor a host.
    assert!(matches!(dc(&cfg, "none"), Err(Some(_))));
    assert!(has(
        &logs,
        "nsext: bad config: localdaemon or remotehost required"
    ));
    cfg.put("ns/db/driver/nohost", "RemoteHost", "db");
    assert!(dc(&cfg, "nohost").is_err());
    assert!(has(
        &logs,
        "nsext: bad config: proxyhost requires proxyport"
    ));
    cfg.put("ns/db/driver/local", "LocalDaemon", "nssqlitepd");
    cfg.put("ns/db/driver/local", "timeout", "5");
    cfg.put("ns/db/driver/local", "iotrace", "on");
    cfg.put("ns/db/driver/local", "TrimData", "true");
    cfg.put("ns/db/driver/local", "maxElementSize", "10");
    cfg.put("ns/db/driver/local", "param", "p");
    let d = Ctx::from_config(&DriverConfig {
        name: "local",
        path: "ns/db/driver/local".into(),
        config: &cfg,
        home: Some(std::path::Path::new("/srv/ns")),
        log: &log,
    })
    .unwrap();
    assert_eq!(
        d.path.as_deref(),
        Some(std::path::Path::new("/srv/ns/bin/nssqlitepd"))
    );
    assert_eq!(
        (d.timeout, d.io_trace, d.trim_data, d.max_element_size),
        (5, true, true, 10)
    );
    assert_eq!(d.param.as_deref(), Some("p"));
    assert!(format!("{d:?}").contains("nssqlitepd"));
    cfg.put("ns/db/driver/remote", "RemoteHost", "db");
    cfg.put("ns/db/driver/remote", "RemotePort", "8199");
    cfg.put("ns/db/driver/abs", "LocalDaemon", "/opt/pd");
    let Ok(d) = dc(&cfg, "remote") else {
        panic!("remote config")
    };
    assert_eq!(d.name(), "External");
    let e = Ctx::from_config(&DriverConfig {
        name: "abs",
        path: "ns/db/driver/abs".into(),
        config: &cfg,
        home: None,
        log: &log,
    })
    .unwrap();
    assert_eq!(e.path.as_deref(), Some(std::path::Path::new("/opt/pd")));
    assert_eq!(
        (e.timeout, e.max_element_size),
        (DEFAULT_TIMEOUT, DEFAULT_MAX_ELEMENT_SIZE)
    );
    assert!(has(&logs, "Notice: nsext: module started"));
    // The database type is the identity's first word, at most 63 bytes.
    e.set_ident("Oracle Proxy Daemon v4");
    assert_eq!(e.db_type(), "Oracle");
    e.set_ident(&"é".repeat(40));
    assert_eq!(e.db_type().len(), 62);
    e.set_ident("");
    assert_eq!(e.db_type(), "");
    assert!(format!("{:?}", ExtDriver::new(None)).contains("None"));
    register();
    assert!(nsdb::driver_factory("nsext").is_some());
}

#[test]
fn sends_need_their_argument() {
    let (mut c, _, logs) = open_fake(vec![], |_| {});
    assert!(!c.send(Msg::Exec, None));
    assert!(has(
        &logs,
        "Bug: nsext: 'exec' message requires argument (connection 0)"
    ));
    assert!(format!("{c:?}").contains("num: 0"));
    assert_eq!(c.number(), 0);
}

use nspd::msg::Msg;

#[test]
fn every_reply_is_read_in_step() {
    // Each request gets a reply of exactly the items `Msg::reply` lists
    // (a failed status alone); the client must read all of them and be
    // ready again (`verus/extproto.rs`).
    let ok = |s: &str| s.as_bytes().to_vec();
    let (mut c, _, logs) = open_fake(
        vec![
            ("exec", ok("ok\nexec_dml\n")),
            ("exec", ok("E#bad\n")),
            ("bindrow", ok("ok\n1\na-1\n")),
            ("bindrow", ok("E#bad\n")),
            ("getrow", ok("ok\n1\n1-1\n")),
            ("getrow", ok("E#bad\n")),
            ("flush", ok("ok\n")),
            ("cancel", ok("E#bad\n")),
            ("resethandle", ok("ok\n")),
            ("spstart", ok("ok\n")),
            ("spsetparam", ok("E#bad\n")),
            ("spexec", ok("ok\nexec_rows\n")),
            ("spexec", ok("E#bad\n")),
            ("spreturncode", ok("7\n")),
            ("spgetparams", ok("ok\n2\n@a-1\n1\n1-1\n")),
            ("spgetparams", ok("E#bad\n")),
            ("ping", ok("ok\n")),
            ("traceon", ok("ok\n")),
            ("traceoff", ok("ok\n")),
            ("gettypes", ok("int text\n")),
            ("resultid", ok("id\n")),
            ("resultrows", ok("3\n")),
            ("setmaxrows", ok("ok\n")),
            ("createtmpfile", ok("ok\n/r/pd1\n")),
            ("createtmpfile", ok("cannot\n")),
            ("deletefile", ok("ok\n")),
            ("deletefile", ok("cannot\n")),
            ("openfile", ok("ok\n3\n")),
            ("writefile", ok("ok\n")),
            ("closefile", ok("ok\n")),
            ("openfile", ok("ok\n4\n")),
            ("readfile", ok("ok\n2\nhi-1\n")),
            ("readfile", ok("ok\n0\n-1\n")),
            ("closefile", ok("ok\n")),
            ("openfile", ok("cannot\n")),
            ("openfile", ok("ok\n5\n")),
            ("readfile", ok("cannot\n")),
            ("closefile", ok("cannot\n")),
        ],
        |_| {},
    );
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::write(&src, "x").unwrap();
    let dst = dir.path().join("dst").display().to_string();
    let src = src.display().to_string();
    let cmd = |c: &mut ExtConn, args: &[&str]| {
        let mut argv = vec!["ns_ext".to_owned(), args[0].to_owned(), "nsdb0".to_owned()];
        argv.extend(args[1..].iter().map(|a| (*a).to_owned()));
        let r = cmd::ext_cmd(c, &argv).map(|o| match o {
            cmd::Out::Str(s) => s,
            cmd::Out::List(l) => l.join(" "),
        });
        assert!(c.in_step(), "{args:?} {r:?} {:?}", logs.lock().unwrap());
        r
    };
    let step = |c: &ExtConn, what: &str| {
        assert!(c.in_step(), "{what} {:?}", logs.lock().unwrap());
    };
    assert!(c.in_step());
    assert!(c.exec("a").is_ok());
    step(&c, "exec");
    assert!(c.exec("b").is_err());
    step(&c, "exec error");
    assert!(c.bind_row().unwrap().is_ok());
    step(&c, "bindrow");
    assert!(c.bind_row().unwrap().is_err());
    step(&c, "bindrow error");
    assert!(c.next_row_n(1).unwrap().is_some());
    step(&c, "getrow");
    assert!(c.next_row_n(1).is_err());
    step(&c, "getrow error");
    assert!(c.try_flush().is_ok());
    assert!(c.cancel().is_err());
    assert!(c.reset().is_ok());
    assert!(c.sp_start("p").is_ok());
    assert!(c.sp_setparam("@a int in 1").is_err());
    assert!(c.sp_exec().is_ok());
    assert!(c.sp_exec().is_err());
    assert_eq!(c.sp_returncode().unwrap(), "7");
    assert_eq!(c.sp_getparams().unwrap().len(), 1);
    assert!(c.sp_getparams().is_err());
    step(&c, "stored procedures");
    assert_eq!(cmd(&mut c, &["ping"]).unwrap(), "ok");
    assert!(cmd(&mut c, &["traceon", "/t"]).is_ok());
    assert!(cmd(&mut c, &["traceoff"]).is_ok());
    assert_eq!(cmd(&mut c, &["gettypes"]).unwrap(), "int text");
    assert_eq!(cmd(&mut c, &["resultid"]).unwrap(), "id");
    assert_eq!(cmd(&mut c, &["resultrows"]).unwrap(), "3");
    assert!(cmd(&mut c, &["setmaxrows", "1"]).is_ok());
    assert_eq!(cmd(&mut c, &["mktemp"]).unwrap(), "/r/pd1");
    assert!(cmd(&mut c, &["mktemp"]).is_err());
    assert_eq!(cmd(&mut c, &["rm", "/r/pd1"]).unwrap(), "");
    assert_eq!(cmd(&mut c, &["rm", "/r/pd1"]).unwrap(), "rm error: cannot");
    assert_eq!(cmd(&mut c, &["cpto", &src, "/r/f"]).unwrap(), "");
    assert_eq!(cmd(&mut c, &["cpfrom", &dst, "/r/f"]).unwrap(), "");
    assert_eq!(std::fs::read(&dst).unwrap(), b"hi");
    assert!(cmd(&mut c, &["cpfrom", &dst, "/r/f"]).is_err());
    assert!(cmd(&mut c, &["cpfrom", &dst, "/r/f"]).is_err());
    assert!(c.connected());
}

#[test]
fn out_of_step_use_drops_the_connection() {
    // A request sent while a reply is unread.
    let (mut c, _, logs) = open_fake(vec![("exec", b"ok\nexec_dml\n".to_vec())], |_| {});
    assert!(c.send(Msg::Exec, Some(b"x")));
    assert!(!c.in_step());
    assert!(!c.send(Msg::Ping, None));
    assert!(!c.connected() && !c.in_step());
    assert!(has(
        &logs,
        "Bug: nsext: protocol error: 'ping' sent with 2 reply items unread"
    ));
    // A read of an item the reply does not hold.
    let (mut c, _, logs) = open_fake(vec![("ping", b"ok\n".to_vec())], |_| {});
    assert!(c.send(Msg::Ping, None));
    assert!(c.get_list().is_none());
    assert!(!c.connected());
    assert!(has(
        &logs,
        "Bug: nsext: protocol error: read a List item, expected [Line]"
    ));
    // Nothing is read once the reply is complete.
    let (mut c, _, logs) = open_fake(vec![("ping", b"ok\n".to_vec())], |_| {});
    assert!(c.send(Msg::Ping, None) && c.ping_reply());
    assert!(c.get_status(10).is_none() && !c.connected());
    assert!(has(
        &logs,
        "Bug: nsext: protocol error: read a Status item, expected []"
    ));
}

#[test]
fn list_sizes_follow_the_verus_model() {
    use crate::conn::{size_verdict, Size};
    // `list_framing` of verus/extproto.rs, over a range of sizes/limits.
    for max in -3..=6 {
        for n in -5..=8 {
            let v = size_verdict(n, max);
            if let Size::Elem(k) = v {
                assert!(n >= 0 && n <= max && k == usize::try_from(n).unwrap());
            }
            if (0..=max).contains(&n) {
                assert_eq!(v, Size::Elem(usize::try_from(n).unwrap()));
            }
            if v == Size::End {
                assert_eq!(n, -1);
            }
            if n == -1 && max >= 0 {
                assert_eq!(v, Size::End);
            }
            if n < -1 {
                assert!(matches!(v, Size::TooBig | Size::Bad), "{n} {max}");
            }
            assert_eq!(v == Size::TooBig, n > max);
        }
    }
}
