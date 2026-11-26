//! The database driver interface (`include/nsdb.h`, `Ns_DbDriver*`) and the
//! built-in SQLite driver.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

/// A database error with an optional native code (`ns_dberrorcode`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct DbError {
    /// Driver-specific code (SQLSTATE-like), empty if none.
    pub code: String,
    /// Message.
    pub message: String,
}

impl DbError {
    /// An error without a code.
    pub fn msg(message: impl Into<String>) -> Self {
        Self {
            code: String::new(),
            message: message.into(),
        }
    }
}

/// Result of executing SQL (`NS_DML` / `NS_ROWS`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecResult {
    /// A statement that returns no rows; carries the affected row count.
    Dml(usize),
    /// A query; carries the column names.  Rows are fetched with
    /// [`DbConnection::next_row`].
    Rows(Vec<String>),
}

/// The error of a stored-procedure call on a driver without them.
fn no_sp() -> DbError {
    DbError::msg("stored procedures are not supported")
}

/// An open database connection.
///
/// Only `exec`, `next_row` and `flush` are required; the other methods
/// have defaults that suit drivers which buffer a query's rows when it
/// runs (the SQLite driver).  Drivers that talk to a server on each call
/// (`nsext`) override them.
pub trait DbConnection: Send {
    /// Execute `sql` (`Ns_DbExec`).
    fn exec(&mut self, sql: &str) -> Result<ExecResult, DbError>;
    /// Next row of the last query (`Ns_DbGetRow`); `None` at the end.
    /// NULL columns are `None`.
    fn next_row(&mut self) -> Result<Option<Vec<Option<String>>>, DbError>;
    /// Next row for a row set of `ncols` columns: `Ns_DbGetRow` passes
    /// the caller's set, whose size some drivers send on.
    fn next_row_n(&mut self, ncols: usize) -> Result<Option<Vec<Option<String>>>, DbError> {
        let _ = ncols;
        self.next_row()
    }
    /// The columns of the pending query (`Ns_DbBindRow`).  `None` means
    /// the columns [`DbConnection::exec`] returned apply.
    fn bind_row(&mut self) -> Option<Result<Vec<String>, DbError>> {
        None
    }
    /// Discard any pending rows (`Ns_DbFlush`).
    fn flush(&mut self);
    /// Discard pending rows, reporting failure (`Ns_DbFlush`'s status);
    /// by default [`DbConnection::flush`], which cannot fail.
    fn try_flush(&mut self) -> Result<(), DbError> {
        self.flush();
        Ok(())
    }
    /// Cancel the pending query (`Ns_DbCancel`); by default a flush.
    fn cancel(&mut self) -> Result<(), DbError> {
        self.try_flush()
    }
    /// Reset the connection after an error (`Ns_DbResetHandle`).
    fn reset(&mut self) -> Result<(), DbError> {
        self.flush();
        Ok(())
    }
    /// Whether the connection is usable.  A handle drops a connection
    /// that reports `false` and is then disconnected.
    fn connected(&self) -> bool {
        true
    }
    /// This connection's database type, when it is not the driver's
    /// [`DbDriver::db_type`] (`Ns_DbDriverDbType`).
    fn db_type(&self) -> Option<String> {
        None
    }
    /// The connection as `Any`, for driver-specific commands (`ns_ext`).
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        None
    }
    /// Whether the driver implements stored procedures (`ns_db sp_*`).
    fn supports_sp(&self) -> bool {
        false
    }
    /// Begin a stored procedure (`Ns_DbSpStart`).
    fn sp_start(&mut self, procname: &str) -> Result<(), DbError> {
        let _ = procname;
        Err(no_sp())
    }
    /// Set a parameter; `args` is `name type in|out value`
    /// (`Ns_DbSpSetParam`).
    fn sp_setparam(&mut self, args: &str) -> Result<(), DbError> {
        let _ = args;
        Err(no_sp())
    }
    /// Run the procedure (`Ns_DbSpExec`).  The columns of a row result
    /// come from [`DbConnection::bind_row`].
    fn sp_exec(&mut self) -> Result<ExecResult, DbError> {
        Err(no_sp())
    }
    /// The procedure's return code (`Ns_DbSpReturnCode`).
    fn sp_returncode(&mut self) -> Result<String, DbError> {
        Err(no_sp())
    }
    /// The output parameters as `(name, value)` pairs (`Ns_DbSpGetParams`).
    fn sp_getparams(&mut self) -> Result<Vec<(String, Option<String>)>, DbError> {
        Err(no_sp())
    }
}

