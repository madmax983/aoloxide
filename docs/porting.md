# Porting notes

This document maps AOLserver 4.5 sources to the Rust port, lists every
intentional behavioural deviation, and tracks what is not yet ported.

## Source map

| AOLserver | aoloxide |
|---|---|
| `nsd/set.c`, `tclset.c` | `ns-core/src/set.rs`, `nsd/src/cmds/set.rs` |
| `nsd/urlspace.c` | `ns-core/src/urlspace.rs` |
| `Tcl_StringMatch` | `ns-core/src/glob.rs` |
| `nsd/request.c` | `ns-core/src/request.rs` |
| `nsd/pathname.c` | `ns-core/src/pathname.rs` |
| `nsd/urlencode.c` | `ns-core/src/urlencode.rs` |
| `nsd/httptime.c` | `ns-core/src/httptime.rs` |
| `nsd/quotehtml.c`, `uuencode.c` | `ns-core/src/text.rs` |
| `nsd/crypt.c` | `ns-core/src/crypt.rs` |
| `nsd/dns.c` | `ns-core/src/dns.rs` |
| `nsd/mimetypes.c` | `ns-core/src/mimetypes.rs` |
| `nsd/config.c`, `tclconf.c` | `ns-core/src/config.rs`, `nsd/src/lib.rs::eval_config` |
| `nsd/cache.c`, `tclcache.c` | `ns-core/src/cache.rs`, `nsd/src/tclcache.rs`, `cmds/cache.rs` |
| `nsd/log.c`, `rollfile.c` | `ns-core/src/log.rs` |
| `nsd/form.c` | `ns-core/src/form.rs` |
| `nsd/tclinit.c`, `init.tcl` | `nsd/src/interp.rs`, `nsd/tcl/init.tcl` |
| `nsd/nsconf.c`, `info.c` | `nsd/src/global.rs`, `cmds/info.rs` |
| `nsd/server.c` | `nsd/src/server.rs` |
| `nsd/driver.c`, `nssock/` | `nsd/src/driver.rs` |
| `nsd/queue.c`, `pools.c`, `limits.c` | `nsd/src/queue.rs`, `pool.rs` |
| `nsd/conn.c`, `connio.c`, `return.c` | `nsd/src/conn.rs`, `cmds/conn.rs` |
| `nsd/op.c`, `filter.c`, `auth.c` | `nsd/src/op.rs` |
| `nsd/tclrequest.c` | `nsd/src/cmds/register.rs` |
| `nsd/fastpath.c` | `nsd/src/fastpath.rs` |
| `nsd/adpparse.c`, `adpeval.c`, `adprequest.c`, `adpcmds.c` | `nsd/src/adp/`, `cmds/adp.rs` |
| `nsd/sched.c`, `tclsched.c` | `nsd/src/sched.rs`, `cmds/sched.rs` |
| `nsd/callbacks.c` | `nsd/src/callbacks.rs` |
| `nsd/tcljob.c` | `nsd/src/job.rs`, `cmds/thread.rs` |
| `nsd/tclthread.c`, `nsthread/` | `nsd/src/sync.rs`, `cmds/thread.rs` (std threads) |
| `nsd/tclvar.c` | `nsd/src/cmds/nsv.rs` |
| `nsd/tclmisc.c`, `tclhttp.c`, `tclxkeylist.c`, `tclloop.c`, `tclenv.c`, `tclimg.c`, `tclsock.c` | `nsd/src/cmds/misc.rs` |
| `nsd/tclfile.c` | `nsd/src/cmds/file.rs` |
| `nsd/modload.c` | `nsd/src/module.rs` |
| `nsd/nsmain.c`, `main.c`, `unix.c`, `pidfile.c` | `nsd/src/lib.rs`, `aoloxide/src/main.rs` |
| `nslog/` | `crates/nslog` |
| `nsperm/` | `crates/nsperm` |
| `nscgi/` | `crates/nscgi` |
| `nsssl/` (and `nsopenssl`) | `crates/nsssl` (rustls `StreamWrapper` over the socket driver) |
| `nszlib/` | `crates/nszlib` |
| `nscp/` | `crates/nscp` |
| `nsdb/` | `crates/nsdb` (`driver.rs`: driver trait + SQLite) |
| `nsproxy/nsproxylib.c`, `nsproxy.c` | `crates/nsproxy` (`src/bin/nsproxy.rs` is the worker) |
| `nsd/dstring.c`, `index.c`, `lisp.c`, `str.c`, `dsprintf.c`, `fd.c`, `binder.c` | replaced by the Rust standard library |

## Intentional deviations

HTTP handling:

1. **HTTP/1.1 keep-alive by default.** AOLserver 4.5 kept a connection
   alive only for `GET` requests carrying `Connection: keep-alive` with a
   200/304 response.  aoloxide also keeps HTTP/1.1 connections alive unless
   the client sends `Connection: close`, allows `HEAD`, and allows any
   response with a `Content-Length` or chunked encoding.
