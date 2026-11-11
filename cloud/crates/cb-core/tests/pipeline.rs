//! The job pipeline against SQLite, with the scheduler's writes done by hand.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use async_trait::async_trait;
use cb_core::model::{JobResult, Workunit};
use cb_core::pipeline::{due_workunits, transition, Ctx};
use cb_core::work::{
    acknowledge_assimilation, cancel_workunits, create_work, ready_to_assimilate, CancelTarget,
    JobSpec, Submission,
};
use cb_core::{Config, CoreError, CoreResult, FileKind, FileStat, Files, Webhook, NEVER};
use cb_crypto::Signer;
use cb_db::{query, query_one, stmt, Db};
use cb_proto::consts::*;
use cb_sqlite::SqliteDb;
use futures_lite::future::block_on;

const INPUT_TEMPLATE: &str = "<input_template>\n<workunit>\n<rsc_fpops_est>1e12</rsc_fpops_est>\n<delay_bound>3600</delay_bound>\n</workunit>\n</input_template>\n";
const RESULT_TEMPLATE: &str = "<output_template>\n<file_info>\n    <name><OUTFILE_0/></name>\n    <generated_locally/>\n    <upload_when_present/>\n    <max_nbytes>5000000</max_nbytes>\n    <url><UPLOAD_URL/></url>\n</file_info>\n<result>\n    <file_ref>\n        <file_name><OUTFILE_0/></file_name>\n        <open_name>out</open_name>\n    </file_ref>\n</result>\n</output_template>\n";

#[derive(Default)]
struct MemFiles {
    objects: RefCell<HashMap<(FileKind, String), FileStat>>,
    fail: Cell<bool>,
}

#[async_trait(?Send)]
impl Files for MemFiles {
    async fn stat(&self, kind: FileKind, name: &str) -> CoreResult<Option<FileStat>> {
        if self.fail.get() {
            return Err(CoreError::Files("down".into()));
        }
        Ok(self
            .objects
            .borrow()
            .get(&(kind, name.to_string()))
            .cloned())
    }
    async fn delete(&self, kind: FileKind, names: &[String]) -> CoreResult<()> {
        for n in names {
            self.objects.borrow_mut().remove(&(kind, n.clone()));
        }
        Ok(())
    }
}

struct NoWebhook;

#[async_trait(?Send)]
impl Webhook for NoWebhook {
    async fn post_json(
        &self,
        _url: &str,
        _body: &serde_json::Value,
    ) -> CoreResult<serde_json::Value> {
        Err(CoreError::Invalid("no webhook".into()))
    }
}

struct World {
    db: SqliteDb,
    files: MemFiles,
    config: Config,
    signer: Signer,
    seq: Cell<u32>,
}

impl World {
    fn new(validator: &str) -> World {
        let db = SqliteDb::with_schema().unwrap();
        let (private, _public) = cb_crypto::generate_key_pair().unwrap();
        let mut config = Config::parse("<config><master_url>https://p.example/</master_url><delete_delay_hours>1</delete_delay_hours></config>").unwrap();
        config.daily_result_quota = 100;
        block_on(db.batch(vec![
            stmt!("INSERT INTO app (id, create_time, name, user_friendly_name, validator, target_nresults) VALUES (1, 0, 'a', 'A', ?, 2)", validator),
            stmt!("INSERT INTO user (id, create_time, email_addr, name, authenticator) VALUES (1, 0, 'u@example.com', 'u', 'auth1')"),
            stmt!("INSERT INTO host (id, create_time, userid) VALUES (1, 0, 1), (2, 0, 1), (3, 0, 1)"),
            stmt!("INSERT INTO host_app_version (host_id, app_version_id, max_jobs_per_day) VALUES (1, 7, 10), (2, 7, 10), (3, 7, 10)"),
        ]))
        .unwrap();
        World {
            db,
            files: MemFiles::default(),
            config,
            signer: Signer::new(&private).unwrap(),
            seq: Cell::new(0),
        }
    }

    fn rand(&self) -> u32 {
        self.seq.set(self.seq.get() + 1);
        self.seq.get()
    }

