//! Purging finished workunits, after `sched/db_purge.cpp`: a workunit whose
//! files are deleted and that has not changed for `purge_days` is written to
//! the archive with its results, then removed from the database.

use cb_db::{stmt, Db, Row, Stmt, Value};
use cb_proto::consts::{FILE_DELETE_DONE, RESULT_SERVER_STATE_OVER};
use serde::Serialize;

use crate::{Config, CoreError, CoreResult, FileKind, Files};

/// Most workunits one pass removes. D1 binds at most 100 values to a
/// statement, and the delete binds every ID and the cutoff.
pub const MAX_PURGE_BATCH: usize = 90;

#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct PurgeReport {
    pub workunits: u64,
    pub results: u64,
    /// Name of the archive object written, under `archive/`.
    pub archive: Option<String>,
}

/// `YYYY-MM-DD` of a Unix time, in UTC.
fn utc_date(secs: i64) -> String {
    let z = secs.div_euclid(86400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn row_id(row: &Row) -> i64 {
    row.get("id").and_then(|v| v.as_i64()).unwrap_or(0)
}

fn push_line(out: &mut Vec<u8>, table: &str, row: Row) -> CoreResult<()> {
    let mut line = Row::new();
    line.insert("table".into(), table.into());
    line.extend(row);
    serde_json::to_writer(&mut *out, &line).map_err(|e| CoreError::Files(e.to_string()))?;
    out.push(b'\n');
    Ok(())
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

/// One pass: up to `limit` workunits, oldest first. The rows are stored as
/// JSON lines in one archive object before they are deleted, so a pass that
/// fails after the write archives the same rows again on the next run and
/// loses none.
pub async fn purge_pass(
    db: &dyn Db,
    files: &dyn Files,
    config: &Config,
    now: i64,
    limit: usize,
) -> CoreResult<PurgeReport> {
    if config.purge_days <= 0.0 || limit == 0 {
        return Ok(PurgeReport::default());
    }
    debug_assert_eq!(FILE_DELETE_DONE, 2);
    let limit = limit.min(MAX_PURGE_BATCH);
    let cutoff = now - (config.purge_days * 86400.0) as i64;
    // `file_delete_state = 2` is written out so that the `wu_purge` partial
    // index applies. A workunit named "nodelete" is kept, as upstream keeps it.
    let workunits = db
        .run(stmt!(
            "SELECT * FROM workunit WHERE file_delete_state = 2 AND mod_time < ? \
             AND name NOT LIKE '%nodelete%' \
             AND NOT EXISTS (SELECT 1 FROM result WHERE result.workunitid = workunit.id \
             AND result.server_state <> ?) \
             ORDER BY mod_time LIMIT ?",
            cutoff,
            RESULT_SERVER_STATE_OVER,
            limit
        ))
        .await?
        .rows;
    if workunits.is_empty() {
        return Ok(PurgeReport::default());
    }
    let ids: Vec<i64> = workunits.iter().map(row_id).collect();
    let marks = placeholders(ids.len());
    let bind_ids = |sql: String| {
        ids.iter()
            .fold(Stmt::new(sql), |s, id| s.bind(Value::Int(*id)))
    };
    let results = db
        .run(bind_ids(format!(
            "SELECT * FROM result WHERE workunitid IN ({marks}) ORDER BY id"
        )))
        .await?
        .rows;

    let first = ids.iter().min().copied().unwrap_or(0);
    let last = ids.iter().max().copied().unwrap_or(0);
    let name = format!("{}/wu_{first}_{last}_{now}.jsonl", utc_date(now));
    let mut body = Vec::new();
    for row in workunits {
        push_line(&mut body, "workunit", row)?;
    }
    for row in results {
        push_line(&mut body, "result", row)?;
    }
    files.put(FileKind::Archive, &name, body).await?;

    // The conditions are checked again inside the transaction: a workunit
    // that changed since it was read stays, with its results.
    let still = format!(
        "SELECT id FROM workunit WHERE id IN ({marks}) AND file_delete_state = 2 AND mod_time < ?"
    );
    let out = db
        .batch(vec![
            bind_ids(format!("DELETE FROM result WHERE workunitid IN ({still})")).bind(cutoff),
            bind_ids(format!(
                "DELETE FROM assignment WHERE workunitid IN ({still})"
            ))
            .bind(cutoff),
            bind_ids(format!(
                "DELETE FROM workunit WHERE id IN ({marks}) AND file_delete_state = 2 AND mod_time < ?"
            ))
            .bind(cutoff),
        ])
        .await?;
    Ok(PurgeReport {
        workunits: out.get(2).map(|r| r.changes).unwrap_or(0),
        results: out.first().map(|r| r.changes).unwrap_or(0),
        archive: Some(name),
    })
}

#[cfg(test)]
mod tests {
    use super::utc_date;

    #[test]
    fn dates() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(951_782_400), "2000-02-29");
        assert_eq!(utc_date(1_794_441_599), "2026-11-11");
        assert_eq!(utc_date(1_794_441_600), "2026-11-12");
    }
}
