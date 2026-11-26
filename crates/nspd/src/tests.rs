#![allow(clippy::unwrap_used)]

use super::*;
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::time::Duration;

type Logs = Arc<Mutex<Vec<String>>>;

fn opts() -> (Options, Logs, tempfile::TempDir) {
    let logs: Logs = Arc::default();
    let l = Arc::clone(&logs);
    let tmp = tempfile::tempdir().unwrap();
    let o = Options {
        log: Arc::new(move |lv, m| l.lock().unwrap().push(format!("{lv:?}: {m}"))),
        tmpdir: tmp.path().to_path_buf(),
        max_request: CMDMAX,
    };
    (o, logs, tmp)
}

struct Client {
    w: UnixStream,
    r: BufReader<UnixStream>,
    t: Option<std::thread::JoinHandle<()>>,
}

impl Client {
    fn start<D: PdDb + Send + 'static>(db: D, o: &Options) -> Self {
        let (a, b) = UnixStream::pair().unwrap();
        a.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let o = o.clone();
        let t = std::thread::spawn(move || {
            let w = b.try_clone().unwrap();
            main_loop(b, w, || Some(db), &o);
        });
        Self {
            w: a.try_clone().unwrap(),
            r: BufReader::new(a),
            t: Some(t),
        }
    }

    fn send(&mut self, m: &str, arg: &[u8]) {
        let mut v = format!("{m}\n{}\n", arg.len()).into_bytes();
        v.extend_from_slice(arg);
        self.w.write_all(&v).unwrap();
    }

    fn line(&mut self) -> String {
        let mut s = String::new();
        self.r.read_line(&mut s).unwrap();
        assert!(s.ends_with('\n'), "{s:?}");
        s.pop();
        s
    }

    fn ask(&mut self, m: &str, arg: &str) -> String {
        self.send(m, arg.as_bytes());
        self.line()
    }

    fn list(&mut self) -> Vec<Option<String>> {
        let mut out = Vec::new();
        loop {
            let n: i64 = self.line().parse().unwrap();
            if n == -1 {
                return out;
            }
            let mut b = vec![0; usize::try_from(n).unwrap()];
            self.r.read_exact(&mut b).unwrap();
            out.push((n > 0).then(|| String::from_utf8(b).unwrap()));
        }
    }

    /// Wait for the session to end (it closed its end).
    fn finish(mut self) {
        drop(self.w.shutdown(std::net::Shutdown::Write));
        if let Some(t) = self.t.take() {
            t.join().unwrap();
        }
    }

    fn eof(&mut self) -> bool {
        let mut s = String::new();
        self.r.read_line(&mut s).unwrap() == 0
    }
}

fn s(v: &[&str]) -> Vec<Option<String>> {
    v.iter().map(|x| Some((*x).to_owned())).collect()
}

