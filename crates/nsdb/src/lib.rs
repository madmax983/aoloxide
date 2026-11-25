//! # nsdb
//!
//! The AOLserver database module (`nsdb/dbinit.c`, `dbdrv.c`, `dbutil.c`,
//! `dbtcl.c`): named connection pools, handles, and the `ns_db` command,
//! with a pluggable [`DbDriver`] interface and a bundled SQLite driver.
//!
//! Config:
//!
//! ```tcl
//! ns_section ns/db/drivers
//! ns_param sqlite nssqlite.so
//! ns_section ns/db/pools
//! ns_param main "Main database"
//! ns_section ns/db/pool/main
//! ns_param driver sqlite
//! ns_param datasource /var/db/main.sqlite
//! ns_param connections 4
//! ns_param maxidle 600         ;# close handles idle this long (0: never)
//! ns_param maxopen 3600        ;# close handles open this long (0: never)
//! ns_param checkinterval 600   ;# how often idle handles are checked
//! ns_section ns/server/s1/db
//! ns_param pools *
//! ns_param defaultpool main
//! ```
//!
//! As in AOLserver, pools are process-wide, a thread may hold handles from
//! a pool only through one `gethandle` at a time, handle ids (`nsdb0`,
//! `nsdb1`, ...) are private to the interp that took them, and handles are
//! released automatically when the interp is cleaned up (`ns_cleanup`).

pub mod driver;

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock, Weak};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

pub use driver::{DbConnection, DbDriver, DbError, ExecResult, SqliteDriver};
use ns_core::config::Config;
use ns_core::log::Severity;
use ns_core::NsSet;
use ns_script::{command, wrong_args, Exception, Interp, ScriptResult};
use nsd::callbacks::When;
use nsd::conn::{lock_set, set_ref};
use nsd::sched::{Repeat, SchedFlags};
use nsd::{Global, ModuleContext, NsError};

/// Exception code for errors raised by nsdb itself (`NS_SQLERRORCODE`).
pub const NS_SQLERRORCODE: &str = "NSINT";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now() -> i64 {
    ns_core::httptime::now()
}

/// Wait on `cond` until notified or `deadline` (`Ns_CondTimedWait`; no
/// deadline waits for ever).  Returns the guard and whether it timed out.
fn timed_wait<'a, T>(
    cond: &Condvar,
    guard: MutexGuard<'a, T>,
    deadline: Option<Instant>,
) -> (MutexGuard<'a, T>, bool) {
    match deadline {
        None => (
            cond.wait(guard).unwrap_or_else(PoisonError::into_inner),
            false,
        ),
        Some(d) => {
            let left = d.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return (guard, true);
            }
            let (g, r) = cond
                .wait_timeout(guard, left)
                .unwrap_or_else(PoisonError::into_inner);
            (g, r.timed_out())
        }
    }
}

/// Where a pool's messages go (the server log).
pub type Logger = Arc<dyn Fn(Severity, &str) + Send + Sync>;

/// Pool settings read from `ns/db/pool/<name>` (`CreatePool`).
#[derive(Clone)]
pub struct PoolConfig {
    /// Pool name.
    pub name: String,
    /// Description from `ns/db/pools`.
    pub desc: String,
    /// Driver.
    pub driver: Arc<dyn DbDriver>,
    /// Datasource string.
    pub datasource: String,
    /// User.
    pub user: Option<String>,
    /// Password.
    pub password: Option<String>,
    /// `connections`; `<= 0` means 2.
    pub nhandles: i64,
    /// `verbose`: log every statement.
    pub verbose: bool,
    /// `logsqlerrors`.
    pub log_errors: bool,
    /// `maxidle` seconds (`< 0` means 600, 0 never expires).
    pub maxidle: i64,
    /// `maxopen` seconds (`< 0` means 3600, 0 never expires).
    pub maxopen: i64,
    /// `checkinterval` seconds (`< 0` means 600).
    pub checkinterval: i64,
}

impl PoolConfig {
    /// A pool with AOLserver's defaults.
    pub fn new(name: &str, driver: Arc<dyn DbDriver>, datasource: &str) -> Self {
        Self {
            name: name.to_owned(),
            desc: String::new(),
            driver,
            datasource: datasource.to_owned(),
            user: None,
            password: None,
            nhandles: 2,
            verbose: false,
            log_errors: false,
            maxidle: 600,
            maxopen: 3600,
            checkinterval: 600,
        }
    }
}

/// A connection pool (`struct Pool`).
pub struct Pool {
    /// Pool name.
    pub name: String,
    /// Description from `ns/db/pools`.
    pub desc: String,
    /// Driver.
    pub driver: Arc<dyn DbDriver>,
    /// Datasource string.
    pub datasource: String,
    /// User.
    pub user: Option<String>,
    /// Password.
    pub password: Option<String>,
    /// Number of handles.
    pub nhandles: usize,
    /// Initial `verbose` of each handle (`fVerbose`).
    pub verbose: bool,
    /// Log SQL errors (`fVerboseError`).
    pub log_errors: bool,
    /// Seconds a handle may sit idle before it is closed (0: never).
    pub maxidle: i64,
    /// Seconds a handle may stay open before it is closed (0: never).
    pub maxopen: i64,
    /// Seconds between idle-handle checks.
    pub checkinterval: i64,
    stale_on_close: AtomicU64,
    log: Logger,
    state: Mutex<PoolState>,
    wait_cond: Condvar,
    get_cond: Condvar,
}

struct PoolState {
    /// Handles not in use: connected ones first (`firstPtr` list).
    free: VecDeque<Slot>,
    /// A thread is collecting handles (`waiting`).
    waiting: bool,
    /// Handles held per thread (`IncrCount`'s thread-local table).
    owners: HashMap<ThreadId, usize>,
}

/// The state of one pool handle, kept while it is free or in use.
struct Slot {
    conn: Option<Box<dyn DbConnection>>,
    columns: Vec<String>,
    exception: (String, String),
    verbose: bool,
    otime: i64,
    atime: i64,
    stale: bool,
    stale_on_close: u64,
}

impl Slot {
    /// `NsDbDisconnect`.
    fn disconnect(&mut self) {
        self.conn = None;
        self.atime = 0;
        self.otime = 0;
        self.stale = false;
    }
}

