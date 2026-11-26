//! # nsext
//!
//! The AOLserver external database driver (`nsext/nsext.c`): an `nsdb`
//! driver that talks to a database proxy daemon (see the `nspd` crate)
//! instead of linking a database client library.  Each pool handle owns
//! one daemon connection, opened with the handle and closed with it.
//!
//! Config (AOLserver's):
//!
//! ```tcl
//! ns_section ns/db/drivers
//! ns_param external nsext.so
//! ns_section ns/db/driver/external
//! ns_param LocalDaemon nssqlitepd   ;# spawned per handle (relative: home/bin)
//! # or
//! ns_param RemoteHost dbhost        ;# a daemon listening on RemotePort
//! ns_param RemotePort 8199
//! ns_param Param ""                 ;# passed to the daemon on open
//! ns_param Timeout 60               ;# seconds per read or write (0: none)
//! ns_param IOTrace false            ;# log every message
//! ns_param TrimData false           ;# strip trailing spaces from values
//! ns_param maxElementSize 32768     ;# largest value accepted
//! ns_section ns/db/pool/main
//! ns_param driver external
//! ns_param datasource /var/db/main.sqlite
//! ```
//!
//! The driver adds `ns_ext` to the servers that use its pools.

mod cmd;
mod conn;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use ns_core::log::Severity;
use nsd::{ModuleContext, NsError};
use nsdb::{DbConnection, DbDriver, DbError, DriverConfig, Logger};

pub use conn::ExtConn;

/// The driver's name (`DRIVER_NAME`), reported by `ns_db driver`.
pub const DRIVER_NAME: &str = "External";
/// `DEFAULT_TIMEOUT` (seconds).
pub const DEFAULT_TIMEOUT: i64 = 60;
/// `DEFAULT_MAX_ELEMENT_SIZE` (bytes).
pub const DEFAULT_MAX_ELEMENT_SIZE: i64 = 32768;
/// `MAX_DBTYPE`: the longest database type.
const MAX_DBTYPE: usize = 64;

/// The driver's settings and shared state (`NsExtCtx`).
pub struct Ctx {
    /// `LocalDaemon`: the daemon spawned for each connection.
    pub path: Option<PathBuf>,
    /// `RemoteHost`.
    pub host: Option<String>,
    /// `RemotePort`.
    pub port: i64,
    /// `Param`, passed to the daemon with the datasource.
    pub param: Option<String>,
    /// `Timeout` in seconds for each read or write; 0 or less waits for
    /// ever.
    pub timeout: i64,
    /// `IOTrace`: log every message sent and received.
    pub io_trace: bool,
    /// `TrimData`: strip trailing spaces from values.
    pub trim_data: bool,
    /// `maxElementSize`: the largest element of a list.
    pub max_element_size: i64,
    /// `ns/parameters verbose`.
    pub verbose: bool,
    conn_num: AtomicU32,
    ident: Mutex<String>,
    log: Logger,
}

