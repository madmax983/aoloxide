use super::*;
use std::sync::atomic::AtomicUsize;

/// A SQLite driver that counts opens and can make `exec` slow.
struct TestDriver {
    opens: AtomicUsize,
    delay: Duration,
}

struct SlowConn {
    inner: Box<dyn DbConnection>,
    delay: Duration,
}

impl DbConnection for SlowConn {
    fn exec(&mut self, sql: &str) -> Result<ExecResult, DbError> {
        std::thread::sleep(self.delay);
        self.inner.exec(sql)
    }
    fn next_row(&mut self) -> Result<Option<Vec<Option<String>>>, DbError> {
        self.inner.next_row()
    }
    fn flush(&mut self) {
        self.inner.flush();
    }
}

impl DbDriver for TestDriver {
    fn name(&self) -> &'static str {
        "test"
    }
    fn db_type(&self) -> &'static str {
        "Test"
    }
    fn open(
        &self,
        datasource: &str,
        user: Option<&str>,
        password: Option<&str>,
    ) -> Result<Box<dyn DbConnection>, DbError> {
        if datasource == "fail" {
            return Err(DbError::msg("cannot open"));
        }
        self.opens.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(SlowConn {
            inner: SqliteDriver.open(datasource, user, password)?,
            delay: self.delay,
        }))
    }
}

fn test_driver(delay: Duration) -> Arc<TestDriver> {
    Arc::new(TestDriver {
        opens: AtomicUsize::new(0),
        delay,
    })
}

fn pool_with(n: i64, f: impl FnOnce(&mut PoolConfig)) -> Arc<Pool> {
    let mut c = PoolConfig::new("p", Arc::new(SqliteDriver), ":memory:");
    c.nhandles = n;
    f(&mut c);
    Arc::new(Pool::new(c, Arc::new(|_, _| {})))
}

type Captured = Arc<Mutex<Vec<String>>>;

fn capture() -> (Logger, Captured) {
    let logs: Captured = Arc::default();
    let l = Arc::clone(&logs);
    let log: Logger = Arc::new(move |sev, m| lock(&l).push(format!("{sev:?}: {m}")));
    (log, logs)
}

fn pool(n: i64) -> Arc<Pool> {
    pool_with(n, |_| {})
}

#[test]
fn split_sql_follows_interpret_sql_file() {
    let (stmts, rest) =
        split_sql(b"-- c\ncreate table t (a);\n -- kept\ninsert into t values ('x;y', 'it''s');");
    assert_eq!(
        stmts,
        vec![
            "create table t (a)",
            "\n -- kept\ninsert into t values ('x;y', 'it''s')"
        ]
    );
    assert_eq!(rest, "");
    // Double quotes do not quote; a trailing statement is left over.
    let (stmts, rest) = split_sql(b"select \"a;b\";\nselect 2\n");
    assert_eq!(stmts, vec!["select \"a", "b\""]);
    assert_eq!(rest, "\nselect 2\n");
    let (stmts, rest) = split_sql(b"select 1;\nselect 2");
    assert_eq!((stmts.len(), rest.as_str()), (1, "\nselect 2"));
    // A single `-` at the start of a line is kept.
    let (stmts, _) = split_sql(b"\n-1;");
    assert_eq!(stmts, vec!["\n-1"]);
    // An unterminated quote and a lone trailing `-`.
    assert_eq!(split_sql(b"'abc").1, "'abc");
    assert_eq!(split_sql(b"x;\n-").1, "\n");
    assert_eq!(split_sql(b"'a'").1, "'a");
}

#[test]
fn quote_list_follows_quote_list_to_list_cmd() {
    assert_eq!(quote_list_to_list("a b 'c d'"), vec!["a", "b", "c d"]);
    assert_eq!(quote_list_to_list("'it''s'"), vec!["it", "s"]);
    assert_eq!(quote_list_to_list("'a','b'"), vec!["a", ",b"]);
    assert_eq!(quote_list_to_list("'b\\'c' x\\ y"), vec!["b'c", "x y"]);
    assert_eq!(quote_list_to_list("''  tail\\"), vec!["", "tail\\"]);
}

