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
//! 1. Recounts the tasks received in the open three-hour period, in each closed
//!    one that has tasks still waiting for validation (at most hourly), in each
//!    closed one soon after it closes, and in periods that have no row yet (six
//!    a pass, which fills a new deployment's week in a few passes), from the
//!    partial index `res_received`. The counts are written absolutely, so a pass
//!    that runs twice, or fails half way, leaves the same rows, and an
//!    application that has no task in a recounted period is written as zero
//!    there. A period that starts before the purge horizon is never counted or
//!    recounted, because what is left of it is only part of what happened.
//! 2. Records the tasks ready to send, in the open period (the "queued" chart).
//!    One statement counts up to [`UNSENT_COUNT_CAP`] tasks in application
//!    order; past the cap the applications from the last one counted on are
//!    marked as lower bounds (`Overview::unsent_from`), and so is the period's
//!    figure (`queued_capped`), which stays marked once the period is closed.
//! 3. Writes the `overview` snapshot: per application the tasks ready to send,
//!    in progress, workunits waiting for validation and for assimilation, and
//!    what was completed and what failed in the last day and week (sums of the
//!    period rows), with how many of the periods those sums cover.
//! 4. Writes the `inflight` snapshot: tasks in progress by computer and
//!    application, from the covering index `res_host_inprog`.
//!
//! A pass is given a statement budget (the Free plan allows 50 queries an
//! invocation, and the cron's purge shares it): it counts as many of the due
//! periods as fit, newest first, and leaves the rest for the next pass. What it
//! sends is a function of the number of periods alone ([`statements_for`]):
//! applications beyond the first [`TRACKED_APPS`] are counted together, and the
//! queue gauge is one row.
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

use cb_db::{stmt, Db, Row, Stmt, Value};
use cb_proto::consts::{
    ASSIMILATE_READY, RESULT_OUTCOME_SUCCESS, RESULT_SERVER_STATE_IN_PROGRESS,
    RESULT_SERVER_STATE_UNSENT, VALIDATE_STATE_INCONCLUSIVE, VALIDATE_STATE_INIT,
    VALIDATE_STATE_INVALID, VALIDATE_STATE_VALID,
};
use serde::{Deserialize, Serialize};

use crate::CoreResult;

/// Length of a rollup period.
pub const BUCKET_SECS: i64 = 3 * 3600;
/// Periods the charts show: seven days.
pub const WEEK_BUCKETS: usize = 56;
/// Periods the figures "of the last 24 hours" cover.
pub const DAY_BUCKETS: usize = 8;
/// Periods kept; older rows are deleted.
const KEEP_BUCKETS: i64 = 64;
/// A pass that finds another one younger than this does nothing.
pub const MIN_INTERVAL_SECS: i64 = 240;
/// A closed period is counted again if its last count was before this long after its end.
const EARLY_SECS: i64 = 600;
/// A closed period with tasks waiting for validation is recounted this often.
const PENDING_RECHECK_SECS: i64 = 3600;
/// Periods without a row that one pass counts.
const BACKFILL_PER_PASS: usize = 6;
/// Most unsent tasks counted in all; a larger queue is shown as "at least".
pub const UNSENT_COUNT_CAP: i64 = 100_000;
/// Most (computer, application) groups kept in the in-progress snapshot.
pub const MAX_INFLIGHT_ROWS: i64 = 4000;
/// Rows of one multi-row upsert (ten values a row; D1 allows 100 values).
const PERIOD_ROWS_PER_STMT: usize = 10;
/// Applications the rollup follows one by one: the first ones by id,
/// deprecated or not, so that the set does not change when an application is
/// deprecated or brought back, and periods counted at different times group
/// their tasks the same way. The
/// tasks of every other application are counted together as [`OTHER_APP`], so
/// the rows and statements of a pass do not grow with the number of
/// applications.
pub const TRACKED_APPS: usize = 18;
/// The `appid` of the row that holds all applications that are not tracked.
pub const OTHER_APP: i64 = -1;
/// Statements of a pass that do not depend on the periods it counts, and those
/// each period costs; see [`statements_for`].
const FIXED_STATEMENTS: usize = 3 + 6 + 3 + 3;
const PERIOD_STATEMENTS: usize = 3;
/// The least budget a pass can work with: its fixed statements and one period (the open one).
pub const MIN_BUDGET: usize = FIXED_STATEMENTS + PERIOD_STATEMENTS;
const SNAPSHOT_VERSION: u32 = 2;

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
    /// Tasks ready to send. A lower bound when `unsent_capped`.
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
    /// Accounts and computers that hold a task in progress, exactly.
    pub working_users: i64,
    pub working_hosts: i64,
    /// 0: every application's queue was counted in full. Otherwise the count
    /// stopped at the cap while counting this application, and the figures of
    /// it and of every later one are lower bounds.
    #[serde(default)]
    pub unsent_from: i64,
    /// The applications that have rows of their own; the tasks of every other
    /// one are in the row of [`OTHER_APP`].
    #[serde(default)]
    pub tracked: Vec<i64>,
    /// Periods the 24-hour and 7-day sums cover, and how many they would
    /// cover if every period had been counted. The sums are lower bounds
    /// while the first is below the second.
    #[serde(default)]
    pub periods_24h: i64,
    #[serde(default)]
    pub expected_24h: i64,
    #[serde(default)]
    pub periods_7d: i64,
    #[serde(default)]
    pub expected_7d: i64,
    pub apps: Vec<AppFigures>,
}