#[test]
fn sqlite_session() {
    let (o, logs, dir) = opts();
    let db = dir.path().join("t.db");
    let mut c = Client::start(DriverDb::sqlite(), &o);
    assert_eq!(c.ask("ping", ""), "ok");
    assert!(c
        .ask("exec", "select 1")
        .starts_with("NSDB#database is not open"));
    assert_eq!(c.ask("open", &format!("{}#u#p#param", db.display())), "ok");
    assert_eq!(c.ask("identify", ""), "SQLite Proxy Daemon v4.5");
    assert_eq!(c.ask("gettypes", ""), "integer real text blob null");
    assert_eq!(c.ask("exec", "create table t (a integer, b text)"), "ok");
    assert_eq!(c.line(), "exec_dml");
    assert_eq!(
        c.ask("exec", "insert into t values (1, 'x'), (2, null), (3, 'z')"),
        "ok"
    );
    assert_eq!(c.line(), "exec_dml");
    assert_eq!(c.ask("resultrows", ""), "3");
    assert_eq!(c.ask("resultid", ""), "");
    assert_eq!(c.ask("bindrow", ""), "NSDB#no rows pending");
    assert_eq!(c.ask("getrow", "2"), "ok");
    assert_eq!(c.list(), Vec::<Option<String>>::new());
    assert_eq!(c.ask("exec", "select a, b from t order by a"), "ok");
    assert_eq!(c.line(), "exec_rows");
    assert_eq!(c.ask("bindrow", ""), "ok");
    assert_eq!(c.list(), s(&["a", "b"]));
    assert_eq!(c.ask("getrow", "2"), "ok");
    assert_eq!(c.list(), s(&["1", "x"]));
    assert_eq!(c.ask("getrow", "2"), "ok");
    assert_eq!(c.list(), vec![Some("2".to_owned()), None]);
    assert_eq!(c.ask("flush", ""), "ok");
    assert_eq!(c.ask("getrow", "2"), "ok");
    assert_eq!(c.list(), Vec::<Option<String>>::new());
    // setmaxrows limits a query's rows.
    assert_eq!(c.ask("setmaxrows", "1"), "ok");
    assert_eq!(c.ask("exec", "select a from t"), "ok");
    assert_eq!(c.line(), "exec_rows");
    assert_eq!(c.ask("getrow", "1"), "ok");
    assert_eq!(c.list().len(), 1);
    assert_eq!(c.ask("getrow", "1"), "ok");
    assert_eq!(c.list(), Vec::<Option<String>>::new());
    assert_eq!(c.ask("setmaxrows", "0"), "ok");
    assert_eq!(c.ask("exec", "select a from t"), "ok");
    assert_eq!(c.line(), "exec_rows");
    assert_eq!(c.ask("getrow", "1"), "ok");
    assert_eq!(c.list().len(), 1);
    assert_eq!(c.ask("cancel", ""), "ok");
    assert_eq!(c.ask("resethandle", ""), "ok");
    let e = c.ask("exec", "select * from nosuch");
    assert!(e.contains("#no such table: nosuch"), "{e}");
    // Exceptions never carry newlines.
    let e = c.ask("exec", "select\n* from\nnosuch2");
    assert!(e.contains("no such table"), "{e}");
    // Stored procedures are not supported by the SQLite daemon.
    assert_eq!(
        c.ask("spstart", "p"),
        "NSPD#stored procedures are not supported"
    );
    assert_eq!(
        c.ask("spsetparam", "@a int in 1"),
        "NSPD#stored procedures are not supported"
    );
    assert_eq!(
        c.ask("spexec", ""),
        "NSPD#stored procedures are not supported"
    );
    assert_eq!(c.ask("spreturncode", ""), "");
    assert_eq!(
        c.ask("spgetparams", ""),
        "NSPD#stored procedures are not supported"
    );
    assert_eq!(c.ask("close", ""), "ok");
    assert!(c.eof(), "close ends the session");
    c.finish();
    let l = logs.lock().unwrap().clone();
    assert!(l.contains(&"Notice: nspd: starting".to_owned()), "{l:?}");
}

#[test]
fn opening_a_bad_database_is_an_exception() {
    let (o, _, dir) = opts();
    let mut c = Client::start(DriverDb::sqlite(), &o);
    let bad = dir.path().join("no/such/dir/x.db");
    let r = c.ask("open", &format!("{}###", bad.display()));
    assert!(r.starts_with("CannotOpen#") || r.contains('#'), "{r}");
    assert_eq!(c.ask("close", ""), "ok");
    c.finish();
}

#[test]
fn unknown_and_unsupported_messages_get_no_reply() {
    let (o, logs, _d) = opts();
    let mut c = Client::start(DriverDb::sqlite(), &o);
    c.send("bogus", b"");
    c.send("gettableinfo", b"t");
    c.send("tablelist", b"t");
    c.send("bestrowid", b"t");
    assert_eq!(c.ask("PING", ""), "ok", "names ignore case");
    c.finish();
    let l = logs.lock().unwrap().clone();
    for m in [
        "Error: nspd: unknown dispatch message 'bogus'",
        "Error: nspd: unsupported dispatch command: GetTableInfo",
        "Error: nspd: unsupported dispatch command: TableList",
        "Error: nspd: unsupported dispatch command: BestRowId",
    ] {
        assert!(l.contains(&m.to_owned()), "{m} {l:?}");
    }
}