#[test]
fn csv_follows_get_csv_cmd() {
    assert_eq!(parse_csv(" a , b ,\"c \","), vec!["a", "b", "c ", ""]);
    assert_eq!(
        parse_csv("a,\"b,c\",\"d\"\"e\","),
        vec!["a", "b,c", "d\"e", ""]
    );
    assert_eq!(parse_csv(""), Vec::<String>::new());
    assert_eq!(parse_csv("   "), Vec::<String>::new());
    assert_eq!(parse_csv(","), vec![""]);
    assert_eq!(parse_csv("a\rb"), vec!["a"]);
    assert_eq!(parse_csv("\"x\""), vec!["x"]);
    assert_eq!(parse_csv("\"x"), vec!["x"]);
}

#[test]
fn op_failed_message() {
    assert_eq!(
        op_failed("select", Some(("E1", "bad"))).message,
        "Database operation \"select\" failed (exception E1, \"bad\")"
    );
    assert_eq!(
        op_failed("dml", Some(("E1", ""))).message,
        "Database operation \"dml\" failed (exception E1)"
    );
    assert_eq!(
        op_failed("dml", Some(("", "ignored"))).message,
        "Database operation \"dml\" failed"
    );
    assert_eq!(
        op_failed("bouncepool", None).message,
        "Database operation \"bouncepool\" failed"
    );
    assert_eq!(columns_set(&["x".into()]).get("x"), Some(""));
    assert!(drivers().contains_key("sqlite"));
}

#[test]
fn pool_handles() {
    let p = pool(1);
    let mut h = p.get_handles(1, None).expect("handle").pop().expect("one");
    assert_eq!(p.outstanding(), 1);
    assert!(matches!(
        p.get_handles(2, None),
        Err(GetError::Failed(m)) if m.contains("failed to get 2 handles from a db pool of only 1 handles: 'p'")
    ));
    assert_eq!(
        h.exec("select 1 as one").expect("exec"),
        ExecResult::Rows(vec!["one".into()])
    );
    assert!(h.exec("bogus sql").is_err());
    assert_ne!(h.exception().1, "");
    assert!(!h.dml("select 1"));
    assert_eq!(
        h.exception(),
        ("NSDB", "Query was not a DML or DDL command.")
    );
    h.clear_exception();
    assert!(h.select("create table t (a)").is_none());
    assert_eq!(
        h.exception(),
        ("NSDB", "Query was not a statement returning rows.")
    );
    // A driver error is kept, not replaced.
    assert!(h.select("bogus").is_none());
    assert!(!h.exception().1.starts_with("Query was not"));
    h.release();
    assert_eq!(p.outstanding(), 0);
    p.bounce();
    assert_eq!(p.connected(), 0);
    let h = p.get_handles(1, None).expect("again").pop().expect("one");
    assert!(h.connected());
    assert_eq!(h.exception(), ("", ""), "exception cleared on put");
    drop(h);
    assert_eq!(p.connected(), 1);
}

#[test]
fn row_helpers_set_nsint_exceptions() {
    let p = pool(1);
    let mut h = p.get_handles(1, None).expect("h").pop().expect("one");
    assert!(h.dml("create table t (a)"));
    assert!(h.dml("insert into t values (1), (2)"));
    assert!(h.one_row("select a from t where a = 9").is_none());
    assert_eq!(
        h.exception(),
        (NS_SQLERRORCODE, "Query did not return a row.")
    );
    assert!(h.zero_or_one_row("select a from t").is_none());
    assert_eq!(
        h.exception(),
        (NS_SQLERRORCODE, "Query returned more than one row.")
    );
    let (cols, row) = h.one_row("select a from t where a = 2").expect("row");
    assert_eq!(
        (cols, row),
        (vec!["a".to_owned()], vec![Some("2".to_owned())])
    );
    assert_eq!(
        h.zero_or_one_row("select a from t where a = 9"),
        Some((vec!["a".to_owned()], None))
    );
    assert!(h.reset());
    h.disconnect();
    assert!(!h.connected() && !h.flush() && !h.reset());
    assert!(h.bind_row().is_none() && h.next_row().is_err());
    assert!(h.select("select 1").is_none());
    assert_eq!(
        h.exception(),
        (NS_SQLERRORCODE, "Query returned more than one row.")
    );
    drop(h);
    // The disconnected handle is reconnected by the next get.
    let h = p.get_handles(1, None).expect("h").pop().expect("one");
    assert!(h.connected());
}

