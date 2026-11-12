//! Where mapped rows go: a SQLite file, or numbered `.sql` files for D1.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use fallible_iterator::FallibleIterator;
use rusqlite::Connection;

use crate::error::{ImportError, Result};
use crate::schema::{apply_migrations, TableInfo};
use crate::value::{text_literal, Value};

/// D1 limits (https://developers.cloudflare.com/d1/platform/limits/, read
/// 2026-11-12): a statement is at most 100,000 bytes, a row at most
/// 2,000,000 bytes, a statement binds at most 100 parameters (none are bound
/// here: every value is a literal), and a file given to `d1 execute --file`
/// is at most 5 GB.
pub const D1_MAX_STATEMENT_BYTES: usize = 100_000;
pub const D1_MAX_ROW_BYTES: usize = 2_000_000;
/// The statement size the importer keeps to: under D1's, with room for the
/// `UPDATE` wrapper of a chunk.
pub const STATEMENT_BUDGET: usize = 96_000;
/// Text of one chunk of an oversize column, in bytes of source text. Written
/// as hex it is twice that, under [`STATEMENT_BUDGET`].
pub const CHUNK_BYTES: usize = 40_000;
/// The default size of one `.sql` file.
/// Written beside the `.sql` files: the Unix time of the import.
pub const IMPORT_TIME_FILE: &str = "import_time.txt";
pub const DEFAULT_MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

pub trait Sink {
    fn begin_table(&mut self, table: &TableInfo) -> Result<()>;
    fn row(&mut self, table: &TableInfo, vals: &[Value]) -> Result<()>;
    fn end_table(&mut self, table: &TableInfo) -> Result<u64>;
    /// Records the time of the import, which later checks use for the one value the
    /// import derives from it (see `Ctx::workunit`).
    fn set_import_time(&mut self, _t: i64) -> Result<()> {
        Ok(())
    }
    /// `(name, last_id)` pairs for the `id_floor` table.
    fn finish(&mut self, floors: &[(String, i64)]) -> Result<()>;
}

// ---------------------------------------------------------------- SQLite

pub struct SqliteSink {
    pub path: PathBuf,
    conn: Connection,
    in_txn: u64,
    written: u64,
}

const TXN_ROWS: u64 = 20_000;

impl SqliteSink {
    /// Creates the file (refusing to overwrite one) with the schema of
    /// `cloud/migrations`.
    pub fn create(path: &Path) -> Result<SqliteSink> {
        if path.exists() {
            return Err(ImportError::usage(
                "the --target-sqlite file exists; give a path that does not",
            ));
        }
        crate::fsutil::create_private_file(path)
            .map_err(|_| ImportError::usage("cannot create the --target-sqlite file"))?;
        let conn = Connection::open(path)?;
        // The file is a build artefact; if the run fails it is deleted.
        conn.execute_batch(
            "PRAGMA journal_mode = OFF; PRAGMA synchronous = OFF; PRAGMA locking_mode = EXCLUSIVE; \
             PRAGMA temp_store = MEMORY; PRAGMA cache_size = -200000;",
        )?;
        apply_migrations(&conn)?;
        Ok(SqliteSink {
            path: path.to_path_buf(),
            conn,
            in_txn: 0,
            written: 0,
        })
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }
}

