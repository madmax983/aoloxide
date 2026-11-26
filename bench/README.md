# Benchmark: aoloxide vs axum

A reproducible HTTP/1.1 throughput and latency comparison between aoloxide
and an [axum](https://github.com/tokio-rs/axum) server that serves the same
endpoints with byte-identical bodies.  Latest results and analysis:
[docs/benchmarks.md](../docs/benchmarks.md).

```sh
sudo apt-get install wrk tcl8.6-dev
bench/run.sh                                 # ~7 min, writes bench/results/<host>-<date>.{md,csv}
DURATION=3 WARMUP=1 CONNS=64 bench/run.sh    # quick look
```

## What is compared

| Scenario | Path | aoloxide | axum | axum-cached |
|---|---|---|---|---|
| `hello` | `/hello` | Tcl `ns_register_proc` → `ns_return` ([`aoloxide/bench.tcl`](aoloxide/bench.tcl)) | handler returning `&'static str` | (same as axum) |
| `static-1k` | `/1k.txt` | fastpath, in-memory file cache (on by default) | `tower-http` `ServeDir` (open + read per request) | page root preloaded into a `HashMap` |
| `static-64k` | `/64k.txt` | same | same | same |
| `adp-page` | `/page.adp?name=bench` | ADP page with a Tcl loop ([`www/page.adp`](www/page.adp)) | `format!`-style handler | (same as axum) |

* **aoloxide** runs `nsd` with [`aoloxide/nsconfig.tcl`](aoloxide/nsconfig.tcl): only
  `nssock` is loaded (no access log, auth or CGI), `minthreads = maxthreads = $THREADS`
  (default 20), `maxsock 1024`, and `ns_limits set default -maxwait 1024`.
  AOLserver's default `maxwait` of 100 answers excess queued requests with
  503 (cheap responses that inflate req/s at 256 connections); axum queues
  without bound, so the cap is lifted.
* **axum** is [`axum-server/`](axum-server), a standalone crate (its own
  `[workspace]`, so axum is not a dependency of the aoloxide workspace).  It
  sets `TCP_NODELAY` on accepted sockets, as aoloxide's driver does; without it
  `ServeDir`'s separate header/body writes hit the Nagle/delayed-ACK 40 ms stall.
* **axum-cached** is the same binary with `cached`: it isolates "server speed"
  from "file cache" on the static scenarios, since aoloxide's fastpath serves
  from memory.

Before timing anything, `run.sh` fetches every path from every server and
fails unless the bodies hash identically to aoloxide's.

## Method

* Release builds (`lto = "thin"` on both).
* The server under test is pinned to `SERVER_CPUS` (default `0-1`) and wrk to
  `CLIENT_CPUS` (default `2-3`) with `taskset`, so client and server do not
  fight for cores.  axum's tokio runtime sizes itself to the 2 pinned CPUs.
* Per point: a `WARMUP`-second wrk run, then a `DURATION`-second run with
  `--latency`; a Lua `done` hook ([`run.sh`](run.sh)) reports req/s, p50/p90/p99/max and
  errors.  Concurrency sweeps `CONNS` (default `16 64 256`), keep-alive on.
* **CPU µs/req** is the server process's `utime + stime` over the timed run
  divided by requests served: the cost of a request independent of how
  many cores were available.
* **Peak RSS** is the server's `VmHWM` after all its runs.
* Raw wrk output and server logs go to `bench/results/raw/` (git-ignored).

## Caveats

* Loopback only; no network, TLS or request bodies.  Small payloads measure
  per-request overhead, not bandwidth.
* One machine, one run: treat differences under ~10% as noise.
* Cloud VMs have noisy neighbours; rerun before drawing fine conclusions.