2. **Chunked request bodies** are decoded and **`Expect: 100-continue`** is
   honoured.
3. **Malformed requests get a status line** (400, 413, 414) before the
   socket closes; AOLserver mostly dropped them silently.
4. **Bodies over `maxinput`** are rejected with 413.  There is no temp-file
   spooling yet (phase 2).
5. **`+` in a URL path is a literal plus.**  AOLserver decoded `+` as a
   space in paths as well as in query strings.
6. **A request that produced no response gets a 500** (for example a
   request proc that never calls `ns_return*`, or an ADP page aborted
   before any output).  AOLserver closed the socket with no response.
7. `NS_FILTER_PRE_QUEUE` and `NS_FILTER_READ` filters run at the start of
   connection processing, in the connection thread, rather than in the
   driver thread (they need an interp; see ADR 0003).

Server behaviour:

8. Without a `directoryfile` setting the index files are
   `index.adp,index.tcl,index.html,index.htm` (AOLserver: none).
9. URL-space removal resolves the channel exactly as registration does.
   AOLserver's `JunctionDelete` could remove an entry from a different,
   glob-matching channel.
10. `nsperm` adds `ns_perm checkpass`, `setpass` and `listusers`.  The
    shipped `compat.tcl` called `checkpass`, which AOLserver's C never
    implemented.
11. `init.tcl` skips TclOO's `::oo` namespaces when saving interp state.
    On Tcl 8.6 re-creating them fails and broke every new interp.

Script API:

12. `ns_sockopen` returns the same bidirectional Tcl channel twice
    (`[list $chan $chan]`).
13. `ns_info threads|locks|callbacks|sockcallbacks` return empty lists, and
    `ns_server active|all|keepalive` report minimal information.
14. `ns_thread name` reports but cannot rename the current thread.
15. `ns_http`/`ns_geturl` support plain `http://` only.

Modules:

16. `nsssl` is built on rustls and answers to both `nsssl` and
    `nsopenssl` module names; it accepts PEM `certfile`/`keyfile` only (no
    OpenSSL cipher strings or client-certificate options).
17. `nsdb` ships a SQLite driver (`sqlite`) in place of AOLserver's
    external C drivers.  Drivers buffer a query's rows when it runs;
    `ns_db sp_*` (stored procedures) return a "not supported" error.
18. `ns_proxy active pool` lists checked-out handles that are evaluating.
    AOLserver walked the pool's *free* list, so it always returned an
    empty list.
19. `nsproxy` workers speak a private length-prefixed protocol (the worker
    binary ships with the server).  Replies are read whole by a reader
    thread, so `-sendtimeout` and `-recvtimeout` are accepted and reported
    but have no effect; `-evaltimeout` and `-waittimeout` behave as in
    AOLserver.

## Not yet supported (phase 2)

These commands exist but return a clear `not supported` error:
`ns_share`, `ns_chan` (except `cleanup`/`list`), `ns_conn channel|
contentchannel|sock|interp`, `ns_sockaccept`, `ns_sockselect`,
`ns_sockcallback`, `ns_socklisten`, `ns_socklistencallback`,
`ns_socketpair`, `ns_quewait`, `ns_adp_debug`, `ns_adp_ctl channel`,
`ns_ictl cancel`.

Modules still to port:

| Module | Plan |
|---|---|
| `nsext`, `nspd` | external database proxy protocol, as an `nsdb` driver |

Also planned: spooling large uploads to temp files, `ns_sockcallback` on
the tokio runtime, and channel transfer for `ns_chan`.

## Formal verification

`verus/` holds Verus specs and proofs, checked in CI:

| File | Proves |
|---|---|
| `ns_set.rs` | `find` returns the first match, `put` appends, `delete` preserves order |
| `glob.rs` | an executable `*`/`?` matcher equals the recursive spec; literal and `*` lemmas |
| `pool.rs` | pool transitions keep `idle ≤ current ≤ max`; enqueue never strands work while there is capacity |
| `cache.rs` | inserts keep total size ≤ max; eviction removes only LRU entries |
| `proxy.rs` | nsproxy handle accounting (`avail + out = max`, idle workers ≤ spare capacity, failed `get` restores the pool) and the `Idle → Busy → Done → Idle` proxy state machine |

The production implementations are additionally checked against the same
specifications by property tests (`proptest`).

## Licensing

AOLserver is available under the AOLserver Public License 1.1 (a Mozilla
Public License 1.1 derivative) or the GPL ([LICENSE-AOLSERVER](../LICENSE-AOLSERVER)).
A translation of its source is a derivative work, and the Tcl library files
shipped in `crates/*/tcl/` are AOLserver's own.  The workspace is therefore
declared `MPL-1.1 OR GPL-2.0-or-later`.  The repository was created with an
Apache-2.0 `LICENSE` file, which predates the port.  The owner should decide
whether to replace it (Apache-2.0 alone is not compatible with MPL 1.1's
file-level copyleft for the derived files).
