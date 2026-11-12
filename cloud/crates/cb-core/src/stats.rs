//! The rollup behind the jobs overview, the week charts and the activity page.
//!
//! A project can hold hundreds of thousands of `result` rows, and these pages
//! are public. No page counts tasks. The pipeline's five-minute cron runs
//! [`rollup_pass`], which does all the counting and writes it to two small
//! tables (`stats_bucket` and `stats_snapshot`, in `migrations/0008_dashboards.sql`);
//! a page reads a few dozen rows of them.
//!
//! What a pass does, in order:
//!
//! 1. Recounts the tasks received in the open three-hour period, and in each
//!    closed one that has not been counted since an hour after it ended (so that
//!    validations that come late are still included), from the partial index
//!    `res_received`. A period that has no row yet is counted too, six per pass,
//!    which fills a new deployment's week in a few passes. The counts are
//!    written absolutely, so a pass that runs twice, or fails half way, leaves
//!    the same rows.
//! 2. Records the number of tasks ready to send, per application, in the open
//!    period (the "queued" chart), counted up to [`UNSENT_COUNT_CAP`] per
//!    application from the partial index `res_unsent`.
//! 3. Writes the `overview` snapshot: per application the tasks ready to send,
//!    in progress, waiting for validation and waiting for assimilation, and what
//!    was completed and what failed in the last day and week (sums of the period
//!    rows).
//! 4. Writes the `inflight` snapshot: tasks in progress by computer and
//!    application, from the covering index `res_host_inprog`.
//!
//! The count of timed-out tasks is the one figure the pass does not make: a task
//! that misses its deadline is never returned, so no `received_time` marks it.
//! The transition that writes it off counts it, in the batch that commits the
//! write-off (`timeout_statements`).
//!
//! Every statement writes the task and workunit states into the SQL as numbers,
//! not bound parameters: SQLite uses a partial index only when the statement's
//! own `WHERE` shows that it applies, and a bound value does not.

use std::collections::{BTreeMap, BTreeSet};

use cb_db::{stmt, Db, Row, Stmt};
use cb_proto::consts::{
    ASSIMILATE_READY, RESULT_OUTCOME_SUCCESS, RESULT_SERVER_STATE_IN_PROGRESS,
    RESULT_SERVER_STATE_UNSENT, VALIDATE_STATE_INVALID, VALIDATE_STATE_VALID,
};
use serde::{Deserialize, Serialize};

use crate::CoreResult;

/// Length of a rollup period.
pub const BUCKET_SECS: i64 = 3 * 3600;
/// Periods the charts show: seven days.
pub const WEEK_BUCKETS: usize = 56;
/// Periods kept; older rows are deleted.
const KEEP_BUCKETS: i64 = 64;
/// A pass that finds another one younger than this does nothing.
pub const MIN_INTERVAL_SECS: i64 = 240;
/// A closed period is recounted until a pass has run this long after its end.
const SETTLE_SECS: i64 = 3600;
/// A closed period is counted again if its last count was before this long after its end.
const EARLY_SECS: i64 = 600;
/// Periods without a row that one pass counts.
const BACKFILL_PER_PASS: usize = 6;
/// Most unsent tasks counted for one application; a larger queue is shown as
/// "at least".
pub const UNSENT_COUNT_CAP: i64 = 100_000;
/// Applications counted individually. The catalog of a real project has a
/// handful; the cap keeps a pass inside the free plan's query limit.
pub const MAX_APPS: usize = 12;
/// Most (computer, application) groups kept in the in-progress snapshot.
pub const MAX_INFLIGHT_ROWS: i64 = 4000;
const SNAPSHOT_VERSION: u32 = 1;

const SNAP_OVERVIEW: &str = "overview";
const SNAP_INFLIGHT: &str = "inflight";
const SNAP_RUN: &str = "rollup_run";

/// Start of the period holding `t`.
pub fn bucket_start(t: i64) -> i64 {
    t - t.rem_euclid(BUCKET_SECS)
}

/// The start of the first period the week chart shows, and of the open one.
pub fn week_bounds(now: i64) -> (i64, i64) {
    let current = bucket_start(now);
    (current - (WEEK_BUCKETS as i64 - 1) * BUCKET_SECS, current)
}

