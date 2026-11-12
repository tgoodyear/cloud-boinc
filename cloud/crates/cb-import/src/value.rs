//! Values in the target's terms, and their SQL literals.

use rusqlite::types::{ToSqlOutput, Value as SqlValue, ValueRef};
use rusqlite::ToSql;

use crate::error::{ImportError, Result};

/// A value of a target column. Every column of the target schema is an
/// integer, a real or text.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
}

impl Value {
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Real(r) => Some(*r as i64),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Real(r) => Some(*r),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    /// The value as a SQL literal for a `.sql` file. Text that holds a
    /// control character (a newline, a NUL) is written as `CAST(X'..' AS
    /// TEXT)`, so that no statement spans lines and nothing that splits a
    /// file into statements can be confused by the text.
    pub fn literal(&self) -> Result<String> {
        Ok(match self {
            Value::Null => "NULL".to_string(),
            Value::Int(i) => i.to_string(),
            Value::Real(r) => {
                if !r.is_finite() {
                    return Err(ImportError::refused("a real value is not finite"));
                }
                format!("{r:?}")
            }
            Value::Text(s) => text_literal(s),
        })
    }
}

pub fn text_literal(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f) {
        format!("CAST(X'{}' AS TEXT)", hex::encode(s.as_bytes()))
    } else {
        format!("'{}'", s.replace('\'', "''"))
    }
}

impl ToSql for Value {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match self {
            Value::Null => ToSqlOutput::Owned(SqlValue::Null),
            Value::Int(i) => ToSqlOutput::Owned(SqlValue::Integer(*i)),
            Value::Real(r) => ToSqlOutput::Owned(SqlValue::Real(*r)),
            Value::Text(s) => ToSqlOutput::Borrowed(ValueRef::Text(s.as_bytes())),
        })
    }
}

/// A value as MySQL returned it.
#[derive(Debug, Clone)]
pub enum Sv {
    Null,
    Int(i64),
    UInt(u64),
    Float(f64),
    Bytes(Vec<u8>),
}

/// The kind of a target column, from its declared type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColKind {
    Int,
    Real,
    Text,
}

impl ColKind {
    pub fn from_decl(decl: &str) -> ColKind {
        let d = decl.to_ascii_uppercase();
        if d.contains("INT") {
            ColKind::Int
        } else if d.contains("REAL") || d.contains("FLOA") || d.contains("DOUB") {
            ColKind::Real
        } else {
            ColKind::Text
        }
    }
    pub fn zero(self) -> Value {
        match self {
            ColKind::Int => Value::Int(0),
            ColKind::Real => Value::Real(0.0),
            ColKind::Text => Value::Text(String::new()),
        }
    }
}

/// Why a value could not be made a value of the column's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoerceError {
    NotUtf8,
    NotNumber,
}

/// Makes a source value a target value of `kind`. NULL is left to the caller.
pub fn coerce(sv: &Sv, kind: ColKind) -> std::result::Result<Value, CoerceError> {
    match (sv, kind) {
        (Sv::Null, _) => Ok(Value::Null),
        (Sv::Int(i), ColKind::Int) => Ok(Value::Int(*i)),
        (Sv::UInt(u), ColKind::Int) => i64::try_from(*u)
            .map(Value::Int)
            .map_err(|_| CoerceError::NotNumber),
        (Sv::Float(f), ColKind::Int) => Ok(Value::Int(f.round() as i64)),
        (Sv::Int(i), ColKind::Real) => Ok(Value::Real(*i as f64)),
        (Sv::UInt(u), ColKind::Real) => Ok(Value::Real(*u as f64)),
        (Sv::Float(f), ColKind::Real) => Ok(Value::Real(*f)),
        (Sv::Bytes(b), ColKind::Text) => String::from_utf8(b.clone())
            .map(Value::Text)
            .map_err(|_| CoerceError::NotUtf8),
        (Sv::Int(i), ColKind::Text) => Ok(Value::Text(i.to_string())),
        (Sv::UInt(u), ColKind::Text) => Ok(Value::Text(u.to_string())),
        (Sv::Float(f), ColKind::Text) => Ok(Value::Text(format!("{f:?}"))),
        (Sv::Bytes(b), ColKind::Int) => {
            let s = std::str::from_utf8(b).map_err(|_| CoerceError::NotNumber)?;
            let s = s.trim();
            if let Ok(i) = s.parse::<i64>() {
                Ok(Value::Int(i))
            } else {
                s.parse::<f64>()
                    .map(|f| Value::Int(f.round() as i64))
                    .map_err(|_| CoerceError::NotNumber)
            }
        }
        (Sv::Bytes(b), ColKind::Real) => std::str::from_utf8(b)
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .map(Value::Real)
            .ok_or(CoerceError::NotNumber),
    }
}

/// Compares a value of the importer with one read back from SQLite.
pub fn same_as_sqlite(v: &Value, r: ValueRef<'_>) -> bool {
    match (v, r) {
        (Value::Null, ValueRef::Null) => true,
        (Value::Int(a), ValueRef::Integer(b)) => *a == b,
        (Value::Real(a), ValueRef::Real(b)) => a.to_bits() == b.to_bits() || *a == b,
        (Value::Real(a), ValueRef::Integer(b)) => *a == b as f64,
        (Value::Text(a), ValueRef::Text(b)) => a.as_bytes() == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals() {
        assert_eq!(Value::Int(-3).literal().unwrap(), "-3");
        assert_eq!(Value::Real(5.0).literal().unwrap(), "5.0");
        assert_eq!(Value::Real(1e300).literal().unwrap(), "1e300");
        assert_eq!(Value::Text("it's".into()).literal().unwrap(), "'it''s'");
        assert_eq!(Value::Text(String::new()).literal().unwrap(), "''");
        assert_eq!(
            Value::Text("a\nb".into()).literal().unwrap(),
            "CAST(X'610a62' AS TEXT)"
        );
        assert_eq!(Value::Text("a;b".into()).literal().unwrap(), "'a;b'");
        assert!(Value::Real(f64::NAN).literal().is_err());
    }

    #[test]
    fn coercion() {
        assert_eq!(
            coerce(&Sv::Bytes(b"12".to_vec()), ColKind::Int),
            Ok(Value::Int(12))
        );
        assert_eq!(
            coerce(&Sv::Bytes(b"12.4".to_vec()), ColKind::Int),
            Ok(Value::Int(12))
        );
        assert_eq!(coerce(&Sv::Float(2.6), ColKind::Int), Ok(Value::Int(3)));
        assert_eq!(coerce(&Sv::Int(2), ColKind::Real), Ok(Value::Real(2.0)));
        assert_eq!(
            coerce(&Sv::Bytes(vec![0xff]), ColKind::Text),
            Err(CoerceError::NotUtf8)
        );
        assert_eq!(
            coerce(&Sv::Bytes(b"x".to_vec()), ColKind::Int),
            Err(CoerceError::NotNumber)
        );
    }
}
