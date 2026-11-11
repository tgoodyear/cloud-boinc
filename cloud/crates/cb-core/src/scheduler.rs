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
//! Not done here: assigned jobs for teams and broadcast (`multi`)
//! assignments, homogeneous redundancy and homogeneous app version, locality
//! scheduling, and the client workload simulation behind upstream's deadline
//! check.

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

use crate::model::{
    anon_gavid, anon_platform_id, anon_uses_gpu, generalized_app_version_id, App, AppVersion, Host,
    HostAppVersion, Platform, User, Workunit,
};
use crate::plan_class::{HostUsage, PlanClassSpecs, PlanHost};
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
}

impl Catalog {
    /// Everything not deprecated.
    pub async fn load(db: &dyn Db) -> CoreResult<Catalog> {
        let mut out = db
            .batch(vec![
                Stmt::new("SELECT * FROM platform WHERE deprecated = 0 ORDER BY id"),
                Stmt::new("SELECT * FROM app WHERE deprecated = 0 ORDER BY id"),
                Stmt::new("SELECT * FROM app_version WHERE deprecated = 0 ORDER BY id"),
            ])
            .await?
            .into_iter();
        Ok(Catalog {
            platforms: out.next().unwrap_or_default().decode()?,
            apps: out.next().unwrap_or_default().decode()?,
            app_versions: out.next().unwrap_or_default().decode()?,
        })
    }

    fn app(&self, id: i64) -> Option<&App> {
        self.apps.iter().find(|a| a.id == id)
    }

    fn app_version(&self, id: i64) -> Option<&AppVersion> {
        self.app_versions.iter().find(|v| v.id == id)
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
}

/// Why a job was passed over, for the message when nothing is sent.
#[derive(Default)]
struct Skipped {
    quota: bool,
    in_progress_limit: bool,
    memory: bool,
    disk: bool,
    speed: bool,
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
    /// For an anonymous-platform version, how far the client's estimate of
    /// its speed is from ours (`CLIENT_APP_VERSION::rsc_fpops_scale`).
    fpops_scale: f64,
    hu: HostUsage,
    /// Counts against the GPU limit on jobs in progress.
    gpu: bool,
    /// Jobs of this version sent to the host today, including this reply.
    n_today: i32,
    /// The host's daily limit for this version.
    quota: i32,
    consecutive_valid: i32,
    /// Whether the host has a `host_app_version` row for it.
    has_row: bool,
    /// Sort key: projected speed with the selection noise applied.
    score: f64,
}

impl Version<'_> {
    fn anonymous(&self) -> bool {
        self.av.is_none()
    }

    /// What the reply needs to know about the version.
    fn sending(&self) -> Sending {
        Sending {
            av_id: self.av.map_or(0, |a| a.id),
            anon: self.anonymous(),
            version_num: self.version_num,
            plan_class: self.plan_class.to_string(),
            platform: self.platform.to_string(),
            fpops_scale: self.fpops_scale,
            hu: self.hu.clone(),
        }
    }
}

