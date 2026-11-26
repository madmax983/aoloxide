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
| `nsd/tclmisc.c`, `tclhttp.c`, `tclxkeylist.c`, `tclloop.c`, `tclenv.c`, `tclimg.c` | `nsd/src/cmds/misc.rs` |
| `nsd/tclsock.c` | `nsd/src/cmds/sock.rs` (DNS commands in `cmds/misc.rs`) |
| `nsd/sock.c` | `nsd/src/sock.rs` |
| `nsd/sockcallback.c`, `listen.c` | `nsd/src/sockcallback.rs` |
| `Ns_QueueWait`, `RunQueWaits` (`nsd/driver.c`) | `nsd/src/quewait.rs` |
| `nsd/tclfile.c` | `nsd/src/cmds/file.rs` |
| `nsd/tclfile.c` (`ns_chan`), `tclshare.c` | `nsd/src/cmds/misc.rs`, `cmds/share.rs`; Tcl channel, trace and async API in `ns-tcl/src/extra.rs` |
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
| `nsext/nsext.c` | `crates/nsext` (`conn.rs`: the protocol client, `cmd.rs`: `ns_ext`) |
| `nsext/msg.c`, `include/nsextmsg.h` | `crates/nspd/src/msg.rs` |
| `nspd/main.c`, `listen.c`, `log.c`, `include/nspd.h` | `crates/nspd` (`src/bin/nssqlitepd.rs`: an SQLite daemon) |
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

12. `ns_info threads|locks|callbacks` return empty lists, and
    `ns_server active|all|keepalive` report minimal information.
13. `ns_thread name` reports but cannot rename the current thread.
14. `ns_http`/`ns_geturl` support plain `http://` only.

Modules:

15. `nsssl` is built on rustls and answers to both `nsssl` and
    `nsopenssl` module names; it accepts PEM `certfile`/`keyfile` only (no
    OpenSSL cipher strings or client-certificate options).
16. `nsdb` ships a SQLite driver (`sqlite`) in place of AOLserver's
    external C drivers.  The SQLite driver buffers a query's rows when it
    runs; on drivers without stored procedures (SQLite) `ns_db sp_*`
    on a connected handle return a "not supported" error (on a
    disconnected handle they fail with `Database operation "sp_..."
    failed`, as in AOLserver, whatever the driver).
17. `ns_proxy active pool` lists checked-out handles that are evaluating.
    AOLserver walked the pool's *free* list, so it always returned an
    empty list.
18. `nsproxy` workers speak a private length-prefixed protocol (the worker
    binary ships with the server).  Replies are read whole by a reader
    thread, so `-sendtimeout` and `-recvtimeout` are accepted and reported
    but have no effect; `-evaltimeout` and `-waittimeout` behave as in
    AOLserver.
19. **nscp:** The control port parses input as a byte stream, not per
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
20. **nszlib:** `ns_zlib uncompress` and `gunzip` decode their result
    as Tcl 8.6 reads bytes: valid UTF-8 is decoded, `C0 80` is NUL and any
    other invalid byte is its Latin-1 character.  Tcl also decodes
    UTF-8-encoded surrogates (`ED A0 80`) to lone UTF-16 surrogates; script
    strings here cannot hold those, so each such byte is its Latin-1
    character instead.
21. **nsdb:** A handle reconnected after `ns_db bouncepool` counts as current.
    AOLserver never updated a reconnected handle's bounce count, so a
    handle that was in use during a bounce was closed again on every later
    return to the pool.
22. **nsdb:** `ns_db` subcommands given the wrong number of arguments raise
    the "wrong # args" error.  For the handle subcommands (`select`,
    `releasehandle`, ...) AOLserver set that message but returned `TCL_OK`.
