//! The job pipeline: one `transition` of a workunit does what upstream
//! splits between the transitioner (`sched/transitioner.cpp`), the validator
//! (`sched/validator.cpp`, `sched/validate_util2.cpp`) and the file deleter.
//!
//! A transition reads the workunit and its results, decides in memory, and
//! writes everything back in one batch. Every writer of a workunit or of one
//! of its results increments `workunit.rev`. The batch's first statement
//! updates the workunit only at the revision that was read and stamps it
//! with a random commit token; the other statements apply only while that
//! token is there. A transition whose read is stale writes nothing and
//! starts over.
//!
//! Assigned (targeted) jobs differ from upstream. Upstream's transitioner
//! creates no results for a workunit flagged `TRANSITION_NO_NEW_RESULTS`,
//! and the scheduler creates one when it sends the job. The scheduler here
//! does not hold the upload key, so the results of such a workunit are
//! created here as usual but reserved (`RESULT_SERVER_STATE_INACTIVE`),
//! where normal dispatch does not see them; the scheduler claims them for
//! the assigned host or user.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use cb_crypto::Signer;
use cb_db::{stmt, Db, Stmt, Value};
use cb_proto::consts::*;
use cb_proto::web_rpc::err::EXIT_ABORTED_VIA_GUI;
use serde::Deserialize;
use serde_json::json;

use crate::avg::{low_average, update_average, AVG_CPU_FPOPS, COBBLESTONE_SCALE, CREDIT_HALF_LIFE};
use crate::model::{
    generalized_app_version_id, App, HostAppVersion, JobResult, Workunit, RESULT_COLS_LIGHT,
};
use crate::template::{file_infos, output_files, process_result_template};
use crate::{Config, CoreError, CoreResult, FileKind, Files, Webhook, NEVER};

/// Everything a transition needs from its surroundings.
pub struct Ctx<'a> {
    pub db: &'a dyn Db,
    pub files: &'a dyn Files,
    pub webhook: &'a dyn Webhook,
    pub config: &'a Config,
    /// Signs upload certificates. `None` when the project runs with
    /// `dont_generate_upload_certificates`.
    pub signer: Option<&'a Signer>,
    /// Unix time of this run.
    pub now: i64,
    pub random: &'a dyn Fn() -> u32,
}

/// What a transition did, for logs and tests.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TransitionReport {
    pub workunit_id: i64,
    /// False when the workunit does not exist.
    pub found: bool,
    pub results_created: usize,
    pub results_timed_out: usize,
    pub results_valid: usize,
    pub results_invalid: usize,
    pub canonical_resultid: i64,
    pub error_mask: i32,
    pub assimilate_ready: bool,
    pub files_deleted: usize,
    pub transition_time: i64,
    /// Read-decide-write rounds; more than one means another writer got in
    /// between.
    pub attempts: u32,
}

/// How a workunit is named in the guard of a result insert.
pub(crate) enum WuGuard<'a> {
    /// An existing workunit whose update, earlier in the same batch, wrote
    /// this commit token.
    Token { id: i64, token: i64 },
    /// A workunit inserted earlier in the same batch.
    NewByName(&'a str),
}

struct Res {
    row: JobResult,
    /// The row as read; `None` for a result created in this transition.
    orig: Option<JobResult>,
}

enum HostEvent {
    Timeout {
        host_id: i64,
        gavid: i64,
        delay_bound: f64,
    },
    Valid {
        host_id: i64,
        user_id: i64,
        gavid: i64,
        turnaround: f64,
        /// Elapsed time per estimated FLOP, when it is usable.
        et_sample: Option<f64>,
        sent_time: f64,
        credit: f64,
        count_consecutive: bool,
    },
    Invalid {
        host_id: i64,
        gavid: i64,
    },
}

struct State {
    wu: Workunit,
    results: Vec<Res>,
    events: Vec<HostEvent>,
    delete_uploads: Vec<String>,
    delete_downloads: Vec<String>,
    report: TransitionReport,
}

/// Validator outcome for one result's output.
#[derive(Debug, Clone, PartialEq)]
enum Fp {
    /// Results with equal fingerprints agree.
    Ok(String),
    /// The output is unusable.
    Invalid,
    /// Could not tell right now.
    Transient,
}

fn result_suffix(name: &str) -> i32 {
    name.rsplit_once('_')
        .and_then(|(_, s)| s.parse().ok())
        .unwrap_or(0)
}

/// Builds the row of a new result of `wu` (`create_result`).
pub(crate) fn new_result(
    wu: &Workunit,
    suffix: i32,
    priority_increase: i32,
    config: &Config,
    signer: Option<&Signer>,
    now: i64,
    random: &dyn Fn() -> u32,
) -> CoreResult<JobResult> {
    let name = format!("{}_{}", wu.name, suffix);
    // The random part keeps a host from guessing its wingman's file names.
    let base = format!("{}_r{}_", name, random());
    let signer = if config.dont_generate_upload_certificates {
        None
    } else {
        signer
    };
    if signer.is_none() && !config.dont_generate_upload_certificates {
        return Err(CoreError::Crypto(
            "the upload private key is not configured".into(),
        ));
    }
    let xml_doc_in =
        process_result_template(&wu.result_template, signer, &base, &config.upload_url)?;
    Ok(JobResult {
        create_time: now,
        workunitid: wu.id,
        server_state: if wu.transitioner_flags == TRANSITION_NO_NEW_RESULTS {
            RESULT_SERVER_STATE_INACTIVE
        } else {
            RESULT_SERVER_STATE_UNSENT
        },
        outcome: RESULT_OUTCOME_INIT,
        name,
        xml_doc_in,
        batch: wu.batch,
        random: i64::from(random() & 0x7fff_ffff),
        appid: wu.appid,
        priority: wu.priority + priority_increase,
        mod_time: now,
        ..Default::default()
    })
}

pub(crate) fn insert_result_stmt(r: &JobResult, guard: WuGuard) -> Stmt {
    let s = Stmt::new(match guard {
        WuGuard::Token { .. } => {
            "INSERT INTO result (create_time, workunitid, server_state, outcome, name, xml_doc_in, batch, \
             random, appid, priority, validate_state, file_delete_state, mod_time) \
             SELECT ?, w.id, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ? FROM workunit w WHERE w.id = ? AND w.commit_token = ?"
        }
        WuGuard::NewByName(_) => {
            "INSERT INTO result (create_time, workunitid, server_state, outcome, name, xml_doc_in, batch, \
             random, appid, priority, validate_state, file_delete_state, mod_time) \
             SELECT ?, w.id, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ? FROM workunit w WHERE w.name = ?"
        }
    })
    .bind(r.create_time)
    .bind(r.server_state)
    .bind(r.outcome)
    .bind(r.name.as_str())
    .bind(r.xml_doc_in.as_str())
    .bind(r.batch)
    .bind(r.random)
    .bind(r.appid)
    .bind(r.priority)
    .bind(r.validate_state)
    .bind(r.file_delete_state)
    .bind(r.mod_time);
    match guard {
        WuGuard::Token { id, token } => s.bind(id).bind(token),
        WuGuard::NewByName(name) => s.bind(name),
    }
}

/// The initial results of a workunit that is about to be inserted, so that
/// creating work does not need a transition of its own. An assigned job's
/// results are reserved for its assignment.
pub(crate) fn initial_results(
    wu: &Workunit,
    config: &Config,
    signer: Option<&Signer>,
    now: i64,
    random: &dyn Fn() -> u32,
) -> CoreResult<Vec<JobResult>> {
    let n = wu.target_nresults.min(wu.max_total_results).max(0);
    (0..n)
        .map(|i| new_result(wu, i, 0, config, signer, now, random))
        .collect()
}