/// Why [`Pool::get_handles`] returned no handles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GetError {
    /// The wait timed out (`NS_TIMEOUT`).
    Timeout,
    /// The request failed (`NS_ERROR`); the reason has been logged.
    Failed(String),
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// A database handle (`Ns_DbHandle`).  Dropping it returns it to its pool.
pub struct Handle {
    pool: Arc<Pool>,
    slot: Option<Slot>,
    owner: ThreadId,
}

impl Pool {
    /// Create a pool and its (unconnected) handles (`CreatePool`).
    pub fn new(cfg: PoolConfig, log: Logger) -> Self {
        let nhandles = usize::try_from(cfg.nhandles)
            .ok()
            .filter(|&n| n > 0)
            .unwrap_or(2);
        let free = (0..nhandles)
            .map(|_| Slot {
                conn: None,
                columns: Vec::new(),
                exception: (String::new(), String::new()),
                verbose: cfg.verbose,
                otime: 0,
                atime: 0,
                stale: false,
                stale_on_close: 0,
            })
            .collect();
        Self {
            name: cfg.name,
            desc: cfg.desc,
            driver: cfg.driver,
            datasource: cfg.datasource,
            user: cfg.user,
            password: cfg.password,
            nhandles,
            verbose: cfg.verbose,
            log_errors: cfg.log_errors,
            maxidle: if cfg.maxidle < 0 { 600 } else { cfg.maxidle },
            maxopen: if cfg.maxopen < 0 { 3600 } else { cfg.maxopen },
            checkinterval: if cfg.checkinterval < 0 {
                600
            } else {
                cfg.checkinterval
            },
            stale_on_close: AtomicU64::new(0),
            log,
            state: Mutex::new(PoolState {
                free,
                waiting: false,
                owners: HashMap::new(),
            }),
            wait_cond: Condvar::new(),
            get_cond: Condvar::new(),
        }
    }

    fn log(&self, sev: Severity, msg: &str) {
        (self.log)(sev, msg);
    }

    /// Take `n` handles, waiting up to `timeout` (`None`: for ever)
    /// (`Ns_DbPoolTimedGetMultipleHandles`).  A thread that already holds
    /// handles from this pool is refused at once.
    pub fn get_handles(
        self: &Arc<Self>,
        n: usize,
        timeout: Option<Duration>,
    ) -> Result<Vec<Handle>, GetError> {
        if self.nhandles < n {
            let m = format!(
                "dbinit: failed to get {n} handles from a db pool of only {} handles: '{}'",
                self.nhandles, self.name
            );
            self.log(Severity::Error, &m);
            return Err(GetError::Failed(m));
        }
        let tid = std::thread::current().id();
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut st = lock(&self.state);
        let prev = st.owners.get(&tid).copied().unwrap_or(0);
        if prev > 0 {
            drop(st);
            let m = format!(
                "dbinit: db handle limit exceeded: thread already owns {prev} handle{} from pool '{}'",
                if prev == 1 { "" } else { "s" },
                self.name
            );
            self.log(Severity::Error, &m);
            return Err(GetError::Failed(m));
        }
        if n == 0 {
            return Ok(Vec::new());
        }
        st.owners.insert(tid, n);
        // Become the exclusive acquiring thread, then collect handles as
        // they are returned.
        let mut timed_out = false;
        while !timed_out && st.waiting {
            (st, timed_out) = timed_wait(&self.wait_cond, st, deadline);
        }
        let mut got: Vec<Slot> = Vec::with_capacity(n);
        if !timed_out {
            st.waiting = true;
            while !timed_out && got.len() < n {
                while !timed_out && st.free.is_empty() {
                    (st, timed_out) = timed_wait(&self.get_cond, st, deadline);
                }
                if let Some(s) = st.free.pop_front() {
                    got.push(s);
                }
            }
            st.waiting = false;
            self.wait_cond.notify_one();
        }
        drop(st);
        // The last handle may arrive just as the wait times out.
        if timed_out && got.len() == n {
            timed_out = false;
        }
        let mut failed = timed_out;
        if !failed {
            for s in &mut got {
                if s.conn.is_none() && !self.connect(s) {
                    failed = true;
                    break;
                }
            }
        }
        if failed {
            let mut st = lock(&self.state);
            while let Some(s) = got.pop() {
                return_slot(&mut st, s);
            }
            if st.waiting {
                self.get_cond.notify_one();
            }
            st.owners.remove(&tid);
            drop(st);
            return Err(if timed_out {
                GetError::Timeout
            } else {
                GetError::Failed(format!(
                    "dbdrv: failed to open database '{}:{}'",
                    self.driver.name(),
                    self.datasource
                ))
            });
        }
        Ok(got
            .into_iter()
            .map(|slot| Handle {
                pool: Arc::clone(self),
                slot: Some(slot),
                owner: tid,
            })
            .collect())
    }

    /// Open a handle's connection (`Connect` / `NsDbOpen`).
    fn connect(&self, s: &mut Slot) -> bool {
        let drv = self.driver.name();
        self.log(
            Severity::Notice,
            &format!("dbdrv: opening database '{drv}:{}'", self.datasource),
        );
        match self.driver.open(
            &self.datasource,
            self.user.as_deref(),
            self.password.as_deref(),
        ) {
            Ok(c) => {
                let t = now();
                s.conn = Some(c);
                s.atime = t;
                s.otime = t;
                // Deviation: a reconnected handle is current with the
                // pool's bounce count (see docs/porting.md).
                s.stale_on_close = self.stale_on_close.load(Ordering::Relaxed);
                true
            }
            Err(e) => {
                self.log(
                    Severity::Error,
                    &format!("dbdrv: failed to open database '{drv}:{}'", self.datasource),
                );
                if !e.message.is_empty() {
                    self.log(Severity::Error, &format!("dbdrv: {}", e.message));
                }
                s.disconnect();
                false
            }
        }
    }

