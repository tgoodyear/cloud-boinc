//! Scheduler behaviour that a project moved from a stock server relies on:
//! jobs pinned to an app version number, client version gates, GPU plan
//! classes (Apple GPU, GPU RAM limits), `host.misc`, the daily job counter
//! under a lost claim, and a large backlog of unsent results.
//!
//! The behaviour under test is upstream's unless a test says otherwise:
//! `get_app_version` in `sched/sched_version.cpp` for the version number a
//! workunit is pinned to, `scan_tables` in `sched/sched_shmem.cpp` for which
//! versions are kept, `process_request` in `sched/handle_request.cpp` for
//! the client version gates, `host_info_json` in `sched/sched_host.cpp` for
//! `host.misc`.

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
use cb_db::{query_one, stmt, Db, DbResult, Rows, Stmt};
use cb_proto::consts::*;
use cb_sqlite::SqliteDb;
use futures_lite::future::block_on;

const LINUX: &str = "x86_64-pc-linux-gnu";
const WINDOWS: &str = "windows_x86_64";
const INPUT_TEMPLATE: &str = "<input_template>\n<workunit>\n<rsc_fpops_est>1e12</rsc_fpops_est>\n<rsc_memory_bound>1e8</rsc_memory_bound>\n<rsc_disk_bound>1e8</rsc_disk_bound>\n<delay_bound>3600</delay_bound>\n</workunit>\n</input_template>\n";
const RESULT_TEMPLATE: &str = "<output_template>\n<file_info>\n    <name><OUTFILE_0/></name>\n    <generated_locally/>\n    <upload_when_present/>\n    <max_nbytes>5000000</max_nbytes>\n    <url><UPLOAD_URL/></url>\n</file_info>\n<result>\n    <file_ref>\n        <file_name><OUTFILE_0/></file_name>\n        <open_name>out</open_name>\n    </file_ref>\n</result>\n</output_template>\n";

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
    hostid: i64,
    seqno: i32,
    platform: String,
    ncpus: i32,
    work_secs: f64,
    cpid: String,
    os: String,
    vendor: String,
    model: String,
    client: (i32, i32, i32),
    /// Whether the client says it understands plan classes.
    cap_plan_class: bool,
    ram: f64,
    /// Inside `<coprocs>`.
    coprocs: String,
    /// Inside `<host_info>`.
    host_extra: String,
    /// At the top level of the request.
    top_extra: String,
    /// `(name, client state, exit status)`
    reports: Vec<(String, i32, i32)>,
}

impl Req {
    fn xml(&self) -> String {
        let (major, minor, release) = self.client;
        let mut s = format!(
            "<scheduler_request>\n<authenticator>auth1</authenticator>\n<hostid>{}</hostid>\n<rpc_seqno>{}</rpc_seqno>\n<core_client_major_version>{major}</core_client_major_version>\n<core_client_minor_version>{minor}</core_client_minor_version>\n<core_client_release>{release}</core_client_release>\n{}<work_req_seconds>{w}</work_req_seconds>\n<cpu_req_secs>{w}</cpu_req_secs>\n<cpu_req_instances>{}</cpu_req_instances>\n<estimated_delay>0</estimated_delay>\n<platform_name>{}</platform_name>\n{}",
            self.hostid,
            self.seqno,
            if self.cap_plan_class {
                "<client_cap_plan_class>1</client_cap_plan_class>\n"
            } else {
                ""
            },
            if self.work_secs > 0.0 { 1 } else { 0 },
            self.platform,
            self.top_extra,
            w = self.work_secs,
        );
        s.push_str(&format!(
            "<host_info>\n<domain_name>{c}</domain_name>\n<host_cpid>{c}</host_cpid>\n<p_ncpus>{}</p_ncpus>\n<p_vendor>{}</p_vendor>\n<p_model>{}</p_model>\n<p_fpops>2e9</p_fpops>\n<m_nbytes>{}</m_nbytes>\n<d_total>1e11</d_total>\n<d_free>5e10</d_free>\n<os_name>{}</os_name>\n<os_version>6.1</os_version>\n{}</host_info>\n",
            self.ncpus,
            self.vendor,
            self.model,
            self.ram,
            self.os,
            self.host_extra,
            c = self.cpid,
        ));
        if !self.coprocs.is_empty() {
            s.push_str(&format!("<coprocs>\n{}\n</coprocs>\n", self.coprocs));
        }
        for (name, state, exit) in &self.reports {
            s.push_str(&format!(
                "<result>\n<name>{name}</name>\n<final_cpu_time>90</final_cpu_time>\n<final_elapsed_time>100</final_elapsed_time>\n<exit_status>{exit}</exit_status>\n<state>{state}</state>\n<app_version_num>100</app_version_num>\n</result>\n"
            ));
        }
        s.push_str("</scheduler_request>\n");
        s
    }

    fn reporting(&self, name: &str) -> Req {
        let mut r = self.clone();
        r.work_secs = 0.0;
        r.reports = vec![(name.into(), RESULT_FILES_UPLOADED, 0)];
        r
    }

    fn client(mut self, major: i32, minor: i32, release: i32) -> Req {
        self.client = (major, minor, release);
        self
    }

    fn coprocs(mut self, xml: &str) -> Req {
        self.coprocs = xml.into();
        self
    }
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

/// Records the SQL sent through it.
struct Recording<'a> {
    inner: &'a SqliteDb,
    sql: RefCell<Vec<String>>,
}

#[async_trait(?Send)]
impl<'a> Db for Recording<'a> {
    async fn batch(&self, stmts: Vec<Stmt>) -> DbResult<Vec<Rows>> {
        self.sql
            .borrow_mut()
            .extend(stmts.iter().map(|s| s.sql.clone()));
        self.inner.batch(stmts).await
    }
}

struct World {
    db: SqliteDb,
    config: Config,
    plan_classes: PlanClassSpecs,
    seq: Cell<u32>,
}

impl World {
    /// A project with the Linux and Windows platforms and one user.
    fn new(plan_classes: &str, config: &str) -> World {
        let db = SqliteDb::with_schema().unwrap();
        let mut config = Config::parse(&format!(
            "<config><master_url>https://p.example/</master_url>{config}</config>"
        ))
        .unwrap();
        config.long_name = "Test".into();
        config.version_select_random_factor = 0.0;
        block_on(db.batch(vec![
            stmt!("INSERT INTO platform (id, create_time, name, user_friendly_name) VALUES (1, 0, ?, 'Linux'), (2, 0, ?, 'Windows')", LINUX, WINDOWS),
            stmt!("INSERT INTO user (id, create_time, email_addr, name, authenticator, cross_project_id) VALUES (1, 0, 'u@example.com', 'u', 'auth1', 'cpid1')"),
        ]))
        .unwrap();
        World {
            db,
            config,
            plan_classes: PlanClassSpecs::parse(plan_classes).unwrap(),
            seq: Cell::new(0),
        }
    }

    fn rand(&self) -> u32 {
        self.seq.set(self.seq.get().wrapping_add(2_654_435_761));
        self.seq.get()
    }

    fn sql(&self, sql: &str) {
        block_on(self.db.run(Stmt::new(sql))).unwrap();
    }

    fn app(&self, id: i64, name: &str, extra: &str) {
        self.sql(&format!(
            "INSERT INTO app (id, create_time, name, user_friendly_name, validator) VALUES ({id}, 0, '{name}', '{name}', 'trivial')"
        ));
        if !extra.is_empty() {
            self.sql(&format!("UPDATE app SET {extra} WHERE id = {id}"));
        }
    }

    fn version(&self, id: i64, app: i64, name: &str, num: i32, platform: i64, plan: &str) {
        self.version_with(id, app, name, num, platform, plan, "");
    }

    #[allow(clippy::too_many_arguments)]
    fn version_with(
        &self,
        id: i64,
        app: i64,
        name: &str,
        num: i32,
        platform: i64,
        plan: &str,
        extra: &str,
    ) {
        let xml = format!("<app_version>\n<app_name>{name}</app_name>\n<version_num>{num}</version_num>\n<file_ref>\n<file_name>{name}_{id}</file_name>\n<main_program/>\n</file_ref>\n</app_version>\n");
        block_on(self.db.run(stmt!(
            "INSERT INTO app_version (id, create_time, appid, version_num, platformid, xml_doc, plan_class) VALUES (?, 0, ?, ?, ?, ?, ?)",
            id, app, num, platform, xml, plan
        )))
        .unwrap();
        if !extra.is_empty() {
            self.sql(&format!("UPDATE app_version SET {extra} WHERE id = {id}"));
        }
    }

