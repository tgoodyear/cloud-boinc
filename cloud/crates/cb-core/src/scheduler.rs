//! The scheduler RPC: authenticate, record reported results, send work.
//!
//! This is `process_request` of `sched/handle_request.cpp` and the parts of
//! `sched/sched_send.cpp`, `sched_version.cpp`, `sched_resend.cpp` and
//! `sched_result.cpp` it calls, arranged as a few read batches and one write
//! batch. There is no feeder and no shared-memory job cache: candidates are
//! read from the index on unsent results and claimed with a conditional
//! UPDATE, so two requests cannot be sent the same result.
//!
//! Assigned (targeted) jobs, with `enable_assignment`, are sent first, as
//! `send_targeted_jobs` in `sched/sched_assign.cpp` does. Their results are
//! created ahead of time in the reserved state (`RESULT_SERVER_STATE_INACTIVE`)
//! by work submission and the pipeline, and a request claims one for its host
//! as it claims an unsent result; see `crates/cb-core/src/pipeline.rs`.
//!
//! A client on the anonymous platform (`<platform_name>anonymous</platform_name>`,
//! with an `app_info.xml`) lists its own app versions in the request. For it
//! the versions to choose among are the client's, as in
//! `get_app_version_anonymous` of `sched/sched_version.cpp`; the reply carries
//! no `<app>` or `<app_version>`; and the result is recorded with the
//! negative `app_version_id` of the resource type it uses and a
//! `host_app_version` row keyed by `anon_gavid`. See "Anonymous platform" in
//! `docs/architecture.md`.
//!
//! Homogeneous redundancy (HR) and homogeneous app version (HAV) are done as
//! in `sched/sched_send.cpp`, `sched/sched_check.cpp` and
//! `sched/sched_version.cpp`: the candidate query leaves out workunits
//! committed to another class or version, and the claim of a result commits
//! the workunit to the host's class and version in the same transaction, with
//! a guard that the workunit is still uncommitted or committed to the same.
//! See "Homogeneous redundancy and homogeneous app version" in
//! `docs/architecture.md` and `crates/cb-core/src/hr.rs`.
//!
//! Not done here: assigned jobs for teams and broadcast (`multi`)
//! assignments, locality scheduling, and the client workload simulation
//! behind upstream's deadline check.

use std::collections::{HashMap, HashSet};

use cb_crypto::account::{check_weak_auth, email_hash, host_cpid, user_cpid, weak_auth_user_id};
use cb_db::{stmt, Db, Stmt};
use cb_proto::consts::*;
use cb_proto::sched_reply::{
    insert_deadline_tag, insert_name_tags, insert_wu_tags, ReplyApp, ReplyAppVersion, ReplyHost,
    ReplyResult, ReplyUser, SchedulerReply, UserMessage, WuTags,
};
use cb_proto::sched_request::{self, ClientAppVersion, ReportedResult, SchedulerRequest};
use cb_proto::web_rpc::err::EXIT_ABORTED_BY_PROJECT;
use cb_proto::xml::{extract_venue, line};
use serde::Deserialize;

use crate::host_misc::host_info_json;
use crate::hr::{app_hr_type, hr_class, hr_unknown_class, HrHost};
use crate::model::{
    anon_gavid, anon_platform_id, anon_proc_type, anon_uses_gpu, generalized_app_version_id, App,
    AppVersion, Host, HostAppVersion, Platform, User, Workunit,
};
use crate::plan_class::{gpu_peak_flops, HostUsage, PlanClassSpecs, PlanHost, DEFAULT_FPOPS};
use crate::site_delete::{account_guard, is_guard_failure};
use crate::template::output_files;
use crate::{Config, CoreError, CoreResult};

/// Number of processor types the work request distinguishes.
const NPROC: usize = 5;
/// Most names in one `IN (...)` list; D1 allows 100 bound parameters.
const MAX_IN_LIST: usize = 90;
/// Most results sent in one reply, whatever the configuration says.
const MAX_SEND: usize = 100;
/// Most lost results looked at in one request.
const MAX_LOST: usize = 50;
/// Most assigned jobs looked at, and so sent, in one request.
const MAX_ASSIGNED: usize = 20;
/// Most windows of unsent results read for one app in one request.
const MAX_WINDOWS: usize = 4;
/// Most reported results handled in one request when the project sets no
/// `max_results_accepted`. The client reports the rest next time.
const MAX_REPORTED: usize = 500;
/// Longest stderr text and output description stored for a result.
const MAX_BLOB: usize = 64 * 1024;
/// Most entries of the client's `<app_versions>` list looked at on the
/// anonymous platform. Upstream has no limit; this keeps the work on one
/// request bounded.
const MAX_CLIENT_VERSIONS: usize = 256;
/// `ET_RATIO_LIMIT` in `sched/sched_version.cpp`: how far above the speed
/// the client states the speed estimated from elapsed times may go.
const ET_RATIO_LIMIT: f64 = 250.0;
/// Bounds on the speed a client may state for one of its own versions, and
/// on the speed taken from its history. They keep the ratio between the two,
/// which scales the workunit's size in the reply, between 1e-11 and 1e11, so
/// that it can never make that size zero, negative or infinite. Real CPUs and
/// GPUs, even several GPUs in one version, lie well within them.
const MIN_ANON_FLOPS: f64 = 1e6;
const MAX_ANON_FLOPS: f64 = 1e17;
/// Daily limit used when the project sets no `daily_result_quota`.
const NO_QUOTA: i32 = 1_000_000;
const GIGA: f64 = 1024.0 * 1024.0 * 1024.0;
const MEGA: f64 = 1024.0 * 1024.0;

/// The platform name of a client with an `app_info.xml`
/// (`is_anonymous` in `sched/sched_send.h`).
pub const ANONYMOUS_PLATFORM: &str = "anonymous";

const DELAY_MISSING_KEY: f64 = 3600.0;
const DELAY_BAD_CLIENT_VERSION: f64 = 86400.0;
const DELAY_PLATFORM_UNSUPPORTED: f64 = 86400.0;
const DELAY_NO_HOST: f64 = 86400.0;
const DELAY_BAD_REQUEST: f64 = 600.0;

/// `MIN_HOST_SAMPLES` and `MIN_VERSION_SAMPLES` in `sched/credit.h`.
const MIN_HOST_SAMPLES: f64 = 10.0;
const MIN_VERSION_SAMPLES: f64 = 100.0;
/// `CONS_VALID_UNREPLICATED` in `sched/sched_config.h`.
const CONS_VALID_UNREPLICATED: i32 = 10;

/// Platforms, apps and app versions: the tables the scheduler reads on every
/// request and that change only when the project is administered. A Worker
/// keeps one of these per isolate for a few seconds.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub platforms: Vec<Platform>,
    pub apps: Vec<App>,
    pub app_versions: Vec<AppVersion>,
    /// Deprecated versions of apps that use homogeneous app version. A
    /// workunit committed to one is still sent with it, as upstream does
    /// (`check_homogeneous_app_version` in `sched/sched_version.cpp`).
    pub retired_versions: Vec<AppVersion>,
}

impl Catalog {
    /// Every version not deprecated, and the deprecated versions of apps that
    /// are not deprecated and use homogeneous app version.
    pub async fn load(db: &dyn Db) -> CoreResult<Catalog> {
        let mut out = db
            .batch(vec![
                Stmt::new("SELECT * FROM platform WHERE deprecated = 0 ORDER BY id"),
                Stmt::new("SELECT * FROM app WHERE deprecated = 0 ORDER BY id"),
                Stmt::new(
                    "SELECT * FROM app_version WHERE deprecated = 0 OR appid IN \
                     (SELECT id FROM app WHERE deprecated = 0 AND homogeneous_app_version != 0) \
                     ORDER BY id",
                ),
            ])
            .await?
            .into_iter();
        let platforms = out.next().unwrap_or_default().decode()?;
        let apps = out.next().unwrap_or_default().decode()?;
        let (retired_versions, app_versions) = out
            .next()
            .unwrap_or_default()
            .decode::<AppVersion>()?
            .into_iter()
            .partition(|v| v.deprecated != 0);
        Ok(Catalog {
            platforms,
            apps,
            app_versions,
            retired_versions,
        })
    }

    fn app(&self, id: i64) -> Option<&App> {
        self.apps.iter().find(|a| a.id == id)
    }

    fn app_version(&self, id: i64) -> Option<&AppVersion> {
        self.app_versions
            .iter()
            .chain(self.retired_versions.iter())
            .find(|v| v.id == id)
    }

    /// Whether a newer version exists for the same app, platform and plan
    /// class. Only the newest is sent, as the upstream feeder arranges.
    fn superseded(&self, av: &AppVersion) -> bool {
        self.app_versions.iter().any(|o| {
            o.appid == av.appid
                && o.platformid == av.platformid
                && o.plan_class == av.plan_class
                && o.version_num > av.version_num
        })
    }
}

/// What one request needs from its caller.
pub struct SchedCtx<'a> {
    pub db: &'a dyn Db,
    pub config: &'a Config,
    pub plan_classes: &'a PlanClassSpecs,
    pub catalog: &'a Catalog,
    /// The project's code-signing public key, as text; empty when unset.
    pub code_sign_key: &'a str,
    pub now: i64,
    pub random: &'a dyn Fn() -> u32,
    /// The address the request came from.
    pub client_ip: &'a str,
}

/// The reply, and what the caller does next.
#[derive(Debug, Default)]
pub struct SchedOutcome {
    pub reply: SchedulerReply,
    /// Workunits to put on the transition queue.
    pub transition: Vec<i64>,
    pub host_id: i64,
    pub user_id: i64,
    pub n_sent: usize,
    pub n_reported: usize,
    pub n_resent: usize,
    /// One line for the request log.
    pub log: String,
}

#[derive(Debug, Deserialize)]
struct ReportedRow {
    id: i64,
    workunitid: i64,
    server_state: i32,
    outcome: i32,
    userid: i64,
    received_time: i64,
    sent_time: i64,
    app_version_id: i64,
    appid: i64,
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct InProgressRow {
    id: i64,
    name: String,
    workunitid: i64,
    app_version_id: i64,
    flops_estimate: f64,
}

/// Why a job was passed over, for the message when nothing is sent.
#[derive(Default)]
struct Skipped {
    quota: bool,
    in_progress_limit: bool,
    memory: bool,
    disk: bool,
    speed: bool,
    /// A workunit was passed over for being committed to another HR class.
    hr_class: bool,
}

#[derive(Debug, Deserialize)]
struct OtherRow {
    name: String,
    server_state: i32,
    outcome: i32,
    error_mask: i32,
    assimilate_state: i32,
}

#[derive(Debug, Deserialize)]
struct Candidate {
    result_id: i64,
    result_name: String,
    xml_doc_in: String,
    #[serde(default)]
    result_priority: i32,
    /// Set when the row is a result already in progress (a resend).
    #[serde(default)]
    report_deadline: i64,
    #[serde(default)]
    sent_time: i64,
    #[serde(flatten)]
    wu: Workunit,
}

/// A row of `user` with the name of the account's team, from the join of the
/// first read (`team_name` is null when `teamid` is 0, or the team is gone).
#[derive(Debug, Deserialize)]
struct UserRow {
    #[serde(flatten)]
    user: User,
    #[serde(default)]
    team_name: Option<String>,
}

/// Selects an account and its team's name: the join looks the team up by its
/// ID, which an account with no team (`teamid` 0) does not have, so the team
/// costs that read nothing. Upstream reads the team in `handle_request`
/// (`team.lookup_id`) and writes it in `SCHEDULER_REPLY::write`.
const USER_WITH_TEAM: &str =
    "SELECT u.*, t.name AS team_name FROM user u LEFT JOIN team t ON t.id = u.teamid";

#[derive(Debug, Deserialize)]
struct WuIdRow {
    workunitid: i64,
}

const CANDIDATE_COLS: &str = "r.id AS result_id, r.name AS result_name, r.xml_doc_in, \
     r.priority AS result_priority, r.report_deadline, r.sent_time, w.*";

/// The parts of the user's project preferences the scheduler reads
/// (`PROJECT_PREFS::parse`).
#[derive(Debug, Default)]
struct ProjectPrefs {
    app_ids: Vec<i64>,
    allow_beta_work: bool,
    no_proc: [bool; NPROC],
    max_jobs_in_progress: i32,
}

impl ProjectPrefs {
    fn parse(xml: &str, venue: &str) -> ProjectPrefs {
        let buf = extract_venue(xml, venue);
        let mut p = ProjectPrefs::default();
        let mut rest = buf.as_str();
        while let Some(at) = rest.find("<app_id>") {
            rest = &rest[at..];
            if let Some(id) = line::parse_int(rest, "<app_id>") {
                if id > 0 {
                    p.app_ids.push(i64::from(id));
                }
            }
            rest = &rest["<app_id>".len()..];
        }
        let flag = |tag: &str| line::parse_bool(&buf, tag).unwrap_or(false);
        p.allow_beta_work = flag("allow_beta_work");
        p.no_proc[PROC_TYPE_CPU as usize] = flag("no_cpu");
        p.no_proc[PROC_TYPE_NVIDIA_GPU as usize] = flag("no_cuda");
        p.no_proc[PROC_TYPE_AMD_GPU as usize] = flag("no_ati");
        p.no_proc[PROC_TYPE_INTEL_GPU as usize] = flag("no_intel_gpu");
        p.no_proc[PROC_TYPE_APPLE_GPU as usize] = flag("no_apple_gpu");
        p.max_jobs_in_progress = line::parse_int(&buf, "<max_jobs>").unwrap_or(0).max(0);
        p
    }
}

/// An app version this host can run, with what it would use. On the
/// anonymous platform it is one of the client's own versions.
struct Version<'a> {
    /// The project's app version; `None` for one of the client's.
    av: Option<&'a AppVersion>,
    appid: i64,
    /// What a result sent with this version stores as `app_version_id`: the
    /// version's ID, or the negative resource type for the anonymous platform.
    result_av_id: i64,
    /// The key of the version's `host_app_version` row.
    gavid: i64,
    version_num: i32,
    plan_class: &'a str,
    platform: &'a str,
    /// Where the version's platform is in the client's list: 0 for the primary
    /// platform, 1 for the first alternate, and so on. `get_app_version` scans the
    /// platforms in that order. Always 0 for a version of the client's own.
    rank: usize,
    /// For an anonymous-platform version, how far the client's estimate of
    /// its speed is from ours (`CLIENT_APP_VERSION::rsc_fpops_scale`).
    fpops_scale: f64,
    hu: HostUsage,
    /// Counts against the GPU limit on jobs in progress.
    gpu: bool,
    /// Sort key: projected speed with the selection noise applied.
    score: f64,
    /// A version the host is sent only workunits committed to it: a
    /// deprecated or superseded version, or one below the app's minimum, of
    /// an app that uses homogeneous app version.
    pinned_only: bool,
    /// A version stock does not load at all (`SCHED_SHMEM::scan_tables`):
    /// deprecated by hand, below the app's `min_version`, or superseded when
    /// there is no `min_version`. It is kept only for workunits already
    /// committed to it under homogeneous app version
    /// (`check_homogeneous_app_version`), never for a new workunit, pinned
    /// to its number or not.
    commit_only: bool,
}

/// What a request keeps for one `host_app_version` row. Several versions of
/// the client can share a row (one app, one resource), and a job sent with
/// any of them counts against it.
struct RowState {
    /// Jobs sent to the host today with versions of the row, including this
    /// reply.
    n_today: i32,
    /// The host's daily limit for the row, scaled by the processors.
    quota: i32,
    consecutive_valid: i32,
    /// Whether the row exists, or an insert for it is in this request.
    has_row: bool,
}

impl Version<'_> {
    fn anonymous(&self) -> bool {
        self.av.is_none()
    }

    /// What the reply needs to know about the version.
    fn sending(&self) -> Sending {
        Sending {
            av_id: self.av.map_or(0, |a| a.id),
            result_av_id: self.result_av_id,
            gavid: self.gavid,
            anon: self.anonymous(),
            version_num: self.version_num,
            plan_class: self.plan_class.to_string(),
            platform: self.platform.to_string(),
            fpops_scale: self.fpops_scale,
            hu: self.hu.clone(),
        }
    }
}

