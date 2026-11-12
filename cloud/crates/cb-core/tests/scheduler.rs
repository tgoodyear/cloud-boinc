//! The scheduler RPC against SQLite.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

use async_trait::async_trait;
use cb_core::model::{Host, HostAppVersion, JobResult, Workunit};
use cb_core::plan_class::PlanClassSpecs;
use cb_core::scheduler::{handle_request, Catalog, SchedCtx, SchedOutcome};
use cb_core::work::{create_work, Assign, AssignTarget, JobSpec, Submission};
use cb_core::Config;
use cb_crypto::Signer;
use cb_db::{query, query_one, stmt, Db, DbResult, Rows, Stmt};
use cb_proto::consts::*;
use cb_sqlite::SqliteDb;
use futures_lite::future::block_on;

const LINUX: &str = "x86_64-pc-linux-gnu";
const INPUT_TEMPLATE: &str = "<input_template>\n<workunit>\n<rsc_fpops_est>1e12</rsc_fpops_est>\n<rsc_memory_bound>1e8</rsc_memory_bound>\n<rsc_disk_bound>1e8</rsc_disk_bound>\n<delay_bound>3600</delay_bound>\n</workunit>\n</input_template>\n";
const RESULT_TEMPLATE: &str = "<output_template>\n<file_info>\n    <name><OUTFILE_0/></name>\n    <generated_locally/>\n    <upload_when_present/>\n    <max_nbytes>5000000</max_nbytes>\n    <url><UPLOAD_URL/></url>\n</file_info>\n<result>\n    <file_ref>\n        <file_name><OUTFILE_0/></file_name>\n        <open_name>out</open_name>\n    </file_ref>\n</result>\n</output_template>\n";
const APP_VERSION_XML: &str = "<app_version>\n    <app_name>a</app_name>\n    <version_num>100</version_num>\n    <file_ref>\n        <file_name>a_100</file_name>\n        <main_program/>\n    </file_ref>\n</app_version>\n";

/// One upload key for all tests; generating one takes a while.
fn signer() -> &'static Signer {
    static SIGNER: OnceLock<Signer> = OnceLock::new();
    SIGNER.get_or_init(|| {
        let (private, _public) = cb_crypto::generate_key_pair().unwrap();
        Signer::new(&private).unwrap()
    })
}

/// What a test client sends.
#[derive(Clone)]
struct Req {
    auth: String,
    hostid: i64,
    seqno: i32,
    platform: String,
    ncpus: i32,
    work_secs: f64,
    cpid: String,
    domain: String,
    /// `(name, client state, exit status)`
    reports: Vec<(String, i32, i32)>,
    /// Names of the results the client holds; `None` leaves the list out.
    other: Option<Vec<String>>,
}

impl Req {
    fn new(hostid: i64) -> Req {
        Req {
            auth: "auth1".into(),
            hostid,
            seqno: 1,
            platform: LINUX.into(),
            ncpus: 4,
            work_secs: 1000.0,
            cpid: "c0ffee".into(),
            domain: "box".into(),
            reports: Vec::new(),
            other: None,
        }
    }

    fn xml(&self) -> String {
        let mut s = format!(
            "<scheduler_request>\n    <authenticator>{}</authenticator>\n    <hostid>{}</hostid>\n    <rpc_seqno>{}</rpc_seqno>\n    <core_client_major_version>8</core_client_major_version>\n    <core_client_minor_version>0</core_client_minor_version>\n    <core_client_release>4</core_client_release>\n<client_cap_plan_class>1</client_cap_plan_class>\n    <work_req_seconds>{w}</work_req_seconds>\n    <cpu_req_secs>{w}</cpu_req_secs>\n    <cpu_req_instances>{}</cpu_req_instances>\n    <estimated_delay>0</estimated_delay>\n    <platform_name>{}</platform_name>\n",
            self.auth,
            self.hostid,
            self.seqno,
            if self.work_secs > 0.0 { 1 } else { 0 },
            self.platform,
            w = self.work_secs,
        );
        s.push_str(&format!(
            "<host_info>\n    <domain_name>{}</domain_name>\n    <ip_addr>192.0.2.17</ip_addr>\n    <host_cpid>{}</host_cpid>\n    <p_ncpus>{}</p_ncpus>\n    <p_vendor>GenuineIntel</p_vendor>\n    <p_model>cpu</p_model>\n    <p_fpops>2e9</p_fpops>\n    <m_nbytes>8e9</m_nbytes>\n    <d_total>1e11</d_total>\n    <d_free>5e10</d_free>\n    <os_name>Linux</os_name>\n    <os_version>6.1</os_version>\n</host_info>\n",
            self.domain, self.cpid, self.ncpus
        ));
        for (name, state, exit) in &self.reports {
            s.push_str(&format!(
                "<result>\n    <name>{name}</name>\n    <final_cpu_time>90</final_cpu_time>\n    <final_elapsed_time>100</final_elapsed_time>\n    <exit_status>{exit}</exit_status>\n    <state>{state}</state>\n    <app_version_num>100</app_version_num>\n</result>\n"
            ));
        }
        if let Some(names) = &self.other {
            s.push_str("<other_results>\n");
            for n in names {
                s.push_str(&format!(
                    "<other_result>\n    <name>{n}</name>\n</other_result>\n"
                ));
            }
            s.push_str("</other_results>\n");
        }
        s.push_str("</scheduler_request>\n");
        s
    }
}

struct World {
    db: SqliteDb,
    config: Config,
    plan_classes: PlanClassSpecs,
    seq: Cell<u32>,
    /// The output template jobs are submitted with.
    result_template: RefCell<String>,
}

impl World {
    fn new() -> World {
        let db = SqliteDb::with_schema().unwrap();
        let mut config =
            Config::parse("<config><master_url>https://p.example/</master_url></config>").unwrap();
        config.long_name = "Test".into();
        config.daily_result_quota = 100;
        config.version_select_random_factor = 0.0;
        block_on(db.batch(vec![
            stmt!("INSERT INTO platform (id, create_time, name, user_friendly_name) VALUES (1, 0, ?, 'Linux')", LINUX),
            stmt!("INSERT INTO app (id, create_time, name, user_friendly_name, validator) VALUES (1, 0, 'a', 'A', 'trivial')"),
            stmt!("INSERT INTO app_version (id, create_time, appid, version_num, platformid, xml_doc) VALUES (7, 0, 1, 100, 1, ?)", APP_VERSION_XML),
            stmt!("INSERT INTO user (id, create_time, email_addr, name, authenticator, cross_project_id) VALUES (1, 0, 'u@example.com', 'u', 'auth1', 'cpid1'), (2, 0, 'v@example.com', 'v', 'auth2', 'cpid2')"),
        ]))
        .unwrap();
        World {
            db,
            config,
            plan_classes: PlanClassSpecs::default(),
            seq: Cell::new(0),
            result_template: RefCell::new(RESULT_TEMPLATE.into()),
        }
    }