    /// Whether a handle should be closed (`IsStale`).
    fn is_stale(&self, s: &Slot, now: i64) -> bool {
        if s.conn.is_none() {
            return false;
        }
        let min_access = now - self.maxidle;
        let min_open = now - self.maxopen;
        if (self.maxidle != 0 && s.atime < min_access)
            || (self.maxopen != 0 && s.otime < min_open)
            || s.stale
            || self.stale_on_close.load(Ordering::Relaxed) > s.stale_on_close
        {
            if self.verbose {
                self.log(
                    Severity::Notice,
                    &format!(
                        "dbinit: closing {} handle in pool '{}'",
                        if s.atime < min_access { "idle" } else { "old" },
                        self.name
                    ),
                );
            }
            return true;
        }
        false
    }

    /// Clean up a handle and return it (`Ns_DbPoolPutHandle`).
    fn put_slot(&self, mut s: Slot, owner: ThreadId) {
        if let Some(c) = s.conn.as_mut() {
            c.flush();
            let _ = c.reset();
        }
        s.columns.clear();
        s.exception = (String::new(), String::new());
        let t = now();
        if self.is_stale(&s, t) {
            s.disconnect();
        } else {
            s.atime = t;
        }
        let mut st = lock(&self.state);
        if let Some(c) = st.owners.get_mut(&owner) {
            *c = c.saturating_sub(1);
            if *c == 0 {
                st.owners.remove(&owner);
            }
        }
        return_slot(&mut st, s);
        if st.waiting {
            self.get_cond.notify_one();
        }
    }

    /// Close every free handle that has gone stale as of `now`
    /// (`CheckPool`).
    pub fn check(&self, now: i64) {
        let list: Vec<Slot> = lock(&self.state).free.drain(..).collect();
        if list.is_empty() {
            return;
        }
        let mut checked = Vec::with_capacity(list.len());
        for mut s in list {
            if self.is_stale(&s, now) {
                s.disconnect();
            }
            checked.push(s);
        }
        let mut st = lock(&self.state);
        for s in checked.into_iter().rev() {
            return_slot(&mut st, s);
        }
        if st.waiting {
            self.get_cond.notify_one();
        }
    }

    /// Close all free handles and mark in-use ones to be closed when they
    /// are returned (`Ns_DbBouncePool`).
    pub fn bounce(&self) {
        {
            let mut st = lock(&self.state);
            let soc = self.stale_on_close.fetch_add(1, Ordering::Relaxed) + 1;
            for s in &mut st.free {
                if s.conn.is_some() {
                    s.stale = true;
                }
                s.stale_on_close = soc;
            }
        }
        self.check(now());
    }

    /// Handles currently checked out.
    pub fn outstanding(&self) -> usize {
        self.nhandles - lock(&self.state).free.len()
    }

    /// Free handles with an open connection.
    pub fn connected(&self) -> usize {
        lock(&self.state)
            .free
            .iter()
            .filter(|s| s.conn.is_some())
            .count()
    }
}

/// `ReturnHandle`: connected handles go to the front, others to the back.
fn return_slot(st: &mut PoolState, s: Slot) {
    if s.conn.is_some() {
        st.free.push_front(s);
    } else {
        st.free.push_back(s);
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if let Some(s) = self.slot.take() {
            self.pool.put_slot(s, self.owner);
        }
    }
}

impl Handle {
    fn s(&mut self) -> &mut Slot {
        self.slot
            .as_mut()
            .expect("a handle owns its slot until dropped")
    }

    fn r(&self) -> &Slot {
        self.slot
            .as_ref()
            .expect("a handle owns its slot until dropped")
    }

    /// Return the handle to its pool (`Ns_DbPoolPutHandle`).
    pub fn release(self) {
        drop(self);
    }

    /// The owning pool.
    pub const fn pool(&self) -> &Arc<Pool> {
        &self.pool
    }

    /// Whether the handle is connected.
    pub fn connected(&self) -> bool {
        self.r().conn.is_some()
    }

    /// The pending exception code and message.
    pub fn exception(&self) -> (&str, &str) {
        let e = &self.r().exception;
        (&e.0, &e.1)
    }

    /// Set the exception (`Ns_DbSetException`).
    pub fn set_exception(&mut self, code: &str, msg: &str) {
        self.s().exception = (code.to_owned(), msg.to_owned());
    }

    /// Clear the exception.
    pub fn clear_exception(&mut self) {
        self.s().exception = (String::new(), String::new());
    }

    /// Whether statements are logged.
    pub fn verbose(&self) -> bool {
        self.r().verbose
    }

    /// Turn statement logging on or off.
    pub fn set_verbose(&mut self, on: bool) {
        self.s().verbose = on;
    }

    /// Close the connection (`NsDbDisconnect`).
    pub fn disconnect(&mut self) {
        self.s().disconnect();
    }

    /// `NsDbLogSql`.
    fn log_sql(&self, sql: &str) {
        let s = self.r();
        if !s.exception.1.is_empty() {
            if self.pool.log_errors || s.verbose {
                self.pool.log(
                    Severity::Error,
                    &format!(
                        "dbinit: error({},{}): '{sql}'",
                        self.pool.datasource, s.exception.1
                    ),
                );
            }
        } else if s.verbose {
            self.pool.log(
                Severity::Notice,
                &format!("dbinit: sql({}): '{sql}'", self.pool.datasource),
            );
        }
    }

    /// Execute SQL, recording any error as the exception (`Ns_DbExec`).
    pub fn exec(&mut self, sql: &str) -> Result<ExecResult, DbError> {
        let s = self.s();
        let Some(c) = s.conn.as_mut() else {
            return Err(DbError::msg("handle not connected"));
        };
        let r = c.exec(sql);
        match &r {
            Ok(ExecResult::Rows(cols)) => s.columns.clone_from(cols),
            Ok(ExecResult::Dml(_)) => s.columns.clear(),
            Err(e) => s.exception = (e.code.clone(), e.message.clone()),
        }
        self.log_sql(sql);
        r
    }

    /// Run a query and return its columns (`Ns_DbSelect`).
    pub fn select(&mut self, sql: &str) -> Option<Vec<String>> {
        if !self.connected() {
            return None;
        }
        if let Ok(ExecResult::Rows(cols)) = self.exec(sql) {
            return Some(cols);
        }
        if self.r().exception.1.is_empty() {
            self.set_exception("NSDB", "Query was not a statement returning rows.");
        }
        None
    }

