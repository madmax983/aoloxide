//! # nsd
//!
//! The AOLserver server core ported to Rust.  This crate is the equivalent
//! of `libnsd`: configuration, virtual servers, socket drivers, connection
//! thread pools, the request pipeline (filters, authorization, request
//! procs), fastpath, ADP, the scheduler, and the `ns_*` script commands.
//!
//! The entry point is [`Nsd`]: build one from a config script, register
//! compiled-in modules, then [`Nsd::start`] and [`Nsd::wait`].
//!
//! ```no_run
//! use nsd::{Nsd, NsdOptions};
//! let opts = NsdOptions::from_file("nsconfig.tcl").expect("read");
//! let server = Nsd::new(opts).expect("init");
//! server.start().expect("start");
//! server.wait();
//! server.shutdown();
//! ```

pub mod adp;
pub mod callbacks;
pub mod cmds;
pub mod conn;
pub mod content;
pub mod driver;
pub mod fastpath;
pub mod global;
pub mod init_tcl;
pub mod interp;
pub mod job;
pub mod module;
pub mod op;
pub mod pool;
pub mod queue;
pub mod sched;
pub mod server;
pub mod sync;
pub mod tclcache;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use ns_core::log::Severity;
use ns_core::Config;
use ns_script::{command, wrong_args, Exception, ScriptEngine};

pub use crate::callbacks::When;
pub use crate::conn::{ConnCtx, ConnRef, Status};
pub use crate::driver::{Driver, DriverInfo, PlainWrapper, StreamWrapper};
pub use crate::global::{Global, NsError};
pub use crate::module::{ModuleContext, ModuleInit, ModuleRegistry};
pub use crate::server::Server;

/// Options for building an [`Nsd`].
pub struct NsdOptions {
    /// The config script text.
    pub config_text: String,
    /// The config file path, if read from a file.
    pub config_file: Option<PathBuf>,
    /// Extra `argv` passed to the config script.
    pub args: Vec<String>,
    /// Home directory override.
    pub home: Option<PathBuf>,
    /// Only initialise this server (`-s`).
    pub server: Option<String>,
    /// Log to stderr (`-f`/`-d`).
    pub log_stderr: bool,
    /// Capture the log in memory (tests).
    pub log_memory: bool,
    /// Compiled-in modules.
    pub modules: ModuleRegistry,
    /// Scripting engine (defaults to Tcl).
    pub engine: Option<Arc<dyn ScriptEngine>>,
}

impl NsdOptions {
    /// Options for a config script given as text.
    pub fn from_text(text: &str) -> Self {
        Self {
            config_text: text.to_owned(),
            config_file: None,
            args: Vec::new(),
            home: None,
            server: None,
            log_stderr: false,
            log_memory: false,
            modules: builtin_modules(),
            engine: None,
        }
    }

    /// Options reading the config script from `path` (`NsConfigRead`).
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, NsError> {
        let p = path.as_ref();
        let meta = std::fs::metadata(p)
            .map_err(|e| NsError::Config(format!("stat({}) failed: {e}", p.display())))?;
        if !meta.is_file() {
            return Err(NsError::Config(format!(
                "not regular file: {}",
                p.display()
            )));
        }
        let text = std::fs::read_to_string(p)?;
        let mut o = Self::from_text(&text);
        o.config_file = Some(std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()));
        Ok(o)
    }
}

/// The modules built into `nsd` itself (`nssock`).
pub fn builtin_modules() -> ModuleRegistry {
    let mut r = ModuleRegistry::new();
    r.register("nssock", Arc::new(nssock_init));
    r
}

/// Initialise a plain-HTTP socket driver (the `nssock` module).
pub fn nssock_init(ctx: &ModuleContext<'_>) -> Result<(), NsError> {
    driver_init(ctx, "nssock", false, Arc::new(PlainWrapper))
}

