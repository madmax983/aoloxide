# Benchmarks: aoloxide vs AOLserver vs axum

How fast is aoloxide, and how much of that is AOLserver itself?  This compares
it with the **original C AOLserver 4.5** it ports and with
[axum](https://github.com/tokio-rs/axum) 0.8, all serving the same endpoints
with byte-identical bodies.  Harness, method and caveats:
[bench/README.md](../bench/README.md).  Full tables:
[bench/results/baseline.md](../bench/results/baseline.md) (CSV alongside).

Setup: 4-vCPU Xeon @ 2.10 GHz cloud VM, Linux 6.18, loopback; server pinned to
2 CPUs, wrk (2 threads) to the other 2; keep-alive; 10 s per point; both
`nsd`s with 20 connection threads, the same `nsconfig.tcl`, no access log;
C AOLserver built at `-O2` against the same Tcl 8.6.14.

## Summary

* **aoloxide performs like the C original.**  It serves 5–30% more
  requests/s on Tcl procs and static files at about the same CPU per request,
  and is level on ADP throughput while spending 20–25% more CPU per ADP page.
* **The 5–8× gap to axum is AOLserver's design, faithfully ported**, not
  overhead added by the port: C AOLserver pays the same per-request costs.
* **One clear port inefficiency:** ADP script blocks are re-parsed and
  re-compiled to bytecode on every request; C AOLserver compiles them once
  per interp.  Caching them recovers most of the ADP gap to C (below).

## Results

Requests/s at 64 connections (higher is better):

| scenario | AOLserver (C) | aoloxide | aoloxide ÷ C | axum | axum-cached | axum ÷ aoloxide |
|---|---:|---:|---:|---:|---:|---:|
| `hello`: Tcl `ns_register_proc` → `ns_return` | 27,463 | 35,591 | 1.30× | 181,911 | — | 5.1× |
| `static-1k`: 1 KiB file | 39,801 | 51,473 | 1.29× | 16,373 | 166,198 | 0.32× (cached 3.2×) |
| `static-64k`: 64 KiB file | 36,544 | 43,704 | 1.20× | 14,518 | 102,536 | 0.33× (cached 2.3×) |
| `adp-page`: templated HTML, 10-row loop | 21,958 | 20,964 | 0.95× | 163,627 | — | 7.8× |

Server CPU µs per request (lower is better; independent of core count) and
context switches per request:

| scenario | AOLserver (C) | aoloxide | axum | axum-cached |
|---|---:|---:|---:|---:|
| `hello` | 59.6 · 1.66 cs | 55.4 · 1.19 cs | 10.5 · 0.01 cs | — |
| `static-1k` | 39.6 · 1.47 cs | 38.7 · 1.31 cs | 110.2 · 9.33 cs | 11.6 · 0.01 cs |
| `static-64k` | 43.0 · 1.45 cs | 45.0 · 1.21 cs | 131.4 · 9.42 cs | 18.7 · 0.02 cs |
| `adp-page` | 76.4 · 1.64 cs | 94.9 · 0.67 cs | 11.9 · 0.01 cs | — |

* Static files: fastpath's in-memory cache beats axum's `ServeDir`, which
  opens and reads the file through tokio's blocking pool on every request
  (hence ~9 context switches per request).  Given the same in-memory files,
  axum is 2.3–3.7× faster.
* Latency follows throughput.  At 256 connections C AOLserver has the lower
  median but a long tail: max ~220 ms and 8–10 wrk timeouts (> 2 s) per
  10 s run in three of four scenarios; aoloxide's max stays under 100 ms with
  no errors.
* Peak RSS: aoloxide 35 MiB, C AOLserver 24 MiB, axum 34 MiB (`ServeDir`
  blocking pool), axum-cached 13 MiB.

## Where the time goes

Stacking the layers (CPU µs per request, 64 connections, 20 threads).  The
two intermediate rows are one-off experiments described below:

| layer | µs/req | is it AOLserver's design? |
|---|---:|---|
| axum, `hello` | ~11 | — |
| AOLserver's threading model alone: tokio driver → OS-thread pool → back | ~13.5 | yes: +3 over axum |
| aoloxide pure-Rust path: fastpath cache hit (`static-1k`) | ~39 | yes: C costs the same (~40) |
| + Tcl request proc, *without* the per-connection `ns_cleanup` | ~39 | — |
| + `ns_cleanup` from AOLserver's own `init.tcl` (`hello`) | ~55–60 | yes: identical script in C |
| + ADP page (`adp-page`) | ~95 | C: ~76; see [the ADP gap](#the-adp-gap) |

### Threading model: cheap

A ~100-line skeleton reproduces aoloxide's handoff with no other logic: a
tokio task reads the request, queues it on a `Mutex`/`Condvar` pool of OS
threads, the thread writes a canned response with `Handle::block_on` and
returns the socket over a oneshot.  It costs **13.5 µs/req** with 20 threads
(1.1 context switches/req) and 7.5 µs with 4, against 5.8 µs for the same
loop without the handoff and ~11 µs for axum.  Writing with a plain `write(2)`
instead of `block_on` made no difference (13.1 µs).  The handoff is real but
small; it is not what separates AOLserver from axum.

### Per-request pipeline: AOLserver's, in both implementations

Everything between "request read" and "bytes sent" (header `Ns_Set`s, URL
space and `ns_limits` lookups, filters/traces/auth hooks, required headers,
HTTP date, fastpath `stat` and cache lookup) costs ~25 µs on top of the
skeleton.  The C server spends the same: 39.6 µs vs aoloxide's 38.7 µs on
`static-1k`.  Syscalls per request match too: one `recv`, one `send`, two
`stat`s for a fastpath hit; the difference is futex wakeups (3.2 per request vs
axum's 1.0).

### `ns_cleanup`: ~17 µs on every Tcl request

After each connection that used an interp, AOLserver's `init.tcl` (registered
with `ns_ictl trace deallocate ns_cleanup`) closes channels, walks every
global variable to unset it, and frees sets.  aoloxide ships the script
byte-identical to C's `nsd/init.tcl`.  With the trace disabled, `hello`
dropped from 56 to **39 µs/req**: the Tcl proc and `ns_return` themselves
cost almost nothing over the pure-Rust path.  C AOLserver pays the same
(its deallocate `Tcl_EvalEx` is ~21 µs/req on `adp-page`, vs aoloxide's ~22).

## The ADP gap

ADP is the one scenario where aoloxide is clearly costlier than C: ~95 vs
~76 µs/req at 20 threads, 107 vs 77 at 4 threads.  `perf` (cpu-clock, call
graphs; C built with frame pointers) at 4 threads, per request:

| | C AOLserver | aoloxide | Δ |
|---|---:|---:|---:|
| libtcl | 30.7 µs | 40.1 µs | +9.4 |
| server core (`libnsd` / Rust `nsd`) | 7.4 µs | 19.3 µs | +11.9 |
| libc (malloc) | 9.9 µs | 17.8 µs | +7.9 |
| kernel | 28.8 µs | 29.0 µs | 0 |
| **ADP execution** (`AdpExec` / `nsd::adp::exec`, inclusive) | **12.8 µs** | **37.6 µs** | **+24.8** |
| `ns_cleanup` (deallocate trace) | ~21 µs | ~22 µs | ~0 |
| response flush + send | ~20 µs | ~20 µs | ~0 |

Practically the whole gap is ADP execution, and most of that is one thing:

1. **Bytecode is not cached.**  C keeps each script block of a parsed page as
   a `Tcl_Obj` in a per-interp page cache (`nsd/adpeval.c`: `Tcl_Obj *objs[]`,
   "Scripts to be compiled and reused") and runs it with `Tcl_EvalObjEx`, so
   Tcl compiles it once per interp.  aoloxide's `adp::exec` passes each block
   to `Interp::eval(&str)`, which is `Tcl_EvalEx`: parse and compile on every
   request (`TclSetByteCodeFromAny` 5.1% inclusive, `Tcl_ParseCommand`, more
   `TclFreeObj`), and C shows none of it.  An experiment that cached script
   objects per thread and evaluated them with `Tcl_EvalObjEx` cut `adp-page`
   CPU by **8–48 µs/req across six paired A/B rounds (median ~22 µs)**, to
   87–104 µs, roughly C's level (76–97 µs across runs); the VM was noisy, so
   read it as "most of the gap".
2. **Command calls convert arguments.**  `ns_tcl::cmd_trampoline` turns every
   Tcl argument into a Rust `String` (`obj_string`) before an `ns_*` command
   runs; C's object commands read the `Tcl_Obj`s directly.  Smaller, and
   inherent to the engine-independent `Interp` trait (ADR 0002).
3. **Allocator.**  C AOLserver is built with `USE_THREAD_ALLOC`: `ns_malloc`
   uses Tcl's per-thread allocator, while aoloxide's Rust code uses glibc
   `malloc`, which is the +8 µs libc row.

## Corrections to the previous version of this page

The first version (before the C comparison) mis-attributed the gap:

* It reported ~37% of aoloxide's CPU in scheduler wakeups.  That was
  overstated: in this VM, cpu-clock samples taken while interrupts are
  disabled are charged to `_raw_spin_unlock_irqrestore` when interrupts come
  back on.  Cutting context switches 5× (20 → 4 threads) saved only ~4 µs/req,
  and the skeleton above shows the handoff costs ~3 µs over axum.
* It ranked "fewer cross-thread hops" as the top speed lever.  The data
  above says the lever is elsewhere (below), and C AOLserver makes the same
  hops.

## Where to look for speed

Ranked by measured payoff.  None changes behaviour; none is done yet.

1. **Cache compiled ADP script blocks per interp**, as C does: ~20 µs/req on
   ADP pages, and closer to C's implementation, not further.  Needs a way to
   hold an engine-specific compiled script on the page cache through the
   `Interp` trait (ADR 0002); a global per-thread cache keyed by script text
   would grow without bound for `eval`'d dynamic code, so it must be scoped
   to parsed pages.
2. **A native `ns_cleanup` fast path** with the same semantics (unset
   globals except `auto_*`/`tcl_*`/`env`, close channels, free sets): up to
   ~17 µs on every Tcl request.  `init.tcl` must stay overridable, so this
   means making the commands it calls cheaper or recognising the stock proc.
3. **Allocation**: try a thread-caching allocator (mimalloc/jemalloc) in the
   `nsd` binary, the analogue of C's `USE_THREAD_ALLOC`.
4. **Per-response work**: one `ns_limits` lookup instead of two (driver and
   queue), the `Date` header formatted once per second, fewer `NsSet`
   allocations for response headers.
5. **`maxthreads` around 2× cores**: 4 threads on 2 CPUs ran `static-1k`
   +37% and `hello` +9% faster than 20 (both servers benefit).

## Reproducing

```sh
sudo apt-get install wrk tcl8.6-dev tcl8.6
bench/build-aolserver.sh /opt/aolserver-bench          # C reference, ~1 min
AOLSERVER_HOME=/opt/aolserver-bench bench/run.sh       # ~9 min
THREADS=4 AOLSERVER_HOME=/opt/aolserver-bench bench/run.sh
```

The skeleton, `ns_cleanup` and bytecode-cache experiments were one-off
measurements made outside the harness and are not part of `bench/`.
