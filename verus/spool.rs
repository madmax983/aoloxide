//! Verus model of request-body read-ahead accounting
//! (`crates/nsd/src/driver.rs`; `SockReadLine`/`SockReadContent` in
//! `nsd/driver.c`).
//!
//! A request with `Content-Length: len` (`len = 0` when the header is
//! absent, as in `SockReadLine`) is admitted only when
//! `len <= maxupload` (the `ns_limits` cap).  Its body is buffered in
//! memory when the whole request (`head` bytes of request line and
//! headers, the body, and room for a trailing CRLF) fits under the driver's
//! `maxinput`; otherwise it is spooled to a temp file.  Each receive takes
//! at most the bytes still owed; the rest of the packet stays in the
//! read-ahead buffer for the next (pipelined) request.  We prove
//!
//! * `mem + disk == read <= len <= maxupload`;
//! * one sink only: a spooled body buffers nothing in memory, and an
//!   in-memory body keeps `head + mem + 2 < maxinput`;
//! * a complete read holds exactly `len` bytes in the chosen sink, and
//!   receiving never over-reads, however a packet is split.
//!
//! Chunked bodies (no `Content-Length`, an aoloxide extension) start in
//! memory and spill to the temp file when the next chunk would reach
//! `maxinput`; the same bounds hold for them.

use vstd::prelude::*;

verus! {

/// Read-ahead state of a `Content-Length` body.
pub struct ReadAhead {
    pub head: nat,
    pub len: nat,
    pub maxinput: nat,
    pub maxupload: nat,
    pub spool: bool,
    pub mem: nat,
    pub disk: nat,
}

pub open spec fn read(r: ReadAhead) -> nat {
    r.mem + r.disk
}

/// `max = roff + contentLength + 2; if (max >= maxinput) spool`.
pub open spec fn spools(head: nat, len: nat, maxinput: nat) -> bool {
    head + len + 2 >= maxinput
}

pub open spec fn inv(r: ReadAhead) -> bool {
    &&& r.mem + r.disk <= r.len
    &&& r.len <= r.maxupload
    &&& r.spool == spools(r.head, r.len, r.maxinput)
    &&& r.spool ==> r.mem == 0
    &&& !r.spool ==> r.disk == 0
}

/// The headers are read: admit and choose the sink.
pub open spec fn start(head: nat, len: nat, maxinput: nat, maxupload: nat) -> ReadAhead {
    ReadAhead {
        head: head,
        len: len,
        maxinput: maxinput,
        maxupload: maxupload,
        spool: spools(head, len, maxinput),
        mem: 0,
        disk: 0,
    }
}

/// Store `n` body bytes in the chosen sink.
pub open spec fn recv(r: ReadAhead, n: nat) -> ReadAhead {
    if r.spool {
        ReadAhead {
            head: r.head,
            len: r.len,
            maxinput: r.maxinput,
            maxupload: r.maxupload,
            spool: r.spool,
            mem: r.mem,
            disk: r.disk + n,
        }
    } else {
        ReadAhead {
            head: r.head,
            len: r.len,
            maxinput: r.maxinput,
            maxupload: r.maxupload,
            spool: r.spool,
            mem: r.mem + n,
            disk: r.disk,
        }
    }
}

/// The body bytes taken from a packet of `got` bytes: no more than owed.
pub open spec fn take(r: ReadAhead, got: nat) -> nat {
    if r.mem + r.disk + got <= r.len {
        got
    } else {
        (r.len - (r.mem + r.disk)) as nat
    }
}

pub proof fn start_establishes(head: nat, len: nat, maxinput: nat, maxupload: nat)
    requires
        len <= maxupload,
    ensures
        inv(start(head, len, maxinput, maxupload)),
        read(start(head, len, maxinput, maxupload)) == 0,
{
}

pub proof fn recv_preserves(r: ReadAhead, n: nat)
    requires
        inv(r),
        r.mem + r.disk + n <= r.len,
    ensures
        inv(recv(r, n)),
        read(recv(r, n)) == read(r) + n,
{
}

/// Taking from any packet keeps the invariant and never over-reads.
pub proof fn take_preserves(r: ReadAhead, got: nat)
    requires
        inv(r),
    ensures
        take(r, got) <= got,
        r.mem + r.disk + take(r, got) <= r.len,
        inv(recv(r, take(r, got))),
{
}

/// A packet that holds everything owed completes the body.
pub proof fn take_completes(r: ReadAhead, got: nat)
    requires
        inv(r),
        r.mem + r.disk + got >= r.len,
    ensures
        read(recv(r, take(r, got))) == r.len,
{
}

/// An in-memory body fits, with its request, under `maxinput`.
pub proof fn memory_fits(r: ReadAhead)
    requires
        inv(r),
        !r.spool,
    ensures
        r.head + r.mem + 2 < r.maxinput,
{
}

/// A complete body is wholly in the chosen sink.
pub proof fn complete_in_sink(r: ReadAhead)
    requires
        inv(r),
        read(r) == r.len,
    ensures
        r.spool ==> r.disk == r.len,
        !r.spool ==> r.mem == r.len,
{
}

/// Read-ahead state of a chunked body.
pub struct Chunked {
    pub head: nat,
    pub maxinput: nat,
    pub maxupload: nat,
    pub spool: bool,
    pub mem: nat,
    pub disk: nat,
}

pub open spec fn cinv(c: Chunked) -> bool {
    &&& c.mem + c.disk <= c.maxupload
    &&& c.spool ==> c.mem == 0
    &&& !c.spool ==> c.disk == 0
    &&& (!c.spool && c.mem > 0) ==> c.head + c.mem + 2 < c.maxinput
}

pub open spec fn cstart(head: nat, maxinput: nat, maxupload: nat) -> Chunked {
    Chunked { head: head, maxinput: maxinput, maxupload: maxupload, spool: false, mem: 0, disk: 0 }
}

/// Append a chunk of `size` bytes, spilling memory to the temp file when
/// the request would reach `maxinput`.
pub open spec fn chunk(c: Chunked, size: nat) -> Chunked {
    if c.spool {
        Chunked {
            head: c.head,
            maxinput: c.maxinput,
            maxupload: c.maxupload,
            spool: true,
            mem: c.mem,
            disk: c.disk + size,
        }
    } else if c.head + c.mem + size + 2 >= c.maxinput {
        Chunked {
            head: c.head,
            maxinput: c.maxinput,
            maxupload: c.maxupload,
            spool: true,
            mem: 0,
            disk: c.mem + size,
        }
    } else {
        Chunked {
            head: c.head,
            maxinput: c.maxinput,
            maxupload: c.maxupload,
            spool: false,
            mem: c.mem + size,
            disk: c.disk,
        }
    }
}

pub proof fn cstart_establishes(head: nat, maxinput: nat, maxupload: nat)
    ensures
        cinv(cstart(head, maxinput, maxupload)),
{
}

/// A chunk within `maxupload` keeps the bounds and loses no bytes.
pub proof fn chunk_preserves(c: Chunked, size: nat)
    requires
        cinv(c),
        c.mem + c.disk + size <= c.maxupload,
    ensures
        cinv(chunk(c, size)),
        chunk(c, size).mem + chunk(c, size).disk == c.mem + c.disk + size,
{
}

/// Once spooled, a chunked body stays spooled.
pub proof fn chunk_spool_sticks(c: Chunked, size: nat)
    requires
        c.spool,
    ensures
        chunk(c, size).spool,
{
}

} // verus!

fn main() {}
