//! Verus model of the socket callback registry
//! (`crates/nsd/src/sockcallback.rs`, `nsd/sockcallback.c`).
//!
//! `Ns_SockCallback` queues a registration under the lock; the `-socks-`
//! thread takes the whole queue and the shutdown flag in one critical
//! section, moves the registrations into its table, sweeps entries with no
//! I/O condition left, and stops if shutdown was pending.  Otherwise it
//! polls and runs ready callbacks; one that returns false gets
//! `when = 0` and is swept on the next pass.  At stop it runs every exit
//! callback and empties the table.
//!
//! A cancel or a new registration for the same descriptor can remove an
//! entry that is still awaiting the sweep (the thread applies the queue
//! before it sweeps).  If the thread cannot be started, the registrations
//! queued so far are dropped and the registry is stopped again.
//!
//! Counts: `queued` (on the queue), `active` (in the table with an I/O
//! condition), `done` (in the table with no I/O condition: `when == 0`
//! after returning false, or an exit-only registration; awaiting the
//! sweep) and `released` (cancelled, replaced, swept, exited or dropped
//! when the thread failed to start; a swept exit-only registration is
//! leaked, as in AOLserver, but has left the registry).  We prove every
//! transition preserves
//!
//! * `queued + active + done + released == accepted` (no registration is
//!   lost or invented),
//! * `!running ==> queued == 0 && active == 0 && done == 0` (a stopped
//!   registry holds nothing),
//!
//! that a registration is refused once shutdown is pending, that the
//! queue therefore stays empty after the thread's final take (so `stop`
//! releases every accepted callback), and that each callback moves only
//! along `Queued -> Active -> (Done ->) Released` and is never called
//! after it returned false or was released.

use vstd::prelude::*;

