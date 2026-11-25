//! Virtual servers (`nsd/server.c`, `struct NsServer`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use encoding_rs::Encoding;
use ns_core::cache::Cache;
use ns_core::request::HeaderCase;
use ns_core::urlencode::charset_encoding;
use ns_core::{Config, UrlFlags, UrlSpace};
use ns_script::Command;

use crate::conn::{ConnCtx, SetRef};
use crate::global::Global;
use crate::interp::InterpTrace;
use crate::op::{AuthorizeProc, Filter, RequestEntry, TraceProc};
use crate::pool::{Limits, Pool};

/// Server-wide response options (`servPtr->opts`).
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct ServerOpts {
    /// Basic-auth realm.
    pub realm: String,
    /// Advertise AOLpress support.
    pub aolpress: bool,
    /// Allow chunked output for streamed responses.
    pub chunked: bool,
    /// Honour `If-Modified-Since`.
    pub modsince: bool,
    /// Add server details to notice pages.
    pub notice_detail: bool,
    /// Allow gzip output.
    pub gzip: bool,
    /// Minimum size to gzip.
    pub gzip_min: usize,
    /// Gzip level 0..=9.
    pub gzip_level: u32,
    /// Header key case.
    pub hdrcase: HeaderCase,
}

/// Character set configuration.
#[derive(Debug, Clone, Default)]
pub struct Charsets {
    /// Encoding for decoding URLs.
    pub url_encoding: Option<&'static Encoding>,
    /// Default charset name appended to `text/` types.
    pub output_charset: Option<String>,
    /// Encoding for text output.
    pub output_encoding: Option<&'static Encoding>,
    /// Encoding for decoding form input.
    pub input_encoding: Option<&'static Encoding>,
}

/// Size limits (`servPtr->limits`).
#[derive(Debug, Clone, Copy)]
pub struct ServerLimits {
    /// Minimum size of error pages (pads for IE "friendly errors").
    pub errorminsize: usize,
    /// Connections per thread before exit (0 = unlimited).
    pub connsperthread: u64,
}

/// Fastpath configuration (`servPtr->fastpath`).
#[derive(Debug)]
pub struct FastpathConfig {
    /// Document root.
    pub pageroot: PathBuf,
    /// Directory index files.
    pub dirv: Vec<String>,
    /// Proc to list directories (`directoryproc`).
    pub dirproc: Option<String>,
    /// ADP to list directories (`directoryadp`).
    pub diradp: Option<String>,
    /// Cache of small files.
    pub cache: Option<Cache<Arc<Vec<u8>>>>,
    /// Largest file put in the cache.
    pub cachemaxentry: usize,
}

/// ADP configuration (`servPtr->adp`).
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct AdpConfig {
    /// Error page ADP.
    pub errorpage: Option<String>,
    /// Start page ADP run before every page.
    pub startpage: Option<String>,
    /// Send `Expires: now`.
    pub expire: bool,
    /// Enable debugging hooks.
    pub debug: bool,
    /// Evaluate in a safe interp (unsupported; logged).
    pub safe: bool,
    /// Combine blocks into one script.
    pub single: bool,
    /// Gzip ADP output.
    pub gzip: bool,
    /// Trace execution.
    pub trace: bool,
    /// Log connection details on error.
    pub detail: bool,
    /// Abort on first error.
    pub strict: bool,
    /// Show errors in the output.
    pub display: bool,
    /// Trim leading whitespace of text blocks.
    pub trim: bool,
    /// Abort when the client disconnects mid-flush.
    pub autoabort: bool,
    /// Output buffer size before auto-flush (0 = buffer all).
    pub bufsize: usize,
    /// Parsed-page cache size.
    pub cachesize: usize,
    /// Trace output size.
    pub tracesize: usize,
    /// Debug init proc.
    pub debuginit: String,
    /// Default mime type.
    pub defaultmime: Option<String>,
}

/// A registered ADP tag (`ns_adp_registertag` / `registeradp` /
/// `registerscript`).
#[derive(Debug, Clone)]
pub struct AdpTag {
    /// Tag name (lower case).
    pub name: String,
    /// End tag, if the tag encloses content.
    pub endtag: Option<String>,
    /// What to do with the tag.
    pub kind: AdpTagKind,
}