23. **nsdb:** Deleting an interp returns the handles it still holds to their
    pools (AOLserver's `FreeData` dropped them, shrinking the pool).  The
    SQLite driver treats a statement of only whitespace or comments (for
    example between two `;` in `interpretsqlfile`) as an empty DML command,
    and reports errors without a SQLite code as exception `NSDB`.
24. **nsproxy:** `ns_proxy config` accepts `-gettimeout` as an alias of
    AOLserver's `-getimeout` (the flag reported back is `-getimeout`).
25. **nsproxy:** A failed `ns_proxy get` returns its proxies the way
    `ns_proxy release` does: a proxy whose return leaves no spare capacity
    (`-max` was lowered while the handles were being checked) is freed
    rather than put on the free list, so idle workers never exceed the
    spare capacity (proved in `verus/proxy.rs`).  AOLserver put them all
    back on the free list.
26. **nsproxy:** When the ping or `-init` script of a handle being checked
    out fails, the proxy is reset to idle with its worker closed.
    AOLserver left a timed-out proxy marked busy, so the replacement
    worker's `-init` (or the caller's first `eval`) failed with `EBusy`.
27. **nsproxy:** Protocol messages are capped at 256 MiB, and the worker's
    private protocol descriptors are close-on-exec.  A closing worker is
    reaped by polling for its exit rather than for EOF on its result pipe;
    the `SIGTERM`/`SIGKILL` sequence and its warnings match AOLserver.
28. **channels:** `ns_chan put` refuses a channel that is registered in more
    than one interp ("channel is shared"), as `ns_chan create` does;
    AOLserver cut it from under the other interp.  `ns_chan cleanup` and
    `ns_chan put` find a channel by name and identity, so one the script
    already closed is skipped (AOLserver used the stale pointer), and
    `ns_chan get` of a channel whose name the interp already has fails
    instead of hitting Tcl's duplicate-name panic, and leaves the channel
    in the shared table.  `ns_chan put` to a name that was taken again
    since the `get` overwrites the entry as AOLserver did, but closes the
    channel it displaces (flushing its output) where AOLserver leaked it
    (see `verus/chan.rs`).  `ns_chan list` returns names in sorted order.
29. **channels:** `ns_conn channel` needs a plain TCP connection: on a TLS
    connection it fails, and `ns_conn sock` returns -1 there (AOLserver
    handed out the raw, encrypted socket).  A second `ns_conn channel`
    fails with "bad file number".  The channel wraps a duplicate of the
    socket descriptor (the original is owned by the async runtime until
    the channel exists, so a failure leaves the connection its socket),
    so its `sockN` name need not match the earlier `ns_conn sock`.
