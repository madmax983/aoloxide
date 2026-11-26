//! Process-wide server state (`struct _nsconf` in `nsd.h`, `nsconf.c`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, RwLock, Weak};
use std::time::{Duration, Instant};

use ns_core::log::{Log, LogFilter, Severity};
use ns_core::mimetypes::MimeTypes;
use ns_core::Config;
use ns_script::ScriptEngine;

use crate::callbacks::Callbacks;
use crate::job::JobRegistry;
use crate::pool::PoolRegistry;
use crate::sched::Scheduler;
use crate::server::Server;
use crate::sockcallback::SockCallbacks;
use crate::sync::SyncRegistry;
use crate::tclcache::CacheRegistry;

/// Server product name reported by `ns_info name` and the `Server` header.
pub const SERVER_NAME: &str = "AOLserver";
/// Version reported by `ns_info version`.
pub const SERVER_VERSION: &str = ns_core::NS_PATCH_LEVEL;
/// Section holding global parameters.
pub const PARAMS: &str = "ns/parameters";

/// Error raised while configuring or running the server.
#[derive(Debug, thiserror::Error)]
pub enum NsError {
    /// Bad or missing configuration.
    #[error("config: {0}")]
    Config(String),
    /// I/O failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Script failure.
    #[error("script: {0}")]
    Script(String),
}

impl From<ns_script::Exception> for NsError {
    fn from(e: ns_script::Exception) -> Self {
        Self::Script(e.detail().to_owned())
    }
}

/// Static process information (`Ns_Info*`).
#[derive(Debug, Clone)]
pub struct ProcessInfo {
    /// `argv[0]`.
    pub argv0: String,
    /// Absolute path of the server binary.
    pub nsd: PathBuf,
    /// Server home directory.
    pub home: PathBuf,
    /// Config file path.
    pub config_file: Option<PathBuf>,
    /// Process id.
    pub pid: u32,
    /// Boot time (Unix seconds).
    pub boot_time: i64,
    /// Host name.
    pub hostname: String,
    /// Primary address.
    pub address: String,
    /// Build stamp.
    pub build: String,
}

/// Global tuning parameters from `ns/parameters`.
#[derive(Debug, Clone)]
pub struct Params {
    /// Seconds to wait for clean shutdown.
    pub shutdown_timeout: Duration,
    /// Warn when scheduled procs run longer than this.
    pub sched_max_elapsed: Duration,
    /// Listen backlog.
    pub backlog: u32,
    /// Maximum HTTP major version spoken.
    pub http_major: u32,
    /// Maximum HTTP minor version spoken.
    pub http_minor: u32,
    /// Serialize interp creation.
    pub tcl_init_lock: bool,
    /// Max log backups kept by `ns_logroll`.
    pub log_max_backup: usize,
    /// Roll the log on SIGHUP.
    pub log_roll: bool,
}

impl Params {
    /// Read from config.
    pub fn from_config(c: &Config) -> Self {
        let secs = |k, d| Duration::from_secs(c.int_or(PARAMS, k, d).max(0) as u64);
        Self {
            shutdown_timeout: secs("shutdowntimeout", 20),
            sched_max_elapsed: secs("schedmaxelapsed", 2),
            backlog: c.int_or(PARAMS, "listenbacklog", 32).max(1) as u32,
            http_major: c.int_or(PARAMS, "httpmajor", 1).max(0) as u32,
            http_minor: c.int_or(PARAMS, "httpminor", 1).max(0) as u32,
            tcl_init_lock: c.bool_or(PARAMS, "tclinitlock", false),
            log_max_backup: c.int_or(PARAMS, "logmaxbackup", 10).clamp(0, 999) as usize,
            log_roll: c.bool_or(PARAMS, "logroll", true),
        }
    }
}

/// Shutdown coordination (`nsconf.state`).
#[derive(Debug, Default)]
pub struct State {
    started: AtomicBool,
    stopping: AtomicBool,
    lock: Mutex<()>,
    cond: Condvar,
}

impl State {
    /// Mark the server started.
    pub fn set_started(&self) {
        let _g = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        self.started.store(true, Ordering::SeqCst);
        self.cond.notify_all();
    }

    /// Whether startup completed.
    pub fn started(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }

    /// Request shutdown and wake waiters.
    pub fn request_stop(&self) {
        let _g = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        self.stopping.store(true, Ordering::SeqCst);
        self.cond.notify_all();
    }

