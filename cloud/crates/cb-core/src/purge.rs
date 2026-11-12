//! Purging finished workunits, after `sched/db_purge.cpp`: a workunit whose
//! files are deleted and that has not changed for `purge_days` is written to
//! the archive with its results, then removed from the database.

use cb_crypto::md5::md5_hex;
use cb_db::{stmt, Db, Row, Stmt, Value};
use cb_proto::consts::{FILE_DELETE_DONE, RESULT_SERVER_STATE_OVER};
use serde::{Deserialize, Serialize};

use crate::{Config, CoreError, CoreResult, FileKind, Files, NEVER};

/// Most workunits one pass removes. D1 binds at most 100 values to a
/// statement, and the delete binds the ID and revision of each workunit
/// and the cutoff.
pub const MAX_PURGE_BATCH: usize = 45;

/// Upper bound on the text one pass reads and archives, as the database
/// measures it. A Worker has 128 MB of memory; the rows are held once as
/// rows and once as the archive body.
pub const MAX_PURGE_BYTES: i64 = 8 << 20;

#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct PurgeReport {
    pub workunits: u64,
    pub results: u64,
    /// Name of the archive object written, under `archive/`.
    pub archive: Option<String>,
}

/// What one pass archives and then deletes.
#[derive(Debug, Clone, PartialEq)]
pub struct PurgePlan {
    /// Name of the archive object, under `archive/`.
    pub name: String,
    /// The rows, as JSON lines.
    pub body: Vec<u8>,
    /// ID and revision of each workunit in `body`.
    workunits: Vec<(i64, i64)>,
    cutoff: i64,
}

