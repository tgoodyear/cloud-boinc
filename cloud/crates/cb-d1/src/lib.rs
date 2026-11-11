//! [`cb_db::Db`] on a D1 binding.
//!
//! One statement is one round trip; a batch is one round trip and one
//! transaction. Row counts come from the `meta` D1 returns, which is what
//! the account is billed by.

use std::cell::Cell;

use async_trait::async_trait;
use cb_db::{normalize_number, Db, DbError, DbResult, Row, Rows, Stmt, Value};
use worker::wasm_bindgen::JsValue;
use worker::{D1Database, D1PreparedStatement, D1Result};

pub struct D1Db {
    db: D1Database,
    written: Cell<u64>,
    read: Cell<u64>,
    statements: Cell<u64>,
    round_trips: Cell<u64>,
}

fn err(e: worker::Error) -> DbError {
    DbError::Backend(e.to_string())
}

impl D1Db {
    pub fn new(db: D1Database) -> Self {
        D1Db {
            db,
            written: Cell::new(0),
            read: Cell::new(0),
            statements: Cell::new(0),
            round_trips: Cell::new(0),
        }
    }

    /// Rows read through this handle, as D1 bills them.
    pub fn rows_read(&self) -> u64 {
        self.read.get()
    }

    pub fn statements(&self) -> u64 {
        self.statements.get()
    }

    pub fn round_trips(&self) -> u64 {
        self.round_trips.get()
    }

    fn prepare(&self, stmt: &Stmt) -> DbResult<D1PreparedStatement> {
        let prepared = self.db.prepare(&stmt.sql);
        if stmt.params.is_empty() {
            return Ok(prepared);
        }
        // D1 takes JavaScript numbers; integers are exact up to 2^53.
        let values: Vec<JsValue> = stmt
            .params
            .iter()
            .map(|p| match p {
                Value::Null => JsValue::NULL,
                Value::Int(v) => JsValue::from_f64(*v as f64),
                Value::Real(v) => JsValue::from_f64(*v),
                Value::Text(v) => JsValue::from_str(v),
            })
            .collect();
        prepared.bind(&values).map_err(err)
    }

    fn convert(&self, result: &D1Result) -> DbResult<Rows> {
        let mut out = Rows::default();
        for value in result.results::<serde_json::Value>().map_err(err)? {
            let serde_json::Value::Object(map) = value else {
                return Err(DbError::Decode("a row is not an object".into()));
            };
            let mut row = Row::new();
            for (name, v) in map {
                let v = match v {
                    serde_json::Value::Number(n) => match n.as_i64() {
                        Some(i) => serde_json::Value::from(i),
                        None => normalize_number(n.as_f64().unwrap_or(0.0)),
                    },
                    serde_json::Value::Bool(b) => serde_json::Value::from(b as i64),
                    other => other,
                };
                row.insert(name, v);
            }
            out.rows.push(row);
        }
        if let Some(meta) = result.meta().map_err(err)? {
            out.changes = meta.changes.unwrap_or(0) as u64;
            out.rows_written = meta.rows_written.unwrap_or(0) as u64;
            out.rows_read = meta.rows_read.unwrap_or(0) as u64;
        }
        self.written.set(self.written.get() + out.rows_written);
        self.read.set(self.read.get() + out.rows_read);
        self.statements.set(self.statements.get() + 1);
        Ok(out)
    }
}

#[async_trait(?Send)]
impl Db for D1Db {
    async fn batch(&self, stmts: Vec<Stmt>) -> DbResult<Vec<Rows>> {
        if stmts.is_empty() {
            return Ok(Vec::new());
        }
        self.round_trips.set(self.round_trips.get() + 1);
        if stmts.len() == 1 {
            let result = self.prepare(&stmts[0])?.all().await.map_err(err)?;
            return Ok(vec![self.convert(&result)?]);
        }
        let prepared = stmts
            .iter()
            .map(|s| self.prepare(s))
            .collect::<DbResult<Vec<_>>>()?;
        let results = self.db.batch(prepared).await.map_err(err)?;
        results.iter().map(|r| self.convert(r)).collect()
    }

    fn rows_written(&self) -> u64 {
        self.written.get()
    }
}
