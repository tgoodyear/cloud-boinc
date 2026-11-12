//! Job submission, cancellation and the assimilation hand-off: what
//! upstream does with `create_work`, `cancel_jobs` and an assimilator
//! daemon.

use std::collections::{HashMap, HashSet};

use cb_crypto::Signer;
use cb_db::{stmt, Db, Stmt};
use cb_proto::consts::*;
use serde::{Deserialize, Serialize};

use crate::model::{App, Workunit};
use crate::pipeline::{initial_results, insert_result_stmt, safety_net_time, WuGuard};
use crate::template::{
    constant_file_names, output_files, process_input_template, InputFile, StagedFile,
};
use crate::{Config, CoreError, CoreResult, FileKind, Files};

/// D1 allows 100 bound parameters per statement.
const MAX_IN_LIST: usize = 90;

/// One job to submit. Unset numbers come from the input template, then from
/// the upstream `create_work` defaults.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct JobSpec {
    pub name: String,
    pub input_files: Vec<InputFile>,
    pub command_line: Option<String>,
    pub additional_xml: Option<String>,
    pub batch: Option<i64>,
    pub priority: Option<i32>,
    pub rsc_fpops_est: Option<f64>,
    pub rsc_fpops_bound: Option<f64>,
    pub rsc_memory_bound: Option<f64>,
    pub rsc_disk_bound: Option<f64>,
    pub delay_bound: Option<i64>,
    pub min_quorum: Option<i32>,
    pub target_nresults: Option<i32>,
    pub max_error_results: Option<i32>,
    pub max_total_results: Option<i32>,
    pub max_success_results: Option<i32>,
    /// Send the job only to this host, or only to this user's hosts
    /// (`create_work --target_host` and `--target_user`).
    pub assign: Option<Assign>,
}

/// Who an assigned (targeted) job goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct Assign {
    pub target: AssignTarget,
    /// The host or user ID.
    pub id: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AssignTarget {
    Host,
    User,
}

impl AssignTarget {
    /// `assignment.target_type`.
    pub fn target_type(self) -> i32 {
        match self {
            AssignTarget::Host => ASSIGN_HOST,
            AssignTarget::User => ASSIGN_USER,
        }
    }
}

/// A submission: jobs of one app sharing their templates.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Submission {
    pub app: String,
    pub input_template: String,
    pub result_template: String,
    pub jobs: Vec<JobSpec>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SubmittedJob {
    pub name: String,
    /// 0 when the job was not created.
    pub workunit_id: i64,
    /// Why the job was not created.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn in_list(n: usize) -> String {
    vec!["?"; n].join(",")
}

fn valid_wu_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        && !name.starts_with('.')
}