#[test]
fn malformed_requests_end_the_session() {
    let (mut o, logs, _d) = opts();
    o.max_request = 64;
    // An argument too large for the request buffer.
    let mut c = Client::start(DriverDb::sqlite(), &o);
    c.w.write_all(b"exec\n100\n").unwrap();
    assert!(c.eof());
    c.finish();
    // A negative length.
    let mut c = Client::start(DriverDb::sqlite(), &o);
    c.w.write_all(b"exec\n-1\n").unwrap();
    assert!(c.eof());
    c.finish();
    // A name line that never ends.
    let mut c = Client::start(DriverDb::sqlite(), &o);
    c.w.write_all(&[b'x'; 100]).unwrap();
    assert!(c.eof());
    c.finish();
    // A length line cut short by end of input.
    let c = Client::start(DriverDb::sqlite(), &o);
    let mut w = c.w.try_clone().unwrap();
    w.write_all(b"exec\n12").unwrap();
    c.finish();
    // An argument cut short.
    let c = Client::start(DriverDb::sqlite(), &o);
    let mut w = c.w.try_clone().unwrap();
    w.write_all(b"exec\n12\nsel").unwrap();
    c.finish();
    let l = logs.lock().unwrap().clone();
    assert!(
        l.iter()
            .any(|m| m.contains("arglen of 100 for exec command is greater than configured max 64")),
        "{l:?}"
    );
    assert!(
        l.iter().any(|m| m.contains("arglen of -1 for exec")),
        "{l:?}"
    );
    assert!(
        l.iter()
            .any(|m| m.contains("no newline terminator for command name")),
        "{l:?}"
    );
    assert!(
        l.iter()
            .any(|m| m.contains("no newline terminator for arglen of command: 'exec'")),
        "{l:?}"
    );
}

#[test]
fn init_failure_ends_at_once() {
    let (o, logs, _d) = opts();
    let (a, b) = UnixStream::pair().unwrap();
    main_loop(&a, &b, || None::<DriverDb>, &o);
    assert!(logs
        .lock()
        .unwrap()
        .contains(&"Error: nspd: initialization failure".to_owned()));
}

#[test]
fn remote_files() {
    let (o, logs, dir) = opts();
    let mut c = Client::start(DriverDb::sqlite(), &o);
    assert_eq!(c.ask("createtmpfile", ""), "ok");
    let tmp = c.line();
    assert!(
        tmp.starts_with(&dir.path().join("pd").display().to_string()),
        "{tmp}"
    );
    assert_eq!(std::fs::read(&tmp).unwrap(), b"");
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&tmp).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644);
    }
    // Write two chunks through a read-write descriptor.
    assert_eq!(c.ask("openfile", &format!("{}#{tmp}", libc::O_RDWR)), "ok");
    let fd = c.line();
    c.send("writefile", format!("{fd}#0#5#he#lo").as_bytes());
    assert_eq!(c.line(), "ok");
    c.send("writefile", format!("{fd}#5#3#\0\n!").as_bytes());
    assert_eq!(c.line(), "ok");
    assert_eq!(c.ask("closefile", &fd), "ok");
    assert_eq!(std::fs::read(&tmp).unwrap(), b"he#lo\0\n!");
    // Read it back in pieces.
    assert_eq!(
        c.ask("openfile", &format!("{}#{tmp}", libc::O_RDONLY)),
        "ok"
    );
    let fd = c.line();
    assert_eq!(c.ask("readfile", &format!("{fd}#0#3")), "ok");
    assert_eq!(c.list(), s(&["he#"]));
    assert_eq!(c.ask("readfile", &format!("{fd}#3#100000")), "ok");
    assert_eq!(c.list(), s(&["lo\0\n!"]));
    assert_eq!(c.ask("readfile", &format!("{fd}#8#10")), "ok");
    assert_eq!(c.list(), vec![None]);
    // Writing to a read-only descriptor fails.
    c.send("writefile", format!("{fd}#0#1#x").as_bytes());
    assert!(c
        .line()
        .starts_with(&format!("Write error to fd {fd} (1 bytes): ")));
    assert_eq!(c.ask("closefile", &fd), "ok");
    // Errors.
    assert_eq!(
        c.ask("closefile", &fd),
        format!("Close error on file descriptor {fd}: Bad file descriptor")
    );
    assert_eq!(
        c.ask("readfile", "99#0#1"),
        "Read error on fd 99 (1 bytes): Bad file descriptor"
    );
    assert_eq!(c.ask("readfile", "x"), "Error parsing read parameters: x");
    c.send("writefile", b"1#2");
    assert_eq!(c.line(), "Error parsing write parameters: 1#2");
    c.send("writefile", b"99#0#4#ab");
    assert_eq!(
        c.line(),
        "Write error to fd 99 (4 bytes): Bad file descriptor"
    );
    assert_eq!(
        c.ask("openfile", "nonsense"),
        "Error parsing open parameters: nonsense"
    );
    assert_eq!(c.ask("openfile", "0#"), "Error parsing open parameters: 0#");
    let missing = dir.path().join("missing");
    assert_eq!(
        c.ask("openfile", &format!("0#{} trailing", missing.display())),
        format!(
            "Can't open file {} (oflags=0): No such file or directory",
            missing.display()
        )
    );
    assert_eq!(c.ask("deletefile", &tmp), "ok");
    assert_eq!(
        c.ask("deletefile", &tmp),
        format!("Can't delete file {tmp}: No such file or directory")
    );
    c.finish();
    let l = logs.lock().unwrap().clone();
    assert!(
        l.iter()
            .any(|m| m.starts_with("Error: nspd: failed closing file 'Close error")),
        "{l:?}"
    );
    assert!(
        l.iter()
            .any(|m| m.starts_with("Error: failed reading file: 'Read error")),
        "{l:?}"
    );
    assert!(
        l.iter()
            .any(|m| m.starts_with("Error: nspd: failed deleting file")),
        "{l:?}"
    );
}

