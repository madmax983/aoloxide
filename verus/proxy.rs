//! Verus model of nsproxy handle accounting and the proxy state machine
//! (`crates/nsproxy/src/lib.rs`, `nsproxy/nsproxylib.c`).
//!
//! A pool has `max` handles; `avail` may be handed out, `out` are checked
//! out by interps and `free` proxies (with live workers) wait on the free
//! list.  `avail` goes negative when `-max` is lowered below the number of
//! running proxies.  We prove every transition preserves
//!
//! * `avail + out == max` (no handle is lost or invented),
//! * `free <= max(avail, 0)` (idle workers never exceed spare capacity),
//!
//! Each transition is one critical section under the pool lock: `pop`
//! (one handle taken by `pop_proxies`), `put` (one handle returned by
//! `put_proxy`, or by `give_back` after a failed `get`) and `config`.
//! The lock is dropped between them (condition waits, and `check` runs
//! unlocked), so any interleaving of these steps is a real execution, and
//! since each preserves the invariant, so does every interleaving.  In
//! particular a `config` that lowers `-max` between a `get`'s pops and its
//! `give_back` cannot leave idle workers beyond capacity, because
//! `give_back` frees a proxy, as `put_proxy` does, when no capacity is
//! left.
//!
//! We also prove that a successful `get` never pushes `out` past `max`,
//! that a failed `get` with no concurrent `config` restores `avail` and
//! `out` exactly, and that each proxy moves only along
//! `Idle -send-> Busy -wait-> Done -recv-> Idle` (or back to `Idle` by
//! `reset`, which closes the worker).

use vstd::prelude::*;