    fn rand(&self) -> u32 {
        self.seq.set(self.seq.get().wrapping_add(2_654_435_761));
        self.seq.get()
    }

    fn submit(&self, name: &str, now: i64, quorum: i32, target: i32) -> i64 {
        self.submit_assigned(name, now, quorum, target, None)
    }

    fn submit_assigned(
        &self,
        name: &str,
        now: i64,
        quorum: i32,
        target: i32,
        assign: Option<Assign>,
    ) -> i64 {
        let sub = Submission {
            app: "a".into(),
            input_template: INPUT_TEMPLATE.into(),
            result_template: self.result_template.borrow().clone(),
            jobs: vec![JobSpec {
                name: name.into(),
                min_quorum: Some(quorum),
                target_nresults: Some(target),
                assign,
                ..Default::default()
            }],
        };
        let random = || self.rand();
        let out = block_on(create_work(
            &self.db,
            &self.config,
            Some(signer()),
            now,
            &random,
            &sub,
        ))
        .unwrap();
        assert_eq!(out[0].error, None);
        out[0].workunit_id
    }

    fn rpc(&self, req: &Req, now: i64) -> SchedOutcome {
        let catalog = block_on(Catalog::load(&self.db)).unwrap();
        let random = || self.rand();
        let ctx = SchedCtx {
            db: &self.db,
            config: &self.config,
            plan_classes: &self.plan_classes,
            catalog: &catalog,
            code_sign_key: "",
            now,
            random: &random,
            client_ip: "198.51.100.9",
        };
        block_on(handle_request(&ctx, &req.xml())).unwrap()
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

    /// Rows of `upload_epoch`: `(name, result ID, epoch)`, by name.
    fn epochs(&self) -> Vec<(String, i64, i64)> {
        let rows = block_on(self.db.run(stmt!(
            "SELECT name, result_id, epoch FROM upload_epoch ORDER BY name"
        )))
        .unwrap()
        .rows;
        rows.iter()
            .map(|r| {
                (
                    r["name"].as_str().unwrap().to_string(),
                    r["result_id"].as_i64().unwrap(),
                    r["epoch"].as_i64().unwrap(),
                )
            })
            .collect()
    }

    fn host(&self, id: i64) -> Host {
        block_on(query_one(
            &self.db,
            stmt!("SELECT * FROM host WHERE id = ?", id),
        ))
        .unwrap()
        .unwrap()
    }

    fn hav(&self, host: i64) -> HostAppVersion {
        block_on(query_one(
            &self.db,
            stmt!(
                "SELECT * FROM host_app_version WHERE host_id = ? AND app_version_id = 7",
                host
            ),
        ))
        .unwrap()
        .unwrap()
    }
}

fn has_message(out: &SchedOutcome, text: &str) -> bool {
    out.reply.messages.iter().any(|m| m.message.contains(text))
}

#[test]
fn new_host_is_created_and_sent_work() {
    let w = World::new();
    let wu = w.submit("j1", 1000, 1, 1);
    let rev = w.wu(wu).rev;
    let before = w.db.rows_written();
    let out = w.rpc(&Req::new(0), 2000);
    let written = w.db.rows_written() - before;

    assert_eq!(out.n_sent, 1);
    assert!(out.host_id > 0);
    assert_eq!(out.reply.hostid, out.host_id as u64);
    assert_eq!(out.reply.user.id, 1);
    assert!(out.transition.is_empty());
    // host insert, host update, counter row, claim, counter, workunit
    assert!(written <= 6, "{written} rows written");

    let r = &w.results(wu)[0];
    assert_eq!(r.server_state, RESULT_SERVER_STATE_IN_PROGRESS);
    assert_eq!(r.hostid, out.host_id);
    assert_eq!(r.userid, 1);
    assert_eq!(r.sent_time, 2000);
    assert_eq!(r.report_deadline, 5600);
    assert_eq!(r.app_version_id, 7);
    assert_eq!(r.flops_estimate, 2e9);
    let wurow = w.wu(wu);
    assert_eq!(wurow.transition_time, 5600);
    assert_eq!(wurow.rev, rev + 1);
    let h = w.host(out.host_id);
    assert_eq!(h.p_ncpus, 4);
    assert_eq!(h.rpc_time, 2000);
    assert_eq!(h.external_ip_addr, "198.51.100.9");
    assert_eq!(h.serialnum, "[BOINC|8.0.4]");
    assert_eq!(w.hav(out.host_id).n_jobs_today, 1);

    let xml = out.reply.write().unwrap();
    assert!(xml.contains("<hostid>"));
    assert!(xml.contains("<name>j1_0</name>"));
    assert!(xml.contains("<wu_name>j1</wu_name>"));
    assert!(xml.contains("<report_deadline>5600</report_deadline>"));
    assert!(xml.contains("<app_name>a</app_name>"));
    assert!(xml.contains(&format!("<platform>{LINUX}</platform>")));
    assert!(xml.contains("<file_name>a_100</file_name>"));
    assert!(xml.contains("<project_name>Test</project_name>"));

    // The same host asks again: nothing left, and it is the same host.
    let mut again = Req::new(out.host_id);
    again.seqno = 2;
    let out2 = w.rpc(&again, 2100);
    assert_eq!(out2.host_id, out.host_id);
    assert_eq!(out2.reply.hostid, 0);
    assert_eq!(out2.n_sent, 0);
    assert!(has_message(&out2, "Project has no tasks available"));
}

#[test]
fn reported_results_are_recorded() {
    let w = World::new();
    let wu1 = w.submit("j1", 1000, 1, 1);
    let wu2 = w.submit("j2", 1000, 1, 1);
    let out = w.rpc(&Req::new(0), 2000);
    assert_eq!(out.n_sent, 2);
    let host = out.host_id;

    let mut req = Req::new(host);
    req.seqno = 2;
    req.work_secs = 0.0;
    req.reports = vec![
        ("j1_0".into(), RESULT_FILES_UPLOADED, 0),
        ("j2_0".into(), RESULT_COMPUTE_ERROR, 195),
        ("nonesuch_0".into(), RESULT_FILES_UPLOADED, 0),
    ];
    let out = w.rpc(&req, 2500);
    assert_eq!(out.n_reported, 2);
    assert_eq!(out.n_sent, 0);
    assert_eq!(out.transition, vec![wu1, wu2]);
    assert_eq!(out.reply.result_acks.len(), 3);
    assert!(!has_message(&out, "No tasks sent"));

    let good = &w.results(wu1)[0];
    assert_eq!(good.server_state, RESULT_SERVER_STATE_OVER);
    assert_eq!(good.outcome, RESULT_OUTCOME_SUCCESS);
    assert_eq!(good.validate_state, VALIDATE_STATE_INIT);
    assert_eq!(good.received_time, 2500);
    assert_eq!(good.elapsed_time, 100.0);
    assert_eq!(good.cpu_time, 90.0);
    let bad = &w.results(wu2)[0];
    assert_eq!(bad.outcome, RESULT_OUTCOME_CLIENT_ERROR);
    assert_eq!(bad.validate_state, VALIDATE_STATE_INVALID);
    assert_eq!(bad.exit_status, 195);
    assert_eq!(w.wu(wu1).transition_time, 2500);
    assert_eq!(w.wu(wu2).transition_time, 2500);
    // One success doubles the limit (capped), one failure takes one off.
    assert_eq!(w.hav(host).max_jobs_per_day, 99);

    // Reporting again changes nothing.
    req.seqno = 3;
    let out = w.rpc(&req, 2600);
    assert_eq!(out.n_reported, 0);
    assert_eq!(w.results(wu1)[0].received_time, 2500);
}

#[test]
fn bad_account_key_is_refused() {
    let w = World::new();
    w.submit("j1", 1000, 1, 1);
    let mut req = Req::new(0);
    req.auth = "wrong".into();
    let out = w.rpc(&req, 2000);
    assert!(has_message(&out, "Invalid or missing account key"));
    assert!(out.reply.nucleus_only);
    assert_eq!(out.n_sent, 0);
    assert_eq!(out.reply.request_delay, 3600.0);
}

#[test]
fn unknown_platform_is_refused() {
    let w = World::new();
    w.submit("j1", 1000, 1, 1);
    let mut req = Req::new(0);
    req.platform = "sparc-sun-solaris".into();
    let out = w.rpc(&req, 2000);
    assert!(has_message(&out, "doesn't support computers of type"));
    assert_eq!(out.n_sent, 0);
}

#[test]
fn garbage_is_answered() {
    let w = World::new();
    let catalog = block_on(Catalog::load(&w.db)).unwrap();
    let random = || w.rand();
    let ctx = SchedCtx {
        db: &w.db,
        config: &w.config,
        plan_classes: &w.plan_classes,
        catalog: &catalog,
        code_sign_key: "",
        now: 1,
        random: &random,
        client_ip: "",
    };
    let out = block_on(handle_request(&ctx, "hello")).unwrap();
    assert!(has_message(&out, "Incomplete request"));
    assert!(out.reply.write().unwrap().contains("<scheduler_reply>"));
}

#[test]
fn one_result_per_user_per_workunit() {
    let mut w = World::new();
    w.config.one_result_per_user_per_wu = true;
    let wu = w.submit("j1", 1000, 2, 2);
    let a = w.rpc(&Req::new(0), 2000);
    assert_eq!(a.n_sent, 1);

    // Another computer of the same user gets nothing from this workunit.
    let mut other = Req::new(0);
    other.cpid = "beef".into();
    other.domain = "box2".into();
    let b = w.rpc(&other, 2001);
    assert_ne!(b.host_id, a.host_id);
    assert_eq!(b.n_sent, 0);

    // Another user does.
    let mut second = Req::new(0);
    second.auth = "auth2".into();
    let c = w.rpc(&second, 2002);
    assert_eq!(c.n_sent, 1);
    let rs = w.results(wu);
    assert_eq!(rs[0].userid + rs[1].userid, 3);
}

#[test]
fn daily_quota_and_in_progress_limit() {
    let mut w = World::new();
    w.config.daily_result_quota = 1;
    for i in 0..8 {
        w.submit(&format!("j{i}"), 1000, 1, 1);
    }
    let mut req = Req::new(0);
    req.ncpus = 2;
    req.work_secs = 1e6;
    let out = w.rpc(&req, 2000);
    // one a day per CPU
    assert_eq!(out.n_sent, 2);
    let host = out.host_id;
    req.hostid = host;
    req.seqno = 2;
    let out = w.rpc(&req, 2100);
    assert_eq!(out.n_sent, 0);
    assert!(has_message(&out, "limit on tasks sent per day"));

    // The next day the count starts again; now the in-progress limit binds.
    // The host's own limit was set from the old quota; lift it too.
    w.config.daily_result_quota = 100;
    w.config.max_wus_in_progress = 2;
    block_on(w.db.run(stmt!("UPDATE host_app_version SET max_jobs_per_day = 100"))).unwrap();
    req.seqno = 3;
    let out = w.rpc(&req, 2000 + 86400);
    // 2 CPUs x 2 = 4 in progress at most, 2 already held
    assert_eq!(out.n_sent, 2);
    req.seqno = 4;
    let out = w.rpc(&req, 2100 + 86400);
    assert_eq!(out.n_sent, 0);
    assert!(
        has_message(&out, "limit on tasks in progress"),
        "{:?} {}",
        out.reply
            .messages
            .iter()
            .map(|m| m.message.clone())
            .collect::<Vec<_>>(),
        out.log
    );
}

#[test]
fn claim_guard_holds_when_the_candidate_is_gone() {
    // A request that read a candidate another request then claimed sends
    // nothing; the guards are in the UPDATE, not in what was read.
    let w = World::new();
    let wu = w.submit("j1", 1000, 1, 1);
    let first = w.rpc(&Req::new(0), 2000);
    assert_eq!(first.n_sent, 1);
    let r = w.results(wu)[0].clone();
    let n = block_on(w.db.run(stmt!(
        "UPDATE result SET server_state = 4, hostid = 99 WHERE id = ? AND server_state = 2 RETURNING id",
        r.id
    )))
    .unwrap();
    assert!(n.rows.is_empty());
    assert_eq!(w.results(wu)[0].hostid, first.host_id);
}

#[test]
fn lost_results_are_resent() {
    let mut w = World::new();
    w.config.resend_lost_results = true;
    let wu = w.submit("j1", 1000, 1, 1);
    let out = w.rpc(&Req::new(0), 2000);
    assert_eq!(out.n_sent, 1);
    let host = out.host_id;

    // The client holds nothing but asks for no work: upstream looks for lost
    // results only when it may send work, so this leaves the result be.
    let mut req = Req::new(host);
    req.seqno = 2;
    req.work_secs = 0.0;
    req.other = Some(Vec::new());
    let out = w.rpc(&req, 2050);
    assert_eq!(out.n_resent, 0);
    assert!(!has_message(&out, "lost task"));
    assert_eq!(
        w.results(wu)[0].server_state,
        RESULT_SERVER_STATE_IN_PROGRESS
    );

    // Asking for work, it is resent.
    req.seqno = 3;
    req.work_secs = 1000.0;
    let out = w.rpc(&req, 2100);
    assert_eq!(out.n_resent, 1);
    assert_eq!(out.n_sent, 0);
    assert!(has_message(&out, "Resent lost task j1_0"));
    let xml = out.reply.write().unwrap();
    assert!(xml.contains("<name>j1_0</name>"));
    assert!(xml.contains("<report_deadline>5600</report_deadline>"));
    assert_eq!(
        w.results(wu)[0].server_state,
        RESULT_SERVER_STATE_IN_PROGRESS
    );
    // The resend is a new sending, as upstream's `resend_lost_work` records
    // it. The deadline is the one it had.
    assert_eq!(w.results(wu)[0].sent_time, 2100);
    assert_eq!(w.results(wu)[0].report_deadline, 5600);
    // The resend moves the file's epoch to 1, so the pieces of the lost
    // attempt are never counted by an upload.
    let r = &w.results(wu)[0];
    let name = cb_core::template::output_files(&r.xml_doc_in)[0]
        .name
        .clone();
    assert_eq!(w.epochs(), vec![(name, r.id, 1)]);

    // The client holds it: nothing to resend.
    req.seqno = 4;
    req.other = Some(vec!["j1_0".into()]);
    let out = w.rpc(&req, 2200);
    assert_eq!(out.n_resent, 0);

    // The workunit was cancelled meanwhile: the lost result is not resent
    // and times out at once.
    block_on(w.db.run(stmt!(
        "UPDATE workunit SET error_mask = ? WHERE id = ?",
        WU_ERROR_CANCELLED,
        wu
    )))
    .unwrap();
    req.seqno = 5;
    req.other = Some(Vec::new());
    let out = w.rpc(&req, 2300);
    assert_eq!(out.n_resent, 0);
    assert!(has_message(&out, "Didn't resend lost task j1_0"));
    assert_eq!(out.transition, vec![wu]);
    assert_eq!(w.results(wu)[0].report_deadline, 2299);
    assert_eq!(w.wu(wu).transition_time, 2300);
}

#[test]
fn unwanted_results_are_aborted() {
    let mut w = World::new();
    w.config.send_result_abort = true;
    let wu1 = w.submit("j1", 1000, 1, 1);
    let wu2 = w.submit("j2", 1000, 1, 1);
    let out = w.rpc(&Req::new(0), 2000);
    assert_eq!(out.n_sent, 2);
    block_on(w.db.batch(vec![
        stmt!(
            "UPDATE workunit SET error_mask = ? WHERE id = ?",
            WU_ERROR_CANCELLED,
            wu1
        ),
        stmt!(
            "UPDATE workunit SET assimilate_state = ? WHERE id = ?",
            ASSIMILATE_DONE,
            wu2
        ),
    ]))
    .unwrap();
    let mut req = Req::new(out.host_id);
    req.seqno = 2;
    req.work_secs = 0.0;
    req.other = Some(vec!["j1_0".into(), "j2_0".into(), "gone_0".into()]);
    let out = w.rpc(&req, 2100);
    assert_eq!(out.reply.result_aborts, vec!["j1_0", "gone_0"]);
    assert_eq!(out.reply.result_abort_if_not_starteds, vec!["j2_0"]);
}

#[test]
fn a_detached_client_gives_its_jobs_back() {
    let w = World::new();
    let wu = w.submit("j1", 1000, 1, 1);
    let out = w.rpc(&Req::new(0), 2000);
    assert_eq!(out.n_sent, 1);

    // The same computer attaches afresh: host ID 0, same CPID, no jobs.
    let mut req = Req::new(0);
    req.work_secs = 0.0;
    let again = w.rpc(&req, 2100);
    assert_eq!(again.host_id, out.host_id);
    assert_eq!(again.reply.hostid, out.host_id as u64);
    assert_eq!(again.transition, vec![wu]);
    let r = &w.results(wu)[0];
    assert_eq!(r.server_state, RESULT_SERVER_STATE_OVER);
    assert_eq!(r.outcome, RESULT_OUTCOME_CLIENT_DETACHED);
    assert_eq!(w.wu(wu).transition_time, 2100);
}

#[test]
fn an_unproven_host_raises_replication() {
    let w = World::new();
    block_on(w.db.run(stmt!("UPDATE app SET target_nresults = 2 WHERE id = 1"))).unwrap();
    let wu = w.submit("j1", 1000, 1, 1);
    let out = w.rpc(&Req::new(0), 2000);
    assert_eq!(out.n_sent, 1);
    assert_eq!(out.transition, vec![wu]);
    let wurow = w.wu(wu);
    assert_eq!(wurow.target_nresults, 2);
    assert_eq!(wurow.min_quorum, 2);
    assert_eq!(wurow.transition_time, 2000);
}

#[test]
fn memory_and_deadline_are_checked() {
    let w = World::new();
    let wu = w.submit("j1", 1000, 1, 1);
    block_on(w.db.run(stmt!(
        "UPDATE workunit SET rsc_memory_bound = 1e12 WHERE id = ?",
        wu
    )))
    .unwrap();
    let out = w.rpc(&Req::new(0), 2000);
    assert_eq!(out.n_sent, 0);
    assert!(has_message(&out, "more memory"));

    // 1e15 FLOPs at 2e9 FLOPS is far beyond the hour allowed.
    block_on(w.db.run(stmt!(
        "UPDATE workunit SET rsc_memory_bound = 1e8, rsc_fpops_est = 1e15 WHERE id = ?",
        wu
    )))
    .unwrap();
    let mut req = Req::new(out.host_id);
    req.seqno = 2;
    let out = w.rpc(&req, 2100);
    assert_eq!(out.n_sent, 0);
    assert!(has_message(&out, "won't finish in time"));
}

fn to_host(id: i64) -> Option<Assign> {
    Some(Assign {
        target: AssignTarget::Host,
        id,
    })
}

fn to_user(id: i64) -> Option<Assign> {
    Some(Assign {
        target: AssignTarget::User,
        id,
    })
}

/// A request from a computer of `auth`'s user that asks for no work, so
/// that the host exists before jobs are assigned to it.
fn computer(w: &World, auth: &str, cpid: &str, now: i64) -> Req {
    let mut req = Req::new(0);
    req.auth = auth.into();
    req.cpid = cpid.into();
    req.domain = cpid.into();
    req.work_secs = 0.0;
    let out = w.rpc(&req, now);
    assert_eq!(out.n_sent, 0);
    req.hostid = out.host_id;
    req.seqno = 2;
    req.work_secs = 1e6;
    req
}

#[test]
fn a_host_assigned_job_goes_only_to_its_host() {
    let mut w = World::new();
    w.config.enable_assignment = true;
    let mut mine = computer(&w, "auth1", "aa", 1000);
    let mut other = computer(&w, "auth2", "bb", 1000);
    let wu = w.submit_assigned("t1", 1100, 1, 1, to_host(mine.hostid));
    assert_eq!(w.results(wu)[0].server_state, RESULT_SERVER_STATE_INACTIVE);

    let out = w.rpc(&other, 2000);
    assert_eq!(out.n_sent, 0);
    assert!(has_message(&out, "No tasks sent"));
    other.seqno += 1;

    let out = w.rpc(&mine, 2000);
    assert_eq!(out.n_sent, 1);
    assert!(out.transition.is_empty());
    let r = &w.results(wu)[0];
    assert_eq!(r.server_state, RESULT_SERVER_STATE_IN_PROGRESS);
    assert_eq!(r.hostid, mine.hostid);
    assert_eq!(r.report_deadline, 2000 + 3600);
    assert_eq!(w.wu(wu).transition_time, 2000 + 3600);
    assert_eq!(w.hav(mine.hostid).n_jobs_today, 1);
    assert!(out.reply.write().unwrap().contains("<name>t1_0</name>"));

    mine.seqno += 1;
    assert_eq!(w.rpc(&mine, 2100).n_sent, 0);
    assert_eq!(w.rpc(&other, 2100).n_sent, 0);
}

#[test]
fn a_user_assigned_job_goes_to_each_of_the_users_hosts_once() {
    let mut w = World::new();
    w.config.enable_assignment = true;
    let mut first = computer(&w, "auth1", "aa", 1000);
    let second = computer(&w, "auth1", "cc", 1000);
    let stranger = computer(&w, "auth2", "bb", 1000);
    let wu = w.submit_assigned("t1", 1100, 2, 2, to_user(1));
    assert_eq!(w.results(wu).len(), 2);

    // Two instances wanted, one per host however much is asked for.
    let out = w.rpc(&first, 2000);
    assert_eq!(out.n_sent, 1);
    first.seqno += 1;
    assert_eq!(w.rpc(&first, 2100).n_sent, 0);
    assert_eq!(w.rpc(&stranger, 2100).n_sent, 0);
    assert_eq!(w.rpc(&second, 2200).n_sent, 1);

    let rs = w.results(wu);
    assert!(rs
        .iter()
        .all(|r| r.server_state == RESULT_SERVER_STATE_IN_PROGRESS));
    let mut hosts: Vec<i64> = rs.iter().map(|r| r.hostid).collect();
    hosts.sort_unstable();
    assert_eq!(hosts, vec![first.hostid, second.hostid]);
}

#[test]
fn assigned_work_is_sent_alone() {
    let mut w = World::new();
    w.config.enable_assignment = true;
    let mut req = computer(&w, "auth1", "aa", 1000);
    let open = w.submit("j1", 1100, 1, 1);
    let targeted = w.submit_assigned("t1", 1100, 1, 1, to_user(1));

    let out = w.rpc(&req, 2000);
    assert_eq!(out.n_sent, 1);
    assert_eq!(
        w.results(targeted)[0].server_state,
        RESULT_SERVER_STATE_IN_PROGRESS
    );
    assert_eq!(w.results(open)[0].server_state, RESULT_SERVER_STATE_UNSENT);

    // With nothing assigned left, regular work flows again.
    req.seqno += 1;
    let out = w.rpc(&req, 2100);
    assert_eq!(out.n_sent, 1);
    assert_eq!(
        w.results(open)[0].server_state,
        RESULT_SERVER_STATE_IN_PROGRESS
    );
}

#[test]
fn assignment_disabled_sends_no_assigned_jobs() {
    let mut w = World::new();
    w.config.enable_assignment = true;
    let req = computer(&w, "auth1", "aa", 1000);
    let wu = w.submit_assigned("t1", 1100, 1, 1, to_host(req.hostid));
    w.config.enable_assignment = false;
    let out = w.rpc(&req, 2000);
    assert_eq!(out.n_sent, 0);
    assert_eq!(w.results(wu)[0].server_state, RESULT_SERVER_STATE_INACTIVE);
}

#[test]
fn assigned_jobs_the_host_cannot_run_do_not_hold_back_later_ones() {
    let mut w = World::new();
    w.config.enable_assignment = true;
    let req = computer(&w, "auth1", "aa", 1000);
    block_on(w.db.run(stmt!("UPDATE app SET homogeneous_app_version = 1"))).unwrap();
    // More jobs than one request looks at, committed to a version the host
    // has no use of (the app uses homogeneous app version), ahead of one it
    // can run.
    for i in 0..25 {
        let wu = w.submit_assigned(&format!("p{i}"), 1100, 1, 1, to_host(req.hostid));
        block_on(w.db.run(stmt!(
            "UPDATE workunit SET app_version_id = 999999 WHERE id = ?",
            wu
        )))
        .unwrap();
    }
    let wu = w.submit_assigned("t1", 1100, 1, 1, to_host(req.hostid));
    let out = w.rpc(&req, 2000);
    assert_eq!(out.n_sent, 1);
    assert_eq!(w.results(wu)[0].hostid, req.hostid);
}

/// A database that, before the first write batch of a request, runs another
/// request to completion: two requests planned from the same reads.
struct Interleave<'a> {
    inner: &'a SqliteDb,
    before_write: RefCell<Option<Pin<Box<dyn Future<Output = SchedOutcome> + 'a>>>>,
    other: RefCell<Option<SchedOutcome>>,
}