verus! {

pub struct Reg {
    pub accepted: int,
    pub queued: int,
    pub active: int,
    pub done: int,
    pub released: int,
    pub pending: bool,
    pub running: bool,
}

pub open spec fn inv(r: Reg) -> bool {
    &&& r.queued >= 0
    &&& r.active >= 0
    &&& r.done >= 0
    &&& r.released >= 0
    &&& r.queued + r.active + r.done + r.released == r.accepted
    &&& (!r.running ==> r.queued == 0 && r.active == 0 && r.done == 0)
}

/// `QueueSock`: refused while shutdown is pending; otherwise queued, and
/// the thread is started if it was not running.
pub open spec fn register(r: Reg) -> Reg {
    if r.pending {
        r
    } else {
        Reg {
            accepted: r.accepted + 1,
            queued: r.queued + 1,
            active: r.active,
            done: r.done,
            released: r.released,
            pending: r.pending,
            running: true,
        }
    }
}

/// The thread's take of the queue (`apply`): `x` of the registrations
/// have no I/O condition (exit-only) and count as done.
pub open spec fn drain(r: Reg, x: int) -> Reg {
    Reg {
        accepted: r.accepted,
        queued: 0,
        active: r.active + r.queued - x,
        done: r.done + x,
        released: r.released,
        pending: r.pending,
        running: r.running,
    }
}

/// A ready callback returned false: `when = 0`.
pub open spec fn fire_false(r: Reg) -> Reg {
    Reg {
        accepted: r.accepted,
        queued: r.queued,
        active: r.active - 1,
        done: r.done + 1,
        released: r.released,
        pending: r.pending,
        running: r.running,
    }
}

/// A cancel, or a new registration replacing the entry for its
/// descriptor: the old entry leaves the table.
pub open spec fn remove(r: Reg) -> Reg {
    Reg {
        accepted: r.accepted,
        queued: r.queued,
        active: r.active - 1,
        done: r.done,
        released: r.released + 1,
        pending: r.pending,
        running: r.running,
    }
}

/// A cancel, or a new registration for its descriptor, removes an entry
/// that is awaiting the sweep.
pub open spec fn remove_done(r: Reg) -> Reg {
    Reg {
        accepted: r.accepted,
        queued: r.queued,
        active: r.active,
        done: r.done - 1,
        released: r.released + 1,
        pending: r.pending,
        running: r.running,
    }
}

/// The thread could not be started: the registrations queued since the
/// registry was started (the table is still empty) are dropped and the
/// registry is stopped.
pub open spec fn abandon(r: Reg) -> Reg {
    Reg {
        accepted: r.accepted,
        queued: 0,
        active: r.active,
        done: r.done,
        released: r.released + r.queued,
        pending: r.pending,
        running: false,
    }
}

/// The sweep after each take.
pub open spec fn sweep(r: Reg) -> Reg {
    Reg {
        accepted: r.accepted,
        queued: r.queued,
        active: r.active,
        done: 0,
        released: r.released + r.done,
        pending: r.pending,
        running: r.running,
    }
}

/// `NsStartSockShutdown`: only a running registry is told to stop.
pub open spec fn start_shutdown(r: Reg) -> Reg {
    if r.running {
        Reg {
            accepted: r.accepted,
            queued: r.queued,
            active: r.active,
            done: r.done,
            released: r.released,
            pending: true,
            running: r.running,
        }
    } else {
        r
    }
}

/// The thread's exit: every exit callback runs, the table is emptied.
pub open spec fn stop(r: Reg) -> Reg {
    Reg {
        accepted: r.accepted,
        queued: r.queued,
        active: 0,
        done: 0,
        released: r.released + r.active + r.done,
        pending: r.pending,
        running: false,
    }
}

pub proof fn register_preserves(r: Reg)
    requires
        inv(r),
    ensures
        inv(register(r)),
        r.pending ==> register(r) == r,
        !r.pending ==> register(r).accepted == r.accepted + 1,
{
}

pub proof fn drain_preserves(r: Reg, x: int)
    requires
        inv(r),
        r.running,
        0 <= x,
        x <= r.queued,
    ensures
        inv(drain(r, x)),
        drain(r, x).queued == 0,
{
}

pub proof fn fire_false_preserves(r: Reg)
    requires
        inv(r),
        r.active > 0,
    ensures
        inv(fire_false(r)),
{
}

pub proof fn remove_preserves(r: Reg)
    requires
        inv(r),
        r.active > 0,
    ensures
        inv(remove(r)),
{
}

pub proof fn remove_done_preserves(r: Reg)
    requires
        inv(r),
        r.done > 0,
    ensures
        inv(remove_done(r)),
{
}

pub proof fn abandon_preserves(r: Reg)
    requires
        inv(r),
        r.active == 0,
        r.done == 0,
    ensures
        inv(abandon(r)),
        abandon(r).released == r.accepted,
        !abandon(r).running,
{
}

pub proof fn sweep_preserves(r: Reg)
    requires
        inv(r),
    ensures
        inv(sweep(r)),
        sweep(r).done == 0,
{
}

pub proof fn start_shutdown_preserves(r: Reg)
    requires
        inv(r),
    ensures
        inv(start_shutdown(r)),
{
}

/// Once shutdown is pending and the thread has taken the queue, later
/// registrations are refused, so the queue is still empty when the
/// thread stops.
pub proof fn queue_stays_empty(r: Reg)
    requires
        inv(r),
        r.pending,
        r.queued == 0,
    ensures
        register(r).queued == 0,
        register(r).accepted == r.accepted,
{
}

/// Stopping after the final take releases every accepted callback.
pub proof fn stop_releases_all(r: Reg)
    requires
        inv(r),
        r.running,
        r.pending,
        r.queued == 0,
    ensures
        inv(stop(r)),
        stop(r).released == r.accepted,
        !stop(r).running,
{
}

/// The life of one callback.
pub enum St {
    Queued,
    Active,
    Done,
    Released,
}

pub enum Ev {
    /// The thread takes the queue.
    Drain,
    /// Ready, and the callback returned true.
    FireKeep,
    /// Ready, and the callback returned false.
    FireStop,
    /// Cancelled or replaced.
    Remove,
    /// The sweep after a take.
    Sweep,
    /// The thread stops.
    Exit,
    /// The thread could not be started.
    Abandon,
}

pub open spec fn step(s: St, e: Ev) -> St {
    match s {
        St::Queued => match e {
            Ev::Drain => St::Active,
            Ev::Abandon => St::Released,
            _ => St::Queued,
        },
        St::Active => match e {
            Ev::FireStop => St::Done,
            Ev::Remove => St::Released,
            Ev::Exit => St::Released,
            _ => St::Active,
        },
        St::Done => match e {
            Ev::Remove => St::Released,
            Ev::Sweep => St::Released,
            Ev::Exit => St::Released,
            _ => St::Done,
        },
        St::Released => St::Released,
    }
}

/// Whether the event invokes the callback: only an active callback is
/// fired, or told about the exit.
pub open spec fn calls(s: St, e: Ev) -> bool {
    match s {
        St::Active => match e {
            Ev::FireKeep => true,
            Ev::FireStop => true,
            Ev::Exit => true,
            _ => false,
        },
        _ => false,
    }
}

/// A callback that returned false is never called again, and only
/// leaves the table.
pub proof fn done_is_final(e: Ev)
    ensures
        !calls(St::Done, e),
        step(St::Done, e) == St::Done || step(St::Done, e) == St::Released,
{
}

/// A released callback is never called and never comes back.
pub proof fn released_is_final(e: Ev)
    ensures
        !calls(St::Released, e),
        step(St::Released, e) == St::Released,
{
}

/// A queued callback is not called before the thread takes it.
pub proof fn queued_not_called(e: Ev)
    ensures
        !calls(St::Queued, e),
        step(St::Queued, e) == St::Queued || step(St::Queued, e) == St::Active
            || step(St::Queued, e) == St::Released,
{
}

/// An active callback only stays active, finishes or is released.
pub proof fn active_steps(e: Ev)
    ensures
        step(St::Active, e) != St::Queued,
{
}

} // verus!

fn main() {}