    fn submit(&self, name: &str, now: i64, quorum: i32, target: i32) -> i64 {
        let sub = Submission {
            app: "a".into(),
            input_template: INPUT_TEMPLATE.into(),
            result_template: RESULT_TEMPLATE.into(),
            jobs: vec![JobSpec {
                name: name.into(),
                min_quorum: Some(quorum),
                target_nresults: Some(target),
                ..Default::default()
            }],
        };
        let random = || self.rand();
        let out = block_on(create_work(
            &self.db,
            &self.config,
            Some(&self.signer),
            now,
            &random,
            &sub,
        ))
        .unwrap();
        assert_eq!(out[0].error, None);
        out[0].workunit_id
    }

    fn transition(&self, wu: i64, now: i64) -> cb_core::pipeline::TransitionReport {
        let random = || self.rand();
        let ctx = Ctx {
            db: &self.db,
            files: &self.files,
            webhook: &NoWebhook,
            config: &self.config,
            signer: Some(&self.signer),
            now,
            random: &random,
        };
        block_on(transition(&ctx, wu)).unwrap()
    }

    fn wu(&self, id: i64) -> Workunit {
        block_on(query_one(
            &self.db,
            stmt!("SELECT * FROM workunit WHERE id = ?", id),
        ))
        .unwrap()
        .unwrap()
    }

    fn results(&self, wu: i64) -> Vec<JobResult> {
        block_on(query(
            &self.db,
            stmt!("SELECT * FROM result WHERE workunitid = ? ORDER BY id", wu),
        ))
        .unwrap()
    }

    /// What the scheduler does when it sends a result.
    fn dispatch(&self, r: &JobResult, host: i64, now: i64) {
        block_on(self.db.batch(vec![
            stmt!(
                "UPDATE result SET server_state = 4, hostid = ?, userid = 1, sent_time = ?, report_deadline = ?, app_version_id = 7, flops_estimate = 2e9 WHERE id = ? AND server_state = 2",
                host,
                now,
                now + 3600,
                r.id
            ),
            stmt!("UPDATE workunit SET transition_time = min(transition_time, ?), rev = rev + 1 WHERE id = ?", now + 3600, r.workunitid),
        ]))
        .unwrap();
    }

    /// What the scheduler does when a host reports a result; `content`
    /// stands for the uploaded output.
    fn report(&self, r: &JobResult, now: i64, content: Option<&str>, exit_status: i32) {
        let outcome = if content.is_some() {
            RESULT_OUTCOME_SUCCESS
        } else {
            RESULT_OUTCOME_CLIENT_ERROR
        };
        if let Some(c) = content {
            let name = cb_core::template::output_files(&r.xml_doc_in)[0]
                .name
                .clone();
            self.files.objects.borrow_mut().insert(
                (FileKind::Upload, name),
                FileStat {
                    nbytes: c.len() as u64,
                    md5: cb_crypto::md5_hex(c.as_bytes()),
                },
            );
        }
        block_on(self.db.batch(vec![
            stmt!(
                "UPDATE result SET server_state = 5, outcome = ?, received_time = ?, elapsed_time = 100, exit_status = ? WHERE id = ?",
                outcome,
                now,
                exit_status,
                r.id
            ),
            stmt!("UPDATE workunit SET transition_time = ?, rev = rev + 1 WHERE id = ?", now, r.workunitid),
        ]))
        .unwrap();
    }

    fn scalar(&self, sql: &str) -> f64 {
        let rows = block_on(self.db.run(cb_db::Stmt::new(sql))).unwrap();
        rows.rows[0]
            .values()
            .next()
            .and_then(|v| v.as_f64())
            .unwrap()
    }
}

