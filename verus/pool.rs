//! Verus model of connection-pool thread accounting
//! (`crates/nsd/src/pool.rs`, `nsd/queue.c`).
//!
//! The pool is a state machine over `current` (live threads), `idle`
//! (threads waiting for work), `queue` (waiting work items), `min` and
//! `max`.  We prove every transition the implementation performs preserves
//!
//! * `idle <= current <= max`, and `1 <= max`, `min <= max`;
//!
//! and that enqueueing work when no thread is idle and the pool is below
//! `max` starts a thread (no work can be stranded while capacity remains).

use vstd::prelude::*;

verus! {

pub struct Pool {
    pub current: nat,
    pub idle: nat,
    pub queue: nat,
    pub min: nat,
    pub max: nat,
}

pub open spec fn inv(p: Pool) -> bool {
    &&& p.idle <= p.current
    &&& p.current <= p.max
    &&& 1 <= p.max
    &&& p.min <= p.max
}

/// `Pool::enqueue`: push work; start a thread if more work is queued than
/// threads are idle and the pool has room.
pub open spec fn enqueue(p: Pool) -> Pool {
    let q = p.queue + 1;
    if p.idle < q && p.current < p.max {
        Pool { current: p.current + 1, idle: p.idle, queue: q, min: p.min, max: p.max }
    } else {
        Pool { current: p.current, idle: p.idle, queue: q, min: p.min, max: p.max }
    }
}

/// A busy thread finds no work and starts waiting.
pub open spec fn go_idle(p: Pool) -> Pool {
    Pool { current: p.current, idle: p.idle + 1, queue: p.queue, min: p.min, max: p.max }
}

/// An idle thread wakes up.
pub open spec fn wake(p: Pool) -> Pool {
    Pool { current: p.current, idle: (p.idle - 1) as nat, queue: p.queue, min: p.min, max: p.max }
}

/// A busy thread takes a work item.
pub open spec fn take(p: Pool) -> Pool {
    Pool { current: p.current, idle: p.idle, queue: (p.queue - 1) as nat, min: p.min, max: p.max }
}

/// A busy thread exits (timeout above `min`, `maxconns`, or shutdown).
pub open spec fn exit(p: Pool) -> Pool {
    Pool { current: (p.current - 1) as nat, idle: p.idle, queue: p.queue, min: p.min, max: p.max }
}

/// `Pool::start`: spawn threads up to `min`.
pub open spec fn start(p: Pool) -> Pool {
    if p.current < p.min {
        Pool { current: p.min, idle: p.idle, queue: p.queue, min: p.min, max: p.max }
    } else {
        p
    }
}

pub proof fn enqueue_preserves(p: Pool)
    requires
        inv(p),
    ensures
        inv(enqueue(p)),
{
}

/// Only a busy thread (one not counted as idle) can go idle.
pub proof fn go_idle_preserves(p: Pool)
    requires
        inv(p),
        p.idle < p.current,
    ensures
        inv(go_idle(p)),
{
}

pub proof fn wake_preserves(p: Pool)
    requires
        inv(p),
        p.idle > 0,
    ensures
        inv(wake(p)),
{
}

pub proof fn take_preserves(p: Pool)
    requires
        inv(p),
        p.queue > 0,
    ensures
        inv(take(p)),
{
}

/// Only a busy thread exits, so idle threads are never miscounted.
pub proof fn exit_preserves(p: Pool)
    requires
        inv(p),
        p.idle < p.current,
    ensures
        inv(exit(p)),
{
}

pub proof fn start_preserves(p: Pool)
    requires
        inv(p),
    ensures
        inv(start(p)),
        start(p).current >= p.min,
{
}

/// No stranded work: with no idle thread and spare capacity, enqueue
/// always starts a thread.
pub proof fn enqueue_starts_thread_when_needed(p: Pool)
    requires
        inv(p),
        p.idle == 0,
        p.current < p.max,
    ensures
        enqueue(p).current == p.current + 1,
{
}

/// Capacity is never exceeded, however many items are enqueued.
pub proof fn enqueue_many_bounded(p: Pool, n: nat)
    requires
        inv(p),
    ensures
        inv(enqueue_n(p, n)),
    decreases n,
{
    if n > 0 {
        enqueue_preserves(p);
        enqueue_many_bounded(enqueue(p), (n - 1) as nat);
    }
}

pub open spec fn enqueue_n(p: Pool, n: nat) -> Pool
    decreases n,
{
    if n == 0 {
        p
    } else {
        enqueue_n(enqueue(p), (n - 1) as nat)
    }
}

} // verus!

fn main() {}
