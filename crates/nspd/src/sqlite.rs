//! A [`PdDb`] over any `nsdb` driver, so a daemon can serve, for example,
//! the bundled SQLite driver to nsext.

use std::sync::Arc;

use nsdb::{DbConnection, DbDriver, DbError, ExecResult, SqliteDriver};

use crate::{OpenArgs, PdDb, PdError, PdExec};

/// An `nsdb` driver served through the external-driver protocol.
pub struct DriverDb {
    driver: Arc<dyn DbDriver>,
    version: String,
    conn: Option<Box<dyn DbConnection>>,
    columns: Vec<String>,
    rows: bool,
    affected: usize,
    max_rows: usize,
    fetched: usize,
}

impl std::fmt::Debug for DriverDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriverDb")
            .field("driver", &self.driver.name())
            .field("open", &self.conn.is_some())
            .finish_non_exhaustive()
    }
}

/// The exception for a driver error (code `NSDB` when it has none).
fn exc(e: DbError) -> PdError {
    let code = if e.code.is_empty() {
        "NSDB".to_owned()
    } else {
        e.code
    };
    PdError::exception(code, e.message)
}

fn not_open() -> PdError {
    PdError::exception("NSDB", "database is not open")
}

impl DriverDb {
    /// Serve `driver`; `identify` answers `<dbtype> Proxy Daemon v<version>`.
    pub fn new(driver: Arc<dyn DbDriver>, version: &str) -> Self {
        Self {
            driver,
            version: version.to_owned(),
            conn: None,
            columns: Vec::new(),
            rows: false,
            affected: 0,
            max_rows: 0,
            fetched: 0,
        }
    }

    /// The bundled SQLite driver; the datasource is a database file.
    pub fn sqlite() -> Self {
        Self::new(Arc::new(SqliteDriver), "4.5")
    }

    fn conn(&mut self) -> Result<&mut Box<dyn DbConnection>, PdError> {
        self.conn.as_mut().ok_or_else(not_open)
    }
}

impl PdDb for DriverDb {
    fn open(&mut self, args: &OpenArgs) -> Result<(), PdError> {
        let ds = args.datasource.as_deref().unwrap_or_default();
        let c = self
            .driver
            .open(ds, args.user.as_deref(), args.password.as_deref())
            .map_err(exc)?;
        self.conn = Some(c);
        Ok(())
    }

    fn close(&mut self) -> Result<(), PdError> {
        self.conn = None;
        Ok(())
    }

    fn identify(&mut self) -> String {
        format!("{} Proxy Daemon v{}", self.driver.db_type(), self.version)
    }

    fn get_types(&mut self) -> String {
        "integer real text blob null".to_owned()
    }

    fn exec(&mut self, sql: &str) -> Result<PdExec, PdError> {
        self.rows = false;
        self.columns.clear();
        self.fetched = 0;
        match self.conn()?.exec(sql).map_err(exc)? {
            ExecResult::Rows(cols) => {
                self.columns = cols;
                self.rows = true;
                Ok(PdExec::Rows)
            }
            ExecResult::Dml(n) => {
                self.affected = n;
                Ok(PdExec::Dml)
            }
        }
    }

    fn bind_row(&mut self) -> Result<Vec<String>, PdError> {
        if !self.rows {
            return Err(PdError::exception("NSDB", "no rows pending"));
        }
        Ok(self.columns.clone())
    }

    fn get_row(&mut self, _ncols: i64) -> Result<Option<Vec<Option<String>>>, PdError> {
        if !self.rows {
            return Ok(None);
        }
        if self.max_rows > 0 && self.fetched >= self.max_rows {
            self.flush()?;
            return Ok(None);
        }
        let row = self.conn()?.next_row().map_err(exc)?;
        let Some(r) = row else {
            self.rows = false;
            return Ok(None);
        };
        self.fetched += 1;
        Ok(Some(r))
    }

    fn flush(&mut self) -> Result<(), PdError> {
        self.rows = false;
        if let Some(c) = self.conn.as_mut() {
            c.flush();
        }
        Ok(())
    }

    fn result_rows(&mut self) -> String {
        self.affected.to_string()
    }

    fn set_max_rows(&mut self, max: &str) -> Result<(), PdError> {
        self.max_rows = usize::try_from(crate::msg::atoi(max)).unwrap_or(0);
        Ok(())
    }

    fn reset_handle(&mut self) -> Result<(), PdError> {
        self.rows = false;
        match self.conn.as_mut() {
            Some(c) => c.reset().map_err(exc),
            None => Ok(()),
        }
    }

    fn cleanup(&mut self) {
        self.conn = None;
    }
}
