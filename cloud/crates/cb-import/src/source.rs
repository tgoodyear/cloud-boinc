//! The stock database, read-only.
//!
//! The connection opens a `READ ONLY` transaction with a consistent snapshot
//! and sends nothing but SELECT, SET, START TRANSACTION and SHOW. Every
//! statement passes [`check_statement`] before it leaves, and is kept in
//! [`Source::statements`] so a test can look at what was sent.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use mysql::prelude::Queryable;
use mysql::{Conn, Opts, OptsBuilder, SslOpts, Value as MyValue};

use crate::error::{ImportError, Result};
use crate::value::Sv;

/// True for the statements the importer may send: SELECT, SET, START
/// TRANSACTION and SHOW, one statement each, with nothing that writes (no
/// INTO OUTFILE or DUMPFILE, no locking reads).
pub fn is_read_only_statement(sql: &str) -> bool {
    let t = sql.trim();
    let t = t.strip_suffix(';').unwrap_or(t);
    if t.contains(';') {
        return false;
    }
    let lower = t.to_ascii_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '@'))
        .filter(|w| !w.is_empty())
        .collect();
    let first = words.first().copied().unwrap_or("");
    let allowed = match first {
        "select" | "show" | "set" => true,
        "start" => words.get(1) == Some(&"transaction"),
        _ => false,
    };
    if !allowed {
        return false;
    }
    if words.contains(&"outfile") || words.contains(&"dumpfile") {
        return false;
    }
    // `SELECT ... FOR UPDATE` and `LOCK IN SHARE MODE` take locks.
    if lower.contains("for update") || lower.contains("lock in share mode") {
        return false;
    }
    true
}

pub fn check_statement(sql: &str) -> Result<()> {
    if is_read_only_statement(sql) {
        Ok(())
    } else {
        Err(ImportError::usage(
            "internal error: a statement other than SELECT, SET, START TRANSACTION or SHOW was about to be sent to the source",
        ))
    }
}

/// One row of a result: column names shared by all rows of a statement.
#[derive(Debug, Clone)]
pub struct SrcRow {
    pub cols: Rc<Vec<String>>,
    pub vals: Vec<Sv>,
}

impl SrcRow {
    pub fn get(&self, name: &str) -> Option<&Sv> {
        self.cols
            .iter()
            .position(|c| c == name)
            .map(|i| &self.vals[i])
    }
    pub fn int(&self, name: &str) -> Option<i64> {
        match self.get(name)? {
            Sv::Int(i) => Some(*i),
            Sv::UInt(u) => i64::try_from(*u).ok(),
            Sv::Float(f) => Some(*f as i64),
            Sv::Bytes(b) => std::str::from_utf8(b).ok()?.trim().parse().ok(),
            Sv::Null => None,
        }
    }
    pub fn float(&self, name: &str) -> Option<f64> {
        match self.get(name)? {
            Sv::Int(i) => Some(*i as f64),
            Sv::UInt(u) => Some(*u as f64),
            Sv::Float(f) => Some(*f),
            Sv::Bytes(b) => std::str::from_utf8(b).ok()?.trim().parse().ok(),
            Sv::Null => None,
        }
    }
    pub fn text(&self, name: &str) -> Option<String> {
        match self.get(name)? {
            Sv::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
            Sv::Int(i) => Some(i.to_string()),
            Sv::UInt(u) => Some(u.to_string()),
            Sv::Float(f) => Some(f.to_string()),
            Sv::Null => None,
        }
    }
}

/// A column of a stock table, from `SHOW COLUMNS`.
#[derive(Debug, Clone)]
pub struct SrcCol {
    pub name: String,
    pub sql_type: String,
}

impl SrcCol {
    /// Columns the stock schema declares as `timestamp` or `datetime`.
    pub fn is_time(&self) -> bool {
        let t = self.sql_type.to_ascii_lowercase();
        t.starts_with("timestamp") || t.starts_with("datetime")
    }
}

fn map_error(e: &mysql::Error, what: &str) -> ImportError {
    // The text of a MySQL error can name the account, the host and values;
    // only its code is shown.
    match e {
        mysql::Error::MySqlError(me) => ImportError::source(format!(
            "{what}: the source database answered with error {}",
            me.code
        )),
        mysql::Error::IoError(_) | mysql::Error::DriverError(_) => ImportError::source(format!(
            "{what}: network or protocol error talking to the source database"
        )),
        _ => ImportError::source(format!("{what}: source database error")),
    }
}