/// Shared driver setup for `nssock` and TLS drivers (`Ns_DriverInit`).
///
/// `driver_name` is the driver's fixed name (`init->name`, e.g. `nssock`
/// or `nsssl`, reported by `ns_conn driver`); the module instance name
/// (`ctx.name`) is kept separately as the driver's module.
pub fn driver_init(
    ctx: &ModuleContext<'_>,
    driver_name: &str,
    ssl: bool,
    wrapper: Arc<dyn StreamWrapper>,
) -> Result<(), NsError> {
    let path = ctx.config_path();
    let c = &ctx.global.config;
    let info = DriverInfo::from_config(
        c,
        &path,
        driver_name,
        ctx.name,
        ctx.server.map(|s| s.name.as_str()),
        ssl,
        &ctx.global.info.hostname,
    );
    let (map, default) = if let Some(s) = ctx.server {
        (HashMap::new(), Arc::clone(s))
    } else {
        let defserver = c.get(&path, "defaultserver").ok_or_else(|| {
            NsError::Config(format!("{}: no defaultserver defined: {path}", ctx.name))
        })?;
        let mut map = HashMap::new();
        if let Some(sec) = c.section(&format!("{path}/servers")) {
            for (srv, host) in sec.iter() {
                let server = ctx.global.server(srv).ok_or_else(|| {
                    NsError::Config(format!("{}: no such server: {srv}", ctx.name))
                })?;
                if let Some(h) = host {
                    map.insert(h.to_ascii_lowercase(), server);
                }
            }
        }
        let def = ctx.global.server(defserver).ok_or_else(|| {
            NsError::Config(format!(
                "{}: default server {defserver} not defined",
                ctx.name
            ))
        })?;
        (map, def)
    };
    let d = Driver::bind(ctx.global, info, map, default, wrapper)?;
    ctx.add_driver(d);
    Ok(())
}

/// A running AOLserver process.
pub struct Nsd {
    /// Process-wide state.
    pub global: Arc<Global>,
    /// Virtual servers in config order.
    pub servers: Vec<Arc<Server>>,
    drivers: Mutex<Vec<Arc<Driver>>>,
    runtime: Mutex<Option<tokio::runtime::Runtime>>,
    pidfile: Option<PathBuf>,
}

impl std::fmt::Debug for Nsd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Nsd")
            .field("servers", &self.servers)
            .finish_non_exhaustive()
    }
}

/// Evaluate a config script into a [`Config`] (`NsConfigEval`).
///
/// `config_file` is reported by `ns_info config` while the script runs.
pub fn eval_config(
    engine: &dyn ScriptEngine,
    text: &str,
    args: &[String],
    config_file: Option<&Path>,
) -> Result<Config, NsError> {
    let interp = engine.create_interp()?;
    let state = Arc::new(Mutex::new((Config::new(), None::<usize>)));
    let s1 = Arc::clone(&state);
    interp.register(
        "ns_section",
        command(move |_, argv| {
            if argv.len() != 2 {
                return Err(Exception::error(format!(
                    "wrong # args: should be \"{} sectionname\"",
                    argv[0]
                )));
            }
            let mut g = s1.lock().unwrap_or_else(PoisonError::into_inner);
            let idx = g.0.section_index_or_create(&argv[1]);
            g.1 = Some(idx);
            Ok(String::new())
        }),
    );
    let s2 = Arc::clone(&state);
    interp.register(
        "ns_param",
        command(move |_, argv| {
            if argv.len() != 3 {
                return Err(Exception::error(format!(
                    "wrong # args: should be \"{} key value",
                    argv[0]
                )));
            }
            let mut g = s2.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(idx) = g.1 else {
                return Err(Exception::error(format!(
                    "{} not preceded by an ns_section command.",
                    argv[0]
                )));
            };
            if let Some(sec) = g.0.section_mut(idx) {
                sec.put_str(&argv[1], &argv[2]);
            }
            Ok(String::new())
        }),
    );
    let hostname = {
        let mut buf = [0u8; 256];
        // SAFETY: buf is valid for writes of its length.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            String::from_utf8_lossy(&buf[..end]).into_owned()
        } else {
            "localhost".into()
        }
    };
    let cfg_path = config_file
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    interp.register(
        "ns_info",
        command(move |_, argv| {
            let opt = argv
                .get(1)
                .map(String::as_str)
                .ok_or_else(|| wrong_args(argv, 1, "option"))?;
            Ok(match opt {
                "hostname" => hostname.clone(),
                "config" => cfg_path.clone(),
                "address" => "127.0.0.1".into(),
                "name" => global::SERVER_NAME.into(),
                "version" | "patchlevel" => global::SERVER_VERSION.into(),
                "pid" => std::process::id().to_string(),
                "platform" => std::env::consts::OS.into(),
                "nsd" => std::env::current_exe()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                "home" => std::env::current_exe()
                    .ok()
                    .and_then(|p| {
                        p.parent()
                            .and_then(Path::parent)
                            .map(|p| p.to_string_lossy().into_owned())
                    })
                    .unwrap_or_default(),
                _ => String::new(),
            })
        }),
    );
    interp.register(
        "ns_log",
        command(|_, argv| {
            eprintln!("config: {}", argv[2..].concat());
            Ok(String::new())
        }),
    );
    interp.set_global("argc", &args.len().to_string())?;
    interp.set_global("argv", &interp.list_from(args))?;
    interp.set_global("optind", "0")?;
    interp
        .eval_global(text)
        .map_err(|e| NsError::Config(e.detail().to_owned()))?;
    drop(interp);
    let cfg = std::mem::take(&mut state.lock().unwrap_or_else(PoisonError::into_inner).0);
    Ok(cfg)
}

