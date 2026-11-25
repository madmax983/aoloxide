//! Integration tests for the compiled-in modules and the `nsd` binary.

#![allow(clippy::unwrap_used)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
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
    let extra = format!(
        "ns_section ns/server/s1/module/nsssl\nns_param certfile {}\nns_param keyfile {}\nns_param port 0\nns_param address 127.0.0.1\nns_param hostname localhost",
        cert.display(),
        key.display()
    );
    let srv = boot("ns_param nsssl nsssl.so", &extra, |h| {
        std::fs::write(h.join("servers/s1/pages/s.txt"), "secure").unwrap();
    });
    let drivers = srv.nsd.drivers();
    let tls = drivers
        .iter()
        .find(|d| d.info.protocol == "https")
        .expect("https driver");
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
        String::from_utf8_lossy(&out).into_owned()
    });
    assert!(body.starts_with("HTTP/1.0 200 OK"), "{body}");
    assert!(body.ends_with("secure"));
}

#[test]
fn nsssl_without_cert_is_skipped() {
    let srv = boot("ns_param nsssl nsssl.so", "", |_| {});
    assert_eq!(srv.nsd.drivers().len(), 1, "only nssock");
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
            "ns_register_proc GET /z z_proc\nproc z_proc {} { ns_return 200 text/plain [ns_zlib uncompress [ns_zlib compress roundtrip]] }\n",
        )
        .unwrap();
    });
    let r = request(srv.port, "GET /z HTTP/1.0\r\n\r\n");
    assert!(r.ends_with("roundtrip"), "{r}");
}