    /// Whether shutdown has been requested.
    pub fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Block until shutdown is requested or `timeout` elapses.  Returns true
    /// if stopping.
    pub fn wait_stop(&self, timeout: Option<Duration>) -> bool {
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut g = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        while !self.stopping() {
            match deadline {
                None => g = self.cond.wait(g).unwrap_or_else(PoisonError::into_inner),
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return false;
                    }
                    g = self
                        .cond
                        .wait_timeout(g, d - now)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
            }
        }
        true
    }

    /// Block until startup completes, shutdown is requested or `timeout`
    /// elapses (`Ns_WaitForStartup`, bounded).  Returns whether started.
    pub fn wait_started(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut g = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        while !self.started() && !self.stopping() {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            g = self
                .cond
                .wait_timeout(g, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        self.started()
    }
}

/// All process-wide state.  Shared by every server, driver and thread.
pub struct Global {
    /// Process information.
    pub info: ProcessInfo,
    /// Global parameters.
    pub params: Params,
    /// The configuration database (read-only after startup).
    pub config: Config,
    /// The server log.
    pub log: Log,
    /// MIME types.
    pub mime: RwLock<MimeTypes>,
    /// Scripting engine.
    pub engine: Arc<dyn ScriptEngine>,
    /// Startup/shutdown/exit callbacks.
    pub callbacks: Callbacks,
    /// The scheduler (`ns_schedule_proc`).
    pub sched: Scheduler,
    /// Connection thread pools.
    pub pools: PoolRegistry,
    /// `ns_cache` caches.
    pub caches: CacheRegistry,
    /// `ns_job` queues.
    pub jobs: JobRegistry,
    /// `ns_mutex`, `ns_cond`, ... handles.
    pub sync: SyncRegistry,
    /// Run state.
    pub state: State,
    /// Socket callbacks (`Ns_SockCallback`, the `-socks-` thread).
    pub socks: SockCallbacks,
    /// Async I/O runtime used by drivers and socket callbacks.
    pub runtime: tokio::runtime::Handle,
    /// Virtual servers by name (weak to avoid cycles).
    servers: RwLock<Vec<(String, Weak<Server>)>>,
    next_conn_id: AtomicU64,
    /// Drivers (for `ns_driver`).
    pub drivers: RwLock<Vec<Arc<crate::driver::DriverInfo>>>,
    /// Stream wrapper factories (TLS) by module name.
    pub wrappers: crate::driver::WrapperRegistry,
    /// Process-wide `ns_share`/`ns_var`-style misc storage.
    pub env_lock: Mutex<()>,
    /// Compression procedure installed by nszlib (`Ns_SetGzipProc`).
    gzip_proc: RwLock<Option<GzipProc>>,
}

/// A gzip compression procedure (`Ns_GzipProc`): compress `data` at
/// `level` into a complete gzip stream.
pub type GzipProc = fn(data: &[u8], level: u32) -> std::io::Result<Vec<u8>>;

impl std::fmt::Debug for Global {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Global")
            .field("home", &self.info.home)
            .finish_non_exhaustive()
    }
}

/// Options used to build a [`Global`].
pub struct GlobalOptions {
    /// Parsed config.
    pub config: Config,
    /// Config file path (for `ns_info config`).
    pub config_file: Option<PathBuf>,
    /// Home directory override; defaults to `ns/parameters home` or the
    /// binary's parent directory's parent.
    pub home: Option<PathBuf>,
    /// Scripting engine.
    pub engine: Arc<dyn ScriptEngine>,
    /// Runtime handle for async I/O.
    pub runtime: tokio::runtime::Handle,
    /// Log to stderr instead of `serverlog` (foreground/debug mode).
    pub log_stderr: bool,
    /// Capture log output in memory (tests).
    pub log_memory: bool,
}

fn resolve_hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf is a valid writable buffer of the given length.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    } else {
        "localhost".to_owned()
    }
}

fn default_home() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().and_then(Path::parent).map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