#[async_trait(?Send)]
impl<'a> Db for Interleave<'a> {
    async fn batch(&self, stmts: Vec<Stmt>) -> DbResult<Vec<Rows>> {
        if stmts
            .iter()
            .any(|s| s.sql.starts_with("UPDATE host SET rpc_seqno"))
        {
            let next = self.before_write.borrow_mut().take();
            if let Some(f) = next {
                let out = f.await;
                *self.other.borrow_mut() = Some(out);
            }
        }
        self.inner.batch(stmts).await
    }
}

#[test]
fn an_assigned_claim_race_has_one_winner() {
    let mut w = World::new();
    w.config.enable_assignment = true;
    let a = computer(&w, "auth1", "aa", 1000);
    let b = computer(&w, "auth1", "cc", 1000);
    let wu = w.submit_assigned("t1", 1100, 1, 1, to_user(1));

    let catalog = block_on(Catalog::load(&w.db)).unwrap();
    let random = || w.rand();
    let ctx_b = SchedCtx {
        db: &w.db,
        config: &w.config,
        plan_classes: &w.plan_classes,
        catalog: &catalog,
        code_sign_key: "",
        now: 2000,
        random: &random,
        client_ip: "198.51.100.9",
    };
    let xml_b = b.xml();
    let db = Interleave {
        inner: &w.db,
        before_write: RefCell::new(Some(Box::pin(async {
            handle_request(&ctx_b, &xml_b).await.unwrap()
        }))),
        other: RefCell::new(None),
    };
    let ctx_a = SchedCtx {
        db: &db,
        config: &w.config,
        plan_classes: &w.plan_classes,
        catalog: &catalog,
        code_sign_key: "",
        now: 2000,
        random: &random,
        client_ip: "198.51.100.9",
    };
    let out_a = block_on(handle_request(&ctx_a, &a.xml())).unwrap();
    let out_b = db.other.borrow_mut().take().unwrap();

    // Both chose the reserved result; the request that wrote first has it.
    assert_eq!(out_b.n_sent, 1);
    assert_eq!(out_a.n_sent, 0);
    assert!(out_a.reply.results.is_empty());
    let rs = w.results(wu);
    assert_eq!(rs.len(), 1);
    assert_eq!(rs[0].hostid, b.hostid);
    assert_eq!(w.hav(b.hostid).n_jobs_today, 1);
}