    /// Run a statement that returns no rows (`Ns_DbDML`).
    pub fn dml(&mut self, sql: &str) -> bool {
        if !self.connected() {
            return false;
        }
        match self.exec(sql) {
            Ok(ExecResult::Dml(_)) => true,
            Ok(ExecResult::Rows(_)) => {
                self.set_exception("NSDB", "Query was not a DML or DDL command.");
                self.flush();
                false
            }
            Err(_) => false,
        }
    }

    /// The columns of the pending query (`Ns_DbBindRow`).
    pub fn bind_row(&self) -> Option<Vec<String>> {
        self.connected().then(|| self.r().columns.clone())
    }

    /// The next row of the pending query, `None` at the end
    /// (`Ns_DbGetRow`).
    pub fn next_row(&mut self) -> Result<Option<Vec<Option<String>>>, DbError> {
        let s = self.s();
        let Some(c) = s.conn.as_mut() else {
            return Err(DbError::msg("handle not connected"));
        };
        let r = c.next_row();
        if let Err(e) = &r {
            s.exception = (e.code.clone(), e.message.clone());
        }
        r
    }

    /// Discard pending rows (`Ns_DbFlush`, `Ns_DbCancel`).
    pub fn flush(&mut self) -> bool {
        match self.s().conn.as_mut() {
            Some(c) => {
                c.flush();
                true
            }
            None => false,
        }
    }

    /// Reset the connection (`Ns_DbResetHandle`).
    pub fn reset(&mut self) -> bool {
        let s = self.s();
        let Some(c) = s.conn.as_mut() else {
            return false;
        };
        match c.reset() {
            Ok(()) => true,
            Err(e) => {
                s.exception = (e.code, e.message);
                false
            }
        }
    }

    /// Run a query expected to return at most one row: the columns and
    /// the row, if any (`Ns_Db0or1Row`).
    pub fn zero_or_one_row(
        &mut self,
        sql: &str,
    ) -> Option<(Vec<String>, Option<Vec<Option<String>>>)> {
        let cols = self.select(sql)?;
        let Some(row) = self.next_row().ok()? else {
            return Some((cols, None));
        };
        if self.next_row().ok()?.is_none() {
            return Some((cols, Some(row)));
        }
        self.set_exception(NS_SQLERRORCODE, "Query returned more than one row.");
        self.flush();
        None
    }

    /// Run a query expected to return exactly one row (`Ns_Db1Row`).
    pub fn one_row(&mut self, sql: &str) -> Option<(Vec<String>, Vec<Option<String>>)> {
        if let (cols, Some(row)) = self.zero_or_one_row(sql)? {
            return Some((cols, row));
        }
        self.set_exception(NS_SQLERRORCODE, "Query did not return a row.");
        None
    }

    /// Run each `;`-terminated statement of a file
    /// (`Ns_DbInterpretSqlFile`).
    pub fn interpret_sql_file(&mut self, path: &str) -> bool {
        let Ok(text) = std::fs::read(path) else {
            self.set_exception(NS_SQLERRORCODE, "Could not read file");
            return false;
        };
        let (stmts, rest) = split_sql(&text);
        for sql in &stmts {
            if self.exec(sql).is_err() {
                return false;
            }
        }
        if rest.bytes().any(|b| !is_c_space(b)) {
            self.set_exception(NS_SQLERRORCODE, "File ends with unterminated SQL");
            return false;
        }
        true
    }
}

/// C `isspace` in the "C" locale.
const fn is_c_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Per-server settings (`ServData`).
#[derive(Debug, Clone, Default)]
struct ServData {
    allowed: Vec<String>,
    defpool: Option<String>,
}

/// All pools and per-server access rules.
pub struct DbState {
    pools: BTreeMap<String, Arc<Pool>>,
    servers: RwLock<HashMap<String, ServData>>,
}

impl std::fmt::Debug for DbState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbState")
            .field("pools", &self.pools.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// Drivers available to `ns/db/pool/*/driver`, by name.
pub fn drivers() -> HashMap<&'static str, Arc<dyn DbDriver>> {
    let sqlite: Arc<dyn DbDriver> = Arc::new(SqliteDriver);
    let mut m: HashMap<&'static str, Arc<dyn DbDriver>> = HashMap::new();
    m.insert("sqlite", Arc::clone(&sqlite));
    m.insert("nssqlite", Arc::clone(&sqlite));
    m.insert("sqlite3", sqlite);
    m
}

impl DbState {
    /// Read pools from `config` (`NsDbInitPools`); messages go to `log`.
    pub fn build(c: &Config, log: &Logger) -> Self {
        let avail = drivers();
        let mut names: HashMap<String, Arc<dyn DbDriver>> = HashMap::new();
        if let Some(sec) = c.section("ns/db/drivers") {
            for (name, file) in sec.iter() {
                let key = nsd::module::module_key(name, file.unwrap_or(""));
                match avail.get(key.as_str()).or_else(|| avail.get(name)) {
                    Some(d) => {
                        names.insert(name.to_owned(), Arc::clone(d));
                    }
                    None => log(
                        Severity::Error,
                        &format!("dbdrv: no such driver: {name} ({})", file.unwrap_or("")),
                    ),
                }
            }
        }
        let mut pools = BTreeMap::new();
        if let Some(sec) = c.section("ns/db/pools") {
            for (name, desc) in sec.iter() {
                if pools.contains_key(name) {
                    log(Severity::Error, &format!("dbinit: duplicate pool: {name}"));
                    continue;
                }
                let path = format!("ns/db/pool/{name}");
                let Some(dname) = c.get(&path, "driver") else {
                    log(
                        Severity::Error,
                        &format!("dbinit: no driver for pool '{name}'"),
                    );
                    continue;
                };
                let Some(drv) = names.get(dname).or_else(|| avail.get(dname)).cloned() else {
                    log(
                        Severity::Error,
                        &format!("dbinit: no such driver '{dname}' for pool '{name}'"),
                    );
                    continue;
                };
                let Some(ds) = c.get(&path, "datasource") else {
                    log(
                        Severity::Error,
                        &format!("dbinit: missing datasource for pool '{name}'"),
                    );
                    continue;
                };
                let mut cfg = PoolConfig::new(name, drv, ds);
                desc.unwrap_or("").clone_into(&mut cfg.desc);
                cfg.user = c.get(&path, "user").map(str::to_owned);
                cfg.password = c.get(&path, "password").map(str::to_owned);
                cfg.nhandles = c.int_or(&path, "connections", 2);
                cfg.verbose = c.bool_or(&path, "verbose", false);
                cfg.log_errors = c.bool_or(&path, "logsqlerrors", false);
                cfg.maxidle = c.int_or(&path, "maxidle", 600);
                cfg.maxopen = c.int_or(&path, "maxopen", 3600);
                cfg.checkinterval = c.int_or(&path, "checkinterval", 600);
                pools.insert(name.to_owned(), Arc::new(Pool::new(cfg, Arc::clone(log))));
            }
        }
        Self {
            pools,
            servers: RwLock::new(HashMap::new()),
        }
    }