#[test]
fn quorum_one_job_runs_to_deletion() {
    let w = World::new("trivial");
    let before = w.db.rows_written();
    let wu = w.submit("j1", 1000, 1, 1);
    let rs = w.results(wu);
    assert_eq!(rs.len(), 1);
    assert_eq!(rs[0].name, "j1_0");
    assert_eq!(rs[0].server_state, RESULT_SERVER_STATE_UNSENT);
    assert!(rs[0].xml_doc_in.contains("<xml_signature>"));
    assert!(rs[0]
        .xml_doc_in
        .contains("<url>https://p.example/upload/file_upload_handler</url>"));
    assert!(block_on(due_workunits(&w.db, 1000, 10)).unwrap().is_empty());

    w.dispatch(&rs[0], 1, 1100);
    assert_eq!(w.wu(wu).transition_time, 4700);
    w.report(&rs[0], 1200, Some("out"), 0);
    assert_eq!(block_on(due_workunits(&w.db, 1200, 10)).unwrap(), vec![wu]);

    let rep = w.transition(wu, 1200);
    assert_eq!(rep.attempts, 1);
    assert_eq!(rep.results_valid, 1);
    assert!(rep.assimilate_ready);
    let wurow = w.wu(wu);
    assert_eq!(wurow.canonical_resultid, rs[0].id);
    assert_eq!(wurow.assimilate_state, ASSIMILATE_READY);
    assert_eq!(wurow.transition_time, NEVER);
    assert_eq!(wurow.need_validate, 0);
    let r = &w.results(wu)[0];
    assert_eq!(r.validate_state, VALIDATE_STATE_VALID);
    // 2e9 FLOPS for 100 s, in cobblestones
    let credit = 2e9 * 100.0 * 200.0 / 86400e9;
    assert!((r.granted_credit - credit).abs() < 1e-9);
    assert!((w.scalar("SELECT total_credit FROM host WHERE id = 1") - credit).abs() < 1e-9);
    assert!((w.scalar("SELECT total_credit FROM user WHERE id = 1") - credit).abs() < 1e-9);
    assert_eq!(
        w.scalar("SELECT avg_turnaround FROM host WHERE id = 1"),
        100.0
    );
    assert_eq!(
        w.scalar("SELECT max_jobs_per_day FROM host_app_version WHERE host_id = 1"),
        11.0
    );
    // The app is replicated (target 2) and this job is not: no streak credit.
    assert_eq!(
        w.scalar("SELECT consecutive_valid FROM host_app_version WHERE host_id = 1"),
        0.0
    );

    let ready = block_on(ready_to_assimilate(&w.db, &w.files, "a", 0, 10)).unwrap();
    assert_eq!(ready.len(), 1);
    let canonical = ready[0].canonical_result.as_ref().unwrap();
    assert_eq!(canonical.files.len(), 1);
    assert_eq!(canonical.files[0].nbytes, 3);
    assert_eq!(
        block_on(acknowledge_assimilation(&w.db, &[wu], 1300)).unwrap(),
        vec![wu]
    );
    assert!(block_on(acknowledge_assimilation(&w.db, &[wu], 1300))
        .unwrap()
        .is_empty());

    // Too early to delete: comes back when the delay has passed.
    let rep = w.transition(wu, 1300);
    assert_eq!(rep.files_deleted, 0);
    assert_eq!(rep.transition_time, 1200 + 3600);
    assert_eq!(w.files.objects.borrow().len(), 1);
    // The first transition after the delay only marks the files.
    let rep = w.transition(wu, 4800);
    assert_eq!(rep.files_deleted, 0);
    assert_eq!(rep.transition_time, 4800);
    assert_eq!(w.files.objects.borrow().len(), 1);
    assert_eq!(w.wu(wu).file_delete_state, FILE_DELETE_READY);
    assert_eq!(w.results(wu)[0].file_delete_state, FILE_DELETE_READY);
    let rep = w.transition(wu, 4800);
    assert_eq!(rep.files_deleted, 1);
    assert!(w.files.objects.borrow().is_empty());
    assert_eq!(w.wu(wu).file_delete_state, FILE_DELETE_DONE);
    assert_eq!(w.results(wu)[0].file_delete_state, FILE_DELETE_DONE);
    assert_eq!(w.wu(wu).transition_time, NEVER);

    // The design's write budget for a quorum-1 job, as SQLite counts changes
    // (index writes come on top on D1 and are measured there).
    let written = w.db.rows_written() - before;
    assert!(written <= 18, "{written} rows written");
}