#[test]
fn a_resend_records_where_each_upload_file_starts() {
    // Output names carry whatever text the template puts around
    // `<OUTFILE_n/>`; the epoch is moved for each uploadable one, as
    // generated, and not for a file that stays on the client.
    let mut w = World::new();
    w.config.resend_lost_results = true;
    *w.result_template.borrow_mut() = "<output_template>\n\
<file_info><name>pre_<OUTFILE_0/>_r1_log</name><generated_locally/><upload_when_present/><max_nbytes>100</max_nbytes><url><UPLOAD_URL/></url></file_info>\n\
<file_info><name><OUTFILE_1/>.zip</name><generated_locally/><upload_when_present/><max_nbytes>100</max_nbytes><url><UPLOAD_URL/></url></file_info>\n\
<file_info><name><OUTFILE_2/></name><generated_locally/></file_info>\n\
<result>\n\
<file_ref><file_name>pre_<OUTFILE_0/>_r1_log</file_name><open_name>a</open_name></file_ref>\n\
<file_ref><file_name><OUTFILE_1/>.zip</file_name><open_name>b</open_name></file_ref>\n\
<file_ref><file_name><OUTFILE_2/></file_name><open_name>c</open_name></file_ref>\n\
</result>\n</output_template>\n"
        .to_string();
    let wu = w.submit("j1", 1000, 1, 1);
    let out = w.rpc(&Req::new(0), 2000);
    assert_eq!(out.n_sent, 1);
    assert!(w.epochs().is_empty());
    let mut req = Req::new(out.host_id);
    req.seqno = 2;
    req.other = Some(Vec::new());
    let out = w.rpc(&req, 2100);
    assert_eq!(out.n_resent, 1);
    let r = w.results(wu)[0].clone();
    let base = format!("{}_r", r.name);
    let epochs = w.epochs();
    assert_eq!(epochs.len(), 2, "{epochs:?}");
    assert!(epochs.iter().all(|e| e.1 == r.id && e.2 == 1));
    assert!(epochs
        .iter()
        .any(|e| e.0.starts_with("pre_") && e.0.contains(&base) && e.0.ends_with("_0_r1_log")));
    assert!(epochs
        .iter()
        .any(|e| e.0.starts_with(&base) && e.0.ends_with("_1.zip")));

    // Sent again later: the epoch moves, no new rows.
    req.seqno = 3;
    let out = w.rpc(&req, 2300);
    assert_eq!(out.n_resent, 1);
    let later = w.epochs();
    assert_eq!(later.len(), 2);
    assert!(later.iter().all(|e| e.2 == 2));
}

