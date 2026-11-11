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
            "<scheduler_request>\n    <authenticator>{}</authenticator>\n    <hostid>{}</hostid>\n    <rpc_seqno>{}</rpc_seqno>\n    <core_client_major_version>8</core_client_major_version>\n    <core_client_minor_version>0</core_client_minor_version>\n    <core_client_release>4</core_client_release>\n    <work_req_seconds>{w}</work_req_seconds>\n    <cpu_req_secs>{w}</cpu_req_secs>\n    <cpu_req_instances>{}</cpu_req_instances>\n    <estimated_delay>0</estimated_delay>\n    <platform_name>{}</platform_name>\n",
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
            result_template: RESULT_TEMPLATE.into(),
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

    // The client holds nothing.
    let mut req = Req::new(host);
    req.seqno = 2;
    req.work_secs = 0.0;
    req.other = Some(Vec::new());
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

    // The client holds it: nothing to resend.
    req.seqno = 3;
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
    req.seqno = 4;
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
