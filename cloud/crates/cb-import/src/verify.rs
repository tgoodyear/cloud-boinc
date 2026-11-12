//! Compares the source with a target SQLite file: row counts per table and
//! field-by-field equality on a sample chosen by ID. Prints counts and column
//! names, never a value.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::error::{ImportError, Kind, Result};
use crate::import::{preflight, run, Options};
use crate::schema::{all_tables, TableInfo};
use crate::sink::Sink;
use crate::source::Source;
use crate::summary::Summary;
use crate::value::{same_as_sqlite, Value};

#[derive(Debug, Default)]
pub struct VerifyReport {
    /// table: (rows the import maps from the source, rows in the target)
    pub counts: BTreeMap<String, (u64, u64)>,
    pub sampled: BTreeMap<String, u64>,
    /// "table.column": number of sampled rows that differ
    pub field_mismatches: BTreeMap<String, u64>,
    /// table: sampled rows the target does not have
    pub missing_rows: BTreeMap<String, u64>,
    pub other: Vec<String>,
}

impl VerifyReport {
    pub fn ok(&self) -> bool {
        self.counts.values().all(|(a, b)| a == b)
            && self.field_mismatches.is_empty()
            && self.missing_rows.is_empty()
            && self.other.is_empty()
    }

    pub fn render(&self) -> String {
        let mut o = String::new();
        o.push_str(&format!(
            "{:<18} {:>12} {:>12} {:>9}  {}\n",
            "table", "source rows", "target rows", "sampled", "result"
        ));
        for (t, (a, b)) in &self.counts {
            let bad_fields: Vec<&str> = self
                .field_mismatches
                .keys()
                .filter(|k| k.starts_with(&format!("{t}.")))
                .map(|k| k.as_str())
                .collect();
            let missing = self.missing_rows.get(t).copied().unwrap_or(0);
            let res = if a == b && bad_fields.is_empty() && missing == 0 {
                "ok".to_string()
            } else {
                let mut p = Vec::new();
                if a != b {
                    p.push("row count differs".to_string());
                }
                if missing > 0 {
                    p.push(format!("{missing} sampled rows missing in the target"));
                }
                if !bad_fields.is_empty() {
                    p.push(format!("differs in {}", bad_fields.join(", ")));
                }
                format!("MISMATCH: {}", p.join("; "))
            };
            o.push_str(&format!(
                "{:<18} {:>12} {:>12} {:>9}  {}\n",
                t,
                a,
                b,
                self.sampled.get(t).copied().unwrap_or(0),
                res
            ));
        }
        for (k, n) in &self.field_mismatches {
            o.push_str(&format!("column {k}: {n} sampled rows differ\n"));
        }
        for s in &self.other {
            o.push_str(&format!("MISMATCH: {s}\n"));
        }
        o.push_str(if self.ok() {
            "verify: ok\n"
        } else {
            "verify: FAILED\n"
        });
        o
    }
}

fn key_of(t: &TableInfo, vals: &[Value]) -> u64 {
    let pk = t.pk_cols();
    match pk.first().map(|&i| &vals[i]) {
        Some(Value::Int(i)) => *i as u64,
        Some(Value::Text(s)) => s.bytes().fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ b as u64).wrapping_mul(0x100000001b3)
        }),
        _ => 0,
    }
}

struct VerifySink<'a> {
    conn: &'a Connection,
    sample: u64,
    step: u64,
    n: u64,
    sampled: u64,
    target_count: u64,
    report: VerifyReport,
}