/// A database that keeps the text of every statement run through it.
struct Recording<'a> {
    inner: &'a SqliteDb,
    sqls: RefCell<Vec<String>>,
}

#[async_trait(?Send)]
impl<'a> Db for Recording<'a> {
    async fn batch(&self, stmts: Vec<Stmt>) -> DbResult<Vec<Rows>> {
        self.sqls
            .borrow_mut()
            .extend(stmts.iter().map(|s| s.sql.clone()));
        self.inner.batch(stmts).await
    }
}

impl World {
    /// Adds an app that has a version for another platform only, as the
    /// host of these tests cannot run it.
    fn add_app_b(&self) {
        block_on(self.db.batch(vec![
            stmt!("INSERT INTO platform (id, create_time, name, user_friendly_name) VALUES (2, 0, 'windows_x86_64', 'Windows')"),
            stmt!("INSERT INTO app (id, create_time, name, user_friendly_name, validator) VALUES (2, 0, 'b', 'B', 'trivial')"),
            stmt!("INSERT INTO app_version (id, create_time, appid, version_num, platformid, xml_doc) VALUES (8, 0, 2, 100, 2, ?)", APP_VERSION_XML.replace("a_100", "b_100").replace(">a<", ">b<")),
        ]))
        .unwrap();
    }

