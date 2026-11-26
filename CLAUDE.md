# CLAUDE.md

Guidance for working on aoloxide, a Rust port of AOLserver 4.5.

## Layout

| Crate | Role | AOLserver origin |
|---|---|---|
| `crates/ns-core` | engine-independent data structures & HTTP utilities | `nsd/{set,urlspace,request,...}.c` |
| `crates/ns-script` | `Interp` / `ScriptEngine` trait, arg helpers | (new seam) |
| `crates/ns-tcl` | Tcl 8.6 engine over libtcl FFI | `nsd/tclinit.c` (embedding) |
| `crates/nsd` | server core: drivers, pools, conn pipeline, ADP, commands | `nsd/*.c` |
| `crates/nslog`, `nsperm`, `nscgi`, `nsssl`, `nszlib`, `nscp`, `nsdb`, `nsproxy`, `nsext` | modules | `nslog/`, `nsperm/`, `nscgi/`, `nsssl/`, `nszlib/`, `nscp/`, `nsdb/`, `nsproxy/`, `nsext/` |
| `crates/nspd` | proxy-daemon library + `nssqlitepd` | `nspd/` |
| `crates/aoloxide` | the `nsd` binary + module registry | `nsd/main.c`, `nsmain.c` |
| `verus/` | Verus specs & proofs (run in CI) | |
| `docs/adr/` | architecture decisions | |
| `docs/porting.md` | C → Rust map, deviations, phase plan | |

The original C sources are not vendored; consult
https://github.com/aolserver/aolserver when porting more of it.

## Commands

```sh
sudo apt-get install tcl8.6-dev      # required to link ns-tcl
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo llvm-cov --workspace --summary-only --fail-under-lines 85
git config core.hooksPath .githooks  # pre-commit: fmt + clippy + test
```

Verus: download a release from verus-lang/verus, install the toolchain named
in its `version.json`, then `verus verus/<file>.rs` (see `.github/workflows/ci.yml`).

## Rules

* **Fidelity first.** Behaviour should match AOLserver 4.5 unless a
  deviation is deliberate; every deviation is listed in `docs/porting.md`.
  When unsure, read the C function and port its logic, including quirks.
* **Commands are written against `ns_script::Interp`**, never against Tcl
  directly.  Engine-specific needs (channels, sockets) go through
  `eval_words` with the engine's own commands.
* **Interps are thread-bound.** Never send an `Interp` or `ConnRef` across
  threads.  Cross-thread state lives in `Server`/`Global` behind locks.
* **Never hold a `RefCell` borrow (`conn_mut()`, `ns_interp(..).borrow()`)
  across `interp.eval*`** — commands re-enter.
* No `unwrap()` in production code; `expect` only where an invariant makes
  failure impossible.  Lock poisoning is recovered with
  `unwrap_or_else(PoisonError::into_inner)`.
* Every behaviour needs a test.  Tcl-visible behaviour goes in
  `crates/nsd/tests/commands.rs` (runs scripts in a live server).
* Critical invariants need a Verus spec in `verus/`.
* `cargo fmt` and clippy (pedantic + nursery, `-D warnings`) must pass.

## Adding a module

1. New crate `crates/<name>` depending on `nsd`.
2. `pub fn init(ctx: &nsd::ModuleContext) -> Result<(), nsd::NsError>`:
   read `ctx.config_path()`, register procs/filters/commands
   (`server.add_command` before Tcl init), drivers via `ctx.add_driver`.
3. Register it in `crates/aoloxide/src/lib.rs::modules()`.
4. Ship any Tcl library as `TCL_LIBRARY` and install it in `install_home`.

`nsproxy` workers are the separate `nsproxy` binary, and the nsext tests
start the `nssqlitepd` daemon; both must be built before those tests run
(`cargo test --workspace` does).