#[test]
fn interpret_sql_file_errors() {
    let dir = tempfile::tempdir().expect("dir");
    let p = pool(1);
    let mut h = p.get_handles(1, None).expect("h").pop().expect("one");
    assert!(!h.interpret_sql_file(&dir.path().join("none.sql").display().to_string()));
    assert_eq!(h.exception(), (NS_SQLERRORCODE, "Could not read file"));
    let f = dir.path().join("a.sql");
    std::fs::write(&f, "create table t (a);\ninsert into t values (1);;\n\n").expect("w");
    assert!(h.interpret_sql_file(&f.display().to_string()));
    std::fs::write(&f, "insert into t values (2);\ninsert into t values (3)").expect("w");
    assert!(!h.interpret_sql_file(&f.display().to_string()));
    assert_eq!(
        h.exception(),
        (NS_SQLERRORCODE, "File ends with unterminated SQL")
    );
    std::fs::write(&f, "bogus;\ninsert into t values (4);").expect("w");
    assert!(!h.interpret_sql_file(&f.display().to_string()));
    assert_ne!(h.exception().0, NS_SQLERRORCODE);
    let (_, row) = h.one_row("select count(*) from t").expect("count");
    assert_eq!(row, vec![Some("2".to_owned())], "4 was never inserted");
}

#[test]
fn a_thread_may_not_nest_gethandle() {
    let p = pool(2);
    let h = p.get_handles(1, None).expect("first");
    // The same thread is refused at once, even with a free handle.
    assert_eq!(
        p.get_handles(1, None).err(),
        Some(GetError::Failed(
            "dbinit: db handle limit exceeded: thread already owns 1 handle from pool 'p'".into()
        ))
    );
    // Another thread is not.
    let p2 = Arc::clone(&p);
    let other =
        std::thread::spawn(move || p2.get_handles(1, Some(Duration::ZERO)).map(|v| v.len()));
    assert_eq!(other.join().expect("join"), Ok(1));
    drop(h);
    let two = p.get_handles(2, None).expect("two");
    assert!(matches!(
        p.get_handles(1, None),
        Err(GetError::Failed(m)) if m.contains("owns 2 handles")
    ));
    drop(two);
    assert_eq!(p.get_handles(1, None).map(|v| v.len()), Ok(1));
}

#[test]
fn timeouts_and_open_failures() {
    let p = pool(1);
    let h = p.get_handles(1, None).expect("first");
    let p2 = Arc::clone(&p);
    let t = std::thread::spawn(move || {
        let start = Instant::now();
        let r = p2.get_handles(1, Some(Duration::from_millis(50))).err();
        (r, start.elapsed())
    });
    let (r, took) = t.join().expect("join");
    assert_eq!(r, Some(GetError::Timeout));
    assert!(took >= Duration::from_millis(50));
    // Without a timeout the waiter gets the handle when it is returned.
    let p2 = Arc::clone(&p);
    let t = std::thread::spawn(move || p2.get_handles(1, None).map(|v| v.len()));
    std::thread::sleep(Duration::from_millis(50));
    drop(h);
    assert_eq!(t.join().expect("join"), Ok(1));

    let bad = pool_with(1, |c| {
        c.driver = test_driver(Duration::ZERO);
        c.datasource = "fail".into();
    });
    assert!(matches!(bad.get_handles(1, None), Err(GetError::Failed(_))));
    assert_eq!(bad.outstanding(), 0, "handles returned after a failed open");
    // The failure left no handle count behind.
    assert!(
        matches!(bad.get_handles(1, None), Err(GetError::Failed(m)) if m.contains("failed to open"))
    );
    assert_eq!(pool(0).nhandles, 2);
    assert_eq!(pool(-3).nhandles, 2);
}

#[test]
fn multi_handle_waiter_is_served_first() {
    let p = pool(2);
    let held = p.get_handles(1, None).expect("held");
    // B wants both handles: it takes the free one and waits for the other.
    let pb = Arc::clone(&p);
    let b = std::thread::spawn(move || {
        pb.get_handles(2, Some(Duration::from_secs(5)))
            .map(|v| v.len())
    });
    std::thread::sleep(Duration::from_millis(100));
    // C queues behind B instead of taking a returned handle.
    let pc = Arc::clone(&p);
    let c = std::thread::spawn(move || {
        pc.get_handles(1, Some(Duration::from_millis(200)))
            .map(|v| v.len())
    });
    assert_eq!(c.join().expect("c"), Err(GetError::Timeout));
    drop(held);
    assert_eq!(b.join().expect("b"), Ok(2));
}