    fn submit_b(&self, name: &str, now: i64) {
        let sub = Submission {
            app: "b".into(),
            input_template: INPUT_TEMPLATE.into(),
            result_template: RESULT_TEMPLATE.into(),
            jobs: vec![JobSpec {
                name: name.into(),
                ..Default::default()
            }],
        };
        let random = || self.rand();
        let out = block_on(create_work(
            &self.db,
            &self.config,
            Some(signer()),
            now,
            &random,
            &sub,
        ))
        .unwrap();
        assert_eq!(out[0].error, None);
    }

    /// `rpc`, and the statements the request ran.
    fn rpc_recorded(&self, req: &Req, now: i64) -> (SchedOutcome, Vec<String>) {
        let catalog = block_on(Catalog::load(&self.db)).unwrap();
        let random = || self.rand();
        let db = Recording {
            inner: &self.db,
            sqls: RefCell::new(Vec::new()),
        };
        let ctx = SchedCtx {
            db: &db,
            config: &self.config,
            plan_classes: &self.plan_classes,
            catalog: &catalog,
            code_sign_key: "",
            now,
            random: &random,
            client_ip: "198.51.100.9",
        };
        let out = block_on(handle_request(&ctx, &req.xml())).unwrap();
        (out, db.sqls.into_inner())
    }
}