fn build_workunit(
    sub: &Submission,
    job: &JobSpec,
    app: &App,
    staged: &HashMap<String, StagedFile>,
    config: &Config,
    now: i64,
) -> CoreResult<Workunit> {
    if !valid_wu_name(&job.name) {
        return Err(CoreError::Invalid(
            "job names use letters, digits, '_', '-' and '.'".into(),
        ));
    }
    // create_work.cpp defaults
    let mut wu = Workunit {
        create_time: now,
        appid: app.id,
        name: job.name.clone(),
        rsc_fpops_est: 3600e9,
        rsc_fpops_bound: 86400e9,
        rsc_memory_bound: 5e8,
        rsc_disk_bound: 1e9,
        delay_bound: 7 * 86400,
        min_quorum: 2,
        target_nresults: 2,
        max_error_results: 3,
        max_total_results: 10,
        max_success_results: 6,
        result_template: sub.result_template.clone(),
        mod_time: now,
        ..Default::default()
    };
    process_input_template(
        &mut wu,
        &sub.input_template,
        &job.input_files,
        staged,
        &config.download_urls(),
        job.command_line.as_deref(),
        job.additional_xml.as_deref(),
    )?;
    macro_rules! over {
        ($($f:ident),*) => {$( if let Some(v) = job.$f { wu.$f = v; } )*};
    }
    over!(
        batch,
        priority,
        rsc_fpops_est,
        rsc_fpops_bound,
        rsc_memory_bound,
        rsc_disk_bound,
        delay_bound,
        min_quorum,
        target_nresults,
        max_error_results,
        max_total_results,
        max_success_results
    );
    // The checks of create_work() in tools/backend_lib.cpp.
    let bad = |what: &str| Err(CoreError::Invalid(format!("{what} must be positive")));
    if wu.rsc_fpops_est <= 0.0 {
        return bad("rsc_fpops_est");
    }
    if wu.rsc_fpops_bound <= 0.0 {
        return bad("rsc_fpops_bound");
    }
    if wu.rsc_disk_bound <= 0.0 {
        return bad("rsc_disk_bound");
    }
    if wu.delay_bound <= 0 {
        return bad("delay_bound");
    }
    if wu.min_quorum <= 0 {
        return bad("min_quorum");
    }
    if wu.target_nresults <= 0 {
        return bad("target_nresults");
    }
    if wu.max_error_results <= 0 {
        return bad("max_error_results");
    }
    if wu.max_total_results <= 0 {
        return bad("max_total_results");
    }
    if wu.max_success_results <= 0 {
        return bad("max_success_results");
    }
    if wu.max_success_results > wu.max_total_results {
        return Err(CoreError::Invalid(
            "max_success_results is above max_total_results".into(),
        ));
    }
    if wu.max_error_results > wu.max_total_results {
        return Err(CoreError::Invalid(
            "max_error_results is above max_total_results".into(),
        ));
    }
    if wu.target_nresults > wu.max_success_results {
        return Err(CoreError::Invalid(
            "target_nresults is above max_success_results".into(),
        ));
    }
    if wu.min_quorum > wu.target_nresults {
        return Err(CoreError::Invalid(
            "min_quorum is above target_nresults".into(),
        ));
    }
    if let Some(a) = &job.assign {
        if !config.enable_assignment {
            return Err(CoreError::Invalid(
                "assigned jobs need enable_assignment in the project config".into(),
            ));
        }
        if a.id <= 0 {
            return Err(CoreError::Invalid(
                "an assigned job needs a nonzero host or user ID".into(),
            ));
        }
        // A host gets at most one instance of an assigned job, so a
        // second instance could never be sent.
        if a.target == AssignTarget::Host && wu.target_nresults > 1 {
            return Err(CoreError::Invalid(
                "a job assigned to a host needs target_nresults 1".into(),
            ));
        }
        // As create_work does. Here the flag also means that the
        // workunit's results are created reserved for the assignment
        // (`initial_results`).
        wu.transitioner_flags = TRANSITION_NO_NEW_RESULTS;
    }
    wu.transition_time = safety_net_time(&wu, now);
    Ok(wu)
}

/// The assignment row of a job, for a workunit inserted earlier in the same
/// batch. Upstream does not check that the host or user exists.
fn insert_assignment_stmt(a: &Assign, wu_name: &str, now: i64) -> Stmt {
    stmt!(
        "INSERT INTO assignment (create_time, target_id, target_type, multi, workunitid) \
         SELECT ?, ?, ?, 0, id FROM workunit WHERE name = ?",
        now,
        a.id,
        a.target.target_type(),
        wu_name
    )
}

fn insert_workunit_stmt(wu: &Workunit) -> Stmt {
    stmt!(
        "INSERT INTO workunit (create_time, appid, name, xml_doc, batch, rsc_fpops_est, rsc_fpops_bound, \
         rsc_memory_bound, rsc_disk_bound, transition_time, delay_bound, min_quorum, target_nresults, \
         max_error_results, max_total_results, max_success_results, result_template, priority, mod_time, \
         rsc_bandwidth_bound, transitioner_flags) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
        wu.create_time,
        wu.appid,
        wu.name.as_str(),
        wu.xml_doc.as_str(),
        wu.batch,
        wu.rsc_fpops_est,
        wu.rsc_fpops_bound,
        wu.rsc_memory_bound,
        wu.rsc_disk_bound,
        wu.transition_time,
        wu.delay_bound,
        wu.min_quorum,
        wu.target_nresults,
        wu.max_error_results,
        wu.max_total_results,
        wu.max_success_results,
        wu.result_template.as_str(),
        wu.priority,
        wu.mod_time,
        wu.rsc_bandwidth_bound,
        wu.transitioner_flags
    )
}