#[test]
fn handles_on_different_threads_run_concurrently() {
    let delay = Duration::from_millis(300);
    let p = pool_with(2, |c| c.driver = test_driver(delay));
    let start = Instant::now();
    let ts: Vec<_> = (0..2)
        .map(|_| {
            let p = Arc::clone(&p);
            std::thread::spawn(move || {
                let mut h = p.get_handles(1, None).expect("h").pop().expect("one");
                h.exec("select 1").is_ok()
            })
        })
        .collect();
    for t in ts {
        assert!(t.join().expect("join"));
    }
    // Each thread connects (no delay) then runs one slow statement.
    assert!(start.elapsed() < delay * 2, "{:?}", start.elapsed());
}

#[test]
fn idle_and_old_handles_are_closed() {
    let drv = test_driver(Duration::ZERO);
    let d2: Arc<dyn DbDriver> = Arc::clone(&drv) as Arc<dyn DbDriver>;
    let p = pool_with(1, |c| {
        c.driver = d2;
        c.maxidle = 10;
        c.maxopen = 100;
        c.verbose = true;
    });
    drop(p.get_handles(1, None).expect("h"));
    assert_eq!((drv.opens.load(Ordering::Relaxed), p.connected()), (1, 1));
    p.check(now() + 5);
    assert_eq!(p.connected(), 1, "not idle yet");
    p.check(now() + 20);
    assert_eq!(p.connected(), 0, "idle handle closed");
    drop(p.get_handles(1, None).expect("h"));
    assert_eq!(drv.opens.load(Ordering::Relaxed), 2);
    // maxopen applies even to a handle in steady use.
    let mut h = p.get_handles(1, None).expect("h").pop().expect("one");
    h.s().otime -= 200;
    drop(h);
    assert_eq!(p.connected(), 0, "old handle closed on return");
    // 0 disables both limits.
    let never = pool_with(1, |c| {
        c.maxidle = 0;
        c.maxopen = 0;
    });
    drop(never.get_handles(1, None).expect("h"));
    never.check(now() + 1_000_000);
    assert_eq!(never.connected(), 1);
    assert_eq!(pool_with(1, |c| c.maxidle = -1).maxidle, 600);
    assert_eq!(pool_with(1, |c| c.maxopen = -1).maxopen, 3600);
}

#[test]
fn bounce_closes_in_use_handles_once() {
    let drv = test_driver(Duration::ZERO);
    let d2: Arc<dyn DbDriver> = Arc::clone(&drv) as Arc<dyn DbDriver>;
    let p = pool_with(1, |c| c.driver = d2);
    let h = p.get_handles(1, None).expect("h");
    p.bounce();
    drop(h);
    assert_eq!(p.connected(), 0, "in-use handle closed when returned");
    drop(p.get_handles(1, None).expect("h"));
    assert_eq!(p.connected(), 1, "reconnected handle is kept");
    drop(p.get_handles(1, None).expect("h"));
    assert_eq!(drv.opens.load(Ordering::Relaxed), 2);
}

#[test]
fn config_defaults() {
    let mut c = Config::new();
    for (k, v) in [("a", "A"), ("b", "B"), ("c", "C")] {
        c.put("ns/db/pools", k, v);
    }
    for p in ["a", "b", "c"] {
        let path = format!("ns/db/pool/{p}");
        c.put(&path, "driver", "sqlite");
        c.put(&path, "datasource", ":memory:");
    }
    c.put("ns/db/pool/a", "connections", "0");
    c.put("ns/db/pool/b", "connections", "-4");
    c.put("ns/db/pool/b", "MaxIdle", "-1");
    c.put("ns/db/pool/b", "MaxOpen", "60");
    c.put("ns/db/pool/c", "connections", "5");
    c.put("ns/db/pool/c", "maxidle", "0");
    c.put("ns/db/pool/c", "checkinterval", "30");
    c.put("ns/server/s1/db", "pools", "a, c");
    c.put("ns/server/s1/db", "defaultpool", "b");
    c.put("ns/server/s2/db", "defaultpool", "nosuch");
    let (log, logs) = capture();
    let st = DbState::build(&c, &log);
    let get = |n: &str| Arc::clone(st.get_pool(n).expect("pool"));
    assert_eq!(
        (get("a").nhandles, get("b").nhandles, get("c").nhandles),
        (2, 2, 5)
    );
    assert_eq!(
        (get("a").maxidle, get("a").maxopen, get("a").checkinterval),
        (600, 3600, 600)
    );
    assert_eq!((get("b").maxidle, get("b").maxopen), (600, 60));
    assert_eq!((get("c").maxidle, get("c").checkinterval), (0, 30));
    st.configure_server(&c, "s1", &*log);
    st.configure_server(&c, "s2", &*log);
    assert_eq!(st.server_pools("s1"), vec!["a", "c"]);
    // The default pool need only exist (`NsDbInitServer`).
    assert_eq!(st.default_pool("s1").as_deref(), Some("b"));
    assert!(!st.allowable("s1", "b") && st.allowable("s1", "a"));
    assert_eq!(st.default_pool("s2"), None);
    assert_eq!(st.server_pools("s3"), Vec::<String>::new());
    assert!(lock(&logs)
        .iter()
        .any(|m| m == "Error: dbinit: no such default pool 'nosuch'"));
}