/// How a registered tag is processed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdpTagKind {
    /// Call a proc with the attribute set (and content) — `registerproc`.
    Proc(String),
    /// Substitute ADP text — `registeradp`.
    Adp(String),
    /// Evaluate a script with the content — `registerscript`.
    Script(String),
}

/// Per-server Tcl state (`servPtr->tcl`).
pub struct TclState {
    /// Private library directory.
    pub library: PathBuf,
    /// Bootstrap script.
    pub initfile: PathBuf,
    /// The saved interp init script (`ns_ictl save`).
    pub script: RwLock<String>,
    /// Epoch of the saved script.
    pub epoch: AtomicU64,
    /// Interp traces.
    pub traces: RwLock<Vec<InterpTrace>>,
    /// `ns_ictl once` names already run.
    pub once: Mutex<HashSet<String>>,
    /// Packages required via `ns_ictl package require`.
    pub packages: Mutex<BTreeMap<String, String>>,
    /// Modules added with `ns_ictl addmodule`.
    pub modules: Mutex<Vec<String>>,
    /// Commands installed into every interp.
    pub commands: RwLock<Vec<(String, Command)>>,
}

impl std::fmt::Debug for TclState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TclState")
            .field("library", &self.library)
            .field("initfile", &self.initfile)
            .finish_non_exhaustive()
    }
}

/// Shared variables (`nsv_*`), hashed into buckets.
#[derive(Debug)]
pub struct Nsv {
    buckets: Vec<Mutex<HashMap<String, BTreeMap<String, String>>>>,
}

impl Nsv {
    /// `n` buckets (at least one).
    pub fn new(n: usize) -> Self {
        Self {
            buckets: (0..n.max(1)).map(|_| Mutex::new(HashMap::new())).collect(),
        }
    }

    fn bucket(&self, array: &str) -> &Mutex<HashMap<String, BTreeMap<String, String>>> {
        let h = array.bytes().fold(0usize, |a, b| {
            a.wrapping_mul(31).wrapping_add(usize::from(b))
        });
        &self.buckets[h % self.buckets.len()]
    }

    /// Run `f` with the array (creating it if `create`).
    pub fn with<R>(
        &self,
        array: &str,
        create: bool,
        f: impl FnOnce(Option<&mut BTreeMap<String, String>>) -> R,
    ) -> R {
        let mut b = self
            .bucket(array)
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if create {
            f(Some(b.entry(array.to_owned()).or_default()))
        } else {
            f(b.get_mut(array))
        }
    }

    /// Remove an array.
    pub fn remove_array(&self, array: &str) -> bool {
        self.bucket(array)
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(array)
            .is_some()
    }