#[derive(Deserialize)]
struct JobFileRow {
    name: String,
    md5: String,
    nbytes: f64,
    no_delete: i64,
}

#[derive(Deserialize)]
struct NameRow {
    name: String,
}

/// Creates the workunits of a submission, each with its first results, so
/// they can be dispatched at once. A job that cannot be created is reported
/// in its entry and does not stop the others.
pub async fn create_work(
    db: &dyn Db,
    config: &Config,
    signer: Option<&Signer>,
    now: i64,
    random: &dyn Fn() -> u32,
    sub: &Submission,
) -> CoreResult<Vec<SubmittedJob>> {
    let app = cb_db::query_one::<App>(
        db,
        stmt!("SELECT * FROM app WHERE name = ?", sub.app.as_str()),
    )
    .await?
    .ok_or_else(|| CoreError::NotFound(format!("app {}", sub.app)))?;

    // Staged input files and names already taken, in one round trip.
    let mut file_names: Vec<String> = constant_file_names(&sub.input_template);
    for j in &sub.jobs {
        file_names.extend(
            j.input_files
                .iter()
                .filter(|f| f.url.is_none())
                .map(|f| f.name.clone()),
        );
    }
    file_names.sort();
    file_names.dedup();
    let job_names: Vec<&str> = sub.jobs.iter().map(|j| j.name.as_str()).collect();
    let mut reads = Vec::new();
    for chunk in file_names.chunks(MAX_IN_LIST) {
        let mut s = Stmt::new(format!(
            "SELECT name, md5, nbytes, no_delete FROM job_file WHERE name IN ({})",
            in_list(chunk.len())
        ));
        for n in chunk {
            s = s.bind(n);
        }
        reads.push(s);
    }
    let n_file_reads = reads.len();
    for chunk in job_names.chunks(MAX_IN_LIST) {
        let mut s = Stmt::new(format!(
            "SELECT name FROM workunit WHERE name IN ({})",
            in_list(chunk.len())
        ));
        for n in chunk {
            s = s.bind(*n);
        }
        reads.push(s);
    }
    let mut staged = HashMap::new();
    let mut taken = HashSet::new();
    if !reads.is_empty() {
        for (i, rows) in db.batch(reads).await?.into_iter().enumerate() {
            if i < n_file_reads {
                for f in rows.decode::<JobFileRow>()? {
                    staged.insert(
                        f.name,
                        StagedFile {
                            md5: f.md5,
                            nbytes: f.nbytes,
                            no_delete: f.no_delete != 0,
                        },
                    );
                }
            } else {
                taken.extend(rows.decode::<NameRow>()?.into_iter().map(|r| r.name));
            }
        }
    }

    let mut out: Vec<SubmittedJob> = Vec::with_capacity(sub.jobs.len());
    let mut writes = Vec::new();
    // Index into `writes` of each created job's workunit insert.
    let mut insert_at: Vec<Option<usize>> = Vec::with_capacity(sub.jobs.len());
    for job in &sub.jobs {
        let built = if !taken.insert(job.name.clone()) {
            Err(CoreError::Conflict("a job with this name exists".into()))
        } else {
            build_workunit(sub, job, &app, &staged, config, now).and_then(|wu| {
                let results = initial_results(&wu, config, signer, now, random)?;
                Ok((wu, results))
            })
        };
        match built {
            Ok((wu, results)) => {
                insert_at.push(Some(writes.len()));
                writes.push(insert_workunit_stmt(&wu));
                for r in &results {
                    writes.push(insert_result_stmt(r, WuGuard::NewByName(&wu.name)));
                }
                if let Some(a) = &job.assign {
                    writes.push(insert_assignment_stmt(a, &wu.name, now));
                }
                out.push(SubmittedJob {
                    name: job.name.clone(),
                    workunit_id: 0,
                    error: None,
                });
            }
            Err(e) => {
                insert_at.push(None);
                out.push(SubmittedJob {
                    name: job.name.clone(),
                    workunit_id: 0,
                    error: Some(e.to_string()),
                });
            }
        }
    }
    if writes.is_empty() {
        return Ok(out);
    }
    let written = db.batch(writes).await?;
    for (job, at) in out.iter_mut().zip(insert_at) {
        if let Some(at) = at {
            job.workunit_id = written.get(at).and_then(|r| r.scalar_i64()).unwrap_or(0);
        }
    }
    Ok(out)
}