impl std::fmt::Debug for Ctx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ctx")
            .field("path", &self.path)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl Ctx {
    /// Read `ns/db/driver/<name>` (`Ns_DbDriverInit`).  Errors are
    /// logged; `None` means the config is unusable.
    pub fn from_config(dc: &DriverConfig<'_>) -> Option<Self> {
        let c = dc.config;
        let p = dc.path.as_str();
        let log = Arc::clone(dc.log);
        let path = c.get(p, "LocalDaemon").map(str::to_owned);
        let host = c.get(p, "RemoteHost").map(str::to_owned);
        let port = c.get_int(p, "RemotePort").unwrap_or(0);
        if path.is_none() && host.is_none() {
            log(
                Severity::Error,
                "nsext: bad config: localdaemon or remotehost required",
            );
            return None;
        }
        if path.is_none() && port == 0 {
            log(
                Severity::Error,
                "nsext: bad config: proxyhost requires proxyport",
            );
            return None;
        }
        let path = path.map(|d| {
            let d = PathBuf::from(d);
            match dc.home {
                Some(home) if d.is_relative() => home.join("bin").join(d),
                _ => d,
            }
        });
        Some(Self {
            path,
            host,
            port,
            param: c.get(p, "Param").map(str::to_owned),
            timeout: c.get_int(p, "Timeout").unwrap_or(DEFAULT_TIMEOUT),
            io_trace: c.get_bool(p, "IOTrace").unwrap_or(false),
            trim_data: c.get_bool(p, "TrimData").unwrap_or(false),
            max_element_size: c
                .get_int(p, "maxElementSize")
                .unwrap_or(DEFAULT_MAX_ELEMENT_SIZE),
            verbose: c.get_bool("ns/parameters", "verbose").unwrap_or(false),
            conn_num: AtomicU32::new(0),
            ident: Mutex::new(String::new()),
            log,
        })
    }

    fn log(&self, sev: Severity, m: &str) {
        (self.log)(sev, m);
    }

    /// The daemon's identity, from the last handle opened
    /// (`ctx->ident`).
    pub fn ident(&self) -> String {
        self.ident
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set_ident(&self, s: &str) {
        s.clone_into(&mut self.ident.lock().unwrap_or_else(PoisonError::into_inner));
    }

    /// `ExtDbType`: the first word of the identity, `<dbtype> Proxy
    /// Daemon vX.X`, at most 63 bytes.
    pub fn db_type(&self) -> String {
        let id = self.ident();
        let mut t = id.split(' ').next().unwrap_or_default();
        if t.len() >= MAX_DBTYPE {
            let mut i = MAX_DBTYPE - 1;
            while !t.is_char_boundary(i) {
                i -= 1;
            }
            t = &t[..i];
        }
        t.to_owned()
    }

    fn next_conn_num(&self) -> u32 {
        self.conn_num.fetch_add(1, Ordering::Relaxed)
    }
}

/// The external driver.  `ctx` is `None` when its config was unusable:
/// AOLserver still registered such a driver, whose handles then fail to
/// open.
pub struct ExtDriver {
    ctx: Option<Arc<Ctx>>,
}

impl std::fmt::Debug for ExtDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtDriver").field("ctx", &self.ctx).finish()
    }
}

impl ExtDriver {
    /// A driver with these settings.
    pub fn new(ctx: Option<Ctx>) -> Self {
        Self {
            ctx: ctx.map(Arc::new),
        }
    }

    /// The settings, if usable.
    pub fn ctx(&self) -> Option<&Arc<Ctx>> {
        self.ctx.as_ref()
    }
}

impl DbDriver for ExtDriver {
    fn name(&self) -> &'static str {
        DRIVER_NAME
    }

    fn db_type(&self) -> &'static str {
        DRIVER_NAME
    }

    /// `ExtOpenDb`: start (or connect to) a daemon, ping it, send
    /// `open datasource#user#password#param` and identify it.
    fn open(
        &self,
        datasource: &str,
        user: Option<&str>,
        password: Option<&str>,
    ) -> Result<Box<dyn DbConnection>, DbError> {
        let Some(ctx) = self.ctx.as_ref() else {
            return Err(DbError::msg(""));
        };
        conn::ExtConn::open(ctx, datasource, user, password)
            .map(|c| Box::new(c) as Box<dyn DbConnection>)
    }

    /// `ExtServerInit`: add `ns_ext` to the server's interps.
    fn server_init(&self, server: &Arc<nsd::Server>) -> Result<(), String> {
        server.add_command("ns_ext", ns_script::command(cmd::ns_ext));
        Ok(())
    }
}

/// `Ns_DbDriverInit` for `ns/db/drivers` entries naming `nsext`.
pub fn driver_init(dc: &DriverConfig<'_>) -> Result<Arc<dyn DbDriver>, Option<Arc<dyn DbDriver>>> {
    let ctx = Ctx::from_config(dc);
    let ok = ctx.is_some();
    let d: Arc<dyn DbDriver> = Arc::new(ExtDriver::new(ctx));
    (dc.log)(Severity::Notice, "nsext: module started");
    if ok {
        Ok(d)
    } else {
        Err(Some(d))
    }
}

/// Make the driver loadable from `ns/db/drivers` as `nsext` (for example
/// `ns_param external nsext.so`).
pub fn register() {
    nsdb::register_driver("nsext", Arc::new(driver_init));
}

/// Module init, for configs that also list `nsext` as a server module:
/// the driver itself is loaded by `nsdb` from `ns/db/drivers`.
pub fn init(_ctx: &ModuleContext<'_>) -> Result<(), NsError> {
    register();
    Ok(())
}

#[cfg(test)]
mod tests;