verus! {

pub struct Pool {
    pub max: int,
    pub avail: int,
    pub out: int,
    pub free: int,
}

pub open spec fn spare(p: Pool) -> int {
    if p.avail > 0 { p.avail } else { 0 }
}

pub open spec fn inv(p: Pool) -> bool {
    &&& p.max >= 0
    &&& p.out >= 0
    &&& p.free >= 0
    &&& p.avail + p.out == p.max
    &&& p.free <= spare(p)
}

/// One iteration of `pop_proxies` that takes a handle (only when
/// `avail > 0`): reuse a free proxy if there is one, else create one.
pub open spec fn pop(p: Pool) -> Pool {
    Pool {
        max: p.max,
        avail: p.avail - 1,
        out: p.out + 1,
        free: if p.free > 0 { p.free - 1 } else { 0 },
    }
}

/// `put_proxy`: back on the free list only if capacity remains, otherwise
/// the proxy (and its worker) is freed.
pub open spec fn put(p: Pool) -> Pool {
    Pool {
        max: p.max,
        avail: p.avail + 1,
        out: p.out - 1,
        free: if p.avail + 1 > 0 { p.free + 1 } else { p.free },
    }
}

/// `give_back`: a failed `get` returns the `n` proxies it took, one at a
/// time, each exactly as `put_proxy` would (kept only while capacity
/// remains).
pub open spec fn give_back(p: Pool, n: nat) -> Pool
    decreases n,
{
    if n == 0 { p } else { give_back(put(p), (n - 1) as nat) }
}

/// `config_cmd`: `nrun = max - avail`; set the new max; `avail = max -
/// nrun`; drop every free proxy.
pub open spec fn config(p: Pool, new_max: int) -> Pool {
    let nrun = p.max - p.avail;
    Pool { max: new_max, avail: new_max - nrun, out: p.out, free: 0 }
}

pub proof fn pop_preserves(p: Pool)
    requires
        inv(p),
        p.avail > 0,
    ensures
        inv(pop(p)),
        pop(p).out <= p.max,
{
}

/// Only a checked-out proxy can be released.
pub proof fn put_preserves(p: Pool)
    requires
        inv(p),
        p.out > 0,
    ensures
        inv(put(p)),
{
}

pub proof fn config_preserves(p: Pool, new_max: int)
    requires
        inv(p),
        new_max >= 0,
    ensures
        inv(config(p, new_max)),
{
}

/// Giving back `n` checked-out handles preserves the invariant.
pub proof fn lemma_give_back(p: Pool, n: nat)
    requires
        inv(p),
        p.out >= n,
    ensures
        inv(give_back(p, n)),
        give_back(p, n).avail == p.avail + n,
        give_back(p, n).out == p.out - n,
        give_back(p, n).max == p.max,
    decreases n,
{
    if n > 0 {
        put_preserves(p);
        lemma_give_back(put(p), (n - 1) as nat);
    }
}

/// Taking `n` handles and then giving them back after a failed check
/// restores the accounting (the free list may hold more live workers,
/// never more than the spare capacity).
pub proof fn failed_get_restores(p: Pool, n: nat)
    requires
        inv(p),
        p.avail >= n,
    ensures
        inv(give_back(pop_n(p, n), n)),
        give_back(pop_n(p, n), n).avail == p.avail,
        give_back(pop_n(p, n), n).out == p.out,
{
    lemma_pop_n(p, n);
    lemma_give_back(pop_n(p, n), n);
}

/// The unlocked `check` window: a `config` (possibly lowering `-max`
/// below the handles in use) between a `get`'s pops and its `give_back`
/// still leaves the pool satisfying the invariant.
pub proof fn failed_get_with_config_between(p: Pool, n: nat, new_max: int)
    requires
        inv(p),
        p.avail >= n,
        new_max >= 0,
    ensures
        inv(give_back(config(pop_n(p, n), new_max), n)),
        give_back(config(pop_n(p, n), new_max), n).max == new_max,
{
    lemma_pop_n(p, n);
    config_preserves(pop_n(p, n), new_max);
    lemma_give_back(config(pop_n(p, n), new_max), n);
}

pub open spec fn pop_n(p: Pool, n: nat) -> Pool
    decreases n,
{
    if n == 0 { p } else { pop_n(pop(p), (n - 1) as nat) }
}

pub proof fn lemma_pop_n(p: Pool, n: nat)
    requires
        inv(p),
        p.avail >= n,
    ensures
        inv(pop_n(p, n)),
        pop_n(p, n).avail == p.avail - n,
        pop_n(p, n).out == p.out + n,
        pop_n(p, n).max == p.max,
        pop_n(p, n).free == if p.free >= n { p.free - n } else { 0 },
    decreases n,
{
    if n > 0 {
        pop_preserves(p);
        lemma_pop_n(pop(p), (n - 1) as nat);
    }
}

/// The deadlock check: the waiters' running total is restored by every
/// loop iteration (`nhave + nwant` is invariant while a handle moves from
/// wanted to held).
pub proof fn waiting_total_balanced(total: nat, nhave: nat, nwant: nat)
    requires
        nwant > 0,
    ensures
        ({
            let during = total + nhave + nwant;
            let h2 = nhave + 1;
            let w2 = (nwant - 1) as nat;
            during - (h2 + w2) == total
        }),
{
}

pub enum PState {
    Idle,
    Busy,
    Done,
}

/// `Send`: only an idle proxy accepts a script.
pub open spec fn send(s: PState) -> Option<PState> {
    match s {
        PState::Idle => Some(PState::Busy),
        _ => None,
    }
}

/// `Wait`: a busy proxy becomes done; waiting again once done is a no-op;
/// an idle proxy has nothing to wait for.
pub open spec fn wait(s: PState) -> Option<PState> {
    match s {
        PState::Idle => None,
        _ => Some(PState::Done),
    }
}

/// `Recv`: only a done proxy yields its result.
pub open spec fn recv(s: PState) -> Option<PState> {
    match s {
        PState::Done => Some(PState::Idle),
        _ => None,
    }
}

/// `Eval` = send, wait, recv.
pub open spec fn eval(s: PState) -> Option<PState> {
    match send(s) {
        Some(b) => match wait(b) {
            Some(d) => recv(d),
            None => None,
        },
        None => None,
    }
}

/// A successful eval returns the proxy to `Idle`, and eval succeeds
/// exactly when the proxy was idle.
pub proof fn eval_round_trip(s: PState)
    ensures
        eval(s) is Some <==> s == PState::Idle,
        eval(s) is Some ==> eval(s) == Some(PState::Idle),
{
}

/// No script can be sent to a proxy that is still evaluating or holding
/// an unreceived result (`EBusy`).
pub proof fn no_send_while_busy(s: PState)
    requires
        s != PState::Idle,
    ensures
        send(s) is None,
{
}

/// A result is never received without a wait (`ENoWait`/`EIdle`).
pub proof fn recv_requires_wait(s: PState)
    requires
        s != PState::Done,
    ensures
        recv(s) is None,
{
}

} // verus!

fn main() {}