/// The figures of one application, or of the whole project (`id` 0), in the
/// overview snapshot.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AppFigures {
    pub id: i64,
    /// Tasks ready to send; counted up to [`UNSENT_COUNT_CAP`].
    pub unsent: i64,
    pub unsent_capped: bool,
    pub in_progress: i64,
    /// Workunits waiting for the validator.
    pub wait_validate: i64,
    /// Workunits ready to assimilate that the project has not collected.
    pub wait_assimilate: i64,
    /// Tasks received with a success outcome, in the last 24 hours and the
    /// last week, counted in whole three-hour periods.
    pub ok_24h: i64,
    pub ok_7d: i64,
    /// Tasks that failed (any other outcome, invalid, or timed out) in the
    /// same windows.
    pub err_24h: i64,
    pub err_7d: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Overview {
    pub v: u32,
    /// When the rollup wrote it.
    pub at: i64,
    /// Accounts and computers that hold a task in progress.
    #[serde(default)]
    pub working_users: i64,
    #[serde(default)]
    pub working_hosts: i64,
    pub apps: Vec<AppFigures>,
}

/// Tasks in progress held by one computer for one application.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InflightRow {
    pub host: i64,
    pub user: i64,
    pub app: i64,
    pub n: i64,
    /// The computer's last contact with the server.
    pub rpc: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Inflight {
    pub v: u32,
    pub at: i64,
    /// More groups existed than fit; the largest are kept.
    pub truncated: bool,
    pub rows: Vec<InflightRow>,
}

/// One period of the project-wide history.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Period {
    pub bucket: i64,
    pub returned_ok: i64,
    pub valid: i64,
    pub invalid: i64,
    pub failed: i64,
    pub timed_out: i64,
    pub queued: i64,
    pub users: i64,
    pub hosts: i64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RollupReport {
    pub ran: bool,
    /// Starts of the periods recounted.
    pub recounted: Vec<i64>,
    pub apps: usize,
}

fn int(row: &Row, col: &str) -> i64 {
    row.get(col)
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        .unwrap_or(0)
}

/// Counts written by one recount of a period.
#[derive(Default, Clone, Copy)]
struct Counts {
    ok: i64,
    valid: i64,
    invalid: i64,
    failed: i64,
}

/// The periods a pass recounts, newest first: the open one, closed ones not yet
/// settled, and up to [`BACKFILL_PER_PASS`] with no row, none of them before
/// `not_before`.
fn due_periods(now: i64, existing: &BTreeMap<i64, i64>, not_before: i64) -> Vec<i64> {
    let (first, current) = week_bounds(now);
    let mut out = vec![current];
    let mut backfilled = 0;
    let mut b = current - BUCKET_SECS;
    while b >= first {
        match existing.get(&b) {
            Some(&computed) => {
                // Counted again once soon after it closed, and once more an
                // hour after, so that late validations are in; not on every pass.
                let end = b + BUCKET_SECS;
                if computed < end + EARLY_SECS
                    || (computed < end + SETTLE_SECS && now >= end + SETTLE_SECS)
                {
                    out.push(b);
                }
            }
            None => {
                if backfilled < BACKFILL_PER_PASS && b + BUCKET_SECS > not_before {
                    out.push(b);
                    backfilled += 1;
                }
            }
        }
        b -= BUCKET_SECS;
    }
    out
}

fn snapshot_upsert(name: &str, at: i64, body: &str) -> Stmt {
    stmt!(
        "INSERT INTO stats_snapshot (name, at, body) VALUES (?, ?, ?) \
         ON CONFLICT (name) DO UPDATE SET at = excluded.at, body = excluded.body",
        name,
        at,
        body
    )
}

