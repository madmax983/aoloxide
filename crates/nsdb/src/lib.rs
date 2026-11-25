//! # nsdb
//!
//! The AOLserver database module (`nsdb/dbinit.c`, `dbdrv.c`, `dbtcl.c`):
//! named connection pools, handles, and the `ns_db` command, with a
//! pluggable [`DbDriver`] interface and a bundled SQLite driver.
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
//! ns_section ns/server/s1/db
//! ns_param pools *
//! ns_param defaultpool main
//! ```
//!
//! Handles taken with `ns_db gethandle` are released automatically when the
//! interp is cleaned up (`ns_cleanup`), as in AOLserver.

pub mod driver;

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

pub use driver::{DbConnection, DbDriver, DbError, ExecResult, SqliteDriver};
use ns_core::log::Severity;
use ns_core::NsSet;
use ns_script::{command, wrong_args, Exception, Interp, ScriptResult};
use nsd::conn::{lock_set, set_ref};
use nsd::{Global, ModuleContext, NsError};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
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
    /// Maximum handles.
    pub nhandles: usize,
    /// Log every SQL statement.
    pub verbose: AtomicBool,
    /// Log SQL errors.
    pub log_errors: bool,
    state: Mutex<PoolState>,
    cond: Condvar,
}

struct PoolState {
    free: Vec<Box<dyn DbConnection>>,
    out: usize,
    epoch: u64,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// A database handle (`Ns_DbHandle`).
pub struct Handle {
    /// Owning pool.
    pub pool: Arc<Pool>,
    conn: Option<Box<dyn DbConnection>>,
    columns: Vec<String>,
    exception: (String, String),
    epoch: u64,
    verbose: bool,
}

impl Pool {
    /// Take `n` handles, waiting up to `timeout` (`Ns_DbPoolTimedGetMultipleHandles`).
    pub fn get_handles(
        self: &Arc<Self>,
        n: usize,
        timeout: Option<Duration>,
    ) -> Result<Vec<Handle>, DbError> {
        if n == 0 || n > self.nhandles {
            return Err(DbError::msg(format!(
                "invalid nhandles {n}: pool \"{}\" has {} handles",
                self.name, self.nhandles
            )));
        }
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut st = lock(&self.state);
        while st.out + n > self.nhandles {
            st = match deadline {
                None => self.cond.wait(st).unwrap_or_else(PoisonError::into_inner),
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return Err(DbError::msg(format!(
                            "wait for database handle timed out (pool {})",
                            self.name
                        )));
                    }
                    self.cond
                        .wait_timeout(st, d - now)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
            };
        }
        st.out += n;
        let epoch = st.epoch;
        let mut conns: Vec<Option<Box<dyn DbConnection>>> = (0..n).map(|_| st.free.pop()).collect();
        drop(st);
        let mut out = Vec::with_capacity(n);
        for c in &mut conns {
            let conn = match c.take() {
                Some(c) => c,
                None => match self.driver.open(
                    &self.datasource,
                    self.user.as_deref(),
                    self.password.as_deref(),
                ) {
                    Ok(c) => c,
                    Err(e) => {
                        let mut st = lock(&self.state);
                        st.out -= n;
                        drop(st);
                        self.cond.notify_all();
                        return Err(e);
                    }
                },
            };
            out.push(Handle {
                pool: Arc::clone(self),
                conn: Some(conn),
                columns: Vec::new(),
                exception: (String::new(), String::new()),
                epoch,
                verbose: self.verbose.load(Ordering::Relaxed),
            });
        }
        Ok(out)
    }

    /// Return a handle (`Ns_DbPoolPutHandle`).
    pub fn put_handle(&self, mut h: Handle) {
        let mut st = lock(&self.state);
        st.out = st.out.saturating_sub(1);
        if let Some(mut c) = h.conn.take() {
            c.flush();
            if h.epoch == st.epoch && c.connected() {
                st.free.push(c);
            }
        }
        drop(st);
        self.cond.notify_all();
    }

    /// Close all idle connections and mark in-use ones stale (`Ns_DbBouncePool`).
    pub fn bounce(&self) {
        let mut st = lock(&self.state);
        st.free.clear();
        st.epoch += 1;
    }

    /// Handles currently checked out.
    pub fn outstanding(&self) -> usize {
        lock(&self.state).out
    }
}

impl Handle {
    fn conn(&mut self) -> Result<&mut Box<dyn DbConnection>, DbError> {
        self.conn
            .as_mut()
            .ok_or_else(|| DbError::msg("handle not connected"))
    }