    /// Read pools from the server config and schedule their checks.
    pub fn from_config(g: &Arc<Global>) -> Self {
        let w = Arc::downgrade(g);
        let log: Logger = Arc::new(move |sev, m| {
            if let Some(g) = w.upgrade() {
                g.log(sev, m);
            }
        });
        let s = Self::build(&g.config, &log);
        for p in s.pools.values() {
            let w = Arc::downgrade(p);
            let secs = u64::try_from(p.checkinterval).unwrap_or(0);
            g.sched.schedule(
                Arc::new(move |_| {
                    if let Some(p) = w.upgrade() {
                        p.check(now());
                    }
                }),
                SchedFlags::default(),
                Repeat::Interval(Duration::from_secs(secs)),
                &format!("nsdb:check {}", p.name),
            );
        }
        s
    }

    /// A pool by name, whatever the server (`GetPool`).
    pub fn get_pool(&self, name: &str) -> Option<&Arc<Pool>> {
        self.pools.get(name)
    }

    /// Configure which pools `server` may use (`NsDbInitServer`).
    pub fn configure_server(&self, c: &Config, server: &str, log: &dyn Fn(Severity, &str)) {
        let path = format!("ns/server/{server}/db");
        let mut defpool = c.get(&path, "defaultpool").map(str::to_owned);
        if let Some(d) = &defpool {
            if !self.pools.contains_key(d) {
                log(
                    Severity::Error,
                    &format!("dbinit: no such default pool '{d}'"),
                );
                defpool = None;
            }
        }
        let allowed: Vec<String> = match c.get(&path, "pools") {
            Some("*") => self.pools.keys().cloned().collect(),
            Some(list) => list
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|p| self.pools.contains_key(p))
                .collect(),
            None => Vec::new(),
        };
        self.servers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(server.to_owned(), ServData { allowed, defpool });
    }

    fn serv(&self, server: &str) -> ServData {
        self.servers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(server)
            .cloned()
            .unwrap_or_default()
    }

    /// Pools `server` may use (`Ns_DbPoolList`).
    pub fn server_pools(&self, server: &str) -> Vec<String> {
        self.serv(server).allowed
    }

    /// Whether `server` may use `pool` (`Ns_DbPoolAllowable`).
    pub fn allowable(&self, server: &str, pool: &str) -> bool {
        self.serv(server).allowed.iter().any(|p| p == pool)
    }

    /// The server's default pool (`Ns_DbPoolDefault`).
    pub fn default_pool(&self, server: &str) -> Option<String> {
        self.serv(server).defpool
    }
}

/// Values keyed by a live `Arc<K>`.  A key is held as a `Weak`, which keeps
/// its allocation (so its address cannot be reused) until the entry is
/// pruned once the key has been dropped.
struct Registry<K, V> {
    entries: Mutex<Vec<(Weak<K>, Arc<V>)>>,
}

impl<K, V> Registry<K, V> {
    const fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }

    /// The value for `key`, made with `make` if absent; whether it is new.
    fn get_or_insert(&self, key: &Arc<K>, make: impl FnOnce() -> V) -> (Arc<V>, bool) {
        let w = Arc::downgrade(key);
        let mut v = lock(&self.entries);
        v.retain(|(k, _)| k.strong_count() > 0);
        if let Some((_, s)) = v.iter().find(|(k, _)| k.ptr_eq(&w)) {
            return (Arc::clone(s), false);
        }
        let s = Arc::new(make());
        v.push((w, Arc::clone(&s)));
        (s, true)
    }

    /// Forget the entry holding `value`.
    fn forget(&self, value: &Weak<V>) {
        let gone: Vec<(Weak<K>, Arc<V>)> = {
            let mut v = lock(&self.entries);
            let (gone, keep) = std::mem::take(&mut *v)
                .into_iter()
                .partition(|(_, s)| Weak::ptr_eq(&Arc::downgrade(s), value));
            *v = keep;
            gone
        };
        drop(gone);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        lock(&self.entries).len()
    }
}

static STATES: Registry<Global, DbState> = Registry::new();

/// The pools of `g`'s process, created on first use and dropped at exit.
fn state_for(g: &Arc<Global>) -> Arc<DbState> {
    let (s, created) = STATES.get_or_insert(g, || DbState::from_config(g));
    if created {
        let ws = Arc::downgrade(&s);
        g.callbacks
            .register(When::Exit, Arc::new(move || STATES.forget(&ws)));
    }
    s
}

/// `Database operation "op" failed (exception CODE, "msg")`: the trailer
/// of `DbObjCmd`.
fn op_failed(op: &str, exc: Option<(&str, &str)>) -> Exception {
    let mut m = format!("Database operation \"{op}\" failed");
    if let Some((code, msg)) = exc {
        if !code.is_empty() {
            m.push_str(" (exception ");
            m.push_str(code);
            if !msg.is_empty() {
                m.push_str(", \"");
                m.push_str(msg);
                m.push('"');
            }
            m.push(')');
        }
    }
    Exception::error(m)
}

fn failed(op: &str, h: &Handle) -> Exception {
    op_failed(op, Some(h.exception()))
}

fn columns_set(cols: &[String]) -> NsSet {
    let mut s = NsSet::default();
    for c in cols {
        s.put(c, Some(""));
    }
    s
}

fn row_set(cols: &[String], row: &[Option<String>]) -> NsSet {
    let mut s = NsSet::default();
    for (c, v) in cols.iter().zip(row) {
        s.put(c, Some(v.as_deref().unwrap_or("")));
    }
    s
}