impl Overview {
    pub fn day_complete(&self) -> bool {
        self.periods_24h >= self.expected_24h
    }

    pub fn week_complete(&self) -> bool {
        self.periods_7d >= self.expected_7d
    }
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
    /// More groups existed than fit; the largest are kept. `rows` is then not
    /// all there is, and the working counts below still are.
    pub truncated: bool,
    /// Accounts and computers holding a task, counted over all of them.
    pub working_users: i64,
    pub working_hosts: i64,
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
    /// The queue length is a lower bound: the count stopped at the cap.
    pub queued_capped: i64,
    pub users: i64,
    pub hosts: i64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RollupReport {
    pub ran: bool,
    /// The pass found too little budget to do anything.
    pub skipped_for_budget: bool,
    /// Starts of the periods recounted.
    pub recounted: Vec<i64>,
    /// Periods that were due and wait for a later pass.
    pub postponed: usize,
    pub apps: usize,
    /// Statements the pass sent to the database.
    pub statements: usize,
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

/// What is known of a counted period: when it was counted and how many tasks
/// were still waiting for validation then.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Counted {
    at: i64,
    pending: i64,
}

/// The periods a pass wants to recount, in priority order: the open one, closed
/// ones that need another count, then up to [`BACKFILL_PER_PASS`] that have
/// never been counted. A closed period that starts before `not_before` is left
/// as it is.
fn due_periods(now: i64, existing: &BTreeMap<i64, Counted>, not_before: i64) -> Vec<i64> {
    let (first, current) = week_bounds(now);
    let mut recount = Vec::new();
    let mut backfill = Vec::new();
    let mut b = current - BUCKET_SECS;
    while b >= first {
        if b >= not_before {
            match existing.get(&b) {
                Some(c) => {
                    let end = b + BUCKET_SECS;
                    // Soon after it closed, so that every return is in; then
                    // hourly for as long as tasks of it wait to be validated,
                    // because a validation changes which count they are in.
                    if c.at < end + EARLY_SECS
                        || (c.pending > 0 && now - c.at >= PENDING_RECHECK_SECS)
                    {
                        recount.push(b);
                    }
                }
                None => {
                    if backfill.len() < BACKFILL_PER_PASS {
                        backfill.push(b);
                    }
                }
            }
        }
        b -= BUCKET_SECS;
    }
    let mut out = vec![current];
    out.extend(recount);
    out.extend(backfill);
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
/// `cloud/docs/architecture.md`); `n` is how many it wrote off. A row made
/// here has `computed_at` 0, which the pages and the sums treat as not counted
/// yet.
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

/// One recounted period: its rows, by tracked application, and the project's.
struct Recount {
    bucket: i64,
    by_slot: BTreeMap<i64, Counts>,
    users: i64,
    hosts: i64,
    pending: i64,
}

/// The row an application's figures go in: its own when it is one of the
/// [`TRACKED_APPS`] the pass follows, [`OTHER_APP`] otherwise.
fn slot(tracked: &BTreeSet<i64>, app: i64) -> i64 {
    if tracked.contains(&app) {
        app
    } else {
        OTHER_APP
    }
}

/// Statements a pass sends, at most, for `periods` periods: every count below
/// is fixed, whatever the number of applications, users or computers.
///
/// - round trip 1: 3 (last run, tracked applications, the window's project rows);
/// - round trip 2: 1 per period, and 6 (the queue, tasks in progress per
///   application, the largest in-progress groups, who holds tasks, workunits
///   awaiting validation and assimilation);
/// - round trip 3: 2 per period (at most 20 rows of 10 to a statement: the
///   project, [`TRACKED_APPS`] applications and the rest), then the queue gauge,
///   the retention delete and the sums, 3;
/// - round trip 4: the three snapshots.
pub const fn statements_for(periods: usize) -> usize {
    FIXED_STATEMENTS + PERIOD_STATEMENTS * periods
}

/// One rollup pass; see the module comment. `not_before` is the earliest time
/// whose tasks may still be in the table: `now` minus the purge delay when
/// workunits are purged, 0 otherwise. `budget` is how many statements the pass
/// may send; a pass given less than [`MIN_BUDGET`] does nothing, and one given
/// more counts [`statements_for`] periods at most, so it never sends more
/// than it was given.
pub async fn rollup_pass(
    db: &dyn Db,
    now: i64,
    not_before: i64,
    budget: usize,
) -> CoreResult<RollupReport> {
    let (first, current) = week_bounds(now);
    let mut report = RollupReport::default();
    if budget < MIN_BUDGET {
        report.skipped_for_budget = true;
        return Ok(report);
    }

    // Round trip 1: is it time, which applications are tracked, which periods
    // have been counted.
    let mut out = db
        .batch(vec![
            stmt!("SELECT at FROM stats_snapshot WHERE name = ?", SNAP_RUN),
            stmt!("SELECT id FROM app ORDER BY id LIMIT ?", TRACKED_APPS),
            stmt!(
                "SELECT bucket, computed_at, pending FROM stats_bucket \
                 WHERE appid = 0 AND bucket >= ? AND computed_at > 0",
                first
            ),
        ])
        .await?
        .into_iter();
    report.statements += 3;
    let last_run = out
        .next()
        .and_then(|r| r.rows.first().map(|row| int(row, "at")))
        .unwrap_or(0);
    if last_run <= now && now - last_run < MIN_INTERVAL_SECS {
        return Ok(report);
    }
    let tracked: BTreeSet<i64> = out
        .next()
        .map(|r| r.rows.iter().map(|row| int(row, "id")).collect())
        .unwrap_or_default();
    let mut existing: BTreeMap<i64, Counted> = BTreeMap::new();
    for row in &out.next().unwrap_or_default().rows {
        existing.insert(
            int(row, "bucket"),
            Counted {
                at: int(row, "computed_at"),
                pending: int(row, "pending"),
            },
        );
    }
    let mut due = due_periods(now, &existing, not_before);

    // As many periods as the budget holds, the open one first. `MIN_BUDGET`
    // holds one.
    let max_periods = (budget - FIXED_STATEMENTS) / PERIOD_STATEMENTS;
    let postponed = due.len().saturating_sub(max_periods);
    due.truncate(max_periods.max(1));

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
    // 0: the queue, in application order, up to the cap.
    reads.push(stmt!(
        format!(
            "SELECT appid, count(*) AS n FROM \
             (SELECT appid FROM result WHERE server_state = {unsent} LIMIT ?) GROUP BY appid"
        ),
        UNSENT_COUNT_CAP + 1
    ));
    // 1: tasks in progress per application.
    reads.push(Stmt::new(format!(
        "SELECT appid, count(*) AS n FROM result WHERE server_state = {in_progress} GROUP BY appid"
    )));
    // 2: the largest groups of tasks in progress.
    reads.push(stmt!(
        format!(
            "SELECT g.hostid, g.appid, g.n, h.userid, h.rpc_time FROM \
             (SELECT hostid, appid, count(*) AS n FROM result \
              WHERE server_state = {in_progress} AND hostid > 0 GROUP BY hostid, appid) g \
             JOIN host h ON h.id = g.hostid ORDER BY g.n DESC, g.hostid, g.appid LIMIT ?"
        ),
        MAX_INFLIGHT_ROWS + 1
    ));
    // 3: accounts and computers holding tasks, over all of them.
    reads.push(Stmt::new(format!(
        "SELECT count(*) AS hosts, count(DISTINCT h.userid) AS users FROM \
         (SELECT DISTINCT hostid FROM result WHERE server_state = {in_progress} AND hostid > 0) g \
         JOIN host h ON h.id = g.hostid"
    )));
    // 4, 5: workunits waiting for validation and for assimilation.
    reads.push(Stmt::new(
        "SELECT appid, count(*) AS n FROM workunit WHERE need_validate = 1 GROUP BY appid",
    ));
    reads.push(Stmt::new(format!(
        "SELECT appid, count(*) AS n FROM workunit WHERE assimilate_state = {ASSIMILATE_READY} \
         GROUP BY appid"
    )));
    report.statements += reads.len();
    let mut rows = db.batch(reads).await?.into_iter();

    // The recount of each due period, by tracked application and the rest.
    let mut recounts: Vec<Recount> = Vec::new();
    for &b in &due {
        let groups = rows.next().unwrap_or_default();
        let mut user_set: BTreeSet<i64> = BTreeSet::new();
        let mut host_set: BTreeSet<i64> = BTreeSet::new();
        let mut by_slot: BTreeMap<i64, Counts> = BTreeMap::new();
        // Every tracked application has a row, with zero when it has no task:
        // a recount overwrites what an earlier one wrote.
        for app in tracked.iter().copied().chain(std::iter::once(OTHER_APP)) {
            by_slot.entry(app).or_default();
        }
        let mut pending = 0;
        for row in &groups.rows {
            let c = by_slot
                .entry(slot(&tracked, int(row, "appid")))
                .or_default();
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
                } else if state == i64::from(VALIDATE_STATE_INIT)
                    || state == i64::from(VALIDATE_STATE_INCONCLUSIVE)
                {
                    pending += n;
                }
            } else {
                c.failed += n;
            }
        }
        recounts.push(Recount {
            bucket: b,
            by_slot,
            users: user_set.len() as i64,
            hosts: host_set.len() as i64,
            pending,
        });
    }

