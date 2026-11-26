//! The `nssqlitepd` binary on stdin/stdout, as nsext's `LocalDaemon`
//! runs it (this test also makes `cargo test` build the binary that
//! other crates' tests start).

#![allow(clippy::unwrap_used)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::Duration;

use nspd::msg::{encode, Msg};

#[test]
fn serves_one_connection_on_stdio() {
    let (ours, theirs) = UnixStream::pair().unwrap();
    ours.set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let out = theirs.try_clone().unwrap();
    let st = Command::new(env!("CARGO_BIN_EXE_nssqlitepd"))
        .stdin(Stdio::from(std::os::fd::OwnedFd::from(theirs)))
        .stdout(Stdio::from(std::os::fd::OwnedFd::from(out)))
        .status()
        .unwrap();
    // The daemon detaches: its first process exits at once.
    assert!(st.success());
    let dir = tempfile::tempdir().unwrap();
    let ds = format!("{}###", dir.path().join("d.db").display());
    let mut w = ours.try_clone().unwrap();
    let mut r = BufReader::new(ours);
    let mut line = || {
        let mut s = String::new();
        r.read_line(&mut s).unwrap();
        s
    };
    w.write_all(&encode(Msg::Ping, None)).unwrap();
    assert_eq!(line(), "ok\n");
    w.write_all(&encode(Msg::Open, Some(ds.as_bytes())))
        .unwrap();
    assert_eq!(line(), "ok\n");
    w.write_all(&encode(Msg::Identify, None)).unwrap();
    assert_eq!(line(), "SQLite Proxy Daemon v4.5\n");
    w.write_all(&encode(Msg::Exec, Some(b"select 41 + 1 as n")))
        .unwrap();
    assert_eq!(line(), "ok\n");
    assert_eq!(line(), "exec_rows\n");
    w.write_all(&encode(Msg::GetRow, Some(b"1"))).unwrap();
    assert_eq!(line(), "ok\n");
    assert_eq!(line(), "2\n");
    w.write_all(&encode(Msg::Close, None)).unwrap();
    // The rest: `42`, the list end, close's status, then end of file.
    let mut rest = String::new();
    r.read_to_string(&mut rest).unwrap();
    assert_eq!(rest, "42-1\nok\n");
}