/// Per-interp handle table (`InterpData`).
#[derive(Default)]
struct InterpData {
    dbs: HashMap<String, Handle>,
    cleanup: bool,
}

impl InterpData {
    /// Store a handle under a fresh id (`EnterHandle`).
    fn enter(&mut self, h: Handle) -> String {
        let mut next = self.dbs.len();
        loop {
            let id = format!("nsdb{next:x}");
            next += 1;
            if !self.dbs.contains_key(&id) {
                self.dbs.insert(id.clone(), h);
                return id;
            }
        }
    }
}

type InterpDataRef = Rc<RefCell<InterpData>>;

fn interp_data(interp: &dyn Interp) -> InterpDataRef {
    interp.ext().shared(InterpData::default)
}

/// `ReleaseDbs`: return every handle `interp` holds.
pub fn release_all(interp: &dyn Interp) {
    if let Some(d) = interp.ext().get::<InterpDataRef>() {
        let dbs = {
            let mut d = d.borrow_mut();
            d.cleanup = false;
            std::mem::take(&mut d.dbs)
        };
        drop(dbs);
    }
}

fn invalid_id(id: &str) -> Exception {
    Exception::error(format!("invalid database id:  \"{id}\""))
}

/// Run `f` on handle `id` of `interp` (`GetHandle`); `clear` resets the
/// exception first.  No interp code runs while the table is borrowed.
fn with_handle<R>(
    interp: &dyn Interp,
    id: &str,
    clear: bool,
    f: impl FnOnce(&mut Handle) -> R,
) -> Result<R, Exception> {
    let d = interp_data(interp);
    let mut d = d.borrow_mut();
    let h = d.dbs.get_mut(id).ok_or_else(|| invalid_id(id))?;
    if clear {
        h.clear_exception();
    }
    Ok(f(h))
}

const OPTS: [&str; 31] = [
    "getrow",
    "gethandle",
    "releasehandle",
    "select",
    "dml",
    "1row",
    "0or1row",
    "bindrow",
    "exec",
    "sp_exec",
    "sp_getparams",
    "sp_returncode",
    "sp_setparam",
    "sp_start",
    "exception",
    "flush",
    "bouncepool",
    "cancel",
    "connected",
    "datasource",
    "dbtype",
    "disconnect",
    "driver",
    "interpretsqlfile",
    "password",
    "poolname",
    "pools",
    "resethandle",
    "setexception",
    "user",
    "verbose",
];

fn sp_unsupported(opt: &str, h: &Handle) -> Exception {
    Exception::error(format!(
        "ns_db {opt}: stored procedures are not supported by the {} driver",
        h.pool.driver.name()
    ))
}

/// `ns_db` (`DbObjCmd`).
fn ns_db(interp: &dyn Interp, argv: &[String], db: &DbState, server: &str) -> ScriptResult {
    let (_, opt) = nsd::cmds::subcommand(argv, &OPTS)?;
    match opt {
        "bindrow" | "cancel" | "connected" | "datasource" | "dbtype" | "disconnect" | "driver"
        | "flush" | "password" | "poolname" | "releasehandle" | "resethandle" | "sp_exec"
        | "sp_getparams" | "sp_returncode" | "user" => {
            if argv.len() != 3 {
                return Err(wrong_args(argv, 2, "dbId"));
            }
            handle_cmd(interp, opt, &argv[2])
        }
        "exception" => {
            if argv.len() != 3 {
                return Err(wrong_args(argv, 2, "dbId"));
            }
            let (c, m) = with_handle(interp, &argv[2], false, |h| {
                let (c, m) = h.exception();
                (c.to_owned(), m.to_owned())
            })?;
            Ok(interp.list_from(&<[String; 2]>::from((c, m))))
        }
        "0or1row" | "1row" | "dml" | "exec" | "getrow" | "interpretsqlfile" | "select"
        | "sp_start" => {
            if argv.len() != 4 {
                return Err(wrong_args(argv, 2, "dbId arg"));
            }
            handle_sql_cmd(interp, opt, &argv[2], &argv[3])
        }
        "sp_setparam" => {
            if argv.len() != 7 {
                return Err(wrong_args(argv, 2, "dbId paramname type in|out value"));
            }
            ns_script::get_index(&argv[5], &["in", "out"], "option")?;
            Err(with_handle(interp, &argv[2], true, |h| {
                sp_unsupported(opt, h)
            })?)
        }
        "gethandle" => gethandle(interp, argv, db, server),
        "bouncepool" => {
            if argv.len() != 3 {
                return Err(wrong_args(argv, 2, "pool"));
            }
            let p = db.get_pool(&argv[2]).ok_or_else(|| op_failed(opt, None))?;
            p.bounce();
            Ok(String::new())
        }
        "pools" => {
            if argv.len() > 2 {
                return Err(wrong_args(argv, 2, ""));
            }
            Ok(interp.list_from(&db.server_pools(server)))
        }
        "setexception" => {
            if argv.len() != 5 {
                return Err(wrong_args(argv, 2, "dbId code message"));
            }
            let (code, msg) = (&argv[3], &argv[4]);
            with_handle(interp, &argv[2], true, |h| {
                if code.len() > 5 {
                    return Err(Exception::error(format!(
                        "code \"{code}\" more than 5 characters"
                    )));
                }
                h.set_exception(code, msg);
                Ok(String::new())
            })?
        }
        _ => {
            // verbose
            if argv.len() != 3 && argv.len() != 4 {
                return Err(wrong_args(argv, 2, "dbId ?bool?"));
            }
            with_handle(interp, &argv[2], true, |h| {
                if let Some(v) = argv.get(3) {
                    h.set_verbose(ns_script::parse_bool(v)?);
                }
                Ok(nsd::cmds::bool_str(h.verbose()))
            })?
        }
    }
}