/// The transition time a workunit gets when nothing is due sooner: the
/// transitioner's safety net.
pub(crate) fn safety_net_time(wu: &Workunit, now: i64) -> i64 {
    const TEN_DAYS: i64 = 10 * 86400;
    let long_delay = (1.5 * wu.delay_bound as f64) as i64;
    now + long_delay.max(TEN_DAYS)
}

/// `handle_wu` of the transitioner, on the in-memory state.
fn transitioner_pass(st: &mut State, ctx: &Ctx) -> CoreResult<()> {
    let now = ctx.now;
    let config = ctx.config;
    let wu_appid = st.wu.appid;

    let (mut ntotal, mut nerrors, mut nsuccess, mut nunsent, mut ncouldnt_send) = (0, 0, 0, 0, 0);
    let (mut nover, mut nno_reply, mut ninprogress) = (0, 0, 0);
    let mut max_result_suffix = -1;
    let mut have_new_result_to_validate = false;

    let canonical_index = if st.wu.canonical_resultid != 0 {
        st.results
            .iter()
            .position(|r| r.row.id == st.wu.canonical_resultid)
    } else {
        None
    };
    let canonical_result_files_deleted =
        canonical_index.is_some_and(|i| st.results[i].row.file_delete_state != FILE_DELETE_INIT);

    for res in st.results.iter_mut() {
        let r = &mut res.row;
        ntotal += 1;
        max_result_suffix = max_result_suffix.max(result_suffix(&r.name));
        match r.server_state {
            RESULT_SERVER_STATE_INACTIVE | RESULT_SERVER_STATE_UNSENT => nunsent += 1,
            RESULT_SERVER_STATE_IN_PROGRESS => {
                if r.report_deadline < now {
                    r.server_state = RESULT_SERVER_STATE_OVER;
                    r.outcome = RESULT_OUTCOME_NO_REPLY;
                    st.events.push(HostEvent::Timeout {
                        host_id: r.hostid,
                        gavid: generalized_app_version_id(r.app_version_id, wu_appid),
                        delay_bound: st.wu.delay_bound as f64,
                    });
                    st.report.results_timed_out += 1;
                    nover += 1;
                    nno_reply += 1;
                } else {
                    ninprogress += 1;
                }
            }
            RESULT_SERVER_STATE_OVER => {
                nover += 1;
                match r.outcome {
                    RESULT_OUTCOME_COULDNT_SEND => ncouldnt_send += 1,
                    RESULT_OUTCOME_SUCCESS => {
                        if r.validate_state == VALIDATE_STATE_INIT {
                            if canonical_result_files_deleted {
                                r.validate_state = VALIDATE_STATE_TOO_LATE;
                            } else {
                                have_new_result_to_validate = true;
                            }
                        }
                        // Invalid results do not count as successes.
                        if r.validate_state != VALIDATE_STATE_INVALID {
                            nsuccess += 1;
                        }
                    }
                    RESULT_OUTCOME_CLIENT_ERROR => {
                        // A job the user aborted is not the job's fault.
                        if r.exit_status == EXIT_ABORTED_VIA_GUI {
                            nno_reply += 1;
                        } else {
                            nerrors += 1;
                        }
                    }
                    RESULT_OUTCOME_VALIDATE_ERROR => nerrors += 1,
                    RESULT_OUTCOME_CLIENT_DETACHED | RESULT_OUTCOME_NO_REPLY => nno_reply += 1,
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let wu = &mut st.wu;
    if have_new_result_to_validate && nsuccess >= wu.min_quorum {
        wu.need_validate = 1;
    }

    // The limit on successes is checked in the validator pass.
    if ncouldnt_send > 0 {
        wu.error_mask |= WU_ERROR_COULDNT_SEND_RESULT;
    }
    // A workunit that has results with errors and nothing else to wait for
    // (no result succeeded, none is in progress, none timed out and may still
    // come back) gives up its homogeneous redundancy class and app version, so
    // that other platforms and versions can have a go (`handle_wu` in
    // `sched/transitioner.cpp`).
    if nerrors > 0 && !(nsuccess > 0 || ninprogress > 0 || nno_reply > 0) && !config.hr_class_static
    {
        wu.hr_class = 0;
        wu.app_version_id = 0;
    }
    if nerrors > wu.max_error_results {
        wu.error_mask |= WU_ERROR_TOO_MANY_ERROR_RESULTS;
    }

    let mut n_new_results_needed = (wu.target_nresults - nunsent - ninprogress - nsuccess).max(0);
    let n_new_results_allowed = wu.max_total_results - ntotal;
    let too_many = if n_new_results_allowed < 0 {
        true
    } else if n_new_results_allowed == 0 {
        n_new_results_needed > 0
    } else {
        n_new_results_needed = n_new_results_needed.min(n_new_results_allowed);
        false
    };
    if too_many {
        wu.error_mask |= WU_ERROR_TOO_MANY_TOTAL_RESULTS;
    }

    if wu.error_mask != 0 {
        // Send nothing more, and hand the workunit to assimilation.
        for res in st.results.iter_mut() {
            let r = &mut res.row;
            match r.server_state {
                RESULT_SERVER_STATE_INACTIVE | RESULT_SERVER_STATE_UNSENT => {
                    r.server_state = RESULT_SERVER_STATE_OVER;
                    r.outcome = RESULT_OUTCOME_DIDNT_NEED;
                }
                RESULT_SERVER_STATE_OVER
                    if r.outcome == RESULT_OUTCOME_SUCCESS
                        && matches!(
                            r.validate_state,
                            VALIDATE_STATE_INIT | VALIDATE_STATE_INCONCLUSIVE
                        ) =>
                {
                    r.validate_state = VALIDATE_STATE_NO_CHECK;
                }
                _ => {}
            }
        }
        if wu.assimilate_state == ASSIMILATE_INIT {
            wu.assimilate_state = ASSIMILATE_READY;
        }
    } else if wu.canonical_resultid == 0 && n_new_results_needed > 0 {
        // An assigned job (`TRANSITION_NO_NEW_RESULTS`) gets its results
        // too, reserved; upstream's scheduler would create them on sending.
        let priority_increase = if nover > 0 && config.reliable_priority_on_over != 0 {
            config.reliable_priority_on_over
        } else if nover > 0 && nerrors == 0 && config.reliable_priority_on_over_except_error != 0 {
            config.reliable_priority_on_over_except_error
        } else {
            0
        };
        for j in 0..n_new_results_needed {
            let row = new_result(
                wu,
                max_result_suffix + j + 1,
                priority_increase,
                config,
                ctx.signer,
                now,
                ctx.random,
            )?;
            st.results.push(Res { row, orig: None });
            st.report.results_created += 1;
        }
    }

    let mut all_over_and_validated = true;
    let mut all_over_and_ready_to_assimilate = true;
    let mut most_recently_returned = 0;
    for res in &st.results {
        let r = &res.row;
        if r.server_state == RESULT_SERVER_STATE_OVER {
            most_recently_returned = most_recently_returned.max(r.received_time);
            if r.outcome == RESULT_OUTCOME_SUCCESS {
                if r.validate_state == VALIDATE_STATE_INIT {
                    all_over_and_validated = false;
                    all_over_and_ready_to_assimilate = false;
                }
            } else if r.outcome == RESULT_OUTCOME_NO_REPLY && now < r.report_deadline {
                all_over_and_validated = false;
            }
        } else {
            all_over_and_validated = false;
            all_over_and_ready_to_assimilate = false;
        }
    }

    let wu = &mut st.wu;
    if all_over_and_ready_to_assimilate
        && wu.assimilate_state == ASSIMILATE_INIT
        && !st.results.is_empty()
        && wu.canonical_resultid > 0
    {
        wu.assimilate_state = ASSIMILATE_READY;
    }

    // Once assimilated, the files can go, in two transitions. This one marks
    // them FILE_DELETE_READY and deletes nothing, so a report that races it
    // fails the revision check and keeps its files. A transition that reads
    // the ready state deletes the objects; the caller does that before it
    // writes FILE_DELETE_DONE.
    let mut delete_reserved = false;
    if wu.file_delete_state == FILE_DELETE_READY {
        wu.file_delete_state = FILE_DELETE_DONE;
        for fi in file_infos(&wu.xml_doc) {
            // Only files this project serves, and not shared ones. Work
            // imported from a stock server names the stock download URL.
            let ours =
                cb_proto::xml::line::parse_str(&file_info_body(&wu.xml_doc, &fi.name), "<url>")
                    .is_some_and(|u| config.is_own_download_url(&u));
            if ours && !fi.no_delete {
                st.delete_downloads.push(fi.name);
            }
        }
    }
    for res in st.results.iter_mut() {
        let r = &mut res.row;
        if r.file_delete_state == FILE_DELETE_READY {
            r.file_delete_state = FILE_DELETE_DONE;
            for fi in file_infos(&r.xml_doc_in) {
                if !fi.no_delete {
                    st.delete_uploads.push(fi.name);
                }
            }
        }
    }
    let mut deferred_file_delete_time = 0;
    if wu.assimilate_state == ASSIMILATE_DONE {
        let delete_at = most_recently_returned + config.delete_delay as i64;
        if now >= delete_at {
            if all_over_and_validated && wu.file_delete_state == FILE_DELETE_INIT {
                wu.file_delete_state = FILE_DELETE_READY;
                delete_reserved = true;
            }
            for (i, res) in st.results.iter_mut().enumerate() {
                // The canonical result stays until every success is validated.
                if Some(i) == canonical_index && !all_over_and_validated {
                    continue;
                }
                let r = &mut res.row;
                let do_delete = match r.outcome {
                    RESULT_OUTCOME_CLIENT_ERROR => true,
                    RESULT_OUTCOME_SUCCESS => r.validate_state != VALIDATE_STATE_INIT,
                    _ => false,
                };
                if do_delete && r.file_delete_state == FILE_DELETE_INIT {
                    r.file_delete_state = FILE_DELETE_READY;
                    delete_reserved = true;
                }
            }
        } else {
            deferred_file_delete_time = delete_at;
        }
    }

    if wu.canonical_resultid != 0 || wu.error_mask != 0 {
        wu.transition_time = NEVER;
    } else {
        wu.transition_time = safety_net_time(wu, now);
    }
    for res in &st.results {
        if res.row.server_state == RESULT_SERVER_STATE_IN_PROGRESS {
            wu.transition_time = wu.transition_time.min(res.row.report_deadline);
        }
    }
    if deferred_file_delete_time != 0 {
        wu.transition_time = wu.transition_time.min(deferred_file_delete_time);
    }
    if delete_reserved {
        // Come straight back to delete what was just marked.
        wu.transition_time = now;
    }
    // Upstream pushes a transition time in the past further out to shed
    // load. Here a time in the past can only be a deadline that passed
    // while this ran; the sweep picks it up.
    if wu.transition_time < now {
        wu.transition_time = now + 60;
    }
    Ok(())
}

/// The text of the `<file_info>` element of `xml` whose name is `name`.
fn file_info_body(xml: &str, name: &str) -> String {
    let needle = format!("<name>{name}</name>");
    let mut rest = xml;
    while let Some(i) = rest.find("<file_info>") {
        let after = &rest[i..];
        let end = after.find("</file_info>").unwrap_or(after.len());
        let body = &after[..end];
        if body.contains(&needle) {
            return body.to_string();
        }
        rest = &after[end..];
        if rest.len() < 12 {
            break;
        }
        rest = &rest[12..];
    }
    String::new()
}

fn is_unreplicated(wu: &Workunit, app: &App) -> bool {
    wu.target_nresults == 1 && app.target_nresults > 1
}

fn valid_event(r: &JobResult, wu: &Workunit, app: &App, credit: f64) -> HostEvent {
    let usable = r.runtime_outlier == 0 && r.elapsed_time > 0.0 && wu.rsc_fpops_est > 0.0;
    HostEvent::Valid {
        host_id: r.hostid,
        user_id: r.userid,
        gavid: generalized_app_version_id(r.app_version_id, r.appid),
        turnaround: (r.received_time - r.sent_time) as f64,
        et_sample: usable.then(|| r.elapsed_time / wu.rsc_fpops_est),
        sent_time: r.sent_time as f64,
        credit,
        count_consecutive: !is_unreplicated(wu, app),
    }
}

fn invalid_event(r: &JobResult) -> HostEvent {
    HostEvent::Invalid {
        host_id: r.hostid,
        gavid: generalized_app_version_id(r.app_version_id, r.appid),
    }
}

/// Credit for a set of agreeing results, from their run times (upstream's
/// `--credit_from_runtime`): the low average of what each claims.
fn credit_from_runtime(wu: &Workunit, results: &[&JobResult], max_granted_credit: f64) -> f64 {
    let mut cc = Vec::new();
    for r in results {
        let flops = if r.flops_estimate == 1e9 {
            AVG_CPU_FPOPS
        } else {
            r.flops_estimate
        };
        let mut pfc = flops * r.elapsed_time;
        if pfc > wu.rsc_fpops_bound * 2.0 {
            pfc = wu.rsc_fpops_bound;
        }
        let c = pfc * COBBLESTONE_SCALE;
        if c > 0.0 && (max_granted_credit <= 0.0 || c <= max_granted_credit) {
            cc.push(c);
        }
    }
    if cc.is_empty() {
        return 0.0;
    }
    let credit = low_average(&cc);
    if max_granted_credit > 0.0 {
        credit.min(max_granted_credit)
    } else {
        credit
    }
}

/// How long to wait before validating again after a storage or webhook
/// failure. Upstream waits six hours for an operator; these failures pass.
const VALIDATE_RETRY_DELAY: i64 = 600;

enum TransitionTime {
    NoChange,
    Never,
    Delayed,
    Immediate,
}

/// Indexes of the results the validator pass needs fingerprints for.
fn results_to_fingerprint(st: &State) -> Vec<usize> {
    let wu = &st.wu;
    let over_success = |r: &JobResult| {
        r.server_state == RESULT_SERVER_STATE_OVER && r.outcome == RESULT_OUTCOME_SUCCESS
    };
    if wu.canonical_resultid != 0 {
        let Some(ci) = st
            .results
            .iter()
            .position(|r| r.row.id == wu.canonical_resultid)
        else {
            return Vec::new();
        };
        let mut v: Vec<usize> = st
            .results
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                over_success(&r.row)
                    && matches!(
                        r.row.validate_state,
                        VALIDATE_STATE_INIT | VALIDATE_STATE_INCONCLUSIVE
                    )
            })
            .map(|(i, _)| i)
            .collect();
        if !v.is_empty() {
            v.push(ci);
        }
        v
    } else {
        let v: Vec<usize> = st
            .results
            .iter()
            .enumerate()
            .filter(|(_, r)| over_success(&r.row) && r.row.validate_state != VALIDATE_STATE_INVALID)
            .map(|(i, _)| i)
            .collect();
        if (v.len() as i32) < wu.min_quorum {
            Vec::new()
        } else {
            v
        }
    }
}

/// `handle_wu` of the validator with `check_set` and `check_pair`, on the
/// in-memory state. Returns true when the transitioner pass must run again.
fn validator_pass(st: &mut State, app: &App, checked: &Checked, ctx: &Ctx) -> bool {
    let fps = &checked.fps;
    let ids: Vec<i64> = st.results.iter().map(|r| r.row.id).collect();
    let now = ctx.now;
    let max_granted_credit = ctx.config.max_granted_credit;
    let mut transition_time = TransitionTime::NoChange;

    if st.wu.canonical_resultid != 0 {
        let canonical_index = st
            .results
            .iter()
            .position(|r| r.row.id == st.wu.canonical_resultid);
        if let Some(ci) = canonical_index {
            let canonical_credit = st.results[ci].row.granted_credit;
            let canonical_fp = fps.get(&ci).cloned();
            for i in 0..st.results.len() {
                if i == ci {
                    continue;
                }
                let r = &st.results[i].row;
                if r.server_state != RESULT_SERVER_STATE_OVER || r.outcome != RESULT_OUTCOME_SUCCESS
                {
                    continue;
                }
                if !matches!(
                    r.validate_state,
                    VALIDATE_STATE_INIT | VALIDATE_STATE_INCONCLUSIVE
                ) {
                    continue;
                }
                // check_pair
                let (Some(mine), Some(canon)) = (fps.get(&i), canonical_fp.as_ref()) else {
                    transition_time = TransitionTime::Delayed;
                    break;
                };
                if *mine == Fp::Transient || *canon == Fp::Transient {
                    transition_time = TransitionTime::Delayed;
                    break;
                }
                // This may be the last result: let the transitioner pass
                // look at file deletion.
                transition_time = TransitionTime::Immediate;
                let valid = match (mine, canon) {
                    (Fp::Ok(_), Fp::Ok(_)) => Some(checked.agree(&ids, i, ci)),
                    _ => None,
                };
                let wu = &st.wu;
                let r = &mut st.results[i].row;
                match valid {
                    Some(true) => {
                        let credit = checked.credits.get(&i).copied().unwrap_or(canonical_credit);
                        r.validate_state = VALIDATE_STATE_VALID;
                        r.granted_credit = credit;
                        st.events.push(valid_event(r, wu, app, credit));
                        st.report.results_valid += 1;
                    }
                    Some(false) => {
                        r.validate_state = VALIDATE_STATE_INVALID;
                        st.events.push(invalid_event(r));
                        st.report.results_invalid += 1;
                    }
                    None => {
                        r.outcome = RESULT_OUTCOME_VALIDATE_ERROR;
                        r.validate_state = VALIDATE_STATE_INVALID;
                        st.events.push(invalid_event(r));
                        st.report.results_invalid += 1;
                    }
                }
            }
        }
    } else {
        let viable: Vec<usize> = st
            .results
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                r.row.server_state == RESULT_SERVER_STATE_OVER
                    && r.row.outcome == RESULT_OUTCOME_SUCCESS
                    && r.row.validate_state != VALIDATE_STATE_INVALID
            })
            .map(|(i, _)| i)
            .collect();
        if viable.len() as i32 >= st.wu.min_quorum {
            // check_set
            let min_valid = (st.wu.min_quorum / 2 + 1) as usize;
            let mut had_error = vec![false; viable.len()];
            let mut retry = false;
            let mut good_results = 0;
            for (k, &i) in viable.iter().enumerate() {
                match fps.get(&i) {
                    Some(Fp::Ok(_)) => good_results += 1,
                    Some(Fp::Invalid) => {
                        let r = &mut st.results[i].row;
                        r.outcome = RESULT_OUTCOME_VALIDATE_ERROR;
                        r.validate_state = VALIDATE_STATE_INVALID;
                        had_error[k] = true;
                    }
                    Some(Fp::Transient) | None => {
                        retry = true;
                        had_error[k] = true;
                    }
                }
            }
            let mut canonical: Option<usize> = None;
            if good_results >= st.wu.min_quorum {
                for (k, &i) in viable.iter().enumerate() {
                    if had_error[k] {
                        continue;
                    }
                    let matches: Vec<bool> = viable
                        .iter()
                        .enumerate()
                        .map(|(m, &j)| !had_error[m] && checked.agree(&ids, i, j))
                        .collect();
                    if matches.iter().filter(|&&b| b).count() >= min_valid {
                        for (m, &j) in viable.iter().enumerate() {
                            if had_error[m] {
                                continue;
                            }
                            st.results[j].row.validate_state = if matches[m] {
                                VALIDATE_STATE_VALID
                            } else {
                                VALIDATE_STATE_INVALID
                            };
                        }
                        canonical = Some(i);
                        break;
                    }
                }
            }
            if retry {
                // Upstream goes on and counts the unchecked results as
                // inconclusive, which asks for another replica. A storage
                // or webhook failure says nothing about the results, so
                // leave them as they are and look again later.
                for (k, &i) in viable.iter().enumerate() {
                    if had_error[k] && st.results[i].row.validate_state == VALIDATE_STATE_INVALID {
                        st.events.push(invalid_event(&st.results[i].row));
                        st.report.results_invalid += 1;
                    }
                }
                st.wu.need_validate = 0;
                st.wu.transition_time = st.wu.transition_time.min(now + VALIDATE_RETRY_DELAY);
                return false;
            }

            let mut credit = 0.0;
            if canonical.is_some() {
                let rows: Vec<&JobResult> = viable.iter().map(|&i| &st.results[i].row).collect();
                credit = credit_from_runtime(&st.wu, &rows, max_granted_credit);
            }

            let mut n_viable_results = 0;
            for &i in &viable {
                let wu = &st.wu;
                let r = &mut st.results[i].row;
                if r.outcome != RESULT_OUTCOME_SUCCESS || r.validate_state == VALIDATE_STATE_INVALID
                {
                    transition_time = TransitionTime::Immediate;
                } else {
                    n_viable_results += 1;
                }
                match r.validate_state {
                    VALIDATE_STATE_VALID => {
                        let credit = checked.credits.get(&i).copied().unwrap_or(credit);
                        r.granted_credit = credit;
                        st.events.push(valid_event(r, wu, app, credit));
                        st.report.results_valid += 1;
                    }
                    VALIDATE_STATE_INVALID => {
                        st.events.push(invalid_event(r));
                        st.report.results_invalid += 1;
                    }
                    VALIDATE_STATE_INIT => r.validate_state = VALIDATE_STATE_INCONCLUSIVE,
                    _ => {}
                }
            }

            if let Some(ci) = canonical {
                // Hand over to assimilation. The workunit comes back for
                // file deletion when assimilation is acknowledged.
                transition_time = TransitionTime::Never;
                st.wu.canonical_resultid = st.results[ci].row.id;
                st.wu.canonical_credit = st.results[ci].row.granted_credit;
                st.wu.assimilate_state = ASSIMILATE_READY;
                for res in st.results.iter_mut() {
                    if matches!(
                        res.row.server_state,
                        RESULT_SERVER_STATE_INACTIVE | RESULT_SERVER_STATE_UNSENT
                    ) {
                        res.row.server_state = RESULT_SERVER_STATE_OVER;
                        res.row.outcome = RESULT_OUTCOME_DIDNT_NEED;
                    }
                }
            } else {
                if n_viable_results > st.wu.max_success_results {
                    st.wu.error_mask |= WU_ERROR_TOO_MANY_SUCCESS_RESULTS;
                    transition_time = TransitionTime::Immediate;
                }
                // No consensus among a full set: ask for one more.
                if n_viable_results >= st.wu.target_nresults {
                    st.wu.target_nresults = n_viable_results + 1;
                    transition_time = TransitionTime::Immediate;
                }
            }
        }
    }

    st.wu.need_validate = 0;
    match transition_time {
        TransitionTime::Immediate => {
            st.wu.transition_time = now;
            true
        }
        TransitionTime::Delayed => {
            st.wu.transition_time = st.wu.transition_time.min(now + VALIDATE_RETRY_DELAY);
            false
        }
        TransitionTime::Never => {
            st.wu.transition_time = NEVER;
            false
        }
        TransitionTime::NoChange => false,
    }
}

