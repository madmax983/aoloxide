//! End-to-end tests: boot a real server and talk HTTP to it.

#![allow(clippy::unwrap_used)]

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use common::{Response, TestServer};

#[test]
fn serves_static_files_and_index() {
    let srv = TestServer::start("", |home| {
        let p = home.join("servers/s1/pages");
        std::fs::write(p.join("hello.txt"), "hello world").unwrap();
        std::fs::create_dir_all(p.join("dir")).unwrap();
        std::fs::write(p.join("dir/index.html"), "<p>index</p>").unwrap();
    });
    let r = srv.get("/hello.txt");
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body, "hello world");
    assert_eq!(r.header("content-type"), Some("text/plain"));
    assert_eq!(r.header("content-length"), Some("11"));
    assert!(r.header("server").unwrap().starts_with("AOLserver/4.5"));
    assert!(r.header("last-modified").is_some());

    let r = srv.get("/dir");
    assert_eq!(r.status, 302);
    assert!(r.header("location").unwrap().ends_with("/dir/"));
    let r = srv.get("/dir/");
    assert_eq!(r.status, 200);
    assert_eq!(r.body, "<p>index</p>");

    let r = srv.get("/missing.html");
    assert_eq!(r.status, 404);
    assert!(r.body.contains("Not Found"));
    assert!(
        r.body.len() >= 514,
        "error pages are padded to errorminsize"
    );

    let r = srv.get("/../../../etc/passwd");
    assert_eq!(r.status, 404, "no escaping the page root");
}

#[test]
fn conditional_get_and_head() {
    let srv = TestServer::start("", |home| {
        std::fs::write(home.join("servers/s1/pages/a.html"), "<b>a</b>").unwrap();
    });
    let r = srv.get("/a.html");
    let lm = r.header("last-modified").unwrap().to_owned();
    let raw = srv.raw(&format!(
        "GET /a.html HTTP/1.0\r\nIf-Modified-Since: {lm}\r\n\r\n"
    ));
    assert_eq!(Response::parse(&raw).status, 304);
    let raw = srv.raw("HEAD /a.html HTTP/1.0\r\n\r\n");
    let r = Response::parse(&raw);
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-length"), Some("8"));
    assert_eq!(r.body, "");
}