    // The gauges, by tracked application and the rest.
    let mut figures: BTreeMap<i64, AppFigures> = BTreeMap::new();
    let mut total = AppFigures::default();
    let mut counted_unsent = 0;
    let mut last_app = 0;
    let unsent_rows: Vec<(i64, i64)> = rows
        .next()
        .unwrap_or_default()
        .rows
        .iter()
        .map(|r| (int(r, "appid"), int(r, "n")))
        .collect();
    for &(id, n) in &unsent_rows {
        counted_unsent += n;
        last_app = last_app.max(id);
    }
    let truncated = counted_unsent > UNSENT_COUNT_CAP;
    let unsent_from = if truncated { last_app.max(1) } else { 0 };
    let mut overshoot = counted_unsent - UNSENT_COUNT_CAP;
    for &(id, n) in &unsent_rows {
        let mut n = n;
        if truncated && id == last_app {
            // The cap cut the count off inside the last application's queue.
            n = (n - overshoot.max(0)).max(0);
            overshoot = 0;
        }
        figures
            .entry(slot(&tracked, id))
            .or_insert_with(|| fig(slot(&tracked, id)))
            .unsent += n;
    }
    total.unsent = figures.values().map(|f| f.unsent).sum();
    for row in &rows.next().unwrap_or_default().rows {
        let id = slot(&tracked, int(row, "appid"));
        let n = int(row, "n");
        total.in_progress += n;
        figures.entry(id).or_insert_with(|| fig(id)).in_progress += n;
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
    if let Some(row) = rows.next().unwrap_or_default().rows.first() {
        inflight.working_hosts = int(row, "hosts");
        inflight.working_users = int(row, "users");
    }
    for row in &rows.next().unwrap_or_default().rows {
        let id = slot(&tracked, int(row, "appid"));
        let n = int(row, "n");
        total.wait_validate += n;
        figures.entry(id).or_insert_with(|| fig(id)).wait_validate += n;
    }
    for row in &rows.next().unwrap_or_default().rows {
        let id = slot(&tracked, int(row, "appid"));
        let n = int(row, "n");
        total.wait_assimilate += n;
        figures.entry(id).or_insert_with(|| fig(id)).wait_assimilate += n;
    }
    // Every tracked application has a row, with zeros when nothing is going on
    // for it; the page shows these rows and no others (no application gets a
    // row of zeros of its own invention).
    for app in &tracked {
        figures.entry(*app).or_insert_with(|| fig(*app));
    }
    // With applications left over, their row exists when the queue count may
    // have missed some of them.
    if unsent_from > 0 && tracked.len() == TRACKED_APPS {
        figures.entry(OTHER_APP).or_insert_with(|| fig(OTHER_APP));
    }
    // The queue is a lower bound for the application the cap stopped in, every
    // later one, and the row of the rest, which may hold later ones.
    for f in figures.values_mut() {
        f.unsent_capped = unsent_from > 0 && (f.id == OTHER_APP || f.id >= unsent_from);
    }
    total.unsent_capped = unsent_from > 0;

    // Round trip 3: the period rows, then the queue gauge, the retention delete
    // and the sums over the rows.
    let mut writes: Vec<Stmt> = recounts
        .iter()
        .flat_map(|r| period_upserts(r, now))
        .collect();
    writes.push(stmt!(
        "INSERT INTO stats_bucket (appid, bucket, queued, queued_capped) VALUES (0, ?, ?, ?) \
         ON CONFLICT (appid, bucket) DO UPDATE SET queued = excluded.queued, \
         queued_capped = excluded.queued_capped",
        current,
        total.unsent,
        i64::from(truncated)
    ));
    if !existing.contains_key(&current) {
        // The first pass of a period: drop what has aged out.
        writes.push(stmt!(
            "DELETE FROM stats_bucket WHERE bucket < ?",
            current - KEEP_BUCKETS * BUCKET_SECS
        ));
    }
    let day = current - (DAY_BUCKETS as i64 - 1) * BUCKET_SECS;
    // Rows of periods that have been counted only; an application that is not
    // tracked (a timeout made its row) goes in the row of the rest.
    let tracked_list = tracked
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let tracked_case = if tracked_list.is_empty() {
        format!("CASE WHEN appid = 0 THEN 0 ELSE {OTHER_APP} END")
    } else {
        format!("CASE WHEN appid = 0 OR appid IN ({tracked_list}) THEN appid ELSE {OTHER_APP} END")
    };
    writes.push(stmt!(
        format!(
            "SELECT {tracked_case} AS app, \
             coalesce(sum(CASE WHEN bucket >= ? THEN returned_ok ELSE 0 END), 0) AS ok24, \
             coalesce(sum(CASE WHEN bucket >= ? THEN failed + invalid + timed_out ELSE 0 END), 0) AS err24, \
             coalesce(sum(returned_ok), 0) AS ok7, \
             coalesce(sum(failed + invalid + timed_out), 0) AS err7, \
             coalesce(sum(CASE WHEN appid = 0 AND bucket >= ? THEN 1 ELSE 0 END), 0) AS n24, \
             coalesce(sum(CASE WHEN appid = 0 THEN 1 ELSE 0 END), 0) AS n7 \
             FROM stats_bucket WHERE bucket >= ? \
             AND bucket IN (SELECT bucket FROM stats_bucket WHERE appid = 0 AND bucket >= ? AND computed_at > 0) \
             GROUP BY app"
        ),
        day,
        day,
        day,
        first,
        first
    ));
    report.statements += writes.len();
    let mut written = db.batch(writes).await?;
    let sums = written.pop().unwrap_or_default();
    let (mut periods_24h, mut periods_7d) = (0, 0);
    for row in &sums.rows {
        let id = int(row, "app");
        let target = if id == 0 {
            periods_24h = int(row, "n24");
            periods_7d = int(row, "n7");
            &mut total
        } else {
            figures.entry(id).or_insert_with(|| fig(id))
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
        working_users: inflight.working_users,
        working_hosts: inflight.working_hosts,
        unsent_from,
        tracked: tracked.iter().copied().collect(),
        periods_24h,
        // The targets are the whole windows, however much of them can still
        // be counted: a window part of which was purged stays marked short.
        expected_24h: DAY_BUCKETS as i64,
        periods_7d,
        expected_7d: WEEK_BUCKETS as i64,
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
    report.statements += 3;
    report.ran = true;
    report.recounted = recounts.iter().map(|r| r.bucket).collect();
    report.postponed = postponed;
    Ok(report)
}

fn fig(id: i64) -> AppFigures {
    AppFigures {
        id,
        ..Default::default()
    }
}

/// The upserts of one recounted period: the project's row, one per tracked
/// application and one for the rest, at most 20 rows, in statements of at most
/// [`PERIOD_ROWS_PER_STMT`] rows (so at most two). The columns the recount does
/// not own (`timed_out`, `queued`) are left alone.
fn period_upserts(r: &Recount, now: i64) -> Vec<Stmt> {
    let mut project = Counts::default();
    for c in r.by_slot.values() {
        project.ok += c.ok;
        project.valid += c.valid;
        project.invalid += c.invalid;
        project.failed += c.failed;
    }
    // (app, counts, users, hosts, pending)
    let mut rows: Vec<(i64, Counts, i64, i64, i64)> =
        vec![(0, project, r.users, r.hosts, r.pending)];
    rows.extend(r.by_slot.iter().map(|(a, c)| (*a, *c, 0, 0, 0)));
    debug_assert!(rows.len() <= TRACKED_APPS + 2);
    rows.chunks(PERIOD_ROWS_PER_STMT)
        .map(|chunk| {
            let mut sql = String::from(
                "INSERT INTO stats_bucket (appid, bucket, returned_ok, valid, invalid, failed, \
                 users, hosts, pending, computed_at) VALUES ",
            );
            let mut params: Vec<Value> = Vec::new();
            for (i, (app, c, users, hosts, pending)) in chunk.iter().enumerate() {
                if i > 0 {
                    sql.push(',');
                }
                sql.push_str("(?,?,?,?,?,?,?,?,?,?)");
                for v in [
                    *app, r.bucket, c.ok, c.valid, c.invalid, c.failed, *users, *hosts, *pending,
                    now,
                ] {
                    params.push(Value::Int(v));
                }
            }
            sql.push_str(
                " ON CONFLICT (appid, bucket) DO UPDATE SET returned_ok = excluded.returned_ok, \
                 valid = excluded.valid, invalid = excluded.invalid, failed = excluded.failed, \
                 users = excluded.users, hosts = excluded.hosts, pending = excluded.pending, \
                 computed_at = excluded.computed_at",
            );
            Stmt { sql, params }
        })
        .collect()
}

#[derive(Deserialize)]
struct SnapshotRow {
    body: String,
}

/// The overview snapshot and the project-wide history, in one round trip. A
/// snapshot in another format, or none yet, is `None`. A period that has not
/// been counted is `None` in the history, never zeros.
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
                "SELECT bucket, returned_ok, valid, invalid, failed, timed_out, queued, queued_capped, users, hosts \
                 FROM stats_bucket WHERE appid = 0 AND bucket >= ? AND bucket <= ? AND computed_at > 0 \
                 ORDER BY bucket",
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

    fn counted(at: i64, pending: i64) -> Counted {
        Counted { at, pending }
    }

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
    fn due_periods_recount_pending_ones_and_backfill() {
        let now = 2_000_000_000;
        let (first, current) = week_bounds(now);
        // A fresh deployment: the open period and six more, newest first.
        let due = due_periods(now, &BTreeMap::new(), 0);
        assert_eq!(due.len(), 1 + BACKFILL_PER_PASS);
        assert_eq!(due[0], current);
        assert_eq!(due[1], current - BUCKET_SECS);
        // Periods counted long after they closed, with nothing waiting, are final.
        let mut existing = BTreeMap::new();
        let mut b = first;
        while b <= current {
            existing.insert(b, counted(b + BUCKET_SECS + 3600, 0));
            b += BUCKET_SECS;
        }
        assert_eq!(due_periods(now, &existing, 0), vec![current]);
        // One counted just after it closed is counted again.
        let prev = current - BUCKET_SECS;
        existing.insert(prev, counted(current + 60, 0));
        assert_eq!(due_periods(now, &existing, 0), vec![current, prev]);
        // One counted a while after it closed, with tasks still waiting for
        // validation, is counted again an hour after its last count, and
        // again after that, however old it is.
        existing.insert(prev, counted(current + 1200, 5));
        assert_eq!(due_periods(now, &existing, 0), vec![current]);
        assert_eq!(
            due_periods(current + 1200 + 3600, &existing, 0),
            vec![current, prev]
        );
        let old = current - 5 * BUCKET_SECS;
        existing.insert(old, counted(old + BUCKET_SECS + 1200, 2));
        assert!(due_periods(current + 5 * 86400, &existing, 0).contains(&old));
        // Priority: the open one, then recounts, then backfill.
        let mut some = existing.clone();
        some.remove(&(current - 2 * BUCKET_SECS));
        let due = due_periods(current + 1200 + 3600, &some, 0);
        assert_eq!(due[0], current);
        assert_eq!(*due.last().unwrap(), current - 2 * BUCKET_SECS);
    }

    #[test]
    fn periods_before_the_purge_horizon_are_left_alone() {
        let now = 2_000_000_000;
        let (_, current) = week_bounds(now);
        // A period that straddles the horizon is not backfilled.
        let horizon = current - BUCKET_SECS + 100;
        assert_eq!(due_periods(now, &BTreeMap::new(), horizon), vec![current]);
        let horizon = current - BUCKET_SECS;
        assert_eq!(
            due_periods(now, &BTreeMap::new(), horizon),
            vec![current, current - BUCKET_SECS]
        );
        // Nor recounted if it has a row.
        let mut existing = BTreeMap::new();
        existing.insert(current - BUCKET_SECS, counted(current - BUCKET_SECS + 5, 3));
        assert_eq!(
            due_periods(now + 7200, &existing, current - BUCKET_SECS + 1),
            vec![current]
        );
    }
}