    /// All array names.
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .buckets
            .iter()
            .flat_map(|b| {
                b.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .collect();
        v.sort();
        v
    }
}

/// Computes a connection location (`Ns_SetConnLocationProc`).
pub type LocationProc = Arc<dyn Fn(&ConnCtx) -> Option<String> + Send + Sync>;

/// A virtual server.
pub struct Server {
    /// Server name.
    pub name: String,
    /// Process-wide state.
    pub global: Arc<Global>,
    /// Response options.
    pub opts: ServerOpts,
    /// Charsets.
    pub charsets: Charsets,
    /// Limits.
    pub limits: ServerLimits,
    /// Fastpath.
    pub fastpath: FastpathConfig,
    /// ADP.
    pub adp: AdpConfig,
    /// Tcl.
    pub tcl: TclState,
    /// Registered request procs.
    pub requests: RwLock<UrlSpace<Arc<RequestEntry>>>,
    /// Proxy handlers keyed by method+protocol.
    pub proxies: RwLock<HashMap<String, Arc<RequestEntry>>>,
    /// Authorization proc.
    pub auth: RwLock<Option<AuthorizeProc>>,
    /// Status-code redirects.
    pub redirects: RwLock<HashMap<u16, String>>,
    /// Filters in registration order.
    pub filters: RwLock<Vec<Filter>>,
    /// Server traces.
    pub traces: RwLock<Vec<TraceProc>>,
    /// Conn cleanups.
    pub cleanups: RwLock<Vec<TraceProc>>,
    /// URL → pool name.
    pub url2pool: RwLock<UrlSpace<String>>,
    /// URL → limits name.
    pub url2limits: RwLock<UrlSpace<String>>,
    /// URL → input charset (`ns_register_encoding`).
    pub url2encoding: RwLock<UrlSpace<String>>,
    /// Registered ADP tags.
    pub adp_tags: RwLock<HashMap<String, AdpTag>>,
    /// Parsed ADP page cache: path → (mtime, len, code).
    pub adp_pages: Mutex<HashMap<PathBuf, (i64, u64, Arc<crate::adp::AdpCode>)>>,
    /// Shared `ns_set`s.
    pub shared_sets: Mutex<HashMap<String, SetRef>>,
    next_set: AtomicU64,
    /// `nsv_*` arrays.
    pub nsv: Nsv,
    /// `ns_var` variables.
    pub vars: Mutex<HashMap<String, String>>,
    location_proc: RwLock<Option<LocationProc>>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

fn charset(c: &Config, path: &str, key: &str) -> (Option<String>, Option<&'static Encoding>) {
    let name = c
        .get(path, key)
        .or_else(|| c.get(crate::global::PARAMS, key))
        .map(str::to_owned);
    let enc = name.as_deref().and_then(charset_encoding);
    (name, enc)
}

impl Server {
    /// Create a server from config (`CreateServer`).
    pub fn new(global: &Arc<Global>, name: &str) -> Arc<Self> {
        let c = &global.config;
        let path = Config::make_path(Some(name), None, &[]);
        let bool_or = |p: &str, k: &str, d: bool| c.bool_or(p, k, d);
        let hdrcase = c
            .get(&path, "headercase")
            .map_or(HeaderCase::Preserve, HeaderCase::from_config);
        let opts = ServerOpts {
            realm: c.string_or(&path, "realm", name),
            aolpress: bool_or(&path, "enableaolpress", false),
            chunked: bool_or(&path, "chunked", true),
            modsince: bool_or(&path, "checkmodifiedsince", true),
            notice_detail: bool_or(&path, "noticedetail", true),
            gzip: bool_or(&path, "gzip", true),
            gzip_min: c
                .get_int(&path, "gzipmin")
                .filter(|&i| i > 0)
                .unwrap_or(4 * 1024) as usize,
            gzip_level: c
                .get_int(&path, "gziplevel")
                .filter(|i| (0..=9).contains(i))
                .unwrap_or(4) as u32,
            hdrcase,
        };
        let (_, url_encoding) = charset(c, &path, "urlcharset");
        let (output_charset, output_encoding) = charset(c, &path, "outputcharset");
        let (_, input) = charset(c, &path, "inputcharset");
        let charsets = Charsets {
            url_encoding,
            output_charset,
            output_encoding,
            input_encoding: input.or(output_encoding),
        };
        let limits = ServerLimits {
            errorminsize: c.int_or(&path, "errorminsize", 514).max(0) as usize,
            connsperthread: c.int_or(&path, "connsperthread", 0).max(0) as u64,
        };
        let home = |parts: &[&str]| global.home_path(parts);
        let tclpath = Config::make_path(Some(name), None, &["tcl"]);
        let library = c
            .get(&tclpath, "library")
            .map_or_else(|| home(&["servers", name, "modules", "tcl"]), PathBuf::from);
        let initfile = c
            .get(&tclpath, "initfile")
            .map_or_else(|| home(&["bin", "init.tcl"]), PathBuf::from);
        let fp = Config::make_path(Some(name), None, &["fastpath"]);
        let cache = if c.bool_or(&fp, "cache", true) {
            let n = c.int_or(&fp, "cachemaxsize", 5 * 1024 * 1000).max(0) as usize;
            Some((
                Cache::new(
                    &format!("ns:fastpath:{name}"),
                    n,
                    Some(Duration::from_secs(60)),
                ),
                n,
            ))
        } else {
            None
        };
        let cachemaxentry = c
            .get_int(&fp, "cachemaxentry")
            .filter(|&i| i >= 0)
            .map_or_else(|| cache.as_ref().map_or(0, |(_, n)| n / 10), |i| i as usize);
        let dirf = c
            .get(&fp, "directoryfile")
            .or_else(|| c.get(&path, "directoryfile"))
            .unwrap_or("index.adp,index.tcl,index.html,index.htm");
        let pageroot = c
            .get(&fp, "pageroot")
            .or_else(|| c.get(&path, "pageroot"))
            .map_or_else(|| home(&["servers", name, "pages"]), PathBuf::from);
        let listing = c
            .get(&fp, "directorylisting")
            .filter(|p| *p == "simple" || *p == "fancy")
            .map(|_| "_ns_dirlist".to_owned());
        let fastpath = FastpathConfig {
            pageroot,
            dirv: dirf
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect(),
            dirproc: c.get(&fp, "directoryproc").map(str::to_owned).or(listing),
            diradp: c.get(&fp, "directoryadp").map(str::to_owned),
            cache: cache.map(|(c, _)| c),
            cachemaxentry,
        };
        let ap = Config::make_path(Some(name), None, &["adp"]);
        let adp = AdpConfig {
            errorpage: c.get(&ap, "errorpage").map(str::to_owned),
            startpage: c.get(&ap, "startpage").map(str::to_owned),
            expire: bool_or(&ap, "enableexpire", false),
            debug: bool_or(&ap, "enabledebug", false),
            safe: bool_or(&ap, "safeeval", false),
            single: bool_or(&ap, "singlescript", false),
            gzip: bool_or(&ap, "gzip", false),
            trace: bool_or(&ap, "trace", false),
            detail: bool_or(&ap, "detailerror", true),
            strict: bool_or(&ap, "stricterror", false),
            display: bool_or(&ap, "displayerror", false),
            trim: bool_or(&ap, "trimspace", false),
            autoabort: bool_or(&ap, "autoabort", true),
            bufsize: c.int_or(&ap, "bufsize", 1024 * 1000).max(0) as usize,
            cachesize: c.int_or(&ap, "cachesize", 5 * 1024 * 1000).max(0) as usize,
            tracesize: c.int_or(&ap, "tracesize", 40).max(0) as usize,
            debuginit: c.string_or(&ap, "debuginit", "ns_adp_debuginit"),
            defaultmime: c.get(&ap, "defaultmimetype").map(str::to_owned),
        };
        let nsvbuckets = c.int_or(&tclpath, "nsvbuckets", 8).max(1) as usize;
        let server = Arc::new(Self {
            name: name.to_owned(),
            global: Arc::clone(global),
            opts,
            charsets,
            limits,
            fastpath,
            adp,
            tcl: TclState {
                library,
                initfile,
                script: RwLock::new(String::new()),
                epoch: AtomicU64::new(0),
                traces: RwLock::new(Vec::new()),
                once: Mutex::new(HashSet::new()),
                packages: Mutex::new(BTreeMap::new()),
                modules: Mutex::new(Vec::new()),
                commands: RwLock::new(Vec::new()),
            },
            requests: RwLock::new(UrlSpace::new()),
            proxies: RwLock::new(HashMap::new()),
            auth: RwLock::new(None),
            redirects: RwLock::new(HashMap::new()),
            filters: RwLock::new(Vec::new()),
            traces: RwLock::new(Vec::new()),
            cleanups: RwLock::new(Vec::new()),
            url2pool: RwLock::new(UrlSpace::new()),
            url2limits: RwLock::new(UrlSpace::new()),
            url2encoding: RwLock::new(UrlSpace::new()),
            adp_tags: RwLock::new(HashMap::new()),
            adp_pages: Mutex::new(HashMap::new()),
            shared_sets: Mutex::new(HashMap::new()),
            next_set: AtomicU64::new(0),
            nsv: Nsv::new(nsvbuckets),
            vars: Mutex::new(HashMap::new()),
            location_proc: RwLock::new(None),
        });
        global.add_server(&server);
        server.register_defaults();
        server
    }