fn load_modules(
    global: &Arc<Global>,
    registry: &ModuleRegistry,
    server: Option<&Arc<Server>>,
    servers: &[Arc<Server>],
    drivers: &Mutex<Vec<Arc<Driver>>>,
) -> Result<(), NsError> {
    let path = Config::make_path(server.map(|s| s.name.as_str()), None, &["modules"]);
    let Some(sec) = global.config.section(&path) else {
        return Ok(());
    };
    for (name, file) in sec.iter() {
        let file = file.unwrap_or("");
        if let Some(s) = server {
            s.tcl
                .modules
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(name.to_owned());
        }
        if file.eq_ignore_ascii_case("tcl") {
            continue;
        }
        let key = module_key_for(name, file);
        if let Some(init) = registry.get(&key).or_else(|| registry.get(name)) {
            let ctx = ModuleContext {
                global,
                server,
                servers,
                name,
                drivers,
            };
            init(&ctx)?;
            global.log(
                Severity::Notice,
                &format!("modload: loaded module: {name} ({key})"),
            );
        } else {
            global.log(
                Severity::Error,
                &format!("modload: no such module: {name} ({file})"),
            );
            return Err(NsError::Config(format!("no such module: {name}")));
        }
    }
    Ok(())
}

fn module_key_for(name: &str, file: &str) -> String {
    module::module_key(name, file)
}

impl Nsd {
    /// Build the server: evaluate config, create servers, load modules and
    /// run each server's Tcl initialisation (`Ns_Main` up to startup).
    pub fn new(opts: NsdOptions) -> Result<Self, NsError> {
        let engine: Arc<dyn ScriptEngine> = opts
            .engine
            .unwrap_or_else(|| Arc::new(ns_tcl::TclEngine::new()));
        let config = eval_config(
            &*engine,
            &opts.config_text,
            &opts.args,
            opts.config_file.as_deref(),
        )?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("-driver-")
            .enable_all()
            .build()?;
        let global = Global::new(global::GlobalOptions {
            config,
            config_file: opts.config_file,
            home: opts.home,
            engine,
            runtime: runtime.handle().clone(),
            log_stderr: opts.log_stderr,
            log_memory: opts.log_memory,
        })?;
        let weak: Weak<Global> = Arc::downgrade(&global);
        global.pools.set_thread_hook(Arc::new(move || {
            if let Some(g) = weak.upgrade() {
                interp::thread_cleanup(&g);
            }
        }));
        global.log(
            Severity::Notice,
            &format!(
                "nsmain: {}/{} starting (aoloxide, {} {})",
                global::SERVER_NAME,
                global::SERVER_VERSION,
                global.engine.name(),
                global.engine.version()
            ),
        );
        let mut names: Vec<String> = global
            .config
            .section("ns/servers")
            .map(|s| s.iter().map(|(k, _)| k.to_owned()).collect())
            .unwrap_or_default();
        if let Some(only) = &opts.server {
            names.retain(|n| n == only);
            if names.is_empty() {
                return Err(NsError::Config(format!("no such server: {only}")));
            }
        }
        if names.is_empty() {
            return Err(NsError::Config("no servers defined in ns/servers".into()));
        }
        let drivers = Mutex::new(Vec::new());
        let mut servers = Vec::new();
        for name in &names {
            let s = Server::new(&global, name);
            servers.push(s);
        }
        for s in &servers {
            load_modules(&global, &opts.modules, Some(s), &servers, &drivers)?;
            interp::init_server(s)?;
        }
        load_modules(&global, &opts.modules, None, &servers, &drivers)?;
        let pidfile = global
            .config
            .get(global::PARAMS, "pidfile")
            .map(PathBuf::from)
            .or_else(|| (!opts.log_memory).then(|| global.home_path(&["log", "nsd.pid"])));
        Ok(Self {
            global,
            servers,
            drivers,
            runtime: Mutex::new(Some(runtime)),
            pidfile,
        })
    }