    /// Execute SQL, recording any exception (`Ns_DbExec`).
    pub fn exec(&mut self, sql: &str, log: Option<&Global>) -> Result<ExecResult, DbError> {
        if self.verbose {
            if let Some(g) = log {
                g.log(
                    Severity::Notice,
                    &format!("dbinit: sql({}): '{sql}'", self.pool.name),
                );
            }
        }
        let r = self.conn().and_then(|c| c.exec(sql));
        match &r {
            Ok(ExecResult::Rows(cols)) => {
                self.columns.clone_from(cols);
                self.exception = (String::new(), String::new());
            }
            Ok(ExecResult::Dml(_)) => {
                self.columns.clear();
                self.exception = (String::new(), String::new());
            }
            Err(e) => {
                self.exception = (e.code.clone(), e.message.clone());
                if self.pool.log_errors {
                    if let Some(g) = log {
                        g.log(
                            Severity::Error,
                            &format!(
                                "dbinit: error({}): '{}' for '{sql}'",
                                self.pool.name, e.message
                            ),
                        );
                    }
                }
            }
        }
        r
    }
}

/// All pools and per-server access rules.
#[derive(Default)]
pub struct DbState {
    pools: BTreeMap<String, Arc<Pool>>,
    access: HashMap<String, (Vec<String>, Option<String>)>,
    handles: Mutex<HashMap<String, Handle>>,
    next: AtomicU64,
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
    /// Read pools from config (`NsDbInitPools`).
    pub fn from_config(g: &Global) -> Self {
        let c = &g.config;
        let avail = drivers();
        let mut names: HashMap<String, Arc<dyn DbDriver>> = HashMap::new();
        if let Some(sec) = c.section("ns/db/drivers") {
            for (name, file) in sec.iter() {
                let key = nsd::module::module_key(name, file.unwrap_or(""));
                match avail.get(key.as_str()).or_else(|| avail.get(name)) {
                    Some(d) => {
                        names.insert(name.to_owned(), Arc::clone(d));
                    }
                    None => g.log(
                        Severity::Error,
                        &format!("dbdrv: no such driver: {name} ({})", file.unwrap_or("")),
                    ),
                }
            }
        }
        let mut pools = BTreeMap::new();
        if let Some(sec) = c.section("ns/db/pools") {
            for (name, desc) in sec.iter() {
                let path = format!("ns/db/pool/{name}");
                let Some(dname) = c.get(&path, "driver") else {
                    g.log(
                        Severity::Error,
                        &format!("dbinit: no driver for pool '{name}'"),
                    );
                    continue;
                };
                let Some(drv) = names.get(dname).or_else(|| avail.get(dname)).cloned() else {
                    g.log(
                        Severity::Error,
                        &format!("dbinit: no such driver '{dname}' for pool '{name}'"),
                    );
                    continue;
                };
                let Some(ds) = c.get(&path, "datasource") else {
                    g.log(
                        Severity::Error,
                        &format!("dbinit: missing datasource for pool '{name}'"),
                    );
                    continue;
                };
                pools.insert(
                    name.to_owned(),
                    Arc::new(Pool {
                        name: name.to_owned(),
                        desc: desc.unwrap_or("").to_owned(),
                        driver: drv,
                        datasource: ds.to_owned(),
                        user: c.get(&path, "user").map(str::to_owned),
                        password: c.get(&path, "password").map(str::to_owned),
                        nhandles: c.int_or(&path, "connections", 2).max(1) as usize,
                        verbose: AtomicBool::new(c.bool_or(&path, "verbose", false)),
                        log_errors: c.bool_or(&path, "logsqlerrors", false),
                        state: Mutex::new(PoolState {
                            free: Vec::new(),
                            out: 0,
                            epoch: 0,
                        }),
                        cond: Condvar::new(),
                    }),
                );
            }
        }
        Self {
            pools,
            access: HashMap::new(),
            handles: Mutex::new(HashMap::new()),
            next: AtomicU64::new(0),
        }
    }