    /// Register fastpath for `/`, the `fastpath`/`adp` URL maps and the
    /// status redirects (`NsInitServer`).
    fn register_defaults(self: &Arc<Self>) {
        let c = &self.global.config;
        for m in ["GET", "HEAD", "POST"] {
            crate::fastpath::register(self, m, "/", UrlFlags::INHERIT);
        }
        let map = |kind: &str| -> Vec<String> {
            Config::make_path(Some(&self.name), None, &[kind]);
            c.section(&Config::make_path(Some(&self.name), None, &[kind]))
                .map(|s| {
                    s.iter()
                        .filter(|(k, _)| k.eq_ignore_ascii_case("map"))
                        .filter_map(|(_, v)| v.map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        for spec in map("fastpath") {
            let (methods, url) = parse_map(&spec);
            for m in methods {
                crate::fastpath::register(self, &m, &url, UrlFlags::INHERIT);
            }
        }
        for spec in map("adp") {
            let (methods, url) = parse_map(&spec);
            for m in methods {
                crate::adp::register_map(self, &m, &url);
            }
        }
        if let Some(sec) = c.section(&Config::make_path(Some(&self.name), None, &["redirects"])) {
            let mut r = self
                .redirects
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            for (k, v) in sec.iter() {
                if let (Ok(code), Some(url)) = (k.trim().parse::<u16>(), v) {
                    r.insert(code, url.to_owned());
                }
            }
        }
    }

    /// The configured redirect for a status code.
    pub fn redirect_for(&self, status: u16) -> Option<String> {
        self.redirects
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&status)
            .cloned()
    }

    /// The pool serving `method url` (`NsGetConnPool`).
    pub fn pool_for(&self, method: &str, url: &str) -> Arc<Pool> {
        let name = self
            .url2pool
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(method, url)
            .cloned();
        name.map_or_else(
            || self.global.pools.default_pool(),
            |n| self.global.pools.get_or_create(&n),
        )
    }

    /// The limits for `method url` (`NsGetRequestLimits`).
    pub fn limits_for(&self, method: &str, url: &str) -> Arc<Limits> {
        let name = self
            .url2limits
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(method, url)
            .cloned();
        self.global
            .pools
            .limits_or_create(name.as_deref().unwrap_or("default"))
    }

    /// The input encoding for `method url` (`ns_register_encoding`, falling
    /// back to the server's input charset).
    pub fn input_encoding_for(&self, method: &str, url: &str) -> Option<&'static Encoding> {
        self.url2encoding
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(method, url)
            .and_then(|cs| charset_encoding(cs))
            .or(self.charsets.input_encoding)
    }

    /// Install a location proc.
    pub fn set_location_proc(&self, p: Option<LocationProc>) {
        *self
            .location_proc
            .write()
            .unwrap_or_else(PoisonError::into_inner) = p;
    }

    /// The installed location proc.
    pub fn location_proc(&self) -> Option<LocationProc> {
        self.location_proc
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// A fresh id for a shared `ns_set`.
    pub fn next_set_id(&self) -> u64 {
        self.next_set.fetch_add(1, Ordering::Relaxed)
    }

    /// Install a command into every interp created for this server.
    pub fn add_command(&self, name: &str, cmd: Command) {
        let mut c = self
            .tcl
            .commands
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        c.retain(|(n, _)| n != name);
        c.push((name.to_owned(), cmd));
    }

    /// Modules registered for Tcl library sourcing.
    pub fn tcl_modules(&self) -> Vec<String> {
        self.tcl
            .modules
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Path beneath the server's module directory (`Ns_ModulePath`).
    pub fn module_path(&self, module: Option<&str>, parts: &[&str]) -> PathBuf {
        let mut p = self.global.home_path(&["servers", &self.name]);
        if let Some(m) = module {
            p.push("modules");
            p.push(m);
        }
        for part in parts {
            p.push(part);
        }
        p
    }
}

/// Parse a `map` value: `"GET /*.adp"` or `/*.adp` (meaning GET/HEAD/POST).
pub fn parse_map(spec: &str) -> (Vec<String>, String) {
    let mut parts = spec.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some(m), Some(u)) => (vec![m.to_owned()], u.to_owned()),
        (Some(u), None) => (
            vec!["GET".into(), "HEAD".into(), "POST".into()],
            u.to_owned(),
        ),
        _ => (Vec::new(), String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_specs() {
        assert_eq!(
            parse_map("GET /*.adp"),
            (vec!["GET".to_owned()], "/*.adp".to_owned())
        );
        assert_eq!(parse_map("/*.adp").0.len(), 3);
        assert!(parse_map("").0.is_empty());
    }

    #[test]
    fn nsv_buckets() {
        let n = Nsv::new(4);
        n.with("a", true, |m| {
            m.expect("array").insert("k".into(), "v".into());
        });
        assert_eq!(
            n.with("a", false, |m| m.and_then(|m| m.get("k").cloned())),
            Some("v".into())
        );
        assert!(n.with("missing", false, |m| m.is_none()));
        assert_eq!(n.names(), vec!["a"]);
        assert!(n.remove_array("a"));
        assert!(!n.remove_array("a"));
    }
}
