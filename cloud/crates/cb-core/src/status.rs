//! The job and account counts behind `server_status.php` and the operator
//! summary (`GET /admin/v1/summary`). They follow `get_job_status` in
//! upstream `html/user/server_status.php`.
//!
//! A project can hold hundreds of thousands of `result` rows, and a public
//! page must not count them on every request. So everything is computed in one
//! batch of grouped queries, each of which reads an index (the plans are
//! checked by `EXPLAIN QUERY PLAN` in `cloud/crates/cb-core/tests/status.rs`),
//! and stored in the `config` table under [`SNAPSHOT_SETTING`]; a request
//! within [`STATUS_COUNTS_TTL`] of it reads that one row. Upstream does the
//! same with a cache file (`STATUS_PAGE_TTL`, an hour).
//!
//! Two things about the SQL are on purpose. Every state is written into the
//! statement as a number, not bound: a partial index (`res_unsent`,
//! `res_host_inprog`, `res_received`, `wu_assim`, `wu_need_validate`,
//! `wu_transition`) is used only when the query's own `WHERE` shows it
//! applies, and a bound parameter does not. And a term that only the partial
//! index's condition implies (`received_time > 0`) is repeated beside the
//! parameter that does the range.

use std::collections::BTreeMap;

use cb_db::{stmt, Db, Stmt};
use cb_proto::consts::{
    ASSIMILATE_READY, FILE_DELETE_READY, RESULT_OUTCOME_CLIENT_DETACHED,
    RESULT_OUTCOME_CLIENT_ERROR, RESULT_OUTCOME_COULDNT_SEND, RESULT_OUTCOME_DIDNT_NEED,
    RESULT_OUTCOME_NO_REPLY, RESULT_OUTCOME_SUCCESS, RESULT_OUTCOME_VALIDATE_ERROR,
    RESULT_SERVER_STATE_IN_PROGRESS, RESULT_SERVER_STATE_UNSENT, VALIDATE_STATE_INVALID,
    VALIDATE_STATE_VALID,
};
use serde::{Deserialize, Serialize};

use crate::{CoreError, CoreResult, NEVER};

/// `config` row holding the snapshot. `Project::load` ignores names it does
/// not know, so the row is safe beside the settings.
pub const SNAPSHOT_SETTING: &str = "status_counts";
/// `config` row holding the time of the pipeline's last sweep, written by its
/// cron (`record_sweep`).
pub const SWEEP_SETTING: &str = "last_sweep";
/// Seconds a stored snapshot is reused.
pub const STATUS_COUNTS_TTL: i64 = 600;
/// A sweep older than this many seconds means the pipeline is not running. The
/// sweep runs every minute.
pub const SWEEP_STALE_SECS: i64 = 300;
/// Format of the stored snapshot. A snapshot of another version is recomputed.
const SNAPSHOT_VERSION: u32 = 2;
/// The window of the per-application figures, as upstream's `86400`.
const WINDOW_SECS: i64 = 86_400;

/// What the server knows about one application. Times are Unix seconds, 0
/// for none; `*_hours` are upstream's `ceil(x / 3600 * 100) / 100`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AppStatus {
    pub id: i64,
    pub name: String,
    pub user_friendly_name: String,
    pub deprecated: bool,
    /// Tasks ready to send.
    pub unsent: i64,
    pub in_progress: i64,
    /// `create_time` of the oldest unsent task (the lowest ID: IDs grow with
    /// time), 0 when none is unsent.
    pub oldest_unsent_time: i64,
    /// Tasks received in the last 24 hours, with any outcome.
    pub received_24h: i64,
    /// Of those, the ones that have validated.
    pub valid_24h: i64,
    /// Of those, by outcome name (`client_error`, `validate_error`, ...), and
    /// `invalid` for tasks that ran but failed validation.
    pub errors_24h: BTreeMap<String, i64>,
    /// Runtime of the valid ones: `None` when there are none.
    pub avg_hours: Option<f64>,
    pub min_hours: Option<f64>,
    pub max_hours: Option<f64>,
    /// Accounts with a valid task in the window.
    pub users_24h: i64,
    /// Workunits that need validation, and that are ready to assimilate.
    pub wus_need_validate: i64,
    pub wus_need_assimilate: i64,
}

