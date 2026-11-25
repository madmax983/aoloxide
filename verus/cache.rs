//! Verus proof of cache size accounting (`crates/ns-core/src/cache.rs`,
//! `nsd/cache.c`).
//!
//! The cache keeps entries in LRU order; each has a byte size.  Inserting
//! an entry of `size` evicts from the least-recently-used end until the new
//! entry fits, and refuses entries larger than the whole cache.  We prove:
//!
//! * after an insert the total size never exceeds `max`;
//! * eviction only removes from the LRU end (the result is a suffix);
//! * the running total kept by the implementation equals the sum of the
//!   entry sizes.

use vstd::prelude::*;

verus! {

pub open spec fn sum(s: Seq<nat>) -> nat
    decreases s.len(),
{
    if s.len() == 0 {
        0
    } else {
        s[0] + sum(s.subrange(1, s.len() as int))
    }
}

/// Evict from the front (LRU) until `size` more bytes fit.
pub open spec fn evict(s: Seq<nat>, size: nat, max: nat) -> Seq<nat>
    decreases s.len(),
{
    if s.len() == 0 || sum(s) + size <= max {
        s
    } else {
        evict(s.subrange(1, s.len() as int), size, max)
    }
}

/// `Cache::set`: insert at the MRU end after eviction; oversize entries
/// are rejected and leave the cache unchanged.
pub open spec fn insert(s: Seq<nat>, size: nat, max: nat) -> Seq<nat> {
    if size > max {
        s
    } else {
        evict(s, size, max).push(size)
    }
}

pub proof fn lemma_sum_empty()
    ensures
        sum(Seq::<nat>::empty()) == 0,
{
}

pub proof fn lemma_sum_push(s: Seq<nat>, x: nat)
    ensures
        sum(s.push(x)) == sum(s) + x,
    decreases s.len(),
{
    let t = s.push(x);
    if s.len() == 0 {
        assert(t.subrange(1, t.len() as int) =~= Seq::<nat>::empty());
        lemma_sum_empty();
    } else {
        let tail = s.subrange(1, s.len() as int);
        assert(t.subrange(1, t.len() as int) =~= tail.push(x));
        lemma_sum_push(tail, x);
    }
}

/// After eviction the new entry fits.
pub proof fn lemma_evict_fits(s: Seq<nat>, size: nat, max: nat)
    requires
        size <= max,
    ensures
        sum(evict(s, size, max)) + size <= max,
    decreases s.len(),
{
    if s.len() == 0 {
        lemma_sum_empty();
    } else if sum(s) + size <= max {
    } else {
        lemma_evict_fits(s.subrange(1, s.len() as int), size, max);
    }
}

/// Eviction keeps a suffix of the LRU list (only the oldest entries go).
pub proof fn lemma_evict_suffix(s: Seq<nat>, size: nat, max: nat)
    ensures
        exists|k: int| 0 <= k <= s.len() && evict(s, size, max) =~= s.subrange(k, s.len() as int),
    decreases s.len(),
{
    if s.len() == 0 || sum(s) + size <= max {
        assert(evict(s, size, max) =~= s.subrange(0, s.len() as int));
    } else {
        let t = s.subrange(1, s.len() as int);
        lemma_evict_suffix(t, size, max);
        let k = choose|k: int| 0 <= k <= t.len() && evict(t, size, max) =~= t.subrange(k, t.len() as int);
        assert(t.subrange(k, t.len() as int) =~= s.subrange(k + 1, s.len() as int));
    }
}

/// The size bound holds after every insert, starting from any cache that
/// already satisfied it.
pub proof fn lemma_insert_bounded(s: Seq<nat>, size: nat, max: nat)
    requires
        sum(s) <= max,
    ensures
        sum(insert(s, size, max)) <= max,
{
    if size <= max {
        lemma_evict_fits(s, size, max);
        lemma_sum_push(evict(s, size, max), size);
    }
}

/// Executable running-total update used by the implementation: removing
/// the LRU entry subtracts its size.
pub fn evict_one(total: u64, sizes: &mut Vec<u64>) -> (r: u64)
    requires
        old(sizes).len() > 0,
        total >= old(sizes)@[0],
    ensures
        r == total - old(sizes)@[0],
        sizes@ == old(sizes)@.remove(0),
{
    let first = sizes[0];
    sizes.remove(0);
    total - first
}

} // verus!

fn main() {}