impl Global {
    /// Build global state from config (`NsInitConf` + `NsConfUpdate`).
    pub fn new(opts: GlobalOptions) -> Result<Arc<Self>, NsError> {
        let c = opts.config;
        let home = opts
            .home
            .or_else(|| c.get(PARAMS, "home").map(PathBuf::from))
            .unwrap_or_else(default_home);
        let params = Params::from_config(&c);
        let log = if opts.log_memory {
            Log::memory()
        } else if opts.log_stderr {
            Log::stderr()
        } else {
            let file = c.string_or(PARAMS, "serverlog", "server.log");
            let mut path = PathBuf::from(&file);
            if path.is_relative() {
                path = home.join("log").join(path);
            }
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            Log::file(&path)?
        }
        .with_options(
            c.bool_or(PARAMS, "logusec", false),
            c.bool_or(PARAMS, "logexpanded", false),
        );
        if !opts.log_memory {
            let debug = c.bool_or(PARAMS, "debug", false);
            log.set_filter(LogFilter {
                notice: c.bool_or(PARAMS, "lognotice", true),
                debug: debug || c.bool_or(PARAMS, "logdebug", false),
                dev: c.bool_or(PARAMS, "logdev", false),
            });
        }
        let mut mime = MimeTypes::new();
        if let Some(sec) = c.section("ns/mimetypes") {
            mime.configure(sec.iter().map(|(k, v)| (k, v.unwrap_or(""))));
        }
        let argv0 = std::env::args().next().unwrap_or_else(|| "nsd".to_owned());
        let hostname = c
            .get(PARAMS, "hostname")
            .map_or_else(resolve_hostname, str::to_owned);
        let address = c.string_or(PARAMS, "address", "127.0.0.1");
        let info = ProcessInfo {
            nsd: std::env::current_exe().unwrap_or_else(|_| PathBuf::from(&argv0)),
            argv0,
            home,
            config_file: opts.config_file,
            pid: std::process::id(),
            boot_time: ns_core::httptime::now(),
            hostname,
            address,
            build: concat!("aoloxide ", env!("CARGO_PKG_VERSION")).to_owned(),
        };
        let max_elapsed = params.sched_max_elapsed;
        let g = Arc::new_cyclic(|weak: &Weak<Self>| Self {
            info,
            params,
            config: c,
            log,
            mime: RwLock::new(mime),
            engine: opts.engine,
            callbacks: Callbacks::default(),
            sched: Scheduler::new(weak.clone(), max_elapsed),
            pools: PoolRegistry::default(),
            caches: CacheRegistry::default(),
            jobs: JobRegistry::default(),
            sync: SyncRegistry::default(),
            state: State::default(),
            socks: SockCallbacks::new(weak.clone()),
            runtime: opts.runtime,
            servers: RwLock::new(Vec::new()),
            next_conn_id: AtomicU64::new(1),
            drivers: RwLock::new(Vec::new()),
            wrappers: crate::driver::WrapperRegistry::default(),
            env_lock: Mutex::new(()),
            gzip_proc: RwLock::new(None),
        });
        g.pools.limits_or_create("default");
        Ok(g)
    }

    /// Install the gzip procedure used for response compression
    /// (`Ns_SetGzipProc`, called by nszlib).
    pub fn set_gzip_proc(&self, proc_: GzipProc) {
        *self
            .gzip_proc
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(proc_);
    }

    /// Compress with the installed gzip procedure (`Ns_Gzip`).  Fails when
    /// no procedure is installed, i.e. nszlib is not loaded.
    pub fn gzip(&self, data: &[u8], level: u32) -> std::io::Result<Vec<u8>> {
        let proc_ = *self
            .gzip_proc
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        match proc_ {
            Some(p) => p(data, level),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "no gzip procedure installed",
            )),
        }
    }

    /// Write to the server log (`Ns_Log`).
    pub fn log(&self, sev: Severity, msg: &str) {
        self.log.log(sev, msg);
    }

    /// Connections created so far.
    pub fn conn_count(&self) -> u64 {
        self.next_conn_id.load(Ordering::Relaxed) - 1
    }

    /// Next unique connection id.
    pub fn next_conn_id(&self) -> u64 {
        self.next_conn_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Register a virtual server.
    pub fn add_server(&self, server: &Arc<Server>) {
        self.servers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push((server.name.clone(), Arc::downgrade(server)));
    }

    /// Look up a virtual server (`NsGetServer`).
    pub fn server(&self, name: &str) -> Option<Arc<Server>> {
        self.servers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, w)| w.upgrade())
    }

    /// Names of all virtual servers, in config order.
    pub fn server_names(&self) -> Vec<String> {
        self.servers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(n, _)| n.clone())
            .collect()
    }

    /// `Ns_HomePath`: path beneath the server home.
    pub fn home_path(&self, parts: &[&str]) -> PathBuf {
        let mut p = self.info.home.clone();
        for part in parts {
            p.push(part.trim_start_matches('/'));
        }
        p
    }

    /// Server uptime in seconds.
    pub fn uptime(&self) -> i64 {
        ns_core::httptime::now() - self.info.boot_time
    }
}