#[test]
fn tmpfile_in_a_missing_directory() {
    let (mut o, logs, dir) = opts();
    o.tmpdir = dir.path().join("gone/");
    let mut c = Client::start(DriverDb::sqlite(), &o);
    let r = c.ask("createtmpfile", "");
    assert!(
        r.starts_with("open/create of ") && r.contains("pdXXXXXX failed"),
        "{r}"
    );
    assert_eq!(c.ask("ping", ""), "ok");
    c.finish();
    assert!(logs
        .lock()
        .unwrap()
        .iter()
        .any(|m| m.contains("nspd: failed to open/create temp file")));
}

#[test]
fn tracing() {
    let (o, logs, dir) = opts();
    let trace = dir.path().join("trace.log");
    let mut c = Client::start(DriverDb::sqlite(), &o);
    assert_eq!(c.ask("traceon", &trace.display().to_string()), "ok");
    assert_eq!(c.ask("ping", ""), "ok");
    assert_eq!(c.ask("open", ":memory:#u#secret#"), "ok");
    assert_eq!(
        c.ask("openfile", "bad"),
        "Error parsing open parameters: bad"
    );
    assert_eq!(c.ask("traceoff", ""), "ok");
    assert_eq!(c.ask("ping", ""), "ok");
    let bad = dir.path().join("no/dir/trace");
    assert_eq!(
        c.ask("traceon", &bad.display().to_string()),
        format!(
            "Unable to open trace file: {} (No such file or directory)",
            bad.display()
        )
    );
    assert_eq!(c.ask("ping", ""), "ok");
    c.finish();
    let t = std::fs::read_to_string(&trace).unwrap();
    assert_eq!(
        t,
        "nspd: responding to ping\nNs_PdParseOpenArgs: |:memory:#u#secret#|\nPdOpenFile:\n"
    );
    let l = logs.lock().unwrap().clone();
    assert!(l.contains(&"Notice: PdTraceOn: traceon:".to_owned()));
    assert!(l.contains(&"Notice: nspd: traceoff".to_owned()));
    assert!(l
        .iter()
        .any(|m| m.starts_with("Error: Unable to open trace file")));
}

/// A database with stored procedures, to check their replies.
#[derive(Default)]
struct SpDb {
    params: Vec<String>,
}

impl PdDb for SpDb {
    fn open(&mut self, _: &OpenArgs) -> Result<(), PdError> {
        Err(PdError::Silent)
    }
    fn close(&mut self) -> Result<(), PdError> {
        Err(PdError::Message("cannot\nclose".into()))
    }
    fn identify(&mut self) -> String {
        "Sp Proxy Daemon v1\n".into()
    }
    fn get_types(&mut self) -> String {
        String::new()
    }
    fn exec(&mut self, _: &str) -> Result<PdExec, PdError> {
        Ok(PdExec::Rows)
    }
    fn bind_row(&mut self) -> Result<Vec<String>, PdError> {
        Ok(vec![String::new()])
    }
    fn get_row(&mut self, n: i64) -> Result<Option<Vec<Option<String>>>, PdError> {
        Ok(Some(vec![Some(n.to_string())]))
    }
    fn flush(&mut self) -> Result<(), PdError> {
        Err(PdError::exception("X".repeat(40), "y".repeat(5000)))
    }
    fn sp_start(&mut self, p: &str) -> Result<(), PdError> {
        self.params.push(p.to_owned());
        Ok(())
    }
    fn sp_set_param(&mut self, a: &str) -> Result<(), PdError> {
        self.params.push(a.to_owned());
        Ok(())
    }
    fn sp_exec(&mut self) -> Result<PdExec, PdError> {
        Ok(PdExec::Dml)
    }
    fn sp_return_code(&mut self) -> String {
        "0".into()
    }
    fn sp_get_params(&mut self) -> Result<Vec<(String, Option<String>)>, PdError> {
        Ok(self.params.iter().map(|p| (p.clone(), None)).collect())
    }
}