#[derive(Deserialize)]
struct WebhookReply {
    #[serde(default)]
    results: Vec<WebhookResult>,
    /// Pairwise equivalence: result-ID pairs that match. Present (even when
    /// empty) selects the pairwise form; see `pairwise_matches`.
    #[serde(default)]
    matches: Option<Vec<(i64, i64)>>,
}

/// The most pairs a reply may list. Pairs grow with the square of the
/// results of a workunit, so the list is bounded: 4096 covers every pair of
/// 91 results, far above any quorum.
pub const MAX_WEBHOOK_PAIRS: usize = 4096;

/// A pair of result IDs as a set member: smaller ID first.
fn pair_key(a: i64, b: i64) -> (i64, i64) {
    (a.min(b), a.max(b))
}

/// Reads the pairwise form of a reply. `Ok(None)` is the fingerprint form
/// (no `matches`, or `null`). `Ok(Some(set))` is the pairwise form. `Err(())`
/// rejects the whole reply, which then counts as a failed call: more than
/// `MAX_WEBHOOK_PAIRS` pairs, a pair of a result with itself, an ID the
/// request did not name, or `matches` beside any `fingerprint`.
fn pairwise_matches(
    reply: &WebhookReply,
    request_ids: &[i64],
) -> Result<Option<HashSet<(i64, i64)>>, ()> {
    let Some(pairs) = &reply.matches else {
        return Ok(None);
    };
    if pairs.len() > MAX_WEBHOOK_PAIRS || reply.results.iter().any(|r| r.fingerprint.is_some()) {
        return Err(());
    }
    let mut set = HashSet::new();
    for &(a, b) in pairs {
        if a == b || !request_ids.contains(&a) || !request_ids.contains(&b) {
            return Err(());
        }
        set.insert(pair_key(a, b));
    }
    Ok(Some(set))
}

