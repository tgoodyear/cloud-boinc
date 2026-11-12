//! [`cb_db::Db`] on an in-process SQLite database, for native tests and tools.

use std::cell::{Cell, RefCell};

use async_trait::async_trait;
use cb_db::{normalize_number, Db, DbError, DbResult, Row, Rows, Stmt, Value};
use rusqlite::types::ValueRef;
use rusqlite::Connection;

pub struct SqliteDb {
    conn: RefCell<Connection>,
    written: Cell<u64>,
    statements: Cell<u64>,
}

fn err(e: rusqlite::Error) -> DbError {
    DbError::Backend(e.to_string())
}

impl SqliteDb {
    pub fn open_in_memory() -> DbResult<Self> {
        let conn = Connection::open_in_memory().map_err(err)?;
        Ok(SqliteDb {
            conn: RefCell::new(conn),
            written: Cell::new(0),
            statements: Cell::new(0),
        })
    }

    /// Applies a migration file: several statements separated by semicolons.
    pub fn apply(&self, sql: &str) -> DbResult<()> {
        self.conn.borrow().execute_batch(sql).map_err(err)
    }

    /// An in-memory database with every migration under `cloud/migrations` applied.
    pub fn with_schema() -> DbResult<Self> {
        let db = Self::open_in_memory()?;
        db.apply(include_str!("../../../migrations/0001_init.sql"))?;
        db.apply(include_str!("../../../migrations/0002_purge.sql"))?;
        db.apply(include_str!("../../../migrations/0003_user_tasks.sql"))?;
        db.apply(include_str!("../../../migrations/0004_email.sql"))?;
        Ok(db)
    }

    /// Statements executed through this handle; each is a round trip on D1.
    pub fn statements(&self) -> u64 {
        self.statements.get()
    }
}

fn run_one(conn: &Connection, stmt: &Stmt) -> rusqlite::Result<Rows> {
    let mut prepared = conn.prepare_cached(&stmt.sql)?;
    for (i, p) in stmt.params.iter().enumerate() {
        match p {
            Value::Null => prepared.raw_bind_parameter(i + 1, rusqlite::types::Null)?,
            Value::Int(v) => prepared.raw_bind_parameter(i + 1, v)?,
            Value::Real(v) => prepared.raw_bind_parameter(i + 1, v)?,
            Value::Text(v) => prepared.raw_bind_parameter(i + 1, v)?,
        }
    }
    let names: Vec<String> = prepared.column_names().map(|s| s.to_string()).collect();
    let mut out = Rows::default();
    let mut rows = prepared.raw_query();
    while let Some(row) = rows.next()? {
        let mut map = Row::new();
        for (i, name) in names.iter().enumerate() {
            let v = match row.get_ref(i)? {
                ValueRef::Null => serde_json::Value::Null,
                ValueRef::Integer(n) => serde_json::Value::from(n),
                ValueRef::Real(f) => normalize_number(f),
                ValueRef::Text(t) => {
                    serde_json::Value::from(String::from_utf8_lossy(t).into_owned())
                }
                ValueRef::Blob(_) => serde_json::Value::Null,
            };
            map.insert(name.clone(), v);
        }
        out.rows.push(map);
    }
    drop(rows);
    drop(prepared);
    out.rows_read = out.rows.len() as u64;
    Ok(out)
}

#[async_trait(?Send)]
impl Db for SqliteDb {
    async fn batch(&self, stmts: Vec<Stmt>) -> DbResult<Vec<Rows>> {
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction().map_err(err)?;
        let mut out = Vec::with_capacity(stmts.len());
        for stmt in &stmts {
            let before = tx.total_changes();
            let mut rows =
                run_one(&tx, stmt).map_err(|e| DbError::Backend(format!("{e}: {}", stmt.sql)))?;
            rows.changes = tx.total_changes() - before;
            rows.rows_written = rows.changes;
            self.written.set(self.written.get() + rows.changes);
            self.statements.set(self.statements.get() + 1);
            out.push(rows);
        }
        tx.commit().map_err(err)?;
        Ok(out)
    }

    fn rows_written(&self) -> u64 {
        self.written.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_db::stmt;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        futures_lite::future::block_on(f)
    }

    #[test]
    fn schema_applies_and_round_trips() {
        let db = SqliteDb::with_schema().unwrap();
        block_on(async {
            let id = db
                .run(stmt!(
                    "INSERT INTO platform (create_time, name, user_friendly_name) VALUES (?, ?, ?) RETURNING id",
                    1, "x86_64-pc-linux-gnu", "Linux"
                ))
                .await
                .unwrap()
                .scalar_i64()
                .unwrap();
            assert_eq!(id, 1);
            let r = db
                .run(stmt!(
                    "SELECT name, deprecated FROM platform WHERE id = ?",
                    id
                ))
                .await
                .unwrap();
            assert_eq!(r.rows[0]["name"], "x86_64-pc-linux-gnu");
            assert_eq!(r.rows[0]["deprecated"], 0);
            assert_eq!(db.rows_written(), 1);
        });
    }

    #[test]
    fn failed_batch_rolls_back() {
        let db = SqliteDb::with_schema().unwrap();
        block_on(async {
            let res = db
                .batch(vec![
                    stmt!("INSERT INTO config (name, value) VALUES ('a', 'b')"),
                    stmt!("INSERT INTO no_such_table VALUES (1)"),
                ])
                .await;
            assert!(res.is_err());
            let r = db
                .run(stmt!("SELECT count(*) AS n FROM config"))
                .await
                .unwrap();
            assert_eq!(r.scalar_i64(), Some(0));
        });
    }
}