/// The two statements that count tasks written off at their deadline, in the
/// period of `now`, for their application and for the project. They go in the
/// batch of the transition that writes them off, and apply only if that
/// transition's workunit update did (`commit_token`, see "Concurrency" in
/// `cloud/docs/architecture.md`); `n` is how many it wrote off.
pub fn timeout_statements(now: i64, appid: i64, n: i64, wu_id: i64, token: i64) -> Vec<Stmt> {
    let b = bucket_start(now);
    [appid, 0]
        .into_iter()
        .map(|app| {
            // The `WHERE` after the `SELECT` is what makes SQLite read the
            // `ON CONFLICT` as belonging to the `INSERT`.
            stmt!(
                "INSERT INTO stats_bucket (appid, bucket, timed_out) SELECT ?, ?, ? \
                 WHERE EXISTS (SELECT 1 FROM workunit WHERE id = ? AND commit_token = ?) \
                 ON CONFLICT (appid, bucket) DO UPDATE SET timed_out = timed_out + excluded.timed_out",
                app,
                b,
                n,
                wu_id,
                token
            )
        })
        .collect()
}

/// One rollup pass; see the module comment. `not_before` is the earliest time
/// whose tasks may still be in the table: `now` minus the purge delay when
/// workunits are purged, 0 otherwise. Periods that ended before it are not
/// counted, because what is left of them is only part of what happened.
pub async fn rollup_pass(db: &dyn Db, now: i64, not_before: i64) -> CoreResult<RollupReport> {
    let (first, current) = week_bounds(now);
    let mut report = RollupReport::default();

    // Round trip 1: is it time, which applications, which periods have rows.
    let mut out = db
        .batch(vec![
            stmt!("SELECT at FROM stats_snapshot WHERE name = ?", SNAP_RUN),
            stmt!("SELECT id FROM app ORDER BY id LIMIT ?", MAX_APPS),
            stmt!(
                "SELECT bucket, computed_at FROM stats_bucket WHERE appid = 0 AND bucket >= ?",
                first
            ),
        ])
        .await?
        .into_iter();
    let last_run = out
        .next()
        .and_then(|r| r.rows.first().map(|row| int(row, "at")))
        .unwrap_or(0);
    if last_run <= now && now - last_run < MIN_INTERVAL_SECS {
        return Ok(report);
    }
    let app_ids: Vec<i64> = out
        .next()
        .map(|r| r.rows.iter().map(|row| int(row, "id")).collect())
        .unwrap_or_default();
    let existing: BTreeMap<i64, i64> = out
        .next()
        .map(|r| {
            r.rows
                .iter()
                .map(|row| (int(row, "bucket"), int(row, "computed_at")))
                .collect()
        })
        .unwrap_or_default();
    let due = due_periods(now, &existing, not_before);

    // Round trip 2: every count.
    let unsent = RESULT_SERVER_STATE_UNSENT;
    let in_progress = RESULT_SERVER_STATE_IN_PROGRESS;
    let mut reads: Vec<Stmt> = Vec::new();
    for &b in &due {
        // One scan of the period's tasks, grouped finely enough that the
        // accounts and computers can be counted from the groups.
        reads.push(stmt!(
            "SELECT appid, outcome, validate_state, userid, hostid, count(*) AS n FROM result \
             WHERE received_time > 0 AND received_time >= ? AND received_time < ? \
             GROUP BY appid, outcome, validate_state, userid, hostid",
            b,
            b + BUCKET_SECS
        ));
    }
    for &app in &app_ids {
        reads.push(stmt!(
            format!(
                "SELECT count(*) AS n FROM (SELECT 1 FROM result \
                 WHERE server_state = {unsent} AND appid = ? LIMIT ?)"
            ),
            app,
            UNSENT_COUNT_CAP + 1
        ));
    }
    reads.push(Stmt::new(format!(
        "SELECT appid, count(*) AS n FROM result WHERE server_state = {in_progress} GROUP BY appid"
    )));
    reads.push(stmt!(
        format!(
            "SELECT g.hostid, g.appid, g.n, h.userid, h.rpc_time FROM \
             (SELECT hostid, appid, count(*) AS n FROM result \
              WHERE server_state = {in_progress} AND hostid > 0 GROUP BY hostid, appid) g \
             JOIN host h ON h.id = g.hostid ORDER BY g.n DESC, g.hostid, g.appid LIMIT ?"
        ),
        MAX_INFLIGHT_ROWS + 1
    ));
    reads.push(Stmt::new(
        "SELECT appid, count(*) AS n FROM workunit WHERE need_validate = 1 GROUP BY appid",
    ));
    reads.push(Stmt::new(format!(
        "SELECT appid, count(*) AS n FROM workunit WHERE assimilate_state = {ASSIMILATE_READY} \
         GROUP BY appid"
    )));
    let mut rows = db.batch(reads).await?.into_iter();

    // Recount of each due period.
    let mut recounts: Vec<(i64, BTreeMap<i64, Counts>, i64, i64)> = Vec::new();
    for &b in &due {
        let groups = rows.next().unwrap_or_default();
        let mut user_set: BTreeSet<i64> = BTreeSet::new();
        let mut host_set: BTreeSet<i64> = BTreeSet::new();
        let mut by_app: BTreeMap<i64, Counts> = BTreeMap::new();
        for row in &groups.rows {
            let c = by_app.entry(int(row, "appid")).or_default();
            let n = int(row, "n");
            user_set.insert(int(row, "userid"));
            host_set.insert(int(row, "hostid"));
            let (outcome, state) = (int(row, "outcome"), int(row, "validate_state"));
            if outcome == i64::from(RESULT_OUTCOME_SUCCESS) {
                c.ok += n;
                if state == i64::from(VALIDATE_STATE_VALID) {
                    c.valid += n;
                } else if state == i64::from(VALIDATE_STATE_INVALID) {
                    c.invalid += n;
                }
            } else {
                c.failed += n;
            }
        }
        recounts.push((b, by_app, user_set.len() as i64, host_set.len() as i64));
    }

    // The gauges.
    let mut figures: BTreeMap<i64, AppFigures> = BTreeMap::new();
    for &app in &app_ids {
        let n = rows
            .next()
            .and_then(|r| r.rows.first().map(|row| int(row, "n")))
            .unwrap_or(0);
        figures.insert(
            app,
            AppFigures {
                id: app,
                unsent: n.min(UNSENT_COUNT_CAP),
                unsent_capped: n > UNSENT_COUNT_CAP,
                ..Default::default()
            },
        );
    }
    let mut total = AppFigures::default();
    for fig in figures.values() {
        total.unsent += fig.unsent;
        total.unsent_capped |= fig.unsent_capped;
    }
    for row in &rows.next().unwrap_or_default().rows {
        let id = int(row, "appid");
        let n = int(row, "n");
        total.in_progress += n;
        figures
            .entry(id)
            .or_insert_with(|| AppFigures {
                id,
                ..Default::default()
            })
            .in_progress = n;
    }
    let mut inflight = Inflight {
        v: SNAPSHOT_VERSION,
        at: now,
        ..Default::default()
    };
    for row in &rows.next().unwrap_or_default().rows {
        inflight.rows.push(InflightRow {
            host: int(row, "hostid"),
            user: int(row, "userid"),
            app: int(row, "appid"),
            n: int(row, "n"),
            rpc: int(row, "rpc_time"),
        });
    }
    if inflight.rows.len() as i64 > MAX_INFLIGHT_ROWS {
        inflight.rows.truncate(MAX_INFLIGHT_ROWS as usize);
        inflight.truncated = true;
    }
    for row in &rows.next().unwrap_or_default().rows {
        let id = int(row, "appid");
        let n = int(row, "n");
        total.wait_validate += n;
        figures
            .entry(id)
            .or_insert_with(|| AppFigures {
                id,
                ..Default::default()
            })
            .wait_validate = n;
    }
    for row in &rows.next().unwrap_or_default().rows {
        let id = int(row, "appid");
        let n = int(row, "n");
        total.wait_assimilate += n;
        figures
            .entry(id)
            .or_insert_with(|| AppFigures {
                id,
                ..Default::default()
            })
            .wait_assimilate = n;
    }

    // Round trip 3: the period rows, then the sums over them.
    let mut writes: Vec<Stmt> = Vec::new();
    for (b, by_app, users, hosts) in &recounts {
        let mut project = Counts::default();
        for (app, c) in by_app {
            project.ok += c.ok;
            project.valid += c.valid;
            project.invalid += c.invalid;
            project.failed += c.failed;
            writes.push(recount_upsert(*app, *b, c, 0, 0, now));
        }
        writes.push(recount_upsert(0, *b, &project, *users, *hosts, now));
    }
    for fig in figures.values() {
        writes.push(queued_upsert(fig.id, current, fig.unsent));
    }
    writes.push(queued_upsert(0, current, total.unsent));
    if !existing.contains_key(&current) {
        // The first pass of a period: drop what has aged out.
        writes.push(stmt!(
            "DELETE FROM stats_bucket WHERE bucket < ?",
            current - KEEP_BUCKETS * BUCKET_SECS
        ));
    }
    let day = current - (BUCKET_SECS * 7);
    writes.push(stmt!(
        "SELECT appid, \
         coalesce(sum(CASE WHEN bucket >= ? THEN returned_ok ELSE 0 END), 0) AS ok24, \
         coalesce(sum(CASE WHEN bucket >= ? THEN failed + invalid + timed_out ELSE 0 END), 0) AS err24, \
         coalesce(sum(returned_ok), 0) AS ok7, \
         coalesce(sum(failed + invalid + timed_out), 0) AS err7 \
         FROM stats_bucket WHERE bucket >= ? GROUP BY appid",
        day,
        day,
        first
    ));
    let mut written = db.batch(writes).await?;
    let sums = written.pop().unwrap_or_default();
    for row in &sums.rows {
        let id = int(row, "appid");
        let target = if id == 0 {
            &mut total
        } else {
            figures.entry(id).or_insert_with(|| AppFigures {
                id,
                ..Default::default()
            })
        };
        target.ok_24h = int(row, "ok24");
        target.err_24h = int(row, "err24");
        target.ok_7d = int(row, "ok7");
        target.err_7d = int(row, "err7");
    }

    let mut apps: Vec<AppFigures> = vec![total];
    apps.extend(figures.into_values());
    report.apps = apps.len() - 1;
    let overview = Overview {
        v: SNAPSHOT_VERSION,
        at: now,
        working_users: inflight
            .rows
            .iter()
            .map(|r| r.user)
            .collect::<std::collections::BTreeSet<_>>()
            .len() as i64,
        working_hosts: inflight
            .rows
            .iter()
            .map(|r| r.host)
            .collect::<std::collections::BTreeSet<_>>()
            .len() as i64,
        apps,
    };
    let ov =
        serde_json::to_string(&overview).map_err(|e| crate::CoreError::Invalid(e.to_string()))?;
    let inf =
        serde_json::to_string(&inflight).map_err(|e| crate::CoreError::Invalid(e.to_string()))?;
    db.batch(vec![
        snapshot_upsert(SNAP_OVERVIEW, now, &ov),
        snapshot_upsert(SNAP_INFLIGHT, now, &inf),
        snapshot_upsert(SNAP_RUN, now, ""),
    ])
    .await?;
    report.ran = true;
    report.recounted = due;
    Ok(report)
}