/// A database driver (`Ns_DbDriverInit`).
pub trait DbDriver: Send + Sync {
    /// Driver name (e.g. `sqlite`).
    fn name(&self) -> &str;
    /// Database type reported by `ns_db dbtype`.
    fn db_type(&self) -> &str;
    /// Open a connection.
    fn open(
        &self,
        datasource: &str,
        user: Option<&str>,
        password: Option<&str>,
    ) -> Result<Box<dyn DbConnection>, DbError>;
    /// Per-server set-up, run for each pool of the driver a server may
    /// use (`DbFn_ServerInit` via `NsDbDriverInit`), for example to add
    /// driver-specific commands.
    fn server_init(&self, server: &Arc<nsd::Server>) -> Result<(), String> {
        let _ = server;
        Ok(())
    }
}

/// What a [`DriverFactory`] is given (`Ns_DbDriverInit(hDriver,
/// configPath)`).
pub struct DriverConfig<'a> {
    /// The driver's name in `ns/db/drivers` (`hDriver`).
    pub name: &'a str,
    /// Its config section, `ns/db/driver/<name>`.
    pub path: String,
    /// The process config.
    pub config: &'a ns_core::config::Config,
    /// The server home, for relative paths (`Ns_HomePath`), when known.
    pub home: Option<&'a Path>,
    /// Where the driver logs.
    pub log: &'a crate::Logger,
}

/// Creates a driver from its config (`Ns_DbDriverInit`).  `Err(Some(d))`
/// means the driver registered itself but its init failed, which
/// AOLserver still uses for pools; `Err(None)` means nothing registered.
pub type DriverFactory = Arc<
    dyn Fn(&DriverConfig<'_>) -> Result<Arc<dyn DbDriver>, Option<Arc<dyn DbDriver>>> + Send + Sync,
>;

type Factories = Mutex<BTreeMap<String, DriverFactory>>;

fn factories() -> &'static Factories {
    static F: OnceLock<Factories> = OnceLock::new();
    F.get_or_init(Factories::default)
}

/// Make a compiled-in driver module loadable from `ns/db/drivers` under
/// `key`, its module name (`ns_param external nsext.so` loads `nsext`).
pub fn register_driver(key: &str, factory: DriverFactory) {
    factories()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key.to_owned(), factory);
}

/// The factory registered under `key`.
pub fn driver_factory(key: &str) -> Option<DriverFactory> {
    factories()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(key)
        .cloned()
}

/// The SQLite driver (`datasource` is a file path or `:memory:`).
#[derive(Debug, Default, Clone, Copy)]
pub struct SqliteDriver;

struct SqliteConn {
    conn: rusqlite::Connection,
    rows: VecDeque<Vec<Option<String>>>,
}

/// A driver error.  Errors without a SQLite code use `NSDB`, as
/// AOLserver's drivers do, so `ns_db` reports their message.
fn sql_err(e: &rusqlite::Error) -> DbError {
    let code = match e {
        rusqlite::Error::SqliteFailure(f, _) => format!("{:?}", f.code),
        _ => "NSDB".to_owned(),
    };
    DbError {
        code,
        message: e.to_string(),
    }
}

/// Whether `sql` holds only whitespace, comments and `;`.
fn is_blank_sql(sql: &str) -> bool {
    let mut s = sql;
    loop {
        s = s.trim_start_matches(|c: char| c.is_whitespace() || c == ';');
        if let Some(r) = s.strip_prefix("--") {
            s = r.find('\n').map_or("", |i| &r[i..]);
        } else if let Some(r) = s.strip_prefix("/*") {
            s = r.find("*/").map_or("", |i| &r[i + 2..]);
        } else {
            return s.is_empty();
        }
    }
}

fn value_text(v: rusqlite::types::ValueRef<'_>) -> Option<String> {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => None,
        ValueRef::Integer(i) => Some(i.to_string()),
        ValueRef::Real(f) => Some(f.to_string()),
        ValueRef::Text(t) | ValueRef::Blob(t) => Some(String::from_utf8_lossy(t).into_owned()),
    }
}

