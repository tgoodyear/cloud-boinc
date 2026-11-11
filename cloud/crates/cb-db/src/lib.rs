//! Database interface shared by the Workers (D1) and the native tests (SQLite).
//!
//! D1 has no interactive transactions: a Worker sends one statement, or a batch
//! that runs as one transaction. The trait has that shape, so code written
//! against it runs unchanged on both back ends.

use async_trait::async_trait;
use serde::de::DeserializeOwned;

pub type Row = serde_json::Map<String, serde_json::Value>;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database: {0}")]
    Backend(String),
    #[error("row decode: {0}")]
    Decode(String),
}

pub type DbResult<T> = Result<T, DbError>;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
}

macro_rules! value_from_int {
    ($($t:ty),*) => {$(
        impl From<$t> for Value {
            fn from(v: $t) -> Self {
                Value::Int(v as i64)
            }
        }
    )*};
}
value_from_int!(i64, i32, u32, u8, usize);

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Int(v as i64)
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        // SQLite has no NaN or infinity; hosts do report them.
        if v.is_finite() {
            Value::Real(v)
        } else {
            Value::Real(0.0)
        }
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Text(v.to_string())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Text(v)
    }
}
impl From<&String> for Value {
    fn from(v: &String) -> Self {
        Value::Text(v.clone())
    }
}

#[derive(Debug, Clone)]
pub struct Stmt {
    pub sql: String,
    pub params: Vec<Value>,
}

impl Stmt {
    pub fn new(sql: impl Into<String>) -> Self {
        Stmt {
            sql: sql.into(),
            params: Vec::new(),
        }
    }

    pub fn bind(mut self, v: impl Into<Value>) -> Self {
        self.params.push(v.into());
        self
    }
}

/// Builds a [`Stmt`]: `stmt!("SELECT * FROM host WHERE id = ?", id)`.
#[macro_export]
macro_rules! stmt {
    ($sql:expr $(, $p:expr)* $(,)?) => {
        $crate::Stmt { sql: ($sql).to_string(), params: vec![$($crate::Value::from($p)),*] }
    };
}

/// Outcome of one statement.
#[derive(Debug, Default, Clone)]
pub struct Rows {
    pub rows: Vec<Row>,
    /// Rows changed by an INSERT, UPDATE or DELETE.
    pub changes: u64,
    /// Rows written including index entries, as D1 bills them. The SQLite back
    /// end reports `changes` here.
    pub rows_written: u64,
    pub rows_read: u64,
}

impl Rows {
    pub fn decode<T: DeserializeOwned>(self) -> DbResult<Vec<T>> {
        self.rows
            .into_iter()
            .map(|r| {
                serde_json::from_value(serde_json::Value::Object(r))
                    .map_err(|e| DbError::Decode(e.to_string()))
            })
            .collect()
    }

    pub fn first<T: DeserializeOwned>(self) -> DbResult<Option<T>> {
        match self.rows.into_iter().next() {
            Some(r) => serde_json::from_value(serde_json::Value::Object(r))
                .map(Some)
                .map_err(|e| DbError::Decode(e.to_string())),
            None => Ok(None),
        }
    }

    /// First column of the first row as an integer, e.g. `RETURNING id`.
    pub fn scalar_i64(&self) -> Option<i64> {
        let v = self.rows.first()?.values().next()?;
        v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
    }
}

/// Numbers come back from D1 as JavaScript numbers, so an integer-valued REAL
/// column and an INTEGER column look the same. Both back ends store whole
/// numbers as JSON integers so that rows decode the same way everywhere.
pub fn normalize_number(f: f64) -> serde_json::Value {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15 {
        serde_json::Value::from(f as i64)
    } else {
        serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::from(0))
    }
}

#[async_trait(?Send)]
pub trait Db {
    /// Runs the statements in order as one transaction.
    async fn batch(&self, stmts: Vec<Stmt>) -> DbResult<Vec<Rows>>;

    async fn run(&self, stmt: Stmt) -> DbResult<Rows> {
        let mut out = self.batch(vec![stmt]).await?;
        out.pop()
            .ok_or_else(|| DbError::Backend("empty batch result".into()))
    }

    /// Total rows written through this handle, for write-budget assertions.
    fn rows_written(&self) -> u64 {
        0
    }
}

pub async fn query<T: DeserializeOwned>(db: &dyn Db, stmt: Stmt) -> DbResult<Vec<T>> {
    db.run(stmt).await?.decode()
}

pub async fn query_one<T: DeserializeOwned>(db: &dyn Db, stmt: Stmt) -> DbResult<Option<T>> {
    db.run(stmt).await?.first()
}
