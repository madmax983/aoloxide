//! Pre-queue I/O waits (`Ns_QueueWait` and `RunQueWaits` in
//! `nsd/driver.c`).
//!
//! A pre-queue filter may ask the connection to wait for a socket to
//! become readable or writable before the request runs.  In AOLserver the
//! driver thread polls those sockets (and the client socket, to notice a
//! dropped client) and runs each callback in the connection's interp; the
//! request is queued once no waits remain.  Pre-queue filters run at the
//! start of connection processing in this port (see `docs/porting.md`), so
//! the waits are polled there, in the connection thread, between the
//! pre-queue and read filters.

use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use crate::conn::ConnRef;
use crate::sockcallback::{SOCK_DROP, SOCK_READ, SOCK_WRITE};

/// A queue-wait callback (`Ns_QueueWaitProc`): called with the connection,
/// the descriptor and why: [`SOCK_READ`], [`SOCK_WRITE`], [`SOCK_DROP`] or
/// `0` for a timeout.
pub type QueueWaitProc = Box<dyn FnOnce(&ConnRef, RawFd, u32)>;

/// One pending wait (`struct QueWait`).
pub struct QueWait {
    fd: RawFd,
    events: libc::c_short,
    deadline: Instant,
    proc_: QueueWaitProc,
}

/// A connection's queue-wait state.
#[derive(Default)]
pub struct QueWaits {
    /// Whether waits may be registered: only while pre-queue filters and
    /// queue-wait callbacks run (AOLserver: in the driver thread).
    open: bool,
    /// Pending waits, most recently registered first.
    list: Vec<QueWait>,
}

impl std::fmt::Debug for QueWaits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueWaits")
            .field("open", &self.open)
            .field("pending", &self.list.len())
            .finish()
    }
}

/// `ns_quewait` outside the pre-queue phase (AOLserver's `E_QUEWAIT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("attempt to register quewait outside driver thread")]
pub struct QueueWaitError;

/// Allow or forbid registrations for `conn`.
pub fn set_open(conn: &ConnRef, open: bool) {
    conn.conn_mut().que_waits.open = open;
}

/// Arrange for `proc_` to run when `fd` reaches `when` ([`SOCK_READ`]
/// and/or [`SOCK_WRITE`]) or `timeout` passes (`Ns_QueueWait`).  Fails
/// (logging the error) outside the pre-queue phase.
pub fn queue_wait(
    conn: &ConnRef,
    fd: RawFd,
    proc_: QueueWaitProc,
    when: u32,
    timeout: Duration,
) -> Result<(), QueueWaitError> {
    let mut c = conn.conn_mut();
    if !c.que_waits.open {
        let id = c.id;
        drop(c);
        conn.server.global.log(
            ns_core::log::Severity::Error,
            &format!("conn[{id}]: {QueueWaitError}"),
        );
        return Err(QueueWaitError);
    }
    let mut events = 0;
    if when & SOCK_READ != 0 {
        events |= libc::POLLIN;
    }
    if when & SOCK_WRITE != 0 {
        events |= libc::POLLOUT;
    }
    c.que_waits.list.insert(
        0,
        QueWait {
            fd,
            events,
            deadline: Instant::now() + timeout,
            proc_,
        },
    );
    Ok(())
}

/// Poll and run queue-wait callbacks until none remain (the driver's
/// `SOCK_QUEWAIT` state).  Returns false if the client dropped: input on
/// the client socket while waiting counts as a drop, every pending
/// callback runs with [`SOCK_DROP`] and the request must not run.
pub fn run(conn: &ConnRef) -> bool {
    let client = conn.conn().sock.as_ref().and_then(|s| s.fd);
    loop {
        let waits = std::mem::take(&mut conn.conn_mut().que_waits.list);
        if waits.is_empty() {
            return true;
        }
        let mut pfds: Vec<libc::pollfd> = Vec::with_capacity(waits.len() + 1);
        if let Some(fd) = client {
            pfds.push(libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            });
        }
        let base = pfds.len();
        pfds.extend(waits.iter().map(|w| libc::pollfd {
            fd: w.fd,
            events: w.events,
            revents: 0,
        }));
        let now = Instant::now();
        let timeout = waits
            .iter()
            .map(|w| w.deadline.saturating_duration_since(now))
            .min();
        let _ = crate::sock::poll(&mut pfds, timeout);
        let now = Instant::now();
        let dropped = client.is_some() && pfds[0].revents & libc::POLLIN != 0;
        for (i, w) in waits.into_iter().enumerate() {
            let why = if dropped {
                SOCK_DROP
            } else {
                let rev = pfds[base + i].revents;
                let mut why = 0;
                if rev & libc::POLLIN != 0 {
                    why |= SOCK_READ;
                }
                if rev & libc::POLLOUT != 0 {
                    why |= SOCK_WRITE;
                }
                why
            };
            if why == 0 && w.deadline > now {
                conn.conn_mut().que_waits.list.insert(0, w);
            } else {
                (w.proc_)(conn, w.fd, why);
            }
        }
        if dropped {
            conn.conn_mut().que_waits.list.clear();
            return false;
        }
    }
}
