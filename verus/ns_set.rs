//! Verus specification and proof for the `Ns_Set` lookup and deletion
//! contracts implemented in `crates/ns-core/src/set.rs`.
//!
//! Keys are modelled as `u64` identifiers: equality is all the algorithm
//! uses, so the proof carries over to string keys.
//!
//! Proved:
//! * `find` returns the *first* index holding the key, or `None` when the
//!   key is absent (`Ns_SetFind`).
//! * `put` appends and returns the new last index (`Ns_SetPut`).
//! * `delete` removes exactly one field and preserves the relative order
//!   of the others (`Ns_SetDelete`).
//! * After `put(k)`, `find(k)` succeeds and never returns an index past
//!   the new field.

use vstd::prelude::*;

verus! {

/// `i` is the first position of `k` in `s`.
pub open spec fn first_index(s: Seq<u64>, k: u64, i: int) -> bool {
    &&& 0 <= i < s.len()
    &&& s[i] == k
    &&& forall|j: int| 0 <= j < i ==> s[j] != k
}

/// `Ns_SetFind`.
pub fn find(keys: &Vec<u64>, key: u64) -> (r: Option<usize>)
    ensures
        match r {
            Some(i) => first_index(keys@, key, i as int),
            None => forall|j: int| 0 <= j < keys@.len() ==> keys@[j] != key,
        },
{
    let mut i: usize = 0;
    while i < keys.len()
        invariant
            0 <= i <= keys.len(),
            forall|j: int| 0 <= j < i ==> keys@[j] != key,
        decreases keys.len() - i,
    {
        if keys[i] == key {
            return Some(i);
        }
        i = i + 1;
    }
    None
}

/// `Ns_SetPut`: append and return the new index.
pub fn put(keys: &mut Vec<u64>, key: u64) -> (r: usize)
    requires
        old(keys).len() < usize::MAX,
    ensures
        final(keys)@ == old(keys)@.push(key),
        r == old(keys).len(),
{
    keys.push(key);
    keys.len() - 1
}

/// `Ns_SetDelete`.
pub fn delete(keys: &mut Vec<u64>, index: usize)
    requires
        index < old(keys).len(),
    ensures
        final(keys)@ == old(keys)@.remove(index as int),
{
    keys.remove(index);
}

/// Deletion preserves the order of the remaining fields.
pub proof fn lemma_delete_preserves_order(s: Seq<u64>, i: int)
    requires
        0 <= i < s.len(),
    ensures
        s.remove(i).len() == s.len() - 1,
        forall|j: int| 0 <= j < i ==> s.remove(i)[j] == s[j],
        forall|j: int| i <= j < s.len() - 1 ==> s.remove(i)[j] == s[j + 1],
{
}

/// After appending `k`, `k` has a first index no greater than the new
/// field's index.
pub proof fn lemma_put_then_find(s: Seq<u64>, k: u64)
    ensures
        exists|i: int| first_index(s.push(k), k, i) && i <= s.len(),
{
    let t = s.push(k);
    if exists|j: int| 0 <= j < s.len() && s[j] == k {
        let j0 = choose|j: int| 0 <= j < s.len() && s[j] == k;
        lemma_first_exists(t, k, j0);
        let i = choose|i: int| first_index(t, k, i) && i <= j0;
        assert(first_index(t, k, i) && i <= s.len());
    } else {
        assert(t[s.len() as int] == k);
        assert forall|j: int| 0 <= j < s.len() implies t[j] != k by {
            assert(t[j] == s[j]);
        }
        assert(first_index(t, k, s.len() as int));
    }
}

/// If `k` occurs at `j`, a first occurrence exists at or before `j`.
pub proof fn lemma_first_exists(s: Seq<u64>, k: u64, j: int)
    requires
        0 <= j < s.len(),
        s[j] == k,
    ensures
        exists|i: int| first_index(s, k, i) && i <= j,
    decreases j,
{
    if forall|m: int| 0 <= m < j ==> s[m] != k {
        assert(first_index(s, k, j));
    } else {
        let m = choose|m: int| 0 <= m < j && s[m] == k;
        lemma_first_exists(s, k, m);
    }
}

} // verus!

fn main() {}
