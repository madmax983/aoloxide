//! Request procedures, filters, traces and authorization (`nsd/op.c`,
//! `nsd/filter.c`, `nsd/auth.c`).

use std::sync::{Arc, PoisonError};

use ns_core::glob::string_match;
use ns_core::log::Severity;
use ns_core::UrlFlags;

use crate::conn::{ConnRef, Status};
use crate::server::Server;

/// A request procedure (`Ns_OpProc`).
pub type RequestProc = Arc<dyn Fn(&ConnRef) -> Status + Send + Sync>;

/// A registered request procedure.
#[derive(Clone)]
pub struct RequestEntry {
    /// The procedure.
    pub proc_: RequestProc,
    /// Description for listings (e.g. the Tcl proc name).
    pub desc: String,
}

impl std::fmt::Debug for RequestEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestEntry")
            .field("desc", &self.desc)
            .finish()
    }
}

/// Filter phases (`NS_FILTER_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FilterWhen {
    /// Before queueing (runs at the start of processing in this port).
    PreQueue,
    /// After the request is read (runs at the start of processing).
    Read,
    /// After each write.
    Write,
    /// Before authorization.
    PreAuth,
    /// After authorization.
    PostAuth,
    /// After the response, status-aware.
    Trace,
    /// After the response, result ignored (`ns_register_trace`).
    VoidTrace,
}

impl FilterWhen {
    /// Bit value for masks.
    pub const fn bit(self) -> u32 {
        match self {
            Self::PreAuth => 1,
            Self::PostAuth => 2,
            Self::Trace => 4,
            Self::VoidTrace => 8,
            Self::PreQueue => 16,
            Self::Read => 32,
            Self::Write => 64,
        }
    }

    /// Name passed to Tcl filter procs.
    pub const fn name(self) -> &'static str {
        match self {
            Self::PreQueue => "prequeue",
            Self::Read => "read",
            Self::Write => "write",
            Self::PreAuth => "preauth",
            Self::PostAuth => "postauth",
            Self::Trace | Self::VoidTrace => "trace",
        }
    }

    /// Parse a `when` name from `ns_register_filter`.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "prequeue" => Self::PreQueue,
            "read" => Self::Read,
            "write" => Self::Write,
            "preauth" => Self::PreAuth,
            "postauth" => Self::PostAuth,
            "trace" => Self::Trace,
            _ => return None,
        })
    }
}

/// A filter procedure (`Ns_FilterProc`).
pub type FilterProc = Arc<dyn Fn(&ConnRef, FilterWhen) -> Status + Send + Sync>;

/// A registered filter.
#[derive(Clone)]
pub struct Filter {
    /// Method glob.
    pub method: String,
    /// URL glob.
    pub url: String,
    /// Mask of [`FilterWhen::bit`] values.
    pub when: u32,
    /// The filter.
    pub proc_: FilterProc,
    /// Description for listings.
    pub desc: String,
}

impl std::fmt::Debug for Filter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Filter")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("when", &self.when)
            .field("desc", &self.desc)
            .finish()
    }
}

/// A trace or cleanup procedure (`Ns_TraceProc`).
pub type TraceProc = Arc<dyn Fn(&ConnRef) + Send + Sync>;

/// Arguments to an authorization procedure.
#[derive(Debug, Clone)]
pub struct AuthRequest {
    /// Server name.
    pub server: String,
    /// Method.
    pub method: String,
    /// URL.
    pub url: String,
    /// User, if supplied.
    pub user: Option<String>,
    /// Password, if supplied.
    pub passwd: Option<String>,
    /// Client address.
    pub peer: String,
}

/// An authorization procedure (`Ns_RequestAuthorizeProc`): returns
/// [`Status::Ok`], [`Status::Unauthorized`], [`Status::Forbidden`] or
/// [`Status::Error`].
pub type AuthorizeProc = Arc<dyn Fn(&AuthRequest) -> Status + Send + Sync>;

/// Register a request procedure (`Ns_RegisterRequest`).
pub fn register_request(
    server: &Server,
    method: &str,
    url: &str,
    entry: RequestEntry,
    flags: UrlFlags,
) {
    server
        .requests
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .set(method, url, Arc::new(entry), flags);
}

/// Unregister a request procedure (`Ns_UnRegisterRequest`).  With
/// `inherit`, the inheriting registration is removed, otherwise the
/// non-inheriting one.
pub fn unregister_request(
    server: &Server,
    method: &str,
    url: &str,
    noinherit: bool,
    recurse: bool,
) {
    server
        .requests
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(method, url, UrlFlags { noinherit, recurse });
}

/// Look up the procedure for a request (`Ns_GetRequest`).
pub fn get_request(server: &Server, method: &str, url: &str) -> Option<Arc<RequestEntry>> {
    server
        .requests
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .get(method, url)
        .cloned()
}

/// Register a proxy handler for `method protocol` (`Ns_RegisterProxyRequest`).
pub fn register_proxy(server: &Server, method: &str, protocol: &str, entry: RequestEntry) {
    server
        .proxies
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(format!("{method}{protocol}"), Arc::new(entry));
}