    /// Configure which pools `server` may use (`NsDbInitServer`).
    pub fn configure_server(&mut self, g: &Global, server: &str) {
        let path = format!("ns/server/{server}/db");
        let c = &g.config;
        let allowed: Vec<String> = match c.get(&path, "pools") {
            Some("*") => self.pools.keys().cloned().collect(),
            Some(list) => list
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|p| self.pools.contains_key(p))
                .collect(),
            None => Vec::new(),
        };
        let def = c
            .get(&path, "defaultpool")
            .map(str::to_owned)
            .filter(|d| allowed.contains(d));
        self.access.insert(server.to_owned(), (allowed, def));
    }

    /// Pools `server` may use (`Ns_DbPoolList`).
    pub fn server_pools(&self, server: &str) -> Vec<String> {
        self.access
            .get(server)
            .map(|a| a.0.clone())
            .unwrap_or_default()
    }

    /// A pool by name, if `server` may use it.
    pub fn pool(&self, server: &str, name: &str) -> Option<Arc<Pool>> {
        self.server_pools(server)
            .iter()
            .any(|p| p == name)
            .then(|| self.pools.get(name).cloned())
            .flatten()
    }

    /// The server's default pool.
    pub fn default_pool(&self, server: &str) -> Option<String> {
        self.access.get(server).and_then(|a| a.1.clone())
    }

    fn enter(&self, h: Handle) -> String {
        let id = format!("nsdb{}", self.next.fetch_add(1, Ordering::Relaxed));
        lock(&self.handles).insert(id.clone(), h);
        id
    }

    fn release(&self, id: &str) -> bool {
        let h = lock(&self.handles).remove(id);
        h.is_some_and(|h| {
            let p = Arc::clone(&h.pool);
            p.put_handle(h);
            true
        })
    }

    fn with<R>(&self, id: &str, f: impl FnOnce(&mut Handle) -> R) -> Result<R, Exception> {
        let mut m = lock(&self.handles);
        let h = m
            .get_mut(id)
            .ok_or_else(|| Exception::error(format!("invalid database id:  \"{id}\"")))?;
        Ok(f(h))
    }
}

/// State per process (pools are global in AOLserver).
fn state_for(g: &Arc<Global>) -> Arc<Mutex<DbState>> {
    static STATES: OnceLock<Mutex<Vec<(usize, Arc<Mutex<DbState>>)>>> = OnceLock::new();
    let key = Arc::as_ptr(g) as usize;
    let mut v = lock(STATES.get_or_init(|| Mutex::new(Vec::new())));
    if let Some((_, s)) = v.iter().find(|(k, _)| *k == key) {
        return Arc::clone(s);
    }
    let s = Arc::new(Mutex::new(DbState::from_config(g)));
    v.push((key, Arc::clone(&s)));
    s
}

fn op_failed(op: &str, e: &DbError) -> Exception {
    let mut m = format!("Database operation \"{op}\" failed");
    if !e.code.is_empty() {
        m.push_str(&format!(" (exception {}", e.code));
        if !e.message.is_empty() {
            m.push_str(&format!(", \"{}\"", e.message));
        }
        m.push(')');
    } else if !e.message.is_empty() {
        m.push_str(&format!(" (\"{}\")", e.message));
    }
    Exception::error(m).error_code(format!("NSDB {}", e.code))
}

fn columns_set(cols: &[String]) -> NsSet {
    let mut s = NsSet::default();
    for c in cols {
        s.put(c, Some(""));
    }
    s
}