fn probes(sqls: &[String]) -> usize {
    sqls.iter()
        .filter(|s| s.as_str() == cb_core::scheduler::UNSENT_EXISTS)
        .count()
}

#[test]
fn the_unsent_work_probe_uses_the_partial_index_and_reads_one_entry() {
    let w = World::new();
    w.submit("j1", 1000, 1, 1);
    let plan = block_on(w.db.run(Stmt::new(format!(
        "EXPLAIN QUERY PLAN {}",
        cb_core::scheduler::UNSENT_EXISTS
    ))))
    .unwrap()
    .rows;
    let detail: Vec<&str> = plan.iter().map(|r| r["detail"].as_str().unwrap()).collect();
    assert_eq!(detail.len(), 1, "{detail:?}");
    assert!(
        detail[0].contains("INDEX res_unsent"),
        "no use of the partial index: {detail:?}"
    );
    // One row at most, whatever the number of unsent results.
    for n in 2..6 {
        w.submit(&format!("j{n}"), 1000, 1, 1);
    }
    let rows = block_on(w.db.run(Stmt::new(cb_core::scheduler::UNSENT_EXISTS)))
        .unwrap()
        .rows;
    assert_eq!(rows.len(), 1);
}

#[test]
fn a_request_that_got_work_runs_no_probe() {
    let w = World::new();
    w.submit("j1", 1000, 1, 1);
    let (out, sqls) = w.rpc_recorded(&Req::new(0), 2000);
    assert_eq!(out.n_sent, 1);
    assert_eq!(probes(&sqls), 0);
}

#[test]
fn work_for_other_apps_is_work_in_the_cache() {
    // The host has a version of app A. Only app B has unsent tasks, and the
    // host cannot run it. Upstream's cache holds B's tasks, so its reply is
    // "No tasks sent", not "Project has no tasks available".
    let w = World::new();
    w.add_app_b();
    w.submit_b("b1", 1000);
    let (out, sqls) = w.rpc_recorded(&Req::new(0), 2000);
    assert_eq!(out.n_sent, 0);
    assert!(has_message(&out, "No tasks sent"));
    assert!(!has_message(&out, "Project has no tasks available"));
    assert_eq!(probes(&sqls), 1);
}

#[test]
fn an_empty_project_is_told_only_that_it_has_no_tasks() {
    let w = World::new();
    let (out, sqls) = w.rpc_recorded(&Req::new(0), 2000);
    assert_eq!(out.n_sent, 0);
    assert!(has_message(&out, "Project has no tasks available"));
    assert!(!has_message(&out, "No tasks sent"));
    assert_eq!(probes(&sqls), 1);
}

#[test]
fn a_resend_says_the_project_has_no_tasks_only_if_it_has_none() {
    let mut w = World::new();
    w.config.resend_lost_results = true;
    w.add_app_b();
    w.submit("j1", 1000, 1, 1);
    let out = w.rpc(&Req::new(0), 2000);
    assert_eq!(out.n_sent, 1);
    let host = out.host_id;
    let mut req = Req::new(host);
    req.seqno = 2;
    req.other = Some(Vec::new());

    // App B has unsent work: the cache is not empty.
    w.submit_b("b1", 2050);
    let (out, sqls) = w.rpc_recorded(&req, 2100);
    assert_eq!(out.n_resent, 1);
    assert!(has_message(&out, "Resent lost task j1_0"));
    assert!(!has_message(&out, "Project has no tasks available"));
    assert!(!has_message(&out, "No tasks sent"));
    assert_eq!(probes(&sqls), 1);

    // Nothing unsent anywhere: the cache is empty.
    block_on(w.db.run(stmt!(
        "UPDATE result SET server_state = 5 WHERE server_state = 2"
    )))
    .unwrap();
    req.seqno = 3;
    let (out, sqls) = w.rpc_recorded(&req, 2200);
    assert_eq!(out.n_resent, 1);
    assert!(has_message(&out, "Project has no tasks available"));
    assert!(!has_message(&out, "No tasks sent"));
    assert_eq!(probes(&sqls), 1);
}

// ---------------------------------------------------------------------------
// An account that is deleted while one of its requests is under way.

/// A database that starts the deletion of account 1 (the key changes and the
/// account is blocked, as `start_deletion` does in one transaction) just before
/// the first batch that has a statement starting with `trigger`: the request
/// read the account, then the deletion committed, then the request writes.
struct DeletedMeanwhile<'a> {
    inner: &'a SqliteDb,
    trigger: &'static str,
    fired: Cell<bool>,
}