#[derive(Deserialize)]
struct WebhookResult {
    id: i64,
    #[serde(default)]
    fingerprint: Option<String>,
    #[serde(default)]
    invalid: bool,
    #[serde(default)]
    retry: bool,
    /// Credit to grant for this result instead of the computed one. Only a
    /// JSON number counts; any other value is read as no credit, so it
    /// cannot cost the reply its verdicts.
    #[serde(default, deserialize_with = "number_or_none")]
    credit: Option<f64>,
}

fn number_or_none<'de, D>(d: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(v.as_f64())
}

/// The credit a validator asked for, if it is usable: a finite number that is
/// not negative, cut to `max_granted_credit` as the computed credit is.
/// Anything else is ignored and the computed credit stands.
fn webhook_credit(v: Option<f64>, max_granted_credit: f64) -> Option<f64> {
    let c = v.filter(|c| c.is_finite() && *c >= 0.0)?;
    Some(if max_granted_credit > 0.0 {
        c.min(max_granted_credit)
    } else {
        c
    })
}

/// Credit the validator asked for, by result index.
type CreditOverrides = HashMap<usize, f64>;

/// The fingerprints of the results a validator pass looks at, and credit a
/// webhook asked for.
#[derive(Default)]
struct Checked {
    fps: HashMap<usize, Fp>,
    credits: CreditOverrides,
    /// Pairs of result IDs the webhook said match, when it answered in the
    /// pairwise form. `None`: results agree when their fingerprints are equal.
    pairs: Option<HashSet<(i64, i64)>>,
}

