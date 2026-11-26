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
| `nsd/fd.c` (`Ns_GetTemp`), `NsConnContent`, `Ns_ConnContentFd` | `nsd/src/content.rs` |
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
| `nsd/dstring.c`, `index.c`, `lisp.c`, `str.c`, `dsprintf.c`, `fd.c` (except temp files), `binder.c` | replaced by the Rust standard library |

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
   socket closes; AOLserver mostly dropped them silently.  This includes a
   request line and headers that do not end within the driver's
   `maxinput` (`E_RRANGE`, 400).  As in AOLserver, `maxheader` is read but
   not used.
4. **Bodies over the request's `ns_limits` `maxupload`** (default 10 MB)
   are rejected with 413 before they are read; AOLserver closed the socket
   without a reply.  As in AOLserver, a body whose request (line, headers,
   body and a trailing CRLF) reaches the driver's `maxinput` is spooled to
   an unlinked temp file (`Ns_GetTemp`) instead of memory, and the file
   returns to the temp-file pool when the connection is done.  A request
   without a `Content-Length` counts as a 0-byte body there, so its empty
   body is spooled too when the head alone reaches `maxinput`.  Temp
   files are created in `/tmp` (`P_tmpdir`, `TMPDIR` is ignored), and
   `nscgi` gives the CGI the temp file as its standard input.  Chunked
   bodies (deviation 2) take the same first decision, then spill to a
   temp file when the next chunk would reach `maxinput`.
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
20. **nscp:** The control port parses input as a byte stream, not per
    `recv()` chunk as `GetLine` does.  `IAC EOF`, `IAC IP`, a line that
    starts with EOT (Ctrl-D) and end of input (even after a partial line)
    still end the session, and any other telnet command still logs
    "unsupported telnet IAC code received from client" and ends it.  The
    differences: telnet commands are recognised anywhere in the input, not
    only at the start of a chunk, and option negotiation (`IAC
    WILL|WONT|DO|DONT opt`) is always skipped.  AOLserver tolerated only
    up to two `IAC WONT` replies per line, and flushed the client's replies
    to the password prompt's echo negotiation with a blocking `recv()`; a
    client that sent its password at once lost it to that flush.  Each
    line is read up to `\n` (CR and NUL bytes are dropped), so several
    lines arriving in one packet are evaluated one command at a time.
21. **nszlib:** `ns_zlib uncompress` and `gunzip` decode their result
    as Tcl 8.6 reads bytes: valid UTF-8 is decoded, `C0 80` is NUL and any
    other invalid byte is its Latin-1 character.  Tcl also decodes
    UTF-8-encoded surrogates (`ED A0 80`) to lone UTF-16 surrogates; script
    strings here cannot hold those, so each such byte is its Latin-1
    character instead.
22. **nsdb:** A handle reconnected after `ns_db bouncepool` counts as current.
    AOLserver never updated a reconnected handle's bounce count, so a
    handle that was in use during a bounce was closed again on every later
    return to the pool.
23. **nsdb:** `ns_db` subcommands given the wrong number of arguments raise
    the "wrong # args" error.  For the handle subcommands (`select`,
    `releasehandle`, ...) AOLserver set that message but returned `TCL_OK`.
24. **nsdb:** Deleting an interp returns the handles it still holds to their
    pools (AOLserver's `FreeData` dropped them, shrinking the pool).  The
    SQLite driver treats a statement of only whitespace or comments (for
    example between two `;` in `interpretsqlfile`) as an empty DML command,
    and reports errors without a SQLite code as exception `NSDB`.
25. **nsproxy:** `ns_proxy config` accepts `-gettimeout` as an alias of
    AOLserver's `-getimeout` (the flag reported back is `-getimeout`).
26. **nsproxy:** A failed `ns_proxy get` returns its proxies the way
    `ns_proxy release` does: a proxy whose return leaves no spare capacity
    (`-max` was lowered while the handles were being checked) is freed
    rather than put on the free list, so idle workers never exceed the
    spare capacity (proved in `verus/proxy.rs`).  AOLserver put them all
    back on the free list.
27. **nsproxy:** When the ping or `-init` script of a handle being checked
    out fails, the proxy is reset to idle with its worker closed.
    AOLserver left a timed-out proxy marked busy, so the replacement
    worker's `-init` (or the caller's first `eval`) failed with `EBusy`.