/// Whether a version that is no longer sent to everyone stays loaded for the
/// workunits pinned to it. `SCHED_SHMEM::scan_tables` in
/// `sched/sched_shmem.cpp` loads every version at or above the app's
/// `min_version` when that is set, and flags the ones that are not the latest
/// of their platform and plan class; without it only the latest are loaded.
/// An app with homogeneous app version also keeps the versions its committed
/// workunits are on, whatever their number
/// (`check_homogeneous_app_version` in `sched/sched_version.cpp`).
fn keeps_version(app: &App, av: &AppVersion) -> bool {
    app.homogeneous_app_version != 0 || (app.min_version > 0 && av.version_num >= app.min_version)
}

/// What homogeneous redundancy and homogeneous app version ask of one app
/// for the host of this request.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Homogeneous {
    /// The app's HR type (`app_hr_type`); 0 for none.
    hr_type: i32,
    /// The host's class under that type. Set whenever `hr_type` is, even for
    /// a host the type cannot place, as upstream's `hr_class` is.
    host_class: i32,
    /// The type cannot place the host (`hr_unknown_class`): it gets no
    /// unsent work of the app.
    unknown: bool,
    /// The app uses homogeneous app version.
    hav: bool,
}

impl Homogeneous {
    fn of(app: &App, config: &Config, host: &HrHost<'_>) -> Homogeneous {
        let hr_type = app_hr_type(app, config);
        Homogeneous {
            hr_type,
            host_class: if hr_type != 0 {
                hr_class(host, hr_type)
            } else {
                0
            },
            unknown: hr_type != 0 && hr_unknown_class(host, hr_type),
            hav: app.homogeneous_app_version != 0,
        }
    }

    /// `already_sent_to_different_hr_class` in `sched/sched_hr.cpp`: whether
    /// the workunit is committed to a class other than the host's.
    fn other_class(&self, wu: &Workunit) -> bool {
        self.hr_type != 0 && wu.hr_class != 0 && wu.hr_class != self.host_class
    }

    /// What a claim of a result of the workunit commits it to, with the
    /// version the result is sent with (`update_wu_on_send` in
    /// `sched/sched_send.cpp`).
    fn commit(&self, version: &Version<'_>) -> Commit {
        Commit {
            hr_class: (self.hr_type != 0).then_some(self.host_class),
            app_version_id: version.av.filter(|_| self.hav).map(|a| a.id),
        }
    }
}

/// What sending a result commits its workunit to. A part is `None` when the
/// app does not use that feature.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Commit {
    hr_class: Option<i32>,
    app_version_id: Option<i64>,
}

impl Commit {
    /// The claim's condition: the workunit is not committed, or is committed
    /// to the same class and version. Evaluated when the claim runs, so two
    /// hosts of different classes cannot both be sent a result of a workunit
    /// that neither was committed to when they chose it.
    fn guard(&self, wu_id: i64) -> Option<(String, Vec<cb_db::Value>)> {
        if self.hr_class.is_none() && self.app_version_id.is_none() {
            return None;
        }
        let mut sql = String::from(" AND EXISTS (SELECT 1 FROM workunit WHERE id = ?");
        let mut params: Vec<cb_db::Value> = vec![wu_id.into()];
        if let Some(c) = self.hr_class {
            sql.push_str(" AND hr_class IN (0, ?)");
            params.push(c.into());
        }
        if let Some(v) = self.app_version_id {
            sql.push_str(" AND app_version_id IN (0, ?)");
            params.push(v.into());
        }
        sql.push(')');
        Some((sql, params))
    }

    /// The same condition on the workunit row itself, for the update that
    /// follows a claim. `CLAIMED` is true for any request of the same host
    /// that claimed the result in the same second, so when two requests of one
    /// host chose different classes or versions for one result, the loser's
    /// update would pass it and overwrite the winner's commitment.
    fn row_guard(&self) -> (String, Vec<cb_db::Value>) {
        let mut sql = String::new();
        let mut params: Vec<cb_db::Value> = Vec::new();
        if let Some(c) = self.hr_class {
            sql.push_str(" AND hr_class IN (0, ?)");
            params.push(c.into());
        }
        if let Some(v) = self.app_version_id {
            sql.push_str(" AND app_version_id IN (0, ?)");
            params.push(v.into());
        }
        (sql, params)
    }

    /// The assignments the workunit update that follows a claim adds, as
    /// `, hr_class = ?` style text to append to a `SET` list.
    fn set(&self) -> (String, Vec<cb_db::Value>) {
        let mut sql = String::new();
        let mut params: Vec<cb_db::Value> = Vec::new();
        if let Some(c) = self.hr_class {
            sql.push_str(", hr_class = ?");
            params.push(c.into());
        }
        if let Some(v) = self.app_version_id {
            sql.push_str(", app_version_id = ?");
            params.push(v.into());
        }
        (sql, params)
    }
}

/// The app version a result is sent with.
struct Sending {
    /// The project's app version; 0 on the anonymous platform.
    av_id: i64,
    /// What the result stores as `app_version_id`.
    result_av_id: i64,
    /// The key of the `host_app_version` row.
    gavid: i64,
    anon: bool,
    version_num: i32,
    plan_class: String,
    platform: String,
    fpops_scale: f64,
    hu: HostUsage,
}

/// A result going into the reply.
struct Outgoing {
    result_name: String,
    xml_doc_in: String,
    wu: Workunit,
    version: Sending,
    deadline: i64,
    /// Index of the claim statement in the write batch; `None` for a resend.
    claim: Option<usize>,
    /// An assigned job.
    assigned: bool,
}

/// Limits a claim repeats against the database, so that two requests from
/// one host at once cannot both pass them.
struct ClaimLimits<'a> {
    /// The in-progress limit of the version's resource; 0 for none.
    resource: i64,
    gpu: bool,
    /// The GPU app version IDs, as an SQL list.
    gpu_ids: &'a str,
    /// The volunteer's limit on jobs in progress; 0 for none.
    total: i64,
    /// The host's daily limit for the version.
    quota: i32,
}

/// The conditional UPDATE that sends a result to a host.
struct Claim<'a> {
    result_id: i64,
    wu_id: i64,
    host_id: i64,
    user_id: i64,
    team_id: i64,
    now: i64,
    deadline: i64,
    /// Stored as `result.app_version_id`.
    result_av_id: i64,
    /// Stored as `result.app_version_num`; 0 on the anonymous platform, as
    /// upstream's `mark_as_sent` leaves it.
    version_num: i32,
    /// The key of the `host_app_version` row the daily quota is read from.
    gavid: i64,
    peak_flops: f64,
    /// The state the result must still be in: unsent, or reserved for an
    /// assignment.
    from_state: i32,
    limits: Option<ClaimLimits<'a>>,
    /// No result of the workunit may already have this `(column, value)`:
    /// the replica rule, or one instance per host of an assigned job.
    one_per: Option<(&'static str, i64)>,
    /// What the workunit is committed to by this send; the claim fails if it
    /// is committed to anything else.
    commit: Commit,
}

impl Claim<'_> {
    fn stmt(&self) -> Stmt {
        let mut sql = String::from(
            "UPDATE result SET server_state = 4, hostid = ?, userid = ?, teamid = ?, \
             sent_time = ?, report_deadline = ?, app_version_id = ?, app_version_num = ?, \
             flops_estimate = ?, mod_time = ? WHERE id = ? AND server_state = ?",
        );
        let mut params: Vec<cb_db::Value> = vec![
            self.host_id.into(),
            self.user_id.into(),
            self.team_id.into(),
            self.now.into(),
            self.deadline.into(),
            self.result_av_id.into(),
            self.version_num.into(),
            self.peak_flops.into(),
            self.now.into(),
            self.result_id.into(),
            self.from_state.into(),
        ];
        if let Some(l) = &self.limits {
            if l.resource > 0 {
                // An anonymous-platform result runs on the GPU when its
                // negative version ID names one (`anon_uses_gpu`).
                sql.push_str(&format!(
                    " AND (SELECT count(*) FROM result WHERE hostid = ? AND server_state = 4 \
                     AND {}) < ?",
                    if l.gpu {
                        format!("(app_version_id IN ({}) OR app_version_id <= {ANON_PLATFORM_NVIDIA})", l.gpu_ids)
                    } else {
                        format!("(app_version_id NOT IN ({}) AND app_version_id > {ANON_PLATFORM_NVIDIA})", l.gpu_ids)
                    }
                ));
                params.push(self.host_id.into());
                params.push(l.resource.into());
            }
            if l.total > 0 {
                sql.push_str(
                    " AND (SELECT count(*) FROM result WHERE hostid = ? AND server_state = 4) < ?",
                );
                params.push(self.host_id.into());
                params.push(l.total.into());
            }
            sql.push_str(
                " AND coalesce((SELECT n_jobs_today FROM host_app_version \
                 WHERE host_id = ? AND app_version_id = ?), 0) < ?",
            );
            params.push(self.host_id.into());
            params.push(self.gavid.into());
            params.push(l.quota.into());
        }
        if let Some((column, who)) = self.one_per {
            sql.push_str(&format!(
                " AND NOT EXISTS (SELECT 1 FROM result r2 WHERE r2.workunitid = ? \
                 AND r2.{column} = ?)"
            ));
            params.push(self.wu_id.into());
            params.push(who.into());
        }
        if let Some((guard, p)) = self.commit.guard(self.wu_id) {
            sql.push_str(&guard);
            params.extend(p);
        }
        sql.push_str(" RETURNING id");
        Stmt { sql, params }
    }
}

fn unit(random: &dyn Fn() -> u32) -> f64 {
    f64::from(random()) / 4_294_967_296.0
}