/// Records a file stored under `download/` so that jobs can use it.
pub async fn register_job_file(
    db: &dyn Db,
    name: &str,
    md5: &str,
    nbytes: u64,
    no_delete: bool,
    now: i64,
) -> CoreResult<()> {
    db.run(stmt!(
        "INSERT INTO job_file (name, create_time, nbytes, md5, no_delete) VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT (name) DO UPDATE SET nbytes = excluded.nbytes, md5 = excluded.md5, no_delete = excluded.no_delete",
        name,
        now,
        nbytes as i64,
        md5,
        no_delete
    ))
    .await?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OutputFileInfo {
    pub name: String,
    pub nbytes: u64,
    pub md5: String,
}

/// A workunit that is ready to assimilate.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReadyWorkunit {
    pub id: i64,
    pub name: String,
    pub batch: i64,
    /// Non-zero when the workunit failed; there is then no canonical result.
    pub error_mask: i32,
    pub canonical_result: Option<CanonicalResult>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CanonicalResult {
    pub id: i64,
    pub name: String,
    pub host_id: i64,
    pub user_id: i64,
    pub elapsed_time: f64,
    pub granted_credit: f64,
    /// Stored output files; an optional file that was not uploaded is absent.
    pub files: Vec<OutputFileInfo>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ReadyRow {
    id: i64,
    name: String,
    batch: i64,
    error_mask: i32,
    canonical_resultid: i64,
    r_name: Option<String>,
    r_xml_doc_in: Option<String>,
    r_hostid: Option<i64>,
    r_userid: Option<i64>,
    r_elapsed_time: Option<f64>,
    r_granted_credit: Option<f64>,
}

/// Workunits of `app` that are ready to assimilate, by ID, after `after_id`.
pub async fn ready_to_assimilate(
    db: &dyn Db,
    files: &dyn Files,
    app: &str,
    after_id: i64,
    limit: i64,
) -> CoreResult<Vec<ReadyWorkunit>> {
    let rows = cb_db::query::<ReadyRow>(
        db,
        stmt!(
            "SELECT w.id, w.name, w.batch, w.error_mask, w.canonical_resultid, r.name AS r_name, \
             r.xml_doc_in AS r_xml_doc_in, r.hostid AS r_hostid, r.userid AS r_userid, \
             r.elapsed_time AS r_elapsed_time, r.granted_credit AS r_granted_credit \
             FROM workunit w LEFT JOIN result r ON r.id = w.canonical_resultid AND w.canonical_resultid > 0 \
             WHERE w.assimilate_state = 1 AND w.appid = (SELECT id FROM app WHERE name = ?) AND w.id > ? \
             ORDER BY w.id LIMIT ?",
            app,
            after_id,
            limit.clamp(1, 1000)
        ),
    )
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let canonical_result = match (row.canonical_resultid > 0, row.r_name) {
            (true, Some(name)) => {
                let mut fs = Vec::new();
                for f in output_files(row.r_xml_doc_in.as_deref().unwrap_or("")) {
                    if let Some(s) = files.stat(FileKind::Upload, &f.name).await? {
                        fs.push(OutputFileInfo {
                            name: f.name,
                            nbytes: s.nbytes,
                            md5: s.md5,
                        });
                    }
                }
                Some(CanonicalResult {
                    id: row.canonical_resultid,
                    name,
                    host_id: row.r_hostid.unwrap_or(0),
                    user_id: row.r_userid.unwrap_or(0),
                    elapsed_time: row.r_elapsed_time.unwrap_or(0.0),
                    granted_credit: row.r_granted_credit.unwrap_or(0.0),
                    files: fs,
                })
            }
            _ => None,
        };
        out.push(ReadyWorkunit {
            id: row.id,
            name: row.name,
            batch: row.batch,
            error_mask: row.error_mask,
            canonical_result,
        });
    }
    Ok(out)
}

fn ids_of(rows: Vec<cb_db::Rows>) -> Vec<i64> {
    rows.into_iter()
        .flat_map(|r| r.rows)
        .filter_map(|r| r.get("id").and_then(|v| v.as_i64()))
        .collect()
}

/// Marks workunits assimilated. Returns the IDs that changed; the caller
/// queues a transition for each, which schedules file deletion.
pub async fn acknowledge_assimilation(db: &dyn Db, ids: &[i64], now: i64) -> CoreResult<Vec<i64>> {
    let mut stmts = Vec::new();
    for chunk in ids.chunks(MAX_IN_LIST) {
        let mut s = Stmt::new(format!(
            "UPDATE workunit SET assimilate_state = ?, transition_time = ?, mod_time = ?, rev = rev + 1 \
             WHERE assimilate_state = ? AND id IN ({}) RETURNING id",
            in_list(chunk.len())
        ))
        .bind(ASSIMILATE_DONE)
        .bind(now)
        .bind(now)
        .bind(ASSIMILATE_READY);
        for id in chunk {
            s = s.bind(*id);
        }
        stmts.push(s);
    }
    if stmts.is_empty() {
        return Ok(Vec::new());
    }
    Ok(ids_of(db.batch(stmts).await?))
}

/// Which workunits to cancel.
#[derive(Debug, Clone)]
pub enum CancelTarget {
    Ids(Vec<i64>),
    Batch(i64),
}

/// Cancels workunits (`cancel_jobs`): unsent results, and results reserved
/// for an assigned job, are withdrawn and the
/// workunit is flagged, so nothing more is sent; results in progress are
/// told to abort at their host's next request. Returns the IDs to transition.
pub async fn cancel_workunits(
    db: &dyn Db,
    target: &CancelTarget,
    now: i64,
) -> CoreResult<Vec<i64>> {
    let mut stmts = Vec::new();
    let mut push = |wu_where: &str, binds: Vec<i64>| {
        let mut r = Stmt::new(format!(
            "UPDATE result SET server_state = ?, outcome = ?, mod_time = ? WHERE server_state IN (?, ?) \
             AND workunitid IN (SELECT id FROM workunit WHERE {wu_where})"
        ))
        .bind(RESULT_SERVER_STATE_OVER)
        .bind(RESULT_OUTCOME_DIDNT_NEED)
        .bind(now)
        .bind(RESULT_SERVER_STATE_INACTIVE)
        .bind(RESULT_SERVER_STATE_UNSENT);
        let mut w = Stmt::new(format!(
            "UPDATE workunit SET error_mask = error_mask | ?, transition_time = ?, mod_time = ?, rev = rev + 1 \
             WHERE {wu_where} RETURNING id"
        ))
        .bind(WU_ERROR_CANCELLED)
        .bind(now)
        .bind(now);
        for b in &binds {
            r = r.bind(*b);
            w = w.bind(*b);
        }
        stmts.push(r);
        stmts.push(w);
    };
    match target {
        CancelTarget::Ids(ids) => {
            for chunk in ids.chunks(MAX_IN_LIST) {
                push(&format!("id IN ({})", in_list(chunk.len())), chunk.to_vec());
            }
        }
        CancelTarget::Batch(b) => push("batch = ?", vec![*b]),
    }
    if stmts.is_empty() {
        return Ok(Vec::new());
    }
    Ok(ids_of(db.batch(stmts).await?))
}
