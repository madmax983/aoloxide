//! Shared helpers for integration tests: boot a server in a temp home and
//! talk HTTP to it.

#![allow(dead_code, clippy::unwrap_used)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nsd::{ModuleRegistry, Nsd, NsdOptions};

/// A running test server.
pub struct TestServer {
    pub nsd: Nsd,
    pub home: tempfile::TempDir,
    pub port: u16,
}

impl TestServer {
    /// Boot with the given extra config appended to a standard config.
    pub fn start(extra: &str, setup: impl FnOnce(&Path)) -> Self {
        Self::start_with(extra, setup, nsd::builtin_modules())
    }

    /// Boot with a custom module registry.
    pub fn start_with(extra: &str, setup: impl FnOnce(&Path), modules: ModuleRegistry) -> Self {
        let home = tempfile::tempdir().unwrap();
        nsd::init_tcl::install_home(home.path(), true).unwrap();
        let pages = home.path().join("servers/s1/pages");
        std::fs::create_dir_all(&pages).unwrap();
        setup(home.path());
        let cfg = format!(
            r#"
ns_section ns/parameters
ns_param home {home}
ns_param shutdowntimeout 5
ns_section ns/servers
ns_param s1 "Test Server"
ns_section ns/server/s1
ns_param directoryfile index.adp,index.html
ns_param maxthreads 4
ns_section ns/server/s1/modules
ns_param nssock nssock.so
ns_section ns/server/s1/module/nssock
ns_param port 0
ns_param address 127.0.0.1
ns_param hostname localhost
ns_param keepwait 2
ns_section ns/server/s1/adp
ns_param map /*.adp
{extra}
"#,
            home = home.path().display()
        );
        let mut opts = NsdOptions::from_text(&cfg);
        opts.log_memory = true;
        opts.modules = modules;
        let nsd = Nsd::new(opts).unwrap();
        nsd.start().unwrap();
        let port = nsd.drivers()[0].info.port();
        Self { nsd, home, port }
    }

    pub fn pages(&self) -> PathBuf {
        self.home.path().join("servers/s1/pages")
    }

    /// Send a raw request and read until the server closes.
    pub fn raw(&self, req: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.write_all(req.as_bytes()).unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        String::from_utf8_lossy(&out).into_owned()
    }

    /// A simple GET with `Connection: close`.
    pub fn get(&self, url: &str) -> Response {
        Response::parse(&self.raw(&format!("GET {url} HTTP/1.0\r\nHost: localhost\r\n\r\n")))
    }

    pub fn log(&self) -> Vec<String> {
        self.nsd.global.log.captured()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.nsd.shutdown();
    }
}

/// A parsed HTTP response.
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Response {
    pub fn parse(raw: &str) -> Self {
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw, ""));
        let mut lines = head.lines();
        let status = lines
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let headers = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
            .collect();
        Self {
            status,
            headers,
            body: body.to_owned(),
        }
    }

    pub fn header(&self, k: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(h, _)| h == k)
            .map(|(_, v)| v.as_str())
    }
}