28. **nsproxy:** Protocol messages are capped at 256 MiB, and the worker's
    private protocol descriptors are close-on-exec.  A closing worker is
    reaped by polling for its exit rather than for EOF on its result pipe;
    the `SIGTERM`/`SIGKILL` sequence and its warnings match AOLserver.

29. **(spooling)** A spooled body is read into memory on its first
    whole-body use (`ns_conn content` without a range, form parsing), where
    AOLserver `mmap`ed the temp file; `ns_conn content off len`,
    `ns_conn copy` and `ns_writecontent` read the file a piece at a time.
    `ns_conn contentchannel` opens the connection's temp file through
    `/dev/fd`: on Linux each channel has its own file offset starting at
    the beginning of the content, where AOLserver's `dup()` shared one
    offset, so a second channel opened after reading the first started at
    end of file.
30. **(spooling)** A body refused for `maxupload` counts in the limits'
    `ndropped` (`ns_limits get`), which AOLserver never incremented.  A
    temp file that cannot be created or written is always logged as an
    error (AOLserver logged it only with the driver's `debug` on); in
    both, the socket is closed without a reply and no driver statistic
    changes.

## Not yet supported (phase 2)

These commands exist but return a clear `not supported` error:
`ns_share`, `ns_chan` (except `cleanup`/`list`), `ns_conn channel|
sock|interp`, `ns_sockaccept`, `ns_sockselect`,
`ns_sockcallback`, `ns_socklisten`, `ns_socklistencallback`,
`ns_socketpair`, `ns_quewait`, `ns_adp_debug`, `ns_adp_ctl channel`,
`ns_ictl cancel`.

Modules still to port:

| Module | Plan |
|---|---|
| `nsext`, `nspd` | external database proxy protocol, as an `nsdb` driver |

Also planned: `ns_sockcallback` on the tokio runtime, and channel transfer
for `ns_chan`.

## Formal verification

`verus/` holds Verus specs and proofs, checked in CI:

| File | Proves |
|---|---|
| `ns_set.rs` | `find` returns the first match, `put` appends, `delete` preserves order |
| `glob.rs` | an executable `*`/`?` matcher equals the recursive spec; literal and `*` lemmas |
| `pool.rs` | pool transitions keep `idle ≤ current ≤ max`; enqueue never strands work while there is capacity |
| `cache.rs` | inserts keep total size ≤ max; eviction removes only LRU entries |
| `spool.rs` | request-body read-ahead: `mem + disk = read ≤ len ≤ maxupload`, one sink only (a spooled body buffers nothing, an in-memory request stays under `maxinput`), receives never over-read; chunked bodies spill without losing bytes |
| `proxy.rs` | nsproxy handle accounting (`avail + out = max`, idle workers ≤ spare capacity under any interleaving of get, release and config, including a config between a failed `get`'s pops and its give-back; a failed `get` restores the pool) and the `Idle → Busy → Done → Idle` proxy state machine |

The production implementations are additionally checked against the same
specifications by property tests (`proptest`).

## Licensing

AOLserver is available under the AOLserver Public License 1.1 (a Mozilla
Public License 1.1 derivative) or the GPL.
A translation of its source is a derivative work, and the Tcl library files
shipped in `crates/*/tcl/` are AOLserver's own, so aoloxide is distributed
under AOLserver's own terms ([LICENSE](../LICENSE)).  The workspace is
declared `MPL-1.1 OR GPL-2.0-or-later`.