fn recount_upsert(app: i64, bucket: i64, c: &Counts, users: i64, hosts: i64, now: i64) -> Stmt {
    stmt!(
        "INSERT INTO stats_bucket (appid, bucket, returned_ok, valid, invalid, failed, users, hosts, computed_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (appid, bucket) DO UPDATE SET returned_ok = excluded.returned_ok, \
         valid = excluded.valid, invalid = excluded.invalid, failed = excluded.failed, \
         users = excluded.users, hosts = excluded.hosts, computed_at = excluded.computed_at",
        app,
        bucket,
        c.ok,
        c.valid,
        c.invalid,
        c.failed,
        users,
        hosts,
        now
    )
}

fn queued_upsert(app: i64, bucket: i64, queued: i64) -> Stmt {
    stmt!(
        "INSERT INTO stats_bucket (appid, bucket, queued) VALUES (?, ?, ?) \
         ON CONFLICT (appid, bucket) DO UPDATE SET queued = excluded.queued",
        app,
        bucket,
        queued
    )
}

#[derive(Deserialize)]
struct SnapshotRow {
    body: String,
}

/// The overview snapshot and the project-wide history, in one round trip. A
/// snapshot in another format, or none yet, is `None`.
pub async fn load_work(
    db: &dyn Db,
    now: i64,
) -> CoreResult<(Option<Overview>, Vec<Option<Period>>)> {
    let (first, current) = week_bounds(now);
    let mut out = db
        .batch(vec![
            stmt!(
                "SELECT body FROM stats_snapshot WHERE name = ?",
                SNAP_OVERVIEW
            ),
            stmt!(
                "SELECT bucket, returned_ok, valid, invalid, failed, timed_out, queued, users, hosts \
                 FROM stats_bucket WHERE appid = 0 AND bucket >= ? AND bucket <= ? ORDER BY bucket",
                first,
                current
            ),
        ])
        .await?
        .into_iter();
    let overview = out
        .next()
        .unwrap_or_default()
        .first::<SnapshotRow>()?
        .and_then(|r| serde_json::from_str::<Overview>(&r.body).ok())
        .filter(|o| o.v == SNAPSHOT_VERSION);
    let periods: Vec<Period> = out.next().unwrap_or_default().decode()?;
    let mut slots: Vec<Option<Period>> = vec![None; WEEK_BUCKETS];
    for p in periods {
        let i = (p.bucket - first) / BUCKET_SECS;
        if (p.bucket - first) % BUCKET_SECS == 0 && (0..WEEK_BUCKETS as i64).contains(&i) {
            slots[i as usize] = Some(p);
        }
    }
    Ok((overview, slots))
}