impl Checked {
    /// Whether results `i` and `j` (indexes into the workunit's results)
    /// match: a listed pair, or equal fingerprints. A result matches itself.
    fn agree(&self, ids: &[i64], i: usize, j: usize) -> bool {
        match &self.pairs {
            Some(p) => i == j || p.contains(&pair_key(ids[i], ids[j])),
            None => self.fps.get(&i) == self.fps.get(&j),
        }
    }
}

struct OutputStat {
    name: String,
    /// The template's `<open_name>`; empty when it gives none.
    open_name: String,
    /// Position of the file in the result template, counting files that are
    /// not looked up, absent or left out of the request.
    index: usize,
    md5: String,
    nbytes: u64,
}

/// Looks up the stored output files of a result. `Err(Fp::Invalid)` when a
/// required output (one not marked `<optional/>`) that the client is told to
/// upload is not stored, whatever the validator: a client that counts an
/// upload as done without sending it (a size query that answered at least
/// its file's size) reports a result with no finished object, and no result
/// is accepted without its data. An output whose `<file_info>` has no
/// upload URL stays on the client, which reports the result without it, so
/// it is not looked up. `Err(Fp::Transient)` when the store cannot be read.
/// Files marked `<no_validate/>` are looked up but left out of the list.
async fn output_stats(r: &JobResult, files: &dyn Files) -> Result<Vec<OutputStat>, Fp> {
    let mut v = Vec::new();
    for f in output_files(&r.xml_doc_in) {
        if !f.uploadable {
            continue;
        }
        match files.stat(FileKind::Upload, &f.name).await {
            Ok(Some(s)) => {
                if !f.no_validate {
                    v.push(OutputStat {
                        name: f.name,
                        open_name: f.open_name,
                        index: f.index,
                        md5: s.md5,
                        nbytes: s.nbytes,
                    });
                }
            }
            Ok(None) if f.optional => {}
            Ok(None) => return Err(Fp::Invalid),
            Err(_) => return Err(Fp::Transient),
        }
    }
    Ok(v)
}

/// The body of a validator call. `mode` is `check_set` while the workunit has
/// no canonical result and `check_pair` once it has one and a late result is
/// compared with it (the validator's `check_set` and `check_pair`). `call_id`
/// follows the workunit's revision as the transition read it, so a call
/// repeated by a transition that did not commit has the same ID.
fn webhook_request(st: &State, app: &App, results: &[serde_json::Value]) -> serde_json::Value {
    let wu = &st.wu;
    let pair = wu.canonical_resultid != 0;
    let mode = if pair { "check_pair" } else { "check_set" };
    json!({
        "call_id": format!("wu{}-rev{}-{}", wu.id, wu.rev, mode),
        "mode": mode,
        "workunit": {
            "id": wu.id,
            "name": wu.name,
            "app": app.name,
            "batch": wu.batch,
            "min_quorum": wu.min_quorum,
        },
        "canonical_result_id": if pair { serde_json::Value::from(wu.canonical_resultid) } else { serde_json::Value::Null },
        "results": results,
    })
}

