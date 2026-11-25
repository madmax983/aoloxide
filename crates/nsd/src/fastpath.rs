//! Static file serving (`nsd/fastpath.c`, `Ns_FastPathOp`).

use std::path::PathBuf;
use std::sync::{Arc, PoisonError};

use ns_core::httptime::{http_time, parse_http_time, unix_time};
use ns_core::log::Severity;
use ns_core::pathname::url_to_path;
use ns_core::UrlFlags;

use crate::conn::{ConnRef, Status};
use crate::op::{register_request, RequestEntry};
use crate::server::Server;

/// Map a URL to a file under the page root (`NsUrlToFile`).  The URL is
/// normalized so `..` can never escape the root.
pub fn url_to_file(server: &Server, url: &str) -> PathBuf {
    url_to_path(&server.fastpath.pageroot, url)
}

/// Register the fastpath handler for `method url` (`ns_register_fastpath`).
pub fn register(server: &Arc<Server>, method: &str, url: &str, flags: UrlFlags) {
    register_request(
        server,
        method,
        url,
        RequestEntry {
            proc_: Arc::new(fastpath_op),
            desc: "ns:fastpath".into(),
        },
        flags,
    );
}

/// The fastpath request procedure.
pub fn fastpath_op(conn: &ConnRef) -> Status {
    let url = conn.url();
    let path = url_to_file(&conn.server, &url);
    let Ok(meta) = std::fs::metadata(&path) else {
        let _ = conn.return_not_found();
        return Status::Ok;
    };
    if meta.is_file() {
        let _ = fast_return(conn, 200, None, &path);
        return Status::Ok;
    }
    if meta.is_dir() {
        for index in &conn.server.fastpath.dirv {
            let candidate = path.join(index);
            if candidate.is_file() {
                if url.ends_with('/') {
                    let target = format!("{url}{index}");
                    let _ = conn.redirect(&target);
                } else {
                    let _ = conn.return_redirect(Some(&format!("{url}/")));
                }
                return Status::Ok;
            }
        }
        if let Some(adp) = conn.server.fastpath.diradp.clone() {
            return crate::adp::adp_request(conn, &adp);
        }
        if let Some(proc_) = conn.server.fastpath.dirproc.clone() {
            return crate::interp::tcl_request(conn, &proc_);
        }
    }
    let _ = conn.return_not_found();
    Status::Ok
}

/// Return a file with caching and conditional GET (`FastReturn`).
pub fn fast_return(
    conn: &ConnRef,
    status: u16,
    ty: Option<&str>,
    path: &std::path::Path,
) -> std::io::Result<()> {
    let meta = match std::fs::metadata(path) {
        Ok(m) if m.is_file() => m,
        _ => return conn.return_not_found(),
    };
    let ty = ty.map_or_else(
        || {
            conn.server
                .global
                .mime
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .guess(&path.to_string_lossy())
                .to_owned()
        },
        str::to_owned,
    );
    let mtime = meta.modified().map(unix_time).unwrap_or_default();
    conn.update_header("Last-Modified", &http_time(mtime));
    if conn.server.opts.modsince {
        if let Some(ims) = conn
            .header("if-modified-since")
            .and_then(|h| parse_http_time(&h))
        {
            if mtime <= ims {
                return conn.return_not_modified();
            }
        }
    }
    let size = meta.len();
    if conn.conn().flags.skip_body {
        conn.set_type(Some(&ty));
        conn.set_required_headers(Some(&ty), Some(size));
        conn.queue_headers(status);
        conn.send(&[])?;
        conn.close();
        return Ok(());
    }
    let cacheable =
        conn.server.fastpath.cache.as_ref().filter(|_| {
            usize::try_from(size).is_ok_and(|s| s <= conn.server.fastpath.cachemaxentry)
        });
    if let Some(cache) = cacheable {
        let key = format!("{}:{}:{}", path.display(), mtime, size);
        let data = cache.get_or_compute(&key, None, || {
            std::fs::read(path).map(|d| {
                let n = d.len();
                (Arc::new(d), n)
            })
        });
        match data {
            Ok(bytes) => conn.return_data(status, &bytes, Some(&ty)),
            Err(e) => {
                conn.server.global.log(
                    Severity::Warning,
                    &format!("fastpath: failed to read '{}': {e:?}", path.display()),
                );
                conn.return_not_found()
            }
        }
    } else {
        let mut f = std::fs::File::open(path)?;
        conn.return_reader(status, Some(&ty), &mut f, Some(size))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    #[test]
    fn url_mapping_is_contained() {
        let root = Path::new("/srv");
        assert_eq!(
            ns_core::pathname::url_to_path(root, "/../x"),
            Path::new("/srv/x")
        );
    }
}