#[allow(clippy::too_many_lines)]
fn ns_db(
    interp: &dyn Interp,
    argv: &[String],
    db: &Arc<Mutex<DbState>>,
    g: &Arc<Global>,
) -> ScriptResult {
    let opts = [
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
    let (_, opt) = nsd::cmds::subcommand(argv, &opts)?;
    let server = nsd::cmds::server(interp)?.name.clone();
    let st = lock(db);
    match opt {
        "pools" => Ok(interp.list_from(&st.server_pools(&server))),
        "bouncepool" => {
            let name = argv.get(2).ok_or_else(|| wrong_args(argv, 2, "pool"))?;
            let p = st
                .pool(&server, name)
                .ok_or_else(|| Exception::error(format!("could not bounce: {name}")))?;
            p.bounce();
            Ok(String::new())
        }
        "gethandle" => {
            let mut i = 2;
            let mut timeout = None;
            if argv.get(i).is_some_and(|a| a == "-timeout") {
                timeout = Some(nsd::cmds::time_arg(
                    argv.get(i + 1).map_or("", String::as_str),
                )?);
                i += 2;
            }
            let pool = match argv.get(i) {
                Some(p) => p.clone(),
                None => st
                    .default_pool(&server)
                    .ok_or_else(|| Exception::error("no defaultpool configured"))?,
            };
            let n = argv.get(i + 1).map_or(Ok(1), |n| nsd::cmds::int_arg(n))?;
            let p = st
                .pool(&server, &pool)
                .ok_or_else(|| Exception::error(format!("no access to pool: \"{pool}\"")))?;
            drop(st);
            let handles = p
                .get_handles(usize::try_from(n).unwrap_or(0), timeout)
                .map_err(|e| {
                    if e.message.contains("timed out") {
                        Exception::error(String::new())
                    } else {
                        Exception::error(format!(
                            "could not allocate {n} handle(s) from pool \"{pool}\": {}",
                            e.message
                        ))
                    }
                });
            let handles = match handles {
                Ok(h) => h,
                Err(e) if e.message.is_empty() => return Ok(String::new()),
                Err(e) => return Err(e),
            };
            let st = lock(db);
            let ids: Vec<String> = handles.into_iter().map(|h| st.enter(h)).collect();
            drop(st);
            if let Ok(nsi) = nsd::interp::ns_interp(interp) {
                for id in &ids {
                    let id = id.clone();
                    let db = Arc::clone(db);
                    nsi.borrow_mut().defer.push(Box::new(move |_| {
                        lock(&db).release(&id);
                    }));
                }
            }
            Ok(interp.list_from(&ids))
        }
        _ => {
            let id = argv
                .get(2)
                .ok_or_else(|| wrong_args(argv, 2, "dbId ?args?"))?
                .clone();
            let sql = argv.get(3);
            let need_sql = || sql.ok_or_else(|| wrong_args(argv, 2, "dbId sql"));
            match opt {
                "releasehandle" => {
                    st.release(&id);
                    Ok(String::new())
                }
                "select" => {
                    let sql = need_sql()?;
                    let r = st
                        .with(&id, |h| h.exec(sql, Some(g)))?
                        .map_err(|e| op_failed(opt, &e))?;
                    let ExecResult::Rows(cols) = r else {
                        return Err(Exception::error(
                            "Query was not a statement returning rows.",
                        ));
                    };
                    drop(st);
                    nsd::cmds::set::enter_set(interp, set_ref(columns_set(&cols)), true, false)
                }
                "dml" => {
                    let sql = need_sql()?;
                    let r = st
                        .with(&id, |h| h.exec(sql, Some(g)))?
                        .map_err(|e| op_failed(opt, &e))?;
                    match r {
                        ExecResult::Dml(_) => Ok(String::new()),
                        ExecResult::Rows(_) => {
                            st.with(&id, |h| h.conn().map(|c| c.flush()))?.ok();
                            Err(Exception::error("Query was not a DML or DDL command."))
                        }
                    }
                }
                "exec" => {
                    let sql = need_sql()?;
                    let r = st
                        .with(&id, |h| h.exec(sql, Some(g)))?
                        .map_err(|e| op_failed(opt, &e))?;
                    Ok(match r {
                        ExecResult::Dml(_) => "NS_DML",
                        ExecResult::Rows(_) => "NS_ROWS",
                    }
                    .to_owned())
                }
                "1row" | "0or1row" => {
                    let sql = need_sql()?;
                    let (cols, first, extra) = st.with(&id, |h| -> Result<_, Exception> {
                        let r = h.exec(sql, Some(g)).map_err(|e| op_failed(opt, &e))?;
                        let ExecResult::Rows(cols) = r else {
                            return Err(Exception::error(
                                "Query was not a statement returning rows.",
                            ));
                        };
                        let c = h.conn().map_err(|e| op_failed(opt, &e))?;
                        let first = c.next_row().map_err(|e| op_failed(opt, &e))?;
                        let extra = c.next_row().map_err(|e| op_failed(opt, &e))?.is_some();
                        c.flush();
                        Ok((cols, first, extra))
                    })??;
                    drop(st);
                    if extra {
                        return Err(Exception::error("Query returned more than one row."));
                    }
                    match first {
                        None if opt == "1row" => {
                            Err(Exception::error("Query did not return a row."))
                        }
                        None => Ok(String::new()),
                        Some(row) => {
                            let mut s = NsSet::default();
                            for (c, v) in cols.iter().zip(row) {
                                s.put(c, Some(v.as_deref().unwrap_or("")));
                            }
                            nsd::cmds::set::enter_set(interp, set_ref(s), true, false)
                        }
                    }
                }
                "getrow" => {
                    let setid = argv
                        .get(3)
                        .ok_or_else(|| wrong_args(argv, 2, "dbId setId"))?;
                    let row = st
                        .with(&id, |h| h.conn().and_then(|c| c.next_row()))?
                        .map_err(|e| op_failed(opt, &e))?;
                    drop(st);
                    let set = nsd::cmds::set::lookup_set(interp, setid, false)?;
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
                "bindrow" => {
                    let cols = st.with(&id, |h| h.columns.clone())?;
                    drop(st);
                    nsd::cmds::set::enter_set(interp, set_ref(columns_set(&cols)), true, false)
                }
                "flush" | "cancel" => {
                    st.with(&id, |h| h.conn().map(|c| c.flush()))?
                        .map_err(|e| op_failed(opt, &e))?;
                    Ok(String::new())
                }
                "resethandle" => {
                    st.with(&id, |h| h.conn().and_then(|c| c.reset()))?
                        .map_err(|e| op_failed(opt, &e))?;
                    Ok("NS_OK".into())
                }
                "connected" => {
                    Ok(nsd::cmds::bool_str(st.with(&id, |h| {
                        h.conn.as_ref().is_some_and(|c| c.connected())
                    })?))
                }
                "disconnect" => {
                    st.with(&id, |h| h.conn = None)?;
                    Ok(String::new())
                }
                "exception" => {
                    let (c, m) = st.with(&id, |h| h.exception.clone())?;
                    Ok(interp.list_from(&<[String; 2]>::from((c, m))))
                }
                "setexception" => {
                    let (code, msg) = (
                        argv.get(3)
                            .ok_or_else(|| wrong_args(argv, 2, "dbId code message"))?,
                        argv.get(4)
                            .ok_or_else(|| wrong_args(argv, 2, "dbId code message"))?,
                    );
                    if code.len() > 5 {
                        return Err(Exception::error(format!(
                            "code \"{code}\" more than 5 characters"
                        )));
                    }
                    st.with(&id, |h| h.exception = (code.clone(), msg.clone()))?;
                    Ok(String::new())
                }
                "datasource" => st.with(&id, |h| h.pool.datasource.clone()),
                "dbtype" => st.with(&id, |h| h.pool.driver.db_type().to_owned()),
                "driver" => st.with(&id, |h| h.pool.driver.name().to_owned()),
                "password" => st.with(&id, |h| h.pool.password.clone().unwrap_or_default()),
                "user" => st.with(&id, |h| h.pool.user.clone().unwrap_or_default()),
                "poolname" => st.with(&id, |h| h.pool.name.clone()),
                "verbose" => st.with(&id, |h| {
                    if let Some(v) = argv.get(3) {
                        h.verbose = ns_script::parse_bool(v).unwrap_or(h.verbose);
                    }
                    nsd::cmds::bool_str(h.verbose)
                }),
                "interpretsqlfile" => {
                    let file = need_sql()?;
                    let text = std::fs::read_to_string(file)
                        .map_err(|e| Exception::error(format!("could not read {file}: {e}")))?;
                    for stmt in split_sql(&text) {
                        st.with(&id, |h| h.exec(&stmt, Some(g)))?
                            .map_err(|e| op_failed(opt, &e))?;
                    }
                    Ok(String::new())
                }
                _ => Err(Exception::error(format!(
                    "ns_db {opt}: stored procedures are not supported by the {} driver",
                    st.with(&id, |h| h.pool.driver.name().to_owned())
                        .unwrap_or_default()
                ))),
            }
        }
    }
}

/// Split a SQL file on `;` outside of quotes (`Ns_DbInterpretSqlFile`).
pub fn split_sql(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for line in text.lines() {
        if quote.is_none() && line.trim_start().starts_with("--") {
            continue;
        }
        for c in line.chars() {
            match (quote, c) {
                (None, '\'' | '"') => {
                    quote = Some(c);
                    cur.push(c);
                }
                (Some(q), c) if c == q => {
                    quote = None;
                    cur.push(c);
                }
                (None, ';') => {
                    if !cur.trim().is_empty() {
                        out.push(cur.trim().to_owned());
                    }
                    cur.clear();
                }
                _ => cur.push(c),
            }
        }
        cur.push('\n');
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_owned());
    }
    out
}

/// Convert an SQL-quoted list (`'a','b'`) to a Tcl list (`ns_quotelisttolist`).
pub fn quote_list_to_list(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\'' {
            continue;
        }
        let mut item = String::new();
        while let Some(c) = chars.next() {
            if c == '\'' {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                    item.push('\'');
                } else {
                    break;
                }
            } else if c == '\\' {
                if let Some(n) = chars.next() {
                    item.push(n);
                }
            } else {
                item.push(c);
            }
        }
        out.push(item);
    }
    out
}