async fn fingerprints(st: &State, app: &App, ctx: &Ctx<'_>) -> Checked {
    let wanted = results_to_fingerprint(st);
    let mut fps = HashMap::new();
    let mut credits = CreditOverrides::new();
    let mut pairs = None;
    if wanted.is_empty() {
        return Checked::default();
    }
    match app.validator.as_str() {
        "bitwise" => {
            for i in wanted {
                let fp = match output_stats(&st.results[i].row, ctx.files).await {
                    Ok(stats) => Fp::Ok(
                        stats
                            .iter()
                            .map(|s| format!("{}:{}", s.nbytes, s.md5))
                            .collect::<Vec<_>>()
                            .join(","),
                    ),
                    Err(fp) => fp,
                };
                fps.insert(i, fp);
            }
        }
        "webhook" => {
            let canonical_id = st.wu.canonical_resultid;
            let mut body_results = Vec::new();
            for &i in &wanted {
                let r = &st.results[i].row;
                match output_stats(r, ctx.files).await {
                    Ok(stats) => body_results.push(json!({
                        "id": r.id,
                        "name": r.name,
                        "host_id": r.hostid,
                        "canonical": canonical_id != 0 && r.id == canonical_id,
                        "files": stats
                            .iter()
                            .map(|s| json!({
                                "name": s.name,
                                "open_name": if s.open_name.is_empty() {
                                    serde_json::Value::Null
                                } else {
                                    serde_json::Value::from(s.open_name.clone())
                                },
                                "index": s.index,
                                "md5": s.md5,
                                "nbytes": s.nbytes,
                            }))
                            .collect::<Vec<_>>(),
                    })),
                    Err(fp) => {
                        fps.insert(i, fp);
                    }
                }
            }
            if !body_results.is_empty() {
                let body = webhook_request(st, app, &body_results);
                let request_ids: Vec<i64> = body_results
                    .iter()
                    .filter_map(|r| r["id"].as_i64())
                    .collect();
                let reply = ctx
                    .webhook
                    .post_json(&app.validator_url, &body)
                    .await
                    .ok()
                    .and_then(|v| serde_json::from_value::<WebhookReply>(v).ok());
                // A reply with unusable pairs is a failed call.
                let reply = reply.and_then(|rep| match pairwise_matches(&rep, &request_ids) {
                    Ok(p) => {
                        pairs = p;
                        Some(rep)
                    }
                    Err(()) => None,
                });
                let pairwise = pairs.is_some();
                for &i in &wanted {
                    if fps.contains_key(&i) {
                        continue;
                    }
                    let id = st.results[i].row.id;
                    let answer = reply
                        .as_ref()
                        .and_then(|rep| rep.results.iter().find(|x| x.id == id));
                    if let Some(c) =
                        answer.and_then(|x| webhook_credit(x.credit, ctx.config.max_granted_credit))
                    {
                        credits.insert(i, c);
                    }
                    let fp = match answer {
                        Some(x) if x.retry => Fp::Transient,
                        Some(x) if x.invalid => Fp::Invalid,
                        Some(WebhookResult {
                            fingerprint: Some(f),
                            ..
                        }) => Fp::Ok(f.clone()),
                        // Pairwise form: an entry with no flag is a result
                        // the validator could read; the pairs decide.
                        Some(_) if pairwise => Fp::Ok(String::new()),
                        // No answer for this result, or no usable reply.
                        _ => Fp::Transient,
                    };
                    fps.insert(i, fp);
                }
            }
        }
        // "trivial": every result that came back with its outputs is right.
        // A result already checked (the canonical one, looked at again
        // beside a late result) is not looked up again.
        _ => {
            for i in wanted {
                let row = &st.results[i].row;
                let fp = if matches!(
                    row.validate_state,
                    VALIDATE_STATE_INIT | VALIDATE_STATE_INCONCLUSIVE
                ) {
                    match output_stats(row, ctx.files).await {
                        Ok(_) => Fp::Ok(String::new()),
                        Err(fp) => fp,
                    }
                } else {
                    Fp::Ok(String::new())
                };
                fps.insert(i, fp);
            }
        }
    }
    Checked {
        fps,
        credits,
        pairs,
    }
}

#[derive(Deserialize, Default, Clone)]
#[serde(default)]
struct CreditRow {
    id: i64,
    expavg_credit: f64,
    expavg_time: f64,
    avg_turnaround: f64,
    /// An account's team (0 for none) and, from the join in the same read,
    /// whether the team exists: `team_id` is null when the account has no team
    /// or the team is gone.
    teamid: i64,
    team_id: Option<i64>,
}

fn guard_sql(wu_id: i64, token: i64) -> (&'static str, [Value; 2]) {
    (
        " AND EXISTS (SELECT 1 FROM workunit WHERE id = ? AND commit_token = ?)",
        [wu_id.into(), token.into()],
    )
}

/// Makes a statement apply only if this batch's workunit update did.
///
/// The token, not the new revision, is what the guard tests: a transition
/// that lost the race would find the revision it expected to write already
/// there, written by the winner.
fn guarded(mut s: Stmt, wu_id: i64, token: i64) -> Stmt {
    let (sql, params) = guard_sql(wu_id, token);
    s.sql.push_str(sql);
    for p in params {
        s = s.bind(p);
    }
    s
}