/// The numbers of `server_status.php`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerStatus {
    pub results_ready_to_send: i64,
    pub results_in_progress: i64,
    pub workunits_waiting_for_validation: i64,
    pub workunits_waiting_for_assimilation: i64,
    pub workunits_waiting_for_deletion: i64,
    pub results_waiting_for_deletion: i64,
    /// Workunits whose transition is due now.
    pub transitions_due: i64,
    /// Hours since the earliest pending transition was due, 0 when none is.
    pub transitioner_backlog_hours: f64,
    pub users: i64,
    pub hosts: i64,
    /// Accounts and computers with `expavg_credit` over 1, as upstream
    /// counts "recent credit".
    pub users_with_recent_credit: i64,
    pub hosts_with_recent_credit: i64,
    /// With `total_credit` over 1.
    pub users_with_credit: i64,
    pub hosts_with_credit: i64,
    pub users_past_24_hours: i64,
    pub hosts_past_24_hours: i64,
    /// Current GigaFLOPS: the sum of the accounts' `expavg_credit` over 200.
    pub flops: f64,
    pub apps: Vec<AppStatus>,
    /// When the numbers were computed.
    pub cached_time: i64,
    /// When the pipeline's cron last ran, 0 if it never recorded one.
    pub last_sweep: i64,
}

#[derive(Serialize, Deserialize)]
struct Snapshot {
    v: u32,
    status: ServerStatus,
}

#[derive(Deserialize)]
struct ValueRow {
    value: String,
}

/// The pipeline's cron calls this on each sweep, so that the status can say
/// whether it runs.
pub async fn record_sweep(db: &dyn Db, now: i64) -> CoreResult<()> {
    db.run(stmt!(
        "INSERT INTO config (name, value, mod_time) VALUES (?, ?, ?) \
         ON CONFLICT (name) DO UPDATE SET value = excluded.value, mod_time = excluded.mod_time",
        SWEEP_SETTING,
        now.to_string(),
        now
    ))
    .await?;
    Ok(())
}

/// `ceil(x / 3600 * 100) / 100`, upstream's rounding of hours.
fn hours(seconds: f64) -> f64 {
    (seconds / 3600.0 * 100.0).ceil() / 100.0
}

fn outcome_name(outcome: i32) -> Option<&'static str> {
    Some(match outcome {
        RESULT_OUTCOME_COULDNT_SEND => "couldnt_send",
        RESULT_OUTCOME_CLIENT_ERROR => "client_error",
        RESULT_OUTCOME_NO_REPLY => "no_reply",
        RESULT_OUTCOME_DIDNT_NEED => "didnt_need",
        RESULT_OUTCOME_VALIDATE_ERROR => "validate_error",
        RESULT_OUTCOME_CLIENT_DETACHED => "client_detached",
        _ => return None,
    })
}

