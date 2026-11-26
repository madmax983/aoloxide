//! Socket helpers (`nsd/sock.c`): address lookup, bind, listen, accept,
//! connect, blocking mode, socket pairs and readiness waits, on raw
//! descriptors as the C API uses them.
//!
//! Every descriptor these functions create is close-on-exec, as
//! `SockSetup` makes them.

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::fd::RawFd;
use std::time::Duration;

/// Close a descriptor (`ns_sockclose`).  Errors are ignored.
pub fn close(fd: RawFd) {
    if fd >= 0 {
        // SAFETY: closing a descriptor the caller owns.
        unsafe { libc::close(fd) };
    }
}

fn cvt(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

/// Mark `fd` close-on-exec (`SockSetup`).
fn setup(fd: RawFd) -> RawFd {
    // SAFETY: fcntl on a descriptor we own; failure leaves it unchanged.
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    fd
}

/// Tcl's spelling of an OS error (`Tcl_PosixError`): the system message,
/// lower-cased, without Rust's `(os error N)` suffix.
pub fn posix_error(e: &io::Error) -> String {
    let s = e.to_string();
    let s = s.split(" (os error").next().unwrap_or(&s);
    s.to_ascii_lowercase()
}

/// Resolve `host` to an IPv4 address (`Ns_GetSockAddr`): `None` is
/// `INADDR_ANY`, a dotted quad is used as is, anything else is looked up.
pub fn get_sock_addr(host: Option<&str>, port: u16) -> Option<SocketAddrV4> {
    let ip = match host {
        None => Ipv4Addr::UNSPECIFIED,
        Some(h) => match h.parse::<Ipv4Addr>() {
            Ok(ip) => ip,
            Err(_) => ns_core::dns::addrs_by_host(h)
                .iter()
                .find_map(|a| a.parse::<Ipv4Addr>().ok())?,
        },
    };
    Some(SocketAddrV4::new(ip, port))
}

fn sockaddr(sa: SocketAddrV4) -> libc::sockaddr_in {
    // SAFETY: sockaddr_in is plain old data; all-zero is a valid value.
    let mut s: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    s.sin_family = libc::AF_INET as libc::sa_family_t;
    s.sin_port = sa.port().to_be();
    s.sin_addr.s_addr = u32::from(*sa.ip()).to_be();
    s
}

const SALEN: libc::socklen_t = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;

/// A new TCP socket bound to `sa` (`Ns_SockBind`); `SO_REUSEADDR` is set
/// when the port is not 0.
pub fn bind(sa: SocketAddrV4) -> io::Result<RawFd> {
    // SAFETY: plain socket(2) call.
    let fd = setup(cvt(unsafe {
        libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0)
    })?);
    if sa.port() != 0 {
        let one: libc::c_int = 1;
        // SAFETY: fd is ours; the option value is a live c_int.
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                (&raw const one).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
    let s = sockaddr(sa);
    // SAFETY: s is a valid sockaddr_in of SALEN bytes.
    if let Err(e) = cvt(unsafe { libc::bind(fd, (&raw const s).cast(), SALEN) }) {
        close(fd);
        return Err(e);
    }
    Ok(fd)
}

/// A listening socket on `host:port` (`Ns_SockListenEx`).
pub fn listen(host: Option<&str>, port: u16, backlog: u32) -> io::Result<RawFd> {
    let sa = get_sock_addr(host, port)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown host"))?;
    let fd = bind(sa)?;
    let backlog = libc::c_int::try_from(backlog).unwrap_or(libc::c_int::MAX);
    // SAFETY: fd is a bound socket we own.
    if let Err(e) = cvt(unsafe { libc::listen(fd, backlog) }) {
        close(fd);
        return Err(e);
    }
    Ok(fd)
}

/// Accept a connection (`Ns_SockAccept`).
pub fn accept(lsock: RawFd) -> io::Result<RawFd> {
    loop {
        // SAFETY: NULL address pointers are allowed by accept(2).
        match cvt(unsafe { libc::accept(lsock, std::ptr::null_mut(), std::ptr::null_mut()) }) {
            Ok(fd) => return Ok(setup(fd)),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// The local IPv4 address of a socket (`getsockname`).
pub fn local_addr(fd: RawFd) -> Option<Ipv4Addr> {
    // SAFETY: zeroed sockaddr_in is valid; len describes its size.
    let mut s: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut len = SALEN;
    // SAFETY: s and len are live and sized for getsockname.
    let rc = unsafe { libc::getsockname(fd, (&raw mut s).cast(), &raw mut len) };
    (rc == 0 && i32::from(s.sin_family) == libc::AF_INET)
        .then(|| Ipv4Addr::from(u32::from_be(s.sin_addr.s_addr)))
}

/// The local port of a socket.
pub fn local_port(fd: RawFd) -> Option<u16> {
    // SAFETY: as in local_addr.
    let mut s: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut len = SALEN;
    // SAFETY: s and len are live and sized for getsockname.
    let rc = unsafe { libc::getsockname(fd, (&raw mut s).cast(), &raw mut len) };
    (rc == 0).then(|| u16::from_be(s.sin_port))
}

/// Set or clear `O_NONBLOCK` (`Ns_SockSetBlocking`/`Ns_SockSetNonBlocking`).
pub fn set_blocking(fd: RawFd, blocking: bool) -> io::Result<()> {
    // SAFETY: fcntl on a caller-owned descriptor.
    let flags = cvt(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
    let flags = if blocking {
        flags & !libc::O_NONBLOCK
    } else {
        flags | libc::O_NONBLOCK
    };
    // SAFETY: as above.
    cvt(unsafe { libc::fcntl(fd, libc::F_SETFL, flags) }).map(|_| ())
}

/// Connect to `host:port` from `lhost:lport` (`SockConnect`).  With
/// `async_` the connect is started non-blocking and the socket is set
/// back to blocking mode before it is returned, connect still in progress.
pub fn connect(
    host: &str,
    port: u16,
    lhost: Option<&str>,
    lport: u16,
    async_: bool,
) -> io::Result<RawFd> {
    let unknown = || io::Error::new(io::ErrorKind::NotFound, "unknown host");
    let sa = get_sock_addr(Some(host), port).ok_or_else(unknown)?;
    let lsa = get_sock_addr(lhost, lport).ok_or_else(unknown)?;
    let fd = bind(lsa)?;
    if async_ {
        set_blocking(fd, false)?;
    }
    let s = sockaddr(sa);
    // SAFETY: s is a valid sockaddr_in of SALEN bytes.
    if let Err(e) = cvt(unsafe { libc::connect(fd, (&raw const s).cast(), SALEN) }) {
        let pending = matches!(
            e.raw_os_error(),
            Some(libc::EINPROGRESS | libc::EWOULDBLOCK)
        );
        if !async_ || !pending {
            close(fd);
            return Err(e);
        }
    }
    if async_ {
        set_blocking(fd, true)?;
    }
    Ok(fd)
}

/// Connect, waiting at most `timeout` for the connection
/// (`Ns_SockTimedConnect2`).
pub fn timed_connect(
    host: &str,
    port: u16,
    lhost: Option<&str>,
    lport: u16,
    timeout: Duration,
) -> io::Result<RawFd> {
    let fd = connect(host, port, lhost, lport, true)?;
    if !wait(fd, libc::POLLOUT, Some(timeout)) {
        close(fd);
        return Err(io::Error::from_raw_os_error(libc::ETIMEDOUT));
    }
    let mut err: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: err/len are live and sized for SO_ERROR.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            (&raw mut err).cast(),
            &raw mut len,
        )
    };
    if rc != 0 || err != 0 {
        close(fd);
        return Err(io::Error::from_raw_os_error(if rc == 0 {
            err
        } else {
            libc::EIO
        }));
    }
    Ok(fd)
}

/// A connected pair of Unix-domain stream sockets (`ns_sockpair`).
pub fn pair() -> io::Result<[RawFd; 2]> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: fds has room for the two descriptors.
    cvt(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) })?;
    Ok([setup(fds[0]), setup(fds[1])])
}