#[test]
fn handle_ids_are_per_table_and_hex() {
    let p = pool(12);
    let mut d = InterpData::default();
    let hs = p.get_handles(11, None).expect("handles");
    let ids: Vec<String> = hs.into_iter().map(|h| d.enter(h)).collect();
    assert_eq!(ids[0], "nsdb0");
    assert_eq!(ids[10], "nsdba");
    // `EnterHandle` starts at the table size and skips ids in use.
    let h = d.dbs.remove("nsdb3").expect("h");
    assert_eq!(d.enter(h), "nsdbb");
    drop(d);
    assert_eq!(
        p.outstanding(),
        0,
        "dropping the table releases its handles"
    );
}

#[test]
fn registry_never_matches_a_dropped_key() {
    let reg: Registry<u64, u64> = Registry::new();
    for i in 0..20u64 {
        // A fresh key may land where an earlier, dropped key lived.
        let k = Arc::new(i);
        let (v, created) = reg.get_or_insert(&k, || i);
        assert_eq!((*v, created), (i, true));
        assert_eq!(*reg.get_or_insert(&k, || 99).0, i, "same key, same value");
        drop(k);
    }
    assert_eq!(reg.len(), 1, "dead entries are pruned");
    let k = Arc::new(7);
    let (v, _) = reg.get_or_insert(&k, || 7);
    reg.forget(&Arc::downgrade(&v));
    assert_eq!(reg.len(), 0);
}

#[test]
fn sql_logging_follows_ns_db_log_sql() {
    let run = |verbose: bool, log_errors: bool| {
        let (log, logs) = capture();
        let mut c = PoolConfig::new("p", Arc::new(SqliteDriver), ":memory:");
        c.nhandles = 1;
        c.verbose = verbose;
        c.log_errors = log_errors;
        let p = Arc::new(Pool::new(c, log));
        let mut h = p.get_handles(1, None).expect("h").pop().expect("one");
        let _ = h.exec("select 1");
        let _ = h.exec("bogus");
        drop(h);
        let v = lock(&logs).clone();
        v.into_iter()
            .filter(|l| l.contains("dbinit: sql") || l.contains("dbinit: error"))
            .collect::<Vec<_>>()
    };
    assert_eq!(run(false, false), Vec::<String>::new());
    let errs = run(false, true);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(
        errs[0].starts_with("Error: dbinit: error(:memory:,") && errs[0].ends_with("): 'bogus'")
    );
    // A verbose handle logs statements and errors, with the datasource.
    let all = run(true, false);
    assert_eq!(all.len(), 2, "{all:?}");
    assert_eq!(all[0], "Notice: dbinit: sql(:memory:): 'select 1'");
    assert!(all[1].starts_with("Error: dbinit: error(:memory:,"));
}

#[test]
fn connection_defaults() {
    let mut c = SqliteDriver.open(":memory:", None, None).expect("open");
    assert!(c.bind_row().is_none() && c.db_type().is_none() && c.as_any_mut().is_none());
    assert!(!c.supports_sp());
    c.exec("select 1 as a").expect("sel");
    assert_eq!(c.next_row_n(1).expect("row"), Some(vec![Some("1".into())]));
    assert!(c.try_flush().is_ok() && c.cancel().is_ok());
    let no = "stored procedures are not supported";
    assert_eq!(c.sp_start("p").expect_err("sp").message, no);
    assert_eq!(c.sp_setparam("a").expect_err("sp").message, no);
    assert_eq!(c.sp_exec().expect_err("sp").message, no);
    assert_eq!(c.sp_returncode().expect_err("sp").message, no);
    assert_eq!(c.sp_getparams().expect_err("sp").message, no);
}