/// The overview snapshot alone, for the home page.
pub async fn load_overview(db: &dyn Db) -> CoreResult<Option<Overview>> {
    Ok(db
        .run(stmt!(
            "SELECT body FROM stats_snapshot WHERE name = ?",
            SNAP_OVERVIEW
        ))
        .await?
        .first::<SnapshotRow>()?
        .and_then(|r| serde_json::from_str::<Overview>(&r.body).ok())
        .filter(|o| o.v == SNAPSHOT_VERSION))
}

/// The in-progress snapshot.
pub async fn load_inflight(db: &dyn Db) -> CoreResult<Option<Inflight>> {
    Ok(db
        .run(stmt!(
            "SELECT body FROM stats_snapshot WHERE name = ?",
            SNAP_INFLIGHT
        ))
        .await?
        .first::<SnapshotRow>()?
        .and_then(|r| serde_json::from_str::<Inflight>(&r.body).ok())
        .filter(|i| i.v == SNAPSHOT_VERSION))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periods_are_aligned_to_three_hours() {
        assert_eq!(bucket_start(0), 0);
        assert_eq!(bucket_start(10_799), 0);
        assert_eq!(bucket_start(10_800), 10_800);
        let (first, current) = week_bounds(2_000_000_000);
        assert_eq!(current % BUCKET_SECS, 0);
        assert_eq!((current - first) / BUCKET_SECS, WEEK_BUCKETS as i64 - 1);
    }

    #[test]
    fn due_periods_settle_and_backfill() {
        let now = 2_000_000_000;
        let (first, current) = week_bounds(now);
        // A fresh deployment: the open period and six more, newest first.
        let due = due_periods(now, &BTreeMap::new(), 0);
        assert_eq!(due.len(), 1 + BACKFILL_PER_PASS);
        assert_eq!(due[0], current);
        assert_eq!(due[1], current - BUCKET_SECS);
        // Periods counted long after they closed are final.
        let mut existing = BTreeMap::new();
        let mut b = first;
        while b <= current {
            existing.insert(b, b + BUCKET_SECS + SETTLE_SECS);
            b += BUCKET_SECS;
        }
        assert_eq!(due_periods(now, &existing, 0), vec![current]);
        // One counted just after it closed is counted again.
        existing.insert(current - BUCKET_SECS, current + 60);
        assert_eq!(
            due_periods(now, &existing, 0),
            vec![current, current - BUCKET_SECS]
        );
        // One counted a while after it closed waits for the hour mark.
        existing.insert(current - BUCKET_SECS, current + 1200);
        assert_eq!(due_periods(now, &existing, 0), vec![current]);
        assert_eq!(
            due_periods(current + 3605, &existing, 0),
            vec![current, current - BUCKET_SECS]
        );
        // Nothing before `not_before` is backfilled.
        let due = due_periods(now, &BTreeMap::new(), current - BUCKET_SECS);
        assert_eq!(due, vec![current, current - BUCKET_SECS]);
    }
}