/// The statements of one computation, in the order [`build`] reads them.
/// `now` is bound only where a range needs it.
pub fn snapshot_statements(now: i64) -> Vec<Stmt> {
    let unsent = RESULT_SERVER_STATE_UNSENT;
    let in_progress = RESULT_SERVER_STATE_IN_PROGRESS;
    vec![
        // 0: applications.
        Stmt::new("SELECT id, name, user_friendly_name, deprecated FROM app ORDER BY id"),
        // 1: unsent per application, and the creation time of the oldest.
        Stmt::new(format!(
            "SELECT u.appid, u.n, (SELECT create_time FROM result WHERE id = u.first_id) AS oldest \
             FROM (SELECT appid, count(*) AS n, min(id) AS first_id FROM result \
             WHERE server_state = {unsent} GROUP BY appid) u"
        )),
        // 2: in progress per application.
        Stmt::new(format!(
            "SELECT appid, count(*) AS n FROM result WHERE server_state = {in_progress} \
             GROUP BY appid"
        )),
        // 3: what was received in the window, by application and result. The
        // runtime figures are of the valid ones, as upstream's.
        stmt!(
            "SELECT appid, outcome, validate_state, count(*) AS n, sum(elapsed_time) AS et, \
             min(elapsed_time) AS mn, max(elapsed_time) AS mx, count(DISTINCT userid) AS users \
             FROM result WHERE received_time > 0 AND received_time > ? \
             GROUP BY appid, outcome, validate_state",
            now - WINDOW_SECS
        ),
        // 4, 5: workunits by application.
        Stmt::new(
            "SELECT appid, count(*) AS n FROM workunit WHERE need_validate = 1 GROUP BY appid",
        ),
        Stmt::new(format!(
            "SELECT appid, count(*) AS n FROM workunit WHERE assimilate_state = {ASSIMILATE_READY} \
             GROUP BY appid"
        )),
        // 6, 7: files waiting for deletion. A workunit marked ready for
        // deletion is made due at once, so it is among the due transitions,
        // which the index holds and which are few.
        stmt!(
            format!(
                "SELECT count(*) AS n FROM workunit WHERE transition_time < {NEVER} \
                 AND transition_time <= ? AND file_delete_state = {FILE_DELETE_READY}"
            ),
            now
        ),
        stmt!(
            format!(
                "SELECT count(*) AS n FROM result WHERE file_delete_state = {FILE_DELETE_READY} \
                 AND workunitid IN (SELECT id FROM workunit WHERE transition_time < {NEVER} \
                 AND transition_time <= ?)"
            ),
            now
        ),
        // 8: transitions due, and the earliest pending one.
        stmt!(
            format!(
                "SELECT count(*) AS due, min(transition_time) AS earliest FROM workunit \
                 WHERE transition_time < {NEVER} AND transition_time <= ?"
            ),
            now
        ),
        // 9, 10: accounts and computers (these scan their tables).
        stmt!(
            "SELECT count(*) AS n, coalesce(sum(expavg_credit > 1), 0) AS recent, \
             coalesce(sum(total_credit > 1), 0) AS credit, \
             coalesce(sum(create_time > ?), 0) AS new, coalesce(sum(expavg_credit), 0) AS flops \
             FROM user",
            now - WINDOW_SECS
        ),
        stmt!(
            "SELECT count(*) AS n, coalesce(sum(expavg_credit > 1), 0) AS recent, \
             coalesce(sum(total_credit > 1), 0) AS credit, \
             coalesce(sum(create_time > ?), 0) AS new FROM host",
            now - WINDOW_SECS
        ),
    ]
}

fn int(row: &cb_db::Row, col: &str) -> i64 {
    row.get(col)
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        .unwrap_or(0)
}

fn real(row: &cb_db::Row, col: &str) -> f64 {
    row.get(col).and_then(|v| v.as_f64()).unwrap_or(0.0)
}

/// The entry for an application. A row for one that is not in the table (it
/// was removed) still counts toward the totals, under its ID.
fn app_mut(apps: &mut BTreeMap<i64, AppStatus>, id: i64) -> &mut AppStatus {
    apps.entry(id).or_insert_with(|| AppStatus {
        id,
        name: format!("app {id}"),
        user_friendly_name: format!("app {id}"),
        deprecated: true,
        ..Default::default()
    })
}

