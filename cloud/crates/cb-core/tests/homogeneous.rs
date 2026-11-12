//! Homogeneous redundancy (HR) and homogeneous app version (HAV): the
//! scheduler RPC and the job pipeline against SQLite.
//!
//! The behaviour under test is upstream's: `sched/hr.cpp` and
//! `sched/sched_hr.cpp` for the classes, `wu_is_infeasible_fast`,
//! `slow_check` and `update_wu_on_send` for skipping and committing,
//! `get_app_version` and `check_homogeneous_app_version` in
//! `sched/sched_version.cpp` for versions, and `handle_wu` in
//! `sched/transitioner.cpp` for releasing the commitment.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

use async_trait::async_trait;
use cb_core::admin::{job_status, put_app, NewApp};
use cb_core::model::{App, JobResult, Workunit};
use cb_core::pipeline::{transition, Ctx};
use cb_core::plan_class::PlanClassSpecs;
use cb_core::scheduler::{handle_request, Catalog, SchedCtx, SchedOutcome};
use cb_core::work::{create_work, Assign, AssignTarget, JobSpec, Submission};
use cb_core::{Config, CoreResult, FileKind, FileStat, Files, Webhook};
use cb_crypto::Signer;
use cb_db::{query, query_one, stmt, Db, DbResult, Rows, Stmt};
use cb_proto::consts::*;
use cb_sqlite::SqliteDb;
use futures_lite::future::block_on;

const LINUX: &str = "x86_64-pc-linux-gnu";
const INPUT_TEMPLATE: &str = "<input_template>\n<workunit>\n<rsc_fpops_est>1e12</rsc_fpops_est>\n<rsc_memory_bound>1e8</rsc_memory_bound>\n<rsc_disk_bound>1e8</rsc_disk_bound>\n<delay_bound>3600</delay_bound>\n</workunit>\n</input_template>\n";
const RESULT_TEMPLATE: &str = "<output_template>\n<file_info>\n    <name><OUTFILE_0/></name>\n    <generated_locally/>\n    <upload_when_present/>\n    <max_nbytes>5000000</max_nbytes>\n    <url><UPLOAD_URL/></url>\n</file_info>\n<result>\n    <file_ref>\n        <file_name><OUTFILE_0/></file_name>\n        <open_name>out</open_name>\n    </file_ref>\n</result>\n</output_template>\n";
const PLAN_CLASSES: &str = "<plan_classes>\n<plan_class>\n<name>mt</name>\n<min_ncpus>2</min_ncpus>\n<max_threads>8</max_threads>\n</plan_class>\n</plan_classes>\n";

// Apps and their versions; see `World::new`.
/// The version of the HAV app without a plan class, which every host runs,
/// and the multi-threaded one, which needs two CPUs and is faster.
const V_PLAIN: i64 = 10;
const V_MT: i64 = 11;

const LINUX_OS: &str = "Linux";
const WINDOWS_OS: &str = "Microsoft Windows 11";
const INTEL: &str = "GenuineIntel";
const AMD: &str = "AuthenticAMD";
const XEON: &str = "Intel(R) Xeon(R) CPU E5-2680 v4";
const RYZEN: &str = "AMD Ryzen 9 5950X 16-Core Processor";
/// Coarse classes are `os + cpu`: Linux 256, Windows 384, Intel 2.
const LINUX_INTEL_COARSE: i32 = 256 + 2;
const WINDOWS_INTEL_COARSE: i32 = 384 + 2;

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
    /// The anonymous platform's `(app name, version number)`.
    versions: Vec<(String, i32)>,
    /// `(name, client state, exit status)`
    reports: Vec<(String, i32, i32)>,
    /// Names of the results the client holds; `None` leaves the list out.
    other: Option<Vec<String>>,
}

impl Req {
    fn xml(&self) -> String {
        let mut s = format!(
            "<scheduler_request>\n<authenticator>auth1</authenticator>\n<hostid>{}</hostid>\n<rpc_seqno>{}</rpc_seqno>\n<core_client_major_version>8</core_client_major_version>\n<core_client_minor_version>0</core_client_minor_version>\n<core_client_release>4</core_client_release>\n<work_req_seconds>{w}</work_req_seconds>\n<cpu_req_secs>{w}</cpu_req_secs>\n<cpu_req_instances>{}</cpu_req_instances>\n<estimated_delay>0</estimated_delay>\n<platform_name>{}</platform_name>\n",
            self.hostid,
            self.seqno,
            if self.work_secs > 0.0 { 1 } else { 0 },
            self.platform,
            w = self.work_secs,
        );
        s.push_str(&format!(
            "<host_info>\n<domain_name>{c}</domain_name>\n<host_cpid>{c}</host_cpid>\n<p_ncpus>{}</p_ncpus>\n<p_vendor>{}</p_vendor>\n<p_model>{}</p_model>\n<p_fpops>2e9</p_fpops>\n<m_nbytes>8e9</m_nbytes>\n<d_total>1e11</d_total>\n<d_free>5e10</d_free>\n<os_name>{}</os_name>\n<os_version>6.1</os_version>\n</host_info>\n",
            self.ncpus,
            self.vendor,
            self.model,
            self.os,
            c = self.cpid,
        ));
        s.push_str("<app_versions>\n");
        for (app, version) in &self.versions {
            s.push_str(&format!(
                "<app_version>\n<app_name>{app}</app_name>\n<version_num>{version}</version_num>\n<platform>{LINUX}</platform>\n<plan_class></plan_class>\n<avg_ncpus>1</avg_ncpus>\n<flops>1e9</flops>\n</app_version>\n"
            ));
        }
        s.push_str("</app_versions>\n");
        for (name, state, exit) in &self.reports {
            s.push_str(&format!(
                "<result>\n<name>{name}</name>\n<final_cpu_time>90</final_cpu_time>\n<final_elapsed_time>100</final_elapsed_time>\n<exit_status>{exit}</exit_status>\n<state>{state}</state>\n<app_version_num>100</app_version_num>\n</result>\n"
            ));
        }
        if let Some(names) = &self.other {
            s.push_str("<other_results>\n");
            for n in names {
                s.push_str(&format!(
                    "<other_result>\n<name>{n}</name>\n</other_result>\n"
                ));
            }
            s.push_str("</other_results>\n");
        }
        s.push_str("</scheduler_request>\n");
        s
    }

    fn reporting(&self, name: &str, exit_status: i32) -> Req {
        let mut r = self.clone();
        r.work_secs = 0.0;
        r.reports = vec![(name.into(), RESULT_FILES_UPLOADED, exit_status)];
        r
    }