/// The app version a result is sent with.
struct Sending {
    /// The project's app version; 0 on the anonymous platform.
    av_id: i64,
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

/// What an app version would use on this host, with the platform it runs
/// as; or, when the host cannot run it, what to tell the volunteer.
fn usage_for<'a>(
    av: &AppVersion,
    platforms: &[&'a Platform],
    req: &SchedulerRequest,
    plan_classes: &PlanClassSpecs,
    plan_host: &PlanHost<'_>,
    hav: Option<&HostAppVersion>,
) -> Result<(HostUsage, &'a str), Option<&'static str>> {
    let platform = platforms
        .iter()
        .find(|p| p.id == av.platformid)
        .ok_or(None)?;
    if av.min_core_version > 0 && req.core_client_version < av.min_core_version {
        return Err(None);
    }
    if av.max_core_version > 0 && req.core_client_version > av.max_core_version {
        return Err(None);
    }
    let mut hu = plan_classes
        .evaluate(&av.plan_class, plan_host)
        .map_err(|r| r.user_message)?;
    estimate_flops(&mut hu, av, hav);
    if !(hu.projected_flops.is_finite() && hu.projected_flops > 0.0) {
        hu.projected_flops = 1e9;
    }
    Ok((hu, platform.name.as_str()))
}

/// What a version from the client's `app_info.xml` uses on this host
/// (`CLIENT_APP_VERSION::parse`, `HOST_USAGE` of `sched/sched_types.h`).
/// `projected_flops` is the client's `<flops>`, or the host's benchmark speed
/// when it gave none (`estimate_flops_anon_platform`).
fn anon_usage(cav: &ClientAppVersion, req: &SchedulerRequest, p_fpops: f64) -> HostUsage {
    // These come from the client's file; keep them finite.
    let avg_ncpus = if cav.avg_ncpus.is_finite() && cav.avg_ncpus > 0.0 {
        cav.avg_ncpus
    } else {
        1.0
    };
    let gpu_usage = clean(cav.gpu_usage);
    let mut peak_flops = avg_ncpus * p_fpops;
    if cav.proc_type != PROC_TYPE_CPU {
        if let Some(c) = req.coprocs.proc_type(cav.proc_type) {
            peak_flops += gpu_usage * clean(c.peak_flops);
        }
    }
    let projected_flops = if cav.flops.is_finite() && cav.flops > 0.0 {
        cav.flops
    } else {
        p_fpops
    };
    HostUsage {
        proc_type: cav.proc_type,
        gpu_usage,
        gpu_ram: 0.0,
        avg_ncpus,
        mem_usage: 0.0,
        // A host whose benchmark failed has no speed to fall back on.
        projected_flops: if projected_flops > 0.0 {
            projected_flops
        } else {
            1e9
        },
        peak_flops,
        cmdline: String::new(),
    }
}

/// The speed to use for a version from the client's `app_info.xml`, and the
/// factor by which the workunit's `rsc_fpops_est` and `rsc_fpops_bound` are
/// scaled in the reply so that the client's own estimate (workunit size over
/// the speed it stated) comes out as ours
/// (`estimate_flops_anon_platform` in `sched/sched_version.cpp`).
///
/// Given the speed the client stated, with enough elapsed-time history for
/// this host and resource type the estimate is the one that history implies,
/// capped at `ET_RATIO_LIMIT` times the stated speed.
fn anon_projected_flops(stated: f64, hav: Option<&HostAppVersion>) -> (f64, f64) {
    match hav {
        Some(h) if h.et_n > MIN_HOST_SAMPLES && h.et_avg > 0.0 => {
            let new_flops = (1.0 / h.et_avg).min(ET_RATIO_LIMIT * stated);
            (new_flops, stated / new_flops)
        }
        _ => (stated, 1.0),
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
        // `is_64b_platform` in `sched/sched_types.h`
        if need_64b && !v.platform.contains("64") {
            continue;
        }
        if v.n_today >= v.quota {
            if !pick.quota_hit.contains(&v.quota) {
                pick.quota_hit.push(v.quota);
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

    if req.core_client_version < config.min_core_client_version {
        refuse(
            &mut out,
            "Your BOINC client software is too old.  Please install the current version.",
            "notice",
            DELAY_BAD_CLIENT_VERSION,
            "client too old",
        );
        return Ok(out);
    }
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
        "SELECT * FROM user WHERE authenticator = ?",
        req.authenticator.as_str()
    )];
    reads.push(stmt!("SELECT * FROM host WHERE id = ?", req.hostid.max(0)));
    if let Some(id) = weak_id {
        reads.push(stmt!("SELECT * FROM user WHERE id = ?", id as i64));
    }
    let mut rows = ctx.db.batch(reads).await?.into_iter();
    let by_auth: Option<User> = rows.next().unwrap_or_default().first()?;
    let host_row: Option<Host> = rows.next().unwrap_or_default().first()?;
    let by_weak: Option<User> = match rows.next() {
        Some(r) => r.first()?,
        None => None,
    };
    let (user, weak_auth) = match (by_auth, by_weak) {
        (Some(u), _) => (Some(u), false),
        (None, Some(u)) => {
            let ok = check_weak_auth(
                &req.authenticator,
                u.id as u64,
                &u.authenticator,
                &u.passwd_hash,
            );
            (ok.then_some(u), true)
        }
        (None, None) => (None, false),
    };
    let Some(user) = user.filter(|u| u.blocked == 0) else {
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
        if !req.host_info.domain_name.is_empty() {
            lookups.push(stmt!(
                "SELECT * FROM host WHERE userid = ? AND domain_name = ? AND last_ip_addr = ? \
                 AND os_name = ? AND p_model = ? ORDER BY id DESC LIMIT 1",
                user.id,
                req.host_info.domain_name.as_str(),
                req.host_info.last_ip_addr.as_str(),
                req.host_info.os_name.as_str(),
                req.host_info.p_model.as_str()
            ));
        }
        if !lookups.is_empty() {
            for (i, r) in ctx.db.batch(lookups).await?.into_iter().enumerate() {
                if host.is_some() {
                    break;
                }
                host = r.first()?;
                if host.is_some() && i < n_cpid {
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
            let id = ctx
                .db
                .run(stmt!(
                    "INSERT INTO host (create_time, userid, venue, host_cpid) VALUES (?, ?, ?, ?) \
                     RETURNING id",
                    now,
                    user.id,
                    user.venue.as_str(),
                    request_cpid.as_str()
                ))
                .await?
                .scalar_i64()
                .ok_or_else(|| CoreError::Invalid("host insert returned no id".into()))?;
            Host {
                id,
                create_time: now,
                userid: user.id,
                venue: user.venue.clone(),
                host_cpid: request_cpid.clone(),
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
        "SELECT id, name, workunitid, app_version_id FROM result \
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

    let mut writes: Vec<Stmt> = Vec::new();
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
        let p_fpops = clean(req.host_info.p_fpops);
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
            let hu = anon_usage(cav, &req, p_fpops);
            let gavid = anon_gavid(app.id, cav.proc_type);
            let hav = havs.get(&gavid);
            let (projected_flops, fpops_scale) = anon_projected_flops(hu.projected_flops, hav);
            let gpu = pt != 0;
            versions.push(Version {
                av: None,
                appid: app.id,
                result_av_id: anon_platform_id(cav.proc_type),
                gavid,
                version_num: cav.version_num,
                plan_class: &cav.plan_class,
                platform: &cav.platform,
                fpops_scale,
                hu: HostUsage {
                    projected_flops,
                    ..hu
                },
                gpu,
                n_today: match hav {
                    Some(h) if !new_day => h.n_jobs_today,
                    _ => 0,
                },
                quota: quota_of(hav, gpu, pt),
                consecutive_valid: hav.map_or(0, |h| h.consecutive_valid),
                has_row: hav.is_some(),
                score: projected_flops,
            });
        }
    } else {
        for av in &catalog.app_versions {
            let Some(app) = catalog.app(av.appid) else {
                continue;
            };
            if catalog.superseded(av) || av.version_num < app.min_version {
                continue;
            }
            if !prefs.app_ids.is_empty() && !prefs.app_ids.contains(&app.id) {
                continue;
            }
            if (app.beta != 0 || av.beta != 0) && !prefs.allow_beta_work {
                continue;
            }
            let hav = havs.get(&av.id);
            let (hu, platform) =
                match usage_for(av, &host_platforms, &req, ctx.plan_classes, &plan_host, hav) {
                    Ok(x) => x,
                    Err(message) => {
                        if let Some(m) = message {
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
            versions.push(Version {
                av: Some(av),
                appid: av.appid,
                result_av_id: av.id,
                gavid: av.id,
                version_num: av.version_num,
                plan_class: &av.plan_class,
                platform,
                fpops_scale: 1.0,
                hu,
                gpu,
                n_today: match hav {
                    Some(h) if !new_day => h.n_jobs_today,
                    _ => 0,
                },
                quota: quota_of(hav, gpu, pt),
                consecutive_valid: hav.map_or(0, |h| h.consecutive_valid),
                has_row: hav.is_some(),
                score,
            });
        }
    }
    versions.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut too_recent = false;
    if work_requested && config.min_sendwork_interval > 0 && host.rpc_time > 0 {
        let since = now - host.rpc_time;
        if since < i64::from(config.min_sendwork_interval) {
            too_recent = true;
            out.reply.messages.push(UserMessage::new(
                format!("Not sending work - last request too recent: {since} sec"),
                "low",
            ));
        }
    }
    // Upstream's `work_needed` asks for a resource the client needs work for
    // and has an app for: on the anonymous platform that is a resource of one
    // of the client's versions, even one this project has no app for.
    let has_versions = if anonymous {
        (0..NPROC).any(|pt| client_has[pt] && (need_secs[pt] > 0.0 || need_inst[pt] > 0.0))
    } else {
        !versions.is_empty()
    };
    let send = work_requested
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
         p_gpu_fpops = ? WHERE id = ?",
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
        host.id
    ));

    // Global preferences: whichever copy is newer wins.
    let db_mod = line::parse_double(&user.global_prefs, "<mod_time>").unwrap_or(0.0);
    let req_mod = line::parse_double(&req.global_prefs_xml, "<mod_time>").unwrap_or(0.0);
    if !user.global_prefs.trim().is_empty()
        && (req.global_prefs_xml.trim().is_empty() || db_mod > req_mod)
    {
        out.reply.user.global_prefs = Some(user.global_prefs.clone());
    } else if !weak_auth
        && !req.global_prefs_xml.trim().is_empty()
        && req_mod > db_mod
        && req_mod <= now as f64
    {
        writes.push(stmt!(
            "UPDATE user SET global_prefs = ? WHERE id = ?",
            truncate(&req.global_prefs_xml, MAX_BLOB),
            user.id
        ));
    }

    // Batch 3: results the client lost, and candidates to send.
    let other_names: HashSet<&str> = req.other_results.iter().map(|o| o.name.as_str()).collect();
    let lost: Vec<&InProgressRow> =
        if config.resend_lost_results && req.have_other_results_list && key_ok {
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
    let mut app_ids: Vec<i64> = Vec::new();
    if send {
        for v in &versions {
            if !app_ids.contains(&v.appid) {
                app_ids.push(v.appid);
            }
        }
        app_ids.sort_unstable();
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
                !app_ids.contains(&a.id)
                    && (prefs.app_ids.is_empty() || prefs.app_ids.contains(&a.id))
                    && (a.beta == 0 || prefs.allow_beta_work)
            })
            .collect();
    }
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
    let want_assigned = send && config.enable_assignment && !app_ids.is_empty();
    let id_list = |ids: &mut dyn Iterator<Item = i64>| {
        ids.map(|id| id.to_string()).collect::<Vec<_>>().join(",")
    };
    let assigned_apps = id_list(&mut app_ids.iter().copied());
    // Versions a workunit may be pinned to; the client's own are none.
    let assigned_versions =
        id_list(&mut std::iter::once(0).chain(versions.iter().filter_map(|v| v.av.map(|a| a.id))));
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
                 AND w.appid IN ({assigned_apps}) \
                 AND w.app_version_id IN ({assigned_versions}) \
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
    for id in &app_ids {
        reads.push(stmt!(
            format!(
                "SELECT {CANDIDATE_COLS} FROM result r JOIN workunit w ON w.id = r.workunitid \
                 WHERE r.server_state = 2 AND r.appid = ? ORDER BY r.priority DESC, r.id LIMIT ?"
            ),
            *id,
            fetch
        ));
    }
    if !probe_apps.is_empty() {
        let mut s = Stmt::new(format!(
            "SELECT a.id AS workunitid FROM app a WHERE a.id IN ({}) AND EXISTS \
             (SELECT 1 FROM result r WHERE r.server_state = 2 AND r.appid = a.id)",
            in_list(probe_apps.len())
        ));
        for a in &probe_apps {
            s = s.bind(a.id);
        }
        reads.push(s);
    }
    let mut lost_rows: Vec<Candidate> = Vec::new();
    let mut assigned_rows: Vec<Candidate> = Vec::new();
    let mut apps_with_work: Vec<i64> = Vec::new();
    let mut queues: Vec<(i64, std::collections::VecDeque<Candidate>)> = Vec::new();
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
            shuffle_within_priority(&mut c, ctx.random);
            queues.push((*id, c.into()));
        }
        if !probe_apps.is_empty() {
            let found: Vec<WuIdRow> = rows.next().unwrap_or_default().decode()?;
            apps_with_work = found.into_iter().map(|r| r.workunitid).collect();
        }
    }

    let mut outgoing: Vec<Outgoing> = Vec::new();

    // Lost results (`resend_lost_work`).
    for c in lost_rows {
        let Some(ip) = in_progress.iter().find(|r| r.id == c.result_id) else {
            continue;
        };
        let usable: Option<Sending> = if anonymous {
            // Upstream picks a version afresh with `get_app_version`, from the
            // client's, whatever the lost result was sent with: one for a
            // resource it wants work for, with daily quota left.
            catalog.app(c.wu.appid).and_then(|app| {
                let needed = |pt: usize| need_secs[pt] > 0.0 || need_inst[pt] > 0.0;
                let need_64b = c.wu.rsc_memory_bound > 2.0 * GIGA;
                let pick = pick_anonymous(&versions, app, need_64b, &needed);
                pick.chosen.map(|i| versions[i].sending())
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
                .map(|(hu, platform)| Sending {
                    av_id: av.id,
                    anon: false,
                    version_num: av.version_num,
                    plan_class: av.plan_class.clone(),
                    platform: platform.to_string(),
                    fpops_scale: 1.0,
                    hu,
                })
            })
        };
        let wanted = c.wu.error_mask == 0
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
        if deadline <= now {
            // Past its deadline: allow half the original time again.
            deadline = (now + ((c.report_deadline - c.sent_time) / 2).max(60))
                .min(now + c.wu.delay_bound.max(60));
            writes.push(stmt!(
                "UPDATE result SET report_deadline = ?, mod_time = ? \
                 WHERE id = ? AND server_state = 4",
                deadline,
                now,
                c.result_id
            ));
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

    // Interleave the apps' candidates in proportion to app weight.
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

    // Choose (`send_work`, `wu_is_infeasible_fast`).
    let replica_rule = config.one_result_per_user_per_wu || config.one_result_per_host_per_wu;
    let replica_who = if config.one_result_per_user_per_wu {
        ("userid", user.id)
    } else {
        ("hostid", host.id)
    };
    let gpu_ids = if gpu_version_ids.is_empty() {
        "0".to_string()
    } else {
        gpu_version_ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    let cpu_limit = i64::from(config.max_wus_in_progress.max(0)) * i64::from(ncpus);
    let gpu_limit = i64::from(config.max_wus_in_progress_gpu.max(0)) * i64::from(ngpus.max(1));
    let mut n_gpu = in_progress
        .iter()
        .filter(|r| is_gpu_version(r.app_version_id))
        .count() as i64;
    let mut n_cpu = in_progress.len() as i64 - n_gpu;
    let total_limit = i64::from(prefs.max_jobs_in_progress);
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
    let n_candidates = merged.len() + assigned_rows.len();
    let mut n_new = 0;
    let mut n_assigned = 0;
    let mut excluded: HashSet<i64> = HashSet::new();
    // Assigned jobs first. Like upstream, they skip the feasibility checks
    // and limits but need a usable app version and a request for work; when
    // any is sent, the reply carries no other new work.
    for assigned in [true, false] {
        let list = if assigned {
            std::mem::take(&mut assigned_rows)
        } else {
            if n_assigned > 0 {
                break;
            }
            // Batch 4: workunits this user or host already has a result of.
            if replica_rule && !merged.is_empty() {
                let (column, who) = replica_who;
                let mut ids: Vec<i64> = merged.iter().map(|c| c.wu.id).collect();
                ids.sort_unstable();
                ids.dedup();
                let mut reads = Vec::new();
                for chunk in ids.chunks(MAX_IN_LIST) {
                    let mut s = Stmt::new(format!(
                        "SELECT DISTINCT workunitid FROM result WHERE workunitid IN ({}) AND {column} = ?",
                        in_list(chunk.len())
                    ));
                    for id in chunk {
                        s = s.bind(*id);
                    }
                    reads.push(s.bind(who));
                }
                for rows in ctx.db.batch(reads).await? {
                    for r in rows.decode::<WuIdRow>()? {
                        excluded.insert(r.workunitid);
                    }
                }
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
            if sent_wus.contains(&c.wu.id) || excluded.contains(&c.wu.id) {
                continue;
            }
            let Some(app) = catalog.app(c.wu.appid) else {
                continue;
            };
            let admitted = |v: &Version<'_>| {
                v.appid == c.wu.appid
                    && match v.av {
                        Some(av) => {
                            (c.wu.app_version_id == 0 || c.wu.app_version_id == av.id)
                                && ctx.plan_classes.admits_workunit(
                                    &av.plan_class,
                                    c.wu.id,
                                    c.wu.batch,
                                )
                        }
                        // A job committed to one of the project's versions
                        // is not for the client's own.
                        None => c.wu.app_version_id == 0,
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
                    if !assigned && (need_secs[pt] > 0.0 || need_inst[pt] > 0.0) && at_limit(pt) {
                        skipped.in_progress_limit = true;
                    }
                }
                let needed = |pt: usize| {
                    (need_secs[pt] > 0.0 || need_inst[pt] > 0.0) && (assigned || !at_limit(pt))
                };
                let need_64b = c.wu.rsc_memory_bound > 2.0 * GIGA;
                let pick = if c.wu.app_version_id == 0 {
                    pick_anonymous(&versions, app, need_64b, &needed)
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
                for (i, v) in versions.iter().enumerate() {
                    if !admitted(v) {
                        continue;
                    }
                    let pt = proc_index(v.hu.proc_type);
                    if need_secs[pt] <= 0.0 && need_inst[pt] <= 0.0 {
                        continue;
                    }
                    if v.n_today >= v.quota {
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
                    if !config.ignore_delay_bound && est_delay[pt] + est > c.wu.delay_bound as f64 {
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
            let v = &mut versions[i];
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
            v.n_today += 1;
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

            if !v.has_row {
                v.has_row = true;
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
                    quota: v.quota,
                }),
                // Upstream sends at most one instance of an assigned job to
                // a host, whatever the replica rule says.
                one_per: if assigned {
                    Some(("hostid", host.id))
                } else {
                    replica_rule.then_some(replica_who)
                },
            };
            let claim_at = writes.len();
            writes.push(claim.stmt());

            // What follows the claim happens only if the claim did.
            const CLAIMED: &str = "EXISTS (SELECT 1 FROM result WHERE id = ? AND hostid = ? \
                 AND server_state = 4 AND sent_time = ?)";
            writes.push(stmt!(
                format!(
                    "UPDATE host_app_version SET n_jobs_today = n_jobs_today + 1 \
                     WHERE host_id = ? AND app_version_id = ? AND {CLAIMED}"
                ),
                host.id,
                v.gavid,
                c.result_id,
                host.id,
                now
            ));
            // A job sent unreplicated to a host that has not earned it gets
            // the app's replication after all. Not an assigned job: its
            // extra results could go nowhere but its own host or user.
            let trusted = v.consecutive_valid >= CONS_VALID_UNREPLICATED
                && unit(ctx.random) > 1.0 / f64::from(v.consecutive_valid);
            if !assigned && app.target_nresults > 1 && c.wu.target_nresults == 1 && !trusted {
                writes.push(stmt!(
                    format!(
                        "UPDATE workunit SET target_nresults = ?, min_quorum = ?, \
                         transition_time = ?, rev = rev + 1 WHERE id = ? AND {CLAIMED}"
                    ),
                    app.target_nresults,
                    app.target_nresults,
                    now,
                    c.wu.id,
                    c.result_id,
                    host.id,
                    now
                ));
            } else {
                writes.push(stmt!(
                    format!(
                        "UPDATE workunit SET transition_time = min(transition_time, ?), \
                         rev = rev + 1 WHERE id = ? AND {CLAIMED}"
                    ),
                    deadline + i64::from(config.report_grace_period),
                    c.wu.id,
                    c.result_id,
                    host.id,
                    now
                ));
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

    // Batch 5: everything this request changes.
    let written = ctx.db.batch(writes).await?;

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
                    rsc_fpops_est: o.wu.rsc_fpops_est * version.fpops_scale,
                    rsc_fpops_bound: o.wu.rsc_fpops_bound * version.fpops_scale,
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

    if work_requested && out.n_sent == 0 && key_ok && !too_recent {
        out.reply
            .messages
            .push(UserMessage::new("No tasks sent", "low"));
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
        } else if n_candidates == 0 && apps_with_work.is_empty() {
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
        out.reply.request_delay = out
            .reply
            .request_delay
            .max(1.01 * f64::from(config.min_sendwork_interval));
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
            n_today: 0,
            quota: 10,
            consecutive_valid: 0,
            has_row: false,
            score: flops,
        }
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
        let pick = pick_anonymous(&versions, &a, false, &all_needed);
        assert_eq!(pick.chosen, Some(2));
        assert!(pick.found);
    }

    #[test]
    fn versions_are_skipped_for_quota_age_resource_and_address_space() {
        let a = app(1, 105);
        let mut versions = vec![
            anon(1, 110, PROC_TYPE_CPU, 5e9, "windows_intelx86"),
            anon(1, 110, PROC_TYPE_CPU, 4e9, "windows_x86_64"),
            anon(1, 104, PROC_TYPE_CPU, 9e9, "windows_x86_64"),
            anon(1, 110, PROC_TYPE_NVIDIA_GPU, 8e9, "windows_x86_64"),
        ];
        // The GPU is not wanted, the 104 is below the app's minimum, and the
        // 32-bit version cannot take a job that needs 64 bits.
        let cpu_only = |pt: usize| pt == 0;
        let pick = pick_anonymous(&versions, &a, true, &cpu_only);
        assert_eq!(pick.chosen, Some(1));
        let pick = pick_anonymous(&versions, &a, false, &cpu_only);
        assert_eq!(pick.chosen, Some(0));

        // A version whose quota is used up is not "found" either.
        for v in &mut versions {
            v.n_today = v.quota;
        }
        let pick = pick_anonymous(&versions, &a, false, &cpu_only);
        assert_eq!(pick.chosen, None);
        assert!(!pick.found);
        assert_eq!(pick.quota_hit, vec![10]);

        // Found, but not for a resource the client wants work for.
        let versions = vec![anon(1, 110, PROC_TYPE_NVIDIA_GPU, 8e9, "p")];
        let pick = pick_anonymous(&versions, &a, false, &cpu_only);
        assert_eq!(pick.chosen, None);
        assert!(pick.found);
    }
}