#[derive(Deserialize)]
struct Candidate {
    id: i64,
    bytes: i64,
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

fn field(row: &Row, name: &str) -> i64 {
    row.get(name).and_then(|v| v.as_i64()).unwrap_or(0)
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

/// What a workunit has to be for the purge to take it, on top of its age.
/// `file_delete_state = 2` is written out so that the `wu_purge` partial
/// index applies. A workunit with a transition pending is left for the
/// pipeline first.
fn settled() -> String {
    debug_assert_eq!(FILE_DELETE_DONE, 2);
    format!("file_delete_state = 2 AND transition_time = {NEVER}")
}

/// Reads up to `limit` workunits, oldest first, with their results, and
/// builds the archive object. `None` when nothing is old enough.
///
/// The workunit with the highest ID and the one that owns the result with
/// the highest ID are never taken. The tables' IDs are SQLite row IDs,
/// which are handed out as the highest one plus one, so deleting the
/// highest row would let its ID go to a later job, and a delayed request
/// naming the old job would then act on the new one.
pub async fn plan_purge(
    db: &dyn Db,
    config: &Config,
    now: i64,
    limit: usize,
) -> CoreResult<Option<PurgePlan>> {
    if config.purge_days <= 0.0 || limit == 0 {
        return Ok(None);
    }
    let limit = limit.min(MAX_PURGE_BATCH);
    let cutoff = now - (config.purge_days * 86400.0) as i64;
    // A workunit named "nodelete" is kept, as upstream keeps it. The size
    // is an estimate: the long text columns, and 1000 for the rest of a row.
    let candidates: Vec<Candidate> = db
        .run(stmt!(
            &format!(
                "SELECT w.id, 1000 + length(w.xml_doc) + length(w.result_template) + coalesce(\
                 (SELECT sum(1000 + length(xml_doc_in) + length(xml_doc_out) + length(stderr_out)) \
                 FROM result WHERE result.workunitid = w.id), 0) AS bytes \
                 FROM workunit w WHERE {} AND w.mod_time < ? \
                 AND w.name NOT LIKE '%nodelete%' \
                 AND NOT EXISTS (SELECT 1 FROM result WHERE result.workunitid = w.id \
                 AND result.server_state <> ?) \
                 AND w.id <> (SELECT max(id) FROM workunit) \
                 AND w.id <> coalesce((SELECT workunitid FROM result ORDER BY id DESC LIMIT 1), 0) \
                 ORDER BY w.mod_time LIMIT ?",
                settled()
            ),
            cutoff,
            RESULT_SERVER_STATE_OVER,
            limit
        ))
        .await?
        .decode()?;
    // The first workunit is taken whatever its size, so that one large
    // workunit cannot stop the purge; the rest have to fit the budget.
    let mut ids = Vec::new();
    let mut bytes = 0;
    for c in candidates {
        if !ids.is_empty() && bytes + c.bytes > MAX_PURGE_BYTES {
            break;
        }
        bytes += c.bytes;
        ids.push(c.id);
    }
    if ids.is_empty() {
        return Ok(None);
    }
    let marks = placeholders(ids.len());
    let bind_ids = |sql: String| {
        ids.iter()
            .fold(Stmt::new(sql), |s, id| s.bind(Value::Int(*id)))
    };
    // One batch, so that the workunits and their results are read at the
    // same point.
    let mut read = db
        .batch(vec![
            bind_ids(format!(
                "SELECT * FROM workunit WHERE id IN ({marks}) ORDER BY id"
            )),
            bind_ids(format!(
                "SELECT * FROM result WHERE workunitid IN ({marks}) ORDER BY id"
            )),
        ])
        .await?
        .into_iter();
    let workunits = read.next().map(|r| r.rows).unwrap_or_default();
    let results = read.next().map(|r| r.rows).unwrap_or_default();
    let revs: Vec<(i64, i64)> = workunits
        .iter()
        .map(|w| (field(w, "id"), field(w, "rev")))
        .collect();
    let (Some(first), Some(last)) = (revs.first(), revs.last()) else {
        return Ok(None);
    };
    let (first, last) = (first.0, last.0);
    let mut body = Vec::new();
    for row in workunits {
        push_line(&mut body, "workunit", row)?;
    }
    for row in results {
        push_line(&mut body, "result", row)?;
    }
    // The digest keeps two passes that read different snapshots in the same
    // second from writing over each other's object.
    let name = format!(
        "{}/wu_{first}_{last}_{now}_{}.jsonl",
        utc_date(now),
        md5_hex(&body)
    );
    Ok(Some(PurgePlan {
        name,
        body,
        workunits: revs,
        cutoff,
    }))
}

/// Deletes the workunits of a plan whose archive object is stored, with
/// their results and assignments. A workunit that changed since the plan
/// read it has another revision and stays, with its results; a later pass
/// archives it again as it is then.
pub async fn commit_purge(db: &dyn Db, plan: &PurgePlan) -> CoreResult<PurgeReport> {
    let pairs = vec!["(?, ?)"; plan.workunits.len()].join(", ");
    let unchanged = format!(
        "(id, rev) IN (VALUES {pairs}) AND {} AND mod_time < ?",
        settled()
    );
    let bind = |sql: String| {
        plan.workunits
            .iter()
            .fold(Stmt::new(sql), |s, (id, rev)| {
                s.bind(Value::Int(*id)).bind(Value::Int(*rev))
            })
            .bind(plan.cutoff)
    };
    let out = db
        .batch(vec![
            // Before the results, whose IDs it selects by.
            bind(format!(
                "DELETE FROM upload_epoch WHERE result_id IN (SELECT id FROM result \
                 WHERE workunitid IN (SELECT id FROM workunit WHERE {unchanged}))"
            )),
            bind(format!(
                "DELETE FROM result WHERE workunitid IN (SELECT id FROM workunit WHERE {unchanged})"
            )),
            bind(format!(
                "DELETE FROM assignment WHERE workunitid IN (SELECT id FROM workunit WHERE {unchanged})"
            )),
            bind(format!("DELETE FROM workunit WHERE {unchanged}")),
        ])
        .await?;
    Ok(PurgeReport {
        workunits: out.get(3).map(|r| r.changes).unwrap_or(0),
        results: out.get(1).map(|r| r.changes).unwrap_or(0),
        archive: Some(plan.name.clone()),
    })
}

/// One pass: up to `limit` workunits, oldest first. The rows are stored in
/// one archive object before they are deleted, so a pass that fails after
/// the write archives the same rows again on the next run and loses none.
pub async fn purge_pass(
    db: &dyn Db,
    files: &dyn Files,
    config: &Config,
    now: i64,
    limit: usize,
) -> CoreResult<PurgeReport> {
    let Some(plan) = plan_purge(db, config, now, limit).await? else {
        return Ok(PurgeReport::default());
    };
    files
        .put(FileKind::Archive, &plan.name, plan.body.clone())
        .await?;
    commit_purge(db, &plan).await
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
