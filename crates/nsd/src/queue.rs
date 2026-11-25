//! Connection queueing and processing (`nsd/queue.c`: `NsQueueConn`,
//! `ConnRun`).

use std::sync::{Arc, PoisonError};
use std::time::Instant;

use ns_core::log::Severity;
use ns_core::request::HeaderCase;
use ns_core::text::parse_basic_auth;

use crate::conn::{set_ref, Conn, ConnCtx, ConnFlags, ConnRef, ConnTimes, Status};
use crate::driver::Incoming;
use crate::op::{self, FilterWhen};
use crate::server::Server;

/// Queue a request on the pool its URL maps to.  Returns the request back
/// if it cannot be queued (shutdown or overflow), so the driver can answer
/// 503.
pub fn queue_conn(server: &Arc<Server>, incoming: Incoming) -> Result<(), Box<Incoming>> {
    let (method, url) = (
        incoming.request.method.clone(),
        incoming.request.url.clone(),
    );
    let limits = server.limits_for(&method, &url);
    if !limits.admit() {
        server.global.log(
            Severity::Warning,
            &format!(
                "queue: overflow for {method} {url} (limits {})",
                limits.name
            ),
        );
        return Err(Box::new(incoming));
    }
    let pool = server.pool_for(&method, &url);
    let srv = Arc::clone(server);
    let lim = Arc::clone(&limits);
    let pool_name = pool.name.clone();
    // The work closure must be Send: everything captured here is.
    let slot = Arc::new(std::sync::Mutex::new(Some(incoming)));
    let slot2 = Arc::clone(&slot);
    let ok = pool.enqueue(Box::new(move || {
        let Some(inc) = slot2.lock().unwrap_or_else(PoisonError::into_inner).take() else {
            return;
        };
        lim.start_run();
        run_incoming(&srv, inc, &lim, &pool_name);
        lim.end_run();
    }));
    if ok {
        Ok(())
    } else {
        limits.start_run();
        limits.end_run();
        slot.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .map_or(Ok(()), |i| Err(Box::new(i)))
    }
}

fn run_incoming(
    server: &Arc<Server>,
    inc: Incoming,
    limits: &Arc<crate::pool::Limits>,
    pool: &str,
) {
    let Incoming {
        sock,
        mut request,
        mut headers,
        content,
        peer,
        driver,
        accept_time,
        read_time,
    } = inc;
    let url_encoding = server.charsets.url_encoding;
    if url_encoding.is_some() {
        let raw = request.line.clone();
        if let Some(r) = ns_core::request::Request::parse(&raw, url_encoding) {
            request = r;
        }
    }
    match server.opts.hdrcase {
        HeaderCase::Preserve => {}
        case => {
            let mut s = ns_core::NsSet::named("headers");
            for (k, v) in headers.iter() {
                let k = if case == HeaderCase::ToLower {
                    k.to_ascii_lowercase()
                } else {
                    k.to_ascii_uppercase()
                };
                s.put(&k, v);
            }
            headers = s;
        }
    }
    let (auth_user, auth_passwd) = headers
        .iget("authorization")
        .and_then(parse_basic_auth)
        .map_or((None, None), |(u, p)| (Some(u), Some(p)));
    let flags = ConnFlags {
        skip_hdrs: request.major == 0,
        skip_body: request.method == "HEAD",
        ..ConnFlags::default()
    };
    let query_encoding = server.input_encoding_for(&request.method, &request.url);
    let too_large = content.len() > limits.config().maxupload;
    let conn = Conn {
        id: server.global.next_conn_id(),
        request,
        headers: set_ref(headers),
        output_headers: set_ref(ns_core::NsSet::named("outputheaders")),
        auth_user,
        auth_passwd,
        content,
        content_offset: 0,
        flags,
        status: 200,
        content_type: None,
        n_content_sent: 0,
        peer,
        driver,
        times: ConnTimes {
            accept: accept_time,
            queue: read_time,
            run: Instant::now(),
        },
        url_encoding,
        query_encoding,
        output_encoding: server.charsets.output_encoding,
        form: None,
        recursion: 0,
        sock: Some(sock),
        obuf: Vec::new(),
        cls: Vec::new(),
        limits: Some(Arc::clone(limits)),
        pool: pool.to_owned(),
    };
    let ctx = ConnCtx::new(Arc::clone(server), conn);
    if too_large {
        limits.drop_upload();
        let _ = ctx.return_too_large();
        finish(&ctx, Status::Ok);
        return;
    }
    let status = conn_run(&ctx);
    finish(&ctx, status);
}

/// The request pipeline (`ConnRun`): filters, authorization, the request
/// procedure.  Returns the status used to decide whether trace filters run.
pub fn conn_run(conn: &ConnRef) -> Status {
    let is_proxy = {
        let c = conn.conn();
        c.request.protocol.is_some() && c.request.host.is_some()
    };
    if is_proxy {
        return op::run_proxy_request(conn);
    }
    let mut status = op::run_filters(conn, FilterWhen::PreQueue);
    if status == Status::Ok {
        status = op::run_filters(conn, FilterWhen::Read);
    }
    if status == Status::Ok {
        status = op::run_filters(conn, FilterWhen::PreAuth);
    }
    if status == Status::Ok {
        match op::authorize(conn) {
            Status::Ok => {
                status = op::run_filters(conn, FilterWhen::PostAuth);
                if status == Status::Ok {
                    status = op::run_request(conn);
                }
            }
            Status::Forbidden => {
                let _ = conn.return_forbidden();
            }
            Status::Unauthorized => {
                let _ = conn.return_unauthorized();
            }
            _ => {
                let _ = conn.return_internal_error();
            }
        }
    } else if status != Status::FilterReturn {
        let _ = conn.return_internal_error();
        status = Status::FilterReturn;
    }
    status
}

fn finish(conn: &ConnRef, status: Status) {
    // A request proc that never responded still owes the client an answer.
    if !conn.is_closed() && !conn.headers_sent() {
        let _ = conn.return_internal_error();
    }
    conn.close();
    if (status == Status::Ok || status == Status::FilterReturn)
        && op::run_filters(conn, FilterWhen::Trace) == Status::Ok
    {
        let _ = op::run_filters(conn, FilterWhen::VoidTrace);
        op::run_traces(conn);
    }
    op::run_cleanups(conn);
    conn.free_interp();
}
