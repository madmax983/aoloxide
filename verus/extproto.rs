//! Verus model of the AOLserver external-driver protocol between nsext
//! (the client, `crates/nsext`, `nsext/nsext.c`) and a proxy daemon (the
//! server, `crates/nspd`, `nspd/main.c`).
//!
//! The protocol is strict request/response over one byte stream: nsext
//! sends `name\nlen\narg`, then reads the reply, which is a fixed number
//! of *items* (a status line, a plain line, or a size-prefixed list ended
//! by `-1`).  When the leading status is not `ok` the daemon sends nothing
//! more.  There is no framing that would let a reader resynchronise, so
//! the whole protocol depends on both sides agreeing, for every message,
//! on how many items a reply holds, and on the client never sending a
//! request while a reply is still unread.  We prove:
//!
//! * **agreement**: for every message nsext sends and either outcome of
//!   the status, the number of items the daemon writes
//!   (`daemon_items`) equals the number nsext reads (`client_items`);
//! * **sync**: the connection state machine `Ready -send-> Await(n)
//!   -recv*n-> Ready` returns to `Ready` after exactly the reply, a
//!   request can only be sent when `Ready`, and an I/O or protocol
//!   error moves to `Dead`, which is absorbing (the handle is cleaned up
//!   and nothing more is sent: `DbProxyCleanup`);
//! * a message that is never answered (`gettableinfo`, `tablelist`,
//!   `bestrowid`) is never sent by nsext;
//! * list framing: an element is read only for a size in
//!   `0..=maxElementSize`, and a list ends only at `-1`.
//!
//! The code follows the model through `nspd::msg::Msg::reply`, the
//! table of `client_items` (checked by `replies_mirror_the_verus_model`).
//! nsext's `ExtConn` keeps the unread items of the outstanding reply
//! (`Conn.left`): `send` takes them from `Msg::reply`, each read must be
//! the next of them, and a request sent before they are all read, or a
//! read the reply does not hold, drops the connection (`Dead`).  The
//! daemon's replies (`daemon_items`) are checked against the same table
//! for every message and both outcomes by nspd's
//! `every_reply_has_the_items_of_the_message_table`.

use vstd::prelude::*;