30. **channels:** `ns_ictl threads` reports each thread that created an interp
    as the number `N` of `ns_thread id`'s `ThreadId(N)`, and `ns_ictl
    cancel` accepts either form.  When Tcl's event loop runs the cancel
    handler with no interp it does nothing (AOLserver's `AsyncCancel`
    dereferenced the NULL interp).
31. **channels:** `ns_share` raises an error where AOLserver called `Ns_Fatal`
    (a trace that cannot be set, e.g. `ns_share ::nosuch::x`), and the
    "can't determine existence of variable" error no longer leaves the
    share table's critical section held.  Shared arrays are copied into
    an interp in sorted key order.
32. **channels:** The `ns_adp_output` variable that `ns_adp_debug` links to the
    ADP output buffer is always the global one and is refreshed by a
    variable trace; writing it restores the buffer's value instead of
    failing with "linked variable is read-only".

33. **sockets:** `ns_quewait` waits are polled in the connection
    thread, between the pre-queue and read filters, because pre-queue
    filters run there (item 7); waits may be registered only while
    pre-queue filters and queue-wait callbacks run (AOLserver: only in the
    driver thread), otherwise the command fails with "could not register
    sock wait" and logs "attempt to register quewait outside driver
    thread".  The timeout is relative to the call.  AOLserver stored the
    relative time where the driver expected an absolute one, so every wait
    timed out at once unless its socket was already ready.  As in
    AOLserver, input from the client while waiting counts as a drop: the
    callbacks get `dropped` and the connection closes without a response.
34. **sockets:** Socket descriptors made by the socket commands are
    close-on-exec, including the duplicates made by `ns_sockopen`,
    `ns_sockaccept`, listen callbacks and `ns_sockcallback` (AOLserver's
    `ns_sockdup` left them inheritable).  `ns_sockselect` uses `poll(2)`
    (no `FD_SETSIZE` limit), reporting what `select(2)` reports: readable
    includes hang-up and error, writable includes error.  OS errors are
    the system's `strerror` text in lower case, which can differ in
    wording from `Tcl_PosixError`.  A Tcl socket callback cancelled or
    replaced through the C-level API (`SockCallbacks::cancel`, or a new
    registration for the same descriptor) closes its descriptor and
    channel; AOLserver leaked them.
35. **sockets:** `ns_info sockcallbacks` lists the callback thread's table
    as of its last pass (AOLserver read the table while the thread could
    change it), so a new registration appears once the thread has woken.
    The callback thread waits for startup to complete, or for shutdown to
    begin (AOLserver waited for startup only).
36. **sockets:** Failures inside the socket callback system that AOLserver
    treats as fatal (`Ns_Fatal`, which stops the server) are logged
    instead: a failed `poll()` in the `-socks-` thread is retried after
    100 ms, and a failed trigger `send()` or `recv()` is logged and the
    thread carries on.  When the trigger socket pair or the thread cannot
    be created, the registration fails ("could not register callback"),
    and registrations other threads queued in the meantime, already
    accepted, are dropped, closing their descriptors (modelled as
    `abandon` in `verus/sockcallback.rs`).

37. **spooling:** A spooled body is read into memory on its first
    whole-body use (`ns_conn content` without a range, form parsing), where
    AOLserver `mmap`ed the temp file; `ns_conn content off len`,
    `ns_conn copy` and `ns_writecontent` read the file a piece at a time.
    `ns_conn contentchannel` opens the connection's temp file through
    `/dev/fd`: on Linux each channel has its own file offset starting at
    the beginning of the content, where AOLserver's `dup()` shared one
    offset, so a second channel opened after reading the first started at
    end of file.
38. **spooling:** A body refused for `maxupload` counts in the limits'
    `ndropped` (`ns_limits get`), which AOLserver never incremented.  A
    temp file that cannot be created or written is always logged as an
    error (AOLserver logged it only with the driver's `debug` on); in
    both, the socket is closed without a reply and no driver statistic
    changes.
39. **nsext:** Driver modules are compiled in and registered with
    `nsdb::register_driver`; `ns_param external nsext.so` in
    `ns/db/drivers` loads `nsext` from `ns/db/driver/external` as
    `Ns_DbDriverInit` did, when a pool first names the driver
    (`NsDbLoadDriver`); an entry no pool uses is never initialised.  A
    driver whose config is unusable is still registered (as in
    AOLserver) and its handles fail to open.
40. **nsext:** Replies are read through a buffer instead of being peeked
    (`MSG_PEEK`) and then read, so a reply line split across packets is
    no longer a protocol error.  Lines must still fit AOLserver's buffers
    (4096 bytes for a status, 32768 for a response, 256 for an `exec`
    reply, 32 for a list element's size).  An element size below `-1`
    is a protocol error (AOLserver used it as a length).  Exception codes
    are kept whole (AOLserver copied them into a 6-byte buffer), and an
    `ok` status does not clear the handle's exception message (every
    `ns_db` command clears the exception before it runs).
41. **nsext:** `spgetparams` reads the values list even when the names
    list is empty.  AOLserver stopped after an empty names list, leaving
    the values unread and the next reply out of step (the reply shapes
    are proved to agree in `verus/extproto.rs`).
42. **nsext:** `ns_ext cpto` and `cpfrom` report a failed copy even when
    the remote file then closes cleanly.  AOLserver returned the status
    of the final `closefile`, so a failed write could report success.
43. **nsext:** `ns_ext` on a handle whose daemon connection was dropped
    after an error raises `handle "nsdbN" is not connected` (AOLserver
    dereferenced the freed connection).
44. **nspd:** The daemon library serves each accepted connection on a
    thread of one process; AOLserver forked a process per connection.
    Tracing and open remote files belong to each connection.  Requests
    are read through a buffer (a header split across reads is not an
    error; a negative argument length ends the session, as a too-long
    one does).  `closefile`, `readfile` and `writefile` only accept
    descriptors that `openfile` returned on the same connection
    (AOLserver used any descriptor number, the daemon's own sockets
    included), `readfile` returns at most 4096 bytes (AOLserver
    overflowed its 4096-byte buffer), and a `writefile` with fewer data
    bytes than its count fails.  Newlines in reply lines become spaces.
45. **nspd:** `nssqlitepd` serves SQLite through nsdb's SQLite driver: it
    identifies as `SQLite Proxy Daemon v4.5`, `setmaxrows` limits the
    rows of later queries, `resultid` is empty and stored procedures
    fail with exception `NSPD`.  The datasource is the database file; as
    in AOLserver the daemon changes to `/`, so it should be absolute.

## Completeness

Every AOLserver 4.5 module and `ns_*` command is ported; none returns a
"not supported" error.  What remains different is listed under
"Intentional deviations" above.  Two gaps, both inherited from
AOLserver itself:

* `ns_db sp_*` (stored procedures) is not implemented by the SQLite
  driver or `nssqlitepd`; other `nsdb` drivers can implement it.
* `nsext` does not implement the `gettableinfo`, `tablelist` and
  `bestrowid` messages: AOLserver's never sent them, and `nspd` never
  answered them.

## Formal verification

`verus/` holds Verus specs and proofs, checked in CI:

| File | Proves |
|---|---|
| `ns_set.rs` | `find` returns the first match, `put` appends, `delete` preserves order |
| `glob.rs` | an executable `*`/`?` matcher equals the recursive spec; literal and `*` lemmas |
| `pool.rs` | pool transitions keep `idle ≤ current ≤ max`; enqueue never strands work while there is capacity |
| `cache.rs` | inserts keep total size ≤ max; eviction removes only LRU entries |
| `chan.rs` | `ns_chan` ownership: a channel is registered in some interp, held by the server's shared table, or closed, never two of these; `create`/`put` cut only an unshared channel, `get` gives it to exactly one interp |
| `spool.rs` | request-body read-ahead: `mem + disk = read ≤ len ≤ maxupload`, one sink only (a spooled body buffers nothing, an in-memory request stays under `maxinput`), receives never over-read; chunked bodies spill without losing bytes |
| `extproto.rs` | the nsext/nspd protocol: for every message nsext sends, the daemon's reply has the number of items nsext reads (on `ok` and on error); the connection returns to `Ready` after exactly one reply, never sends while a reply is outstanding, and stays `Dead` after an I/O or framing error; unanswered messages are never sent; list elements are read only for sizes within `maxElementSize` and a list ends only at `-1`.  nsext tracks the unread items of each reply from `Msg::reply` (the model's table) and drops a connection that falls out of step; the daemon is tested against the same table for every message |
| `proxy.rs` | nsproxy handle accounting (`avail + out = max`, idle workers ≤ spare capacity under any interleaving of get, release and config, including a config between a failed `get`'s pops and its give-back; a failed `get` restores the pool) and the `Idle → Busy → Done → Idle` proxy state machine |
| `sockcallback.rs` | socket callback registry accounting (`queued + active + done + released = accepted`, nothing held once stopped), registrations refused once shutdown is pending so `stop` releases every callback, removal of entries awaiting the sweep, dropping the queue when the thread cannot start, and the `Queued → Active → Done → Released` callback life cycle: never called after returning false or being released |

The production implementations are additionally checked against the same
specifications by property tests (`proptest`).

## Licensing

AOLserver is available under the AOLserver Public License 1.1 (a Mozilla
Public License 1.1 derivative) or the GPL.
A translation of its source is a derivative work, and the Tcl library files
shipped in `crates/*/tcl/` are AOLserver's own, so aoloxide is distributed
under AOLserver's own terms ([LICENSE](../LICENSE)).  The workspace is
declared `MPL-1.1 OR GPL-2.0-or-later`.