#[test]
fn quorum_two_disagreement_gets_a_third_result() {
    let w = World::new("bitwise");
    let wu = w.submit("j2", 1000, 2, 2);
    let rs = w.results(wu);
    assert_eq!(rs.len(), 2);
    w.dispatch(&rs[0], 1, 1100);
    w.dispatch(&rs[1], 2, 1100);
    w.report(&rs[0], 1200, Some("right"), 0);
    let rep = w.transition(wu, 1200);
    assert_eq!(rep.results_valid, 0);
    assert_eq!(w.wu(wu).need_validate, 0);

    w.report(&rs[1], 1300, Some("wrong"), 0);
    let rep = w.transition(wu, 1300);
    assert_eq!(rep.canonical_resultid, 0);
    assert_eq!(rep.results_created, 1);
    let rs = w.results(wu);
    assert_eq!(rs.len(), 3);
    assert_eq!(rs[2].name, "j2_2");
    assert_eq!(rs[0].validate_state, VALIDATE_STATE_INCONCLUSIVE);
    assert_eq!(rs[1].validate_state, VALIDATE_STATE_INCONCLUSIVE);
    assert_eq!(w.wu(wu).target_nresults, 3);

    w.dispatch(&rs[2], 3, 1400);
    w.report(&rs[2], 1500, Some("right"), 0);
    let rep = w.transition(wu, 1500);
    assert_eq!(rep.canonical_resultid, rs[0].id);
    assert_eq!((rep.results_valid, rep.results_invalid), (2, 1));
    let rs = w.results(wu);
    assert_eq!(rs[0].validate_state, VALIDATE_STATE_VALID);
    assert_eq!(rs[1].validate_state, VALIDATE_STATE_INVALID);
    assert_eq!(rs[2].validate_state, VALIDATE_STATE_VALID);
    assert_eq!(rs[1].granted_credit, 0.0);
    assert!(rs[0].granted_credit > 0.0);
    assert_eq!(w.scalar("SELECT total_credit FROM host WHERE id = 2"), 0.0);
    assert_eq!(
        w.scalar("SELECT consecutive_valid FROM host_app_version WHERE host_id = 3"),
        1.0
    );
}

#[test]
fn late_result_is_checked_against_the_canonical_one() {
    let w = World::new("bitwise");
    let wu = w.submit("j3", 1000, 1, 2);
    let rs = w.results(wu);
    w.dispatch(&rs[0], 1, 1100);
    w.dispatch(&rs[1], 2, 1100);
    w.report(&rs[0], 1200, Some("right"), 0);
    let rep = w.transition(wu, 1200);
    assert_eq!(rep.canonical_resultid, rs[0].id);
    w.report(&rs[1], 1300, Some("right"), 0);
    let rep = w.transition(wu, 1300);
    assert_eq!(rep.results_valid, 1);
    let rs = w.results(wu);
    assert_eq!(rs[1].validate_state, VALIDATE_STATE_VALID);
    assert_eq!(rs[1].granted_credit, rs[0].granted_credit);
    assert_eq!(w.wu(wu).transition_time, NEVER);
}

#[test]
fn timeout_creates_a_replacement_and_penalises_the_host() {
    let w = World::new("trivial");
    let wu = w.submit("j4", 1000, 1, 1);
    let rs = w.results(wu);
    w.dispatch(&rs[0], 1, 1100);
    // Not due yet: nothing happens.
    let rep = w.transition(wu, 2000);
    assert_eq!((rep.results_timed_out, rep.results_created), (0, 0));
    assert_eq!(rep.transition_time, 4700);
    let rep = w.transition(wu, 4701);
    assert_eq!((rep.results_timed_out, rep.results_created), (1, 1));
    let rs = w.results(wu);
    assert_eq!(rs[0].outcome, RESULT_OUTCOME_NO_REPLY);
    assert_eq!(rs[1].name, "j4_1");
    assert_eq!(rs[1].server_state, RESULT_SERVER_STATE_UNSENT);
    assert_eq!(
        w.scalar("SELECT max_jobs_per_day FROM host_app_version WHERE host_id = 1"),
        9.0
    );
    assert_eq!(
        w.scalar("SELECT turnaround_n FROM host_app_version WHERE host_id = 1"),
        1.0
    );
}

