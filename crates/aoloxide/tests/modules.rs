//! Integration tests for the compiled-in modules and the `nsd` binary.

#![allow(clippy::unwrap_used)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;

use ns_script::ScriptEngine;
use std::time::{Duration, Instant};

use nsd::{Nsd, NsdOptions};

struct Srv {
    nsd: Nsd,
    _home: tempfile::TempDir,
    home: std::path::PathBuf,
    port: u16,
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.nsd.shutdown();
    }
}

fn boot(modules: &str, extra: &str, setup: impl FnOnce(&Path)) -> Srv {
    let home = tempfile::tempdir().unwrap();
    aoloxide::install_home(home.path(), true).unwrap();
    std::fs::create_dir_all(home.path().join("servers/s1/pages")).unwrap();
    setup(home.path());
    let cfg = format!(
        r#"
ns_section ns/parameters
ns_param home {home}
ns_section ns/servers
ns_param s1 "S1"
ns_section ns/server/s1/modules
ns_param nssock nssock.so
{modules}
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
    o.modules = aoloxide::modules();
    let nsd = Nsd::new(o).unwrap();
    nsd.start().unwrap();
    let port = nsd.drivers()[0].info.port();
    let p = home.path().to_path_buf();
    Srv {
        nsd,
        _home: home,
        home: p,
        port,
    }
}

fn request(port: u16, req: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

fn status(resp: &str) -> u16 {
    resp.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

#[test]
fn nslog_writes_combined_log() {
    let srv = boot(
        "ns_param nslog nslog.so",
        "ns_section ns/server/s1/module/nslog\nns_param file access.log\nns_param rolllog false",
        |h| std::fs::write(h.join("servers/s1/pages/x.txt"), "xyz").unwrap(),
    );
    let r = request(
        srv.port,
        "GET /x.txt?q=1 HTTP/1.0\r\nReferer: http://ref/\r\nUser-Agent: tester/1\r\n\r\n",
    );
    assert_eq!(status(&r), 200);
    let log = srv.home.join("servers/s1/modules/nslog/access.log");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut text = String::new();
    while Instant::now() < deadline {
        text = std::fs::read_to_string(&log).unwrap_or_default();
        if !text.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(text.starts_with("127.0.0.1 - - ["), "{text}");
    assert!(
        text.contains("\"GET /x.txt?q=1 HTTP/1.0\" 200 3 \"http://ref/\" \"tester/1\""),
        "{text}"
    );
}

#[test]
fn nsperm_basic_auth() {
    let srv = boot("ns_param nsperm nsperm.so", "", |h| {
        let dir = h.join("servers/s1/modules/nsperm");
        std::fs::create_dir_all(&dir).unwrap();
        let pass = ns_core::crypt::crypt("pw", "ab");
        std::fs::write(
            dir.join("passwd"),
            format!("admin:{pass}:::::\nguest::::::\n"),
        )
        .unwrap();
        std::fs::write(dir.join("group"), "staff:::admin\n").unwrap();
        std::fs::write(dir.join("perms"), "allowgroup inherit GET /private staff\n").unwrap();
        let p = h.join("servers/s1/pages/private");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("s.txt"), "secret").unwrap();
        std::fs::write(h.join("servers/s1/pages/open.txt"), "open").unwrap();
    });
    assert_eq!(
        status(&request(srv.port, "GET /open.txt HTTP/1.0\r\n\r\n")),
        200
    );
    let r = request(srv.port, "GET /private/s.txt HTTP/1.0\r\n\r\n");
    assert_eq!(status(&r), 401, "{r}");
    assert!(r.contains("WWW-Authenticate: Basic realm=\"s1\""));
    let good = ns_core::text::htuu_encode(b"admin:pw");
    let r = request(
        srv.port,
        &format!("GET /private/s.txt HTTP/1.0\r\nAuthorization: Basic {good}\r\n\r\n"),
    );
    assert_eq!(status(&r), 200, "{r} {:?}", srv.nsd.global.log.captured());
    assert!(r.ends_with("secret"));
    let bad = ns_core::text::htuu_encode(b"admin:nope");
    let r = request(
        srv.port,
        &format!("GET /private/s.txt HTTP/1.0\r\nAuthorization: Basic {bad}\r\n\r\n"),
    );
    assert_eq!(status(&r), 401);
    let guest = ns_core::text::htuu_encode(b"guest:");
    let r = request(
        srv.port,
        &format!("GET /private/s.txt HTTP/1.0\r\nAuthorization: Basic {guest}\r\n\r\n"),
    );
    assert_eq!(status(&r), 401, "guest is not in staff");
}

#[test]
fn nscgi_runs_scripts() {
    use std::os::unix::fs::PermissionsExt;
    let cgi_dir = tempfile::tempdir().unwrap();
    let script = cgi_dir.path().join("env.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf 'Content-Type: text/plain\\r\\nX-Cgi: yes\\r\\n\\r\\n'\necho \"method=$REQUEST_METHOD query=$QUERY_STRING info=$PATH_INFO\"\ncat\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let redirect = cgi_dir.path().join("redir.sh");
    std::fs::write(
        &redirect,
        "#!/bin/sh\nprintf 'Location: /elsewhere\\r\\n\\r\\n'\n",
    )
    .unwrap();
    std::fs::set_permissions(&redirect, std::fs::Permissions::from_mode(0o755)).unwrap();
    let extra = format!(
        "ns_section ns/server/s1/module/nscgi\nns_param map \"GET /cgi-bin {d}\"\nns_param map \"POST /cgi-bin {d}\"",
        d = cgi_dir.path().display()
    );
    let srv = boot("ns_param nscgi nscgi.so", &extra, |_| {});
    let r = request(srv.port, "GET /cgi-bin/env.sh/a/b?x=1 HTTP/1.0\r\n\r\n");
    assert_eq!(status(&r), 200, "{r}");
    assert!(r.contains("X-Cgi: yes"));
    assert!(r.contains("method=GET query=x=1 info=/a/b"), "{r}");
    let r = request(
        srv.port,
        "POST /cgi-bin/env.sh HTTP/1.0\r\nContent-Length: 5\r\n\r\nhello",
    );
    assert!(r.contains("method=POST"));
    assert!(r.trim_end().ends_with("hello"), "{r}");
    let r = request(srv.port, "GET /cgi-bin/redir.sh HTTP/1.0\r\n\r\n");
    assert_eq!(status(&r), 302);
    assert!(r.contains("/elsewhere"));
    assert_eq!(
        status(&request(srv.port, "GET /cgi-bin/missing HTTP/1.0\r\n\r\n")),
        404
    );
}

#[test]
fn nsd_binary_runs_and_stops_on_sigterm() {
    let home = tempfile::tempdir().unwrap();
    aoloxide::install_home(home.path(), true).unwrap();
    std::fs::create_dir_all(home.path().join("servers/s1/pages")).unwrap();
    std::fs::write(home.path().join("servers/s1/pages/index.html"), "binary ok").unwrap();
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let cfg = home.path().join("nsd.tcl");
    std::fs::write(
        &cfg,
        format!(
            "ns_section ns/parameters\nns_param home {}\nns_section ns/servers\nns_param s1 S1\n\
             ns_section ns/server/s1/modules\nns_param nssock nssock.so\n\
             ns_section ns/server/s1/module/nssock\nns_param port {port}\nns_param address 127.0.0.1\n",
            home.path().display()
        ),
    )
    .unwrap();
    let bin = env!("CARGO_BIN_EXE_nsd");
    let check = std::process::Command::new(bin)
        .args(["-c", "-t"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(check.status.success());
    let mut child = std::process::Command::new(bin)
        .args(["-f", "-t"])
        .arg(&cfg)
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut body = String::new();
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            body = request(port, "GET / HTTP/1.0\r\n\r\n");
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // SAFETY: sending SIGTERM to our own child process.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    let status = child.wait().unwrap();
    assert!(body.ends_with("binary ok"), "{body}");
    assert!(status.success(), "{status:?}");
    assert!(
        !home.path().join("log/nsd.pid").exists(),
        "pid file removed at exit"
    );
}

#[test]
fn nsssl_serves_https() {
    use std::sync::Arc;
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cert = dir.path().join("cert.pem");
    let key = dir.path().join("key.pem");
    std::fs::write(&cert, ck.cert.pem()).unwrap();
    std::fs::write(&key, ck.key_pair.serialize_pem()).unwrap();
    // Loaded under an instance name other than the driver's own.
    let extra = format!(
        "ns_section ns/server/s1/module/ssl1\nns_param certfile {}\nns_param keyfile {}\nns_param port 0\nns_param address 127.0.0.1\nns_param hostname localhost",
        cert.display(),
        key.display()
    );
    let srv = boot("ns_param ssl1 nsssl.so", &extra, |h| {
        std::fs::write(h.join("servers/s1/pages/s.txt"), "secure").unwrap();
        let lib = h.join("servers/s1/modules/tcl");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(
            lib.join("d.tcl"),
            "ns_register_proc GET /drv d_proc\nproc d_proc {} { ns_return 200 text/plain [ns_conn driver] }\n",
        )
        .unwrap();
    });
    let drivers = srv.nsd.drivers();
    let tls = drivers
        .iter()
        .find(|d| d.info.protocol == "https")
        .expect("https driver");
    assert_eq!(tls.info.name, "nsssl", "fixed DRIVER_NAME");
    assert_eq!(tls.info.module, "ssl1");
    assert!(tls.info.location().starts_with("https://localhost:"));
    let port = tls.info.port();

    let mut roots = rustls::RootCertStore::empty();
    roots.add(ck.cert.der().clone()).unwrap();
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let body = rt.block_on(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let conn = tokio_rustls::TlsConnector::from(Arc::new(cfg));
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let mut s = conn.connect(name, tcp).await.unwrap();
        s.write_all(b"GET /s.txt HTTP/1.0\r\n\r\n").await.unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out).await;
        let first = String::from_utf8_lossy(&out).into_owned();
        let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let mut s = conn.connect(name, tcp).await.unwrap();
        s.write_all(b"GET /drv HTTP/1.0\r\n\r\n").await.unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out).await;
        (first, String::from_utf8_lossy(&out).into_owned())
    });
    let (body, drv) = body;
    assert!(body.starts_with("HTTP/1.0 200 OK"), "{body}");
    assert!(body.ends_with("secure"));
    assert!(drv.ends_with("\r\n\r\nnsssl"), "ns_conn driver: {drv}");
}

#[test]
fn nsssl_without_cert_is_skipped() {
    let srv = boot("ns_param nsssl nsssl.so", "", |h| {
        let lib = h.join("servers/s1/modules/tcl");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(
            lib.join("si.tcl"),
            "ns_register_proc GET /si si_proc\nproc si_proc {} { ns_return 200 text/plain \"<[ssl_info]>\" }\n",
        )
        .unwrap();
    });
    assert_eq!(srv.nsd.drivers().len(), 1, "only nssock");
    // NsSSLInitialize registers ssl_info before the certfile check; its
    // InfoCmd output is "{SSL_EXPORT} {SSL_PROTOCOL_VERSION} ".
    let r = request(srv.port, "GET /si HTTP/1.0\r\n\r\n");
    assert!(r.ends_with("<{0} {2} >"), "{r}");
    assert!(srv
        .nsd
        .global
        .log
        .captured()
        .iter()
        .any(|l| l.contains("certfile not specified")));
}

#[test]
fn nszlib_command() {
    let srv = boot("ns_param nszlib nszlib.so", "", |h| {
        let lib = h.join("servers/s1/modules/tcl");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(
            lib.join("z.tcl"),
            "ns_register_proc GET /z z_proc\nproc z_proc {} { ns_return 200 text/plain [ns_zlib uncompress [ns_zlib compress roundtrip]] }\n\
             ns_register_proc GET /big big_proc\nproc big_proc {} { ns_conn gzip 1; ns_return 200 text/plain [string repeat abc 5000] }\n",
        )
        .unwrap();
    });
    let r = request(srv.port, "GET /z HTTP/1.0\r\n\r\n");
    assert!(r.ends_with("roundtrip"), "{r}");
    // Loading nszlib installs Ns_SetGzipProc, so responses are compressed.
    let mut s = TcpStream::connect(("127.0.0.1", srv.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(b"GET /big HTTP/1.0\r\nAccept-Encoding: gzip\r\n\r\n")
        .unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let split = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&out[..split]).into_owned();
    assert!(head.contains("Content-Encoding: gzip"), "{head}");
    let body = nszlib::gunzip_bytes(&out[split + 4..]).unwrap();
    assert_eq!(body, "abc".repeat(5000).as_bytes());
}

#[test]
fn nscp_control_port() {
    let pass = ns_core::crypt::crypt("secret", "xy");
    let extra = format!(
        "ns_section ns/server/s1/module/nscp\nns_param address 127.0.0.1\nns_param port 0\nns_param cpcmdlogging true\n\
         ns_section ns/server/s1/module/nscp/users\nns_param user \"admin:{pass}\""
    );
    let srv = boot("ns_param nscp nscp.so", &extra, |_| {});
    let port: u16 = srv.nsd.server("s1").unwrap().nsv.with("nscp", false, |a| {
        a.unwrap().get("port").unwrap().parse().unwrap()
    });
    let session = |input: &str| {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.write_all(input.as_bytes()).unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        String::from_utf8_lossy(&out).into_owned()
    };
    let out = session(
        "admin\nsecret\nexpr {6 * 7}\nset x [list a \\\nb]\nnsv_set cp k v\nerror oops\nexit\n",
    );
    assert!(out.contains("Welcome to s1"), "{out}");
    assert!(out.contains("42\r\n"), "{out}");
    assert!(out.contains("s1:nscp 2>>> "), "continuation prompt: {out}");
    assert!(out.contains("a b\r\n"));
    assert!(out.contains("oops\r\n"));
    let v = srv
        .nsd
        .server("s1")
        .unwrap()
        .nsv
        .with("cp", false, |a| a.and_then(|a| a.get("k").cloned()));
    assert_eq!(v.as_deref(), Some("v"));
    assert!(session("admin\nwrong\n").contains("Access denied!"));
    assert!(srv
        .nsd
        .global
        .log
        .captured()
        .iter()
        .any(|l| l.contains("nscp: admin 1: expr {6 * 7}")));
}

#[test]
fn nsdb_sqlite_pool() {
    let dbfile = tempfile::NamedTempFile::new().unwrap();
    let extra = format!(
        "ns_section ns/db/drivers\nns_param sqlite nssqlite.so\n\
         ns_section ns/db/pools\nns_param main \"Main DB\"\n\
         ns_section ns/db/pool/main\nns_param driver sqlite\nns_param datasource {}\nns_param connections 2\n\
         ns_section ns/server/s1/db\nns_param pools *\nns_param defaultpool main",
        dbfile.path().display()
    );
    let srv = boot("ns_param nsdb nsdb.so", &extra, |h| {
        let lib = h.join("servers/s1/modules/tcl");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(
            lib.join("db.tcl"),
            r#"
ns_register_proc GET /db db_proc
proc db_proc {} {
    set db [ns_db gethandle]
    ns_db dml $db "create table if not exists t (id integer, name text)"
    ns_db dml $db "delete from t"
    ns_db dml $db "insert into t values (1, [ns_dbquotevalue alice])"
    ns_db dml $db "insert into t values (2, 'bob')"
    set out {}
    set row [ns_db select $db "select id, name from t order by id"]
    while {[ns_db getrow $db $row]} {
        lappend out "[ns_set get $row id]=[ns_set get $row name]"
    }
    set one [ns_db 1row $db "select count(*) as n from t"]
    lappend out n=[ns_set get $one n]
    lappend out none=[ns_db 0or1row $db "select * from t where id = 99"]
    lappend out [ns_db exec $db "select 1"] [ns_db exec $db "update t set name = name"]
    lappend out [catch {ns_db 1row $db "select * from t"} m] $m
    lappend out [catch {ns_db select $db "select * from nosuch"} m] [string match {Database operation "select" failed*} $m]
    lappend out [ns_db poolname $db] [ns_db dbtype $db] [ns_db driver $db] [ns_db pools] [ns_pooldescription main]
    # The thread already owns a handle from main: a nested gethandle fails.
    lappend out [catch {ns_db gethandle main 1} m] $m $db
    ns_db releasehandle $db
    ns_return 200 text/plain $out
}
"#,
        )
        .unwrap();
    });
    let r = request(srv.port, "GET /db HTTP/1.0\r\n\r\n");
    let body = r.split("\r\n\r\n").nth(1).unwrap_or("");
    assert_eq!(
        body,
        "1=alice 2=bob n=2 none= NS_ROWS NS_DML 1 {Database operation \"1row\" failed (exception NSINT, \"Query returned more than one row.\")} 1 1 main SQLite sqlite main {Main DB} 1 {could not allocate 1 handle(s) from pool \"main\"} nsdb0",
        "{r} {:?}",
        srv.nsd.global.log.captured()
    );
    assert!(
        srv.nsd.global.log.captured().iter().any(|l| l.contains(
            "dbinit: db handle limit exceeded: thread already owns 1 handle from pool 'main'"
        )),
        "{:?}",
        srv.nsd.global.log.captured()
    );
    // The second handle taken above was released by ns_cleanup: both
    // handles are available again on the next request.
    let r = request(srv.port, "GET /db HTTP/1.0\r\n\r\n");
    assert!(r.contains("1=alice 2=bob"), "{r}");
}

#[test]
fn nsproxy_handles_are_released_after_each_conn() {
    // The worker binary lives in the target profile dir, an ancestor of
    // this test executable (`cargo test --workspace` builds it).
    let exe = std::env::current_exe().unwrap();
    let worker = exe
        .ancestors()
        .map(|d| d.join("nsproxy"))
        .find(|p| p.is_file())
        .expect("nsproxy worker binary: run `cargo build -p nsproxy` first");
    let srv = boot("ns_param nsproxy nsproxy.so", "", |h| {
        let lib = h.join("servers/s1/modules/tcl");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(
            lib.join("proxy.tcl"),
            r#"
ns_proxy config web -exec {WORKER} -max 1 -gettimeout 2000 -evaltimeout 5000
ns_register_proc GET /proxy proxy_proc
proc proxy_proc {} {
    # Deliberately never released: the deallocate trace must return it.
    set h [ns_proxy get web]
    ns_return 200 text/plain "$h [ns_proxy eval $h {expr {2+3}}]"
}
"#
            .replace("WORKER", &worker.display().to_string()),
        )
        .unwrap();
    });
    for _ in 0..3 {
        let r = request(srv.port, "GET /proxy HTTP/1.0\r\n\r\n");
        let body = r.split("\r\n\r\n").nth(1).unwrap_or("");
        assert_eq!(
            body,
            "web-proxy-0 5",
            "{r} {:?}",
            srv.nsd.global.log.captured()
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn nsdb_handle_subcommands_and_misconfig() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("x.sqlite");
    // Ns_DbInterpretSqlFile: `--` comments only at the start of a line,
    // `;` inside single quotes, doubled quotes kept.
    std::fs::write(
        dir.path().join("schema.sql"),
        "-- schema\ncreate table s (a integer, b text);\ninsert into s values (1, 'x;y');\n\
         -- second\ninsert into s values (2, 'it''s');\n",
    )
    .unwrap();
    // A trailing statement without `;` is an error and is not run.
    std::fs::write(
        dir.path().join("bad.sql"),
        "insert into s values (9, 'z')\n",
    )
    .unwrap();
    // Double quotes do not protect a `;`.
    std::fs::write(
        dir.path().join("dq.sql"),
        "insert into s values (3, \"a;b\");\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("x.csv"), " a , b ,\"c \",\n\n").unwrap();
    let extra = format!(
        "ns_section ns/db/drivers\nns_param sqlite nssqlite.so\nns_param bogus nsbogus.so\n\
         ns_section ns/db/pools\nns_param one One\nns_param nodrv x\nns_param baddrv x\nns_param nods x\nns_param hidden H\n\
         ns_section ns/db/pool/one\nns_param driver sqlite\nns_param datasource {db}\nns_param connections 1\n\
         ns_param user scott\nns_param password tiger\nns_param verbose on\nns_param logsqlerrors on\n\
         ns_section ns/db/pool/nodrv\nns_param datasource x\n\
         ns_section ns/db/pool/baddrv\nns_param driver nosuch\nns_param datasource x\n\
         ns_section ns/db/pool/nods\nns_param driver sqlite\n\
         ns_section ns/db/pool/hidden\nns_param driver sqlite\nns_param datasource :memory:\n\
         ns_section ns/server/s1/db\nns_param pools one\nns_param defaultpool one",
        db = db.display()
    );
    let script = r#"
ns_register_proc GET /dbx dbx_proc
proc dbx_proc {} {
    set out {}
    set h [ns_db gethandle one]
    lappend out $h [ns_db pools] [ns_db datasource $h] [ns_db user $h] [ns_db password $h]
    lappend out [ns_db verbose $h] [ns_db verbose $h off] [ns_db verbose $h on]
    lappend out [catch {ns_db verbose $h maybe} m] $m
    ns_db interpretsqlfile $h SQLDIR/schema.sql
    lappend out [ns_set get [ns_db 1row $h "select count(*) as n from s"] n]
    lappend out [ns_set get [ns_db 1row $h "select b from s where a = 1"] b]
    lappend out [ns_set get [ns_db 1row $h "select b from s where a = 2"] b]
    lappend out [catch {ns_db interpretsqlfile $h SQLDIR/bad.sql} m] $m
    lappend out [catch {ns_db interpretsqlfile $h SQLDIR/dq.sql} m] [string match {Database operation "interpretsqlfile" failed (exception *} $m]
    lappend out [catch {ns_db interpretsqlfile $h SQLDIR/missing.sql} m] $m
    lappend out [ns_set get [ns_db 1row $h "select count(*) as n from s"] n]
    lappend out [catch {ns_db select $h "delete from s where a = 99"} m] $m
    lappend out [catch {ns_db dml $h "select * from s"} m] $m
    lappend out [ns_db exception $h] [ns_dberrorcode $h] [ns_dberrormsg $h]
    lappend out [catch {ns_db 1row $h "select * from s where a = 99"} m] $m
    lappend out [catch {ns_db 0or1row $h "select * from s"} m] $m
    lappend out [ns_db connected $h] [ns_db exception $h]
    set row [ns_db select $h "select a from s order by a"]
    lappend out [catch {ns_db getrow $h nosuchset} m] $m
    lappend out [ns_db getrow $h $row] [ns_set get $row a]
    lappend out [ns_set size [ns_db bindrow $h]]
    ns_db flush $h
    lappend out [ns_db getrow $h $row]
    lappend out [catch {ns_db dml $h "insert into nosuch values (1)"} m] [string match {Database operation "dml" failed (exception *} $m]
    lappend out [ns_db resethandle $h] [ns_db connected $h]
    ns_db setexception $h 42000 "my message"
    lappend out [ns_db exception $h]
    lappend out [catch {ns_db setexception $h 123456 x} m] $m
    lappend out [catch {ns_db sp_start $h p} m] $m
    lappend out [catch {ns_db gethandle one} m] $m
    lappend out [catch {ns_db gethandle one 0} m] $m
    lappend out [catch {ns_db gethandle one 2} m] $m
    lappend out [catch {ns_db gethandle one 1 extra} m] $m
    lappend out [catch {ns_db gethandle -timeout x one} m] $m
    # Another thread: the only handle is held here, so a timed wait gives "".
    lappend out [ns_thread wait [ns_thread begin {ns_db gethandle -timeout 0 one}]]
    # Handle ids belong to the interp that took them.
    lappend out [ns_thread wait [ns_thread begin "ns_db poolname $h"]]
    lappend out [ns_thread wait [ns_thread begin "ns_db releasehandle $h"]] [ns_db poolname $h]
    # A negative timeout waits for ever.
    set tid [ns_thread begin {ns_db gethandle -timeout -1 one}]
    after 300
    ns_db releasehandle $h
    lappend out [ns_thread wait $tid]
    set h [ns_db gethandle one]
    lappend out [catch {ns_db gethandle hidden} m] $m
    lappend out [catch {ns_db gethandle nosuch} m] $m
    lappend out [catch {ns_db bouncepool nosuch} m] $m
    lappend out [ns_db bouncepool hidden] [ns_pooldescription hidden] [ns_pooldescription nosuch]
    ns_db disconnect $h
    lappend out [ns_db connected $h] [ns_db dbtype $h]
    lappend out [catch {ns_db select $h "select 1"} m] $m
    lappend out [catch {ns_db flush $h} m] $m
    ns_db releasehandle $h
    lappend out [catch {ns_db releasehandle $h} m] $m
    ns_db bouncepool one
    set h [ns_db gethandle]
    lappend out $h [ns_set get [ns_db 0or1row $h "select a from s where a = 2"] a]
    ns_db releasehandle $h
    lappend out [ns_quotelisttolist {a b 'c d'}] [ns_quotelisttolist {'it''s'}] [ns_quotelisttolist {'a','b'}]
    set f [open SQLDIR/x.csv]
    set cols unset
    lappend out [ns_getcsv $f cols] $cols
    lappend out [ns_getcsv $f cols] $cols
    lappend out [ns_getcsv $f cols] $cols
    close $f
    lappend out [catch {ns_db select nsdb0} m] $m [catch {ns_db releasehandle} m] $m
    ns_return 200 text/plain $out
}
"#
    .replace("SQLDIR", &dir.path().display().to_string());
    let srv = boot("ns_param nsdb nsdb.so", &extra, |h| {
        let lib = h.join("servers/s1/modules/tcl");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("dbx.tcl"), &script).unwrap();
    });
    let r = request(srv.port, "GET /dbx HTTP/1.0\r\n\r\n");
    let body = r.split("\r\n\r\n").nth(1).unwrap_or("");
    let log = srv.nsd.global.log.captured();
    let i = ns_tcl::TclEngine::new().create_interp().unwrap();
    let items = i.list_split(body).unwrap_or_default();
    assert!(items.len() > 80, "{r} {log:?}");
    let ds = db.display().to_string();
    let want: Vec<(usize, &str)> = vec![
        (0, "nsdb0"),
        (1, "one"),
        (2, &ds),
        (3, "scott"),
        (4, "tiger"),
        (5, "1"),
        (6, "0"),
        (7, "1"),
        (8, "1"),
        (9, "expected boolean value but got \"maybe\""),
        (10, "2"),
        (11, "x;y"),
        (12, "it's"),
        (13, "1"),
        (
            14,
            "Database operation \"interpretsqlfile\" failed (exception NSINT, \"File ends with unterminated SQL\")",
        ),
        (15, "1"),
        (16, "1"),
        (
            18,
            "Database operation \"interpretsqlfile\" failed (exception NSINT, \"Could not read file\")",
        ),
        (19, "2"),
        (20, "1"),
        (
            21,
            "Database operation \"select\" failed (exception NSDB, \"Query was not a statement returning rows.\")",
        ),
        (22, "1"),
        (
            23,
            "Database operation \"dml\" failed (exception NSDB, \"Query was not a DML or DDL command.\")",
        ),
        (24, "NSDB {Query was not a DML or DDL command.}"),
        (25, "NSDB"),
        (26, "Query was not a DML or DDL command."),
        (27, "1"),
        (
            28,
            "Database operation \"1row\" failed (exception NSINT, \"Query did not return a row.\")",
        ),
        (29, "1"),
        (
            30,
            "Database operation \"0or1row\" failed (exception NSINT, \"Query returned more than one row.\")",
        ),
        (31, "1"),
        (32, "{} {}"),
        (33, "1"),
        (34, "no such set: nosuchset"),
        (35, "1"),
        (36, "1"),
        (37, "1"),
        (38, "0"),
        (39, "1"),
        (40, "1"),
        (41, "0"),
        (42, "1"),
        (43, "42000 {my message}"),
        (44, "1"),
        (45, "code \"123456\" more than 5 characters"),
        (46, "1"),
        (48, "1"),
        (49, "could not allocate 1 handle(s) from pool \"one\""),
        (50, "1"),
        (51, "invalid nhandles:  \"0\": should be greater than 0."),
        (52, "1"),
        (53, "could not allocate 2 handle(s) from pool \"one\""),
        (54, "1"),
        (
            55,
            "wrong # args: should be \"ns_db gethandle ?-timeout seconds? ?pool? ?nhandles?\"",
        ),
        (56, "1"),
        (57, "expected integer but got \"x\""),
        (58, ""),
        (59, "invalid database id:  \"nsdb0\""),
        (60, "invalid database id:  \"nsdb0\""),
        (61, "one"),
        (62, "nsdb0"),
        (63, "1"),
        (64, "no access to pool: \"hidden\""),
        (65, "1"),
        (66, "no access to pool: \"nosuch\""),
        (67, "1"),
        (68, "Database operation \"bouncepool\" failed"),
        (69, ""),
        (70, "H"),
        (71, ""),
        (72, "0"),
        (73, ""),
        (74, "1"),
        (75, "Database operation \"select\" failed"),
        (76, "1"),
        (77, "Database operation \"flush\" failed"),
        (78, "1"),
        (79, "invalid database id:  \"nsdb0\""),
        (80, "nsdb0"),
        (81, "2"),
        (82, "a b {c d}"),
        (83, "it s"),
        (84, "a ,b"),
        (85, "4"),
        (86, "a b {c } {}"),
        (87, "0"),
        (88, ""),
        (89, "-1"),
        (90, ""),
        (91, "1"),
        (92, "wrong # args: should be \"ns_db select dbId arg\""),
        (93, "1"),
        (94, "wrong # args: should be \"ns_db releasehandle dbId\""),
    ];
    for (idx, w) in want {
        assert_eq!(
            items.get(idx).map(String::as_str),
            Some(w),
            "item {idx} in {items:?}\n{log:?}"
        );
    }
    assert!(items[47].contains("not supported"), "{}", items[47]);
    assert_eq!(items.len(), 95, "{items:?}");
    let log = log.join("\n");
    for want in [
        "no such driver: bogus",
        "no driver for pool 'nodrv'",
        "no such driver 'nosuch' for pool 'baddrv'",
        "missing datasource for pool 'nods'",
        &format!("dbinit: sql({ds}): 'select count(*) as n from s'"),
        &format!("dbinit: error({ds},"),
        "dbinit: db handle limit exceeded: thread already owns 1 handle from pool 'one'",
        "dbinit: failed to get 2 handles from a db pool of only 1 handles: 'one'",
        &format!("dbdrv: opening database 'sqlite:{ds}'"),
    ] {
        assert!(log.contains(want), "missing {want:?} in {log}");
    }
}

#[test]
fn nsdb_pools_belong_to_each_server_instance() {
    // Each server reads its own pools, even when a new `Global` reuses
    // the address of one that was shut down.
    for i in 0..4 {
        let extra = format!(
            "ns_section ns/db/pools\nns_param pool{i} P{i}\n\
             ns_section ns/db/pool/pool{i}\nns_param driver sqlite\nns_param datasource :memory:\n\
             ns_section ns/server/s1/db\nns_param pools *"
        );
        let srv = boot("ns_param nsdb nsdb.so", &extra, |h| {
            let lib = h.join("servers/s1/modules/tcl");
            std::fs::create_dir_all(&lib).unwrap();
            std::fs::write(
                lib.join("p.tcl"),
                "ns_register_proc GET /p p_proc\nproc p_proc {} {ns_return 200 text/plain [ns_db pools]}\n",
            )
            .unwrap();
        });
        let r = request(srv.port, "GET /p HTTP/1.0\r\n\r\n");
        let want = format!("pool{i}");
        assert_eq!(r.split("\r\n\r\n").nth(1), Some(want.as_str()), "{r}");
        drop(srv);
    }
}

#[test]
fn nsext_pool_with_a_local_daemon() {
    // The SQLite proxy daemon lives in the target profile dir, like the
    // nsproxy worker (`cargo test --workspace` builds it).
    let exe = std::env::current_exe().unwrap();
    let daemon = exe
        .ancestors()
        .map(|d| d.join("nssqlitepd"))
        .find(|p| p.is_file())
        .expect("nssqlitepd binary: run `cargo build -p nspd` first");
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ext.sqlite");
    // A relative LocalDaemon is found in the home's bin directory.
    let extra = format!(
        "ns_section ns/db/drivers\nns_param external nsext.so\n\
         ns_section ns/db/driver/external\nns_param LocalDaemon nssqlitepd\nns_param Timeout 20\n\
         ns_section ns/db/pools\nns_param ext \"External DB\"\n\
         ns_section ns/db/pool/ext\nns_param driver external\nns_param datasource {}\nns_param connections 2\n\
         ns_section ns/server/s1/db\nns_param pools *\nns_param defaultpool ext",
        db.display()
    );
    let srv = boot("ns_param nsdb nsdb.so", &extra, |h| {
        std::fs::create_dir_all(h.join("bin")).unwrap();
        std::fs::copy(&daemon, h.join("bin/nssqlitepd")).unwrap();
        let lib = h.join("servers/s1/modules/tcl");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(
            lib.join("ext.tcl"),
            r#"
ns_register_proc GET /ext ext_proc
proc ext_proc {} {
    set db [ns_db gethandle]
    ns_db dml $db "create table if not exists t (id integer, name text)"
    ns_db dml $db "insert into t values (1, 'one')"
    set out {}
    set row [ns_db select $db "select id, name from t order by id limit 1"]
    while {[ns_db getrow $db $row]} {
        lappend out "[ns_set get $row id]=[ns_set get $row name]"
    }
    lappend out [ns_db driver $db] [ns_db dbtype $db] [ns_ext isremote $db] [ns_ext ping $db]
    lappend out [ns_set get [ns_db 1row $db "select count(*) as n from t"] n]
    ns_db releasehandle $db
    ns_return 200 text/plain $out
}
"#,
        )
        .unwrap();
    });
    for n in 1..=2 {
        let r = request(srv.port, "GET /ext HTTP/1.0\r\n\r\n");
        let body = r.split("\r\n\r\n").nth(1).unwrap_or("");
        assert_eq!(
            body,
            format!("1=one External SQLite 0 ok {n}"),
            "{r} {:?}",
            srv.nsd.global.log.captured()
        );
    }
    // Both requests used the same daemon connection.
    let opened = srv
        .nsd
        .global
        .log
        .captured()
        .iter()
        .filter(|l| l.contains("nsext: datasource opened:"))
        .count();
    assert_eq!(opened, 1);
}