    /// The same computer, as seen by a client that lost its results.
    fn lost(&self) -> Req {
        let mut r = self.clone();
        r.other = Some(Vec::new());
        r
    }
}

struct MemFiles;

#[async_trait(?Send)]
impl Files for MemFiles {
    async fn stat(&self, _kind: FileKind, _name: &str) -> CoreResult<Option<FileStat>> {
        Ok(Some(FileStat {
            nbytes: 3,
            md5: cb_crypto::md5_hex(b"out"),
        }))
    }
    async fn delete(&self, _kind: FileKind, _names: &[String]) -> CoreResult<()> {
        Ok(())
    }
    async fn put(&self, _kind: FileKind, _name: &str, _bytes: Vec<u8>) -> CoreResult<()> {
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
        Err(cb_core::CoreError::Invalid("no webhook".into()))
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

/// A database that counts the round trips and statements sent through it.
struct Counting<'a> {
    inner: &'a SqliteDb,
    batches: Cell<usize>,
    statements: Cell<usize>,
}

#[async_trait(?Send)]
impl<'a> Db for Counting<'a> {
    async fn batch(&self, stmts: Vec<Stmt>) -> DbResult<Vec<Rows>> {
        self.batches.set(self.batches.get() + 1);
        self.statements.set(self.statements.get() + stmts.len());
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
    /// A project with the Linux platform and `anonymous`, four apps, each
    /// with a version for Linux, and one user. `a` uses neither feature, `r`
    /// uses coarse HR, `f` fine HR and `h` HAV, and has two versions, plain
    /// and `mt`. A host gets one result of a workunit.
    fn new() -> World {
        let db = SqliteDb::with_schema().unwrap();
        let mut config =
            Config::parse("<config><master_url>https://p.example/</master_url></config>").unwrap();
        config.long_name = "Test".into();
        config.daily_result_quota = 100;
        config.version_select_random_factor = 0.0;
        config.one_result_per_host_per_wu = true;
        let mut writes = vec![
            stmt!("INSERT INTO platform (id, create_time, name, user_friendly_name) VALUES (1, 0, ?, 'Linux'), (2, 0, 'anonymous', 'anonymous')", LINUX),
            stmt!("INSERT INTO app (id, create_time, name, user_friendly_name, validator, homogeneous_redundancy, homogeneous_app_version) VALUES (1, 0, 'a', 'A', 'trivial', 0, 0), (2, 0, 'r', 'R', 'trivial', 2, 0), (3, 0, 'f', 'F', 'trivial', 1, 0), (4, 0, 'h', 'H', 'trivial', 0, 1)"),
            stmt!("INSERT INTO user (id, create_time, email_addr, name, authenticator, cross_project_id) VALUES (1, 0, 'u@example.com', 'u', 'auth1', 'cpid1')"),
        ];
        for (id, app, name, plan) in [
            (7, 1, "a", ""),
            (8, 2, "r", ""),
            (9, 3, "f", ""),
            (V_PLAIN, 4, "h", ""),
            (V_MT, 4, "h", "mt"),
        ] {
            writes.push(stmt!(
                "INSERT INTO app_version (id, create_time, appid, version_num, platformid, xml_doc, plan_class) VALUES (?, 0, ?, 100, 1, ?, ?)",
                id,
                app,
                format!("<app_version>\n<app_name>{name}</app_name>\n<version_num>100</version_num>\n<file_ref>\n<file_name>{name}_100_{plan}</file_name>\n<main_program/>\n</file_ref>\n</app_version>\n"),
                plan
            ));
        }
        block_on(db.batch(writes)).unwrap();
        World {
            db,
            config,
            plan_classes: PlanClassSpecs::parse(PLAN_CLASSES).unwrap(),
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

    /// A job of an app with `n` results (replication and quorum).
    fn submit(&self, app: &str, name: &str, now: i64, n: i32) -> i64 {
        self.submit_with(app, name, now, n, None)
    }

    fn submit_with(&self, app: &str, name: &str, now: i64, n: i32, assign: Option<Assign>) -> i64 {
        let sub = Submission {
            app: app.into(),
            input_template: INPUT_TEMPLATE.into(),
            result_template: RESULT_TEMPLATE.into(),
            jobs: vec![JobSpec {
                name: name.into(),
                min_quorum: Some(n),
                target_nresults: Some(n),
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

    /// A computer of the user, known to the project, with its next request.
    fn computer(&self, cpid: &str, os: &str, vendor: &str, model: &str, ncpus: i32) -> Req {
        let mut req = Req {
            hostid: 0,
            seqno: 1,
            platform: LINUX.into(),
            ncpus,
            work_secs: 0.0,
            cpid: cpid.into(),
            os: os.into(),
            vendor: vendor.into(),
            model: model.into(),
            versions: Vec::new(),
            reports: Vec::new(),
            other: None,
        };
        let out = self.rpc(&req, 500);
        assert_eq!(out.n_sent, 0);
        req.hostid = out.host_id;
        req.seqno = 2;
        req.work_secs = 1e6;
        req
    }

    fn linux_intel(&self, cpid: &str) -> Req {
        self.computer(cpid, LINUX_OS, INTEL, XEON, 4)
    }

    fn windows_intel(&self, cpid: &str) -> Req {
        self.computer(cpid, WINDOWS_OS, INTEL, "Intel(R) Core(TM) i7-9700K", 4)
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
        let catalog = block_on(Catalog::load(&self.db)).unwrap();
        let random = || self.rand();
        let ctx = self.ctx(&self.db, &catalog, &random, now);
        block_on(handle_request(&ctx, &req.xml())).unwrap()
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

    /// `outer` reads, then `sql` is run, then `outer` writes: another
    /// scheduler's commit landing between the two.
    fn race_sql(&self, outer: &Req, sql: &str, now: i64) -> SchedOutcome {
        let catalog = block_on(Catalog::load(&self.db)).unwrap();
        let random = || self.rand();
        let db = Interleave {
            inner: &self.db,
            before_write: RefCell::new(Some(Box::pin(async {
                self.db.run(Stmt::new(sql)).await.unwrap();
                SchedOutcome::default()
            }))),
            other: RefCell::new(None),
        };
        let ctx = self.ctx(&db, &catalog, &random, now);
        block_on(handle_request(&ctx, &outer.xml())).unwrap()
    }

    fn transition(&self, wu: i64, now: i64) {
        let random = || self.rand();
        let ctx = Ctx {
            db: &self.db,
            files: &MemFiles,
            webhook: &NoWebhook,
            config: &self.config,
            signer: Some(signer()),
            now,
            random: &random,
        };
        block_on(transition(&ctx, wu)).unwrap();
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

    /// The versions the results of a workunit that were sent went with,
    /// sorted. Which unsent result of a workunit goes first is not fixed.
    fn versions_of(&self, wu: i64) -> Vec<i64> {
        let mut v: Vec<i64> = self
            .results(wu)
            .iter()
            .filter(|r| r.server_state != RESULT_SERVER_STATE_UNSENT)
            .map(|r| r.app_version_id)
            .collect();
        v.sort_unstable();
        v
    }

    /// The hosts the results of a workunit that were sent went to, sorted.
    fn hosts_of(&self, wu: i64) -> Vec<i64> {
        let mut v: Vec<i64> = self
            .results(wu)
            .iter()
            .filter(|r| r.hostid > 0)
            .map(|r| r.hostid)
            .collect();
        v.sort_unstable();
        v
    }

    fn unsent(&self, wu: i64) -> usize {
        self.results(wu)
            .iter()
            .filter(|r| r.server_state == RESULT_SERVER_STATE_UNSENT)
            .count()
    }

    fn result_named(&self, name: &str) -> JobResult {
        block_on(query_one(
            &self.db,
            stmt!("SELECT * FROM result WHERE name = ?", name),
        ))
        .unwrap()
        .unwrap()
    }
}

fn has_message(out: &SchedOutcome, text: &str) -> bool {
    out.reply.messages.iter().any(|m| m.message.contains(text))
}

fn sent_names(out: &SchedOutcome) -> Vec<String> {
    out.reply
        .results
        .iter()
        .map(|r| {
            // The result's own name, not the output file's.
            let s = &r.xml_doc_in;
            let result = s.find("<result>").unwrap();
            let at = s[result..].find("<name>").unwrap() + result + 6;
            s[at..s[at..].find('<').unwrap() + at].to_string()
        })
        .collect()
}

// ---- Homogeneous redundancy ------------------------------------------------

#[test]
fn the_first_send_commits_a_workunit_to_the_hosts_class() {
    let w = World::new();
    let wu = w.submit("r", "j1", 1000, 2);
    let a = w.linux_intel("a");
    let b = w.windows_intel("b");
    let c = w.computer("c", LINUX_OS, AMD, RYZEN, 4);
    assert_eq!(w.wu(wu).hr_class, 0);

    let out = w.rpc(&a, 2000);
    assert_eq!(out.n_sent, 1);
    assert_eq!(w.wu(wu).hr_class, LINUX_INTEL_COARSE);

    // The second result is for Linux and Intel hosts only. Coarse HR counts
    // AMD as Intel, so the AMD host takes it; the Windows host does not.
    let out = w.rpc(&b, 2100);
    assert_eq!(out.n_sent, 0);
    assert!(has_message(&out, "Tasks are committed to other platforms"));
    assert!(!has_message(&out, "Project has no tasks available"));
    assert_eq!(w.unsent(wu), 1);
    assert_eq!(w.wu(wu).hr_class, LINUX_INTEL_COARSE);

    let out = w.rpc(&c, 2200);
    assert_eq!(out.n_sent, 1);
    assert_eq!(w.unsent(wu), 0);
    assert_eq!(w.hosts_of(wu), vec![a.hostid, c.hostid]);
    assert_eq!(w.wu(wu).hr_class, LINUX_INTEL_COARSE);
}

#[test]
fn a_host_of_another_class_takes_other_uncommitted_work() {
    let w = World::new();
    let wu1 = w.submit("r", "j1", 1000, 2);
    let wu2 = w.submit("r", "j2", 1000, 2);
    let a = w.linux_intel("a");
    let b = w.windows_intel("b");
    // A asks for a lot: one result of each workunit.
    let out = w.rpc(&a, 2000);
    assert_eq!(out.n_sent, 2);
    assert_eq!(w.wu(wu1).hr_class, LINUX_INTEL_COARSE);
    assert_eq!(w.wu(wu2).hr_class, LINUX_INTEL_COARSE);
    // A third workunit, uncommitted: B takes it and commits it to its own
    // class, and the replicas of the first two stay for Linux hosts.
    let wu3 = w.submit("r", "j3", 1100, 2);
    let out = w.rpc(&b, 2100);
    assert_eq!(out.n_sent, 1);
    assert_eq!(w.wu(wu3).hr_class, WINDOWS_INTEL_COARSE);
    assert_eq!(w.unsent(wu1), 1);
    assert_eq!(w.unsent(wu2), 1);
}

#[test]
fn fine_classes_tell_amd_from_intel_and_coarse_ones_do_not() {
    let w = World::new();
    let coarse = w.submit("r", "c1", 1000, 2);
    let fine = w.submit("f", "f1", 1000, 2);
    let intel = w.linux_intel("i");
    let amd = w.computer("m", LINUX_OS, AMD, RYZEN, 4);
    // The Intel host takes one result of each workunit.
    let out = w.rpc(&intel, 2000);
    assert_eq!(out.n_sent, 2);
    // Fine: Linux 256 + Xeon 11. Coarse: Linux 256 + Intel 2.
    assert_eq!(w.wu(fine).hr_class, 256 + 11);
    assert_eq!(w.wu(coarse).hr_class, LINUX_INTEL_COARSE);
    // The AMD host is the same coarse class but another fine one (Linux
    // 256 + AMD 3): it gets the coarse replica and not the fine one.
    let out = w.rpc(&amd, 2100);
    assert_eq!(out.n_sent, 1);
    assert!(sent_names(&out)[0].starts_with("c1_"));
    assert_eq!(w.unsent(fine), 1);
    assert_eq!(w.unsent(coarse), 0);
}

#[test]
fn the_project_type_applies_to_apps_without_their_own() {
    let mut w = World::new();
    w.config.homogeneous_redundancy = 2;
    let plain = w.submit("a", "p1", 1000, 2);
    let fine = w.submit("f", "f1", 1000, 2);
    let a = w.linux_intel("a");
    let b = w.windows_intel("b");
    let out = w.rpc(&a, 2000);
    assert_eq!(out.n_sent, 2);
    // `a` has no type of its own and takes the project's, coarse; `f` keeps
    // its own, fine.
    assert_eq!(w.wu(plain).hr_class, LINUX_INTEL_COARSE);
    assert_eq!(w.wu(fine).hr_class, 256 + 11);
    let out = w.rpc(&b, 2100);
    assert_eq!(out.n_sent, 0);
}

#[test]
fn an_app_without_hr_is_not_affected() {
    let w = World::new();
    let wu = w.submit("a", "p1", 1000, 2);
    let a = w.linux_intel("a");
    let b = w.windows_intel("b");
    assert_eq!(w.rpc(&a, 2000).n_sent, 1);
    assert_eq!(w.rpc(&b, 2100).n_sent, 1);
    assert_eq!(w.wu(wu).hr_class, 0);
}

#[test]
fn a_host_hr_cannot_place_gets_nothing_from_the_app() {
    let w = World::new();
    let hr = w.submit("r", "r1", 1000, 1);
    let plain = w.submit("a", "p1", 1000, 1);
    let solaris = w.computer("s", "SunOS Solaris", "sparc", "UltraSPARC", 4);
    // Other apps do not use HR, so the host still gets work from them.
    let out = w.rpc(&solaris, 2000);
    assert_eq!(out.n_sent, 1);
    assert_eq!(w.results(plain)[0].hostid, solaris.hostid);
    assert_eq!(w.unsent(hr), 1);
    assert!(!has_message(&out, "not supported"));

    // When every app uses HR the host is told, and gets no work at all.
    w.sql("UPDATE app SET homogeneous_redundancy = 2");
    let wu = w.submit("a", "p2", 2100, 1);
    let out = w.rpc(&solaris, 2200);
    assert_eq!(out.n_sent, 0);
    assert!(has_message(
        &out,
        "Your computer type is not supported by this project"
    ));
    assert_eq!(w.unsent(wu), 1);
}

#[test]
fn a_candidate_window_full_of_other_classes_does_not_hide_work() {
    let w = World::new();
    // More workunits committed to Windows, ahead in the queue, than one
    // request reads.
    for i in 0..130 {
        w.submit("r", &format!("w{i}"), 1000, 2);
    }
    w.sql("UPDATE workunit SET hr_class = 386");
    let open = w.submit("r", "open", 1100, 2);
    let a = w.linux_intel("a");
    let out = w.rpc(&a, 2000);
    assert_eq!(out.n_sent, 1);
    assert!(sent_names(&out)[0].starts_with("open_"));
    assert_eq!(w.wu(open).hr_class, LINUX_INTEL_COARSE);
}

#[test]
fn a_workunit_whose_results_all_failed_is_released() {
    let w = World::new();
    let wu = w.submit("r", "j1", 1000, 2);
    let a = w.linux_intel("a");
    let b = w.windows_intel("b");
    let out = w.rpc(&a, 2000);
    assert_eq!(out.n_sent, 1);
    let name = sent_names(&out).remove(0);
    assert_eq!(w.rpc(&b, 2050).n_sent, 0);
    assert_eq!(w.wu(wu).hr_class, LINUX_INTEL_COARSE);

    // The result comes back with an error. Nothing else is in progress or
    // succeeded, so the class is released and the Windows host can have it.
    let out = w.rpc(&a.reporting(&name, 1), 2100);
    assert_eq!(out.n_reported, 1);
    w.transition(wu, 2100);
    assert_eq!(w.wu(wu).hr_class, 0);
    assert_eq!(w.rpc(&b, 2200).n_sent, 1);
    assert_eq!(w.wu(wu).hr_class, WINDOWS_INTEL_COARSE);
}

#[test]
fn a_class_is_kept_while_another_result_is_in_progress() {
    let w = World::new();
    let wu = w.submit("r", "j1", 1000, 2);
    let a = w.linux_intel("a");
    let c = w.computer("c", LINUX_OS, AMD, RYZEN, 4);
    let name = sent_names(&w.rpc(&a, 2000)).remove(0);
    assert_eq!(w.rpc(&c, 2050).n_sent, 1);
    // A fails; C still holds the other result.
    w.rpc(&a.reporting(&name, 1), 2100);
    w.transition(wu, 2100);
    assert_eq!(w.wu(wu).hr_class, LINUX_INTEL_COARSE);
}

#[test]
fn a_class_is_kept_while_a_timed_out_result_may_still_return() {
    let w = World::new();
    let wu = w.submit("r", "j1", 1000, 2);
    let a = w.linux_intel("a");
    let c = w.computer("c", LINUX_OS, AMD, RYZEN, 4);
    let name = sent_names(&w.rpc(&a, 2000)).remove(0);
    assert_eq!(w.rpc(&c, 2050).n_sent, 1);
    // A fails. C's result passes its deadline (3600 s) unreported: timed
    // out, but it may still come back, so the class stays.
    w.rpc(&a.reporting(&name, 1), 2100);
    w.transition(wu, 2050 + 3600 + 10);
    let wu_row = w.wu(wu);
    assert_eq!(wu_row.hr_class, LINUX_INTEL_COARSE);
    assert!(w
        .results(wu)
        .iter()
        .any(|r| r.outcome == RESULT_OUTCOME_NO_REPLY));
}

#[test]
fn a_static_class_is_never_released() {
    let mut w = World::new();
    w.config.hr_class_static = true;
    let wu = w.submit("r", "j1", 1000, 2);
    let a = w.linux_intel("a");
    let name = sent_names(&w.rpc(&a, 2000)).remove(0);
    w.rpc(&a.reporting(&name, 1), 2100);
    w.transition(wu, 2100);
    assert_eq!(w.wu(wu).hr_class, LINUX_INTEL_COARSE);
}

#[test]
fn hosts_of_different_classes_cannot_both_commit_an_uncommitted_workunit() {
    let w = World::new();
    let wu = w.submit("r", "j1", 1000, 2);
    let linux = w.linux_intel("a");
    let windows = w.windows_intel("b");
    // Both read the workunit uncommitted. The Windows request writes first.
    let (out_linux, out_windows) = w.race(&linux, &windows, 2000);
    assert_eq!(out_windows.n_sent, 1);
    assert_eq!(out_linux.n_sent, 0);
    assert!(out_linux.reply.results.is_empty());
    assert_eq!(w.wu(wu).hr_class, WINDOWS_INTEL_COARSE);
    assert_eq!(w.hosts_of(wu), vec![windows.hostid]);
    assert_eq!(w.unsent(wu), 1);
}

#[test]
fn a_class_committed_after_the_candidates_were_read_stops_the_claim() {
    // The workunit is uncommitted when the request reads it, and committed to
    // Windows (by a claim of another result) before the request writes. The
    // claim is conditional on the class, so it fails; nothing is sent and the
    // class stays.
    let w = World::new();
    let wu = w.submit("r", "j1", 1000, 2);
    let a = w.linux_intel("a");
    let out = w.race_sql(
        &a,
        &format!("UPDATE workunit SET hr_class = 386 WHERE id = {wu}"),
        2000,
    );
    assert_eq!(out.n_sent, 0);
    assert_eq!(w.wu(wu).hr_class, 386);
    assert_eq!(w.unsent(wu), 2);
}

#[test]
fn hosts_of_one_class_are_not_held_off_by_each_other_in_a_race() {
    let w = World::new();
    let wu = w.submit("r", "j1", 1000, 2);
    let a = w.linux_intel("a");
    let c = w.computer("c", LINUX_OS, AMD, RYZEN, 4);
    // Both may choose the same result, in which case only the first claim
    // holds, as for any work; the class is no reason for a claim to fail.
    let (out_a, out_c) = w.race(&a, &c, 2000);
    assert!(out_a.n_sent + out_c.n_sent >= 1);
    assert_eq!(w.wu(wu).hr_class, LINUX_INTEL_COARSE);
    assert_eq!(w.hosts_of(wu).len(), out_a.n_sent + out_c.n_sent);
}

#[test]
fn a_lost_result_is_resent_to_a_host_still_in_its_class() {
    let mut w = World::new();
    w.config.resend_lost_results = true;
    let wu = w.submit("r", "j1", 1000, 2);
    let a = w.linux_intel("a");
    let first = sent_names(&w.rpc(&a, 2000));
    let out = w.rpc(&a.lost(), 2100);
    assert_eq!(out.n_resent, 1);
    assert_eq!(sent_names(&out), first);
    assert_eq!(w.wu(wu).hr_class, LINUX_INTEL_COARSE);
}

#[test]
fn a_lost_result_is_not_resent_to_a_host_of_another_class() {
    let mut w = World::new();
    w.config.resend_lost_results = true;
    w.submit("r", "j1", 1000, 2);
    let a = w.linux_intel("a");
    let name = sent_names(&w.rpc(&a, 2000)).remove(0);
    // The same host, now reporting another OS: it is not in the class the
    // workunit is committed to, so the result is let go (`wu_is_infeasible_fast`
    // runs for a resend too).
    let mut moved = a.lost();
    moved.os = WINDOWS_OS.into();
    let out = w.rpc(&moved, 2100);
    assert_eq!(out.n_resent, 0);
    assert!(has_message(
        &out,
        &format!("Didn't resend lost task {name} (expired)")
    ));
    assert_eq!(w.result_named(&name).report_deadline, 2100 - 1);
}

#[test]
fn an_assigned_job_follows_the_class_it_was_committed_to() {
    let mut w = World::new();
    w.config.enable_assignment = true;
    let a = w.linux_intel("a");
    let b = w.windows_intel("b");
    let c = w.computer("c", LINUX_OS, AMD, RYZEN, 4);
    // Assigned to the user, two results: one for each of two hosts.
    let wu = w.submit_with(
        "r",
        "j1",
        1000,
        2,
        Some(Assign {
            target: AssignTarget::User,
            id: 1,
        }),
    );
    assert_eq!(w.rpc(&a, 2000).n_sent, 1);
    assert_eq!(w.wu(wu).hr_class, LINUX_INTEL_COARSE);
    // The Windows host would be next in line, but the job is for Linux.
    let out = w.rpc(&b, 2100);
    assert_eq!(out.n_sent, 0);
    assert_eq!(w.results(wu)[1].server_state, RESULT_SERVER_STATE_INACTIVE);
    assert_eq!(w.rpc(&c, 2200).n_sent, 1);
    assert_eq!(w.hosts_of(wu), vec![a.hostid, c.hostid]);
}

#[test]
fn an_assigned_job_claim_race_between_classes_has_one_winner() {
    let mut w = World::new();
    w.config.enable_assignment = true;
    let a = w.linux_intel("a");
    let b = w.windows_intel("b");
    let wu = w.submit_with(
        "r",
        "j1",
        1000,
        2,
        Some(Assign {
            target: AssignTarget::User,
            id: 1,
        }),
    );
    let (out_a, out_b) = w.race(&a, &b, 2000);
    assert_eq!(out_b.n_sent, 1);
    assert_eq!(out_a.n_sent, 0);
    assert_eq!(w.wu(wu).hr_class, WINDOWS_INTEL_COARSE);
}

// ---- Homogeneous app version -----------------------------------------------

#[test]
fn a_workunit_is_committed_to_the_first_version_sent_and_keeps_it() {
    let w = World::new();
    let wu1 = w.submit("h", "j1", 1000, 2);
    // Four CPUs: the multi-threaded version is the fastest.
    let x = w.linux_intel("x");
    // One CPU: only the plain version runs.
    let y = w.computer("y", LINUX_OS, INTEL, XEON, 1);
    let z = w.computer("z", LINUX_OS, INTEL, XEON, 4);

    let out = w.rpc(&x, 2000);
    assert_eq!(out.n_sent, 1);
    assert_eq!(w.versions_of(wu1), vec![V_MT]);
    assert_eq!(w.wu(wu1).app_version_id, V_MT);

    // The plain-version host cannot take the other result of j1; it takes
    // the results of j2 and commits that to the plain version.
    let wu2 = w.submit("h", "j2", 2050, 2);
    let out = w.rpc(&y, 2100);
    assert_eq!(out.n_sent, 1);
    assert!(sent_names(&out)[0].starts_with("j2_"));
    assert_eq!(w.wu(wu2).app_version_id, V_PLAIN);
    assert_eq!(w.versions_of(wu1), vec![V_MT]);

    // The host that runs either takes each remaining result with its
    // workunit's version.
    let out = w.rpc(&z, 2200);
    assert_eq!(out.n_sent, 2);
    assert_eq!(w.versions_of(wu1), vec![V_MT, V_MT]);
    assert_eq!(w.versions_of(wu2), vec![V_PLAIN, V_PLAIN]);
    // The reply carries both versions' files.
    let xml = out.reply.write().unwrap();
    assert!(xml.contains("h_100_mt"), "{xml}");
    assert!(xml.contains("h_100_<"), "{xml}");
}

#[test]
fn hosts_choosing_different_versions_cannot_both_commit_a_workunit() {
    let w = World::new();
    let wu = w.submit("h", "j1", 1000, 2);
    let x = w.linux_intel("x");
    let y = w.computer("y", LINUX_OS, INTEL, XEON, 1);
    // X would use the multi-threaded version, Y the plain one; Y writes first.
    let (out_x, out_y) = w.race(&x, &y, 2000);
    assert_eq!(out_y.n_sent, 1);
    assert_eq!(out_x.n_sent, 0);
    assert_eq!(w.wu(wu).app_version_id, V_PLAIN);
    assert_eq!(w.versions_of(wu), vec![V_PLAIN]);
}

#[test]
fn a_version_committed_after_the_candidates_were_read_stops_the_claim() {
    let w = World::new();
    let wu = w.submit("h", "j1", 1000, 2);
    let x = w.linux_intel("x");
    // X would send the multi-threaded version; the workunit is committed to
    // the plain one first.
    let out = w.race_sql(
        &x,
        &format!("UPDATE workunit SET app_version_id = {V_PLAIN} WHERE id = {wu}"),
        2000,
    );
    assert_eq!(out.n_sent, 0);
    assert_eq!(w.wu(wu).app_version_id, V_PLAIN);
    assert_eq!(w.unsent(wu), 2);
}

#[test]
fn a_deprecated_version_still_serves_its_committed_workunits() {
    let w = World::new();
    let wu1 = w.submit("h", "j1", 1000, 2);
    let x = w.linux_intel("x");
    let z = w.computer("z", LINUX_OS, INTEL, XEON, 4);
    assert_eq!(w.rpc(&x, 2000).n_sent, 1);
    assert_eq!(w.wu(wu1).app_version_id, V_MT);

    // The version is withdrawn. New workunits no longer go with it, but the
    // committed one does, and the reply names the version's files.
    w.sql(&format!(
        "UPDATE app_version SET deprecated = 1 WHERE id = {V_MT}"
    ));
    let wu2 = w.submit("h", "j2", 2100, 1);
    let out = w.rpc(&z, 2200);
    assert_eq!(out.n_sent, 2);
    assert_eq!(w.versions_of(wu1), vec![V_MT, V_MT]);
    assert_eq!(w.versions_of(wu2), vec![V_PLAIN]);
    let xml = out.reply.write().unwrap();
    assert!(xml.contains("h_100_mt"), "{xml}");
}

#[test]
fn a_superseded_version_still_serves_its_committed_workunits() {
    let w = World::new();
    let wu1 = w.submit("h", "j1", 1000, 2);
    let x = w.linux_intel("x");
    let z = w.computer("z", LINUX_OS, INTEL, XEON, 4);
    assert_eq!(w.rpc(&x, 2000).n_sent, 1);
    // A newer version of the same plan class replaces the committed one.
    w.sql(
        "INSERT INTO app_version (id, create_time, appid, version_num, platformid, xml_doc, plan_class) \
         VALUES (12, 0, 4, 101, 1, '<app_version>\n<app_name>h</app_name>\n<version_num>101</version_num>\n<file_ref>\n<file_name>h_101_mt</file_name>\n<main_program/>\n</file_ref>\n</app_version>\n', 'mt')",
    );
    let wu2 = w.submit("h", "j2", 2100, 1);
    let out = w.rpc(&z, 2200);
    assert_eq!(out.n_sent, 2);
    assert_eq!(w.versions_of(wu1), vec![V_MT, V_MT]);
    assert_eq!(w.versions_of(wu2), vec![12]);
}

/// Every version of the HAV app is withdrawn, one way or another, so none is
/// still sent for new work.
fn withdraw_all_versions(w: &World, how: &str) {
    match how {
        "deprecated" => w.sql("UPDATE app_version SET deprecated = 1 WHERE appid = 4"),
        "below the minimum" => w.sql("UPDATE app SET min_version = 200 WHERE id = 4"),
        _ => unreachable!(),
    }
}

#[test]
fn the_catalog_keeps_withdrawn_versions_only_of_live_hav_apps() {
    let w = World::new();
    // App 5 is deprecated and uses HAV, app 6 is live and does not; each has a
    // deprecated version, as app 4 (live, HAV) gets below.
    w.sql(
        "INSERT INTO app (id, create_time, name, user_friendly_name, validator, deprecated, homogeneous_app_version) \
         VALUES (5, 0, 'dead', 'D', 'trivial', 1, 1), (6, 0, 'plain2', 'P', 'trivial', 0, 0)",
    );
    w.sql(
        "INSERT INTO app_version (id, create_time, appid, version_num, platformid, xml_doc, plan_class, deprecated) \
         VALUES (50, 0, 5, 100, 1, '', '', 1), (51, 0, 5, 101, 1, '', '', 0), (60, 0, 6, 100, 1, '', '', 1)",
    );
    w.sql(&format!(
        "UPDATE app_version SET deprecated = 1 WHERE id = {V_MT}"
    ));
    let catalog = block_on(Catalog::load(&w.db)).unwrap();
    let ids = |v: &[cb_core::model::AppVersion]| {
        let mut ids: Vec<i64> = v.iter().map(|v| v.id).collect();
        ids.sort_unstable();
        ids
    };
    // Withdrawn: the live HAV app's version only, not those of a deprecated
    // app or of an app that does not use HAV.
    assert_eq!(ids(&catalog.retired_versions), vec![V_MT]);
    // Not withdrawn: as before, including a version of a deprecated app,
    // which has no app to be sent with.
    assert_eq!(ids(&catalog.app_versions), vec![7, 8, 9, V_PLAIN, 51]);
}

#[test]
fn a_host_with_only_withdrawn_versions_is_still_sent_committed_work() {
    for how in ["deprecated", "below the minimum"] {
        let w = World::new();
        let wu1 = w.submit("h", "j1", 1000, 2);
        let x = w.linux_intel("x");
        let z = w.computer("z", LINUX_OS, INTEL, XEON, 4);
        assert_eq!(w.rpc(&x, 2000).n_sent, 1, "{how}");
        assert_eq!(w.wu(wu1).app_version_id, V_MT);
        // No version is left for new work, in the whole project, but j1 is
        // committed to one.
        w.sql("DELETE FROM app_version WHERE appid <> 4");
        withdraw_all_versions(&w, how);
        let wu2 = w.submit("h", "j2", 2100, 2);
        let out = w.rpc(&z, 2200);
        assert_eq!(out.n_sent, 1, "{how}");
        assert!(sent_names(&out)[0].starts_with("j1_"), "{how}");
        assert_eq!(w.versions_of(wu1), vec![V_MT, V_MT]);
        // The uncommitted workunit is not sent with a withdrawn version.
        assert_eq!(w.unsent(wu2), 2, "{how}");
    }
}

#[test]
fn uncommitted_work_does_not_hide_committed_work_when_every_version_is_withdrawn() {
    for how in ["deprecated", "below the minimum"] {
        for other_app in [false, true] {
            let w = World::new();
            // More uncommitted workunits, ahead in the queue, than one request
            // reads; then one committed to a version that is withdrawn.
            for i in 0..130 {
                w.submit("h", &format!("u{i}"), 1000, 1);
            }
            let committed = w.submit("h", "c1", 1100, 1);
            w.sql(&format!(
                "UPDATE workunit SET app_version_id = {V_MT} WHERE id = {committed}"
            ));
            let plain = other_app.then(|| w.submit("a", "p1", 1100, 1));
            withdraw_all_versions(&w, how);
            let z = w.computer("z", LINUX_OS, INTEL, XEON, 4);
            let out = w.rpc(&z, 2000);
            let mut names = sent_names(&out);
            names.sort();
            // Another app's non-pinned version does not open the HAV app to
            // its uncommitted work.
            let want: Vec<String> = if other_app {
                vec!["c1_0".into(), "p1_0".into()]
            } else {
                vec!["c1_0".into()]
            };
            let got: Vec<String> = names
                .iter()
                .map(|n| n.split("_r").next().unwrap().to_string())
                .collect();
            assert_eq!(got, want, "{how} {other_app}");
            assert_eq!(w.versions_of(committed), vec![V_MT]);
            if let Some(p) = plain {
                assert_eq!(w.versions_of(p), vec![7]);
            }
        }
    }
}

#[test]
fn uncommitted_assignments_do_not_hide_a_committed_one_when_every_version_is_withdrawn() {
    let mut w = World::new();
    w.config.enable_assignment = true;
    let to_user = || {
        Some(Assign {
            target: AssignTarget::User,
            id: 1,
        })
    };
    // More uncommitted assignments than one request looks at, then one
    // committed to a version that is withdrawn.
    for i in 0..25 {
        w.submit_with("h", &format!("u{i}"), 1000, 1, to_user());
    }
    let committed = w.submit_with("h", "c1", 1100, 1, to_user());
    w.sql(&format!(
        "UPDATE workunit SET app_version_id = {V_MT} WHERE id = {committed}"
    ));
    withdraw_all_versions(&w, "deprecated");
    let z = w.computer("z", LINUX_OS, INTEL, XEON, 4);
    let out = w.rpc(&z, 2000);
    assert_eq!(out.n_sent, 1);
    assert!(sent_names(&out)[0].starts_with("c1_"));
    assert_eq!(w.versions_of(committed), vec![V_MT]);

    // With a version still sent for new work, uncommitted assignments are
    // sent as before.
    let w2 = {
        let mut w2 = World::new();
        w2.config.enable_assignment = true;
        w2
    };
    let wu = w2.submit_with("h", "n1", 1000, 1, to_user());
    let z = w2.computer("z", LINUX_OS, INTEL, XEON, 4);
    assert_eq!(w2.rpc(&z, 2000).n_sent, 1);
    assert_eq!(w2.versions_of(wu), vec![V_MT]);
}

#[test]
fn a_workunit_committed_to_a_version_the_host_cannot_run_waits() {
    let w = World::new();
    let wu = w.submit("h", "j1", 1000, 2);
    let x = w.linux_intel("x");
    let y = w.computer("y", LINUX_OS, INTEL, XEON, 1);
    assert_eq!(w.rpc(&x, 2000).n_sent, 1);
    let out = w.rpc(&y, 2100);
    assert_eq!(out.n_sent, 0);
    assert_eq!(w.unsent(wu), 1);
}

#[test]
fn the_commitment_is_ignored_when_the_app_stops_using_hav() {
    let w = World::new();
    let wu = w.submit("h", "j1", 1000, 2);
    let x = w.linux_intel("x");
    let y = w.computer("y", LINUX_OS, INTEL, XEON, 1);
    assert_eq!(w.rpc(&x, 2000).n_sent, 1);
    w.sql("UPDATE app SET homogeneous_app_version = 0 WHERE id = 4");
    // As upstream, `app.homogeneous_app_version` decides, not the column.
    assert_eq!(w.rpc(&y, 2100).n_sent, 1);
    assert_eq!(w.versions_of(wu), vec![V_PLAIN, V_MT]);
}

#[test]
fn a_workunit_whose_results_all_failed_is_released_from_its_version() {
    let w = World::new();
    let wu = w.submit("h", "j1", 1000, 2);
    let x = w.linux_intel("x");
    let y = w.computer("y", LINUX_OS, INTEL, XEON, 1);
    let name = sent_names(&w.rpc(&x, 2000)).remove(0);
    assert_eq!(w.wu(wu).app_version_id, V_MT);
    w.rpc(&x.reporting(&name, 1), 2100);
    w.transition(wu, 2100);
    assert_eq!(w.wu(wu).app_version_id, 0);
    // Free again, a host with another version can take it.
    assert_eq!(w.rpc(&y, 2200).n_sent, 1);
    assert_eq!(w.wu(wu).app_version_id, V_PLAIN);
}

#[test]
fn a_static_commitment_keeps_the_version_too() {
    let mut w = World::new();
    w.config.hr_class_static = true;
    let wu = w.submit("h", "j1", 1000, 2);
    let x = w.linux_intel("x");
    let name = sent_names(&w.rpc(&x, 2000)).remove(0);
    w.rpc(&x.reporting(&name, 1), 2100);
    w.transition(wu, 2100);
    assert_eq!(w.wu(wu).app_version_id, V_MT);
}

#[test]
fn the_anonymous_platform_gets_no_work_of_a_hav_app() {
    let w = World::new();
    let hav = w.submit("h", "j1", 1000, 1);
    let plain = w.submit("a", "p1", 1000, 1);
    let mut anon = w.linux_intel("x");
    anon.platform = "anonymous".into();
    anon.versions = vec![("h".into(), 100), ("a".into(), 100)];
    let out = w.rpc(&anon, 2000);
    // Never, committed or not; the app without HAV is served as before.
    assert_eq!(out.n_sent, 1);
    assert_eq!(sent_names(&out), vec!["p1_0".to_string()]);
    assert_eq!(w.unsent(hav), 1);
    assert_eq!(w.results(plain)[0].app_version_id, ANON_PLATFORM_CPU);
    assert!(!has_message(&out, "usable version of H"));
}

#[test]
fn the_anonymous_platform_is_not_resent_a_hav_result() {
    let mut w = World::new();
    w.config.resend_lost_results = true;
    w.submit("h", "j1", 1000, 1);
    let x = w.linux_intel("x");
    let name = sent_names(&w.rpc(&x, 2000)).remove(0);
    let mut anon = x.lost();
    anon.platform = "anonymous".into();
    anon.versions = vec![("h".into(), 100)];
    let out = w.rpc(&anon, 2100);
    assert_eq!(out.n_resent, 0);
    assert!(has_message(
        &out,
        &format!("Didn't resend lost task {name} (expired)")
    ));
    assert_eq!(w.result_named(&name).report_deadline, 2100 - 1);
}

#[test]
fn a_lost_result_of_a_withdrawn_version_is_resent_with_it() {
    let mut w = World::new();
    w.config.resend_lost_results = true;
    let wu = w.submit("h", "j1", 1000, 2);
    let x = w.linux_intel("x");
    assert_eq!(w.rpc(&x, 2000).n_sent, 1);
    w.sql(&format!(
        "UPDATE app_version SET deprecated = 1 WHERE id = {V_MT}"
    ));
    let out = w.rpc(&x.lost(), 2100);
    assert_eq!(out.n_resent, 1);
    let xml = out.reply.write().unwrap();
    assert!(xml.contains("h_100_mt"), "{xml}");
    assert_eq!(w.versions_of(wu), vec![V_MT]);
}

#[test]
fn an_assigned_job_follows_the_version_it_was_committed_to() {
    let mut w = World::new();
    w.config.enable_assignment = true;
    let x = w.linux_intel("x");
    let y = w.computer("y", LINUX_OS, INTEL, XEON, 1);
    let z = w.computer("z", LINUX_OS, INTEL, XEON, 4);
    let wu = w.submit_with(
        "h",
        "j1",
        1000,
        2,
        Some(Assign {
            target: AssignTarget::User,
            id: 1,
        }),
    );
    assert_eq!(w.rpc(&x, 2000).n_sent, 1);
    assert_eq!(w.wu(wu).app_version_id, V_MT);
    // The one-CPU host has no use of that version.
    assert_eq!(w.rpc(&y, 2100).n_sent, 0);
    assert_eq!(w.rpc(&z, 2200).n_sent, 1);
    assert_eq!(w.versions_of(wu), vec![V_MT, V_MT]);
}

// ---- Cost ------------------------------------------------------------------

/// One request that is sent one job: round trips and statements.
fn cost(app: &str) -> (usize, usize) {
    let w = World::new();
    w.submit(app, "j1", 1000, 2);
    let req = w.linux_intel("a");
    let catalog = block_on(Catalog::load(&w.db)).unwrap();
    let random = || w.rand();
    let db = Counting {
        inner: &w.db,
        batches: Cell::new(0),
        statements: Cell::new(0),
    };
    let ctx = w.ctx(&db, &catalog, &random, 2000);
    let out = block_on(handle_request(&ctx, &req.xml())).unwrap();
    assert_eq!(out.n_sent, 1);
    (db.batches.get(), db.statements.get())
}

#[test]
fn hr_and_hav_add_no_round_trips_or_statements_to_a_request_that_sends_work() {
    let plain = cost("a");
    assert_eq!(cost("r"), plain, "coarse HR");
    assert_eq!(cost("f"), plain, "fine HR");
    assert_eq!(cost("h"), plain, "HAV");
}

/// Round trips of one request from a new Windows host to a project with
/// nothing unsent, with and without HR apps.
fn idle_cost(hr: bool) -> usize {
    let w = World::new();
    if !hr {
        w.sql("UPDATE app SET homogeneous_redundancy = 0");
    }
    let b = w.windows_intel("b");
    let catalog = block_on(Catalog::load(&w.db)).unwrap();
    let random = || w.rand();
    let db = Counting {
        inner: &w.db,
        batches: Cell::new(0),
        statements: Cell::new(0),
    };
    let ctx = w.ctx(&db, &catalog, &random, 2100);
    let out = block_on(handle_request(&ctx, &b.xml())).unwrap();
    assert_eq!(out.n_sent, 0);
    db.batches.get()
}

#[test]
fn a_request_that_finds_nothing_adds_one_round_trip_for_the_hr_message() {
    assert_eq!(idle_cost(true), idle_cost(false) + 1);
}

// ---- App settings ----------------------------------------------------------

fn app_named(w: &World, name: &str) -> App {
    block_on(query_one(
        &w.db,
        stmt!("SELECT * FROM app WHERE name = ?", name),
    ))
    .unwrap()
    .unwrap()
}

#[test]
fn app_settings_are_kept_when_a_put_leaves_them_out() {
    let w = World::new();
    let put = |a: NewApp| block_on(put_app(&w.db, 3000, &a)).unwrap();
    // `r` has coarse HR; a put that changes something else keeps it.
    put(NewApp {
        name: "r".into(),
        assimilate_url: "https://example.org/hook".into(),
        ..Default::default()
    });
    let r = app_named(&w, "r");
    assert_eq!(r.homogeneous_redundancy, 2);
    assert_eq!(r.homogeneous_app_version, 0);
    assert_eq!(r.assimilate_url, "https://example.org/hook");
    // Set both, then clear one.
    put(NewApp {
        name: "r".into(),
        homogeneous_redundancy: Some(1),
        homogeneous_app_version: Some(true),
        ..Default::default()
    });
    let r = app_named(&w, "r");
    assert_eq!(
        (r.homogeneous_redundancy, r.homogeneous_app_version),
        (1, 1)
    );
    put(NewApp {
        name: "r".into(),
        homogeneous_redundancy: Some(0),
        ..Default::default()
    });
    let r = app_named(&w, "r");
    assert_eq!(
        (r.homogeneous_redundancy, r.homogeneous_app_version),
        (0, 1)
    );
    // A new app gets none unless it is asked for.
    put(NewApp {
        name: "n".into(),
        ..Default::default()
    });
    let n = app_named(&w, "n");
    assert_eq!(
        (n.homogeneous_redundancy, n.homogeneous_app_version),
        (0, 0)
    );
    put(NewApp {
        name: "m".into(),
        homogeneous_redundancy: Some(2),
        ..Default::default()
    });
    assert_eq!(app_named(&w, "m").homogeneous_redundancy, 2);
}

#[test]
fn an_unknown_hr_type_is_refused() {
    let w = World::new();
    for t in [-1, 3, 100] {
        let e = block_on(put_app(
            &w.db,
            3000,
            &NewApp {
                name: "r".into(),
                homogeneous_redundancy: Some(t),
                ..Default::default()
            },
        ))
        .unwrap_err();
        assert!(e.to_string().contains("homogeneous_redundancy"), "{e}");
    }
    assert_eq!(app_named(&w, "r").homogeneous_redundancy, 2);
}

#[test]
fn job_status_shows_what_a_workunit_is_committed_to() {
    let w = World::new();
    let hav = w.submit("h", "j1", 1000, 2);
    let hr = w.submit("r", "j2", 1000, 2);
    let x = w.linux_intel("x");
    assert_eq!(w.rpc(&x, 2000).n_sent, 2);
    let h = block_on(job_status(&w.db, "j1")).unwrap().unwrap();
    assert_eq!((h.id, h.hr_class, h.app_version_id), (hav, 0, V_MT));
    let r = block_on(job_status(&w.db, "j2")).unwrap().unwrap();
    assert_eq!(
        (r.id, r.hr_class, r.app_version_id),
        (hr, LINUX_INTEL_COARSE, 0)
    );
}
