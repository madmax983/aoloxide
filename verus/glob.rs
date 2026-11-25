//! Verus specification and proof for the `*` / `?` / literal fragment of
//! Tcl glob matching (`Tcl_StringMatch`), implemented in
//! `crates/ns-core/src/glob.rs`.
//!
//! The specification `gmatch` is the textbook recursive definition.  The
//! executable matcher below is proved equal to it; the production matcher
//! (an iterative backtracking version that also handles `[...]` and `\`)
//! is checked against the same recursive definition by property tests
//! (`agrees_with_reference` in `glob.rs`).

use vstd::prelude::*;

verus! {

pub const STAR: u8 = 42;  // '*'
pub const QUEST: u8 = 63; // '?'

/// Glob semantics for `*`, `?` and literal bytes.
pub open spec fn gmatch(s: Seq<u8>, p: Seq<u8>) -> bool
    decreases p.len(), s.len(),
{
    if p.len() == 0 {
        s.len() == 0
    } else if p[0] == STAR {
        gmatch(s, p.drop_first()) || (s.len() > 0 && gmatch(s.drop_first(), p))
    } else if s.len() == 0 {
        false
    } else if p[0] == QUEST || p[0] == s[0] {
        gmatch(s.drop_first(), p.drop_first())
    } else {
        false
    }
}

pub open spec fn tail(v: Seq<u8>, i: int) -> Seq<u8> {
    v.subrange(i, v.len() as int)
}

proof fn lemma_tail_drop(v: Seq<u8>, i: int)
    requires
        0 <= i < v.len(),
    ensures
        tail(v, i).drop_first() =~= tail(v, i + 1),
        tail(v, i)[0] == v[i],
        tail(v, i).len() > 0,
{
}

/// Executable matcher, proved equal to `gmatch` on the suffixes
/// `s[si..]` and `p[pi..]`.
pub fn glob(s: &Vec<u8>, si: usize, p: &Vec<u8>, pi: usize) -> (r: bool)
    requires
        si <= s.len(),
        pi <= p.len(),
    ensures
        r == gmatch(tail(s@, si as int), tail(p@, pi as int)),
    decreases p.len() - pi, s.len() - si,
{
    if pi == p.len() {
        assert(tail(p@, pi as int).len() == 0);
        return si == s.len();
    }
    proof {
        lemma_tail_drop(p@, pi as int);
    }
    if p[pi] == STAR {
        if glob(s, si, p, pi + 1) {
            return true;
        }
        if si < s.len() {
            proof {
                lemma_tail_drop(s@, si as int);
            }
            return glob(s, si + 1, p, pi);
        }
        assert(tail(s@, si as int).len() == 0);
        return false;
    }
    if si == s.len() {
        assert(tail(s@, si as int).len() == 0);
        return false;
    }
    proof {
        lemma_tail_drop(s@, si as int);
    }
    if p[pi] == QUEST || p[pi] == s[si] {
        return glob(s, si + 1, p, pi + 1);
    }
    false
}

/// `*` matches everything.
pub proof fn lemma_star_matches_all(s: Seq<u8>)
    ensures
        gmatch(s, seq![STAR]),
    decreases s.len(),
{
    let p = seq![STAR];
    assert(p.drop_first() =~= Seq::<u8>::empty());
    if s.len() == 0 {
        assert(gmatch(s, p.drop_first()));
    } else {
        lemma_star_matches_all(s.drop_first());
    }
}

/// A pattern without metacharacters matches only itself.
pub proof fn lemma_literal_matches_itself(s: Seq<u8>)
    requires
        forall|i: int| 0 <= i < s.len() ==> s[i] != STAR && s[i] != QUEST,
    ensures
        gmatch(s, s),
    decreases s.len(),
{
    if s.len() > 0 {
        let d = s.drop_first();
        assert forall|i: int| 0 <= i < d.len() implies d[i] != STAR && d[i] != QUEST by {
            assert(d[i] == s[i + 1]);
        }
        lemma_literal_matches_itself(d);
    }
}

} // verus!

fn main() {}