/// Turns the host events of a transition into guarded statements.
///
/// Counters are written relative to the stored value. The averages are
/// written as computed from the values read here; two transitions crediting
/// one host (or one account) at the same instant can lose one sample of an
/// average, never credit. A team's is computed in its statement from the stored
/// row and loses none.
async fn host_statements(st: &State, ctx: &Ctx<'_>, token: i64) -> CoreResult<Vec<Stmt>> {
    if st.events.is_empty() {
        return Ok(Vec::new());
    }
    let wu_id = st.wu.id;
    let now = ctx.now as f64;
    let quota = ctx.config.daily_result_quota;

    let mut host_ids = BTreeSet::new();
    let mut user_ids = BTreeSet::new();
    let mut hav_keys = BTreeSet::new();
    for e in &st.events {
        match e {
            HostEvent::Timeout { host_id, gavid, .. } | HostEvent::Invalid { host_id, gavid } => {
                hav_keys.insert((*host_id, *gavid));
            }
            HostEvent::Valid {
                host_id,
                user_id,
                gavid,
                ..
            } => {
                host_ids.insert(*host_id);
                user_ids.insert(*user_id);
                hav_keys.insert((*host_id, *gavid));
            }
        }
    }
    let id_list = |ids: &BTreeSet<i64>| {
        ids.iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    let mut reads = Vec::new();
    if !host_ids.is_empty() {
        reads.push(Stmt::new(format!(
            "SELECT id, expavg_credit, expavg_time, avg_turnaround FROM host WHERE id IN ({})",
            id_list(&host_ids)
        )));
        // The team's average comes in the same statement: a join on `teamid`,
        // which finds nothing for an account with no team (`teamid` 0), so a
        // grant to an account without a team reads and writes nothing more.
        reads.push(Stmt::new(format!(
            "SELECT u.id, u.expavg_credit, u.expavg_time, u.teamid, t.id AS team_id \
             FROM user u LEFT JOIN team t ON t.id = u.teamid WHERE u.id IN ({})",
            id_list(&user_ids)
        )));
    }
    let mut hav_read = Stmt::new(format!(
        "SELECT * FROM host_app_version WHERE {}",
        vec!["(host_id = ? AND app_version_id = ?)"; hav_keys.len()].join(" OR ")
    ));
    for (h, g) in &hav_keys {
        hav_read = hav_read.bind(*h).bind(*g);
    }
    reads.push(hav_read);
    let mut read = ctx.db.batch(reads).await?.into_iter();

    let mut hosts: BTreeMap<i64, CreditRow> = BTreeMap::new();
    let mut users: BTreeMap<i64, CreditRow> = BTreeMap::new();
    if !host_ids.is_empty() {
        for h in read.next().unwrap_or_default().decode::<CreditRow>()? {
            hosts.insert(h.id, h);
        }
        for u in read.next().unwrap_or_default().decode::<CreditRow>()? {
            users.insert(u.id, u);
        }
    }
    struct Hav {
        row: HostAppVersion,
        orig_max_jobs: i32,
        max_jobs_set: bool,
        max_jobs_delta: i32,
        consecutive_reset: bool,
        consecutive_delta: i32,
    }
    let mut havs: BTreeMap<(i64, i64), Hav> = BTreeMap::new();
    for row in read.next().unwrap_or_default().decode::<HostAppVersion>()? {
        havs.insert(
            (row.host_id, row.app_version_id),
            Hav {
                orig_max_jobs: row.max_jobs_per_day,
                row,
                max_jobs_set: false,
                max_jobs_delta: 0,
                consecutive_reset: false,
                consecutive_delta: 0,
            },
        );
    }

    // Credit for the teams of the accounts credited: `(team, credit, sent time)`
    // for each grant. The team's average is not worked out here from a row read
    // earlier: the statement does it from the stored row (`team_grant_statement`),
    // so concurrent transitions for members of one team lose no sample. The
    // account's and the computer's averages below are still written from the
    // values read here.
    let mut team_grants: Vec<(i64, f64, f64)> = Vec::new();
    let mut host_credit: BTreeMap<i64, f64> = BTreeMap::new();
    let mut user_credit: BTreeMap<i64, f64> = BTreeMap::new();
    // Valid tasks, for the leaderboards' "jobs completed".
    let mut host_valid: BTreeMap<i64, i64> = BTreeMap::new();
    let mut user_valid: BTreeMap<i64, i64> = BTreeMap::new();
    for e in &st.events {
        match e {
            HostEvent::Timeout {
                host_id,
                gavid,
                delay_bound,
            } => {
                // result_timed_out
                if let Some(h) = havs.get_mut(&(*host_id, *gavid)) {
                    let mut t = h.row.turnaround();
                    t.update_hav(*delay_bound);
                    h.row.set_turnaround(t);
                    let mut n = h.row.max_jobs_per_day;
                    if n == 0 || n > quota {
                        n = quota;
                    }
                    h.row.max_jobs_per_day = (n - 1).max(1);
                    h.max_jobs_set = true;
                    h.max_jobs_delta = 0;
                    h.consecutive_reset = true;
                    h.consecutive_delta = 0;
                }
            }
            HostEvent::Invalid { host_id, gavid } => {
                // is_invalid
                if let Some(h) = havs.get_mut(&(*host_id, *gavid)) {
                    h.consecutive_reset = true;
                    h.consecutive_delta = 0;
                    if h.row.max_jobs_per_day > quota {
                        h.row.max_jobs_per_day -= 1;
                        h.max_jobs_delta -= 1;
                    }
                }
            }
            HostEvent::Valid {
                host_id,
                user_id,
                gavid,
                turnaround,
                et_sample,
                sent_time,
                credit,
                count_consecutive,
            } => {
                // is_valid and grant_credit
                if let Some(h) = hosts.get_mut(host_id) {
                    h.avg_turnaround = if h.avg_turnaround == 0.0 {
                        *turnaround
                    } else {
                        0.7 * h.avg_turnaround + 0.3 * turnaround
                    };
                    update_average(
                        now,
                        *sent_time,
                        *credit,
                        CREDIT_HALF_LIFE,
                        &mut h.expavg_credit,
                        &mut h.expavg_time,
                    );
                    *host_credit.entry(*host_id).or_default() += credit;
                    *host_valid.entry(*host_id).or_default() += 1;
                }
                if let Some(u) = users.get_mut(user_id) {
                    update_average(
                        now,
                        *sent_time,
                        *credit,
                        CREDIT_HALF_LIFE,
                        &mut u.expavg_credit,
                        &mut u.expavg_time,
                    );
                    *user_credit.entry(*user_id).or_default() += credit;
                    *user_valid.entry(*user_id).or_default() += 1;
                    if let Some(team_id) = u.team_id.filter(|_| u.teamid > 0) {
                        team_grants.push((team_id, *credit, *sent_time));
                    }
                }
                if let Some(h) = havs.get_mut(&(*host_id, *gavid)) {
                    h.row.max_jobs_per_day += 1;
                    h.max_jobs_delta += 1;
                    if *count_consecutive {
                        h.consecutive_delta += 1;
                    }
                    if let Some(s) = et_sample {
                        let mut et = h.row.et();
                        et.update_hav(*s);
                        h.row.set_et(et);
                    }
                    let mut t = h.row.turnaround();
                    t.update_hav(*turnaround);
                    h.row.set_turnaround(t);
                }
            }
        }
    }

    let mut out = Vec::new();
    for (id, credit) in &host_credit {
        let h = &hosts[id];
        out.push(guarded(
            stmt!(
                "UPDATE host SET total_credit = total_credit + ?, expavg_credit = ?, expavg_time = ?, \
                 avg_turnaround = ?, valid_results = valid_results + ?, recent_key = ? WHERE id = ?",
                *credit,
                h.expavg_credit,
                h.expavg_time,
                h.avg_turnaround,
                host_valid.get(id).copied().unwrap_or(0),
                crate::avg::recent_key(h.expavg_credit, h.expavg_time),
                *id
            ),
            wu_id,
            token,
        ));
    }
    for (id, credit) in &user_credit {
        let u = &users[id];
        out.push(guarded(
            stmt!(
                "UPDATE user SET total_credit = total_credit + ?, expavg_credit = ?, expavg_time = ?, \
                 valid_results = valid_results + ?, recent_key = ? WHERE id = ?",
                *credit,
                u.expavg_credit,
                u.expavg_time,
                user_valid.get(id).copied().unwrap_or(0),
                crate::avg::recent_key(u.expavg_credit, u.expavg_time),
                *id
            ),
            wu_id,
            token,
        ));
    }
    // The team, when an account credited has one: one statement for each grant,
    // in the same batch, that adds the credit and updates the average from the
    // stored row.
    for (team_id, credit, sent_time) in &team_grants {
        out.push(guarded(
            crate::teams::team_grant_statement(*team_id, now, *sent_time, *credit),
            wu_id,
            token,
        ));
    }
    for ((host_id, gavid), h) in &havs {
        let max_jobs = if h.max_jobs_set {
            Stmt::new("max_jobs_per_day = ?").bind(h.row.max_jobs_per_day)
        } else {
            let _ = h.orig_max_jobs;
            Stmt::new("max_jobs_per_day = max_jobs_per_day + ?").bind(h.max_jobs_delta)
        };
        let consecutive = if h.consecutive_reset {
            Stmt::new("consecutive_valid = ?").bind(h.consecutive_delta)
        } else {
            Stmt::new("consecutive_valid = consecutive_valid + ?").bind(h.consecutive_delta)
        };
        let mut s = Stmt::new(format!(
            "UPDATE host_app_version SET et_n = ?, et_avg = ?, et_var = ?, et_q = ?, turnaround_n = ?, \
             turnaround_avg = ?, turnaround_var = ?, turnaround_q = ?, {}, {} WHERE host_id = ? AND app_version_id = ?",
            max_jobs.sql, consecutive.sql
        ))
        .bind(h.row.et_n)
        .bind(h.row.et_avg)
        .bind(h.row.et_var)
        .bind(h.row.et_q)
        .bind(h.row.turnaround_n)
        .bind(h.row.turnaround_avg)
        .bind(h.row.turnaround_var)
        .bind(h.row.turnaround_q);
        for p in max_jobs.params.into_iter().chain(consecutive.params) {
            s = s.bind(p);
        }
        out.push(guarded(s.bind(*host_id).bind(*gavid), wu_id, token));
    }
    // Tasks written off at their deadline are never returned, so the rollup of
    // the jobs overview cannot find them; they are counted here, in the batch
    // that writes them off.
    let timed_out = st
        .events
        .iter()
        .filter(|e| matches!(e, HostEvent::Timeout { .. }))
        .count();
    if timed_out > 0 {
        out.extend(crate::stats::timeout_statements(
            ctx.now,
            st.wu.appid,
            timed_out as i64,
            wu_id,
            token,
        ));
    }
    Ok(out)
}

fn result_changed(a: &JobResult, b: &JobResult) -> bool {
    a.server_state != b.server_state
        || a.outcome != b.outcome
        || a.validate_state != b.validate_state
        || a.file_delete_state != b.file_delete_state
        || a.granted_credit != b.granted_credit
}

async fn load(db: &dyn Db, wu_id: i64) -> CoreResult<Option<(Workunit, Vec<JobResult>, App)>> {
    let mut rows = db
        .batch(vec![
            stmt!("SELECT * FROM workunit WHERE id = ?", wu_id),
            Stmt::new(format!(
                "SELECT {RESULT_COLS_LIGHT} FROM result WHERE workunitid = ? ORDER BY id"
            ))
            .bind(wu_id),
            stmt!(
                "SELECT * FROM app WHERE id = (SELECT appid FROM workunit WHERE id = ?)",
                wu_id
            ),
        ])
        .await?
        .into_iter();
    let Some(wu) = rows.next().unwrap_or_default().first::<Workunit>()? else {
        return Ok(None);
    };
    let results = rows.next().unwrap_or_default().decode::<JobResult>()?;
    let app = rows
        .next()
        .unwrap_or_default()
        .first::<App>()?
        .ok_or_else(|| CoreError::NotFound(format!("app {} of workunit {}", wu.appid, wu.id)))?;
    Ok(Some((wu, results, app)))
}

/// Brings one workunit up to date: times out late results, validates
/// returned ones, grants credit, creates replacement results, flags the
/// workunit for assimilation, deletes files that are no longer needed, and
/// sets when the workunit must be looked at again.
///
/// Safe to call at any time and any number of times for the same workunit.
pub async fn transition(ctx: &Ctx<'_>, wu_id: i64) -> CoreResult<TransitionReport> {
    const MAX_ATTEMPTS: u32 = 4;
    for attempt in 1..=MAX_ATTEMPTS {
        let Some((wu, results, app)) = load(ctx.db, wu_id).await? else {
            return Ok(TransitionReport {
                workunit_id: wu_id,
                attempts: attempt,
                ..Default::default()
            });
        };
        let orig_rev = wu.rev;
        let mut st = State {
            wu,
            results: results
                .into_iter()
                .map(|r| Res {
                    orig: Some(r.clone()),
                    row: r,
                })
                .collect(),
            events: Vec::new(),
            delete_uploads: Vec::new(),
            delete_downloads: Vec::new(),
            report: TransitionReport {
                workunit_id: wu_id,
                found: true,
                attempts: attempt,
                ..Default::default()
            },
        };
        if st.wu.transitioner_flags == TRANSITION_NONE {
            return Ok(st.report);
        }

        // Transitioner, then validator if there is something to validate,
        // then transitioner again if the validator changed the picture.
        // Each round settles at least one result, so this ends quickly.
        for _ in 0..4 {
            transitioner_pass(&mut st, ctx)?;
            if st.wu.need_validate == 0 {
                break;
            }
            let checked = fingerprints(&st, &app, ctx).await;
            if !validator_pass(&mut st, &app, &checked, ctx) {
                break;
            }
        }

        // Objects go first: deleting twice is harmless, a row saying
        // "deleted" for a file that is still there is a leak.
        if !st.delete_uploads.is_empty() {
            ctx.files
                .delete(FileKind::Upload, &st.delete_uploads)
                .await?;
        }
        if !st.delete_downloads.is_empty() {
            ctx.files
                .delete(FileKind::Download, &st.delete_downloads)
                .await?;
        }

        // Never zero, the value of a workunit no transition has written.
        let token =
            ((i64::from((ctx.random)()) << 31) | i64::from((ctx.random)() & 0x7fff_ffff)) | 1;
        let wu = &st.wu;
        let mut batch = vec![stmt!(
            "UPDATE workunit SET need_validate = ?, canonical_resultid = ?, canonical_credit = ?, \
             transition_time = ?, error_mask = ?, file_delete_state = ?, assimilate_state = ?, \
             target_nresults = ?, app_version_id = ?, hr_class = ?, mod_time = ?, rev = rev + 1, \
             commit_token = ? WHERE id = ? AND rev = ?",
            wu.need_validate,
            wu.canonical_resultid,
            wu.canonical_credit,
            wu.transition_time,
            wu.error_mask,
            wu.file_delete_state,
            wu.assimilate_state,
            wu.target_nresults,
            wu.app_version_id,
            wu.hr_class,
            ctx.now,
            token,
            wu.id,
            orig_rev
        )];
        for res in &st.results {
            match &res.orig {
                None => batch.push(insert_result_stmt(
                    &res.row,
                    WuGuard::Token { id: wu.id, token },
                )),
                Some(orig) if result_changed(orig, &res.row) => batch.push(guarded(
                    stmt!(
                        "UPDATE result SET server_state = ?, outcome = ?, validate_state = ?, \
                         file_delete_state = ?, granted_credit = ?, mod_time = ? WHERE id = ?",
                        res.row.server_state,
                        res.row.outcome,
                        res.row.validate_state,
                        res.row.file_delete_state,
                        res.row.granted_credit,
                        ctx.now,
                        res.row.id
                    ),
                    wu.id,
                    token,
                )),
                Some(_) => {}
            }
        }
        batch.extend(host_statements(&st, ctx, token).await?);
        if !st.delete_downloads.is_empty() {
            let mut s = Stmt::new(format!(
                "DELETE FROM job_file WHERE name IN ({})",
                vec!["?"; st.delete_downloads.len()].join(",")
            ));
            for n in &st.delete_downloads {
                s = s.bind(n.as_str());
            }
            batch.push(guarded(s, wu.id, token));
        }

        let written = ctx.db.batch(batch).await?;
        if written.first().map(|r| r.changes).unwrap_or(0) == 0 {
            // Someone else changed the workunit since it was read.
            continue;
        }
        let mut report = st.report;
        report.canonical_resultid = st.wu.canonical_resultid;
        report.error_mask = st.wu.error_mask;
        report.assimilate_ready = st.wu.assimilate_state == ASSIMILATE_READY;
        report.files_deleted = st.delete_uploads.len() + st.delete_downloads.len();
        report.transition_time = st.wu.transition_time;
        return Ok(report);
    }
    Err(CoreError::Conflict(format!(
        "workunit {wu_id} kept changing during its transition"
    )))
}

/// IDs of workunits whose transition time has come, oldest first. The
/// one-minute sweep runs these; the queue is only a fast path.
pub async fn due_workunits(db: &dyn Db, now: i64, limit: i64) -> CoreResult<Vec<i64>> {
    let rows = db
        .run(stmt!(
            "SELECT id FROM workunit WHERE transition_time < ? AND transition_time <= ? ORDER BY transition_time LIMIT ?",
            NEVER,
            now,
            limit
        ))
        .await?;
    Ok(rows
        .rows
        .iter()
        .filter_map(|r| r.get("id").and_then(|v| v.as_i64()))
        .collect())
}
