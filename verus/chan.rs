//! Verus model of `ns_chan` channel ownership (`crates/nsd/src/cmds/misc.rs`,
//! `nsd/tclfile.c`).
//!
//! A Tcl channel is owned by the interps it is registered in (`interps` is
//! its registration count, Tcl's `refCount`), or by the server's shared
//! table (`ns_chan create`/`put` cut it from its thread with
//! `Tcl_CutChannel` and hold it registered with the NULL interp), or it is
//! closed.  We prove that every `ns_chan` operation, and every script
//! operation on a channel, keeps it in exactly one of those places:
//!
//! * a channel in the shared table is registered in no interp (so no
//!   thread can reach it until `ns_chan get` splices it back),
//! * a channel outside the table and not closed is registered in at
//!   least one interp,
//! * a closed channel is registered nowhere and not in the table.
//!
//! `create` and `put` cut a channel only when it is registered in exactly
//! one interp (`Tcl_IsChannelShared` is false), so a cut channel is never
//! left reachable from another interp.  The table is keyed by name;
//! `create` refuses a name that is taken, `get` removes the entry it
//! splices, so a channel never has two table entries.

use vstd::prelude::*;

verus! {

pub struct Chan {
    /// Interps the channel is registered in.
    pub interps: nat,
    /// Held by the shared table.
    pub in_table: bool,
    /// Closed (freed).
    pub closed: bool,
}

/// Exactly one owner: some interps, the table, or nobody (closed).
pub open spec fn inv(c: Chan) -> bool {
    &&& (c.closed ==> c.interps == 0 && !c.in_table)
    &&& (c.in_table ==> c.interps == 0 && !c.closed)
    &&& (!c.closed && !c.in_table ==> c.interps >= 1)
}

/// `Tcl_IsChannelShared`.
pub open spec fn shared(c: Chan) -> bool {
    c.interps > 1
}

/// Whether `ns_chan create chan name` (or `ns_chan put name`) succeeds:
/// refused unless the channel is registered in exactly one interp
/// (`Tcl_IsChannelShared` is false) and the name is free.  A refused
/// operation is an error that changes nothing.
pub open spec fn cut_ok(c: Chan, name_taken: bool) -> bool {
    !c.closed && !c.in_table && c.interps == 1 && !name_taken
}

/// `UnspliceChannel`: the channel is cut and held by the table.
pub open spec fn cut(c: Chan) -> Chan {
    Chan { interps: 0, in_table: true, closed: false }
}

/// Whether `ns_chan get name` succeeds: the name is in the table.
pub open spec fn get_ok(c: Chan) -> bool {
    c.in_table
}

/// `SpliceChannel`: splice the table's channel into this interp.
pub open spec fn splice(c: Chan) -> Chan {
    Chan { interps: 1, in_table: false, closed: false }
}

/// `ns_chan cleanup -shared`: splice and unregister from the NULL interp,
/// which closes it.
pub open spec fn cleanup_shared(c: Chan) -> Chan {
    if c.in_table {
        Chan { interps: 0, in_table: false, closed: true }
    } else {
        c
    }
}

/// `Tcl_UnregisterChannel(interp)` (`close`, `ns_chan cleanup`, interp
/// deletion): drop one registration; the last one closes the channel.
pub open spec fn unregister(c: Chan) -> Chan {
    if c.interps == 0 {
        c
    } else if c.interps == 1 {
        Chan { interps: 0, in_table: false, closed: true }
    } else {
        Chan { interps: (c.interps - 1) as nat, in_table: c.in_table, closed: c.closed }
    }
}

/// `Tcl_RegisterChannel` into another interp of the same thread (e.g.
/// `interp share`): only a channel already reachable from an interp.
pub open spec fn register(c: Chan) -> Chan {
    if c.closed || c.in_table {
        c
    } else {
        Chan { interps: c.interps + 1, in_table: c.in_table, closed: c.closed }
    }
}

/// `create` and `put` move the channel from its one interp to the table.
pub proof fn cut_preserves(c: Chan, name_taken: bool)
    requires
        inv(c),
        cut_ok(c, name_taken),
    ensures
        inv(cut(c)),
        cut(c).in_table,
        cut(c).interps == 0,
{
}

/// A channel registered in several interps, or a taken name, is refused.
pub proof fn shared_is_refused(c: Chan, name_taken: bool)
    requires
        inv(c),
        shared(c) || name_taken || c.in_table,
    ensures
        !cut_ok(c, name_taken),
{
}

/// `get` moves the channel from the table to exactly one interp.
pub proof fn splice_preserves(c: Chan)
    requires
        inv(c),
        get_ok(c),
    ensures
        inv(splice(c)),
        !splice(c).in_table,
        splice(c).interps == 1,
{
}

pub proof fn cleanup_shared_preserves(c: Chan)
    requires
        inv(c),
    ensures
        inv(cleanup_shared(c)),
        !cleanup_shared(c).in_table,
{
}

pub proof fn unregister_preserves(c: Chan)
    requires
        inv(c),
    ensures
        inv(unregister(c)),
{
}

pub proof fn register_preserves(c: Chan)
    requires
        inv(c),
    ensures
        inv(register(c)),
        c.in_table ==> register(c) == c,
{
}

/// A `put` followed by a `get` (in any thread) gives the channel back to
/// exactly one interp.
pub proof fn put_get_round_trip(c: Chan)
    requires
        inv(c),
        c.interps == 1,
        !c.in_table,
    ensures
        cut_ok(c, false),
        get_ok(cut(c)),
        splice(cut(c)) == c,
{
}

/// A channel held by the table cannot be reached by script operations:
/// registering and unregistering leave it where it is, and it cannot be
/// cut again.
pub proof fn table_is_exclusive(c: Chan, name_taken: bool)
    requires
        inv(c),
        c.in_table,
    ensures
        register(c) == c,
        unregister(c) == c,
        !cut_ok(c, name_taken),
{
}

} // verus!

fn main() {}