fn sv(v: MyValue) -> Result<Sv> {
    Ok(match v {
        MyValue::NULL => Sv::Null,
        MyValue::Bytes(b) => Sv::Bytes(b),
        MyValue::Int(i) => Sv::Int(i),
        MyValue::UInt(u) => Sv::UInt(u),
        MyValue::Float(f) => Sv::Float(f as f64),
        MyValue::Double(d) => Sv::Float(d),
        MyValue::Date(..) | MyValue::Time(..) => {
            return Err(ImportError::source(
                "a column of a date or time type was selected; time columns are read with UNIX_TIMESTAMP",
            ))
        }
    })
}

/// Splits the DSN's own parameters off before the URL goes to the MySQL
/// crate: `tls=required` turns TLS on and `tls_ca_file=PATH` names the PEM
/// file of the CA that signed the server's certificate (the certificate is
/// verified, against that file or else the system's well-known roots).
/// `socket=PATH` and the crate's other parameters are left in the URL.
pub fn parse_dsn(dsn: &str) -> Result<(Opts, Option<SslOpts>)> {
    let bad = || ImportError::usage("the source DSN file does not hold a valid mysql:// URL");
    let (base, query) = match dsn.split_once('?') {
        Some((b, q)) => (b, q),
        None => (dsn, ""),
    };
    let mut tls = false;
    let mut ca: Option<String> = None;
    let mut rest: Vec<&str> = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        match pair.split_once('=') {
            Some(("tls", v)) => tls = matches!(v, "required" | "true" | "1"),
            Some(("tls_ca_file", v)) => ca = Some(v.to_string()),
            _ => rest.push(pair),
        }
    }
    let url = if rest.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", rest.join("&"))
    };
    let opts = Opts::from_url(&url).map_err(|_| bad())?;
    let ssl = if tls {
        let mut s = SslOpts::default();
        if let Some(path) = ca {
            s = s.with_root_cert_path(Some(PathBuf::from(path)));
        }
        Some(s)
    } else {
        None
    };
    Ok((opts, ssl))
}

pub struct Source {
    conn: Conn,
    sent: Vec<String>,
}

impl Source {
    /// Opens the session described by the file at `dsn_file` (one line,
    /// `mysql://user:password@host:port/database`; the file is never echoed).
    pub fn connect(dsn_file: &Path) -> Result<Source> {
        let text = std::fs::read_to_string(dsn_file)
            .map_err(|_| ImportError::usage("cannot read the source DSN file"))?;
        let (opts, ssl) = parse_dsn(text.trim())?;
        let conn = Conn::new(OptsBuilder::from_opts(opts).ssl_opts(ssl))
            .map_err(|e| map_error(&e, "connecting to the source database"))?;
        let mut s = Source {
            conn,
            sent: Vec::new(),
        };
        s.run("SET SESSION TRANSACTION READ ONLY")?;
        s.run("SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ")?;
        // So that UNIX_TIMESTAMP(timestamp column) does not depend on the
        // server's zone.
        s.run("SET SESSION time_zone = '+00:00'")?;
        // MySQL 8 caches table statistics (AUTO_INCREMENT); MariaDB does not
        // have the variable, and the importer goes on without it.
        let _ = s.run("SET SESSION information_schema_stats_expiry = 0");
        s.run("START TRANSACTION READ ONLY, WITH CONSISTENT SNAPSHOT")?;
        Ok(s)
    }

    /// Every statement sent so far.
    pub fn statements(&self) -> &[String] {
        &self.sent
    }

    fn run(&mut self, sql: &str) -> Result<()> {
        check_statement(sql)?;
        self.sent.push(sql.to_string());
        self.conn
            .query_drop(sql)
            .map_err(|e| map_error(&e, "setting up the source session"))
    }

    /// Runs a SELECT and hands each row to `f` as it arrives.
    pub fn query(&mut self, sql: &str, mut f: impl FnMut(SrcRow) -> Result<()>) -> Result<()> {
        check_statement(sql)?;
        self.sent.push(sql.to_string());
        let res = self
            .conn
            .exec_iter(sql, ())
            .map_err(|e| map_error(&e, "reading from the source database"))?;
        let mut cols: Option<Rc<Vec<String>>> = None;
        for row in res {
            let row = row.map_err(|e| map_error(&e, "reading from the source database"))?;
            let names = cols
                .get_or_insert_with(|| {
                    Rc::new(
                        row.columns_ref()
                            .iter()
                            .map(|c| c.name_str().into_owned())
                            .collect(),
                    )
                })
                .clone();
            let mut vals = Vec::with_capacity(names.len());
            for v in row.unwrap() {
                vals.push(sv(v)?);
            }
            f(SrcRow { cols: names, vals })?;
        }
        Ok(())
    }