    fn job(&self, app: &str, name: &str, now: i64, spec: JobSpec) -> i64 {
        let sub = Submission {
            app: app.into(),
            input_template: INPUT_TEMPLATE.into(),
            result_template: RESULT_TEMPLATE.into(),
            jobs: vec![JobSpec {
                name: name.into(),
                min_quorum: Some(1),
                target_nresults: Some(1),
                ..spec
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
        assert_eq!(out[0].error, None, "{name}");
        out[0].workunit_id
    }

    fn pinned(&self, app: &str, name: &str, num: i32) -> i64 {
        self.job(
            app,
            name,
            1000,
            JobSpec {
                app_version_num: Some(num),
                ..Default::default()
            },
        )
    }

    fn unpinned(&self, app: &str, name: &str) -> i64 {
        self.job(app, name, 1000, JobSpec::default())
    }

    /// A computer of the user, known to the project, with its next request.
    fn computer(&self, cpid: &str) -> Req {
        let mut req = Req {
            hostid: 0,
            seqno: 1,
            platform: LINUX.into(),
            ncpus: 4,
            work_secs: 0.0,
            cpid: cpid.into(),
            os: "Linux".into(),
            vendor: "GenuineIntel".into(),
            model: "Intel(R) Xeon(R) CPU E5-2680 v4".into(),
            client: (8, 0, 4),
            cap_plan_class: true,
            ram: 8e9,
            coprocs: String::new(),
            host_extra: String::new(),
            top_extra: String::new(),
            reports: Vec::new(),
        };
        let out = self.rpc(&req, 500);
        assert_eq!(out.n_sent, 0);
        req.hostid = out.host_id;
        req.seqno = 2;
        req.work_secs = 1e6;
        req
    }

    fn ctx<'a>(
        &'a self,
        db: &'a dyn Db,
        catalog: &'a Catalog,
        random: &'a dyn Fn() -> u32,
        now: i64,
    ) -> SchedCtx<'a> {
        SchedCtx {
            db,
            config: &self.config,
            plan_classes: &self.plan_classes,
            catalog,
            code_sign_key: "",
            now,
            random,
            client_ip: "198.51.100.9",
        }
    }

    fn rpc(&self, req: &Req, now: i64) -> SchedOutcome {
        self.rpc_xml(&req.xml(), now)
    }

    fn rpc_xml(&self, xml: &str, now: i64) -> SchedOutcome {
        let catalog = block_on(Catalog::load(&self.db)).unwrap();
        let random = || self.rand();
        let ctx = self.ctx(&self.db, &catalog, &random, now);
        block_on(handle_request(&ctx, xml)).unwrap()
    }

    /// A request, with the SQL it sent.
    fn rpc_recorded(&self, req: &Req, now: i64) -> (SchedOutcome, Vec<String>) {
        let catalog = block_on(Catalog::load(&self.db)).unwrap();
        let random = || self.rand();
        let db = Recording {
            inner: &self.db,
            sql: RefCell::new(Vec::new()),
        };
        let ctx = self.ctx(&db, &catalog, &random, now);
        let out = block_on(handle_request(&ctx, &req.xml())).unwrap();
        (out, db.sql.take())
    }

    /// `outer` reads, then `inner` runs completely, then `outer` writes.
    fn race(&self, outer: &Req, inner: &Req, now: i64) -> (SchedOutcome, SchedOutcome) {
        let catalog = block_on(Catalog::load(&self.db)).unwrap();
        let random = || self.rand();
        let ctx_inner = self.ctx(&self.db, &catalog, &random, now);
        let xml_inner = inner.xml();
        let db = Interleave {
            inner: &self.db,
            before_write: RefCell::new(Some(Box::pin(async {
                handle_request(&ctx_inner, &xml_inner).await.unwrap()
            }))),
            other: RefCell::new(None),
        };
        let ctx_outer = self.ctx(&db, &catalog, &random, now);
        let out_outer = block_on(handle_request(&ctx_outer, &outer.xml())).unwrap();
        let out_inner = db.other.borrow_mut().take().unwrap();
        (out_outer, out_inner)
    }

    fn result_named(&self, name: &str) -> JobResult {
        block_on(query_one(
            &self.db,
            stmt!("SELECT * FROM result WHERE name = ?", name),
        ))
        .unwrap()
        .unwrap()
    }

    fn wu(&self, name: &str) -> Workunit {
        block_on(query_one(
            &self.db,
            stmt!("SELECT * FROM workunit WHERE name = ?", name),
        ))
        .unwrap()
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

    /// The version numbers jobs went out with, by workunit name.
    fn sent_versions(&self, out: &SchedOutcome) -> Vec<(String, i32)> {
        let mut v: Vec<(String, i32)> = sent_names(out)
            .into_iter()
            .zip(out.reply.results.iter())
            .map(|(result, r)| (wu_of(&result), r.version_num))
            .collect();
        v.sort();
        v
    }

    fn unsent_of(&self, app: &str) -> i64 {
        block_on(query_one::<Count>(
            &self.db,
            stmt!(
                "SELECT count(*) AS n FROM result WHERE server_state = 2 AND appid = (SELECT id FROM app WHERE name = ?)",
                app
            ),
        ))
        .unwrap()
        .unwrap()
        .n
    }
}

#[derive(serde::Deserialize)]
struct Count {
    n: i64,
}

fn has_message(out: &SchedOutcome, text: &str) -> bool {
    out.reply.messages.iter().any(|m| m.message.contains(text))
}

fn sent_names(out: &SchedOutcome) -> Vec<String> {
    out.reply
        .results
        .iter()
        .map(|r| {
            let s = &r.xml_doc_in;
            let result = s.find("<result>").unwrap();
            let at = s[result..].find("<name>").unwrap() + result + 6;
            s[at..s[at..].find('<').unwrap() + at].to_string()
        })
        .collect()
}

/// A result's name is `<workunit name>_<n>`.
fn wu_of(result: &str) -> String {
    result.rsplit_once('_').unwrap().0.to_string()
}

// ---- Jobs pinned to an app version number ---------------------------------

/// Two apps, each with versions 100 and 101 for Linux. `p` keeps versions
/// from 100 up (`min_version`), `q` sends only its latest.
fn pinned_world() -> World {
    let w = World::new("", "<enable_assignment>1</enable_assignment>");
    w.app(1, "p", "min_version = 100");
    w.app(2, "q", "");
    w.version(11, 1, "p", 100, 1, "");
    w.version(12, 1, "p", 101, 1, "");
    w.version(21, 2, "q", 100, 1, "");
    w.version(22, 2, "q", 101, 1, "");
    w
}

#[test]
fn a_pinned_job_goes_only_with_its_version_and_unpinned_ones_with_the_newest() {
    let w = pinned_world();
    w.unpinned("p", "p_any");
    w.pinned("p", "p_old", 100);
    w.pinned("p", "p_new", 101);
    w.pinned("p", "p_none", 99);
    w.unpinned("q", "q_any");
    // `q` has no `min_version`: its version 100 is superseded and not kept.
    w.pinned("q", "q_old", 100);
    w.pinned("q", "q_new", 101);
    let host = w.computer("a");
    let out = w.rpc(&host, 2000);
    assert_eq!(
        w.sent_versions(&out),
        vec![
            ("p_any".into(), 101),
            ("p_new".into(), 101),
            ("p_old".into(), 100),
            ("q_any".into(), 101),
            ("q_new".into(), 101),
        ]
    );
    // What the result records is the version it went with.
    assert_eq!(w.result_named("p_old_0").app_version_id, 11);
    assert_eq!(w.result_named("p_old_0").app_version_num, 100);
    // The versions the reply carries are the ones used.
    let number = |doc: &str| -> i32 {
        doc.split("<version_num>")
            .nth(1)
            .unwrap()
            .split('<')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    let mut nums: Vec<i32> = out
        .reply
        .app_versions
        .iter()
        .map(|v| {
            let app = if v.xml_doc.contains("<app_name>p<") {
                1000
            } else {
                2000
            };
            app + number(&v.xml_doc)
        })
        .collect();
    nums.sort();
    assert_eq!(nums, vec![1100, 1101, 2101]);
    // A job pinned to a version nobody can send waits, and the host is
    // told so as for any job it passes over, not that there are none.
    let again = w.rpc(&host, 2100);
    assert_eq!(again.n_sent, 0);
    assert!(has_message(&again, "No tasks sent"));
    assert!(!has_message(&again, "Project has no tasks available"));
    assert_eq!(w.unsent_of("p"), 1);
    assert_eq!(w.unsent_of("q"), 1);
}

#[test]
fn a_pinned_job_is_not_sent_to_a_host_without_that_version() {
    // Version 100 is for Linux, 101 for Windows.
    let w = World::new("", "");
    w.app(1, "p", "min_version = 100");
    w.version(11, 1, "p", 100, 1, "");
    w.version(12, 1, "p", 101, 2, "");
    w.pinned("p", "p_win", 101);
    w.pinned("p", "p_lin", 100);
    let linux = w.computer("a");
    let out = w.rpc(&linux, 2000);
    assert_eq!(w.sent_versions(&out), vec![("p_lin".into(), 100)]);
    let mut windows = w.computer("b");
    windows.platform = WINDOWS.into();
    windows.os = "Microsoft Windows 11".into();
    let out = w.rpc(&windows, 2100);
    assert_eq!(w.sent_versions(&out), vec![("p_win".into(), 101)]);
}

#[test]
fn an_assigned_job_pinned_to_a_version_goes_with_it() {
    let w = pinned_world();
    let host = w.computer("a");
    let assign = |id: i64| JobSpec {
        app_version_num: Some(100),
        assign: Some(Assign {
            target: AssignTarget::Host,
            id,
        }),
        ..Default::default()
    };
    w.job("p", "pa", 1000, assign(host.hostid));
    let out = w.rpc(&host, 2000);
    assert_eq!(w.sent_versions(&out), vec![("pa".into(), 100)]);
    // Pinned to a version the project does not have: not sent, and the
    // job does not use up the host's turn.
    let w = pinned_world();
    let host = w.computer("a");
    w.job(
        "p",
        "pb",
        1000,
        JobSpec {
            app_version_num: Some(7),
            ..assign(host.hostid)
        },
    );
    let out = w.rpc(&host, 2000);
    assert_eq!(out.n_sent, 0);
}

#[test]
fn a_pinned_job_of_an_app_with_homogeneous_app_version_commits_to_that_version() {
    let w = World::new("", "");
    w.app(1, "h", "homogeneous_app_version = 1, min_version = 100");
    w.version(11, 1, "h", 100, 1, "");
    w.version(12, 1, "h", 101, 1, "");
    w.pinned("h", "h_old", 100);
    w.unpinned("h", "h_new");
    let host = w.computer("a");
    let out = w.rpc(&host, 2000);
    assert_eq!(
        w.sent_versions(&out),
        vec![("h_new".into(), 101), ("h_old".into(), 100)]
    );
    assert_eq!(w.wu("h_old").app_version_id, 11);
    assert_eq!(w.wu("h_new").app_version_id, 12);
}

#[test]
fn a_job_pinned_to_a_version_is_in_the_job_status() {
    let w = pinned_world();
    w.pinned("p", "p_old", 100);
    let s = block_on(cb_core::admin::job_status(&w.db, "p_old"))
        .unwrap()
        .unwrap();
    assert_eq!(s.app_version_num, 100);
    // A negative number is refused, and so is a negative class.
    let sub = |spec: JobSpec| Submission {
        app: "p".into(),
        input_template: INPUT_TEMPLATE.into(),
        result_template: RESULT_TEMPLATE.into(),
        jobs: vec![JobSpec {
            name: "bad".into(),
            ..spec
        }],
    };
    for spec in [
        JobSpec {
            app_version_num: Some(-1),
            ..Default::default()
        },
        JobSpec {
            hr_class: Some(-5),
            ..Default::default()
        },
    ] {
        let random = || w.rand();
        let out = block_on(create_work(
            &w.db,
            &w.config,
            Some(signer()),
            1,
            &random,
            &sub(spec),
        ))
        .unwrap();
        assert!(out[0].error.is_some());
    }
}

#[test]
fn a_job_can_start_committed_to_a_class() {
    let w = World::new("", "");
    w.app(1, "r", "homogeneous_redundancy = 2");
    w.version(11, 1, "r", 100, 1, "");
    w.job(
        "r",
        "r_win",
        1000,
        JobSpec {
            hr_class: Some(384 + 2),
            ..Default::default()
        },
    );
    w.unpinned("r", "r_free");
    let linux = w.computer("a");
    let out = w.rpc(&linux, 2000);
    assert_eq!(sent_names(&out).len(), 1);
    assert_eq!(wu_of(&sent_names(&out)[0]), "r_free");
    assert_eq!(w.wu("r_win").hr_class, 386);
}

// ---- Client version gates --------------------------------------------------

#[test]
fn a_client_below_the_minimum_gets_no_work_but_its_reports_are_taken() {
    let w = World::new(
        "",
        "<min_core_client_version>80000</min_core_client_version>",
    );
    w.app(1, "a", "");
    w.version(11, 1, "a", 100, 1, "");
    w.unpinned("a", "j1");
    let host = w.computer("a");
    let out = w.rpc(&host, 2000);
    assert_eq!(out.n_sent, 1);
    let name = sent_names(&out)[0].clone();
    w.unpinned("a", "j2");
    // The same host with an old client, asking for work and reporting.
    let mut old = host.reporting(&name).client(7, 20, 0);
    old.work_secs = 1e6;
    old.seqno = 3;
    let out = w.rpc(&old, 3000);
    assert_eq!(out.n_reported, 1);
    assert_eq!(out.n_sent, 0);
    assert!(has_message(&out, "client software is too old"));
    // Nothing else said: no "No tasks sent" for a client not offered work.
    assert!(!has_message(&out, "No tasks sent"));
    assert_eq!(out.reply.request_delay, 86400.0);
    assert_eq!(w.result_named(&name).server_state, RESULT_SERVER_STATE_OVER);
    // With nothing to report it is turned away before the database is read.
    let mut old = host.clone().client(7, 20, 0);
    old.seqno = 4;
    let out = w.rpc(&old, 3100);
    assert_eq!(out.n_sent, 0);
    assert!(has_message(&out, "client software is too old"));
    assert_eq!(out.host_id, 0);
    // A current client is served.
    let mut now = host.clone();
    now.seqno = 5;
    now.work_secs = 1e6;
    assert_eq!(w.rpc(&now, 3200).n_sent, 1);
}

#[test]
fn a_client_that_will_soon_be_too_old_is_warned() {
    let w = World::new(
        "",
        "<min_core_client_version_announced>80300</min_core_client_version_announced>\
         <min_core_client_upgrade_deadline>1000000</min_core_client_upgrade_deadline>",
    );
    w.app(1, "a", "");
    w.version(11, 1, "a", 100, 1, "");
    let host = w.computer("a");
    // 4 days and 5 hours before the deadline: low priority.
    let now = 1_000_000 - (4 * 86400 + 5 * 3600 + 100);
    let out = w.rpc(&host, now);
    let m = out
        .reply
        .messages
        .iter()
        .find(|m| m.message.contains("this project will require"))
        .unwrap();
    assert_eq!(
        m.message,
        "In 4 days and 5 hours, this project will require a minimum BOINC version of 8.3.0.  You are currently using version 8.0.4; please upgrade before this time."
    );
    assert_eq!(m.priority, "low");
    // Within three days it is a notice.
    let mut h2 = host.clone();
    h2.seqno = 3;
    let out = w.rpc(&h2, 1_000_000 - (2 * 86400 + 3600 + 100));
    let m = out
        .reply
        .messages
        .iter()
        .find(|m| m.message.contains("this project will require"))
        .unwrap();
    assert!(m.message.starts_with("In 2 days and 1 hours"));
    assert_eq!(m.priority, "notice");
    // After the deadline, and for a client that is new enough, nothing.
    h2.seqno = 4;
    let out = w.rpc(&h2, 1_000_001);
    assert!(!has_message(&out, "will require"));
    let new = h2.clone().client(8, 3, 0);
    let out = w.rpc(&new, 1_000_000 - 100_000);
    assert!(!has_message(&out, "will require"));
}

#[test]
fn an_app_versions_client_range_is_kept_to() {
    let w = World::new("", "");
    w.app(1, "a", "");
    // `min_core_version` 800 is read as 80000, as `scan_tables` does.
    w.version_with(11, 1, "a", 100, 1, "", "min_core_version = 800");
    w.app(2, "b", "");
    w.version_with(21, 2, "b", 100, 1, "", "max_core_version = 70500");
    w.app(3, "c", "");
    w.version_with(
        31,
        3,
        "c",
        100,
        1,
        "",
        "min_core_version = 70000, max_core_version = 79999",
    );
    for app in ["a", "b", "c"] {
        w.unpinned(app, &format!("{app}_j"));
    }
    let new = w.computer("a");
    let old = w.computer("b").client(7, 4, 0);
    let out = w.rpc(&new, 2000);
    assert_eq!(
        w.sent_versions(&out)
            .iter()
            .map(|v| v.0.as_str())
            .collect::<Vec<_>>(),
        vec!["a_j"],
        "8.0.4: above b's maximum, above c's"
    );
    let mut old = old;
    old.seqno = 3;
    let out = w.rpc(&old, 2100);
    assert_eq!(
        w.sent_versions(&out)
            .iter()
            .map(|v| v.0.as_str())
            .collect::<Vec<_>>(),
        vec!["b_j", "c_j"],
        "7.4.0: below a's minimum"
    );
    // The volunteer is not told to update for a version's sake.
    assert!(!has_message(&out, "newer BOINC"));
}

#[test]
fn a_plan_class_with_a_client_range_says_so_when_the_client_is_too_old() {
    let w = World::new(
        "<plan_classes><plan_class><name>new</name><min_core_client_version>80300</min_core_client_version></plan_class></plan_classes>",
        "",
    );
    w.app(1, "a", "");
    w.version(11, 1, "a", 100, 1, "new");
    w.unpinned("a", "j");
    let host = w.computer("a");
    let out = w.rpc(&host, 2000);
    assert_eq!(out.n_sent, 0);
    assert!(has_message(
        &out,
        "A newer BOINC may be required for some tasks."
    ));
    let mut new = host.client(8, 3, 0);
    new.seqno = 3;
    assert_eq!(w.rpc(&new, 2100).n_sent, 1);
}

#[test]
fn a_client_that_does_not_know_plan_classes_gets_only_one_cpu_versions() {
    let w = World::new(
        "<plan_classes><plan_class><name>mt</name><min_ncpus>2</min_ncpus><max_threads>8</max_threads></plan_class></plan_classes>",
        "",
    );
    w.app(1, "a", "");
    w.version(11, 1, "a", 100, 1, "");
    w.version(12, 1, "a", 100, 1, "mt");
    w.unpinned("a", "j1");
    let mut host = w.computer("a");
    host.cap_plan_class = false;
    let out = w.rpc(&host, 2000);
    assert_eq!(out.n_sent, 1);
    assert_eq!(out.reply.app_versions.len(), 1);
    assert_eq!(out.reply.app_versions[0].plan_class, "");
    // One that does is given the multi-threaded version, the faster.
    w.unpinned("a", "j2");
    let mut cap = host.clone();
    cap.cap_plan_class = true;
    cap.seqno = 3;
    let out = w.rpc(&cap, 2100);
    assert_eq!(out.n_sent, 1);
    assert_eq!(out.reply.app_versions[0].plan_class, "mt");
}

// ---- GPU plan classes ------------------------------------------------------

const APPLE: &str = "<coproc_apple_gpu>\n<count>1</count>\n<model>Apple M2 Pro</model>\n<available_ram>17179869184.000000</available_ram>\n<have_metal>1</have_metal>\n<have_opencl>1</have_opencl>\n<ncores>19</ncores>\n<metal_support>3</metal_support>\n<req_secs>1000</req_secs>\n<req_instances>1</req_instances>\n<peak_flops>6800000000000.000000</peak_flops>\n<coproc_opencl>\n<name>Apple M2 Pro</name>\n<vendor>Apple</vendor>\n<global_mem_size>17179869184</global_mem_size>\n<opencl_device_version>OpenCL 1.2 </opencl_device_version>\n<opencl_device_version_int>102</opencl_device_version_int>\n</coproc_opencl>\n</coproc_apple_gpu>";

#[test]
fn an_apple_gpu_host_gets_a_metal_version_and_the_gpu_is_recorded() {
    let w = World::new(
        "<plan_classes><plan_class><name>metal</name><gpu_type>apple_gpu</gpu_type><min_metal_support>3</min_metal_support><min_gpu_ram_mb>8192</min_gpu_ram_mb><gpu_ram_used_mb>2048</gpu_ram_used_mb></plan_class></plan_classes>",
        "",
    );
    w.app(1, "m", "");
    w.version(11, 1, "m", 100, 1, "metal");
    w.unpinned("m", "j1");
    // A host with no Apple GPU gets nothing.
    let plain = w.computer("a");
    assert_eq!(w.rpc(&plain, 2000).n_sent, 0);
    let mut mac = w.computer("b").coprocs(APPLE);
    mac.seqno = 3;
    let out = w.rpc(&mac, 2100);
    assert_eq!(out.n_sent, 1);
    let v = &out.reply.app_versions[0];
    assert_eq!(v.plan_class, "metal");
    assert_eq!(v.proc_type, PROC_TYPE_APPLE_GPU);
    assert_eq!(v.gpu_usage, 1.0);
    assert_eq!(v.gpu_ram, 2048.0 * 1_048_576.0);
    // Counted against the host's GPU, not its CPUs.
    let r = w.result_named("j1_0");
    assert_eq!(r.app_version_id, 11);
    // The GPU is in `host.misc`, as `host_info_json` writes it.
    let misc: serde_json::Value = serde_json::from_str(&w.host(mac.hostid).misc).unwrap();
    assert_eq!(
        misc["gpus"][0],
        serde_json::json!({
            "type": "apple",
            "model": "Apple M2 Pro",
            "count": 1,
            "ram_mb": 16384,
            "driver_version": "3",
            "opencl_version": "102"
        })
    );
}

// ---- Computers --------------------------------------------------------------

/// A client that knows its host ID is 0 after a detach and attach finds its host by the account
/// and the host CPID. When the CPID is new, `find_host_by_other` in `sched/handle_request.cpp`
/// looks for the account's host with the same name, address, OS name, CPU model and memory,
/// unless the client says it was started with `--allow_multiple_clients`: then each instance is
/// a host of its own, and its CPID differs from the others' by design (it is made from the
/// data directory).
#[test]
fn clients_that_share_a_computer_get_a_host_each_only_when_they_say_so() {
    let w = World::new("", "");
    let mut a = w.computer("a");
    // The first request had no address; stock searches only when it has one.
    a.host_extra = "<ip_addr>198.51.100.7</ip_addr>\n".into();
    a.work_secs = 0.0;
    a.seqno = 2;
    assert_eq!(w.rpc(&a, 1500).host_id, a.hostid);
    let another = |cpid: &str, multiple: bool, now: i64| -> i64 {
        let mut b = a.clone();
        b.hostid = 0;
        b.seqno = 0;
        if multiple {
            b.top_extra = "<allow_multiple_clients>1</allow_multiple_clients>\n".into();
        }
        let xml = b.xml().replace(
            "<host_cpid>a</host_cpid>",
            &format!("<host_cpid>{cpid}</host_cpid>"),
        );
        w.rpc_xml(&xml, now).host_id
    };
    // A client with a new CPID on a computer that looks like a known one is that computer.
    assert_eq!(another("b", false, 2000), a.hostid);
    // One of several clients on the computer is not.
    let second = another("c", true, 2100);
    assert_ne!(second, a.hostid);
    assert!(second > 0);
    // A client that sends no address is not searched for.
    let mut no_ip = a.clone();
    no_ip.hostid = 0;
    no_ip.seqno = 0;
    no_ip.host_extra = String::new();
    let xml = no_ip
        .xml()
        .replace("<host_cpid>a</host_cpid>", "<host_cpid>d</host_cpid>");
    let third = w.rpc_xml(&xml, 2200).host_id;
    assert!(third != a.hostid && third != second, "{third}");
}

/// A host with the client's CPID is the client's host unless it is obviously another computer
/// (`obviously_different` in `sched/handle_request.cpp`: processor count, vendor and model, OS
/// name and version). A CPID is made from the MAC address and the data directory, so a cloned
/// disk image or virtual machine gives two computers the same one.
#[test]
fn a_host_with_the_clients_cpid_that_is_another_computer_is_not_reused() {
    let w = World::new("", "");
    let a = w.computer("a");
    let again = |model: &str, ncpus: i32, now: i64| -> i64 {
        let mut b = a.clone();
        b.hostid = 0;
        b.seqno = 0;
        b.model = model.into();
        b.ncpus = ncpus;
        w.rpc(&b, now).host_id
    };
    // The same computer that lost its host ID.
    assert_eq!(again(&a.model, a.ncpus, 2000), a.hostid);
    // The same CPID on another CPU, or with another number of them.
    let arm = again("ARMv8 Processor", a.ncpus, 2100);
    assert!(arm > 0 && arm != a.hostid);
    let more = again(&a.model, a.ncpus * 2, 2200);
    assert!(more > 0 && more != a.hostid && more != arm);
}

// ---- Which host a request is ------------------------------------------------
//
// `authenticate_user` in `sched/handle_request.cpp`. The label `didnt_find_host` is where a
// host ID of another account and a stale `rpc_seqno` go: it skips `find_host_by_cpid`, runs
// `find_host_by_other` (not for a client that is one of several), and then makes a host. A
// request with no host ID, or one that names no host, goes through `find_host_by_cpid` first
// (`lookup_user_and_make_new_host`). Whichever host is found or made is stored with
// `rpc_seqno` 0 and named in the reply.

const IP_A: &str = "<ip_addr>198.51.100.7</ip_addr>\n";
const MULTIPLE: &str = "<allow_multiple_clients>1</allow_multiple_clients>\n";

/// A computer `a` of the account that has sent an address.
fn computer_with_address(w: &World) -> Req {
    let mut a = w.computer("a");
    a.host_extra = IP_A.into();
    a.work_secs = 0.0;
    a.seqno = 6;
    assert_eq!(w.rpc(&a, 1500).host_id, a.hostid);
    a
}

/// Another computer of the account with the same kind of hardware, without an address.
fn twin(w: &World, cpid: &str) -> Req {
    let mut b = w.computer(cpid);
    b.work_secs = 0.0;
    b
}

/// A copy of `a`'s state file on another computer: `a`'s host ID with a sequence number below
/// the one stored for it (which is set to 6 here).
fn stale(w: &World, a: &Req) -> Req {
    w.sql(&format!(
        "UPDATE host SET rpc_seqno = 6 WHERE id = {}",
        a.hostid
    ));
    let mut c = a.clone();
    c.seqno = 2;
    c
}

/// The request with the host CPID element changed and nothing else (the domain name of a test
/// client is its CPID string, so changing `cpid` changes that too).
fn with_host_cpid(req: &Req, cpid: &str) -> String {
    req.xml().replace(
        &format!("<host_cpid>{}</host_cpid>", req.cpid),
        &format!("<host_cpid>{cpid}</host_cpid>"),
    )
}

#[test]
fn a_seqno_below_the_stored_one_makes_a_new_host_even_when_the_cpid_matches_another() {
    let w = World::new("", "");
    let a = computer_with_address(&w);
    let b = twin(&w, "b");
    // The copy has the CPID of the account's other host `b` and no address, so only
    // `find_host_by_cpid` could match it, and stock does not run that here.
    let mut copy = stale(&w, &a);
    copy.cpid = "b".into();
    copy.host_extra = String::new();
    let out = w.rpc(&copy, 2000);
    assert!(out.host_id > 0 && out.host_id != a.hostid && out.host_id != b.hostid);
    assert_eq!(out.reply.hostid, out.host_id as u64);
    let made = w.host(out.host_id);
    assert_eq!(made.userid, 1);
    assert_eq!(made.rpc_seqno, 0);
    // The host the copy named is untouched.
    assert_eq!(w.host(a.hostid).rpc_seqno, 6);
    // The client takes the new ID and starts from 0: the next requests are that host.
    let mut next = copy.clone();
    next.hostid = out.host_id;
    next.seqno = 0;
    let again = w.rpc(&next, 2100);
    assert_eq!(again.host_id, out.host_id);
    assert_eq!(again.reply.hostid, 0);
    next.seqno = 1;
    assert_eq!(w.rpc(&next, 2200).host_id, out.host_id);
}

#[test]
fn a_seqno_below_the_stored_one_finds_a_host_by_name_address_os_and_model() {
    let w = World::new("", "");
    let a = computer_with_address(&w);
    // The copy runs on a computer that looks like `a` (`find_host_by_other`): that host is
    // used, whatever CPID the copy has, and the reply names it so that the client takes it.
    let copy = stale(&w, &a);
    let out = w.rpc_xml(&with_host_cpid(&copy, "other"), 2000);
    assert_eq!(out.host_id, a.hostid);
    assert_eq!(out.reply.hostid, a.hostid as u64);
    assert_eq!(w.host(a.hostid).rpc_seqno, 0);
    // The same state file on a client that says it is one of several gets a host of its own.
    let mut shared = stale(&w, &a);
    shared.top_extra = MULTIPLE.into();
    let second = w.rpc(&shared, 2100).host_id;
    assert!(second > 0 && second != a.hostid);
    // One without an address, or on another model, does not look like `a`.
    let mut no_ip = stale(&w, &a);
    no_ip.host_extra = String::new();
    let third = w.rpc(&no_ip, 2200).host_id;
    assert!(third != a.hostid && third != second);
    let mut other_model = stale(&w, &a);
    other_model.model = "ARMv8 Processor".into();
    let fourth = w.rpc(&other_model, 2300).host_id;
    assert!(fourth != a.hostid && fourth != second && fourth != third);
}

#[test]
fn a_host_found_by_name_address_os_and_model_loses_its_jobs_unless_the_client_lists_some() {
    let w = World::new("", "");
    w.app(1, "a", "");
    w.version(11, 1, "a", 100, 1, "");
    w.job("a", "j1", 1000, JobSpec::default());
    w.job("a", "j2", 1000, JobSpec::default());
    let mut a = w.computer("a");
    a.host_extra = IP_A.into();
    a.seqno = 6;
    let first = w.rpc(&a, 1500);
    assert_eq!(first.n_sent, 2, "{:?}", first.reply.messages);
    // A copy of the state file that lists a job it holds: the jobs stay.
    let mut copy = stale(&w, &a);
    copy.work_secs = 0.0;
    copy.top_extra =
        "<other_results>\n<other_result>\n<name>j1_0</name>\n</other_result>\n</other_results>\n"
            .into();
    // (A CPID that no host has, so that only `find_host_by_other` can match.)
    assert_eq!(
        w.rpc_xml(&with_host_cpid(&copy, "new"), 2000).host_id,
        a.hostid
    );
    for name in ["j1_0", "j2_0"] {
        assert_eq!(
            w.result_named(name).server_state,
            RESULT_SERVER_STATE_IN_PROGRESS
        );
    }
    // One that lists none: the jobs are written off, as `mark_results_over` does.
    let mut copy = stale(&w, &a);
    copy.work_secs = 0.0;
    assert_eq!(
        w.rpc_xml(&with_host_cpid(&copy, "newer"), 2100).host_id,
        a.hostid
    );
    for name in ["j1_0", "j2_0"] {
        let r = w.result_named(name);
        assert_eq!(r.server_state, RESULT_SERVER_STATE_OVER, "{name}");
        assert_eq!(r.outcome, RESULT_OUTCOME_CLIENT_DETACHED, "{name}");
    }
}

/// A computer of another account, known to the project.
const THEIRS: i64 = 500;

fn host_of_another_account(w: &World) {
    w.sql("INSERT INTO user (id, create_time, email_addr, name, authenticator, cross_project_id) VALUES (2, 0, 'v@example.com', 'v', 'auth2', 'cpid2')");
    w.sql("INSERT INTO host (id, create_time, userid, rpc_seqno, host_cpid, domain_name, last_ip_addr, os_name, p_model, m_nbytes) VALUES (500, 0, 2, 0, 'x', 'a', '198.51.100.7', 'Linux', 'Intel(R) Xeon(R) CPU E5-2680 v4', 8e9)");
}

#[test]
fn a_host_id_of_another_account_makes_a_new_host_even_when_the_cpid_matches_one_of_the_account() {
    let w = World::new("", "");
    host_of_another_account(&w);
    let a = twin(&w, "a");
    // The account's own host `a` has the request's CPID and no address is sent, so only
    // `find_host_by_cpid` could match it: stock goes to `didnt_find_host` and skips it.
    let mut req = a.clone();
    req.hostid = THEIRS;
    req.seqno = 7;
    let out = w.rpc(&req, 2000);
    assert!(out.host_id > 0 && out.host_id != THEIRS && out.host_id != a.hostid);
    assert_eq!(out.reply.hostid, out.host_id as u64);
    let made = w.host(out.host_id);
    assert_eq!(made.userid, 1);
    assert_eq!(made.rpc_seqno, 0);
    // The other account's host is not touched.
    let other = w.host(THEIRS);
    assert_eq!(other.userid, 2);
    assert_eq!(other.rpc_time, 0);
}

#[test]
fn a_host_id_of_another_account_finds_a_host_by_name_address_os_and_model() {
    let w = World::new("", "");
    host_of_another_account(&w);
    let a = computer_with_address(&w);
    let mut req = a.clone();
    req.hostid = THEIRS;
    req.seqno = 7;
    let out = w.rpc_xml(&with_host_cpid(&req, "other"), 2000);
    // The account's own host, not the other's, though both look like the request.
    assert_eq!(out.host_id, a.hostid);
    assert_eq!(out.reply.hostid, a.hostid as u64);
    assert_eq!(w.host(THEIRS).rpc_time, 0);
}

#[test]
fn a_weak_authenticator_is_not_taken_for_a_host_of_another_account() {
    let w = World::new("", "");
    host_of_another_account(&w);
    let weak = cb_crypto::account::weak_auth(1, "auth1", "");
    let mut req = w.computer("a");
    req.work_secs = 0.0;
    let own = req.hostid;
    let with_weak = |r: &Req| {
        r.xml()
            .replace("<authenticator>auth1<", &format!("<authenticator>{weak}<"))
    };
    req.hostid = THEIRS;
    let out = w.rpc_xml(&with_weak(&req), 2000);
    assert_eq!(out.host_id, 0);
    assert!(has_message(&out, "Invalid or missing account key"));
    // With a host ID of its own account the weak authenticator is accepted.
    req.hostid = own;
    assert_eq!(w.rpc_xml(&with_weak(&req), 2100).host_id, own);
}

#[test]
fn an_unknown_host_id_is_a_request_with_none_and_the_client_is_told() {
    // The CPID is looked up first, as for no host ID.
    let w = World::new("", "");
    let a = computer_with_address(&w);
    let b = twin(&w, "b");
    let mut with_cpid_of_b = a.clone();
    with_cpid_of_b.hostid = 9999;
    with_cpid_of_b.seqno = 0;
    let out = w.rpc_xml(&with_host_cpid(&with_cpid_of_b, "b"), 2000);
    assert_eq!(out.host_id, b.hostid);
    assert_eq!(out.reply.hostid, b.hostid as u64);
    let note = out
        .reply
        .messages
        .iter()
        .find(|m| m.message == "Can't find host record")
        .expect("the message of `authenticate_user`");
    assert_eq!(note.priority, "low");
    // Then the other lookup: a CPID nobody has, on the computer of `a`.
    let w = World::new("", "");
    let a = computer_with_address(&w);
    let mut moved = a.clone();
    moved.hostid = 9999;
    moved.seqno = 0;
    assert_eq!(
        w.rpc_xml(&with_host_cpid(&moved, "other"), 2000).host_id,
        a.hostid
    );
    // Neither: a new host.
    let w = World::new("", "");
    let a = computer_with_address(&w);
    let mut stranger = a.clone();
    stranger.host_extra = String::new();
    stranger.cpid = "q".into();
    stranger.hostid = 9999;
    stranger.seqno = 0;
    let out = w.rpc(&stranger, 2000);
    assert!(out.host_id > 0 && out.host_id != a.hostid && out.host_id != 9999);
    assert!(has_message(&out, "Can't find host record"));
}

#[test]
fn no_host_id_looks_up_the_cpid_and_then_the_other_host() {
    let none = |w: &World, a: &Req, cpid: &str, ip: bool| -> SchedOutcome {
        let mut r = a.clone();
        r.hostid = 0;
        r.seqno = 0;
        r.host_extra = if ip { IP_A.into() } else { String::new() };
        w.rpc_xml(&with_host_cpid(&r, cpid), 2000)
    };
    // The CPID of `b`, with the address and name of `a`: the CPID is looked up first.
    let w = World::new("", "");
    let a = computer_with_address(&w);
    let b = twin(&w, "b");
    let out = none(&w, &a, "b", true);
    assert_eq!(out.host_id, b.hostid);
    assert!(!has_message(&out, "Can't find host record"));
    // A CPID nobody has, with the name, address, OS, model and memory of `a`.
    let w = World::new("", "");
    let a = computer_with_address(&w);
    let _b = twin(&w, "b");
    assert_eq!(none(&w, &a, "other", true).host_id, a.hostid);
    // Neither matches.
    let c = none(&w, &a, "c", false).host_id;
    assert!(c > 0 && c != a.hostid);
}

#[test]
fn a_merged_host_is_followed_to_the_host_it_became() {
    let w = World::new("", "");
    let a = twin(&w, "a");
    // Hosts merged on the web keep the row with no user and the new host's ID in `rpc_seqno`.
    w.sql(&format!("INSERT INTO host (id, create_time, userid, rpc_seqno) VALUES (600, 0, 0, {}), (601, 0, 0, 600)", a.hostid));
    let mut req = a.clone();
    req.hostid = 601;
    req.seqno = 9;
    let out = w.rpc(&req, 2000);
    assert_eq!(out.host_id, a.hostid);
    // The client is told its new ID.
    assert_eq!(out.reply.hostid, a.hostid as u64);
    assert!(!has_message(&out, "Can't find host record"));
}

#[test]
fn a_chain_of_merged_hosts_that_is_too_long_or_broken_is_an_unknown_host_id() {
    let w = World::new("", "");
    let a = twin(&w, "a");
    // 700 -> 701 -> ... -> 710 -> `a`.
    for i in 0..=10 {
        let next = if i == 10 { a.hostid } else { 701 + i };
        w.sql(&format!(
            "INSERT INTO host (id, create_time, userid, rpc_seqno) VALUES ({}, 0, 0, {next})",
            700 + i
        ));
    }
    let mut req = a.clone();
    req.seqno = 0;
    // Eleven links: `authenticate_user` stops after ten, and the CPID lookup finds `a`.
    req.hostid = 700;
    let out = w.rpc(&req, 2000);
    assert!(has_message(&out, "Can't find host record"));
    assert_eq!(out.host_id, a.hostid);
    // Ten links are followed.
    req.hostid = 701;
    let out = w.rpc(&req, 2100);
    assert!(!has_message(&out, "Can't find host record"));
    assert_eq!(out.host_id, a.hostid);
    // A link to nothing.
    w.sql("INSERT INTO host (id, create_time, userid, rpc_seqno) VALUES (800, 0, 0, 12345)");
    req.hostid = 800;
    assert!(has_message(&w.rpc(&req, 2200), "Can't find host record"));
}

// ---- Alternate platforms ---------------------------------------------------

const I686: &str = "i686-pc-linux-gnu";
const ALT_I686: &str = "<alt_platform>\n<name>i686-pc-linux-gnu</name>\n</alt_platform>\n";
const ALT_X86_64: &str = "<alt_platform>\n<name>x86_64-pc-linux-gnu</name>\n</alt_platform>\n";

/// An app `a` and the platforms Linux x86-64 (1), Windows (2) and Linux i686 (3). The version
/// numbers 11 (i686) and 12 (x86-64) are added by each test.
fn platforms_world(config: &str) -> World {
    let w = World::new("", config);
    w.sql("INSERT INTO platform (id, create_time, name, user_friendly_name) VALUES (3, 0, 'i686-pc-linux-gnu', 'Linux i686'), (4, 0, 'windows_intelx86', 'Windows 32-bit')");
    w.app(1, "a", "");
    w
}

fn platform_of(out: &SchedOutcome) -> Vec<&str> {
    out.reply
        .app_versions
        .iter()
        .map(|v| v.platform.as_str())
        .collect()
}

/// `process_request` in `sched/handle_request.cpp` adds each alternate platform the project has
/// to the client's list, and `get_app_version` scans the whole list.
#[test]
fn a_client_gets_a_version_for_an_alternate_platform_when_its_primary_has_none() {
    let w = platforms_world("");
    w.version(11, 1, "a", 100, 3, "");
    w.unpinned("a", "j1");
    w.unpinned("a", "j2");
    // No alternate: the client's platform has no version.
    let plain = w.computer("a");
    assert_eq!(w.rpc(&plain, 2000).n_sent, 0);
    // The same kind of client, saying it can run i686 too.
    let mut both = w.computer("b");
    both.top_extra = ALT_I686.into();
    both.seqno = 3;
    let out = w.rpc(&both, 2100);
    assert_eq!(out.n_sent, 2);
    assert_eq!(platform_of(&out), [I686]);
    assert_eq!(w.result_named("j1_0").app_version_id, 11);
    // An alternate the project does not have changes nothing.
    let mut unknown = w.computer("c");
    unknown.top_extra = "<alt_platform>\n<name>sparc-sun-solaris</name>\n</alt_platform>\n".into();
    unknown.seqno = 3;
    assert_eq!(w.rpc(&unknown, 2200).n_sent, 0);
}

/// Of versions with the same speed, `get_app_version` keeps the first it meets, which is the one
/// for the client's primary platform; `config.version_select_random_factor` is 0 here.
#[test]
fn of_two_equal_versions_the_primary_platform_wins() {
    let w = platforms_world("");
    // The i686 version is the older row, so that catalog order would pick it.
    w.version(11, 1, "a", 100, 3, "");
    w.version(12, 1, "a", 100, 1, "");
    w.unpinned("a", "j1");
    let mut on64 = w.computer("a");
    on64.top_extra = ALT_I686.into();
    let out = w.rpc(&on64, 2000);
    assert_eq!(out.n_sent, 1);
    assert_eq!(platform_of(&out), [LINUX]);
    w.unpinned("a", "j2");
    let mut on32 = w.computer("b");
    on32.platform = I686.into();
    on32.top_extra = ALT_X86_64.into();
    on32.seqno = 3;
    let out = w.rpc(&on32, 2100);
    assert_eq!(out.n_sent, 1);
    assert_eq!(platform_of(&out), [I686]);
}

/// A version that is faster by the record of the app's other results wins whatever its platform
/// is, unless `prefer_primary_platform` is set, when the scan stops at the first platform that
/// has a usable version.
#[test]
fn prefer_primary_platform_stops_at_the_first_platform_with_a_version() {
    for (config, expect) in [
        ("", I686),
        (
            "<prefer_primary_platform>1</prefer_primary_platform>",
            LINUX,
        ),
    ] {
        let w = platforms_world(config);
        // The i686 version has done twice the work per unit of peak speed.
        w.version_with(11, 1, "a", 100, 3, "", "pfc_n = 1000, pfc_avg = 0.5");
        w.version_with(12, 1, "a", 100, 1, "", "pfc_n = 1000, pfc_avg = 1.0");
        w.unpinned("a", "j1");
        let mut c = w.computer("a");
        c.top_extra = ALT_I686.into();
        let out = w.rpc(&c, 2000);
        assert_eq!(out.n_sent, 1, "{config}");
        assert_eq!(platform_of(&out), [expect], "{config}");
    }
}

/// A job whose memory bound is more than a 32-bit app can address is sent only with a version for
/// a platform with "64" in its name (`get_app_version`, `is_64b_platform`). The limit is
/// 3 GiB for a Linux client and 2 GiB for a Windows client
/// (`max_32b_address_space`).
#[test]
fn a_job_too_big_for_a_32_bit_app_goes_only_with_a_64_bit_version() {
    let big = |gib: f64| JobSpec {
        rsc_memory_bound: Some(gib * 1024.0 * 1024.0 * 1024.0),
        ..Default::default()
    };
    let w = platforms_world("");
    // The 32-bit versions are the faster, so that nothing but the rule keeps them out.
    w.version_with(11, 1, "a", 100, 3, "", "pfc_n = 1000, pfc_avg = 0.5");
    w.version_with(12, 1, "a", 100, 1, "", "pfc_n = 1000, pfc_avg = 1.0");
    w.job("a", "small", 1000, big(0.1));
    w.job("a", "mid", 1000, big(2.5));
    w.job("a", "huge", 1000, big(3.5));
    let mut c = w.computer("a");
    c.top_extra = ALT_I686.into();
    c.ram = 16e9;
    let out = w.rpc(&c, 2000);
    let mut got: Vec<(String, String)> = sent_names(&out)
        .into_iter()
        .zip(out.reply.results.iter())
        .map(|(n, r)| (wu_of(&n), r.platform.clone()))
        .collect();
    got.sort();
    // Up to 3 GiB, the faster 32-bit version is used; above it only the 64-bit one.
    assert_eq!(
        got,
        [
            ("huge".to_string(), LINUX.to_string()),
            ("mid".to_string(), I686.to_string()),
            ("small".to_string(), I686.to_string()),
        ]
    );

    // A client whose only platform is 32-bit gets nothing for the big one.
    let w = platforms_world("");
    w.version(11, 1, "a", 100, 3, "");
    w.job("a", "huge", 1000, big(3.5));
    w.job("a", "mid", 1000, big(2.5));
    let mut c = w.computer("a");
    c.platform = I686.into();
    c.ram = 16e9;
    let out = w.rpc(&c, 2000);
    assert_eq!(
        sent_names(&out)
            .iter()
            .map(|n| wu_of(n))
            .collect::<Vec<_>>(),
        ["mid"]
    );
}

// ---- host.misc -------------------------------------------------------------

#[test]
fn host_misc_holds_what_host_info_json_stores() {
    let w = World::new("", "");
    w.app(1, "a", "");
    w.version(11, 1, "a", 100, 1, "");
    let nvidia = "<coproc_cuda>\n<count>2</count>\n<name>NVIDIA \"GeForce\" RTX 3060 Ti</name>\n<have_cuda>1</have_cuda>\n<peak_flops>1e13</peak_flops>\n<cudaVersion>12020</cudaVersion>\n<drvVersion>53529</drvVersion>\n<totalGlobalMem>8589934592.000000</totalGlobalMem>\n<major>8</major>\n<minor>6</minor>\n<coproc_opencl>\n<name>x</name>\n<opencl_device_version>OpenCL 3.0 CUDA</opencl_device_version>\n</coproc_opencl>\n</coproc_cuda>";
    let mut host = w.computer("a").client(8, 2, 4).coprocs(nvidia);
    host.host_extra = "<docker_version>27.0.3</docker_version>\n<docker_type>1</docker_type>\n<virtualbox_version>7.0.20</virtualbox_version>\n<p_features>fpu vmx sse2</p_features>\n".into();
    host.top_extra = "<dont_use_wsl>1</dont_use_wsl>\n".into();
    host.work_secs = 0.0;
    w.rpc(&host, 2000);
    let misc: serde_json::Value = serde_json::from_str(&w.host(host.hostid).misc).unwrap();
    assert_eq!(misc["client_version"], "8.2.4");
    assert_eq!(misc["gpus"][0]["type"], "nvidia");
    // A quote in a name is escaped, where upstream's text would not parse.
    assert_eq!(misc["gpus"][0]["model"], "NVIDIA \"GeForce\" RTX 3060 Ti");
    assert_eq!(misc["gpus"][0]["count"], 2);
    assert_eq!(misc["gpus"][0]["ram_mb"], 8192);
    assert_eq!(misc["gpus"][0]["driver_version"], "53529");
    assert_eq!(misc["gpus"][0]["opencl_version"], "300");
    assert_eq!(
        misc["docker"],
        serde_json::json!({"version": "27.0.3", "type": 1})
    );
    assert_eq!(
        misc["vbox"],
        serde_json::json!({"version": "7.0.20", "hw_accel": true, "hw_accel_enabled": true})
    );
    assert_eq!(
        misc["config"],
        serde_json::json!({"dont_use_docker": false, "dont_use_wsl": true})
    );
    assert!(misc.get("wsl").is_none());
    // The layout is upstream's, key by key.
    let text = w.host(host.hostid).misc;
    assert!(text.starts_with("{\n    \"client_version\": \"8.2.4\",\n    \"gpus\": [\n        {\n            \"type\": \"nvidia\","));
}

#[test]
fn host_misc_is_bounded_whatever_the_client_sends() {
    let w = World::new("", "");
    w.app(1, "a", "");
    w.version(11, 1, "a", 100, 1, "");
    let long = "x".repeat(60_000);
    let gpus = format!(
        "<coproc_cuda>\n<count>1</count>\n<name>{long}</name>\n<totalGlobalMem>1e9</totalGlobalMem>\n</coproc_cuda>\n<coproc_ati>\n<count>1</count>\n<name>{long}</name>\n<version>{long}</version>\n</coproc_ati>\n<coproc_intel_gpu>\n<count>1</count>\n<coproc_opencl>\n<name>{long}</name>\n<opencl_driver_version>{long}</opencl_driver_version>\n</coproc_opencl>\n</coproc_intel_gpu>\n<coproc_apple_gpu>\n<count>1</count>\n<model>{long}</model>\n</coproc_apple_gpu>\n<coproc>\n<type>x</type>\n<count>1</count>\n<coproc_opencl>\n<vendor>{long}</vendor>\n<name>{long}</name>\n<opencl_device_version>{long}</opencl_device_version>\n</coproc_opencl>\n</coproc>"
    );
    let mut host = w.computer("a").coprocs(&gpus);
    host.work_secs = 0.0;
    w.rpc(&host, 2000);
    let misc = w.host(host.hostid).misc;
    assert!(misc.len() <= cb_core::host_misc::MAX_MISC, "{}", misc.len());
    let v: serde_json::Value = serde_json::from_str(&misc).unwrap();
    assert_eq!(v["client_version"], "8.0.4");
}

// ---- The daily counter under a lost claim ----------------------------------

#[test]
fn a_request_that_loses_a_claim_does_not_count_the_job() {
    let w = World::new(
        "",
        "<one_result_per_host_per_wu>1</one_result_per_host_per_wu>",
    );
    w.app(1, "a", "");
    w.version(11, 1, "a", 100, 1, "");
    w.unpinned("a", "j1");
    let host = w.computer("a");
    // Two requests of one host in the same second read the same unsent
    // result; one claims it.
    let mut other = host.clone();
    other.seqno = 3;
    let (outer, inner) = w.race(&host, &other, 2000);
    assert_eq!(outer.n_sent + inner.n_sent, 1);
    let hav: HostAppVersion = block_on(query_one(
        &w.db,
        stmt!(
            "SELECT * FROM host_app_version WHERE host_id = ? AND app_version_id = 11",
            host.hostid
        ),
    ))
    .unwrap()
    .unwrap();
    assert_eq!(hav.n_jobs_today, 1);
}

// ---- A large backlog -------------------------------------------------------

/// Unsent results of an app, `count` of them, by the pattern of `row`.
struct Bulk {
    app: i64,
    count: usize,
    prefix: &'static str,
    batch: i64,
    app_version_num: i32,
    hr_class: i32,
    memory_bound: f64,
}

impl Bulk {
    fn new(app: i64, prefix: &'static str, count: usize) -> Bulk {
        Bulk {
            app,
            count,
            prefix,
            batch: 0,
            app_version_num: 0,
            hr_class: 0,
            memory_bound: 1e8,
        }
    }
}

/// Inserts workunits with one unsent result each, straight into the tables
/// (a job through `create_work` signs its upload certificates, which is slow
/// for thousands), with the documents of one real job.
fn bulk_insert(w: &World, template: &(String, String), b: &Bulk) {
    let mut stmts = Vec::new();
    for i in 0..b.count {
        let name = format!("{}{i}", b.prefix);
        stmts.push(stmt!(
            "INSERT INTO workunit (create_time, appid, name, xml_doc, batch, rsc_fpops_est, rsc_fpops_bound, rsc_memory_bound, rsc_disk_bound, transition_time, delay_bound, min_quorum, target_nresults, max_error_results, max_total_results, max_success_results, result_template, app_version_num, hr_class) VALUES (1000, ?, ?, ?, ?, 1e12, 1e13, ?, 1e8, 2147483647, 1000000, 1, 1, 3, 10, 6, '', ?, ?)",
            b.app,
            name.as_str(),
            template.0.as_str(),
            b.batch,
            b.memory_bound,
            b.app_version_num,
            b.hr_class
        ));
        stmts.push(stmt!(
            "INSERT INTO result (create_time, workunitid, server_state, name, xml_doc_in, random, appid) VALUES (1000, (SELECT id FROM workunit WHERE name = ?), 2, ?, ?, 1, ?)",
            name.as_str(),
            format!("{name}_0"),
            template.1.as_str(),
            b.app
        ));
    }
    block_on(w.db.batch(stmts)).unwrap();
}

/// The workunit and result documents of a real job, which the rows copy.
fn documents(w: &World, app: &str) -> (String, String) {
    w.unpinned(app, "template");
    let wu = w.wu("template");
    let r = w.result_named("template_0");
    w.sql("DELETE FROM result WHERE name = 'template_0'");
    w.sql("DELETE FROM workunit WHERE name = 'template'");
    (wu.xml_doc, r.xml_doc_in)
}

const LATE_PLAN: &str = "<plan_classes><plan_class><name>late</name><min_wu_id>501</min_wu_id></plan_class><plan_class><name>gpu</name><gpu_type>nvidia</gpu_type></plan_class></plan_classes>";

/// About 2,000 unsent results of five apps. For this host (Linux, Intel, no
/// GPU), the first window of every app holds only results it cannot be sent:
/// of an app with no version for its platform, pinned to a version it lacks,
/// committed to another homogeneous redundancy class, or behind a plan class
/// window that is closed. After them come 500 it can.
#[test]
fn a_host_gets_the_work_it_can_run_from_behind_a_large_backlog() {
    let w = World::new(
        LATE_PLAN,
        "<one_result_per_host_per_wu>1</one_result_per_host_per_wu>",
    );
    w.app(1, "plain", "");
    w.app(2, "winonly", "");
    w.app(3, "pin", "min_version = 100");
    w.app(4, "hr", "homogeneous_redundancy = 2");
    w.app(5, "gate", "");
    w.version(1, 1, "plain", 100, 1, "");
    w.version(2, 2, "winonly", 100, 2, "");
    // The newer version is a GPU one this host cannot use; the older one is
    // not superseded by it, since a plan class has its own latest.
    w.version(3, 3, "pin", 100, 1, "");
    w.version(4, 3, "pin", 101, 1, "gpu");
    w.version(5, 4, "hr", 100, 1, "");
    w.version(6, 5, "gate", 100, 1, "late");
    let template = documents(&w, "plain");
    // The plan class opens at workunit 501. The gate app's first 300
    // workunits are 1 to 300; the rest of its work comes after the other
    // apps', beyond 500.
    bulk_insert(&w, &template, &Bulk::new(5, "gate_closed", 300));
    bulk_insert(&w, &template, &Bulk::new(2, "winonly", 300));
    bulk_insert(
        &w,
        &template,
        &Bulk {
            app_version_num: 101,
            ..Bulk::new(3, "pin_new", 400)
        },
    );
    bulk_insert(&w, &template, &Bulk::new(3, "pin_free", 100));
    bulk_insert(
        &w,
        &template,
        &Bulk {
            app_version_num: 100,
            ..Bulk::new(3, "pin_old", 40)
        },
    );
    bulk_insert(
        &w,
        &template,
        &Bulk {
            hr_class: 384 + 2,
            ..Bulk::new(4, "hr_win", 400)
        },
    );
    bulk_insert(&w, &template, &Bulk::new(4, "hr_free", 60));
    bulk_insert(&w, &template, &Bulk::new(5, "gate_open", 100));
    bulk_insert(&w, &template, &Bulk::new(1, "plain", 300));
    let unsent: i64 = ["plain", "winonly", "pin", "hr", "gate"]
        .iter()
        .map(|a| w.unsent_of(a))
        .sum();
    assert_eq!(unsent, 2000);

    let host = w.computer("a");
    let mut sent = 0;
    let mut requests = 0;
    let mut now = 2000;
    let mut seen = std::collections::HashSet::new();
    loop {
        now += 1000;
        requests += 1;
        let mut req = host.clone();
        req.seqno += requests;
        let out = w.rpc(&req, now);
        for name in sent_names(&out) {
            assert!(seen.insert(name.clone()), "{name} sent twice");
            let wu = wu_of(&name);
            assert!(
                ["gate_open", "plain", "pin_old", "pin_free", "hr_free"]
                    .iter()
                    .any(|p| wu.starts_with(p)),
                "{wu} was sent to a host that cannot run it"
            );
        }
        sent += out.n_sent;
        if out.n_sent == 0 {
            break;
        }
        assert!(requests <= 25, "still sending after {requests} requests");
    }
    // Every job it can run, and none it cannot.
    assert_eq!(sent, 300 + 40 + 100 + 60 + 100);
    // 40 jobs to a request: 15 requests send them and one finds none.
    assert!(requests <= 16, "{requests} requests");
    assert_eq!(w.unsent_of("plain"), 0);
    assert_eq!(w.unsent_of("winonly"), 300);
    assert_eq!(w.unsent_of("pin"), 400);
    assert_eq!(w.unsent_of("hr"), 400);
    assert_eq!(w.unsent_of("gate"), 300);
}

/// The first request already sends work from behind a window of blocked
/// ones: nothing to wait for.
#[test]
fn the_first_request_reaches_work_behind_closed_windows() {
    let w = World::new(LATE_PLAN, "");
    w.app(1, "gate", "");
    w.version(1, 1, "gate", 100, 1, "late");
    let template = documents(&w, "gate");
    bulk_insert(&w, &template, &Bulk::new(1, "closed", 500));
    bulk_insert(&w, &template, &Bulk::new(1, "open", 10));
    let host = w.computer("a");
    let out = w.rpc(&host, 3000);
    assert_eq!(out.n_sent, 10);
}

/// A window of jobs that need more memory than the host has, which the
/// queries do not see: the request reads on, a bounded number of windows.
#[test]
fn jobs_the_host_is_too_small_for_do_not_hide_the_ones_behind_them() {
    let w = World::new("", "");
    w.app(1, "a", "");
    w.version(1, 1, "a", 100, 1, "");
    let template = documents(&w, "a");
    bulk_insert(
        &w,
        &template,
        &Bulk {
            memory_bound: 1e12,
            ..Bulk::new(1, "big", 250)
        },
    );
    bulk_insert(&w, &template, &Bulk::new(1, "small", 20));
    let host = w.computer("a");
    let out = w.rpc(&host, 3000);
    assert_eq!(out.n_sent, 20);
    // Past the windows read in one request (four of 100) the host is told
    // what stopped it, and the next request starts again from the front.
    let w = World::new("", "");
    w.app(1, "a", "");
    w.version(1, 1, "a", 100, 1, "");
    let template = documents(&w, "a");
    bulk_insert(
        &w,
        &template,
        &Bulk {
            memory_bound: 1e12,
            ..Bulk::new(1, "big", 450)
        },
    );
    bulk_insert(&w, &template, &Bulk::new(1, "small", 20));
    let host = w.computer("a");
    let out = w.rpc(&host, 3000);
    assert_eq!(out.n_sent, 0);
    assert!(has_message(&out, "more memory"));
}

/// Workunits the host already holds a result of, with another result unsent
/// (replication 2), in front of ones it does not: with
/// `one_result_per_host_per_wu` the first are not candidates, so they cannot
/// fill the window.
#[test]
fn workunits_the_host_already_has_a_result_of_do_not_fill_the_window() {
    let w = World::new(
        "",
        "<one_result_per_host_per_wu>1</one_result_per_host_per_wu>",
    );
    w.app(1, "a", "");
    w.version(1, 1, "a", 100, 1, "");
    let template = documents(&w, "a");
    let host = w.computer("a");
    bulk_insert(&w, &template, &Bulk::new(1, "held", 250));
    // The host timed out on the first result of each of them.
    w.sql(&format!(
        "INSERT INTO result (create_time, workunitid, server_state, outcome, hostid, userid, name, xml_doc_in, random, appid) \
         SELECT 1000, id, 5, 3, {}, 1, name || '_old', '', 1, appid FROM workunit WHERE name LIKE 'held%'",
        host.hostid
    ));
    bulk_insert(&w, &template, &Bulk::new(1, "free", 20));
    let out = w.rpc(&host, 3000);
    assert_eq!(out.n_sent, 20);
    for name in sent_names(&out) {
        assert!(wu_of(&name).starts_with("free"), "{name}");
    }
    // The ones it holds a result of are left for other hosts.
    assert_eq!(w.unsent_of("a"), 250);
}

// ---- Versions kept only for committed workunits -------------------------------

/// An app with homogeneous app version and versions 100 and 101, for one of
/// three reasons version 100 is not one stock loads: it is superseded and
/// there is no `min_version`, it is below `min_version`, or it is deprecated
/// by hand while `min_version` would keep it. A new workunit pinned to 100 is
/// not sent (stock never loads it); one already committed to it still is
/// (`check_homogeneous_app_version` looks the version up anyway).
fn kept_for_commitments_only(app_extra: &str, old_extra: &str) {
    let w = World::new("", "");
    w.app(1, "h", &format!("homogeneous_app_version = 1{app_extra}"));
    w.version_with(11, 1, "h", 100, 1, "", old_extra);
    w.version(12, 1, "h", 101, 1, "");
    w.pinned("h", "new_old", 100);
    w.pinned("h", "committed_old", 100);
    w.sql("UPDATE workunit SET app_version_id = 11 WHERE name = 'committed_old'");
    w.pinned("h", "new_new", 101);
    w.unpinned("h", "free");
    let host = w.computer("a");
    let out = w.rpc(&host, 2000);
    assert_eq!(
        w.sent_versions(&out),
        vec![
            ("committed_old".into(), 100),
            ("free".into(), 101),
            ("new_new".into(), 101)
        ],
        "{app_extra} {old_extra}"
    );
    assert_eq!(w.wu("new_old").app_version_id, 0);
    assert_eq!(w.unsent_of("h"), 1);
    // Told what it is, not that nothing is there.
    let again = w.rpc(&host, 2100);
    assert_eq!(again.n_sent, 0);
    assert!(has_message(&again, "No tasks sent"));
}

#[test]
fn a_new_job_pinned_to_a_superseded_version_is_not_sent_without_min_version() {
    kept_for_commitments_only("", "");
}

#[test]
fn a_new_job_pinned_to_a_version_below_min_version_is_not_sent() {
    kept_for_commitments_only(", min_version = 101", "");
}

#[test]
fn a_new_job_pinned_to_a_deprecated_version_is_not_sent() {
    kept_for_commitments_only(", min_version = 100", "deprecated = 1");
}

#[test]
fn a_pinned_job_goes_to_a_superseded_version_that_min_version_keeps() {
    // The control: with `min_version` 100, version 100 is loaded and marked
    // deprecated in memory, which a pinned job accepts.
    let w = World::new("", "");
    w.app(1, "h", "homogeneous_app_version = 1, min_version = 100");
    w.version(11, 1, "h", 100, 1, "");
    w.version(12, 1, "h", 101, 1, "");
    w.pinned("h", "old", 100);
    let host = w.computer("a");
    let out = w.rpc(&host, 2000);
    assert_eq!(w.sent_versions(&out), vec![("old".into(), 100)]);
}

// ---- When the next window is not read ----------------------------------------

/// Reads of a window after the first.
fn next_windows(sql: &[String]) -> usize {
    sql.iter().filter(|s| s.contains("r.priority < ?")).count()
}

/// A resend sends nothing else, so the windows of unsent work behind it are
/// not read, however full the first was.
#[test]
fn a_resend_sends_only_the_resent_tasks() {
    let w = World::new("", "<resend_lost_results>1</resend_lost_results>");
    w.app(1, "a", "");
    w.version(1, 1, "a", 100, 1, "");
    let template = documents(&w, "a");
    bulk_insert(&w, &template, &Bulk::new(1, "j", 1));
    let host = w.computer("a");
    let out = w.rpc(&host, 3000);
    assert_eq!(out.n_sent, 1);
    // More than two windows of unsent work, and a client that lost its task.
    bulk_insert(&w, &template, &Bulk::new(1, "more", 250));
    let mut lost = host.clone();
    lost.seqno = 3;
    lost.top_extra = "<other_results>\n</other_results>\n".into();
    let (out, sql) = w.rpc_recorded(&lost, 3100);
    assert_eq!(out.n_resent, 1);
    assert_eq!(out.n_sent, 0);
    assert_eq!(next_windows(&sql), 0);
    assert_eq!(w.unsent_of("a"), 250);
}

/// The ways a host stops getting work, each with more than a window of
/// unsent work waiting: no further window is read, since no job could be
/// taken from it.
#[test]
fn nothing_is_read_past_the_first_window_when_no_job_can_be_taken() {
    // The daily limit is used up by the first window's jobs.
    let w = World::new("", "<daily_result_quota>1</daily_result_quota>");
    w.app(1, "a", "");
    w.version(1, 1, "a", 100, 1, "");
    let template = documents(&w, "a");
    bulk_insert(&w, &template, &Bulk::new(1, "j", 450));
    let host = w.computer("a");
    let (out, sql) = w.rpc_recorded(&host, 3000);
    // One job a day for each of the host's four CPUs.
    assert_eq!(out.n_sent, 4);
    assert_eq!(next_windows(&sql), 0);

    // The limit on jobs in progress is reached.
    let w = World::new("", "<max_wus_in_progress>1</max_wus_in_progress>");
    w.app(1, "a", "");
    w.version(1, 1, "a", 100, 1, "");
    let template = documents(&w, "a");
    bulk_insert(&w, &template, &Bulk::new(1, "j", 450));
    let host = w.computer("a");
    let (out, sql) = w.rpc_recorded(&host, 3000);
    assert_eq!(out.n_sent, 4);
    assert_eq!(next_windows(&sql), 0);

    // No disk is left: every job fails the disk check, and none is looked for
    // behind the first window.
    let w = World::new("", "");
    w.app(1, "a", "");
    w.version(1, 1, "a", 100, 1, "");
    let template = documents(&w, "a");
    bulk_insert(&w, &template, &Bulk::new(1, "j", 450));
    let mut host = w.computer("a");
    host.host_extra = "<d_free>1000</d_free>\n".into();
    let (out, sql) = w.rpc_recorded(&host, 3000);
    assert_eq!(out.n_sent, 0);
    assert!(has_message(&out, "more disk space"));
    assert_eq!(next_windows(&sql), 0);

    // The project sends no jobs, the request asks for none, and a client
    // that is too old gets none: no candidate is read at all.
    for (config, tweak) in [
        ("<dont_send_jobs>1</dont_send_jobs>", 0),
        ("", 1),
        (
            "<min_core_client_version>90000</min_core_client_version>",
            2,
        ),
    ] {
        let w = World::new("", config);
        w.app(1, "a", "");
        w.version(1, 1, "a", 100, 1, "");
        let template = documents(&w, "a");
        bulk_insert(&w, &template, &Bulk::new(1, "j", 450));
        let mut host = w.computer("a");
        host.client = (8, 0, 4);
        if tweak == 1 {
            host.top_extra = "<dont_send_work>1</dont_send_work>\n".into();
        }
        let (out, sql) = w.rpc_recorded(&host, 3000);
        assert_eq!(out.n_sent, 0, "{config}");
        assert!(
            !sql.iter().any(|s| s.contains("r.server_state = 2")),
            "{config}: unsent results were read"
        );
    }

    // The control: with room, the windows are read (a host too small for the
    // jobs in the first two windows gets the small ones behind them).
    let w = World::new("", "");
    w.app(1, "a", "");
    w.version(1, 1, "a", 100, 1, "");
    let template = documents(&w, "a");
    bulk_insert(
        &w,
        &template,
        &Bulk {
            memory_bound: 1e12,
            ..Bulk::new(1, "big", 150)
        },
    );
    bulk_insert(&w, &template, &Bulk::new(1, "small", 5));
    let host = w.computer("a");
    let (out, sql) = w.rpc_recorded(&host, 3000);
    assert_eq!(out.n_sent, 5);
    assert!(next_windows(&sql) >= 1);
}