    /// The drivers created by modules.
    pub fn drivers(&self) -> Vec<Arc<Driver>> {
        self.drivers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Look up a server by name.
    pub fn server(&self, name: &str) -> Option<&Arc<Server>> {
        self.servers.iter().find(|s| s.name == name)
    }

    /// Start pools and drivers and run startup callbacks.
    pub fn start(&self) -> Result<(), NsError> {
        let g = &self.global;
        g.callbacks.run(When::PreStartup);
        g.pools.start_all();
        for d in self.drivers() {
            d.start()?;
        }
        g.callbacks.run(When::Startup);
        if let Some(p) = &self.pidfile {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(p, format!("{}\n", std::process::id()));
        }
        g.state.set_started();
        g.callbacks.run(When::Ready);
        g.log(Severity::Notice, "nsmain: AOLserver/aoloxide running");
        Ok(())
    }

    /// Block until shutdown is requested (by `ns_shutdown`,
    /// [`Nsd::request_shutdown`] or a signal handled by
    /// [`Nsd::install_signal_handlers`]).
    pub fn wait(&self) {
        self.global.state.wait_stop(None);
    }

    /// Ask the server to stop.
    pub fn request_shutdown(&self) {
        self.global.state.request_stop();
    }

    /// Install SIGTERM/SIGINT (shutdown) and SIGHUP (signal callbacks and
    /// log roll) handlers (`NsHandleSignals`).
    pub fn install_signal_handlers(&self) {
        use tokio::signal::unix::{signal, SignalKind};
        let g = Arc::clone(&self.global);
        let rt = g.runtime.clone();
        rt.spawn(async move {
            let (Ok(mut term), Ok(mut int), Ok(mut hup)) = (
                signal(SignalKind::terminate()),
                signal(SignalKind::interrupt()),
                signal(SignalKind::hangup()),
            ) else {
                return;
            };
            loop {
                tokio::select! {
                    _ = term.recv() => { g.log(Severity::Notice, "nsmain: received SIGTERM"); g.state.request_stop(); break; }
                    _ = int.recv() => { g.log(Severity::Notice, "nsmain: received SIGINT"); g.state.request_stop(); break; }
                    _ = hup.recv() => {
                        g.log(Severity::Notice, "nsmain: received SIGHUP");
                        let g2 = Arc::clone(&g);
                        let _ = tokio::task::spawn_blocking(move || {
                            g2.callbacks.run(When::Signal);
                            if g2.params.log_roll {
                                let _ = g2.log.roll(g2.params.log_max_backup);
                            }
                        }).await;
                    }
                }
            }
        });
    }

    /// Orderly shutdown (`Ns_Main` exit path): stop drivers, the scheduler,
    /// run shutdown callbacks, drain pools and job queues, run exit
    /// callbacks.
    pub fn shutdown(&self) {
        let g = &self.global;
        g.state.request_stop();
        let timeout = g.params.shutdown_timeout;
        let deadline = Instant::now() + timeout;
        let left = || deadline.saturating_duration_since(Instant::now());
        g.log(
            Severity::Notice,
            &format!("nsmain: shutting down with {}s timeout", timeout.as_secs()),
        );
        let drivers = self.drivers();
        for d in &drivers {
            d.stop();
        }
        for d in &drivers {
            d.wait(left().min(Duration::from_secs(2)));
        }
        g.sched.shutdown(left());
        g.callbacks.run(When::Shutdown);
        if !g.pools.shutdown_all(left()) {
            g.log(
                Severity::Warning,
                "nsmain: timeout waiting for connection threads",
            );
        }
        if !g.jobs.shutdown(left()) {
            g.log(Severity::Warning, "nsmain: timeout waiting for job threads");
        }
        g.callbacks.run(When::Exit);
        interp::thread_cleanup(g);
        if let Some(p) = &self.pidfile {
            let _ = std::fs::remove_file(p);
        }
        g.log(Severity::Notice, "nsmain: exiting");
        let rt = self
            .runtime
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(rt) = rt {
            rt.shutdown_timeout(Duration::from_secs(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_eval() {
        let e = ns_tcl::TclEngine::new();
        let c = eval_config(
            &e,
            "ns_section ns/parameters\nns_param home /tmp\nns_param debug [expr {1+1}]\nset x [ns_info name]\nns_section ns/servers\nns_param s1 {Server One}\nns_param argc $argc",
            &["a".into()],
            None,
        )
        .expect("config");
        assert_eq!(c.get("ns/parameters", "home"), Some("/tmp"));
        assert_eq!(c.get("ns/parameters", "debug"), Some("2"));
        assert_eq!(c.get("ns/servers", "s1"), Some("Server One"));
        assert_eq!(c.get("ns/servers", "argc"), Some("1"));
        assert!(eval_config(&e, "ns_param a b", &[], None).is_err());
        assert!(eval_config(&e, "ns_section", &[], None).is_err());
        assert!(eval_config(&e, "error boom", &[], None).is_err());
        let c = eval_config(
            &e,
            "ns_section x\nns_param c [ns_info config]",
            &[],
            Some(Path::new("/etc/nsd.tcl")),
        )
        .expect("cfg");
        assert_eq!(c.get("x", "c"), Some("/etc/nsd.tcl"));
    }

    #[test]
    fn module_keys() {
        assert_eq!(module_key_for("nssock", "nssock.so"), "nssock");
        assert!(builtin_modules().get("nssock").is_some());
        assert!(NsdOptions::from_file("/nonexistent/x.tcl").is_err());
    }
}