/// Subcommands taking just a handle id.
fn handle_cmd(interp: &dyn Interp, opt: &str, id: &str) -> ScriptResult {
    if opt == "releasehandle" {
        let d = interp_data(interp);
        let h = d
            .borrow_mut()
            .dbs
            .remove(id)
            .ok_or_else(|| invalid_id(id))?;
        h.release();
        return Ok(String::new());
    }
    if opt == "bindrow" {
        let cols = with_handle(interp, id, true, |h| {
            h.bind_row().ok_or_else(|| failed(opt, h))
        })??;
        return nsd::cmds::set::enter_set(interp, set_ref(columns_set(&cols)), false, false);
    }
    with_handle(interp, id, true, |h| {
        let p = Arc::clone(&h.pool);
        Ok(match opt {
            "cancel" | "flush" => {
                if !h.flush() {
                    return Err(failed(opt, h));
                }
                String::new()
            }
            "connected" => nsd::cmds::bool_str(h.connected()),
            "datasource" => p.datasource.clone(),
            "dbtype" => {
                if h.connected() {
                    p.driver.db_type().to_owned()
                } else {
                    String::new()
                }
            }
            "disconnect" => {
                h.disconnect();
                String::new()
            }
            "driver" => p.driver.name().to_owned(),
            "password" => p.password.clone().unwrap_or_default(),
            "poolname" => p.name.clone(),
            "resethandle" => {
                if !h.reset() {
                    return Err(failed(opt, h));
                }
                "0".to_owned()
            }
            "user" => p.user.clone().unwrap_or_default(),
            _ => return Err(sp_unsupported(opt, h)),
        })
    })?
}

/// Subcommands taking a handle id and one argument.
fn handle_sql_cmd(interp: &dyn Interp, opt: &str, id: &str, arg: &str) -> ScriptResult {
    match opt {
        "0or1row" => {
            let r = with_handle(interp, id, true, |h| {
                h.zero_or_one_row(arg).ok_or_else(|| failed(opt, h))
            })??;
            match r {
                (_, None) => Ok(String::new()),
                (cols, Some(row)) => {
                    nsd::cmds::set::enter_set(interp, set_ref(row_set(&cols, &row)), true, false)
                }
            }
        }
        "1row" => {
            let (cols, row) = with_handle(interp, id, true, |h| {
                h.one_row(arg).ok_or_else(|| failed(opt, h))
            })??;
            nsd::cmds::set::enter_set(interp, set_ref(row_set(&cols, &row)), true, false)
        }
        "dml" => with_handle(interp, id, true, |h| {
            if h.dml(arg) {
                Ok(String::new())
            } else {
                Err(failed(opt, h))
            }
        })?,
        "exec" => with_handle(interp, id, true, |h| match h.exec(arg) {
            Ok(ExecResult::Dml(_)) => Ok("NS_DML".to_owned()),
            Ok(ExecResult::Rows(_)) => Ok("NS_ROWS".to_owned()),
            Err(_) => Err(failed(opt, h)),
        })?,
        "getrow" => {
            // The set is looked up before a row is fetched (`Ns_TclGetSet2`).
            with_handle(interp, id, true, |_| ())?;
            let set = nsd::cmds::set::lookup_set(interp, arg, false)?;
            let row = with_handle(interp, id, false, |h| {
                h.next_row().map_err(|_| failed(opt, h))
            })??;
            match row {
                None => Ok("0".into()),
                Some(vals) => {
                    let mut s = lock_set(&set);
                    for (i, v) in vals.iter().enumerate() {
                        if i < s.len() {
                            s.put_value(i, Some(v.as_deref().unwrap_or("")));
                        }
                    }
                    Ok("1".into())
                }
            }
        }
        "interpretsqlfile" => with_handle(interp, id, true, |h| {
            if h.interpret_sql_file(arg) {
                Ok(String::new())
            } else {
                Err(failed(opt, h))
            }
        })?,
        "select" => {
            let cols = with_handle(interp, id, true, |h| {
                h.select(arg).ok_or_else(|| failed(opt, h))
            })??;
            nsd::cmds::set::enter_set(interp, set_ref(columns_set(&cols)), false, false)
        }
        _ => Err(with_handle(interp, id, true, |h| sp_unsupported(opt, h))?),
    }
}

/// `ns_db gethandle ?-timeout seconds? ?pool? ?nhandles?`.
fn gethandle(interp: &dyn Interp, argv: &[String], db: &DbState, server: &str) -> ScriptResult {
    let mut args = &argv[2..];
    let mut timeout = None;
    if args.len() >= 2 {
        if args[0] == "-timeout" {
            // A negative timeout waits for ever.
            let t = nsd::cmds::int_arg(&args[1])?;
            timeout = u64::try_from(t).ok().map(Duration::from_secs);
            args = &args[2..];
        } else if args.len() > 2 {
            return Err(wrong_args(argv, 2, "?-timeout seconds? ?pool? ?nhandles?"));
        }
    }
    let pool = match args.first() {
        Some(p) => p.clone(),
        None => db
            .default_pool(server)
            .ok_or_else(|| Exception::error("no defaultpool configured"))?,
    };
    if !db.allowable(server, &pool) {
        return Err(Exception::error(format!("no access to pool: \"{pool}\"")));
    }
    let n = match args.get(1) {
        None => 1,
        Some(a) => {
            let n = nsd::cmds::int_arg(a)?;
            if n <= 0 {
                return Err(Exception::error(format!(
                    "invalid nhandles:  \"{a}\": should be greater than 0."
                )));
            }
            n
        }
    };
    let could_not = || {
        Exception::error(format!(
            "could not allocate {n} handle(s) from pool \"{pool}\""
        ))
    };
    let p = db.get_pool(&pool).ok_or_else(could_not)?;
    let handles = match p.get_handles(usize::try_from(n).unwrap_or(usize::MAX), timeout) {
        Ok(h) => h,
        Err(GetError::Timeout) => return Ok(String::new()),
        Err(GetError::Failed(_)) => return Err(could_not()),
    };
    let d = interp_data(interp);
    let (ids, register) = {
        let mut d = d.borrow_mut();
        let ids: Vec<String> = handles.into_iter().rev().map(|h| d.enter(h)).collect();
        (ids, !d.cleanup)
    };
    if register {
        if let Ok(nsi) = nsd::interp::ns_interp(interp) {
            nsi.borrow_mut().defer.push(Box::new(release_all));
            d.borrow_mut().cleanup = true;
        }
    }
    Ok(interp.list_from(&ids))
}