/// A connection that dies after its first statement, as an external
/// driver's does when the daemon goes away.
struct Dying {
    alive: bool,
}

impl DbConnection for Dying {
    fn exec(&mut self, _: &str) -> Result<ExecResult, DbError> {
        self.alive = false;
        Err(DbError {
            code: "GONE".into(),
            message: "daemon went away".into(),
        })
    }
    fn next_row(&mut self) -> Result<Option<Vec<Option<String>>>, DbError> {
        Ok(None)
    }
    fn flush(&mut self) {}
    fn connected(&self) -> bool {
        self.alive
    }
    fn db_type(&self) -> Option<String> {
        Some("Dying".into())
    }
}

struct DyingDriver;

impl DbDriver for DyingDriver {
    fn name(&self) -> &'static str {
        "dying"
    }
    fn db_type(&self) -> &'static str {
        "Static"
    }
    fn open(
        &self,
        _: &str,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<Box<dyn DbConnection>, DbError> {
        Ok(Box::new(Dying { alive: true }))
    }
}

#[test]
fn a_connection_the_driver_drops_disconnects_the_handle() {
    let c = PoolConfig::new("d", Arc::new(DyingDriver), "x");
    let p = Arc::new(Pool::new(c, Arc::new(|_, _| {})));
    let mut h = p.get_handles(1, None).expect("get").pop().expect("handle");
    assert!(h.connected());
    assert_eq!(h.db_type(), "Dying");
    assert!(h.with_connection(|c| c.connected()).expect("connected"));
    assert!(h.exec("select 1").is_err());
    assert_eq!(h.exception(), ("GONE", "daemon went away"));
    assert!(!h.connected(), "the dead connection was dropped");
    assert_eq!(h.db_type(), "");
    assert!(h.with_connection(|_| ()).is_none());
    assert!(!h.cancel() && !h.supports_sp());
    drop(h);
    // The next checkout reconnects.
    let h = p.get_handles(1, None).expect("get").pop().expect("handle");
    assert!(h.connected());
}

#[test]
fn driver_modules_load_from_their_config() {
    let made: Arc<Mutex<Vec<String>>> = Arc::default();
    let m = Arc::clone(&made);
    register_driver(
        "nstestext",
        Arc::new(move |dc: &DriverConfig<'_>| {
            lock(&m).push(format!(
                "{} {} {:?} {:?}",
                dc.name,
                dc.path,
                dc.config.get(&dc.path, "opt"),
                dc.home
            ));
            match dc.name {
                "good" => Ok(Arc::new(DyingDriver) as Arc<dyn DbDriver>),
                "half" => Err(Some(Arc::new(DyingDriver) as Arc<dyn DbDriver>)),
                _ => Err(None),
            }
        }),
    );
    let mut c = Config::new();
    for n in ["good", "half", "bad"] {
        c.put("ns/db/drivers", n, "nstestext.so");
        c.put("ns/db/pools", &format!("p{n}"), "");
        c.put(&format!("ns/db/pool/p{n}"), "driver", n);
        c.put(&format!("ns/db/pool/p{n}"), "datasource", "x");
    }
    c.put("ns/db/driver/good", "opt", "1");
    let (log, logs) = capture();
    let st = DbState::build_in(&c, &log, Some(std::path::Path::new("/home")));
    assert!(st.get_pool("pgood").is_some() && st.get_pool("phalf").is_some());
    assert!(st.get_pool("pbad").is_none());
    assert_eq!(
        lock(&made).clone(),
        vec![
            "good ns/db/driver/good Some(\"1\") Some(\"/home\")",
            "half ns/db/driver/half None Some(\"/home\")",
            "bad ns/db/driver/bad None Some(\"/home\")",
        ]
    );
    let l = lock(&logs).clone();
    for m in [
        "Error: dbdrv: failed to load driver 'half'",
        "Error: dbdrv: failed to load driver 'bad'",
        "Error: dbinit: no such driver 'bad' for pool 'pbad'",
    ] {
        assert!(l.contains(&m.to_owned()), "{m} {l:?}");
    }
    assert!(driver_factory("nosuch").is_none());
}