#[test]
fn custom_database_replies() {
    let (o, _, _d) = opts();
    let mut c = Client::start(SpDb::default(), &o);
    assert_eq!(c.ask("open", "x"), "silentError");
    assert_eq!(c.ask("identify", ""), "Sp Proxy Daemon v1 ");
    assert_eq!(c.ask("exec", "x"), "ok");
    assert_eq!(c.line(), "exec_rows");
    assert_eq!(c.ask("bindrow", ""), "ok");
    assert_eq!(c.list(), vec![None]);
    assert_eq!(c.ask("getrow", "7"), "ok");
    assert_eq!(c.list(), s(&["7"]));
    let f = c.ask("flush", "");
    assert_eq!(f, format!("{}#{}", "X".repeat(32), "y".repeat(4096)));
    assert_eq!(c.ask("cancel", ""), f, "cancel defaults to flush");
    assert_eq!(c.ask("setmaxrows", "3"), "ok");
    assert_eq!(c.ask("resethandle", ""), "ok");
    assert_eq!(c.ask("resultid", ""), "");
    assert_eq!(c.ask("resultrows", ""), "");
    assert_eq!(c.ask("spstart", "proc"), "ok");
    assert_eq!(c.ask("spsetparam", "@a int out 1"), "ok");
    assert_eq!(c.ask("spexec", ""), "ok");
    assert_eq!(c.line(), "exec_dml");
    assert_eq!(c.ask("spreturncode", ""), "0");
    assert_eq!(c.ask("spgetparams", ""), "ok");
    assert_eq!(c.list(), s(&["proc", "@a int out 1"]));
    assert_eq!(c.list(), vec![None, None]);
    assert_eq!(c.ask("close", ""), "cannot close");
    assert!(c.eof());
    c.finish();
}

#[test]
fn listener_serves_each_connection() {
    let (o, _, dir) = opts();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || serve(&l, || Some(DriverDb::sqlite()), &o));
    let db = dir.path().join("shared.db");
    for i in 0..2 {
        let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut w = s.try_clone().unwrap();
        let mut r = BufReader::new(s);
        let mut line = || {
            let mut x = String::new();
            r.read_line(&mut x).unwrap();
            x
        };
        let ds = format!("{}###", db.display());
        w.write_all(&msg::encode(Msg::Open, Some(ds.as_bytes())))
            .unwrap();
        assert_eq!(line(), "ok\n");
        let sql = format!("create table t{i} (a)");
        w.write_all(&msg::encode(Msg::Exec, Some(sql.as_bytes())))
            .unwrap();
        assert_eq!(line(), "ok\n");
        assert_eq!(line(), "exec_dml\n");
    }
}

#[test]
fn helpers() {
    assert_eq!(
        OpenArgs::parse("ds#u#p#a#b"),
        OpenArgs {
            datasource: Some("ds".into()),
            user: Some("u".into()),
            password: Some("p".into()),
            param: Some("a#b".into()),
        }
    );
    assert_eq!(OpenArgs::parse("ds"), OpenArgs::default());
    assert_eq!(
        OpenArgs::parse("ds#u"),
        OpenArgs {
            datasource: Some("ds".into()),
            ..OpenArgs::default()
        }
    );
    assert_eq!(
        OpenArgs::parse("#u#"),
        OpenArgs {
            user: Some("u".into()),
            ..OpenArgs::default()
        }
    );
    assert_eq!(OpenArgs::parse("###"), OpenArgs::default());
    assert_eq!(PdError::exception("C", "a\nb").line(), "C#a b");
    assert_eq!(PdError::Silent.line(), "silentError");
    assert_eq!(truncate("héllo", 2), "h");
    assert_eq!(parse_port("8199"), Some(8199));
    assert_eq!(parse_port("0"), None);
    assert_eq!(parse_port("65535"), None);
    assert_eq!(parse_port("x"), None);
    assert_eq!(
        strerror(&io::Error::from_raw_os_error(libc::ENOENT)),
        "No such file or directory"
    );
    assert_eq!(strerror(&io::Error::other("plain")), "plain");
    let o = Options::default();
    assert!(format!("{o:?}").contains("/var/tmp/"));
    (o.log)(Level::Notice, "ignored");
    assert!(format!("{:?}", DriverDb::sqlite()).contains("sqlite"));
}