verus! {

// Message codes (`include/nsextmsg.h`): 0 exec, 1 bindrow, 2 getrow,
// 3 flush, 4 cancel, 5 gettableinfo, 6 tablelist, 7 bestrowid,
// 8 resultid, 9 resultrows, 10 setmaxrows, 11 close, 12 open, 13 ping,
// 14 identify, 15 traceon, 16 traceoff, 17 gettypes, 18 openfile,
// 19 closefile, 20 readfile, 21 writefile, 22 deletefile,
// 23 createtmpfile, 24 resethandle, 25 spstart, 26 spsetparam,
// 27 spexec, 28 spreturncode, 29 spgetparams.

pub open spec fn valid_code(code: int) -> bool {
    0 <= code && code <= 29
}

/// Messages nsext sends (every code except the three table queries).
pub open spec fn client_sends(code: int) -> bool {
    valid_code(code) && code != 5 && code != 6 && code != 7
}

/// Messages whose first reply item is a status that may cut the reply
/// short (`DbProxyCheckStatus`, or an explicit `strcasecmp(.., "ok")`).
pub open spec fn has_status(code: int) -> bool {
    code == 0 || code == 1 || code == 2 || code == 3 || code == 4
        || code == 11 || code == 12 || code == 18 || code == 19 || code == 20
        || code == 21 || code == 22 || code == 23 || code == 25 || code == 26
        || code == 27 || code == 29
}

/// Items the daemon writes for `code` (`DispatchCmd` and the driver's
/// `Ns_PdDb*` handlers): the status, then on `ok` the payload.  Messages
/// without a status always write one line.  Unanswered messages write
/// nothing.
pub open spec fn daemon_items(code: int, ok: bool) -> int {
    if code == 5 || code == 6 || code == 7 {
        0
    } else if !has_status(code) || !ok {
        1
    } else if code == 0 || code == 27 || code == 18 || code == 23 {
        2  // status, `exec_rows`/`exec_dml` (or the fd / file name)
    } else if code == 1 || code == 2 || code == 20 {
        2  // status, list
    } else if code == 29 {
        3  // status, names list, values list
    } else {
        1
    }
}

/// Items nsext reads after sending `code` (`ExtExec`, `ExtBindRow`,
/// `ExtGetRow`, `ExtSpGetParams`, `DbProxyGetPingReply`, ...).
pub open spec fn client_items(code: int, ok: bool) -> int {
    if !has_status(code) {
        1
    } else if !ok {
        1
    } else if code == 29 {
        3
    } else if code == 0 || code == 27 || code == 18 || code == 23
        || code == 1 || code == 2 || code == 20 {
        2
    } else {
        1
    }
}

/// Both sides agree on every reply nsext can provoke.
pub proof fn agreement(code: int, ok: bool)
    requires
        client_sends(code),
    ensures
        daemon_items(code, ok) == client_items(code, ok),
        client_items(code, ok) >= 1,
{
}

/// The unanswered table queries are exactly the messages nsext never
/// sends, so no read ever waits on a reply that cannot come.
pub proof fn unanswered_never_sent(code: int, ok: bool)
    requires
        valid_code(code),
        daemon_items(code, ok) == 0,
    ensures
        !client_sends(code),
{
}

pub enum Phase {
    Ready,
    Await,
    Dead,
}

pub struct Conn {
    pub phase: Phase,
    /// Reply items still to read while `Await`.
    pub left: int,
}

pub open spec fn inv(c: Conn) -> bool {
    match c.phase {
        Phase::Await => c.left >= 1,
        _ => c.left == 0,
    }
}

pub open spec fn ready() -> Conn {
    Conn { phase: Phase::Ready, left: 0 }
}

pub open spec fn dead() -> Conn {
    Conn { phase: Phase::Dead, left: 0 }
}

/// `DbProxySend`: only a ready connection sends; the reply's first item
/// is then awaited.  Sending on anything else is a protocol violation
/// that the implementation rules out (the result is `Dead`).
pub open spec fn send(c: Conn) -> Conn {
    match c.phase {
        Phase::Ready => Conn { phase: Phase::Await, left: 1 },
        _ => dead(),
    }
}

/// Read the first item of the reply to `code` (a status, or the only
/// line): on `ok` the rest of the reply is awaited.
pub open spec fn recv_first(c: Conn, code: int, ok: bool) -> Conn {
    match c.phase {
        Phase::Await => {
            let rest = client_items(code, ok) - 1;
            if rest > 0 { Conn { phase: Phase::Await, left: rest } } else { ready() }
        },
        _ => dead(),
    }
}

/// Read one further item.
pub open spec fn recv(c: Conn) -> Conn {
    match c.phase {
        Phase::Await => {
            if c.left > 1 {
                Conn { phase: Phase::Await, left: c.left - 1 }
            } else {
                ready()
            }
        },
        _ => dead(),
    }
}

pub open spec fn recv_n(c: Conn, n: nat) -> Conn
    decreases n,
{
    if n == 0 { c } else { recv_n(recv(c), (n - 1) as nat) }
}

/// A read or write that fails, times out or breaks the framing
/// (`DbProxyCleanup`).
pub open spec fn fail(_c: Conn) -> Conn {
    dead()
}

pub proof fn send_preserves(c: Conn)
    requires
        inv(c),
    ensures
        inv(send(c)),
{
}

pub proof fn recv_preserves(c: Conn)
    requires
        inv(c),
    ensures
        inv(recv(c)),
{
}

/// No request is sent while a reply is outstanding or after a failure.
pub proof fn no_send_unless_ready(c: Conn)
    requires
        c.phase != Phase::Ready,
    ensures
        send(c).phase == Phase::Dead,
{
}

/// A failed connection stays failed whatever happens next.
pub proof fn dead_is_absorbing(code: int, ok: bool)
    ensures
        send(dead()) == dead(),
        recv(dead()) == dead(),
        recv_first(dead(), code, ok) == dead(),
        fail(ready()) == dead(),
{
}

/// Reading `n` further items from `Await(n)` lands on `Ready`.
pub proof fn lemma_recv_n(left: int, n: nat)
    requires
        left >= 1,
        n == left,
    ensures
        recv_n(Conn { phase: Phase::Await, left: left }, n) == ready(),
    decreases n,
{
    let c = Conn { phase: Phase::Await, left: left };
    if n == 1 {
        assert(recv(c) == ready());
        assert(recv_n(recv(c), 0) == recv(c));
    } else {
        assert(recv(c) == Conn { phase: Phase::Await, left: left - 1 });
        lemma_recv_n(left - 1, (n - 1) as nat);
    }
}

/// A complete exchange: send, read the first item, then the rest of the
/// reply as the daemon writes it, and the connection is ready again.
pub proof fn exchange_round_trip(code: int, ok: bool)
    requires
        client_sends(code),
    ensures
        ({
            let a = recv_first(send(ready()), code, ok);
            let rest = daemon_items(code, ok) - 1;
            rest >= 0 && (rest == 0 ==> a == ready()) && (rest > 0 ==> recv_n(a, rest as nat)
                == ready())
        }),
{
    agreement(code, ok);
    let rest = client_items(code, ok) - 1;
    if rest > 0 {
        lemma_recv_n(rest, rest as nat);
    }
}

/// List framing (`DbProxyGetList`): what the size line `n` before each
/// element means, given `maxElementSize` `max`.  0: the `-1` terminator;
/// 1: an element of `n` bytes; 2: over the size limit (checked first, as
/// in AOLserver); 3: any other negative size (a protocol error).  Mirrored
/// by `size_verdict` in `crates/nsext/src/conn.rs`, which `get_list` uses.
pub open spec fn size_verdict(n: int, max: int) -> int {
    if n > max {
        2
    } else if n == -1 {
        0
    } else if n < 0 {
        3
    } else {
        1
    }
}

/// Every element read is a whole, non-negative size within the limit,
/// every such size is read, and the list ends only at `-1`.
pub proof fn list_framing(n: int, max: int)
    ensures
        size_verdict(n, max) == 1 ==> 0 <= n && n <= max,
        0 <= n && n <= max ==> size_verdict(n, max) == 1,
        size_verdict(n, max) == 0 ==> n == -1,
        n == -1 && max >= 0 ==> size_verdict(n, max) == 0,
        n < -1 ==> size_verdict(n, max) >= 2,
{
}

} // verus!

fn main() {}