/// Builds the numbers from the results of [`snapshot_statements`].
fn build(out: &[cb_db::Rows], now: i64) -> ServerStatus {
    let rows = |i: usize| out.get(i).map(|r| r.rows.as_slice()).unwrap_or_default();
    let one = |i: usize| rows(i).first().cloned().unwrap_or_default();
    let mut apps: BTreeMap<i64, AppStatus> = BTreeMap::new();
    for r in rows(0) {
        let id = int(r, "id");
        apps.insert(
            id,
            AppStatus {
                id,
                name: r.get("name").and_then(|v| v.as_str()).unwrap_or("").into(),
                user_friendly_name: r
                    .get("user_friendly_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .into(),
                deprecated: int(r, "deprecated") != 0,
                ..Default::default()
            },
        );
    }
    let mut s = ServerStatus {
        cached_time: now,
        ..Default::default()
    };
    for r in rows(1) {
        let a = app_mut(&mut apps, int(r, "appid"));
        a.unsent = int(r, "n");
        a.oldest_unsent_time = int(r, "oldest");
        s.results_ready_to_send += a.unsent;
    }
    for r in rows(2) {
        let a = app_mut(&mut apps, int(r, "appid"));
        a.in_progress = int(r, "n");
        s.results_in_progress += a.in_progress;
    }
    for r in rows(3) {
        let a = app_mut(&mut apps, int(r, "appid"));
        let (outcome, validate_state, n) = (
            int(r, "outcome") as i32,
            int(r, "validate_state") as i32,
            int(r, "n"),
        );
        a.received_24h += n;
        if validate_state == VALIDATE_STATE_VALID {
            a.valid_24h += n;
            a.users_24h += int(r, "users");
            if n > 0 {
                a.avg_hours = Some(hours(real(r, "et") / n as f64));
                a.min_hours = Some(hours(real(r, "mn")));
                a.max_hours = Some(hours(real(r, "mx")));
            }
        }
        if let Some(name) = outcome_name(outcome) {
            *a.errors_24h.entry(name.into()).or_insert(0) += n;
        } else if outcome == RESULT_OUTCOME_SUCCESS && validate_state == VALIDATE_STATE_INVALID {
            *a.errors_24h.entry("invalid".into()).or_insert(0) += n;
        }
    }
    for r in rows(4) {
        let a = app_mut(&mut apps, int(r, "appid"));
        a.wus_need_validate = int(r, "n");
        s.workunits_waiting_for_validation += a.wus_need_validate;
    }
    for r in rows(5) {
        let a = app_mut(&mut apps, int(r, "appid"));
        a.wus_need_assimilate = int(r, "n");
        s.workunits_waiting_for_assimilation += a.wus_need_assimilate;
    }
    s.workunits_waiting_for_deletion = int(&one(6), "n");
    s.results_waiting_for_deletion = int(&one(7), "n");
    let due = one(8);
    s.transitions_due = int(&due, "due");
    let earliest = int(&due, "earliest");
    s.transitioner_backlog_hours = if earliest > 0 && earliest < now {
        (now - earliest) as f64 / 3600.0
    } else {
        0.0
    };
    let u = one(9);
    s.users = int(&u, "n");
    s.users_with_recent_credit = int(&u, "recent");
    s.users_with_credit = int(&u, "credit");
    s.users_past_24_hours = int(&u, "new");
    s.flops = real(&u, "flops") / 200.0;
    let h = one(10);
    s.hosts = int(&h, "n");
    s.hosts_with_recent_credit = int(&h, "recent");
    s.hosts_with_credit = int(&h, "credit");
    s.hosts_past_24_hours = int(&h, "new");
    s.apps = apps.into_values().collect();
    s
}

/// The status, from the stored snapshot when it is at most `max_age` seconds
/// old, else computed and stored. With `max_age` 0 it is always computed.
/// A request that finds the snapshot expired computes it itself, so several
/// that arrive together each do, once per expiry.
pub async fn server_status_within(db: &dyn Db, now: i64, max_age: i64) -> CoreResult<ServerStatus> {
    let out = db
        .batch(vec![
            stmt!("SELECT value FROM config WHERE name = ?", SNAPSHOT_SETTING),
            stmt!("SELECT value FROM config WHERE name = ?", SWEEP_SETTING),
        ])
        .await?;
    let last_sweep = match out.get(1).cloned() {
        Some(rows) => rows
            .first::<ValueRow>()?
            .and_then(|r| r.value.trim().parse::<i64>().ok())
            .unwrap_or(0),
        None => 0,
    };
    let cached = match out.first().cloned() {
        Some(rows) => rows
            .first::<ValueRow>()?
            .and_then(|r| serde_json::from_str::<Snapshot>(&r.value).ok()),
        None => None,
    };
    if let Some(snap) = cached {
        let age = now - snap.status.cached_time;
        if snap.v == SNAPSHOT_VERSION
            && max_age > 0
            && snap.status.cached_time <= now
            && age < max_age
        {
            return Ok(ServerStatus {
                last_sweep,
                ..snap.status
            });
        }
    }
    let computed = db.batch(snapshot_statements(now)).await?;
    let mut status = build(&computed, now);
    let json = serde_json::to_string(&Snapshot {
        v: SNAPSHOT_VERSION,
        status: status.clone(),
    })
    .map_err(|e| CoreError::Invalid(e.to_string()))?;
    db.run(stmt!(
        "INSERT INTO config (name, value, mod_time) VALUES (?, ?, ?) \
         ON CONFLICT (name) DO UPDATE SET value = excluded.value, mod_time = excluded.mod_time",
        SNAPSHOT_SETTING,
        json,
        now
    ))
    .await?;
    status.last_sweep = last_sweep;
    Ok(status)
}

/// [`server_status_within`] with the default age, [`STATUS_COUNTS_TTL`].
pub async fn server_status(db: &dyn Db, now: i64) -> CoreResult<ServerStatus> {
    server_status_within(db, now, STATUS_COUNTS_TTL).await
}

/// What `cboinc summary` and `GET /admin/v1/summary` show: one entry per
/// application with the figures an operator or a monitor polls.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct OperatorSummary {
    /// When the figures were computed; they are at most the requested age old.
    pub as_of: i64,
    /// Seconds since `as_of` at the time of the reply.
    pub age_secs: i64,
    pub pipeline: PipelineHealth,
    pub apps: Vec<AppSummary>,
    pub totals: SummaryTotals,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PipelineHealth {
    /// Unix time of the last sweep of the pipeline's cron, 0 if never recorded.
    pub last_sweep: i64,
    /// Seconds since then; null when none was recorded.
    pub sweep_age_secs: Option<i64>,
    /// `false` when a sweep was recorded and is older than [`SWEEP_STALE_SECS`].
    pub running: bool,
    pub transitions_due: i64,
    pub transitioner_backlog_hours: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct AppSummary {
    pub id: i64,
    pub name: String,
    pub deprecated: bool,
    pub unsent: i64,
    pub in_progress: i64,
    /// Seconds since the oldest unsent task was created; null when none is unsent.
    pub oldest_unsent_age_secs: Option<i64>,
    pub received_24h: i64,
    pub validated_24h: i64,
    pub errors_24h: BTreeMap<String, i64>,
    pub workunits_awaiting_validation: i64,
    pub workunits_awaiting_assimilation: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SummaryTotals {
    pub unsent: i64,
    pub in_progress: i64,
    pub received_24h: i64,
    pub validated_24h: i64,
    pub workunits_awaiting_validation: i64,
    pub workunits_awaiting_assimilation: i64,
}

/// Whether the pipeline's cron is running, as far as its sweeps say.
pub fn pipeline_running(last_sweep: i64, now: i64) -> bool {
    last_sweep == 0 || now - last_sweep <= SWEEP_STALE_SECS
}

impl OperatorSummary {
    pub fn from_status(s: &ServerStatus, now: i64) -> OperatorSummary {
        let apps: Vec<AppSummary> = s
            .apps
            .iter()
            .map(|a| AppSummary {
                id: a.id,
                name: a.name.clone(),
                deprecated: a.deprecated,
                unsent: a.unsent,
                in_progress: a.in_progress,
                oldest_unsent_age_secs: (a.oldest_unsent_time > 0)
                    .then(|| (now - a.oldest_unsent_time).max(0)),
                received_24h: a.received_24h,
                validated_24h: a.valid_24h,
                errors_24h: a.errors_24h.clone(),
                workunits_awaiting_validation: a.wus_need_validate,
                workunits_awaiting_assimilation: a.wus_need_assimilate,
            })
            .collect();
        OperatorSummary {
            as_of: s.cached_time,
            age_secs: (now - s.cached_time).max(0),
            pipeline: PipelineHealth {
                last_sweep: s.last_sweep,
                sweep_age_secs: (s.last_sweep > 0).then(|| (now - s.last_sweep).max(0)),
                running: pipeline_running(s.last_sweep, now),
                transitions_due: s.transitions_due,
                transitioner_backlog_hours: s.transitioner_backlog_hours,
            },
            totals: SummaryTotals {
                unsent: s.results_ready_to_send,
                in_progress: s.results_in_progress,
                received_24h: apps.iter().map(|a| a.received_24h).sum(),
                validated_24h: apps.iter().map(|a| a.validated_24h).sum(),
                workunits_awaiting_validation: s.workunits_waiting_for_validation,
                workunits_awaiting_assimilation: s.workunits_waiting_for_assimilation,
            },
            apps,
        }
    }
}

/// The summary, from a snapshot at most `max_age` seconds old.
pub async fn operator_summary(db: &dyn Db, now: i64, max_age: i64) -> CoreResult<OperatorSummary> {
    let s = server_status_within(db, now, max_age).await?;
    Ok(OperatorSummary::from_status(&s, now))
}
