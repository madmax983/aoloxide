# Benchmark: aoloxide vs AOLserver vs axum

A reproducible HTTP/1.1 throughput and latency comparison between aoloxide,
the original C AOLserver 4.5 it ports (optional), and an
[axum](https://github.com/tokio-rs/axum) server that serves the same
endpoints with byte-identical bodies.  Latest results and analysis:
[docs/benchmarks.md](../docs/benchmarks.md).

```sh
sudo apt-get install wrk tcl8.6-dev tcl8.6
bench/run.sh                                 # aoloxide + axum, ~7 min
bench/build-aolserver.sh /opt/aolserver-bench          # optional: C reference, ~1 min
AOLSERVER_HOME=/opt/aolserver-bench bench/run.sh       # all four servers, ~9 min
DURATION=3 WARMUP=1 CONNS=64 bench/run.sh    # quick look
```

Results go to `bench/results/<host>-<date>.{md,csv}`.

## What is compared

| Scenario | Path | aoloxide / AOLserver | axum | axum-cached |
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
* **aolserver** (only with `AOLSERVER_HOME`) is the C original, built by
  [`build-aolserver.sh`](build-aolserver.sh) from upstream at a pinned commit
  against the same system Tcl 8.6, with its default `-O2` flags.  It runs the
  *same* `nsconfig.tcl` and `bench.tcl`, from its own `bin/init.tcl`.  `nsd`
  refuses to run as root, so under root `run.sh` starts it with `-u nobody`.
  AOLserver 4.5 predates Tcl 8.6: it logs one TclOO "Tcl exception" per new
  interp (not per request) and may crash while shutting down; neither affects
  the measurement.
* **axum** is [`axum-server/`](axum-server), a standalone crate (its own
  `[workspace]`, so axum is not a dependency of the aoloxide workspace).  It
  sets `TCP_NODELAY` on accepted sockets, as aoloxide's driver does; without it
  `ServeDir`'s separate header/body writes hit the Nagle/delayed-ACK 40 ms stall.
* **axum-cached** is the same binary with `cached`: it isolates "server speed"
  from "file cache" on the static scenarios, since fastpath serves from memory.

Before timing anything, `run.sh` fetches every path from every server and
fails unless the bodies hash identically to aoloxide's.

## Method

* Release builds (`lto = "thin"` for the Rust servers).
* The server under test is pinned to `SERVER_CPUS` (default `0-1`) and wrk to
  `CLIENT_CPUS` (default `2-3`) with `taskset`, so client and server do not
  fight for cores.  axum's tokio runtime sizes itself to the 2 pinned CPUs.
* wrk sends `Connection: keep-alive` to every server.  AOLserver 4.5 keeps a
  connection open only when the client asks (aoloxide deliberately keeps
  HTTP/1.1 connections alive by default, see `docs/porting.md`); without the
  header the C server would be measured on a new TCP connection per request.
* Per point: a `WARMUP`-second wrk run, then a `DURATION`-second run with
  `--latency`; a Lua `done` hook ([`run.sh`](run.sh)) reports req/s, p50/p90/p99/max and
  errors.  Concurrency sweeps `CONNS` (default `16 64 256`).
* **CPU µs/req** is the server process's `utime + stime` over the timed run
  divided by requests served: the cost of a request independent of how
  many cores were available.
* **ctxsw/req** is the change in the server's voluntary + involuntary context
  switches, per thread from `/proc/<pid>/task/*/status`, divided by requests.
  Threads that exit during a run are not counted (a slight undercount).
* **Peak RSS** is the server's `VmHWM` after all its runs.
* Raw wrk output and server logs go to `bench/results/raw/` (git-ignored).

## Caveats

* Loopback only; no network, TLS or request bodies.  Small payloads measure
  per-request overhead, not bandwidth.
* One machine, one run: treat differences under ~10% as noise.  Cloud VMs
  have noisy neighbours (run-to-run swings of 20% were seen on the VM behind
  `results/baseline.md`); compare servers within one run, and rerun before
  drawing fine conclusions.