#[test]
fn too_many_errors_fail_the_workunit() {
    let w = World::new("trivial");
    let wu = w.submit("j5", 1000, 1, 1);
    let mut now = 1100;
    loop {
        let rs = w.results(wu);
        let Some(r) = rs
            .iter()
            .find(|r| r.server_state == RESULT_SERVER_STATE_UNSENT)
        else {
            break;
        };
        w.dispatch(r, 1, now);
        w.report(r, now + 10, None, 1);
        w.transition(wu, now + 10);
        now += 100;
        assert!(now < 3000);
    }
    let wurow = w.wu(wu);
    assert_eq!(wurow.error_mask, WU_ERROR_TOO_MANY_ERROR_RESULTS);
    assert_eq!(wurow.assimilate_state, ASSIMILATE_READY);
    assert_eq!(w.results(wu).len(), 4);
    let ready = block_on(ready_to_assimilate(&w.db, &w.files, "a", 0, 10)).unwrap();
    assert_eq!(ready[0].error_mask, WU_ERROR_TOO_MANY_ERROR_RESULTS);
    assert!(ready[0].canonical_result.is_none());
}

#[test]
fn storage_failure_delays_validation() {
    let w = World::new("bitwise");
    let wu = w.submit("j6", 1000, 1, 1);
    let rs = w.results(wu);
    w.dispatch(&rs[0], 1, 1100);
    w.report(&rs[0], 1200, Some("x"), 0);
    w.files.fail.set(true);
    let rep = w.transition(wu, 1200);
    assert_eq!(rep.canonical_resultid, 0);
    assert_eq!(rep.transition_time, 1800);
    assert_eq!(w.results(wu).len(), 1);
    w.files.fail.set(false);
    // need_validate was cleared; the retry must still validate.
    let rep = w.transition(wu, 1800);
    assert_eq!(rep.canonical_resultid, rs[0].id);
}

#[test]
fn missing_output_is_a_validate_error() {
    let w = World::new("bitwise");
    let wu = w.submit("j7", 1000, 1, 1);
    let rs = w.results(wu);
    w.dispatch(&rs[0], 1, 1100);
    w.report(&rs[0], 1200, Some("x"), 0);
    w.files.objects.borrow_mut().clear();
    let rep = w.transition(wu, 1200);
    assert_eq!(rep.results_created, 1);
    let rs = w.results(wu);
    assert_eq!(rs[0].outcome, RESULT_OUTCOME_VALIDATE_ERROR);
    assert_eq!(rs[0].validate_state, VALIDATE_STATE_INVALID);
}

#[test]
fn cancel_withdraws_unsent_results() {
    let w = World::new("trivial");
    let wu = w.submit("j8", 1000, 1, 1);
    assert_eq!(
        block_on(cancel_workunits(&w.db, &CancelTarget::Ids(vec![wu]), 1100)).unwrap(),
        vec![wu]
    );
    w.transition(wu, 1100);
    let wurow = w.wu(wu);
    assert_eq!(wurow.error_mask, WU_ERROR_CANCELLED);
    assert_eq!(wurow.assimilate_state, ASSIMILATE_READY);
    assert_eq!(w.results(wu)[0].outcome, RESULT_OUTCOME_DIDNT_NEED);
}

#[test]
fn duplicate_and_invalid_jobs_are_reported_per_job() {
    let w = World::new("trivial");
    w.submit("j9", 1000, 1, 1);
    let sub = Submission {
        app: "a".into(),
        input_template: INPUT_TEMPLATE.into(),
        result_template: RESULT_TEMPLATE.into(),
        jobs: vec![
            JobSpec {
                name: "j9".into(),
                min_quorum: Some(1),
                target_nresults: Some(1),
                ..Default::default()
            },
            JobSpec {
                name: "bad name".into(),
                ..Default::default()
            },
            JobSpec {
                name: "j10".into(),
                min_quorum: Some(3),
                target_nresults: Some(2),
                ..Default::default()
            },
            JobSpec {
                name: "j11".into(),
                ..Default::default()
            },
        ],
    };
    let random = || w.rand();
    let out = block_on(create_work(
        &w.db,
        &w.config,
        Some(&w.signer),
        1000,
        &random,
        &sub,
    ))
    .unwrap();
    assert!(out[0].error.is_some() && out[1].error.is_some() && out[2].error.is_some());
    assert!(out[3].error.is_none() && out[3].workunit_id > 0);
    assert_eq!(w.results(out[3].workunit_id).len(), 2);
}
