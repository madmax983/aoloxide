# ADR 0004: Modules are statically linked and registered by name

* Status: accepted
* Date: 2026-11-25

## Context

AOLserver `dlopen`s modules listed in `ns/server/S/modules`
(`ns_param nslog ${bindir}/nslog.so`) and calls `Ns_ModuleInit`.  Rust has
no stable ABI for passing `&Server` across a dynamic-library boundary.

## Decision

Modules are crates linked into the binary and registered in a
`ModuleRegistry` by name.  A config entry's value is reduced to its file
stem (`${bindir}/nslog.so` → `nslog`), so existing configs work unchanged.
The value `tcl` keeps its AOLserver meaning: a Tcl-only module whose
library is sourced at startup.  Each module still gets its Tcl library
sourced via `ns_ictl addmodule` semantics.

## Consequences

* Existing configs load the same modules with no edits.
* Third-party C modules cannot be loaded; they must be ported, or exposed
  through a future FFI shim crate.
* Embedders can build their own binary with a custom `ModuleRegistry`.