/// A standard normal deviate (Box-Muller).
fn normal(random: &dyn Fn() -> u32) -> f64 {
    let u1 = unit(random).max(1e-12);
    let u2 = unit(random);
    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

fn clean(x: f64) -> f64 {
    if x.is_finite() && x > 0.0 {
        x
    } else {
        0.0
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn in_list(n: usize) -> String {
    vec!["?"; n].join(",")
}

fn proto(e: cb_proto::ProtoError) -> CoreError {
    CoreError::Xml(e.to_string())
}

fn proc_index(proc_type: i32) -> usize {
    usize::try_from(proc_type)
        .ok()
        .filter(|i| *i < NPROC)
        .unwrap_or(0)
}

/// `warn_user_if_core_client_upgrade_scheduled` in `sched/handle_request.cpp`:
/// tell a client below the announced minimum how long it has to upgrade.
fn warn_if_upgrade_scheduled(
    out: &mut SchedOutcome,
    req: &SchedulerRequest,
    config: &Config,
    now: i64,
) {
    let announced = config.min_core_client_version_announced;
    if req.core_client_version >= announced {
        return;
    }
    // Hours left, as upstream's integer arithmetic gives them.
    let remaining = (config.min_core_client_upgrade_deadline - now) / 3600;
    if remaining <= 0 {
        return;
    }
    let (days, hours) = (remaining / 24, remaining % 24);
    let message = format!(
        "In {days} days and {hours} hours, this project will require a minimum \
         BOINC version of {}.{}.{}.  You are currently using \
         version {}.{}.{}; please upgrade before this time.",
        announced / 10000,
        (announced / 100) % 100,
        announced % 100,
        req.core_client_major_version,
        req.core_client_minor_version,
        req.core_client_release,
    );
    // Low priority until three days are left.
    out.reply.messages.push(UserMessage::new(
        message,
        if days < 3 { "notice" } else { "low" },
    ));
}

fn base_reply(ctx: &SchedCtx<'_>) -> SchedulerReply {
    let c = ctx.config;
    SchedulerReply {
        master_url: c.master_url.clone(),
        project_name: c.long_name.clone(),
        next_rpc_delay: c.next_rpc_delay,
        non_cpu_intensive: c.non_cpu_intensive,
        strict_memory_bound: c.strict_memory_bound,
        verify_files_on_app_start: c.verify_files_on_app_start,
        ..Default::default()
    }
}

fn refuse(out: &mut SchedOutcome, message: &str, priority: &str, delay: f64, why: &str) {
    out.reply.messages.push(UserMessage::new(message, priority));
    out.reply.request_delay = out.reply.request_delay.max(delay);
    out.log = why.to_string();
}

/// The host summary upstream keeps in `host.serialnum`: client version,
/// GPUs, and the virtualisation the client reported.
fn host_summary(req: &SchedulerRequest) -> String {
    let mut s = format!(
        "[BOINC|{}.{}.{}]",
        req.core_client_major_version, req.core_client_minor_version, req.core_client_release
    );
    for c in req.coprocs.all() {
        s.push_str(&format!(
            "[{}|{}|{}MB]",
            c.coproc_type,
            c.count,
            (c.available_ram / MEGA) as i64
        ));
    }
    if !req.host_info.virtualbox_version.is_empty() {
        s.push_str(&format!("[vbox|{}]", req.host_info.virtualbox_version));
    }
    if !req.host_info.docker_version.is_empty() {
        s.push_str(&format!("[docker|{}]", req.host_info.docker_version));
    }
    truncate(&s, 254).to_string()
}

/// Disk the project may still use on the host (`max_allowable_disk`).
fn disk_available(req: &SchedulerRequest, config: &Config) -> f64 {
    let prefs = &req.working_global_prefs;
    let host = &req.host_info;
    let used = req.disk_usage.d_boinc_used_total;
    let mut x = f64::MAX;
    if prefs.disk_max_used_gb > 0.0 {
        x = x.min(prefs.disk_max_used_gb * GIGA - used);
    }
    let pct = if prefs.disk_max_used_pct > 0.0 {
        prefs.disk_max_used_pct
    } else {
        100.0
    };
    if host.d_total > 0.0 {
        x = x.min(host.d_total * pct / 100.0 - used);
    }
    let min_free = prefs.disk_min_free_gb.max(config.default_disk_min_free_gb);
    x = x.min(host.d_free - min_free * GIGA);
    if req.disk_usage.d_boinc_max > 0.0 {
        x = x.min(req.disk_usage.d_boinc_max - used);
    }
    x.max(0.0)
}

/// RAM the client lets jobs use.
fn usable_ram(req: &SchedulerRequest) -> f64 {
    let p = &req.working_global_prefs;
    let mut frac = 1.0;
    if p.ram_max_used_busy_frac > 0.0 && p.ram_max_used_idle_frac > 0.0 {
        frac = p
            .ram_max_used_busy_frac
            .max(p.ram_max_used_idle_frac)
            .min(1.0);
    }
    req.host_info.m_nbytes * frac
}

/// `estimate_flops` in `sched/sched_version.cpp`: prefer this host's own
/// record with the version, then the version's record across hosts.
fn estimate_flops(hu: &mut HostUsage, av: &AppVersion, hav: Option<&HostAppVersion>) {
    if let Some(h) = hav {
        if h.et_n > MIN_HOST_SAMPLES && h.et_avg > 0.0 {
            hu.projected_flops = 1.0 / h.et_avg;
            return;
        }
    }
    if av.pfc_n > MIN_VERSION_SAMPLES && av.pfc_avg > 0.0 {
        hu.projected_flops = hu.peak_flops / av.pfc_avg;
    }
}

/// A client or version number stored as `major * 100 + minor` is multiplied
/// by 100, as `SCHED_SHMEM::scan_tables` does for `min_core_version` and
/// `max_core_version` ("min core version too small - multiplying by 100").
fn core_version(v: i32) -> i32 {
    if v > 0 && v < 10000 {
        v.saturating_mul(100)
    } else {
        v
    }
}

/// Whether a stored host is obviously not the computer a request describes
/// (`obviously_different` in `sched/handle_request.cpp`).
fn obviously_different(h: &Host, hi: &cb_proto::sched_request::HostInfo) -> bool {
    h.p_ncpus != hi.p_ncpus
        || h.p_vendor != hi.p_vendor
        || h.p_model != hi.p_model
        || h.os_name != hi.os_name
        || h.os_version != hi.os_version
}

/// `is_64b_platform` in `sched/sched_types.h`.
fn is_64b_platform(name: &str) -> bool {
    name.contains("64")
}

/// The largest memory bound a job can have and still run in a 32-bit app, by the client's
/// primary platform (`max_32b_address_space` in `sched/sched_version.cpp`, which looks at
/// `g_request->platform`).
fn max_32b_address_space(primary_platform: &str) -> f64 {
    if primary_platform.contains("windows") {
        2.0 * GIGA
    } else if primary_platform.contains("linux") {
        3.0 * GIGA
    } else if primary_platform.contains("darwin") || primary_platform.contains("solaris") {
        4.0 * GIGA
    } else {
        // The anonymous platform, which upstream takes to be Windows, and any other.
        2.0 * GIGA
    }
}

/// What an app version would use on this host, with the platform it runs
/// as; or, when the host cannot run it, what to tell the volunteer.
fn usage_for<'a>(
    av: &AppVersion,
    platforms: &[&'a Platform],
    req: &SchedulerRequest,
    plan_classes: &PlanClassSpecs,
    plan_host: &PlanHost<'_>,
    hav: Option<&HostAppVersion>,
) -> Result<(HostUsage, &'a str, usize), Option<&'static str>> {
    let rank = platforms
        .iter()
        .position(|p| p.id == av.platformid)
        .ok_or(None)?;
    let platform = platforms[rank];
    // The volunteer is not told that the client is too old for a particular
    // version, as upstream does not say it (`get_app_version`).
    if req.core_client_version < core_version(av.min_core_version) {
        return Err(None);
    }
    let max = core_version(av.max_core_version);
    if max > 0 && req.core_client_version > max {
        return Err(None);
    }
    let mut hu = plan_classes
        .evaluate(&av.plan_class, plan_host)
        .map_err(|r| r.user_message)?;
    // A client that does not state that it understands plan classes is sent
    // only versions that use one CPU.
    if !av.plan_class.is_empty()
        && !req.client_cap_plan_class
        && !(hu.proc_type == PROC_TYPE_CPU && hu.avg_ncpus == 1.0)
    {
        return Err(None);
    }
    estimate_flops(&mut hu, av, hav);
    if !(hu.projected_flops.is_finite() && hu.projected_flops > 0.0) {
        hu.projected_flops = 1e9;
    }
    Ok((hu, platform.name.as_str(), rank))
}

/// What a version from the client's `app_info.xml` uses on this host
/// (`CLIENT_APP_VERSION::parse`, `HOST_USAGE` of `sched/sched_types.h`).
/// `projected_flops` is the client's `<flops>`, or the host's benchmark speed
/// when it gave none (`estimate_flops_anon_platform`). `p_fpops` is that
/// benchmark speed, already positive: the caller replaces a missing or zero
/// one with `DEFAULT_FPOPS`.
///
/// What the client states drives what the result is credited for (its peak
/// speed is the result's `flops_estimate`), so it is bound by what the host
/// reports: CPUs by the host's, GPU instances by the host's count of that
/// GPU, and GPU speed by the host's figure for it, sanitised as the plan
/// class path does. The stated `<flops>` only sets the client's runtime
/// estimate and is bound by `MIN_ANON_FLOPS` and `MAX_ANON_FLOPS`.
fn anon_usage(
    cav: &ClientAppVersion,
    req: &SchedulerRequest,
    p_fpops: f64,
    ncpus: i32,
) -> HostUsage {
    let avg_ncpus = if cav.avg_ncpus.is_finite() && cav.avg_ncpus > 0.0 {
        cav.avg_ncpus.min(f64::from(ncpus.max(1)))
    } else {
        1.0
    };
    let mut gpu_usage = 0.0;
    let mut peak_flops = avg_ncpus * p_fpops;
    if cav.proc_type != PROC_TYPE_CPU {
        if let Some(c) = req.coprocs.proc_type(cav.proc_type) {
            gpu_usage = clean(cav.gpu_usage).min(f64::from(c.count.clamp(0, 8)));
            peak_flops += gpu_usage * gpu_peak_flops(c);
        }
    }
    let stated = if cav.flops.is_finite() && cav.flops > 0.0 {
        cav.flops
    } else {
        p_fpops
    };
    let mut hu = HostUsage {
        proc_type: cav.proc_type,
        gpu_usage,
        gpu_ram: 0.0,
        avg_ncpus,
        mem_usage: 0.0,
        projected_flops: stated.clamp(MIN_ANON_FLOPS, MAX_ANON_FLOPS),
        peak_flops,
        cmdline: String::new(),
    };
    hu.sanitize();
    hu
}

/// The speed to use for a version from the client's `app_info.xml`, and the
/// factor by which the workunit's `rsc_fpops_est` and `rsc_fpops_bound` are
/// scaled in the reply so that the client's own estimate (workunit size over
/// the speed it stated) comes out as ours
/// (`estimate_flops_anon_platform` in `sched/sched_version.cpp`).
///
/// Given the speed the client stated, with enough elapsed-time history for
/// this host and resource type the estimate is the one that history implies,
/// capped at `ET_RATIO_LIMIT` times the stated speed and kept within
/// `MIN_ANON_FLOPS` and `MAX_ANON_FLOPS`. The stated speed is within them
/// already (`anon_usage`), so the factor is finite and positive.
fn anon_projected_flops(stated: f64, hav: Option<&HostAppVersion>) -> (f64, f64) {
    match hav {
        Some(h) if h.et_n > MIN_HOST_SAMPLES && h.et_avg > 0.0 => {
            let new_flops = (1.0 / h.et_avg)
                .min(ET_RATIO_LIMIT * stated)
                .clamp(MIN_ANON_FLOPS, MAX_ANON_FLOPS);
            (new_flops, stated / new_flops)
        }
        _ => (stated, 1.0),
    }
}

/// A workunit size multiplied by the factor of `anon_projected_flops`; the
/// size itself if the product would not be a positive finite number.
fn scaled_size(size: f64, scale: f64) -> f64 {
    let x = size * scale;
    if x.is_finite() && (x > 0.0 || size <= 0.0) {
        x
    } else {
        size
    }
}

/// What `get_app_version_anonymous` found among the client's versions.
struct AnonPick {
    /// The version to use: the one with the highest projected speed that
    /// passes the checks, the earlier of two equal ones.
    chosen: Option<usize>,
    /// Some version of the app passed the checks other than the one for the
    /// resource the client wants work for; without one the client gets the
    /// message that its `app_info.xml` has no usable version.
    found: bool,
    /// Daily limits that stopped a version.
    quota_hit: Vec<i32>,
}

/// `get_app_version_anonymous`: scan the client's versions for the best one
/// for `app`. Skips versions for a 32-bit platform when the job needs more
/// address space than that allows, whose daily quota is used up, or that are
/// older than the app's minimum version; of the rest, takes the fastest for
/// which `needed` says the client wants work (`need_this_resource`).
fn pick_anonymous(
    versions: &[Version<'_>],
    rows: &HashMap<i64, RowState>,
    app: &App,
    need_64b: bool,
    needed: &dyn Fn(usize) -> bool,
) -> AnonPick {
    let mut pick = AnonPick {
        chosen: None,
        found: false,
        quota_hit: Vec::new(),
    };
    for (i, v) in versions.iter().enumerate() {
        if v.appid != app.id || !v.anonymous() {
            continue;
        }
        if need_64b && !is_64b_platform(v.platform) {
            continue;
        }
        let row = &rows[&v.gavid];
        if row.n_today >= row.quota {
            if !pick.quota_hit.contains(&row.quota) {
                pick.quota_hit.push(row.quota);
            }
            continue;
        }
        if v.version_num < app.min_version {
            continue;
        }
        pick.found = true;
        if !needed(proc_index(v.hu.proc_type)) {
            continue;
        }
        let faster = pick
            .chosen
            .is_none_or(|b| v.hu.projected_flops > versions[b].hu.projected_flops);
        if faster {
            pick.chosen = Some(i);
        }
    }
    pick
}

/// Shuffle each run of equal priority, so that hosts asking at the same
/// moment do not all try to claim the same results.
fn shuffle_within_priority(c: &mut [Candidate], random: &dyn Fn() -> u32) {
    let mut start = 0;
    while start < c.len() {
        let mut end = start + 1;
        while end < c.len() && c[end].result_priority == c[start].result_priority {
            end += 1;
        }
        for i in (start + 1..end).rev() {
            let j = start + (random() as usize) % (i - start + 1);
            c.swap(i, j);
        }
        start = end;
    }
}

/// Whether any result is unsent, whatever its app or platform: upstream's
/// feeder cache not being empty. The literal state lets SQLite use the
/// partial index `res_unsent`, which holds only unsent results, and read one
/// entry.
pub const UNSENT_EXISTS: &str = "SELECT 1 AS n FROM result WHERE server_state = 2 LIMIT 1";
const _: () = assert!(RESULT_SERVER_STATE_UNSENT == 2);

async fn unsent_work_exists(db: &dyn Db) -> CoreResult<bool> {
    Ok(!db.run(Stmt::new(UNSENT_EXISTS)).await?.rows.is_empty())
}

/// Handle one scheduler request. `body` is the request XML.
pub async fn handle_request(ctx: &SchedCtx<'_>, body: &str) -> CoreResult<SchedOutcome> {
    let config = ctx.config;
    let catalog = ctx.catalog;
    let now = ctx.now;
    let mut out = SchedOutcome {
        reply: base_reply(ctx),
        ..Default::default()
    };

    let req = match sched_request::parse(body) {
        Ok(r) => r,
        Err(e) => {
            refuse(
                &mut out,
                "Incomplete request received.",
                "low",
                DELAY_BAD_REQUEST,
                &format!("bad request: {e}"),
            );
            return Ok(out);
        }
    };
    out.reply.core_client_version = req.core_client_version;

    // A client below the project's minimum gets no work, but its reports are
    // still taken and its request still answered: `process_request` in
    // `sched/handle_request.cpp` clears `ok_to_send_work` and goes on when
    // the request carries results.
    let too_old = req.core_client_version < config.min_core_client_version;
    if too_old {
        out.reply.messages.push(UserMessage::new(
            "Your BOINC client software is too old.  Please install the current version.",
            "notice",
        ));
        out.reply.request_delay = out.reply.request_delay.max(DELAY_BAD_CLIENT_VERSION);
        if req.results.is_empty() {
            out.log = "client too old".to_string();
            return Ok(out);
        }
    }
    warn_if_upgrade_scheduled(&mut out, &req, config, now);
    if req.authenticator.is_empty() {
        out.reply.nucleus_only = true;
        refuse(
            &mut out,
            "Invalid or missing account key.  To fix, remove and add this project.",
            "notice",
            DELAY_MISSING_KEY,
            "no authenticator",
        );
        return Ok(out);
    }

    // Batch 1: who is asking.
    let weak_id = weak_auth_user_id(&req.authenticator);
    let mut reads = vec![stmt!(
        format!("{USER_WITH_TEAM} WHERE u.authenticator = ?"),
        req.authenticator.as_str()
    )];
    reads.push(stmt!("SELECT * FROM host WHERE id = ?", req.hostid.max(0)));
    if let Some(id) = weak_id {
        reads.push(stmt!(format!("{USER_WITH_TEAM} WHERE u.id = ?"), id as i64));
    }
    let mut rows = ctx.db.batch(reads).await?.into_iter();
    let by_auth: Option<UserRow> = rows.next().unwrap_or_default().first()?;
    let host_row: Option<Host> = rows.next().unwrap_or_default().first()?;
    let by_weak: Option<UserRow> = match rows.next() {
        Some(r) => r.first()?,
        None => None,
    };
    let (user, weak_auth) = match (by_auth, by_weak) {
        (Some(u), _) => (Some(u), false),
        (None, Some(u)) => {
            let ok = check_weak_auth(
                &req.authenticator,
                u.user.id as u64,
                &u.user.authenticator,
                &u.user.passwd_hash,
            );
            (ok.then_some(u), true)
        }
        (None, None) => (None, false),
    };
    let Some(UserRow { user, team_name }) = user.filter(|u| u.user.blocked == 0) else {
        out.reply.nucleus_only = true;
        refuse(
            &mut out,
            "Invalid or missing account key.  To fix, remove and add this project.",
            "notice",
            DELAY_MISSING_KEY,
            "bad authenticator",
        );
        return Ok(out);
    };
    out.user_id = user.id;

    // The host: the one named, if it is this user's and the request is not
    // from an older copy of its state; else one that looks like this
    // computer; else a new one.
    let request_cpid = if req.host_info.host_cpid.is_empty() {
        String::new()
    } else {
        host_cpid(&req.host_info.host_cpid, &user.email_addr)
    };
    let mut host = host_row
        .filter(|h| h.userid == user.id && i64::from(req.rpc_seqno) >= h.rpc_seqno && h.id > 0);
    let mut results_detached = false;
    if host.is_none() {
        let mut lookups = Vec::new();
        if !request_cpid.is_empty() {
            lookups.push(stmt!(
                "SELECT * FROM host WHERE userid = ? AND host_cpid = ? ORDER BY id DESC LIMIT 1",
                user.id,
                request_cpid.as_str()
            ));
        }
        let n_cpid = lookups.len();
        // `find_host_by_other` in `sched/handle_request.cpp`: only when the client has all four
        // of the name, address, OS and CPU model, and not when it says it is one of several
        // clients on a computer ("each client instance needs its own host record"), whose
        // host CPIDs differ by design.
        let hi = &req.host_info;
        if req.allow_multiple_clients != 1
            && !hi.domain_name.is_empty()
            && !hi.last_ip_addr.is_empty()
            && !hi.os_name.is_empty()
            && !hi.p_model.is_empty()
        {
            lookups.push(stmt!(
                "SELECT * FROM host WHERE userid = ? AND domain_name = ? AND last_ip_addr = ? \
                 AND os_name = ? AND p_model = ? AND m_nbytes = ? ORDER BY id DESC LIMIT 1",
                user.id,
                hi.domain_name.as_str(),
                hi.last_ip_addr.as_str(),
                hi.os_name.as_str(),
                hi.p_model.as_str(),
                hi.m_nbytes
            ));
        }
        if !lookups.is_empty() {
            for (i, r) in ctx.db.batch(lookups).await?.into_iter().enumerate() {
                if host.is_some() {
                    break;
                }
                host = r.first()?;
                if host.is_some() && i < n_cpid {
                    // A host with the same CPID that is obviously another computer is not
                    // this one (`obviously_different` in `sched/handle_request.cpp`); the
                    // CPID is made from the MAC address and the data directory, which a
                    // cloned machine or a virtual machine image shares.
                    if host
                        .as_ref()
                        .is_some_and(|h| obviously_different(h, &req.host_info))
                    {
                        host = None;
                        continue;
                    }
                    // The client lost track of its jobs unless it lists
                    // them, or says other clients share the host.
                    results_detached =
                        req.allow_multiple_clients != 1 && req.other_results.is_empty();
                }
            }
        }
    }
    let host = match host {
        Some(h) => h,
        None => {
            // Only while the account still has the key this request
            // authenticated with and is not blocked (an account being deleted
            // is): a request that began before the deletion makes no host.
            let sql = format!(
                "INSERT INTO host (id, create_time, userid, venue, host_cpid, expavg_time) \
                 SELECT {}, ?, ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM user WHERE id = ? \
                 AND authenticator = ? AND blocked = 0) RETURNING id",
                crate::web::next_id_sql("host")
            );
            let inserted = ctx
                .db
                .run(stmt!(
                    sql,
                    now,
                    user.id,
                    user.venue.as_str(),
                    request_cpid.as_str(),
                    now,
                    user.id,
                    user.authenticator.as_str()
                ))
                .await?
                .scalar_i64();
            let Some(id) = inserted else {
                out.reply.nucleus_only = true;
                refuse(
                    &mut out,
                    "Invalid or missing account key.  To fix, remove and add this project.",
                    "notice",
                    DELAY_MISSING_KEY,
                    "bad authenticator",
                );
                return Ok(out);
            };
            Host {
                id,
                create_time: now,
                userid: user.id,
                venue: user.venue.clone(),
                host_cpid: request_cpid.clone(),
                // As `handle_request` in sched/handle_request.cpp sets it.
                expavg_time: now as f64,
                ..Default::default()
            }
        }
    };
    out.host_id = host.id;
    if host.max_results_day == -1 {
        refuse(
            &mut out,
            "Not accepting requests from this host",
            "notice",
            DELAY_NO_HOST,
            "host refused",
        );
        return Ok(out);
    }

    out.reply.hostid = if host.id != req.hostid {
        host.id as u64
    } else {
        0
    };
    out.reply.host = ReplyHost {
        total_credit: host.total_credit,
        expavg_credit: host.expavg_credit,
        venue: host.venue.clone(),
        create_time: host.create_time,
    };
    // `<teamid>` and `<team_name>`, as `SCHEDULER_REPLY::write` does when the
    // account has a team; a team that is gone is written as no team.
    out.reply.team = match team_name {
        Some(name) if user.teamid > 0 => Some((user.teamid as u64, name)),
        _ => None,
    };
    out.reply.user = ReplyUser {
        id: user.id as u64,
        name: user.name.clone(),
        total_credit: user.total_credit,
        expavg_credit: user.expavg_credit,
        create_time: user.create_time,
        email_hash: email_hash(&user.email_addr),
        cross_project_id: user.cross_project_id.clone(),
        external_cpid: user_cpid(&user.cross_project_id, &user.email_addr),
        global_prefs: None,
        project_prefs: user.project_prefs.clone(),
    };

    // Platforms the host runs that the project has. On the anonymous
    // platform the alternate platforms are ignored (`process_request` in
    // `sched/handle_request.cpp`). The project must list a platform named
    // `anonymous` to serve such clients, as upstream's `project.xml` does.
    let alternates: &[String] = if req.platform_name == ANONYMOUS_PLATFORM {
        &[]
    } else {
        &req.alt_platforms
    };
    let host_platforms: Vec<&Platform> = std::iter::once(&req.platform_name)
        .chain(alternates.iter())
        .filter_map(|n| catalog.platforms.iter().find(|p| &p.name == n))
        .collect();
    let anonymous = host_platforms
        .first()
        .is_some_and(|p| p.name == ANONYMOUS_PLATFORM);
    if host_platforms.is_empty() {
        refuse(
            &mut out,
            &format!(
                "This project doesn't support computers of type {}",
                req.platform_name
            ),
            "notice",
            DELAY_PLATFORM_UNSUPPORTED,
            "platform unsupported",
        );
        return Ok(out);
    }
    if req.host_info.p_vendor.is_empty() {
        out.reply
            .messages
            .push(UserMessage::new("Missing CPU type in request", "notice"));
    }
    if req.host_info.os_name.is_empty() {
        out.reply
            .messages
            .push(UserMessage::new("Missing OS name in request", "notice"));
    }

    // Code-signing key: hand it to a client that has none; a client holding
    // a different one must re-attach.
    let mut key_ok = true;
    if req.code_sign_key.trim().is_empty() {
        out.reply.code_sign_key = ctx.code_sign_key.to_string();
    } else if !ctx.code_sign_key.is_empty() && req.code_sign_key.trim() != ctx.code_sign_key.trim()
    {
        key_ok = false;
        out.reply.messages.push(UserMessage::new(
            "The project has changed its security key.  Please remove and add this project.",
            "notice",
        ));
    }

    // Batch 2: the results this request talks about and the host's record.
    let max_reported = if config.max_results_accepted > 0 {
        config.max_results_accepted as usize
    } else {
        MAX_REPORTED
    };
    let reported: Vec<&ReportedResult> = req.results.iter().take(max_reported).collect();
    let want_aborts = config.send_result_abort && req.have_other_results_list;
    #[derive(Clone, Copy, PartialEq)]
    enum Read {
        Reported,
        InProgress,
        Hav,
        Other,
    }
    let mut kinds = Vec::new();
    let mut reads = Vec::new();
    for chunk in reported.chunks(MAX_IN_LIST) {
        let mut s = Stmt::new(format!(
            "SELECT id, workunitid, server_state, outcome, userid, received_time, sent_time, \
             app_version_id, appid, name FROM result WHERE name IN ({})",
            in_list(chunk.len())
        ));
        for r in chunk {
            s = s.bind(r.name.as_str());
        }
        reads.push(s);
        kinds.push(Read::Reported);
    }
    reads.push(stmt!(
        "SELECT id, name, workunitid, app_version_id, flops_estimate FROM result \
         WHERE hostid = ? AND server_state = 4",
        host.id
    ));
    kinds.push(Read::InProgress);
    reads.push(stmt!(
        "SELECT * FROM host_app_version WHERE host_id = ?",
        host.id
    ));
    kinds.push(Read::Hav);
    if want_aborts {
        for chunk in req.other_results.chunks(MAX_IN_LIST) {
            let mut s = Stmt::new(format!(
                "SELECT r.name, r.server_state, r.outcome, w.error_mask, w.assimilate_state \
                 FROM result r JOIN workunit w ON w.id = r.workunitid WHERE r.name IN ({})",
                in_list(chunk.len())
            ));
            for r in chunk {
                s = s.bind(r.name.as_str());
            }
            reads.push(s);
            kinds.push(Read::Other);
        }
    }
    let mut reported_rows: HashMap<String, ReportedRow> = HashMap::new();
    let mut in_progress: Vec<InProgressRow> = Vec::new();
    let mut havs: HashMap<i64, HostAppVersion> = HashMap::new();
    let mut other_rows: HashMap<String, OtherRow> = HashMap::new();
    for (kind, rows) in kinds.into_iter().zip(ctx.db.batch(reads).await?) {
        match kind {
            Read::Reported => {
                for r in rows.decode::<ReportedRow>()? {
                    reported_rows.insert(r.name.clone(), r);
                }
            }
            Read::InProgress => in_progress = rows.decode()?,
            Read::Hav => {
                for h in rows.decode::<HostAppVersion>()? {
                    havs.insert(h.app_version_id, h);
                }
            }
            Read::Other => {
                for r in rows.decode::<OtherRow>()? {
                    other_rows.insert(r.name.clone(), r);
                }
            }
        }
    }

    // The first statement stops the whole batch if the account was blocked or
    // its key changed since this request read it (see `account_guard`).
    let mut writes: Vec<Stmt> = vec![account_guard(user.id, &user.authenticator)];
    let mut transition: Vec<i64> = Vec::new();

    // A new day: the daily counts start again.
    let new_day = host.rpc_time / 86400 != now / 86400;
    if new_day && !havs.is_empty() {
        writes.push(stmt!(
            "UPDATE host_app_version SET n_jobs_today = 0 WHERE host_id = ?",
            host.id
        ));
    }

    let daily_quota = if config.daily_result_quota > 0 {
        config.daily_result_quota
    } else {
        NO_QUOTA
    };

    // Reported results (`handle_results`).
    let mut reported_ids: HashSet<i64> = HashSet::new();
    for rr in &reported {
        out.reply.result_acks.push(rr.name.clone());
        let Some(row) = reported_rows.get(&rr.name) else {
            continue;
        };
        let accept = match row.server_state {
            RESULT_SERVER_STATE_IN_PROGRESS => true,
            // Already timed out or written off: take it anyway.
            RESULT_SERVER_STATE_OVER => matches!(
                row.outcome,
                RESULT_OUTCOME_NO_REPLY | RESULT_OUTCOME_CLIENT_DETACHED
            ),
            _ => false,
        };
        // Another host may report it only if it is the same user's.
        if !accept || row.received_time != 0 || row.userid != user.id {
            continue;
        }
        reported_ids.insert(row.id);
        let turnaround = (now - row.sent_time).max(0) as f64;
        let mut elapsed = clean(rr.elapsed_time);
        if elapsed > turnaround {
            elapsed = turnaround;
        }
        let cpu_time = clean(rr.cpu_time);
        let success = rr.client_state == RESULT_FILES_UPLOADED && rr.exit_status == 0;
        let (outcome, validate_state) = if success {
            (RESULT_OUTCOME_SUCCESS, VALIDATE_STATE_INIT)
        } else {
            (RESULT_OUTCOME_CLIENT_ERROR, VALIDATE_STATE_INVALID)
        };
        let stderr = if success && config.dont_store_success_stderr {
            ""
        } else {
            truncate(rr.stderr_text(), MAX_BLOB)
        };
        writes.push(stmt!(
            "UPDATE result SET server_state = 5, outcome = ?, validate_state = ?, client_state = ?, \
             received_time = ?, cpu_time = ?, elapsed_time = ?, exit_status = ?, \
             app_version_num = ?, stderr_out = ?, xml_doc_out = ?, peak_working_set_size = ?, \
             peak_swap_size = ?, peak_disk_usage = ?, hostid = ?, teamid = ?, mod_time = ? \
             WHERE id = ? AND received_time = 0 AND server_state IN (4, 5)",
            outcome,
            validate_state,
            rr.client_state,
            now,
            cpu_time,
            elapsed,
            rr.exit_status,
            rr.app_version_num,
            stderr,
            truncate(&rr.xml_doc_out, MAX_BLOB),
            clean(rr.peak_working_set_size),
            clean(rr.peak_swap_size),
            clean(rr.peak_disk_usage),
            host.id,
            user.teamid,
            now,
            row.id
        ));
        // The daily limit doubles on success and drops by one on failure.
        // An anonymous-platform result (negative `app_version_id`) counts
        // against the row of its app and resource type
        // (`got_good_result`, `got_bad_result` in `sched/sched_result.cpp`).
        let gavid = generalized_app_version_id(row.app_version_id, row.appid);
        if gavid > 0 {
            if success {
                writes.push(stmt!(
                    "UPDATE host_app_version SET max_jobs_per_day = min(max_jobs_per_day * 2, ?) \
                     WHERE host_id = ? AND app_version_id = ?",
                    daily_quota,
                    host.id,
                    gavid
                ));
            } else if rr.exit_status != EXIT_ABORTED_BY_PROJECT {
                writes.push(stmt!(
                    "UPDATE host_app_version SET \
                     max_jobs_per_day = max(1, min(max_jobs_per_day, ?) - 1), \
                     consecutive_valid = 0 WHERE host_id = ? AND app_version_id = ?",
                    daily_quota,
                    host.id,
                    gavid
                ));
            }
        }
        writes.push(stmt!(
            "UPDATE workunit SET transition_time = ?, rev = rev + 1 WHERE id = ?",
            now,
            row.workunitid
        ));
        transition.push(row.workunitid);
        out.n_reported += 1;
    }
    in_progress.retain(|r| !reported_ids.contains(&r.id));

    // The client came back without its jobs: write them off.
    if results_detached && !in_progress.is_empty() {
        for r in &in_progress {
            writes.push(stmt!(
                "UPDATE workunit SET transition_time = ?, rev = rev + 1 WHERE id = ?",
                now,
                r.workunitid
            ));
            transition.push(r.workunitid);
        }
        writes.push(stmt!(
            "UPDATE result SET server_state = 5, outcome = ?, received_time = ?, mod_time = ? \
             WHERE hostid = ? AND server_state = 4",
            RESULT_OUTCOME_CLIENT_DETACHED,
            now,
            now,
            host.id
        ));
        in_progress.clear();
    }

    // Results the client holds that the project no longer wants
    // (`send_result_abort`).
    if want_aborts {
        for o in &req.other_results {
            if reported.iter().any(|r| r.name == o.name) {
                continue;
            }
            match other_rows.get(&o.name) {
                None => out.reply.result_aborts.push(o.name.clone()),
                Some(r) if r.error_mask & WU_ERROR_CANCELLED != 0 => {
                    out.reply.result_aborts.push(o.name.clone())
                }
                Some(r)
                    if r.assimilate_state == ASSIMILATE_DONE
                        || (r.server_state == RESULT_SERVER_STATE_OVER
                            && r.outcome == RESULT_OUTCOME_NO_REPLY) =>
                {
                    out.reply.result_abort_if_not_starteds.push(o.name.clone())
                }
                Some(_) => {}
            }
        }
    }

    // What the host can run.
    let prefs = ProjectPrefs::parse(&user.project_prefs, &host.venue);
    let ncpus = req.host_info.p_ncpus.clamp(1, config.max_ncpus.max(1));
    let mut ninstances = [0.0_f64; NPROC];
    let mut need_secs = [0.0_f64; NPROC];
    let mut need_inst = [0.0_f64; NPROC];
    let mut est_delay = [0.0_f64; NPROC];
    ninstances[0] = f64::from(ncpus);
    need_secs[0] = req.cpu_req_secs;
    need_inst[0] = req.cpu_req_instances;
    est_delay[0] = req.cpu_estimated_delay;
    let mut ngpus = 0;
    for pt in 1..NPROC {
        if let Some(c) = req.coprocs.proc_type(pt as i32) {
            ninstances[pt] = f64::from(c.count.clamp(0, 8));
            need_secs[pt] = c.req_secs;
            need_inst[pt] = c.req_instances;
            est_delay[pt] = c.estimated_delay;
            ngpus += c.count.clamp(0, 8);
        }
    }
    if need_secs.iter().chain(need_inst.iter()).all(|x| *x <= 0.0) && req.work_req_seconds > 0.0 {
        // A client that does not ask per resource.
        need_secs[0] = req.work_req_seconds;
    }
    let work_requested = need_secs.iter().chain(need_inst.iter()).any(|x| *x > 0.0);

    let gpu_version_ids: Vec<i64> = catalog
        .app_versions
        .iter()
        .chain(catalog.retired_versions.iter())
        .filter(|v| {
            ctx.plan_classes
                .find(&v.plan_class)
                .is_some_and(|s| !s.gpu_type.is_empty())
        })
        .map(|v| v.id)
        .collect();
    // A result runs on a GPU when its version has a GPU plan class, or, on
    // the anonymous platform, when its negative ID names a GPU.
    let is_gpu_version = |id: i64| gpu_version_ids.contains(&id) || anon_uses_gpu(id);

    let plan_host = PlanHost {
        request: &req,
        user_id: user.id,
        host_id: host.id,
        project_prefs: &user.project_prefs,
        host_serialnum: &host.serialnum,
        random: unit(ctx.random),
    };
    let mut plan_messages: Vec<&'static str> = Vec::new();

    // Homogeneous redundancy and app version, per app, for this host. The
    // class comes from the host's description in this request, as upstream's
    // `g_request->host` does.
    let hr_host = HrHost {
        os_name: &req.host_info.os_name,
        p_vendor: &req.host_info.p_vendor,
        p_model: &req.host_info.p_model,
    };
    let homog: HashMap<i64, Homogeneous> = catalog
        .apps
        .iter()
        .map(|a| (a.id, Homogeneous::of(a, config, &hr_host)))
        .collect();
    // `hr_unknown_platform`: every app uses HR and none can place the host.
    let hr_blocked = !catalog.apps.is_empty()
        && catalog
            .apps
            .iter()
            .all(|a| homog[&a.id].hr_type != 0 && homog[&a.id].unknown);

    // The daily limit of a (host, app version): the host's row, scaled by the
    // processors the version uses (`scaled_max_jobs_per_day`).
    let quota_of = |hav: Option<&HostAppVersion>, gpu: bool, pt: usize| {
        let scale = if gpu {
            (ninstances[pt].max(1.0) as i32).saturating_mul(config.gpu_multiplier.max(1))
        } else {
            ncpus
        };
        let base = hav
            .map_or(daily_quota, |h| h.max_jobs_per_day.min(daily_quota))
            .max(1);
        base.saturating_mul(scale)
    };
    // What this request keeps for each `host_app_version` row, by key.
    let mut rows: HashMap<i64, RowState> = HashMap::new();
    let mut row_of = |gavid: i64, hav: Option<&HostAppVersion>, gpu: bool, pt: usize| {
        rows.entry(gavid).or_insert_with(|| RowState {
            n_today: match hav {
                Some(h) if !new_day => h.n_jobs_today,
                _ => 0,
            },
            quota: quota_of(hav, gpu, pt),
            consecutive_valid: hav.map_or(0, |h| h.consecutive_valid),
            has_row: hav.is_some(),
        });
    };

    // App versions the host can be sent, fastest first.
    let mut versions: Vec<Version<'_>> = Vec::new();
    // Resource types for which the client's `app_info.xml` has a version
    // (`client_has_apps_for_proc_type`).
    let mut client_has = [false; NPROC];
    if anonymous {
        // The client's own versions (`estimate_flops_anon_platform` and
        // `CLIENT_APP_VERSION::parse`). Entries are kept in the client's
        // order: ties in speed go to the earlier one, as upstream's scan.
        let apps_by_name: HashMap<&str, &App> =
            catalog.apps.iter().map(|a| (a.name.as_str(), a)).collect();
        // A host whose benchmark is missing or zero gets the same default
        // as for a sequential app, so that its CPU version has a positive
        // peak speed, and so a result a positive `flops_estimate` to be
        // credited on (`HostUsage::sequential`, `host_fpops`).
        let p_fpops = match clean(req.host_info.p_fpops) {
            x if x > 0.0 => x,
            _ => DEFAULT_FPOPS,
        };
        for cav in req.client_app_versions.iter().take(MAX_CLIENT_VERSIONS) {
            let pt = proc_index(cav.proc_type);
            client_has[pt] = true;
            if !cav.parsed {
                out.reply
                    .messages
                    .push(UserMessage::new("Syntax error in app_info.xml", "notice"));
                continue;
            }
            let Some(app) = apps_by_name.get(cav.app_name.as_str()).copied() else {
                out.reply.messages.push(UserMessage::new(
                    format!("Unknown app name {} in app_info.xml", cav.app_name),
                    "notice",
                ));
                continue;
            };
            if !prefs.app_ids.is_empty() && !prefs.app_ids.contains(&app.id) {
                continue;
            }
            if app.beta != 0 && !prefs.allow_beta_work {
                continue;
            }
            let hu = anon_usage(cav, &req, p_fpops, ncpus);
            let gavid = anon_gavid(app.id, cav.proc_type);
            let hav = havs.get(&gavid);
            let (projected_flops, fpops_scale) = anon_projected_flops(hu.projected_flops, hav);
            let gpu = pt != 0;
            row_of(gavid, hav, gpu, pt);
            versions.push(Version {
                av: None,
                appid: app.id,
                result_av_id: anon_platform_id(cav.proc_type),
                gavid,
                version_num: cav.version_num,
                plan_class: &cav.plan_class,
                platform: &cav.platform,
                rank: 0,
                fpops_scale,
                hu: HostUsage {
                    projected_flops,
                    ..hu
                },
                gpu,
                score: projected_flops,
                pinned_only: false,
                commit_only: false,
            });
        }
    } else {
        for av in catalog.app_versions.iter().chain(&catalog.retired_versions) {
            let Some(app) = catalog.app(av.appid) else {
                continue;
            };
            // A version that is no longer sent to anyone can still be the
            // one a homogeneous-app-version workunit is committed to.
            let superseded = catalog.superseded(av);
            let pinned_only = av.deprecated != 0 || superseded || av.version_num < app.min_version;
            // `scan_tables` loads a version unless it is deprecated by hand,
            // below `min_version`, or (with no `min_version`) not the latest
            // of its platform and plan class.
            let commit_only = av.deprecated != 0
                || av.version_num < app.min_version
                || (superseded && app.min_version == 0);
            if pinned_only && !keeps_version(app, av) {
                continue;
            }
            if !prefs.app_ids.is_empty() && !prefs.app_ids.contains(&app.id) {
                continue;
            }
            if (app.beta != 0 || av.beta != 0) && !prefs.allow_beta_work {
                continue;
            }
            let hav = havs.get(&av.id);
            let (hu, platform, rank) =
                match usage_for(av, &host_platforms, &req, ctx.plan_classes, &plan_host, hav) {
                    Ok(x) => x,
                    Err(message) => {
                        if let Some(m) = message.filter(|_| !pinned_only) {
                            if !plan_messages.contains(&m) {
                                plan_messages.push(m);
                            }
                        }
                        continue;
                    }
                };
            let pt = proc_index(hu.proc_type);
            if prefs.no_proc[pt] {
                continue;
            }
            let gpu = is_gpu_version(av.id);
            let mut score = hu.projected_flops;
            if config.version_select_random_factor > 0.0 {
                score *= 1.0 + 0.1 * config.version_select_random_factor * normal(ctx.random);
            }
            row_of(av.id, hav, gpu, pt);
            versions.push(Version {
                av: Some(av),
                appid: av.appid,
                result_av_id: av.id,
                gavid: av.id,
                version_num: av.version_num,
                plan_class: &av.plan_class,
                platform,
                rank,
                fpops_scale: 1.0,
                hu,
                gpu,
                score,
                pinned_only,
                commit_only,
            });
        }
    }
    // The fastest first; of two equal ones the version of the platform the client lists
    // first (`get_app_version` scans the platforms in order and replaces its choice only
    // by a version that is faster), then the earlier in the catalog (the sort is stable).
    versions.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.rank.cmp(&b.rank))
    });

    let mut too_recent = false;
    let mut too_recent_message: Option<String> = None;
    if work_requested && config.min_sendwork_interval > 0 && host.rpc_time > 0 {
        let since = now - host.rpc_time;
        if since < i64::from(config.min_sendwork_interval) {
            too_recent = true;
            // As `process_request` in sched/handle_request.cpp words it; the
            // wait is what is left of the interval. Said below, unless the
            // reply resends lost tasks, after which upstream looks no further.
            too_recent_message = Some(format!(
                "Not sending work. Last request too recent. Please wait {} seconds.",
                i64::from(config.min_sendwork_interval) - since
            ));
        }
    }
    // Upstream's `work_needed` asks for a resource the client needs work for
    // and has an app for: on the anonymous platform that is a resource of one
    // of the client's versions, even one this project has no app for.
    let has_versions = if anonymous {
        (0..NPROC).any(|pt| client_has[pt] && (need_secs[pt] > 0.0 || need_inst[pt] > 0.0))
    } else {
        // A version kept only for committed workunits is enough to enter
        // dispatch; `admitted` keeps it off uncommitted ones, and the queries
        // below do not read uncommitted work for an app that has no other.
        !versions.is_empty()
    };
    let send = work_requested
        && !too_old
        && !hr_blocked
        && key_ok
        && !config.dont_send_jobs
        && !req.dont_send_work
        && !too_recent
        && has_versions;

    // The host's record of itself.
    let nsame_ip = if !req.host_info.last_ip_addr.is_empty()
        && req.host_info.last_ip_addr == host.last_ip_addr
    {
        host.nsame_ip_addr.saturating_add(1)
    } else {
        0
    };
    let gpu_fpops = req
        .coprocs
        .all()
        .iter()
        .map(|c| clean(c.peak_flops))
        .fold(0.0, f64::max);
    let hi = &req.host_info;
    let unit_frac = |x: f64| if x > 0.0 && x <= 1.0 { x } else { 1.0 };
    writes.push(stmt!(
        "UPDATE host SET rpc_seqno = ?, rpc_time = ?, timezone = ?, domain_name = ?, \
         serialnum = ?, last_ip_addr = ?, nsame_ip_addr = ?, on_frac = ?, connected_frac = ?, \
         active_frac = ?, gpu_active_frac = ?, duration_correction_factor = ?, p_ncpus = ?, \
         p_vendor = ?, p_model = ?, p_fpops = ?, p_iops = ?, p_membw = ?, os_name = ?, \
         os_version = ?, m_nbytes = ?, m_cache = ?, m_swap = ?, d_total = ?, d_free = ?, \
         d_boinc_used_total = ?, d_boinc_used_project = ?, d_boinc_max = ?, n_bwup = ?, \
         n_bwdown = ?, host_cpid = ?, external_ip_addr = ?, product_name = ?, p_ngpus = ?, \
         p_gpu_fpops = ?, misc = ? WHERE id = ?",
        i64::from(req.rpc_seqno),
        now,
        hi.timezone,
        truncate(&hi.domain_name, 254),
        host_summary(&req),
        truncate(&hi.last_ip_addr, 254),
        nsame_ip,
        req.time_stats.on_frac,
        req.time_stats.connected_frac,
        req.time_stats.active_frac,
        req.time_stats.gpu_active_frac,
        req.duration_correction_factor,
        hi.p_ncpus,
        truncate(&hi.p_vendor, 254),
        truncate(&hi.p_model, 254),
        clean(hi.p_fpops),
        clean(hi.p_iops),
        clean(hi.p_membw),
        truncate(&hi.os_name, 254),
        truncate(&hi.os_version, 254),
        clean(hi.m_nbytes),
        clean(hi.m_cache),
        clean(hi.m_swap),
        clean(hi.d_total),
        clean(hi.d_free),
        clean(req.disk_usage.d_boinc_used_total),
        clean(req.disk_usage.d_boinc_used_project),
        clean(req.disk_usage.d_boinc_max),
        clean(req.net_stats.n_bwup),
        clean(req.net_stats.n_bwdown),
        if request_cpid.is_empty() {
            host.host_cpid.as_str()
        } else {
            request_cpid.as_str()
        },
        truncate(ctx.client_ip, 254),
        truncate(&hi.product_name, 254),
        ngpus,
        gpu_fpops,
        host_info_json(&req),
        host.id
    ));

    // Global preferences (`handle_global_prefs` in sched/handle_request.cpp).
    // Three copies can exist: the client's working copy, the copy it got
    // from a project's web site, and the account's. The account's is sent
    // when it is newer than both of the client's; the client's web copy
    // replaces the account's when it is newer and comes from this account.
    // A time in the future counts as now.
    let cap = |t: f64| t.min(now as f64);
    let mod_time = |xml: &str| {
        if xml.trim().is_empty() {
            0.0
        } else {
            cap(line::parse_double(xml, "<mod_time>").unwrap_or(0.0))
        }
    };
    let have_master = !req.global_prefs_xml.trim().is_empty();
    let have_db = !user.global_prefs.trim().is_empty();
    let master_mod = mod_time(&req.global_prefs_xml);
    let working_mod = mod_time(&req.working_global_prefs_xml);
    let db_mod = mod_time(&user.global_prefs);
    let same_account = req.global_prefs_source_email_hash == email_hash(&user.email_addr);
    if !weak_auth && have_master {
        let newer = if have_db { master_mod > db_mod } else { true };
        if newer && same_account {
            writes.push(stmt!(
                "UPDATE user SET global_prefs = ? WHERE id = ?",
                truncate(&req.global_prefs_xml, MAX_BLOB),
                user.id
            ));
        }
    }
    if have_db && db_mod > master_mod && db_mod > working_mod {
        out.reply.user.global_prefs = Some(user.global_prefs.clone());
    }

    // Batch 3: results the client lost, and candidates to send.
    let other_names: HashSet<&str> = req.other_results.iter().map(|o| o.name.as_str()).collect();
    // Upstream looks for lost results only when it may send work
    // (`ok_to_send_work` in `process_request`: the project sends jobs and the
    // request asks for work), so a request that only reports results leaves
    // them be.
    let may_send_work = work_requested && !too_old && !config.dont_send_jobs && !req.dont_send_work;
    let lost: Vec<&InProgressRow> =
        if config.resend_lost_results && req.have_other_results_list && key_ok && may_send_work {
            in_progress
                .iter()
                .filter(|r| !other_names.contains(r.name.as_str()))
                .take(MAX_LOST)
                .collect()
        } else {
            Vec::new()
        };
    let per_rpc = i64::from(config.max_wus_to_send.max(1))
        * (i64::from(ncpus) + i64::from(config.gpu_multiplier.max(0)) * i64::from(ngpus));
    let max_send = usize::try_from(per_rpc).unwrap_or(1).clamp(1, MAX_SEND);
    // Apps this host has a version of, and of those the apps whose unsent
    // results it may be sent: not an app that uses homogeneous redundancy
    // and cannot place the host, and, on the anonymous platform, not an app
    // that uses homogeneous app version (`get_app_version`).
    let mut usable_apps: Vec<i64> = Vec::new();
    let mut app_ids: Vec<i64> = Vec::new();
    if send {
        for v in &versions {
            if !usable_apps.contains(&v.appid) {
                usable_apps.push(v.appid);
            }
        }
        usable_apps.sort_unstable();
        app_ids = usable_apps
            .iter()
            .copied()
            .filter(|id| {
                let h = homog[id];
                !h.unknown && !(anonymous && h.hav)
            })
            .collect();
    }
    // On the anonymous platform, the apps the client has no version of that
    // may have work: the client is told its `app_info.xml` lacks one
    // (`get_app_version_anonymous` is reached for every job upstream scans).
    // One existence probe each on the index of unsent results.
    let mut probe_apps: Vec<&App> = Vec::new();
    if send && anonymous {
        probe_apps = catalog
            .apps
            .iter()
            .filter(|a| {
                !usable_apps.contains(&a.id)
                    && !homog[&a.id].hav
                    && (prefs.app_ids.is_empty() || prefs.app_ids.contains(&a.id))
                    && (a.beta == 0 || prefs.allow_beta_work)
            })
            .collect();
    }
    // The replica rule: no two results of a workunit to one user or host.
    let replica_rule = config.one_result_per_user_per_wu || config.one_result_per_host_per_wu;
    let replica_who = if config.one_result_per_user_per_wu {
        ("userid", user.id)
    } else {
        ("hostid", host.id)
    };
    let mut reads = Vec::new();
    if !lost.is_empty() {
        let mut s = Stmt::new(format!(
            "SELECT {CANDIDATE_COLS} FROM result r JOIN workunit w ON w.id = r.workunitid \
             WHERE r.id IN ({}) AND r.server_state = 4 AND r.hostid = ?",
            in_list(lost.len())
        ));
        for r in &lost {
            s = s.bind(r.id);
        }
        reads.push(s.bind(host.id));
    }
    // Assigned jobs for this host or user, one reserved result each, that
    // the host holds no result of (`need_targeted_instance`). Only jobs of
    // an app, and a pinned version, this host has a usable version of: the
    // limit counts jobs that can be sent, so ones the host cannot run do
    // not hold back later ones. The IDs are integers of the catalog.
    //
    // Upstream's `send_assigned_job` does not ask whether HR can place the
    // host, so an app that cannot is not left out; a workunit committed to
    // another class, or, for an app with homogeneous app version, to a
    // version the host has no use of, fails the claim and is left out here.
    // The client's own versions are not for an app with homogeneous app
    // version.
    let assigned_list: Vec<i64> = usable_apps
        .iter()
        .copied()
        .filter(|id| !(anonymous && homog[id].hav))
        .collect();
    let want_assigned = send && config.enable_assignment && !assigned_list.is_empty();
    let id_list = |ids: &mut dyn Iterator<Item = i64>| {
        ids.map(|id| id.to_string()).collect::<Vec<_>>().join(",")
    };
    let assigned_apps = id_list(&mut assigned_list.iter().copied());
    // The workunits of an app this host's versions can be sent, as a
    // condition on `w`: the same test as `admitted` below, one clause for each
    // version of the app the host can use, so that a window of unsent results
    // is never filled with ones the host cannot be sent. See "Dispatch
    // without a feeder" in `docs/architecture.md`. The IDs and numbers are
    // integers of the catalog and the plan class file. The client's own
    // versions are none: an app with a client version has no clause.
    let version_gate = |app_id: i64| -> String {
        let hav = catalog
            .app(app_id)
            .is_some_and(|a| a.homogeneous_app_version != 0);
        let mut clauses: Vec<String> = Vec::new();
        for v in versions.iter().filter(|v| v.appid == app_id) {
            let Some(av) = v.av else {
                // The client's own version takes any workunit of a plain
                // app (`admitted` below).
                return "1 = 1".to_string();
            };
            let vn = av.version_num;
            // Uncommitted workunits: a pinned one needs this number; one that
            // is not needs a version still sent to everyone.
            // A version kept only for committed workunits takes no uncommitted
            // one (`admitted` below).
            let free = if v.commit_only {
                None
            } else if v.pinned_only {
                Some(format!("w.app_version_num = {vn}"))
            } else {
                Some(format!("w.app_version_num IN (0, {vn})"))
            };
            let mut clause = match (hav, free) {
                (true, Some(free)) => format!(
                    "((w.app_version_id = {} AND w.app_version_num IN (0, {vn})) \
                     OR (w.app_version_id = 0 AND {free}))",
                    av.id
                ),
                (true, None) => format!(
                    "(w.app_version_id = {} AND w.app_version_num IN (0, {vn}))",
                    av.id
                ),
                (false, Some(free)) => free,
                // Not reached: only an app with homogeneous app version keeps
                // such a version.
                (false, None) => "0 = 1".to_string(),
            };
            if let Some(spec) = ctx.plan_classes.find(&av.plan_class) {
                for (column, bound, upper) in [
                    ("w.id", spec.min_wu_id, false),
                    ("w.id", spec.max_wu_id, true),
                    ("w.batch", spec.min_batch, false),
                    ("w.batch", spec.max_batch, true),
                ] {
                    if bound != 0 {
                        clause.push_str(&format!(
                            " AND {column} {} {bound}",
                            if upper { "<=" } else { ">=" }
                        ));
                    }
                }
            }
            if !clauses.contains(&clause) {
                clauses.push(clause);
            }
        }
        if clauses.is_empty() {
            "0 = 1".to_string()
        } else {
            format!("({})", clauses.join(" OR "))
        }
    };
    let assigned_version_gate: String = assigned_list
        .iter()
        .map(|id| format!(" AND (w.appid != {id} OR {})", version_gate(*id)))
        .collect();
    let assigned_class_gate: String = assigned_list
        .iter()
        .filter(|id| homog[id].hr_type != 0)
        .map(|id| {
            format!(
                " AND NOT (w.appid = {id} AND w.hr_class NOT IN (0, {}))",
                homog[id].host_class
            )
        })
        .collect();
    if want_assigned {
        reads.push(stmt!(
            format!(
                "SELECT {CANDIDATE_COLS} FROM assignment a \
                 JOIN workunit w ON w.id = a.workunitid \
                 JOIN result r ON r.id = (SELECT min(r3.id) FROM result r3 \
                 WHERE r3.workunitid = a.workunitid AND r3.server_state = 1) \
                 WHERE a.multi = 0 AND ((a.target_type = ? AND a.target_id = ?) \
                 OR (a.target_type = ? AND a.target_id = ?)) \
                 AND w.error_mask = 0 AND w.canonical_resultid = 0 \
                 AND w.appid IN ({assigned_apps}){assigned_version_gate}{assigned_class_gate} \
                 AND NOT EXISTS (SELECT 1 FROM result r2 WHERE r2.workunitid = a.workunitid \
                 AND r2.hostid = ?) \
                 ORDER BY a.id LIMIT ?"
            ),
            ASSIGN_HOST,
            host.id,
            ASSIGN_USER,
            user.id,
            host.id,
            MAX_ASSIGNED as i64
        ));
    }
    let fetch = (3 * max_send + 20).min(100);
    // Each app's query up to its `ORDER BY`, to read the next window with.
    let mut app_queries: Vec<(i64, String, Vec<cb_db::Value>)> = Vec::new();
    for id in &app_ids {
        // Workunits committed to another class or version are left out in the
        // query, so that a window of them at the head of the queue cannot hide
        // the ones this host can be sent. The version IDs are integers of the
        // catalog.
        let h = homog[id];
        let mut sql = format!(
            "SELECT {CANDIDATE_COLS} FROM result r JOIN workunit w ON w.id = r.workunitid \
             WHERE r.server_state = 2 AND r.appid = ?"
        );
        let mut params: Vec<cb_db::Value> = vec![(*id).into()];
        if h.hr_type != 0 {
            sql.push_str(" AND w.hr_class IN (0, ?)");
            params.push(h.host_class.into());
        }
        sql.push_str(&format!(" AND {}", version_gate(*id)));
        if replica_rule {
            // A workunit the host or user already has a result of
            // (`slow_check` in `sched/sched_check.cpp`).
            sql.push_str(&format!(
                " AND NOT EXISTS (SELECT 1 FROM result r2 WHERE r2.workunitid = w.id \
                 AND r2.{} = ?)",
                replica_who.0
            ));
            params.push(replica_who.1.into());
        }
        app_queries.push((*id, sql.clone(), params.clone()));
        sql.push_str(" ORDER BY r.priority DESC, r.id LIMIT ?");
        params.push(fetch.into());
        reads.push(Stmt { sql, params });
    }
    let probe_ids: HashSet<i64> = probe_apps.iter().map(|a| a.id).collect();
    if !probe_apps.is_empty() {
        // No bound parameters, so the number of apps cannot reach D1's limit
        // on them; the list is filtered here.
        reads.push(Stmt::new(
            "SELECT a.id AS workunitid FROM app a WHERE EXISTS \
             (SELECT 1 FROM result r WHERE r.server_state = 2 AND r.appid = a.id)",
        ));
    }
    let mut lost_rows: Vec<Candidate> = Vec::new();
    let mut assigned_rows: Vec<Candidate> = Vec::new();
    let mut apps_with_work: Vec<i64> = Vec::new();
    let mut queues: Vec<(i64, std::collections::VecDeque<Candidate>)> = Vec::new();
    // Apps whose window came back full, with the priority and ID of its last
    // row: where the next window starts.
    let mut windows: Vec<(i64, i32, i64)> = Vec::new();
    if !reads.is_empty() {
        let mut rows = ctx.db.batch(reads).await?.into_iter();
        if !lost.is_empty() {
            lost_rows = rows.next().unwrap_or_default().decode()?;
        }
        if want_assigned {
            assigned_rows = rows.next().unwrap_or_default().decode()?;
        }
        for id in &app_ids {
            let mut c: Vec<Candidate> = rows.next().unwrap_or_default().decode()?;
            // A full window may have more behind it.
            if let Some(last) = c.last().filter(|_| c.len() >= fetch) {
                windows.push((*id, last.result_priority, last.result_id));
            }
            shuffle_within_priority(&mut c, ctx.random);
            queues.push((*id, c.into()));
        }
        if !probe_apps.is_empty() {
            let found: Vec<WuIdRow> = rows.next().unwrap_or_default().decode()?;
            apps_with_work = found
                .into_iter()
                .map(|r| r.workunitid)
                .filter(|id| probe_ids.contains(id))
                .collect();
        }
    }

    // HR apps with no candidate for this host: if the request ends up with
    // nothing, one probe says whether that is because their unsent work is
    // committed to other classes.
    let hr_empty: Vec<(i64, i32)> = queues
        .iter()
        .filter(|(id, q)| q.is_empty() && homog[id].hr_type != 0)
        .map(|(id, _)| (*id, homog[id].host_class))
        .collect();

    let mut outgoing: Vec<Outgoing> = Vec::new();
    // Jobs in progress on CPU and on GPU, and their limits. A lost result
    // recorded again for the other kind of resource moves from one count to
    // the other.
    let cpu_limit = i64::from(config.max_wus_in_progress.max(0)) * i64::from(ncpus);
    let gpu_limit = i64::from(config.max_wus_in_progress_gpu.max(0)) * i64::from(ngpus.max(1));
    let mut n_gpu = in_progress
        .iter()
        .filter(|r| is_gpu_version(r.app_version_id))
        .count() as i64;
    let mut n_cpu = in_progress.len() as i64 - n_gpu;
    let total_limit = i64::from(prefs.max_jobs_in_progress);

    // Lost results (`resend_lost_work`).
    for c in lost_rows {
        let Some(ip) = in_progress.iter().find(|r| r.id == c.result_id) else {
            continue;
        };
        let was_gpu = is_gpu_version(ip.app_version_id);
        let usable: Option<Sending> = if anonymous {
            // `resend_lost_work` calls `get_app_version` for the job, which
            // scans the client's versions whatever the lost result was sent
            // with: one for a resource it wants work for, with daily quota
            // left. The result is then recorded with that version
            // (`add_result_to_reply`), below.
            //
            // Unlike upstream, which takes the fastest and counts the jobs in
            // progress from the client's list, so without the lost ones, a
            // result stays on the resource it is recorded on when the client
            // still has a usable version for it; only otherwise does it move,
            // and then to a kind of resource that has room under its limit
            // on jobs in progress.
            // An app with homogeneous app version is never sent to the
            // client's own versions, a resend included.
            catalog
                .app(c.wu.appid)
                .filter(|app| app.homogeneous_app_version == 0)
                .and_then(|app| {
                    let wanted = |pt: usize| need_secs[pt] > 0.0 || need_inst[pt] > 0.0;
                    let need_64b = c.wu.rsc_memory_bound > 2.0 * GIGA;
                    let recorded = anon_proc_type(ip.app_version_id).map(proc_index);
                    let same = recorded.and_then(|r| {
                        pick_anonymous(&versions, &rows, app, need_64b, &|pt| pt == r && wanted(pt))
                            .chosen
                    });
                    let room = |pt: usize| {
                        let gpu = pt != 0;
                        let (count, limit) = if gpu {
                            (n_gpu, gpu_limit)
                        } else {
                            (n_cpu, cpu_limit)
                        };
                        gpu == was_gpu || limit <= 0 || count < limit
                    };
                    same.or_else(|| {
                        pick_anonymous(&versions, &rows, app, need_64b, &|pt| {
                            wanted(pt) && room(pt)
                        })
                        .chosen
                    })
                    .map(|i| versions[i].sending())
                })
        } else {
            catalog.app_version(ip.app_version_id).and_then(|av| {
                usage_for(
                    av,
                    &host_platforms,
                    &req,
                    ctx.plan_classes,
                    &plan_host,
                    havs.get(&av.id),
                )
                .ok()
                .map(|(hu, platform, _)| Sending {
                    av_id: av.id,
                    result_av_id: av.id,
                    gavid: av.id,
                    anon: false,
                    version_num: av.version_num,
                    plan_class: av.plan_class.clone(),
                    platform: platform.to_string(),
                    fpops_scale: 1.0,
                    hu,
                })
            })
        };
        // `wu_is_infeasible_fast` runs for a resend too: HR must still place
        // the host in the class the workunit is committed to.
        let placed = homog
            .get(&c.wu.appid)
            .is_none_or(|h| h.hr_type == 0 || (!h.unknown && !h.other_class(&c.wu)));
        let wanted = placed
            && c.wu.error_mask == 0
            && c.wu.canonical_resultid == 0
            && c.wu.assimilate_state == ASSIMILATE_INIT;
        let Some(version) = usable.filter(|_| wanted) else {
            // Nothing to gain from sending it again: let it time out now.
            writes.push(stmt!(
                "UPDATE result SET report_deadline = ?, mod_time = ? \
                 WHERE id = ? AND server_state = 4",
                now - 1,
                now,
                c.result_id
            ));
            writes.push(stmt!(
                "UPDATE workunit SET transition_time = ?, rev = rev + 1 WHERE id = ?",
                now,
                c.wu.id
            ));
            transition.push(c.wu.id);
            out.reply.messages.push(UserMessage::new(
                format!("Didn't resend lost task {} (expired)", c.result_name),
                "low",
            ));
            continue;
        };
        let mut deadline = c.report_deadline;
        let new_deadline = deadline <= now;
        if new_deadline {
            // Past its deadline: allow half the original time again.
            deadline = (now + ((c.report_deadline - c.sent_time) / 2).max(60))
                .min(now + c.wu.delay_bound.max(60));
        }
        // Sent again with the version chosen now, the result is recorded with
        // it as `add_result_to_reply` does: the resource type it uses and its
        // peak speed. On the anonymous platform the client's versions can
        // have changed since, and the statistics and limits follow the
        // result's row. One statement with the deadline's.
        //
        // The result's `sent_time` is the time of this sending, as
        // `possibly_give_result_new_deadline` in `sched/sched_resend.cpp`
        // sets it. The uploads need the attempt number of each of the
        // result's files, in `upload_epoch` (`current_epoch` in
        // `crates/cb-core/src/upload.rs`), which is written below by name: a
        // file's name is whatever the output template made of it, so the
        // result cannot be told from it.
        let rerecord = version.anon
            && (ip.app_version_id != version.result_av_id
                || ip.flops_estimate != version.hu.peak_flops);
        {
            let mut sql = String::from("UPDATE result SET sent_time = ?, ");
            let mut params: Vec<cb_db::Value> = vec![now.into()];
            if new_deadline {
                sql.push_str("report_deadline = ?, ");
                params.push(deadline.into());
            }
            if rerecord {
                sql.push_str("app_version_id = ?, flops_estimate = ?, ");
                params.push(version.result_av_id.into());
                params.push(version.hu.peak_flops.into());
                let now_gpu = version.hu.proc_type != PROC_TYPE_CPU;
                if now_gpu != was_gpu {
                    let (to, from) = if now_gpu {
                        (&mut n_gpu, &mut n_cpu)
                    } else {
                        (&mut n_cpu, &mut n_gpu)
                    };
                    *to += 1;
                    *from -= 1;
                }
                // A row for the new resource may not exist yet.
                if let Some(row) = rows.get_mut(&version.gavid) {
                    if !row.has_row {
                        row.has_row = true;
                        writes.push(stmt!(
                            "INSERT INTO host_app_version (host_id, app_version_id, \
                             max_jobs_per_day, n_jobs_today) VALUES (?, ?, ?, 0) \
                             ON CONFLICT DO NOTHING",
                            host.id,
                            version.gavid,
                            daily_quota
                        ));
                    }
                }
            }
            sql.push_str("mod_time = ? WHERE id = ? AND server_state = 4");
            params.push(now.into());
            params.push(c.result_id.into());
            writes.push(Stmt { sql, params });
            // The task runs again: a new attempt at each upload file, so
            // that pieces of the lost one are never taken for its own.
            for f in output_files(&c.xml_doc_in).iter().filter(|f| f.uploadable) {
                writes.push(stmt!(
                    "INSERT INTO upload_epoch (name, result_id, epoch) VALUES (?, ?, 1) \
                     ON CONFLICT (name) DO UPDATE SET result_id = excluded.result_id, \
                     epoch = upload_epoch.epoch + 1",
                    f.name.as_str(),
                    c.result_id
                ));
            }
        }
        if new_deadline {
            writes.push(stmt!(
                "UPDATE workunit SET transition_time = min(transition_time, ?), rev = rev + 1 \
                 WHERE id = ?",
                deadline + i64::from(config.report_grace_period),
                c.wu.id
            ));
        }
        out.reply.messages.push(UserMessage::new(
            format!("Resent lost task {}", c.result_name),
            "low",
        ));
        outgoing.push(Outgoing {
            result_name: c.result_name,
            xml_doc_in: c.xml_doc_in,
            wu: c.wu,
            version,
            deadline,
            claim: None,
            assigned: false,
        });
    }

    // When `process_request` has resent lost tasks it sends nothing else
    // ("Resent lost jobs, don't send more" in sched/handle_request.cpp), and
    // does not say why it sent no more.
    let resent_lost = !outgoing.is_empty();
    let had_candidates = queues.iter().any(|(_, q)| !q.is_empty()) || !assigned_rows.is_empty();
    if resent_lost {
        queues.clear();
        assigned_rows.clear();
    } else if let Some(m) = too_recent_message.take() {
        out.reply.messages.push(UserMessage::new(m, "low"));
    }

    // Interleave the apps' candidates in proportion to app weight.
    let merge = |queues: &mut Vec<(i64, std::collections::VecDeque<Candidate>)>| {
        let mut merged: Vec<Candidate> = Vec::new();
        let mut taken: Vec<f64> = vec![0.0; queues.len()];
        loop {
            let mut best: Option<(usize, f64)> = None;
            for (i, (app_id, q)) in queues.iter().enumerate() {
                if q.is_empty() {
                    continue;
                }
                let weight =
                    catalog
                        .app(*app_id)
                        .map_or(1.0, |a| if a.weight > 0.0 { a.weight } else { 1.0 });
                let key = (taken[i] + 1.0) / weight;
                if best.is_none_or(|(_, k)| key < k) {
                    best = Some((i, key));
                }
            }
            let Some((i, _)) = best else {
                break;
            };
            taken[i] += 1.0;
            if let Some(c) = queues[i].1.pop_front() {
                merged.push(c);
            }
        }
        merged
    };
    let mut merged = merge(&mut queues);

    // Choose (`send_work`, `wu_is_infeasible_fast`).
    let gpu_ids = if gpu_version_ids.is_empty() {
        "0".to_string()
    } else {
        gpu_version_ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    let mut disk = disk_available(&req, config);
    let ram = usable_ram(&req);
    let est_secs = |wu: &Workunit, hu: &HostUsage, pt: usize| {
        let mut avail = unit_frac(req.time_stats.on_frac) * unit_frac(req.time_stats.active_frac);
        if pt != 0 {
            avail *= unit_frac(req.time_stats.gpu_active_frac);
        }
        wu.rsc_fpops_est / hu.projected_flops / avail.max(0.1)
    };
    let mut skipped = Skipped::default();
    // Messages for a client that gets no work, `add_no_work_message`.
    let mut no_work_messages: Vec<String> = Vec::new();
    // Anonymous platform: apps already found to have no usable client
    // version, by whether the job needs a 64-bit version.
    let mut no_usable: HashSet<(i64, bool)> = HashSet::new();
    let mut sent_wus: HashSet<i64> = outgoing.iter().map(|o| o.wu.id).collect();
    let mut n_new = 0;
    let mut n_assigned = 0;
    // Assigned jobs first. Like upstream, they skip the feasibility checks
    // and limits but need a usable app version and a request for work; when
    // any is sent, the reply carries no other new work.
    let mut round = 0;
    loop {
        for assigned in [true, false] {
            let list = if assigned {
                std::mem::take(&mut assigned_rows)
            } else {
                if n_assigned > 0 {
                    break;
                }
                std::mem::take(&mut merged)
            };
            for c in list {
                if n_new >= max_send {
                    break;
                }
                if (0..NPROC).all(|pt| need_secs[pt] <= 0.0 && need_inst[pt] <= 0.0) {
                    break;
                }
                if sent_wus.contains(&c.wu.id) {
                    continue;
                }
                let Some(app) = catalog.app(c.wu.appid) else {
                    continue;
                };
                let hom = homog[&app.id];
                // Homogeneous redundancy (`wu_is_infeasible_fast`): a host the
                // type cannot place gets no unsent work, and none of a workunit
                // committed to another class. Assigned jobs skip the first, as
                // `send_assigned_job` does, and are held to the second by the
                // claim.
                if hom.hr_type != 0 {
                    if hom.unknown && !assigned {
                        continue;
                    }
                    if hom.other_class(&c.wu) {
                        skipped.hr_class = true;
                        continue;
                    }
                }
                // A job that needs more address space than a 32-bit app has goes only with
                // a version for a platform whose name has "64" in it, and the platform
                // that sets the limit is the client's primary one: the first test in the
                // scan of `get_app_version`, which a workunit committed to a version
                // (`check_homogeneous_app_version`) does not reach.
                let committed = hom.hav && c.wu.app_version_id != 0;
                let need_64b = c.wu.rsc_memory_bound > max_32b_address_space(&req.platform_name);
                let admitted = |v: &Version<'_>| {
                    v.appid == c.wu.appid
                        && match v.av {
                            Some(av) => {
                                // Homogeneous app version: a committed workunit
                                // goes only with its version, even one stock
                                // no longer loads
                                // (`check_homogeneous_app_version`).
                                // Otherwise a workunit pinned to a version
                                // number goes with that number, if the
                                // scheduler loads it, whether or not the
                                // version is still sent to everyone, and an
                                // unpinned one with a version that is
                                // (`get_app_version` in
                                // `sched/sched_version.cpp`).
                                (if committed {
                                    c.wu.app_version_id == av.id
                                } else if c.wu.app_version_num != 0 {
                                    !v.commit_only
                                } else {
                                    !v.pinned_only
                                }) && (committed || !need_64b || is_64b_platform(v.platform))
                                    && (c.wu.app_version_num == 0
                                        || av.version_num == c.wu.app_version_num)
                                    && ctx.plan_classes.admits_workunit(
                                        &av.plan_class,
                                        c.wu.id,
                                        c.wu.batch,
                                    )
                            }
                            // The client's own versions are never used for an
                            // app with homogeneous app version. A workunit
                            // pinned to a version number is sent to them
                            // regardless, as upstream's
                            // `get_app_version_anonymous` does not look at it.
                            None => !hom.hav,
                        }
                };
                let mut chosen: Option<(usize, usize, f64)> = None;
                if anonymous {
                    // The client's own versions: the fastest one that passes
                    // `get_app_version_anonymous`, with no fall back to a slower
                    // one when the job does not fit it (`wu_is_infeasible_fast`
                    // runs on the one version chosen). Assigned jobs skip the
                    // feasibility checks. A resource whose limit on jobs in
                    // progress is reached is one the client no longer needs work
                    // for (`work_needed` clears its request).
                    let at_limit = |pt: usize| {
                        let (count, limit) = if pt != 0 {
                            (n_gpu, gpu_limit)
                        } else {
                            (n_cpu, cpu_limit)
                        };
                        (limit > 0 && count >= limit)
                            || (total_limit > 0 && n_cpu + n_gpu >= total_limit)
                    };
                    for pt in 0..NPROC {
                        if !assigned && (need_secs[pt] > 0.0 || need_inst[pt] > 0.0) && at_limit(pt)
                        {
                            skipped.in_progress_limit = true;
                        }
                    }
                    let needed = |pt: usize| {
                        (need_secs[pt] > 0.0 || need_inst[pt] > 0.0) && (assigned || !at_limit(pt))
                    };
                    let need_64b = c.wu.rsc_memory_bound > 2.0 * GIGA;
                    let pick = if !hom.hav {
                        pick_anonymous(&versions, &rows, app, need_64b, &needed)
                    } else {
                        AnonPick {
                            chosen: None,
                            found: true,
                            quota_hit: Vec::new(),
                        }
                    };
                    for q in &pick.quota_hit {
                        skipped.quota = true;
                        let m = format!("Daily job limit ({q}) has been reached");
                        if !no_work_messages.contains(&m) {
                            no_work_messages.push(m);
                        }
                    }
                    if !pick.found && no_usable.insert((app.id, need_64b)) {
                        no_work_messages.push(format!(
                            "Your app_info.xml file doesn't have a usable version of {}.",
                            app.user_friendly_name
                        ));
                    }
                    if let Some(i) = pick.chosen {
                        let v = &versions[i];
                        let pt = proc_index(v.hu.proc_type);
                        let est = est_secs(&c.wu, &v.hu, pt);
                        let fits = assigned || {
                            let mem = if v.hu.mem_usage > 0.0 {
                                v.hu.mem_usage
                            } else {
                                c.wu.rsc_memory_bound
                            };
                            if ram > 0.0 && mem > ram {
                                skipped.memory = true;
                                false
                            } else if c.wu.rsc_disk_bound > disk {
                                skipped.disk = true;
                                false
                            } else if !config.ignore_delay_bound
                                && est_delay[pt] + est > c.wu.delay_bound as f64
                            {
                                skipped.speed = true;
                                false
                            } else {
                                true
                            }
                        };
                        if fits {
                            chosen = Some((i, pt, est));
                        }
                    }
                } else if assigned {
                    // The fastest version, preferring one for a resource the
                    // client asks for (upstream's `get_app_version` does not
                    // look at the request for an assigned job).
                    let wanted = |v: &Version<'_>| {
                        let pt = proc_index(v.hu.proc_type);
                        need_secs[pt] > 0.0 || need_inst[pt] > 0.0
                    };
                    chosen = versions
                        .iter()
                        .position(|v| admitted(v) && wanted(v))
                        .or_else(|| versions.iter().position(admitted))
                        .map(|i| {
                            let pt = proc_index(versions[i].hu.proc_type);
                            (i, pt, est_secs(&c.wu, &versions[i].hu, pt))
                        });
                } else {
                    // `prefer_primary_platform`: the scan of `get_app_version` stops at the
                    // first platform with a version that passes the checks up to the
                    // limits, so a later platform's version is used only when none of the
                    // earlier ones does. Within that platform the search below goes on to
                    // the next version when the job does not fit one, as it does without
                    // the option.
                    let min_rank = if config.prefer_primary_platform {
                        versions
                            .iter()
                            .filter(|v| {
                                let pt = proc_index(v.hu.proc_type);
                                let (count, limit) = if v.gpu {
                                    (n_gpu, gpu_limit)
                                } else {
                                    (n_cpu, cpu_limit)
                                };
                                let row = &rows[&v.gavid];
                                admitted(v)
                                    && (need_secs[pt] > 0.0 || need_inst[pt] > 0.0)
                                    && row.n_today < row.quota
                                    && !(limit > 0 && count >= limit)
                                    && !(total_limit > 0 && n_cpu + n_gpu >= total_limit)
                            })
                            .map(|v| v.rank)
                            .min()
                    } else {
                        None
                    };
                    for (i, v) in versions.iter().enumerate() {
                        if !admitted(v) || min_rank.is_some_and(|r| v.rank != r) {
                            continue;
                        }
                        let pt = proc_index(v.hu.proc_type);
                        if need_secs[pt] <= 0.0 && need_inst[pt] <= 0.0 {
                            continue;
                        }
                        let row = &rows[&v.gavid];
                        if row.n_today >= row.quota {
                            skipped.quota = true;
                            continue;
                        }
                        let (count, limit) = if v.gpu {
                            (n_gpu, gpu_limit)
                        } else {
                            (n_cpu, cpu_limit)
                        };
                        if (limit > 0 && count >= limit)
                            || (total_limit > 0 && n_cpu + n_gpu >= total_limit)
                        {
                            skipped.in_progress_limit = true;
                            continue;
                        }
                        let mem = if v.hu.mem_usage > 0.0 {
                            v.hu.mem_usage
                        } else {
                            c.wu.rsc_memory_bound
                        };
                        if ram > 0.0 && mem > ram {
                            skipped.memory = true;
                            continue;
                        }
                        if c.wu.rsc_disk_bound > disk {
                            skipped.disk = true;
                            continue;
                        }
                        let est = est_secs(&c.wu, &v.hu, pt);
                        if !config.ignore_delay_bound
                            && est_delay[pt] + est > c.wu.delay_bound as f64
                        {
                            skipped.speed = true;
                            continue;
                        }
                        chosen = Some((i, pt, est));
                        break;
                    }
                }
                let Some((i, pt, est)) = chosen else {
                    continue;
                };
                let v = &versions[i];
                let row = rows.get_mut(&v.gavid).expect("a row for every version");
                let usage = if pt == 0 {
                    v.hu.avg_ncpus
                } else {
                    v.hu.gpu_usage
                }
                .max(0.01);
                need_secs[pt] -= est * usage;
                need_inst[pt] -= usage;
                est_delay[pt] += est * usage / ninstances[pt].max(1.0);
                disk -= c.wu.rsc_disk_bound;
                row.n_today += 1;
                if v.gpu {
                    n_gpu += 1;
                } else {
                    n_cpu += 1;
                }
                n_new += 1;
                if assigned {
                    n_assigned += 1;
                }
                sent_wus.insert(c.wu.id);

                if !row.has_row {
                    row.has_row = true;
                    writes.push(stmt!(
                        "INSERT INTO host_app_version (host_id, app_version_id, max_jobs_per_day, \
                     n_jobs_today) VALUES (?, ?, ?, 0) ON CONFLICT DO NOTHING",
                        host.id,
                        v.gavid,
                        daily_quota
                    ));
                }

                let deadline = now + c.wu.delay_bound;
                let claim = Claim {
                    result_id: c.result_id,
                    wu_id: c.wu.id,
                    host_id: host.id,
                    user_id: user.id,
                    team_id: user.teamid,
                    now,
                    deadline,
                    result_av_id: v.result_av_id,
                    version_num: if v.anonymous() { 0 } else { v.version_num },
                    gavid: v.gavid,
                    peak_flops: v.hu.peak_flops,
                    from_state: if assigned {
                        RESULT_SERVER_STATE_INACTIVE
                    } else {
                        RESULT_SERVER_STATE_UNSENT
                    },
                    limits: (!assigned).then(|| ClaimLimits {
                        resource: if v.gpu { gpu_limit } else { cpu_limit },
                        gpu: v.gpu,
                        gpu_ids: &gpu_ids,
                        total: total_limit,
                        quota: row.quota,
                    }),
                    // Upstream sends at most one instance of an assigned job to
                    // a host, whatever the replica rule says.
                    one_per: if assigned {
                        Some(("hostid", host.id))
                    } else {
                        replica_rule.then_some(replica_who)
                    },
                    commit: hom.commit(v),
                };
                let claim_at = writes.len();
                writes.push(claim.stmt());

                // What follows the claim happens only if the claim did.
                const CLAIMED: &str = "EXISTS (SELECT 1 FROM result WHERE id = ? AND hostid = ? \
                 AND server_state = 4 AND sent_time = ?)";
                // The counter moves only if this request's claim, the statement
                // just before, changed a row. `CLAIMED` cannot tell it from a
                // claim of the same result by another request of this host in the
                // same second, which this request may have lost to.
                writes.push(stmt!(
                    "UPDATE host_app_version SET n_jobs_today = n_jobs_today + 1 \
                 WHERE host_id = ? AND app_version_id = ? AND (SELECT changes()) > 0",
                    host.id,
                    v.gavid
                ));
                // A job sent unreplicated to a host that has not earned it gets
                // the app's replication after all. Not an assigned job: its
                // extra results could go nowhere but its own host or user.
                let trusted = row.consecutive_valid >= CONS_VALID_UNREPLICATED
                    && unit(ctx.random) > 1.0 / f64::from(row.consecutive_valid);
                // The same statement commits the workunit to the host's class and
                // to the version (`update_wu_on_send`): the claim above held only
                // if the workunit was uncommitted or committed to the same.
                let (commit_set, commit_params) = claim.commit.set();
                // The update also checks the workunit's own commitment: another
                // request of this host may have claimed the result in the same
                // second, which `CLAIMED` cannot tell from this request's claim.
                let (row_guard, guard_params) = claim.commit.row_guard();
                let tail: Vec<cb_db::Value> = std::iter::once(c.wu.id.into())
                    .chain(guard_params)
                    .chain([c.result_id.into(), host.id.into(), now.into()])
                    .collect();
                if !assigned && app.target_nresults > 1 && c.wu.target_nresults == 1 && !trusted {
                    let mut params: Vec<cb_db::Value> = vec![
                        app.target_nresults.into(),
                        app.target_nresults.into(),
                        now.into(),
                    ];
                    params.extend(commit_params);
                    params.extend(tail);
                    writes.push(Stmt {
                        sql: format!(
                            "UPDATE workunit SET target_nresults = ?, min_quorum = ?, \
                         transition_time = ?, rev = rev + 1{commit_set} \
                         WHERE id = ?{row_guard} AND {CLAIMED}"
                        ),
                        params,
                    });
                } else {
                    let mut params: Vec<cb_db::Value> =
                        vec![(deadline + i64::from(config.report_grace_period)).into()];
                    params.extend(commit_params);
                    params.extend(tail);
                    writes.push(Stmt {
                        sql: format!(
                            "UPDATE workunit SET transition_time = min(transition_time, ?), \
                         rev = rev + 1{commit_set} WHERE id = ?{row_guard} AND {CLAIMED}"
                        ),
                        params,
                    });
                }
                outgoing.push(Outgoing {
                    result_name: c.result_name,
                    xml_doc_in: c.xml_doc_in,
                    wu: c.wu,
                    version: v.sending(),
                    deadline,
                    claim: Some(claim_at),
                    assigned,
                });
            }
        }

        // The window of an app can hold only jobs that this host cannot take for
        // reasons the query does not see: memory, disk or deadline against the
        // host. While work is still wanted and some window was full, read the
        // next one, up to `MAX_WINDOWS` in all.
        round += 1;
        let still_needed = (0..NPROC).any(|pt| need_secs[pt] > 0.0 || need_inst[pt] > 0.0);
        // Another window can only help if some version could still take a
        // job: one whose resource the client still wants work for, whose
        // daily quota and limits on jobs in progress are not used up, and
        // with disk left. (Assigned jobs sent, a full reply, a request
        // satisfied, and a reply that resent lost tasks, which sends nothing
        // else, stop it too; the other reasons a host gets no work, `dont_send_jobs`,
        // a refused host, a client that is too old, never reach here: they
        // leave `windows` empty.)
        let a_version_is_open = versions.iter().any(|v| {
            let pt = proc_index(v.hu.proc_type);
            let (count, limit) = if v.gpu {
                (n_gpu, gpu_limit)
            } else {
                (n_cpu, cpu_limit)
            };
            (need_secs[pt] > 0.0 || need_inst[pt] > 0.0)
                && rows.get(&v.gavid).is_some_and(|r| r.n_today < r.quota)
                && !(limit > 0 && count >= limit)
                && !(total_limit > 0 && n_cpu + n_gpu >= total_limit)
        });
        if n_assigned > 0
            || resent_lost
            || n_new >= max_send
            || !still_needed
            || !a_version_is_open
            || disk <= 0.0
            || round >= MAX_WINDOWS
            || windows.is_empty()
        {
            break;
        }
        let mut reads = Vec::new();
        let mut ids = Vec::new();
        for (id, priority, last) in std::mem::take(&mut windows) {
            let Some((_, base, base_params)) = app_queries.iter().find(|q| q.0 == id) else {
                continue;
            };
            let sql = format!(
                "{base} AND (r.priority < ? OR (r.priority = ? AND r.id > ?)) \
             ORDER BY r.priority DESC, r.id LIMIT ?"
            );
            let mut params = base_params.clone();
            params.extend([priority.into(), priority.into(), last.into(), fetch.into()]);
            reads.push(Stmt { sql, params });
            ids.push(id);
        }
        queues.clear();
        for (id, rows) in ids.into_iter().zip(ctx.db.batch(reads).await?) {
            let mut c: Vec<Candidate> = rows.decode()?;
            if let Some(last) = c.last().filter(|_| c.len() >= fetch) {
                windows.push((id, last.result_priority, last.result_id));
            }
            shuffle_within_priority(&mut c, ctx.random);
            queues.push((id, c.into()));
        }
        merged = merge(&mut queues);
        if merged.is_empty() {
            break;
        }
    }

    // Batch 5: everything this request changes.
    let written = match ctx.db.batch(writes).await {
        Ok(w) => w,
        Err(e) => {
            let e = CoreError::from(e);
            if !is_guard_failure(&e) {
                return Err(e);
            }
            // Nothing was written. Reply as to an unknown key, without what
            // the request had put in the reply so far.
            let mut refused = SchedOutcome {
                user_id: out.user_id,
                host_id: out.host_id,
                ..Default::default()
            };
            refused.reply.nucleus_only = true;
            refuse(
                &mut refused,
                "Invalid or missing account key.  To fix, remove and add this project.",
                "notice",
                DELAY_MISSING_KEY,
                "bad authenticator",
            );
            return Ok(refused);
        }
    };

    // The reply carries the results whose claims went through.
    let mut sent_versions: Vec<i64> = Vec::new();
    let mut bumped: Vec<i64> = Vec::new();
    for o in outgoing {
        match o.claim {
            Some(i) => {
                if written.get(i).is_none_or(|r| r.rows.is_empty()) {
                    continue;
                }
                out.n_sent += 1;
            }
            None => out.n_resent += 1,
        }
        let Some(app) = catalog.app(o.wu.appid) else {
            continue;
        };
        let version = &o.version;
        // The client of the anonymous platform has its own apps and app
        // versions: the reply carries neither `<app>` nor `<app_version>`
        // for them (`add_wu_to_reply` in `sched/sched_send.cpp`), and no
        // version files.
        if !version.anon {
            let Some(av) = catalog.app_version(version.av_id) else {
                continue;
            };
            if !out.reply.apps.iter().any(|a| a.name == app.name) {
                out.reply.apps.push(ReplyApp {
                    name: app.name.clone(),
                    user_friendly_name: app.user_friendly_name.clone(),
                    non_cpu_intensive: app.non_cpu_intensive != 0,
                    fraction_done_exact: app.fraction_done_exact != 0,
                });
            }
            if !sent_versions.contains(&av.id) {
                sent_versions.push(av.id);
                out.reply.app_versions.push(ReplyAppVersion {
                    xml_doc: av.xml_doc.clone(),
                    platform: version.platform.clone(),
                    plan_class: av.plan_class.clone(),
                    avg_ncpus: version.hu.avg_ncpus,
                    flops: version.hu.projected_flops,
                    cmdline: version.hu.cmdline.clone(),
                    proc_type: version.hu.proc_type,
                    gpu_usage: version.hu.gpu_usage,
                    gpu_ram: version.hu.gpu_ram,
                });
            }
        }
        out.reply.wus.push(
            insert_wu_tags(
                &o.wu.xml_doc,
                &WuTags {
                    // Scaled for the client's own idea of the version's speed
                    // (`rsc_fpops_scale`); 1 for the project's versions.
                    rsc_fpops_est: scaled_size(o.wu.rsc_fpops_est, version.fpops_scale),
                    rsc_fpops_bound: scaled_size(o.wu.rsc_fpops_bound, version.fpops_scale),
                    rsc_memory_bound: o.wu.rsc_memory_bound,
                    rsc_disk_bound: o.wu.rsc_disk_bound,
                    name: &o.wu.name,
                    app_name: &app.name,
                    job_keyword_ids: &o.wu.keywords,
                },
            )
            .map_err(proto)?,
        );
        let xml = insert_name_tags(&o.xml_doc_in, &o.result_name, &o.wu.name).map_err(proto)?;
        out.reply.results.push(ReplyResult {
            xml_doc_in: insert_deadline_tag(&xml, o.deadline).map_err(proto)?,
            platform: version.platform.clone(),
            version_num: version.version_num,
            plan_class: version.plan_class.clone(),
        });
        if o.claim.is_some() && !o.assigned && app.target_nresults > 1 && o.wu.target_nresults == 1
        {
            bumped.push(o.wu.id);
        }
    }
    // A workunit whose replication may have been raised needs its extra
    // results made now rather than at the next sweep.
    transition.extend(bumped);

    if hr_blocked
        && work_requested
        && !too_old
        && key_ok
        && !too_recent
        && !config.dont_send_jobs
        && !req.dont_send_work
    {
        // Upstream's `send_work` returns before it writes this message
        // (`hr_reject_perm`), so its client is told nothing.
        out.reply.messages.push(UserMessage::new(
            "Your computer type is not supported by this project",
            "notice",
        ));
    }
    // With no job to send at all, upstream's feeder cache is empty and
    // `process_request` says only that, whether or not it resent lost tasks;
    // "No tasks sent" is for when the cache holds jobs and none suits the host.
    // With assignment enabled, upstream does not look at the cache.
    //
    // Upstream's cache holds unsent results of any app and platform
    // (`SCHED_SHMEM::no_work`), so the host's own candidates are not what to
    // ask: a host with a version of app A, when only app B has unsent tasks,
    // finds a cache that is not empty. One probe asks the project, and runs
    // only when a message depends on it: not when the host has candidates
    // (the cache then holds jobs), nor on a request that was sent work.
    let no_jobs_base = !config.dont_send_jobs
        && !config.enable_assignment
        && !req.dont_send_work
        && !versions.is_empty()
        && send
        && apps_with_work.is_empty()
        && !skipped.hr_class;
    let says_something =
        resent_lost || (work_requested && !too_old && out.n_sent == 0 && key_ok && !too_recent);
    let no_jobs_cache = if no_jobs_base && says_something && !had_candidates {
        !unsent_work_exists(ctx.db).await?
    } else {
        false
    };
    if resent_lost && no_jobs_cache {
        out.reply
            .messages
            .push(UserMessage::new("Project has no tasks available", "low"));
    }
    if work_requested && !too_old && out.n_sent == 0 && key_ok && !too_recent && !resent_lost {
        // `hr_reject_temp`. The query leaves out workunits committed to other
        // classes, so for an HR app with nothing to offer this host, one
        // probe asks whether that is what it found.
        let mut committed_elsewhere = skipped.hr_class;
        if !committed_elsewhere && !hr_empty.is_empty() {
            let probes = hr_empty
                .iter()
                .map(|(id, class)| {
                    stmt!(
                        "SELECT 1 AS workunitid FROM result r \
                         JOIN workunit w ON w.id = r.workunitid \
                         WHERE r.server_state = 2 AND r.appid = ? \
                         AND w.hr_class NOT IN (0, ?) LIMIT 1",
                        *id,
                        *class
                    )
                })
                .collect();
            committed_elsewhere = ctx
                .db
                .batch(probes)
                .await?
                .iter()
                .any(|r| !r.rows.is_empty());
        }
        // Jobs committed to other classes are jobs in upstream's cache.
        let no_jobs = no_jobs_cache && !committed_elsewhere;
        if !no_jobs {
            out.reply
                .messages
                .push(UserMessage::new("No tasks sent", "low"));
        }
        let mut why: Vec<String> = Vec::new();
        if config.dont_send_jobs {
            why.push("Project is not sending tasks".into());
        } else if req.dont_send_work {
            why.push("Tasks are not being requested from this project".into());
        } else if versions.is_empty() {
            // On the anonymous platform it is the client that has no version.
            if plan_messages.is_empty() && !anonymous {
                why.push("No app version is available for this computer".into());
            }
        } else if no_jobs {
            // Not when the search never ran: a client on the anonymous
            // platform with no version for a resource it asks work for is
            // told only "No tasks sent", as upstream's `work_needed` stops
            // the search.
            why.push("Project has no tasks available".into());
        }
        why.extend(plan_messages.iter().map(|m| m.to_string()));
        if skipped.quota {
            why.push("This computer has reached a limit on tasks sent per day".into());
        }
        if skipped.in_progress_limit {
            why.push("This computer has reached a limit on tasks in progress".into());
        }
        if skipped.memory {
            why.push("Tasks need more memory than this computer makes available".into());
        }
        if skipped.disk {
            why.push("Tasks need more disk space than BOINC is allowed to use".into());
        }
        if skipped.speed {
            why.push("Tasks won't finish in time on this computer".into());
        }
        if committed_elsewhere {
            why.push("Tasks are committed to other platforms".into());
        }
        for w in why {
            out.reply.messages.push(UserMessage::new(w, "low"));
        }
        // Anonymous platform: apps with work that the client has no version
        // of, then what `get_app_version_anonymous` said while scanning.
        // These are `notice`s, as `add_no_work_message` makes them.
        for id in &apps_with_work {
            if let Some(app) = catalog.app(*id) {
                no_work_messages.push(format!(
                    "Your app_info.xml file doesn't have a usable version of {}.",
                    app.user_friendly_name
                ));
            }
        }
        for m in no_work_messages {
            if !out.reply.messages.iter().any(|x| x.message == m) {
                out.reply.messages.push(UserMessage::new(m, "notice"));
            }
        }
    }

    if config.min_sendwork_interval > 0 {
        // `SCHEDULER_REPLY::write` in sched/sched_types.cpp: 1% more than the
        // interval, for clock skew, and at least a second more.
        let min = f64::from(config.min_sendwork_interval);
        out.reply.request_delay = out.reply.request_delay.max((1.01 * min).max(min + 1.0));
    }
    out.reply.request_delay = out.reply.request_delay.min(DELAY_MAX);
    transition.sort_unstable();
    transition.dedup();
    out.transition = transition;
    out.log = format!(
        "user {} host {} reported {} sent {} resent {}",
        user.id, host.id, out.n_reported, out.n_sent, out.n_resent
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: i64, min_version: i32) -> App {
        App {
            id,
            min_version,
            ..Default::default()
        }
    }

    fn anon<'a>(
        appid: i64,
        version_num: i32,
        pt: i32,
        flops: f64,
        platform: &'a str,
    ) -> Version<'a> {
        Version {
            av: None,
            appid,
            result_av_id: anon_platform_id(pt),
            gavid: anon_gavid(appid, pt),
            version_num,
            plan_class: "",
            platform,
            rank: 0,
            fpops_scale: 1.0,
            hu: HostUsage {
                proc_type: pt,
                gpu_usage: 0.0,
                gpu_ram: 0.0,
                avg_ncpus: 1.0,
                mem_usage: 0.0,
                projected_flops: flops,
                peak_flops: flops,
                cmdline: String::new(),
            },
            gpu: pt != 0,
            score: flops,
            pinned_only: false,
            commit_only: false,
        }
    }

    /// One row per key, with room for 10 jobs a day.
    fn rows(versions: &[Version<'_>]) -> HashMap<i64, RowState> {
        versions
            .iter()
            .map(|v| {
                (
                    v.gavid,
                    RowState {
                        n_today: 0,
                        quota: 10,
                        consecutive_valid: 0,
                        has_row: false,
                    },
                )
            })
            .collect()
    }

    fn all_needed(_: usize) -> bool {
        true
    }

    #[test]
    fn projected_speed_follows_history_only_with_enough_samples() {
        // The client's figure stands without a row, or with few samples.
        assert_eq!(anon_projected_flops(1e9, None), (1e9, 1.0));
        let mut hav = HostAppVersion {
            et_n: MIN_HOST_SAMPLES,
            et_avg: 1e-8,
            ..Default::default()
        };
        assert_eq!(anon_projected_flops(1e9, Some(&hav)), (1e9, 1.0));
        // More than MIN_HOST_SAMPLES: 1 / average elapsed time per FLOP.
        hav.et_n = MIN_HOST_SAMPLES + 1.0;
        let (flops, scale) = anon_projected_flops(1e9, Some(&hav));
        assert!((flops - 1e8).abs() < 1.0);
        assert!((scale - 10.0).abs() < 1e-9);
        // No average yet.
        hav.et_avg = 0.0;
        assert_eq!(anon_projected_flops(1e9, Some(&hav)), (1e9, 1.0));
        // Capped at ET_RATIO_LIMIT times the stated speed.
        hav.et_avg = 1e-15;
        let (flops, scale) = anon_projected_flops(1e9, Some(&hav));
        assert_eq!(flops, 250.0 * 1e9);
        assert!((scale - 1.0 / 250.0).abs() < 1e-12);
    }

    #[test]
    fn the_fastest_passing_version_is_chosen_and_the_first_of_equals() {
        let a = app(1, 0);
        let versions = vec![
            anon(1, 100, PROC_TYPE_CPU, 1e9, "p"),
            anon(2, 100, PROC_TYPE_CPU, 9e9, "p"),
            anon(1, 101, PROC_TYPE_CPU, 2e9, "p"),
            anon(1, 102, PROC_TYPE_CPU, 2e9, "p"),
        ];
        let pick = pick_anonymous(&versions, &rows(&versions), &a, false, &all_needed);
        assert_eq!(pick.chosen, Some(2));
        assert!(pick.found);
    }

    #[test]
    fn versions_are_skipped_for_quota_age_resource_and_address_space() {
        let a = app(1, 105);
        let versions = vec![
            anon(1, 110, PROC_TYPE_CPU, 5e9, "windows_intelx86"),
            anon(1, 110, PROC_TYPE_CPU, 4e9, "windows_x86_64"),
            anon(1, 104, PROC_TYPE_CPU, 9e9, "windows_x86_64"),
            anon(1, 110, PROC_TYPE_NVIDIA_GPU, 8e9, "windows_x86_64"),
        ];
        // The GPU is not wanted, the 104 is below the app's minimum, and the
        // 32-bit version cannot take a job that needs 64 bits.
        let cpu_only = |pt: usize| pt == 0;
        let pick = pick_anonymous(&versions, &rows(&versions), &a, true, &cpu_only);
        assert_eq!(pick.chosen, Some(1));
        let pick = pick_anonymous(&versions, &rows(&versions), &a, false, &cpu_only);
        assert_eq!(pick.chosen, Some(0));

        // A version whose quota is used up is not "found" either.
        let mut full = rows(&versions);
        for row in full.values_mut() {
            row.n_today = row.quota;
        }
        let pick = pick_anonymous(&versions, &full, &a, false, &cpu_only);
        assert_eq!(pick.chosen, None);
        assert!(!pick.found);
        assert_eq!(pick.quota_hit, vec![10]);

        // Found, but not for a resource the client wants work for.
        let versions = vec![anon(1, 110, PROC_TYPE_NVIDIA_GPU, 8e9, "p")];
        let pick = pick_anonymous(&versions, &rows(&versions), &a, false, &cpu_only);
        assert_eq!(pick.chosen, None);
        assert!(pick.found);
    }

    #[test]
    fn the_size_factor_is_always_a_positive_finite_number() {
        let hav = |et_avg: f64| HostAppVersion {
            et_n: 50.0,
            et_avg,
            ..Default::default()
        };
        for stated in [MIN_ANON_FLOPS, 1e9, MAX_ANON_FLOPS] {
            for et_avg in [1e-300, 1e-30, 1e-9, 1.0, 1e30, 1e300, f64::MIN_POSITIVE] {
                let (flops, scale) = anon_projected_flops(stated, Some(&hav(et_avg)));
                assert!((MIN_ANON_FLOPS..=MAX_ANON_FLOPS).contains(&flops));
                assert!(scale.is_finite() && scale > 0.0, "{stated} {et_avg}");
                assert!(
                    (1e-11..=1e11).contains(&scale),
                    "{stated} {et_avg}: {scale}"
                );
            }
        }
        // A size the factor would carry out of range stays as it was.
        assert_eq!(scaled_size(1e12, 1e11), 1e23);
        assert_eq!(scaled_size(1e300, 1e11), 1e300);
        assert_eq!(scaled_size(1e-300, 1e-30), 1e-300);
        assert_eq!(scaled_size(0.0, 5.0), 0.0);
    }
}
