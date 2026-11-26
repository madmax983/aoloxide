//! Temp-file failures (`Ns_GetTemp` returning -1).  A test binary of its
//! own: it points the temp-file directory at a missing path for the whole
//! process, and needs the temp-file pool empty.

#![allow(clippy::unwrap_used)]

mod common;

use std::sync::atomic::Ordering;

use common::{Response, TestServer};

fn post(srv: &TestServer, body: &str) -> String {
    srv.raw(&format!(
        "POST /chan HTTP/1.0\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    ))
}

#[test]
fn temp_file_failures() {
    let missing = tempfile::tempdir().unwrap().path().join("missing");
    nsd::content::set_temp_dir(Some(&missing));
    let srv = TestServer::start(
        "ns_section ns/server/s1/module/nssock\nns_param maxinput 4096\n",
        |home| {
            let lib = home.join("servers/s1/modules/tcl");
            std::fs::create_dir_all(&lib).unwrap();
            std::fs::write(
                lib.join("t.tcl"),
                r"
ns_register_proc POST /chan chan_proc
proc chan_proc {} {
    ns_return 200 text/plain [list [ns_conn contentchannel] [ns_conn contentlength]]
}
",
            )
            .unwrap();
        },
    );
    assert_eq!(nsd::content::pooled_temps(), 0);
    // Ns_ConnContentFd fails: contentchannel returns nothing and logs.
    let r = Response::parse(&post(&srv, "a=1&b=2"));
    assert_eq!((r.status, r.body.as_str()), (200, "{} 7"));
    assert!(
        srv.log().iter().any(|l| l.contains("could not get fd")),
        "{:?}",
        srv.log()
    );
    // The driver cannot spool a body over maxinput: the error is logged and
    // the socket closed with no reply.  As in AOLserver (SOCK_ERROR, then
    // SockClose) no driver statistic changes.
    let drv = &srv.nsd.drivers()[0].info;
    let dropped = drv.stats.dropped.load(Ordering::Relaxed);
    let reads = drv.stats.reads.load(Ordering::Relaxed);
    assert_eq!(post(&srv, &"x".repeat(6000)), "");
    assert!(
        srv.log().iter().any(|l| l.contains(&format!(
            "nssock: could not spool request content: could not open temp file {}",
            missing.display()
        ))),
        "{:?}",
        srv.log()
    );
    assert_eq!(drv.stats.dropped.load(Ordering::Relaxed), dropped);
    assert_eq!(drv.stats.reads.load(Ordering::Relaxed), reads);
    // With the directory back, spooling works again.
    nsd::content::set_temp_dir(None);
    let r = Response::parse(&post(&srv, &"x".repeat(6000)));
    assert_eq!(r.status, 200, "{:?}", srv.log());
    assert!(
        r.body.starts_with("file") && r.body.ends_with(" 6000"),
        "{}",
        r.body
    );
}