impl DbConnection for SqliteConn {
    fn exec(&mut self, sql: &str) -> Result<ExecResult, DbError> {
        self.rows.clear();
        if is_blank_sql(sql) {
            // SQLite compiles no statement from these; treat them as an
            // empty DML command.
            return Ok(ExecResult::Dml(0));
        }
        let mut stmt = self.conn.prepare(sql).map_err(|e| sql_err(&e))?;
        let ncols = stmt.column_count();
        if ncols == 0 {
            let n = stmt.execute([]).map_err(|e| sql_err(&e))?;
            return Ok(ExecResult::Dml(n));
        }
        let cols: Vec<String> = stmt
            .column_names()
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let mut rows = stmt.query([]).map_err(|e| sql_err(&e))?;
        let mut buf = VecDeque::new();
        while let Some(r) = rows.next().map_err(|e| sql_err(&e))? {
            let mut row = Vec::with_capacity(ncols);
            for i in 0..ncols {
                row.push(value_text(r.get_ref(i).map_err(|e| sql_err(&e))?));
            }
            buf.push_back(row);
        }
        self.rows = buf;
        Ok(ExecResult::Rows(cols))
    }

    fn next_row(&mut self) -> Result<Option<Vec<Option<String>>>, DbError> {
        Ok(self.rows.pop_front())
    }

    fn flush(&mut self) {
        self.rows.clear();
    }
}

impl DbDriver for SqliteDriver {
    fn name(&self) -> &'static str {
        "sqlite"
    }

    fn db_type(&self) -> &'static str {
        "SQLite"
    }

    fn open(
        &self,
        datasource: &str,
        _user: Option<&str>,
        _password: Option<&str>,
    ) -> Result<Box<dyn DbConnection>, DbError> {
        let conn = rusqlite::Connection::open(datasource).map_err(|e| sql_err(&e))?;
        let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
        Ok(Box::new(SqliteConn {
            conn,
            rows: VecDeque::new(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_roundtrip() {
        let d = SqliteDriver;
        assert_eq!((d.name(), d.db_type()), ("sqlite", "SQLite"));
        let mut c = d.open(":memory:", None, None).expect("open");
        assert_eq!(
            c.exec("create table t (a integer, b text, c real)")
                .expect("ddl"),
            ExecResult::Dml(0)
        );
        assert_eq!(
            c.exec("insert into t values (1, 'x', 1.5), (2, null, 2.0)")
                .expect("ins"),
            ExecResult::Dml(2)
        );
        assert_eq!(
            c.exec("select a, b, c from t order by a").expect("sel"),
            ExecResult::Rows(vec!["a".into(), "b".into(), "c".into()])
        );
        assert_eq!(
            c.next_row().expect("row"),
            Some(vec![Some("1".into()), Some("x".into()), Some("1.5".into())])
        );
        assert_eq!(
            c.next_row().expect("row"),
            Some(vec![Some("2".into()), None, Some("2".into())])
        );
        assert_eq!(c.next_row().expect("row"), None);
        c.exec("select 1").expect("sel");
        c.flush();
        assert_eq!(c.next_row().expect("row"), None);
        let e = c.exec("select * from nosuch").expect_err("err");
        assert!(e.message.contains("no such table"));
        assert!(c.reset().is_ok() && c.connected());
        // `ns_db interpretsqlfile` passes empty statements through.
        assert_eq!(c.exec("").expect("empty"), ExecResult::Dml(0));
        assert_eq!(c.exec(" \n ").expect("blank"), ExecResult::Dml(0));
        assert_eq!(
            c.exec("-- x\n/* y */ ;").expect("comments"),
            ExecResult::Dml(0)
        );
        assert!(!is_blank_sql("/* a */ select 1") && !is_blank_sql("-- a\nselect 1"));
        assert!(is_blank_sql("/* open") && is_blank_sql("-- open"));
        assert_eq!(
            c.exec("select * from nosuch").expect_err("err").code,
            "Unknown"
        );
        assert_eq!(DbError::msg("m").to_string(), "m");
    }
}
