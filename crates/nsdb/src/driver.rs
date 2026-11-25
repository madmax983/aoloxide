//! The database driver interface (`include/nsdb.h`, `Ns_DbDriver*`) and the
//! built-in SQLite driver.

use std::collections::VecDeque;

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

/// An open database connection.
pub trait DbConnection: Send {
    /// Execute `sql` (`Ns_DbExec`).
    fn exec(&mut self, sql: &str) -> Result<ExecResult, DbError>;
    /// Next row of the last query (`Ns_DbGetRow`); `None` at the end.
    /// NULL columns are `None`.
    fn next_row(&mut self) -> Result<Option<Vec<Option<String>>>, DbError>;
    /// Discard any pending rows (`Ns_DbFlush`).
    fn flush(&mut self);
    /// Reset the connection after an error (`Ns_DbResetHandle`).
    fn reset(&mut self) -> Result<(), DbError> {
        self.flush();
        Ok(())
    }
    /// Whether the connection is usable.
    fn connected(&self) -> bool {
        true
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
}

/// The SQLite driver (`datasource` is a file path or `:memory:`).
#[derive(Debug, Default, Clone, Copy)]
pub struct SqliteDriver;

struct SqliteConn {
    conn: rusqlite::Connection,
    rows: VecDeque<Vec<Option<String>>>,
}

fn sql_err(e: &rusqlite::Error) -> DbError {
    let code = match e {
        rusqlite::Error::SqliteFailure(f, _) => format!("{:?}", f.code),
        _ => String::new(),
    };
    DbError {
        code,
        message: e.to_string(),
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
        assert_eq!(DbError::msg("m").to_string(), "m");
    }
}
