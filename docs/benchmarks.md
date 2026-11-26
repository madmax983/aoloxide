# Benchmarks: aoloxide vs axum

How fast is aoloxide next to an idiomatic, modern Rust server?  This compares
it with [axum](https://github.com/tokio-rs/axum) 0.8 serving the same
endpoints with byte-identical bodies.  Harness, method and caveats:
[bench/README.md](../bench/README.md).  Full tables:
[bench/results/baseline.md](../bench/results/baseline.md) (CSV alongside).

Setup: 4-vCPU Xeon @ 2.10 GHz cloud VM, Linux 6.18, loopback; server pinned to
2 CPUs, wrk (2 threads) to the other 2; HTTP/1.1 keep-alive; 10 s per point;
aoloxide with 20 connection threads, no access log.

## Results

Requests per second (higher is better), 64 connections:

| scenario | aoloxide | axum | axum-cached | axum ÷ aoloxide |
|---|---:|---:|---:|---:|
| `hello`: 13-byte text (Tcl `ns_register_proc` / handler) | 32,198 | 152,596 | — | **4.7×** |
| `static-1k`: 1 KiB file | 45,294 | 14,730 | 140,593 | 0.33× (cached: **3.1×**) |
| `static-64k`: 64 KiB file | 37,035 | 11,951 | 95,541 | 0.32× (cached: **2.6×**) |
| `adp-page`: templated HTML, 10-row loop | 20,306 | 140,738 | — | **6.9×** |

Server CPU per request (lower is better; independent of core count):

| scenario | aoloxide | axum | axum-cached |
|---|---:|---:|---:|
| `hello` | 61 µs | 13 µs | — |
| `static-1k` | 43 µs | 124 µs | 14 µs |
| `static-64k` | 53 µs | 159 µs | 20 µs |
| `adp-page` | 98 µs | 14 µs | — |

Latency at 64 connections, p50 / p99: `hello` aoloxide 1.96 / 4.42 ms vs
axum 0.40 / 1.09 ms; `adp-page` 3.02 / 8.39 ms vs 0.44 / 1.10 ms.
Throughput is flat from 16 to 256 connections for both servers (both are
CPU-bound on their 2 cores from 16 connections up).  Peak RSS: aoloxide
36 MiB, axum 34 MiB (ServeDir's blocking pool), axum-cached 12 MiB.

## Reading the numbers

1. **Dynamic requests: axum is 4.7–7× faster.**  `hello` and `adp-page` are
   where a Tcl-scripted server pays for its flexibility, and also where
   aoloxide's per-request core overhead (below) shows.
2. **Static files: aoloxide is 3× faster than axum's `ServeDir`,** because
   fastpath serves from an in-memory cache (AOLserver's default) while
   `ServeDir` opens and reads the file through tokio's blocking pool on every
   request (~125 µs CPU/req).  That is a caching difference, not a server
   one: given the same in-memory files (`axum-cached`), axum is 2.6–3.1×
   faster than aoloxide.
3. **The cost is mostly in the core, not in Tcl.**  Stacking the scenarios
   (CPU per request, aoloxide):

   | layer | µs/req | adds |
   |---|---:|---:|
   | core pipeline: driver → queue → conn thread → fastpath cache hit (`static-1k`) | 43 | |
   | + Tcl request proc and `ns_return` (`hello`) | 61 | +18 |
   | + ADP page, `ns_queryget`, 10-iteration Tcl loop (`adp-page`) | 98 | +37 |

   axum does a whole request, routing included, in 13–14 µs.  aoloxide spends
   ~3× that before any Tcl runs.

## Where aoloxide's time goes

`perf record` (cpu-clock, call graphs) on `static-1k` at 64 connections,
which exercises no Tcl:

| | share of server CPU |
|---|---:|
| scheduler: wakeups (`_raw_spin_unlock_irqrestore` under `futex_wake → try_to_wake_up` for pool/oneshot handoffs, and `sock_def_readable` for socket readiness) and context switches (`finish_task_switch`) | **~37%** |
| `malloc`/`free`/`realloc` (glibc) | ~16% |
| file `stat()` / path lookup for the fastpath cache check | ~1.3% |
| everything else (TCP stack, parsing, headers, fastpath, tokio) | rest |

Split by thread, 68% of CPU is in connection threads and 32% in the tokio
driver threads.

**Context switches per request: aoloxide 1.53, axum 0.018 (85× fewer).**
That is the threading model of [ADR 0003](adr/0003-threading-model.md),
which follows AOLserver's: a tokio driver task reads the request, queues it
to a pool of OS threads (mutex + condvar wakeup), the connection thread
writes the response through `Handle::block_on` (`conn.rs` `send_bounded`) and hands the socket back
over a oneshot (another wakeup).  Every request crosses threads at least
twice; axum parses, routes, runs the handler and writes on one tokio worker
without leaving it.

### Connection-thread count

The pool size matters a little, not a lot (64 connections, 2 CPUs, 4 s runs):

| `maxthreads` | `hello` req/s | `static-1k` req/s | `adp-page` req/s |
|---:|---:|---:|---:|
| 2 | 30,699 | 48,951 | 16,923 |
| **4** | **35,084** | **60,440** | 19,012 |
| 8 | 35,524 | 45,829 | 19,439 |
| 20 (baseline) | 32,227 | 44,103 | 19,492 |

About 2× the CPU count is best; it narrows the static gap to axum-cached to
~2.3× but leaves the dynamic gap at 4–7×.

### Load shedding

With AOLserver's default `ns_limits` (`maxwait 100`), 20 running plus 100
queued requests is the most aoloxide accepts; at 256 connections it answered
1–6% of requests with 503 (logged as `queue: overflow`), by design.  The
benchmark lifts `maxwait` to 1024 so every response is a real 200; axum
has no equivalent limit and queues without bound.

## Where to look for speed

Ranked by expected payoff, from the profile.  None of these are done yet.

1. **Fewer cross-thread hops.**  The 37% scheduler share is the biggest
   lever.  Options, in increasing order of deviation from AOLserver:
   write responses with plain blocking `send` on the socket rather than
   `Handle::block_on` per write; return keep-alive sockets without the
   oneshot round-trip; or serve fastpath cache hits directly on the driver
   task when no filters/traces are registered for the URL (a documented
   deviation, as AOLserver always queues).
2. **Allocation.**  16% in glibc malloc: per-request `Ns_Set`s, header
   strings and the response buffer.  Pooling per-connection buffers or a
   faster allocator (mimalloc/jemalloc in the `nsd` binary) are cheap
   experiments.
3. **Default `maxthreads` relative to cores.**  ~2× CPUs beat 20 threads on
   2 CPUs by 9–37%; worth noting in the example config.
4. **Tcl and ADP** (18 and 37 µs/req) are already reasonable for
   interpreted code; `ns_queryget` (a Tcl proc that builds the form set) is
   the obvious hot spot on `adp-page`.

## Reproducing

```sh
sudo apt-get install wrk tcl8.6-dev
bench/run.sh                    # ~7 min; writes bench/results/<host>-<date>.{md,csv}
THREADS=4 bench/run.sh          # conn-thread sweep point
```