    pub fn query_all(&mut self, sql: &str) -> Result<Vec<SrcRow>> {
        let mut out = Vec::new();
        self.query(sql, |r| {
            out.push(r);
            Ok(())
        })?;
        Ok(out)
    }

    /// A single integer, or None for NULL or no row.
    pub fn scalar(&mut self, sql: &str) -> Result<Option<i64>> {
        let mut v = None;
        self.query(sql, |r| {
            v = r.vals.first().and_then(|x| match x {
                Sv::Int(i) => Some(*i),
                Sv::UInt(u) => i64::try_from(*u).ok(),
                Sv::Float(f) => Some(*f as i64),
                Sv::Bytes(b) => std::str::from_utf8(b)
                    .ok()
                    .and_then(|s| s.trim().parse().ok()),
                Sv::Null => None,
            });
            Ok(())
        })?;
        Ok(v)
    }

    /// The columns of a stock table. An empty list when it does not exist.
    pub fn columns(&mut self, table: &str) -> Result<Vec<SrcCol>> {
        let exists = self.scalar(&format!(
            "SELECT count(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = '{table}'"
        ))?;
        if exists.unwrap_or(0) == 0 {
            return Ok(Vec::new());
        }
        let rows = self.query_all(&format!("SHOW COLUMNS FROM `{table}`"))?;
        Ok(rows
            .iter()
            .map(|r| SrcCol {
                name: r.text("Field").unwrap_or_default(),
                sql_type: r.text("Type").unwrap_or_default(),
            })
            .collect())
    }

    /// The next AUTO_INCREMENT value of a stock table.
    pub fn auto_increment(&mut self, table: &str) -> Result<Option<i64>> {
        self.scalar(&format!(
            "SELECT AUTO_INCREMENT FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = '{table}'"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsn_parameters() {
        let (o, ssl) = parse_dsn("mysql://u:p@127.0.0.1:3306/db").unwrap();
        assert!(ssl.is_none());
        assert_eq!(o.get_db_name(), Some("db"));
        let (_, ssl) = parse_dsn("mysql://u:p@h/db?tls=required&tls_ca_file=/x/ca.pem").unwrap();
        assert!(ssl.is_some());
        let (o, ssl) =
            parse_dsn("mysql://u:p@localhost/db?socket=/run/mysqld/mysqld.sock").unwrap();
        assert!(ssl.is_none());
        assert_eq!(o.get_socket(), Some("/run/mysqld/mysqld.sock"));
        // A bad URL is a usage error that does not repeat it.
        let e = parse_dsn("not a url with sentinel-password").unwrap_err();
        assert!(!e.message.contains("sentinel"));
    }

    #[test]
    fn only_reads_pass() {
        for ok in [
            "SELECT 1",
            "select id from user where id in (1,2)",
            "SET SESSION TRANSACTION READ ONLY",
            "START TRANSACTION READ ONLY, WITH CONSISTENT SNAPSHOT",
            "SHOW COLUMNS FROM `user`",
            "SELECT count(*) FROM information_schema.TABLES WHERE TABLE_NAME = 'x'",
        ] {
            assert!(is_read_only_statement(ok), "{ok}");
        }
        for bad in [
            "INSERT INTO user VALUES (1)",
            "UPDATE user SET name = 'x'",
            "DELETE FROM user",
            "DROP TABLE user",
            "CREATE TEMPORARY TABLE t (a int)",
            "ALTER TABLE user ADD COLUMN x int",
            "TRUNCATE user",
            "REPLACE INTO user VALUES (1)",
            "CALL p()",
            "LOCK TABLES user READ",
            "COMMIT",
            "BEGIN",
            "SELECT 1; DROP TABLE user",
            "SELECT * FROM user INTO OUTFILE '/tmp/x'",
            "SELECT * FROM user FOR UPDATE",
            "SELECT * FROM user LOCK IN SHARE MODE",
            "START SLAVE",
            "",
        ] {
            assert!(!is_read_only_statement(bad), "{bad}");
        }
    }
}