#[async_trait(?Send)]
impl Db for DeletedMeanwhile<'_> {
    async fn batch(&self, stmts: Vec<Stmt>) -> DbResult<Vec<Rows>> {
        if !self.fired.get() && stmts.iter().any(|s| s.sql.starts_with(self.trigger)) {
            self.fired.set(true);
            self.inner
                .batch(vec![stmt!(
                    "UPDATE user SET authenticator = 'deleted_1_x', email_addr = 'deleted_1_x', \
                     name = 'deleted', blocked = 1 WHERE id = 1"
                )])
                .await?;
        }
        self.inner.batch(stmts).await
    }
}

impl World {
    fn rpc_on(&self, db: &dyn Db, req: &Req, now: i64) -> SchedOutcome {
        let catalog = block_on(Catalog::load(&self.db)).unwrap();
        let random = || self.rand();
        let ctx = SchedCtx {
            db,
            config: &self.config,
            plan_classes: &self.plan_classes,
            catalog: &catalog,
            code_sign_key: "",
            now,
            random: &random,
            client_ip: "198.51.100.9",
        };
        block_on(handle_request(&ctx, &req.xml())).unwrap()
    }
}

fn refused_as_unknown(out: &SchedOutcome) {
    assert!(has_message(out, "Invalid or missing account key"));
    assert_eq!((out.n_sent, out.n_reported, out.n_resent), (0, 0, 0));
    assert!(out.transition.is_empty());
    assert!(out.reply.results.is_empty() && out.reply.wus.is_empty());
}

#[test]
fn a_request_cannot_make_a_computer_for_an_account_being_deleted() {
    let w = World::new();
    w.submit("j1", 1000, 1, 1);
    let racing = DeletedMeanwhile {
        inner: &w.db,
        trigger: "INSERT INTO host",
        fired: Cell::new(false),
    };
    let out = w.rpc_on(&racing, &Req::new(0), 2000);
    assert!(racing.fired.get());
    refused_as_unknown(&out);
    assert_eq!(out.host_id, 0);
    let hosts = block_on(w.db.run(stmt!("SELECT count(*) AS n FROM host"))).unwrap();
    assert_eq!(hosts.scalar_i64(), Some(0));
    let claimed = block_on(w.db.run(stmt!(
        "SELECT count(*) AS n FROM result WHERE server_state = 4"
    )))
    .unwrap();
    assert_eq!(claimed.scalar_i64(), Some(0));
}

#[test]
fn the_writes_of_a_request_do_nothing_for_an_account_deleted_after_it_read_it() {
    let w = World::new();
    let wu1 = w.submit("j1", 1000, 1, 1);
    w.submit("j2", 1000, 1, 1);
    // A host with a task in progress.
    let mut first = Req::new(0);
    first.work_secs = 100.0;
    first.ncpus = 1;
    let out = w.rpc(&first, 2000);
    assert_eq!(out.n_sent, 1);
    let host = out.host_id;
    let before_host = w.host(host);
    let before_wu = w.wu(wu1);
    let before_results: Vec<_> = w
        .results(wu1)
        .iter()
        .map(|r| (r.server_state, r.received_time, r.outcome))
        .collect();

    // It reports the task, asks for work and changes the host's details; the
    // deletion commits between the read and the batch of writes.
    let mut req = Req::new(host);
    req.seqno = 2;
    req.domain = "changed-name".into();
    req.reports = vec![("j1_0".into(), RESULT_FILES_UPLOADED, 0)];
    let racing = DeletedMeanwhile {
        inner: &w.db,
        trigger: "SELECT CASE WHEN EXISTS (SELECT 1 FROM user WHERE id = ?",
        fired: Cell::new(false),
    };
    let out = w.rpc_on(&racing, &req, 2500);
    assert!(racing.fired.get());
    refused_as_unknown(&out);

    // The report, the host's new details, the claim of the next task and
    // everything else the batch held were not written.
    let after_host = w.host(host);
    assert_eq!(after_host.rpc_time, before_host.rpc_time);
    assert_eq!(after_host.domain_name, before_host.domain_name);
    assert_eq!(after_host.rpc_seqno, before_host.rpc_seqno);
    let after_wu = w.wu(wu1);
    assert_eq!(
        (after_wu.rev, after_wu.transition_time),
        (before_wu.rev, before_wu.transition_time)
    );
    let after_results: Vec<_> = w
        .results(wu1)
        .iter()
        .map(|r| (r.server_state, r.received_time, r.outcome))
        .collect();
    assert_eq!(after_results, before_results);
    let sent = block_on(w.db.run(stmt!(
        "SELECT count(*) AS n FROM result WHERE server_state = 4"
    )))
    .unwrap();
    assert_eq!(sent.scalar_i64(), Some(1));

    // The same request from an account that is not being deleted goes through.
    let w2 = World::new();
    w2.submit("j1", 1000, 1, 1);
    let out = w2.rpc(&first, 2000);
    assert_eq!(out.n_sent, 1);
}

#[test]
fn a_new_computer_never_gets_the_id_of_one_that_was_deleted() {
    let w = World::new();
    let first = w.rpc(&Req::new(0), 2000).host_id;
    assert_eq!(first, 1);
    // The highest computer goes, and so does the table's only row.
    block_on(w.db.run(stmt!("DELETE FROM host WHERE id = ?", first))).unwrap();
    let mut again = Req::new(0);
    again.cpid = "another".into();
    again.domain = "other-box".into();
    let second = w.rpc(&again, 2100).host_id;
    assert!(second > first, "{second} after {first}");
    // A computer that is not the highest changes nothing for the next.
    let mut third = Req::new(0);
    third.cpid = "third".into();
    third.domain = "third-box".into();
    let third = w.rpc(&third, 2200).host_id;
    block_on(w.db.run(stmt!("DELETE FROM host WHERE id = ?", second))).unwrap();
    let mut fourth = Req::new(0);
    fourth.cpid = "fourth".into();
    fourth.domain = "fourth-box".into();
    let fourth = w.rpc(&fourth, 2300).host_id;
    assert!(
        fourth > third && third > second,
        "{second} {third} {fourth}"
    );
}