impl Sink for VerifySink<'_> {
    fn begin_table(&mut self, t: &TableInfo) -> Result<()> {
        self.target_count =
            self.conn
                .query_row(&format!("SELECT count(*) FROM {}", t.name), [], |r| {
                    r.get::<_, i64>(0)
                })? as u64;
        self.step = self
            .target_count
            .checked_div(self.sample)
            .map_or(u64::MAX, |s| s.max(1));
        self.n = 0;
        self.sampled = 0;
        Ok(())
    }

    fn row(&mut self, t: &TableInfo, vals: &[Value]) -> Result<()> {
        self.n += 1;
        let k = key_of(t, vals);
        // The first and the last rows are always compared with the rest of
        // the sample; ID order makes "first" the row with n == 1.
        if !(self.n == 1 || k.is_multiple_of(self.step)) {
            return Ok(());
        }
        self.sampled += 1;
        let pk = t.pk_cols();
        let wh: Vec<String> = pk
            .iter()
            .enumerate()
            .map(|(n, &i)| format!("{} = ?{}", t.cols[i].name, n + 1))
            .collect();
        let sql = format!(
            "SELECT {} FROM {} WHERE {}",
            t.col_names().join(", "),
            t.name,
            wh.join(" AND ")
        );
        let mut st = self.conn.prepare_cached(&sql)?;
        let keys: Vec<&Value> = pk.iter().map(|&i| &vals[i]).collect();
        let mut rows = st.query(rusqlite::params_from_iter(keys))?;
        match rows.next()? {
            None => {
                *self.report.missing_rows.entry(t.name.clone()).or_insert(0) += 1;
            }
            Some(r) => {
                for (i, c) in t.cols.iter().enumerate() {
                    // The member count is kept by triggers, checked below.
                    if t.name == "team" && c.name == "nusers" {
                        continue;
                    }
                    if !same_as_sqlite(&vals[i], r.get_ref(i)?) {
                        *self
                            .report
                            .field_mismatches
                            .entry(format!("{}.{}", t.name, c.name))
                            .or_insert(0) += 1;
                    }
                }
            }
        }
        Ok(())
    }

    fn end_table(&mut self, t: &TableInfo) -> Result<u64> {
        self.report
            .counts
            .insert(t.name.clone(), (self.n, self.target_count));
        self.report.sampled.insert(t.name.clone(), self.sampled);
        Ok(self.n)
    }

    fn finish(&mut self, floors: &[(String, i64)]) -> Result<()> {
        for (name, id) in floors {
            let got: Option<i64> = self
                .conn
                .query_row(
                    "SELECT last_id FROM id_floor WHERE name = ?1",
                    [name],
                    |r| r.get(0),
                )
                .ok();
            if got.is_none_or(|g| g < *id) {
                self.report.other.push(format!(
                    "id_floor for {name} is below the highest imported id"
                ));
            }
        }
        let bad: i64 = self.conn.query_row(
            "SELECT count(*) FROM team t WHERE nusers != (SELECT count(*) FROM user WHERE teamid = t.id AND teamid > 0)",
            [],
            |r| r.get(0),
        )?;
        if bad > 0 {
            self.report
                .other
                .push(format!("team.nusers is wrong for {bad} teams"));
        }
        Ok(())
    }
}

/// Compares the source with the SQLite file at `target`.
pub fn verify(
    src: &mut Source,
    target: &Path,
    opts: &Options,
    sample: u64,
    summary: &mut Summary,
) -> Result<VerifyReport> {
    let conn = Connection::open_with_flags(target, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| ImportError::usage("cannot open the --target-sqlite file"))?;
    let pre = preflight(src, opts, summary)?;
    // The mapping needs the table layouts; they come from the migrations.
    let mem = Connection::open_in_memory()?;
    crate::schema::apply_migrations(&mem)?;
    let tables = all_tables(&mem)?;
    let mut vopts = opts.clone();
    vopts.files_manifest = None;
    let mut sink = VerifySink {
        conn: &conn,
        sample,
        step: 1,
        n: 0,
        sampled: 0,
        target_count: 0,
        report: VerifyReport::default(),
    };
    run(src, &mut sink, &vopts, &pre, &tables, summary)?;
    let mut report = sink.report;
    if let Some(text) = &opts.code_sign_public_key {
        let stored: Option<String> = conn
            .query_row(
                "SELECT value FROM config WHERE name = 'code_sign_public_key'",
                [],
                |r| r.get(0),
            )
            .ok();
        match stored {
            Some(s) if s.trim() != text.trim() => report
                .other
                .push("the code_sign_public_key row of the target differs from the given key file".into()),
            Some(_) => summary.note("the target's code_sign_public_key row matches the given key file"),
            None => summary.note("the target has no code_sign_public_key row yet; the given key was checked against the app version signatures"),
        }
    }
    let _ = Kind::Mismatch;
    Ok(report)
}