/// Parse one CSV line (`ns_getcsv`).
pub fn parse_csv(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut inq = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if inq && chars.peek() == Some(&'"') => {
                chars.next();
                cur.push('"');
            }
            '"' => inq = !inq,
            ',' if !inq => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// Module init: global pools on first use, per-server access and commands.
pub fn init(ctx: &ModuleContext<'_>) -> Result<(), NsError> {
    let server = ctx
        .server
        .ok_or_else(|| NsError::Config("nsdb must be loaded as a per-server module".into()))?;
    let db = state_for(ctx.global);
    lock(&db).configure_server(ctx.global, &server.name);
    let pools = lock(&db).server_pools(&server.name);
    for p in pools {
        ctx.global.log(
            Severity::Notice,
            &format!("dbinit: pool '{p}' available to server {}", server.name),
        );
    }
    let (d, g) = (Arc::clone(&db), Arc::clone(ctx.global));
    server.add_command("ns_db", command(move |i, a| ns_db(i, a, &d, &g)));
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
    let d = Arc::clone(&db);
    server.add_command(
        "ns_dberrorcode",
        command(move |_, a| {
            nsd::cmds::check_args(a, 2, 2, 1, "dbId")?;
            lock(&d).with(&a[1], |h| h.exception.0.clone())
        }),
    );
    let d = Arc::clone(&db);
    server.add_command(
        "ns_dberrormsg",
        command(move |_, a| {
            nsd::cmds::check_args(a, 2, 2, 1, "dbId")?;
            lock(&d).with(&a[1], |h| h.exception.1.clone())
        }),
    );
    let name = server.name.clone();
    server.add_command(
        "ns_dbconfigpath",
        command(move |_, _| Ok(format!("ns/server/{name}/db"))),
    );
    let d = Arc::clone(&db);
    let name = server.name.clone();
    server.add_command(
        "ns_pooldescription",
        command(move |_, a| {
            nsd::cmds::check_args(a, 2, 2, 1, "poolname")?;
            Ok(lock(&d)
                .pool(&name, &a[1])
                .map(|p| p.desc.clone())
                .unwrap_or_default())
        }),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(
            split_sql("create table t (a);\n-- c\ninsert into t values ('x;y');"),
            vec!["create table t (a)", "insert into t values ('x;y')"]
        );
        assert_eq!(
            quote_list_to_list("'a','it''s', 'b\\'c'"),
            vec!["a", "it's", "b'c"]
        );
        assert_eq!(
            parse_csv("a,\"b,c\",\"d\"\"e\","),
            vec!["a", "b,c", "d\"e", ""]
        );
        assert_eq!(columns_set(&["x".into()]).get("x"), Some(""));
        let e = op_failed(
            "select",
            &DbError {
                code: "E1".into(),
                message: "bad".into(),
            },
        );
        assert_eq!(
            e.message,
            "Database operation \"select\" failed (exception E1, \"bad\")"
        );
        assert!(drivers().contains_key("sqlite"));
    }

    #[test]
    fn pool_handles() {
        let p = Arc::new(Pool {
            name: "p".into(),
            desc: String::new(),
            driver: Arc::new(SqliteDriver),
            datasource: ":memory:".into(),
            user: None,
            password: None,
            nhandles: 1,
            verbose: AtomicBool::new(false),
            log_errors: false,
            state: Mutex::new(PoolState {
                free: Vec::new(),
                out: 0,
                epoch: 0,
            }),
            cond: Condvar::new(),
        });
        let mut h = p.get_handles(1, None).expect("handle").pop().expect("one");
        assert_eq!(p.outstanding(), 1);
        assert!(p.get_handles(1, Some(Duration::from_millis(10))).is_err());
        assert!(p.get_handles(2, None).is_err());
        assert_eq!(
            h.exec("select 1 as one", None).expect("exec"),
            ExecResult::Rows(vec!["one".into()])
        );
        assert!(h.exec("bogus sql", None).is_err());
        assert_ne!(h.exception.1, "");
        p.put_handle(h);
        assert_eq!(p.outstanding(), 0);
        p.bounce();
        let h = p.get_handles(1, None).expect("again").pop().expect("one");
        p.put_handle(h);
    }
}