#[test]
fn tcl_request_procs_and_filters() {
    let srv = TestServer::start("", |home| {
        let lib = home.join("servers/s1/modules/tcl");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(
            lib.join("app.tcl"),
            r#"
ns_register_proc GET /hello hello_proc
proc hello_proc {} {
    set name [ns_queryget name world]
    ns_return 200 text/plain "hello $name from [ns_conn url] via [ns_conn method]"
}
ns_register_proc POST /echo echo_proc
proc echo_proc {} {
    set form [ns_conn form]
    ns_return 201 text/plain "[ns_set get $form a]+[ns_set get $form b]"
}
ns_register_filter preauth GET /secret/* deny_filter
proc deny_filter {why} {
    ns_returnforbidden
    return filter_return
}
ns_register_proc GET /error error_proc
proc error_proc {} { error "boom" }
ns_register_proc GET /nsv nsv_proc
proc nsv_proc {} {
    nsv_incr counter hits
    ns_return 200 text/plain [nsv_get counter hits]
}
ns_register_proc GET /headers headers_proc
proc headers_proc {} {
    set h [ns_conn headers]
    ns_set put [ns_conn outputheaders] X-Echo [ns_set iget $h x-test]
    ns_return 200 text/plain [ns_set size $h]
}
ns_register_proc GET /redirect redirect_proc
proc redirect_proc {} { ns_returnredirect /hello }
ns_register_proc GET /internal internal_proc
proc internal_proc {} { ns_internalredirect /hello }
"#,
        )
        .unwrap();
    });
    let r = srv.get("/hello?name=Mark");
    assert_eq!(r.status, 200, "{r:?} {:?}", srv.log());
    assert_eq!(r.body, "hello Mark from /hello via GET");

    let body = "a=1&b=two+words";
    let raw = srv.raw(&format!(
        "POST /echo HTTP/1.0\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    ));
    let r = Response::parse(&raw);
    assert_eq!(r.status, 201);
    assert_eq!(r.body, "1+two words");

    assert_eq!(srv.get("/secret/x").status, 403);
    let r = srv.get("/error");
    assert_eq!(r.status, 500);
    assert!(srv.log().iter().any(|l| l.contains("boom")));

    assert_eq!(srv.get("/nsv").body, "1");
    assert_eq!(srv.get("/nsv").body, "2");

    let raw = srv.raw("GET /headers HTTP/1.0\r\nX-Test: yes\r\nHost: h\r\n\r\n");
    let r = Response::parse(&raw);
    assert_eq!(r.header("x-echo"), Some("yes"));
    assert_eq!(r.body, "2");

    let r = srv.get("/redirect");
    assert_eq!(r.status, 302);
    assert!(r.header("location").unwrap().ends_with("/hello"));
    let r = srv.get("/internal");
    assert_eq!(r.status, 200);
    assert!(r.body.starts_with("hello world"));
}

#[test]
fn adp_pages() {
    let srv = TestServer::start("", |home| {
        let p = home.join("servers/s1/pages");
        std::fs::write(
            p.join("index.adp"),
            "<html><% set x 6 %><%= [expr {$x * 7}] %> <% ns_adp_include inc.adp a b %>|<script runat=server>ns_adp_puts -nonewline [ns_adp_argc]</script></html>",
        )
        .unwrap();
        std::fs::write(
            p.join("inc.adp"),
            "inc:<%= [ns_adp_argv 1][ns_adp_argv 2] %>",
        )
        .unwrap();
        std::fs::write(p.join("break.adp"), "before<% ns_adp_break %>after").unwrap();
        std::fs::write(p.join("err.adp"), "x<% error oops %>y").unwrap();
    });
    let r = srv.get("/index.adp");
    assert_eq!(r.status, 200, "{r:?} {:?}", srv.log());
    assert_eq!(r.body, "<html>42 inc:ab|1</html>");
    assert!(r.header("content-type").unwrap().starts_with("text/html"));
    let r = srv.get("/");
    assert_eq!(r.body, "<html>42 inc:ab|1</html>");
    assert_eq!(srv.get("/break.adp").body, "before");
    assert_eq!(srv.get("/err.adp").body, "xy");
}

#[test]
fn keepalive_and_pipelining() {
    let srv = TestServer::start("", |home| {
        std::fs::write(home.join("servers/s1/pages/k.txt"), "k").unwrap();
    });
    let mut s = TcpStream::connect(("127.0.0.1", srv.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"GET /k.txt HTTP/1.1\r\nHost: x\r\n\r\nGET /k.txt HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    assert_eq!(out.matches("HTTP/1.1 200 OK").count(), 2, "{out}");
    assert!(out.contains("Connection: keep-alive"));
    assert!(out.contains("Connection: close"));
}

#[test]
fn malformed_requests() {
    let srv = TestServer::start("", |_| {});
    assert!(srv.raw("BOGUS\r\n\r\n").contains("400"));
    let long = format!("GET /{} HTTP/1.0\r\n\r\n", "a".repeat(10_000));
    assert!(srv.raw(&long).contains("414"));
    let big = "POST / HTTP/1.0\r\nContent-Length: 99999999\r\n\r\n";
    assert!(srv.raw(big).contains("413"));
}

#[test]
fn chunked_streaming_output() {
    let srv = TestServer::start("", |home| {
        std::fs::write(
            home.join("servers/s1/pages/s.adp"),
            "one<% ns_adp_flush %>two",
        )
        .unwrap();
    });
    let raw = srv.raw("GET /s.adp HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    assert!(raw.contains("Transfer-Encoding: chunked"), "{raw}");
    assert!(raw.contains("3\r\none\r\n"));
    assert!(raw.ends_with("0\r\n\r\n"));
}