/// Remove a proxy handler.
pub fn unregister_proxy(server: &Server, method: &str, protocol: &str) {
    server
        .proxies
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&format!("{method}{protocol}"));
}

/// Run the request's procedure (`Ns_ConnRunRequest`).
pub fn run_request(conn: &ConnRef) -> Status {
    if conn.conn().flags.overflow {
        let _ = conn.return_unavailable();
        return Status::Ok;
    }
    if conn.conn().recursion > crate::conn::MAX_RECURSION {
        conn.server.global.log(
            Severity::Error,
            &format!(
                "return: failed to redirect '{} {}': exceeded recursion limit of {}",
                conn.method(),
                conn.url(),
                crate::conn::MAX_RECURSION
            ),
        );
        let _ = conn.return_internal_error();
        return Status::Ok;
    }
    let (method, url) = (conn.method(), conn.url());
    if let Some(entry) = get_request(&conn.server, &method, &url) {
        (entry.proc_)(conn)
    } else {
        let _ = conn.return_not_found();
        Status::Ok
    }
}

/// Run a proxy request (`NsConnRunProxyRequest`).
pub fn run_proxy_request(conn: &ConnRef) -> Status {
    let key = {
        let c = conn.conn();
        format!(
            "{}{}",
            c.request.method,
            c.request.protocol.as_deref().unwrap_or("")
        )
    };
    let entry = conn
        .server
        .proxies
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
        .cloned();
    if let Some(e) = entry {
        (e.proc_)(conn)
    } else {
        let _ = conn.return_not_found();
        Status::Ok
    }
}

/// Register a filter (`Ns_RegisterFilter`).
pub fn register_filter(server: &Server, filter: Filter) {
    server
        .filters
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .push(filter);
}

/// Run filters for phase `why` (`NsRunFilters`).  Returns `Ok`,
/// `FilterReturn` or `Error`.
pub fn run_filters(conn: &ConnRef, why: FilterWhen) -> Status {
    let filters: Vec<Filter> = conn
        .server
        .filters
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter(|f| f.when & why.bit() != 0)
        .cloned()
        .collect();
    if filters.is_empty() {
        return Status::Ok;
    }
    let (method, url) = (conn.method(), conn.url());
    let mut status = Status::Ok;
    for f in filters {
        if string_match(&method, &f.method) && string_match(&url, &f.url) {
            status = (f.proc_)(conn, why);
            if status != Status::Ok {
                break;
            }
        }
    }
    if status == Status::FilterBreak || (why == FilterWhen::Trace && status == Status::FilterReturn)
    {
        status = Status::Ok;
    }
    status
}

/// Register a server trace (`Ns_RegisterServerTrace`).
pub fn register_trace(server: &Server, proc_: TraceProc) {
    server
        .traces
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .push(proc_);
}

/// Register a cleanup (`Ns_RegisterConnCleanup`).
pub fn register_cleanup(server: &Server, proc_: TraceProc) {
    server
        .cleanups
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .push(proc_);
}

/// Run server traces (`NsRunTraces`).
pub fn run_traces(conn: &ConnRef) {
    let traces = conn
        .server
        .traces
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    for t in traces {
        t(conn);
    }
}

/// Run cleanups (`NsRunCleanups`).
pub fn run_cleanups(conn: &ConnRef) {
    let cleanups = conn
        .server
        .cleanups
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    for t in cleanups {
        t(conn);
    }
}

/// Install the server's authorization procedure (`Ns_SetRequestAuthorizeProc`).
pub fn set_authorize_proc(server: &Server, proc_: Option<AuthorizeProc>) {
    *server.auth.write().unwrap_or_else(PoisonError::into_inner) = proc_;
}

/// Authorize a request (`Ns_AuthorizeRequest`): `Ok` when no procedure is
/// installed.
pub fn authorize_request(server: &Server, req: &AuthRequest) -> Status {
    let p = server
        .auth
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    p.map_or(Status::Ok, |p| p(req))
}

/// Authorize the current request of `conn`.
pub fn authorize(conn: &ConnRef) -> Status {
    let req = {
        let c = conn.conn();
        AuthRequest {
            server: conn.server.name.clone(),
            method: c.request.method.clone(),
            url: c.request.url.clone(),
            user: c.auth_user.clone(),
            passwd: c.auth_passwd.clone(),
            peer: c.peer.ip().to_string(),
        }
    };
    authorize_request(&conn.server, &req)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn when_names_and_bits() {
        for w in [
            FilterWhen::PreQueue,
            FilterWhen::Read,
            FilterWhen::Write,
            FilterWhen::PreAuth,
            FilterWhen::PostAuth,
            FilterWhen::Trace,
        ] {
            assert_eq!(FilterWhen::parse(w.name()), Some(w));
        }
        assert_eq!(FilterWhen::parse("bogus"), None);
        assert_eq!(FilterWhen::VoidTrace.name(), "trace");
        let bits: u32 = [1, 2, 4, 8, 16, 32, 64].iter().sum();
        assert_eq!(bits, 127);
    }
}