/// Duplicate a descriptor (`ns_sockdup`), close-on-exec.
pub fn dup(fd: RawFd) -> io::Result<RawFd> {
    // SAFETY: fcntl on a caller-owned descriptor.
    cvt(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) })
}

/// `poll(2)`, retried on `EINTR`; `None` waits forever.  Returns the number
/// of ready descriptors.
pub fn poll(fds: &mut [libc::pollfd], timeout: Option<Duration>) -> io::Result<usize> {
    let ms = timeout.map_or(-1, |t| {
        let ms = t.as_micros().div_ceil(1000);
        libc::c_int::try_from(ms).unwrap_or(libc::c_int::MAX)
    });
    loop {
        // SAFETY: fds is a live slice of pollfd structures.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
        match cvt(rc) {
            Ok(n) => return Ok(n as usize),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// Wait for `events` on `fd` (`Ns_SockWaitEx`).  Returns whether the
/// socket became ready in time.
pub fn wait(fd: RawFd, events: libc::c_short, timeout: Option<Duration>) -> bool {
    let mut p = [libc::pollfd {
        fd,
        events,
        revents: 0,
    }];
    poll(&mut p, timeout).is_ok_and(|n| n > 0)
}

/// Bytes waiting to be read on a socket (`ioctl(FIONREAD)`).
pub fn nread(fd: RawFd) -> io::Result<usize> {
    let mut n: libc::c_int = 0;
    // SAFETY: FIONREAD writes one c_int into n.
    cvt(unsafe { libc::ioctl(fd, libc::FIONREAD, &raw mut n) })?;
    Ok(usize::try_from(n).unwrap_or(0))
}

/// Whether a zero-length `send` succeeds (`ns_sockcheck`).
pub fn check(fd: RawFd) -> bool {
    // SAFETY: a zero-length send reads no memory.
    unsafe { libc::send(fd, std::ptr::null(), 0, 0) == 0 }
}

/// Write one byte, retrying on `EINTR` (the callback trigger).
pub fn send_byte(fd: RawFd) -> io::Result<()> {
    loop {
        // SAFETY: the buffer is a live one-byte array.
        let rc = unsafe { libc::send(fd, [0u8].as_ptr().cast(), 1, 0) };
        if rc == 1 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Read one byte (draining the callback trigger).
pub fn recv_byte(fd: RawFd) -> io::Result<()> {
    let mut b = [0u8; 1];
    loop {
        // SAFETY: the buffer is a live one-byte array.
        let rc = unsafe { libc::recv(fd, b.as_mut_ptr().cast(), 1, 0) };
        if rc == 1 {
            return Ok(());
        }
        let e = if rc == 0 {
            io::Error::from(io::ErrorKind::UnexpectedEof)
        } else {
            io::Error::last_os_error()
        };
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(
            get_sock_addr(None, 80),
            Some(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 80))
        );
        assert_eq!(
            get_sock_addr(Some("127.0.0.1"), 1),
            Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1))
        );
        assert!(get_sock_addr(Some("localhost"), 1).is_some());
        assert_eq!(get_sock_addr(Some("no.such.host.invalid"), 1), None);
        assert_eq!(
            posix_error(&io::Error::from_raw_os_error(libc::ECONNREFUSED)),
            "connection refused"
        );
    }

    #[test]
    fn listen_connect_accept() {
        let l = listen(Some("127.0.0.1"), 0, 5).expect("listen");
        let port = local_port(l).expect("port");
        assert_ne!(port, 0);
        let c = connect("127.0.0.1", port, None, 0, false).expect("connect");
        let a = accept(l).expect("accept");
        assert_eq!(local_addr(a), Some(Ipv4Addr::LOCALHOST));
        assert!(check(c));
        send_byte(c).expect("send");
        assert!(wait(a, libc::POLLIN, Some(Duration::from_secs(5))));
        assert_eq!(nread(a).expect("nread"), 1);
        recv_byte(a).expect("recv");
        let t = timed_connect(
            "127.0.0.1",
            port,
            Some("127.0.0.1"),
            0,
            Duration::from_secs(5),
        )
        .expect("timed");
        let d = dup(t).expect("dup");
        set_blocking(d, false).expect("nonblocking");
        assert!(recv_byte(d).is_err(), "nothing to read");
        set_blocking(d, true).expect("blocking");
        for fd in [l, c, a, t, d] {
            close(fd);
        }
        assert!(connect("127.0.0.1", port, None, 0, false).is_err());
        assert!(timed_connect("127.0.0.1", port, None, 0, Duration::from_secs(5)).is_err());
        assert!(connect("no.such.host.invalid", 1, None, 0, false).is_err());
        assert!(listen(Some("no.such.host.invalid"), 0, 5).is_err());
        assert!(accept(-1).is_err());
        assert!(nread(-1).is_err());
        assert!(set_blocking(-1, true).is_err());
        assert!(dup(-1).is_err());
        assert!(send_byte(-1).is_err());
        assert!(!check(-1));
        assert_eq!(local_addr(-1), None);
        assert_eq!(local_port(-1), None);
        close(-1);
    }

    #[test]
    fn pairs() {
        let [a, b] = pair().expect("pair");
        assert!(!wait(a, libc::POLLIN, Some(Duration::from_millis(10))));
        send_byte(b).expect("send");
        recv_byte(a).expect("recv");
        close(b);
        assert!(recv_byte(a).is_err(), "eof");
        close(a);
        let busy = listen(Some("127.0.0.1"), 0, 5).expect("listen");
        let port = local_port(busy).expect("port");
        let sa = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
        assert!(bind(sa).is_err(), "in use");
        close(busy);
    }
}
