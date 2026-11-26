//! axum baseline for the aoloxide benchmark (`bench/README.md`).
//!
//! Serves the same endpoints, with the same response bodies, as the
//! aoloxide side of the benchmark (`bench/aoloxide/`):
//!
//! | Path | Response |
//! |---|---|
//! | `GET /hello` | `Hello, World!` as `text/plain` |
//! | `GET /page.adp?name=N` | the small HTML page rendered by `page.adp` |
//! | `GET /*` | static files from the page root |
//!
//! Usage: `axum-bench-server <addr:port> <pageroot> [cached]`.
//!
//! Static files are served by `tower-http`'s `ServeDir` (open + read per
//! request, the idiomatic choice).  With `cached`, the page root is instead
//! read into memory at startup, which is what aoloxide's fastpath cache does.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::serve::ListenerExt;
use axum::Router;
use serde::Deserialize;
use tower_http::services::ServeDir;

/// Query string of `/page.adp`.
#[derive(Deserialize)]
struct PageQuery {
    name: Option<String>,
}

/// `GET /hello`: the plaintext case.
async fn hello() -> &'static str {
    "Hello, World!"
}

/// `GET /page.adp`: the same output as `bench/www/page.adp`.
async fn page(Query(q): Query<PageQuery>) -> Html<String> {
    let name = q.name.unwrap_or_else(|| "world".to_owned());
    Html(render_page(&name))
}

/// Render the templated page (kept byte-identical to `page.adp`).
fn render_page(name: &str) -> String {
    let mut out = String::with_capacity(512);
    out.push_str("<html><body><h1>Hello, ");
    out.push_str(&quote_html(name));
    out.push_str("</h1><ul>");
    for i in 1..=10 {
        // Writing to a String cannot fail.
        let _ = write!(out, "<li>row {i}</li>");
    }
    out.push_str("</ul></body></html>\n");
    out
}

/// Escape like `AOLserver`'s `ns_quotehtml`.
fn quote_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&#34;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// In-memory page root: URL path → (content type, body).
type FileCache = HashMap<String, (HeaderValue, Bytes)>;

/// Read every regular file under `root` into memory, keyed by URL path.
fn load_cache(root: &Path) -> Result<FileCache> {
    fn walk(root: &Path, dir: &Path, out: &mut FileCache) -> Result<()> {
        for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
            let path = entry?.path();
            if path.is_dir() {
                walk(root, &path, out)?;
            } else if path.is_file() {
                let rel = path
                    .strip_prefix(root)?
                    .to_string_lossy()
                    .replace('\\', "/");
                let mime = mime_guess::from_path(&path).first_or_octet_stream();
                let ctype = HeaderValue::from_str(mime.as_ref())?;
                out.insert(
                    format!("/{rel}"),
                    (ctype, Bytes::from(std::fs::read(&path)?)),
                );
            }
        }
        Ok(())
    }
    let mut out = HashMap::new();
    walk(root, root, &mut out)?;
    Ok(out)
}

/// Fallback for `cached` mode: serve a preloaded file or 404.
async fn cached_file(State(cache): State<Arc<FileCache>>, uri: Uri) -> Response {
    cache.get(uri.path()).map_or_else(
        || StatusCode::NOT_FOUND.into_response(),
        |(ctype, body)| ([(header::CONTENT_TYPE, ctype.clone())], body.clone()).into_response(),
    )
}

/// The dynamic routes shared by both static-file modes.
fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/hello", get(hello))
        .route("/page.adp", get(page))
}

/// Build the router serving `pageroot` (from memory when `cache` is given).
fn app(pageroot: &str, cache: Option<FileCache>) -> Router {
    match cache {
        Some(c) => routes().fallback(cached_file).with_state(Arc::new(c)),
        None => routes().fallback_service(ServeDir::new(pageroot)),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let addr: SocketAddr = args
        .next()
        .unwrap_or_else(|| "127.0.0.1:8081".to_owned())
        .parse()
        .context("bad listen address")?;
    let root = args.next().unwrap_or_else(|| "www".to_owned());
    let cache = match args.next().as_deref() {
        None => None,
        Some("cached") => Some(load_cache(Path::new(&root))?),
        Some(other) => anyhow::bail!("unknown mode {other:?} (expected `cached`)"),
    };
    let mode = if cache.is_some() {
        "in-memory"
    } else {
        "ServeDir"
    };
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?
        // aoloxide's driver sets TCP_NODELAY on every accepted socket; do the
        // same, or ServeDir's separate header/body writes stall on Nagle.
        .tap_io(|tcp| {
            if let Err(e) = tcp.set_nodelay(true) {
                eprintln!("set_nodelay: {e}");
            }
        });
    eprintln!("axum-bench-server listening on {addr}, serving {root} ({mode})");
    axum::serve(listener, app(&root, cache))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_matches_adp_output() {
        let expected = concat!(
            "<html><body><h1>Hello, bench</h1><ul>",
            "<li>row 1</li><li>row 2</li><li>row 3</li><li>row 4</li><li>row 5</li>",
            "<li>row 6</li><li>row 7</li><li>row 8</li><li>row 9</li><li>row 10</li>",
            "</ul></body></html>\n"
        );
        assert_eq!(render_page("bench"), expected);
    }

    #[test]
    fn quote_html_escapes_specials() {
        assert_eq!(
            quote_html(r#"<a href="x">&'"#),
            "&lt;a href=&#34;x&#34;&gt;&amp;&#39;"
        );
        assert_eq!(quote_html("plain"), "plain");
    }

    #[test]
    fn load_cache_reads_tree_with_mime_types() -> Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("a.txt"), b"hello")?;
        std::fs::create_dir(dir.path().join("sub"))?;
        std::fs::write(dir.path().join("sub/b.html"), b"<p>")?;
        let cache = load_cache(dir.path())?;
        assert_eq!(cache.len(), 2);
        let (ctype, body) = &cache["/a.txt"];
        assert_eq!(ctype, "text/plain");
        assert_eq!(body.as_ref(), b"hello");
        assert_eq!(cache["/sub/b.html"].0, "text/html");
        assert!(load_cache(&dir.path().join("missing")).is_err());
        Ok(())
    }
}