impl Sink for SqliteSink {
    fn begin_table(&mut self, _t: &TableInfo) -> Result<()> {
        self.conn.execute_batch("BEGIN")?;
        self.in_txn = 0;
        self.written = 0;
        Ok(())
    }
    fn row(&mut self, t: &TableInfo, vals: &[Value]) -> Result<()> {
        let mut st = self.conn.prepare_cached(&t.insert_sql())?;
        st.execute(rusqlite::params_from_iter(vals.iter()))?;
        self.written += 1;
        self.in_txn += 1;
        if self.in_txn >= TXN_ROWS {
            self.conn.execute_batch("COMMIT; BEGIN")?;
            self.in_txn = 0;
        }
        Ok(())
    }
    fn end_table(&mut self, _t: &TableInfo) -> Result<u64> {
        self.conn.execute_batch("COMMIT")?;
        Ok(self.written)
    }
    fn set_import_time(&mut self, t: i64) -> Result<()> {
        // In the SQLite file header, so it is not a table that could be loaded anywhere.
        self.conn
            .execute_batch(&format!("PRAGMA user_version = {t}"))?;
        Ok(())
    }
    fn finish(&mut self, floors: &[(String, i64)]) -> Result<()> {
        for (name, id) in floors {
            self.conn.execute(
                "INSERT INTO id_floor (name, last_id) VALUES (?1, ?2) \
                 ON CONFLICT (name) DO UPDATE SET last_id = max(last_id, excluded.last_id)",
                rusqlite::params![name, id],
            )?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- SQL files

pub struct SqlDirSink {
    dir: PathBuf,
    max_file_bytes: u64,
    next_no: u32,
    out: Option<BufWriter<File>>,
    file_bytes: u64,
    table_name: String,
    part: u32,
    written: u64,
    pub files: Vec<PathBuf>,
    /// The import time marker, once this run has written it.
    pub marker: Option<PathBuf>,
    pub max_statement_seen: usize,
    pub split_rows: u64,
}

impl SqlDirSink {
    pub fn create(dir: &Path, max_file_bytes: u64) -> Result<SqlDirSink> {
        crate::fsutil::create_private_dir_all(dir)
            .map_err(|_| ImportError::usage("cannot create the --target-sql-dir directory"))?;
        let any = std::fs::read_dir(dir)
            .map_err(|_| ImportError::usage("cannot read the --target-sql-dir directory"))?
            .any(|e| {
                e.ok()
                    .map(|e| {
                        let n = e.file_name().to_string_lossy().into_owned();
                        n.ends_with(".sql") || n == IMPORT_TIME_FILE
                    })
                    .unwrap_or(false)
            });
        if any {
            return Err(ImportError::usage(
                "the --target-sql-dir directory already holds output of an import (.sql files or import_time.txt); give an empty one",
            ));
        }
        Ok(SqlDirSink {
            dir: dir.to_path_buf(),
            max_file_bytes,
            next_no: 1,
            out: None,
            file_bytes: 0,
            table_name: String::new(),
            part: 0,
            written: 0,
            files: Vec::new(),
            marker: None,
            max_statement_seen: 0,
            split_rows: 0,
        })
    }

    fn open_next(&mut self, label: &str) -> Result<()> {
        self.close()?;
        let name = format!("{:04}_{label}.sql", self.next_no);
        self.next_no += 1;
        let path = self.dir.join(&name);
        let f = crate::fsutil::create_private_file(&path)
            .map_err(|_| ImportError::target(format!("cannot create {name}")))?;
        self.out = Some(BufWriter::with_capacity(1 << 20, f));
        self.files.push(path);
        self.file_bytes = 0;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        if let Some(mut o) = self.out.take() {
            o.flush()
                .map_err(|_| ImportError::target("writing a .sql file failed"))?;
        }
        Ok(())
    }

    fn emit(&mut self, stmt: &str) -> Result<()> {
        if self.out.is_none() || self.file_bytes + stmt.len() as u64 + 1 > self.max_file_bytes {
            let label = if self.part == 0 {
                self.table_name.clone()
            } else {
                format!("{}_{}", self.table_name, self.part)
            };
            self.part += 1;
            self.open_next(&label)?;
        }
        let o = self.out.as_mut().expect("file is open");
        o.write_all(stmt.as_bytes())
            .and_then(|_| o.write_all(b"\n"))
            .map_err(|_| ImportError::target("writing a .sql file failed"))?;
        self.file_bytes += stmt.len() as u64 + 1;
        self.max_statement_seen = self.max_statement_seen.max(stmt.len());
        Ok(())
    }
}

/// The statements for one row: one INSERT, and when it would be over
/// [`STATEMENT_BUDGET`], the largest text columns are written empty in it and
/// filled by `UPDATE ... SET col = col || chunk` statements.
pub fn row_statements(t: &TableInfo, vals: &[Value]) -> Result<Vec<String>> {
    let names = t.col_names().join(", ");
    let mut lits: Vec<String> = Vec::with_capacity(vals.len());
    let mut total = 0usize;
    for v in vals {
        let l = v.literal()?;
        total += match v {
            Value::Text(s) => s.len(),
            _ => l.len(),
        };
        lits.push(l);
    }
    if total > crate::sink::D1_MAX_ROW_BYTES {
        let pk = t.pk_cols();
        let id = pk
            .first()
            .and_then(|&i| vals[i].as_i64())
            .map(|i| i.to_string())
            .unwrap_or_else(|| "?".into());
        return Err(ImportError::refused(format!(
            "a row of {} (key {id}) is over D1's 2,000,000 byte row limit",
            t.name
        )));
    }
    let overhead = 40 + t.name.len() + names.len();
    let mut deferred: Vec<usize> = Vec::new();
    let mut size: usize = overhead + lits.iter().map(|l| l.len() + 2).sum::<usize>();
    while size > STATEMENT_BUDGET {
        // The longest text literal not yet moved out.
        let Some((i, _)) = lits
            .iter()
            .enumerate()
            .filter(|(i, _)| matches!(vals[*i], Value::Text(_)) && !deferred.contains(i))
            .filter(|(_, l)| l.len() > 4)
            .max_by_key(|(_, l)| l.len())
        else {
            return Err(ImportError::refused(format!(
                "a row of {} cannot be split under D1's statement limit",
                t.name
            )));
        };
        size -= lits[i].len();
        lits[i] = "''".to_string();
        size += 2;
        deferred.push(i);
    }
    let mut out = vec![format!(
        "INSERT INTO {} ({}) VALUES ({});",
        t.name,
        names,
        lits.join(", ")
    )];
    if !deferred.is_empty() {
        let pk = t.pk_cols();
        let wh: Vec<String> = pk
            .iter()
            .map(|&i| Ok(format!("{} = {}", t.cols[i].name, vals[i].literal()?)))
            .collect::<Result<_>>()?;
        let wh = wh.join(" AND ");
        for &i in &deferred {
            let text = vals[i].as_str().unwrap_or("");
            let mut rest = text;
            let mut first = true;
            while !rest.is_empty() {
                let mut cut = CHUNK_BYTES.min(rest.len());
                while !rest.is_char_boundary(cut) {
                    cut -= 1;
                }
                let (chunk, tail) = rest.split_at(cut);
                let col = &t.cols[i].name;
                let lit = text_literal(chunk);
                if first {
                    out.push(format!("UPDATE {} SET {col} = {lit} WHERE {wh};", t.name));
                } else {
                    out.push(format!(
                        "UPDATE {} SET {col} = {col} || {lit} WHERE {wh};",
                        t.name
                    ));
                }
                first = false;
                rest = tail;
            }
        }
    }
    Ok(out)
}

impl Sink for SqlDirSink {
    fn begin_table(&mut self, t: &TableInfo) -> Result<()> {
        self.close()?;
        self.table_name = t.name.clone();
        self.part = 0;
        self.written = 0;
        Ok(())
    }
    fn row(&mut self, t: &TableInfo, vals: &[Value]) -> Result<()> {
        let stmts = row_statements(t, vals)?;
        if stmts.len() > 1 {
            self.split_rows += 1;
        }
        for s in &stmts {
            self.emit(s)?;
        }
        self.written += 1;
        Ok(())
    }
    fn end_table(&mut self, _t: &TableInfo) -> Result<u64> {
        self.close()?;
        Ok(self.written)
    }
    fn set_import_time(&mut self, t: i64) -> Result<()> {
        // Not a `.sql` file, so nothing that loads the directory's `.sql` files sees it.
        use std::io::Write as _;
        let path = self.dir.join(IMPORT_TIME_FILE);
        let mut f = crate::fsutil::create_private_file(&path)
            .map_err(|_| ImportError::target("cannot write the import time file"))?;
        self.marker = Some(path);
        writeln!(f, "{t}").map_err(|_| ImportError::target("cannot write the import time file"))
    }
    fn finish(&mut self, floors: &[(String, i64)]) -> Result<()> {
        self.table_name = "id_floor".into();
        self.part = 0;
        self.close()?;
        for (name, id) in floors {
            let s = format!(
                "INSERT INTO id_floor (name, last_id) VALUES ({}, {id}) ON CONFLICT (name) DO UPDATE SET last_id = max(last_id, excluded.last_id);",
                text_literal(name)
            );
            self.emit(&s)?;
        }
        self.close()
    }
}

/// Applies the `.sql` files of `dir` in name order to a new SQLite file with
/// the target schema. This is how a rehearsal checks the files without D1.
pub fn load_sql_dir(dir: &Path, target: &Path) -> Result<HashMap<String, u64>> {
    if target.exists() {
        return Err(ImportError::usage(
            "the --target-sqlite file exists; give a path that does not",
        ));
    }
    // The file is this call's own from here: a failure removes it, and nothing else does.
    let r = load_sql_dir_into(dir, target);
    if r.is_err() {
        let _ = std::fs::remove_file(target);
    }
    r
}

fn load_sql_dir_into(dir: &Path, target: &Path) -> Result<HashMap<String, u64>> {
    crate::fsutil::create_private_file(target)
        .map_err(|_| ImportError::usage("cannot create the --target-sqlite file"))?;
    let conn = Connection::open(target)?;
    conn.execute_batch(
        "PRAGMA journal_mode = OFF; PRAGMA synchronous = OFF; PRAGMA locking_mode = EXCLUSIVE;",
    )?;
    apply_migrations(&conn)?;
    if let Ok(t) = std::fs::read_to_string(dir.join(IMPORT_TIME_FILE)) {
        if let Ok(t) = t.trim().parse::<i64>() {
            conn.execute_batch(&format!("PRAGMA user_version = {t}"))?;
        }
    }
    let mut names: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|_| ImportError::usage("cannot read the --sql-dir directory"))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "sql"))
        .collect();
    names.sort();
    let mut counts = HashMap::new();
    for p in names {
        let sql = std::fs::read_to_string(&p)
            .map_err(|_| ImportError::usage("cannot read a .sql file"))?;
        conn.execute_batch("BEGIN")?;
        // Statement by statement, so a failure can say which one. The error is the code
        // and class only: the text of a damaged dump is data.
        let fname = p
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let mut n = 0u64;
        let mut batch = rusqlite::Batch::new(&conn, &sql);
        loop {
            match batch.next() {
                Ok(Some(mut stmt)) => {
                    n += 1;
                    if let Err(e) = stmt.execute([]) {
                        let _ = conn.execute_batch("ROLLBACK");
                        return Err(ImportError::target(format!(
                            "applying {fname} failed at statement {n}: {}",
                            crate::error::sqlite_message(&e)
                        )));
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(ImportError::target(format!(
                        "applying {fname} failed at statement {}: {}",
                        n + 1,
                        crate::error::sqlite_message(&e)
                    )));
                }
            }
        }
        conn.execute_batch("COMMIT")?;
        *counts
            .entry(p.file_name().unwrap().to_string_lossy().into_owned())
            .or_insert(0) += sql.lines().count() as u64;
    }
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::table_info;

    fn info(table: &str) -> TableInfo {
        let conn = Connection::open_in_memory().unwrap();
        apply_migrations(&conn).unwrap();
        table_info(&conn, table).unwrap()
    }

    #[test]
    fn the_marker_is_tracked_and_a_stale_one_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("o");
        let mut sink = SqlDirSink::create(&dir, DEFAULT_MAX_FILE_BYTES).unwrap();
        assert!(sink.marker.is_none());
        sink.set_import_time(5).unwrap();
        let m = sink.marker.clone().unwrap();
        assert!(m.exists());
        // A directory that holds an earlier run's marker is not an empty output directory.
        assert!(SqlDirSink::create(&dir, DEFAULT_MAX_FILE_BYTES).is_err());
        // The one this run wrote is the one cleanup would remove.
        std::fs::remove_file(&m).unwrap();
        assert!(SqlDirSink::create(&dir, DEFAULT_MAX_FILE_BYTES).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn outputs_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let d = tempfile::tempdir().unwrap();
        let t = info("platform");
        let dir = d.path().join("new/sql");
        let mut sink = SqlDirSink::create(&dir, DEFAULT_MAX_FILE_BYTES).unwrap();
        sink.begin_table(&t).unwrap();
        let vals: Vec<Value> = (0..t.cols.len()).map(|i| t.default_of(i)).collect();
        sink.row(&t, &vals).unwrap();
        sink.end_table(&t).unwrap();
        sink.set_import_time(1_700_000_000).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&d.path().join("new")), 0o700);
        for f in std::fs::read_dir(&dir).unwrap() {
            assert_eq!(mode(&f.unwrap().path()), 0o600);
        }
        let db = d.path().join("t.sqlite");
        let _s = SqliteSink::create(&db).unwrap();
        assert_eq!(mode(&db), 0o600);
    }

    #[test]
    fn small_row_is_one_statement() {
        let t = info("platform");
        let s = row_statements(
            &t,
            &[
                Value::Int(1),
                Value::Int(5),
                Value::Text("x86_64-pc-linux-gnu".into()),
                Value::Text("Linux".into()),
                Value::Int(0),
            ],
        )
        .unwrap();
        assert_eq!(s.len(), 1);
        assert!(s[0].starts_with("INSERT INTO platform (id,"));
        assert!(!s[0].contains('\n'));
    }

    #[test]
    fn oversize_text_is_split_and_reassembles() {
        let t = info("app_version");
        let doc: String = (0..30_000).map(|i| format!("<l>{i}</l>\n")).collect();
        assert!(doc.len() > 200_000);
        let mut vals: Vec<Value> = (0..t.cols.len()).map(|i| t.default_of(i)).collect();
        vals[t.index("id").unwrap()] = Value::Int(7);
        vals[t.index("xml_doc").unwrap()] = Value::Text(doc.clone());
        let stmts = row_statements(&t, &vals).unwrap();
        assert!(stmts.len() > 5);
        for s in &stmts {
            assert!(s.len() <= D1_MAX_STATEMENT_BYTES, "{}", s.len());
            assert!(!s.contains('\n'));
        }
        let conn = Connection::open_in_memory().unwrap();
        apply_migrations(&conn).unwrap();
        for s in &stmts {
            conn.execute_batch(s).unwrap();
        }
        let got: String = conn
            .query_row("SELECT xml_doc FROM app_version WHERE id = 7", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(got, doc);
    }

    #[test]
    fn multibyte_text_splits_on_characters() {
        let t = info("app_version");
        let doc: String = "é\n".repeat(120_000);
        let mut vals: Vec<Value> = (0..t.cols.len()).map(|i| t.default_of(i)).collect();
        vals[t.index("id").unwrap()] = Value::Int(1);
        vals[t.index("xml_doc").unwrap()] = Value::Text(doc.clone());
        let stmts = row_statements(&t, &vals).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        apply_migrations(&conn).unwrap();
        for s in &stmts {
            conn.execute_batch(s).unwrap();
        }
        let got: String = conn
            .query_row("SELECT xml_doc FROM app_version WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(got, doc);
    }

    #[test]
    fn row_over_two_megabytes_is_refused() {
        let t = info("app_version");
        let mut vals: Vec<Value> = (0..t.cols.len()).map(|i| t.default_of(i)).collect();
        vals[t.index("xml_doc").unwrap()] = Value::Text("x".repeat(2_100_000));
        let e = row_statements(&t, &vals).unwrap_err();
        assert!(e.message.contains("row limit"));
    }
}