/// Split a SQL file into the statements `Ns_DbInterpretSqlFile` runs and
/// the text left after the last `;`.  Only single quotes quote (a doubled
/// quote stays doubled), and `--` starts a comment only at the start of a
/// line.
pub fn split_sql(text: &[u8]) -> (Vec<String>, String) {
    let mut out = Vec::new();
    let mut sql: Vec<u8> = Vec::new();
    let mut inquote = false;
    let mut c = b'\n';
    let mut it = text.iter().copied();
    'outer: while let Some(ch) = it.next() {
        let mut lastc = c;
        c = ch;
        // `loopstart`: re-examine `c` without reading a new character.
        loop {
            if inquote {
                if c == b'\'' {
                    let Some(n) = it.next() else {
                        break 'outer;
                    };
                    lastc = c;
                    c = n;
                    if c == b'\'' {
                        sql.extend_from_slice(b"''");
                    } else {
                        sql.push(b'\'');
                        inquote = false;
                        continue;
                    }
                } else {
                    sql.push(c);
                }
            } else if c == b'-' && lastc == b'\n' {
                let Some(n) = it.next() else {
                    break 'outer;
                };
                lastc = c;
                c = n;
                if c != b'-' {
                    sql.push(b'-');
                    continue;
                }
                for n in it.by_ref() {
                    c = n;
                    if c == b'\n' {
                        break;
                    }
                }
            } else if c == b';' {
                out.push(String::from_utf8_lossy(&sql).into_owned());
                sql.clear();
            } else {
                sql.push(c);
                if c == b'\'' {
                    inquote = true;
                }
            }
            break;
        }
    }
    (out, String::from_utf8_lossy(&sql).into_owned())
}

/// Convert a list of SQL-quoted values to a Tcl list
/// (`ns_quotelisttolist`, `QuoteListToListCmd`).
pub fn quote_list_to_list(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut inquotes = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_whitespace() && !inquotes {
            if !buf.is_empty() {
                out.push(std::mem::take(&mut buf));
            }
            while chars.next_if(char::is_ascii_whitespace).is_some() {}
        } else if c == '\\' && chars.peek().is_some() {
            if let Some(n) = chars.next() {
                buf.push(n);
            }
        } else if c == '\'' {
            if inquotes {
                out.push(std::mem::take(&mut buf));
            }
            inquotes = !inquotes;
        } else {
            buf.push(c);
        }
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

/// Parse one CSV line (`ns_getcsv`, `GetCsvCmd`).  Unquoted fields are
/// trimmed, and a blank line has no fields.
pub fn parse_csv(line: &str) -> Vec<String> {
    let mut cols = Vec::new();
    let mut elem = String::new();
    let (mut inquote, mut quoted, mut blank) = (false, false, true);
    let mut chars = line.chars();
    'outer: while let Some(ch) = chars.next() {
        let mut c = ch;
        // `loopstart`
        loop {
            if inquote {
                if c == '"' {
                    let Some(n) = chars.next() else {
                        break 'outer;
                    };
                    c = n;
                    if c != '"' {
                        inquote = false;
                        continue;
                    }
                }
                elem.push(c);
            } else if c == '\n' || c == '\r' {
                break 'outer;
            } else if c == '"' {
                inquote = true;
                quoted = true;
                blank = false;
            } else if elem.is_empty() && c.is_ascii_whitespace() {
                // Leading space is skipped.
            } else if c == ',' {
                if !quoted {
                    elem.truncate(elem.trim_end().len());
                }
                cols.push(std::mem::take(&mut elem));
                quoted = false;
            } else {
                blank = false;
                elem.push(c);
            }
            break;
        }
    }
    if !quoted {
        elem.truncate(elem.trim_end().len());
    }
    if !blank {
        cols.push(elem);
    }
    cols
}

/// Module init: global pools on first use, per-server access and commands.
pub fn init(ctx: &ModuleContext<'_>) -> Result<(), NsError> {
    let server = ctx
        .server
        .ok_or_else(|| NsError::Config("nsdb must be loaded as a per-server module".into()))?;
    let g = ctx.global;
    let db = state_for(g);
    db.configure_server(&g.config, &server.name, &|sev, m| g.log(sev, m));
    for p in db.server_pools(&server.name) {
        g.log(
            Severity::Notice,
            &format!("dbinit: pool '{p}' available to server {}", server.name),
        );
    }
    let (d, name) = (Arc::clone(&db), server.name.clone());
    server.add_command("ns_db", command(move |i, a| ns_db(i, a, &d, &name)));
    server.add_command(
        "ns_quotelisttolist",
        command(|i, a| {
            nsd::cmds::check_args(a, 2, 2, 1, "quotelist")?;
            Ok(i.list_from(&quote_list_to_list(&a[1])))
        }),
    );
    server.add_command(
        "ns_getcsv",
        command(|i, a| {
            nsd::cmds::check_args(a, 3, 3, 1, "fileId varName")?;
            let line = i.eval_words(&["gets", &a[1]])?;
            if line.is_empty() && i.eval_words(&["eof", &a[1]])? == "1" {
                return Ok("-1".into());
            }
            let fields = parse_csv(&line);
            i.set_var(&a[2], &i.list_from(&fields))?;
            Ok(fields.len().to_string())
        }),
    );
    for (cmd, msg) in [("ns_dberrorcode", false), ("ns_dberrormsg", true)] {
        server.add_command(
            cmd,
            command(move |i, a| {
                nsd::cmds::check_args(a, 2, 2, 1, "dbId")?;
                with_handle(i, &a[1], false, |h| {
                    let (c, m) = h.exception();
                    (if msg { m } else { c }).to_owned()
                })
            }),
        );
    }
    let name = server.name.clone();
    server.add_command(
        "ns_dbconfigpath",
        command(move |_, _| Ok(format!("ns/server/{name}/db"))),
    );
    let d = Arc::clone(&db);
    server.add_command(
        "ns_pooldescription",
        command(move |_, a| {
            nsd::cmds::check_args(a, 2, 2, 1, "poolname")?;
            Ok(d.get_pool(&a[1])
                .map(|p| p.desc.clone())
                .unwrap_or_default())
        }),
    );
    Ok(())
}

#[cfg(test)]
mod tests;
