//! The scheduler RPC and the job pipeline for clients on the anonymous
//! platform (an `app_info.xml`), against SQLite.
//!
//! The behaviour under test is upstream's: `get_app_version_anonymous` and
//! `estimate_flops_anon_platform` in `sched/sched_version.cpp`,
//! `add_wu_to_reply` and `add_result_to_reply` in `sched/sched_send.cpp`,
//! `got_good_result` and `got_bad_result` in `sched/sched_result.cpp`, and
//! the `host_app_version` key `generalized_app_version_id` in
//! `sched/sched_util.h`.

use std::cell::Cell;

use async_trait::async_trait;
use cb_core::model::{anon_gavid, Host, HostAppVersion, JobResult, Workunit};
use cb_core::pipeline::{transition, Ctx};
use cb_core::plan_class::PlanClassSpecs;
use cb_core::scheduler::{handle_request, Catalog, SchedCtx, SchedOutcome};
use cb_core::work::{create_work, JobSpec, Submission};
use cb_core::{Config, CoreResult, FileKind, FileStat, Files, Webhook};
use cb_crypto::Signer;
use cb_db::{query, query_one, stmt, Db};
use cb_proto::consts::*;
use cb_sqlite::SqliteDb;
use futures_lite::future::block_on;

const LINUX: &str = "x86_64-pc-linux-gnu";
const INPUT_TEMPLATE: &str = "<input_template>\n<workunit>\n<rsc_fpops_est>1e12</rsc_fpops_est>\n<rsc_memory_bound>1e8</rsc_memory_bound>\n<rsc_disk_bound>1e8</rsc_disk_bound>\n<delay_bound>36000</delay_bound>\n</workunit>\n</input_template>\n";
const RESULT_TEMPLATE: &str = "<output_template>\n<file_info>\n    <name><OUTFILE_0/></name>\n    <generated_locally/>\n    <upload_when_present/>\n    <max_nbytes>5000000</max_nbytes>\n    <url><UPLOAD_URL/></url>\n</file_info>\n<result>\n    <file_ref>\n        <file_name><OUTFILE_0/></file_name>\n        <open_name>out</open_name>\n    </file_ref>\n</result>\n</output_template>\n";
const APP_VERSION_XML: &str = "<app_version>\n    <app_name>a</app_name>\n    <version_num>100</version_num>\n    <file_ref>\n        <file_name>a_100</file_name>\n        <main_program/>\n    </file_ref>\n</app_version>\n";

fn signer() -> &'static Signer {
    use std::sync::OnceLock;
    static SIGNER: OnceLock<Signer> = OnceLock::new();
    SIGNER.get_or_init(|| {
        let (private, _public) = cb_crypto::generate_key_pair().unwrap();
        Signer::new(&private).unwrap()
    })
}

/// An entry of the client's `<app_versions>`.
#[derive(Clone)]
struct Cav {
    /// Text as it goes into the request's XML, entities and all.
    app: String,
    version: i32,
    platform: String,
    plan_class: String,
    ncpus: f64,
    flops: f64,
    /// `(coprocessor type name, instances)`
    coproc: Option<(&'static str, f64)>,
}

impl Cav {
    fn new(app: &str, version: i32) -> Cav {
        Cav {
            app: app.into(),
            version,
            platform: LINUX.into(),
            plan_class: String::new(),
            ncpus: 1.0,
            flops: 0.0,
            coproc: None,
        }
    }

    fn flops(mut self, flops: f64) -> Cav {
        self.flops = flops;
        self
    }

    fn ncpus(mut self, n: f64) -> Cav {
        self.ncpus = n;
        self
    }

    fn platform(mut self, p: &str) -> Cav {
        self.platform = p.into();
        self
    }

    fn plan_class(mut self, c: &str) -> Cav {
        self.plan_class = c.into();
        self
    }

    fn gpu(mut self, kind: &'static str, n: f64) -> Cav {
        self.coproc = Some((kind, n));
        self
    }

    fn xml(&self) -> String {
        let mut s = format!(
            "<app_version>\n<app_name>{}</app_name>\n<version_num>{}</version_num>\n<platform>{}</platform>\n<plan_class>{}</plan_class>\n<avg_ncpus>{}</avg_ncpus>\n<flops>{}</flops>\n",
            self.app, self.version, self.platform, self.plan_class, self.ncpus, self.flops
        );
        if let Some((kind, n)) = self.coproc {
            s.push_str(&format!(
                "<coproc><type>{kind}</type><count>{n}</count></coproc>\n"
            ));
        }
        s.push_str("</app_version>\n");
        s
    }
}

/// What a test client sends.
#[derive(Clone)]
struct Req {
    hostid: i64,
    seqno: i32,
    platform: String,
    ncpus: i32,
    cpu_secs: f64,
    /// The host's `<p_fpops>`, as text.
    fpops: String,
    /// `(request seconds, peak FLOPS)` of one NVIDIA GPU.
    gpu: Option<(f64, f64)>,
    versions: Vec<Cav>,
    /// `(name, client state, exit status)`
    reports: Vec<(String, i32, i32)>,
    other: Option<Vec<String>>,
}

impl Req {
    fn anon(hostid: i64) -> Req {
        Req {
            hostid,
            seqno: 1,
            platform: "anonymous".into(),
            ncpus: 4,
            cpu_secs: 1000.0,
            fpops: "2e9".into(),
            gpu: None,
            versions: Vec::new(),
            reports: Vec::new(),
            other: None,
        }
    }

    fn versions(mut self, v: Vec<Cav>) -> Req {
        self.versions = v;
        self
    }

    fn fpops(mut self, text: &str) -> Req {
        self.fpops = text.into();
        self
    }

    fn xml(&self) -> String {
        let mut s = format!(
            "<scheduler_request>\n<authenticator>auth1</authenticator>\n<hostid>{}</hostid>\n<rpc_seqno>{}</rpc_seqno>\n<core_client_major_version>8</core_client_major_version>\n<core_client_minor_version>0</core_client_minor_version>\n<core_client_release>4</core_client_release>\n<client_cap_plan_class>1</client_cap_plan_class>\n<work_req_seconds>{w}</work_req_seconds>\n<cpu_req_secs>{w}</cpu_req_secs>\n<cpu_req_instances>{}</cpu_req_instances>\n<estimated_delay>0</estimated_delay>\n<platform_name>{}</platform_name>\n",
            self.hostid,
            self.seqno,
            if self.cpu_secs > 0.0 { 1 } else { 0 },
            self.platform,
            w = self.cpu_secs,
        );
        s.push_str(&format!(
            "<host_info>\n<domain_name>box</domain_name>\n<host_cpid>c0ffee</host_cpid>\n<p_ncpus>{}</p_ncpus>\n<p_vendor>GenuineIntel</p_vendor>\n<p_model>cpu</p_model>\n<p_fpops>{}</p_fpops>\n<m_nbytes>8e9</m_nbytes>\n<d_total>1e11</d_total>\n<d_free>5e10</d_free>\n<os_name>Linux</os_name>\n<os_version>6.1</os_version>\n</host_info>\n",
            self.ncpus, self.fpops
        ));
        if let Some((secs, peak)) = self.gpu {
            s.push_str(&format!(
                "<coprocs>\n<coproc_cuda>\n<count>1</count>\n<name>Test GPU</name>\n<peak_flops>{peak}</peak_flops>\n<req_secs>{secs}</req_secs>\n<req_instances>0</req_instances>\n<estimated_delay>0</estimated_delay>\n</coproc_cuda>\n</coprocs>\n"
            ));
        }
        s.push_str("<app_versions>\n");
        for v in &self.versions {
            s.push_str(&v.xml());
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
                    "<other_result>\n<name>{n}</name>\n<app_version>0</app_version>\n</other_result>\n"
                ));
            }
            s.push_str("</other_results>\n");
        }
        s.push_str("</scheduler_request>\n");
        s
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

struct World {
    db: SqliteDb,
    config: Config,
    plan_classes: PlanClassSpecs,
    seq: Cell<u32>,
}

impl World {
    /// A project with a Linux platform and the `anonymous` platform, apps
    /// `a` (with a version for Linux) and `b` (with none), and one user.
    fn new() -> World {
        let db = SqliteDb::with_schema().unwrap();
        let mut config =
            Config::parse("<config><master_url>https://p.example/</master_url></config>").unwrap();
        config.long_name = "Test".into();
        config.daily_result_quota = 100;
        config.version_select_random_factor = 0.0;
        block_on(db.batch(vec![
            stmt!("INSERT INTO platform (id, create_time, name, user_friendly_name) VALUES (1, 0, ?, 'Linux'), (2, 0, 'anonymous', 'anonymous')", LINUX),
            stmt!("INSERT INTO app (id, create_time, name, user_friendly_name, validator) VALUES (1, 0, 'a', 'App A', 'trivial'), (2, 0, 'b', 'App B', 'trivial')"),
            stmt!("INSERT INTO app_version (id, create_time, appid, version_num, platformid, xml_doc) VALUES (7, 0, 1, 100, 1, ?)", APP_VERSION_XML),
            stmt!("INSERT INTO user (id, create_time, email_addr, name, authenticator, cross_project_id) VALUES (1, 0, 'u@example.com', 'u', 'auth1', 'cpid1')"),
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

    fn sql(&self, sql: &str) {
        block_on(self.db.run(stmt!(sql))).unwrap();
    }

    fn submit_app(&self, app: &str, name: &str, now: i64, memory: Option<f64>) -> i64 {
        let sub = Submission {
            app: app.into(),
            input_template: INPUT_TEMPLATE.into(),
            result_template: RESULT_TEMPLATE.into(),
            jobs: vec![JobSpec {
                name: name.into(),
                min_quorum: Some(1),
                target_nresults: Some(1),
                rsc_memory_bound: memory,
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

    fn submit(&self, name: &str, now: i64) -> i64 {
        self.submit_app("a", name, now, None)
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

    fn host(&self, id: i64) -> Host {
        block_on(query_one(
            &self.db,
            stmt!("SELECT * FROM host WHERE id = ?", id),
        ))
        .unwrap()
        .unwrap()
    }

    fn hav(&self, host: i64, gavid: i64) -> Option<HostAppVersion> {
        block_on(query_one(
            &self.db,
            stmt!(
                "SELECT * FROM host_app_version WHERE host_id = ? AND app_version_id = ?",
                host,
                gavid
            ),
        ))
        .unwrap()
    }

    fn havs(&self, host: i64) -> Vec<i64> {
        let rows: Vec<HostAppVersion> = block_on(query(
            &self.db,
            stmt!(
                "SELECT * FROM host_app_version WHERE host_id = ? ORDER BY app_version_id",
                host
            ),
        ))
        .unwrap();
        rows.into_iter().map(|h| h.app_version_id).collect()
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

fn message<'a>(
    out: &'a SchedOutcome,
    text: &str,
) -> Option<&'a cb_proto::sched_reply::UserMessage> {
    out.reply.messages.iter().find(|m| m.message.contains(text))
}

fn has_message(out: &SchedOutcome, text: &str) -> bool {
    message(out, text).is_some()
}

/// The number in `<tag>...</tag>` of the first workunit of the reply.
fn wu_number(out: &SchedOutcome, tag: &str) -> f64 {
    let wu = &out.reply.wus[0];
    let start = wu.find(&format!("<{tag}>")).unwrap() + tag.len() + 2;
    let end = wu[start..].find('<').unwrap() + start;
    wu[start..end].parse().unwrap()
}

#[test]
fn cpu_version_gets_work_and_nothing_else() {
    let w = World::new();
    let wu = w.submit("j1", 1000);
    let req = Req::anon(0).versions(vec![Cav::new("a", 100)
        .platform("client-built")
        .plan_class("my_class")
        .ncpus(2.0)
        .flops(3e9)]);
    let out = w.rpc(&req, 2000);
    assert_eq!(out.n_sent, 1, "{:?}", out.reply.messages);
    let host = out.host_id;

    // No <app>, <app_version> or version <file_info>: the client has its own.
    assert!(out.reply.apps.is_empty());
    assert!(out.reply.app_versions.is_empty());
    let xml = out.reply.write().unwrap();
    assert!(!xml.contains("<app>"), "{xml}");
    assert!(!xml.contains("<app_version>"), "{xml}");
    assert!(!xml.contains("a_100"), "{xml}");
    assert!(xml.contains("<name>j1_0</name>"));
    assert!(xml.contains("<app_name>a</app_name>"));
    // The result names the client's version, as the client wrote it.
    assert!(
        xml.contains("    <platform>client-built</platform>\n    <version_num>100</version_num>\n    <plan_class>my_class</plan_class>\n</result>"),
        "{xml}"
    );
    // Not scaled: the client has no history here.
    assert_eq!(wu_number(&out, "rsc_fpops_est"), 1e12);

    let r = &w.results(wu)[0];
    assert_eq!(r.server_state, RESULT_SERVER_STATE_IN_PROGRESS);
    assert_eq!(r.hostid, host);
    assert_eq!(r.app_version_id, ANON_PLATFORM_CPU);
    assert_eq!(r.app_version_num, 0);
    // 2 CPUs at the host's 2 GFLOPS each
    assert_eq!(r.flops_estimate, 4e9);

    // One row, for the app and the resource type, not for a version.
    assert_eq!(w.havs(host), vec![1_000_002]);
    let hav = w.hav(host, anon_gavid(1, PROC_TYPE_CPU)).unwrap();
    assert_eq!(hav.n_jobs_today, 1);
    assert_eq!(hav.max_jobs_per_day, 100);
}

#[test]
fn normal_client_is_unchanged_by_its_version_list() {
    // Every client since 6.11 sends its app versions; only the platform name
    // makes it anonymous.
    let w = World::new();
    let wu = w.submit("j1", 1000);
    let mut req = Req::anon(0).versions(vec![Cav::new("a", 555).flops(9e12)]);
    req.platform = LINUX.into();
    let out = w.rpc(&req, 2000);
    assert_eq!(out.n_sent, 1);
    assert_eq!(out.reply.apps.len(), 1);
    assert_eq!(out.reply.app_versions.len(), 1);
    let xml = out.reply.write().unwrap();
    assert!(xml.contains("<file_name>a_100</file_name>"));
    assert!(xml.contains("<version_num>100</version_num>"));
    let r = &w.results(wu)[0];
    assert_eq!(r.app_version_id, 7);
    assert_eq!(w.havs(out.host_id), vec![7]);
}

#[test]
fn anonymous_platform_needs_the_platform_row() {
    let w = World::new();
    w.submit("j1", 1000);
    w.sql("DELETE FROM platform WHERE name = 'anonymous'");
    let req = Req::anon(0).versions(vec![Cav::new("a", 100)]);
    let out = w.rpc(&req, 2000);
    assert_eq!(out.n_sent, 0);
    assert!(has_message(
        &out,
        "This project doesn't support computers of type anonymous"
    ));

    // The alternate platforms of an anonymous client do not count.
    let mut xml = req.xml();
    xml = xml.replace(
        "<platform_name>anonymous</platform_name>",
        &format!("<platform_name>anonymous</platform_name>\n<alt_platform><name>{LINUX}</name></alt_platform>"),
    );
    let catalog = block_on(Catalog::load(&w.db)).unwrap();
    let random = || w.rand();
    let ctx = SchedCtx {
        db: &w.db,
        config: &w.config,
        plan_classes: &w.plan_classes,
        catalog: &catalog,
        code_sign_key: "",
        now: 2100,
        random: &random,
        client_ip: "198.51.100.9",
    };
    let out = block_on(handle_request(&ctx, &xml)).unwrap();
    assert_eq!(out.n_sent, 0);
    assert!(has_message(&out, "doesn't support computers of type"));
}

#[test]
fn no_version_of_the_app_means_no_work_and_the_upstream_message() {
    let w = World::new();
    w.submit("j1", 1000);

    // The client has a version of app b only; the job is for app a.
    let req = Req::anon(0).versions(vec![Cav::new("b", 100)]);
    let out = w.rpc(&req, 2000);
    assert_eq!(out.n_sent, 0);
    let m = message(
        &out,
        "Your app_info.xml file doesn't have a usable version of App A.",
    )
    .expect("message");
    assert_eq!(m.priority, "notice");
    assert!(has_message(&out, "No tasks sent"));
    assert!(w.havs(out.host_id).is_empty());

    // An unknown app and a client with no versions at all: the first is
    // named; the second is not told anything about apps (no resource to
    // send work for).
    let req = Req::anon(out.host_id).versions(vec![Cav::new("nonesuch", 100)]);
    let out2 = w.rpc(&req, 2100);
    assert!(has_message(
        &out2,
        "Unknown app name nonesuch in app_info.xml"
    ));
    assert_eq!(out2.n_sent, 0);
    let mut empty = Req::anon(out.host_id);
    empty.seqno = 3;
    let out3 = w.rpc(&empty, 2200);
    assert_eq!(out3.n_sent, 0);
    assert!(!has_message(&out3, "app_info.xml"));
    assert!(has_message(&out3, "No tasks sent"));

    // A version older than the app's minimum is not usable.
    w.sql("UPDATE app SET min_version = 200 WHERE id = 1");
    let mut old = Req::anon(out.host_id).versions(vec![Cav::new("a", 199)]);
    old.seqno = 4;
    let out4 = w.rpc(&old, 2300);
    assert_eq!(out4.n_sent, 0);
    assert!(has_message(
        &out4,
        "doesn't have a usable version of App A."
    ));
    let mut ok = Req::anon(out.host_id).versions(vec![Cav::new("a", 200)]);
    ok.seqno = 5;
    assert_eq!(w.rpc(&ok, 2400).n_sent, 1);
}

#[test]
fn gpu_version_is_used_only_for_a_coprocessor_the_host_has() {
    let w = World::new();
    for i in 0..10 {
        w.submit(&format!("j{i}"), 1000);
    }
    let gpu_version = Cav::new("a", 100).gpu("NVIDIA", 1.0).flops(1e11);

    // No coprocessor in the request: nothing to send a GPU version to.
    let req = Req::anon(0).versions(vec![gpu_version.clone()]);
    let out = w.rpc(&req, 2000);
    assert_eq!(out.n_sent, 0);
    let host = out.host_id;

    // The host has an NVIDIA GPU but the version is for AMD.
    let mut req = Req::anon(host).versions(vec![Cav::new("a", 100).gpu("ATI", 1.0).flops(1e11)]);
    req.seqno = 2;
    req.cpu_secs = 0.0;
    req.gpu = Some((1000.0, 5e12));
    let out = w.rpc(&req, 2100);
    assert_eq!(out.n_sent, 0);

    // With the right GPU: 10 s of work each against 25 s requested sends
    // three, and the CPU request (none) is left alone.
    let mut req = Req::anon(host).versions(vec![gpu_version.clone()]);
    req.seqno = 3;
    req.cpu_secs = 0.0;
    req.gpu = Some((25.0, 5e12));
    let out = w.rpc(&req, 2200);
    assert_eq!(out.n_sent, 3, "{:?}", out.reply.messages);
    let rows: Vec<JobResult> = block_on(query(
        &w.db,
        stmt!("SELECT * FROM result WHERE server_state = 4 ORDER BY id"),
    ))
    .unwrap();
    assert_eq!(rows.len(), 3);
    for r in &rows {
        assert_eq!(r.app_version_id, ANON_PLATFORM_NVIDIA);
        // one CPU at 2 GFLOPS plus the GPU at 5 TFLOPS
        assert_eq!(r.flops_estimate, 2e9 + 5e12);
    }
    // The row is the app's, for the NVIDIA resource type.
    assert_eq!(w.havs(host), vec![1_000_003]);
    assert_eq!(w.hav(host, 1_000_003).unwrap().n_jobs_today, 3);
}

#[test]
fn work_is_accounted_against_the_resource_of_the_version_chosen() {
    let w = World::new();
    for i in 0..10 {
        w.submit(&format!("j{i}"), 1000);
    }
    // A CPU version (500 s a job) and a GPU version (10 s a job) of the same
    // app. The faster GPU version goes first until its 25 s are filled
    // (three jobs), then the CPU version fills the 1000 s of CPU request
    // (two jobs).
    let versions = vec![
        Cav::new("a", 100).flops(2e9),
        Cav::new("a", 100).gpu("NVIDIA", 1.0).flops(1e11),
    ];
    let mut req = Req::anon(0).versions(versions);
    req.gpu = Some((25.0, 5e12));
    let out = w.rpc(&req, 2000);
    assert_eq!(out.n_sent, 5, "{:?}", out.reply.messages);
    let kinds: Vec<i64> = block_on(query::<JobResult>(
        &w.db,
        stmt!("SELECT * FROM result WHERE server_state = 4 ORDER BY id"),
    ))
    .unwrap()
    .iter()
    .map(|r| r.app_version_id)
    .collect();
    assert_eq!(
        kinds.iter().filter(|k| **k == ANON_PLATFORM_NVIDIA).count(),
        3
    );
    assert_eq!(kinds.iter().filter(|k| **k == ANON_PLATFORM_CPU).count(), 2);
    assert_eq!(w.havs(out.host_id), vec![1_000_002, 1_000_003]);

    // Asking for CPU work only never uses the GPU version.
    let w = World::new();
    for i in 0..3 {
        w.submit(&format!("j{i}"), 1000);
    }
    let req = Req::anon(0).versions(vec![
        Cav::new("a", 100).flops(2e9),
        Cav::new("a", 100).gpu("NVIDIA", 1.0).flops(1e11),
    ]);
    let out = w.rpc(&req, 2000);
    assert_eq!(out.n_sent, 2);
    assert_eq!(w.havs(out.host_id), vec![1_000_002]);
}

#[test]
fn the_fastest_version_wins_and_the_earlier_of_two_equal_ones() {
    let w = World::new();
    w.submit("j1", 1000);
    // `get_app_version_anonymous` takes the highest projected speed and
    // replaces its choice only by a strictly faster one.
    let req = Req::anon(0).versions(vec![
        Cav::new("a", 100).flops(1e9),
        Cav::new("a", 101).flops(3e9),
        Cav::new("a", 102).flops(3e9),
        Cav::new("a", 103).flops(2e9),
    ]);
    let out = w.rpc(&req, 2000);
    assert_eq!(out.n_sent, 1);
    assert_eq!(out.reply.results[0].version_num, 101);

    // A client that states no speed gets the host's benchmark speed for it:
    // the version with a stated higher speed still wins.
    let w = World::new();
    w.submit("j1", 1000);
    let req = Req::anon(0).versions(vec![Cav::new("a", 100), Cav::new("a", 101).flops(2.5e9)]);
    let out = w.rpc(&req, 2000);
    assert_eq!(out.reply.results[0].version_num, 101);
    let w = World::new();
    w.submit("j1", 1000);
    let req = Req::anon(0).versions(vec![Cav::new("a", 100), Cav::new("a", 101).flops(1.5e9)]);
    let out = w.rpc(&req, 2000);
    assert_eq!(out.reply.results[0].version_num, 100);
}

#[test]
fn a_version_with_no_quota_left_is_passed_over() {
    let w = World::new();
    w.submit("j1", 1000);
    w.submit("j2", 1000);
    w.submit("j3", 1000);
    let fast = Cav::new("a", 101).flops(3e9);
    let slow = Cav::new("a", 100).gpu("NVIDIA", 1.0).flops(1e9);
    let mut one = Req::anon(0).versions(vec![fast.clone()]);
    one.cpu_secs = 300.0;
    let first = w.rpc(&one, 1500);
    assert_eq!(first.n_sent, 1);
    let host = first.host_id;
    // The fast CPU version has no quota left today (one a day per CPU, four
    // CPUs).
    w.sql(&format!(
        "UPDATE host_app_version SET max_jobs_per_day = 1, n_jobs_today = 4 WHERE host_id = {host}"
    ));

    // With another version to use, the exhausted one is skipped.
    let mut req = Req::anon(host).versions(vec![fast.clone(), slow]);
    req.seqno = 2;
    req.gpu = Some((1000.0, 5e12));
    let out = w.rpc(&req, 1600);
    assert_eq!(out.n_sent, 1, "{:?}", out.reply.messages);
    assert_eq!(out.reply.results[0].version_num, 100);
    assert_eq!(
        w.scalar("SELECT count(*) FROM result WHERE app_version_id = -3"),
        1.0
    );

    // With none, there is no work, and the client is told why.
    let mut req = Req::anon(host).versions(vec![fast.clone()]);
    req.seqno = 3;
    let out = w.rpc(&req, 1700);
    assert_eq!(out.n_sent, 0, "{:?}", out.reply.messages);
    assert!(has_message(&out, "Daily job limit (4) has been reached"));
    assert!(has_message(&out, "reached a limit on tasks sent per day"));
    assert_eq!(message(&out, "Daily job limit").unwrap().priority, "notice");
    assert!(has_message(&out, "doesn't have a usable version of App A."));
}

#[test]
fn daily_quota_applies_to_the_anonymous_row() {
    let mut w = World::new();
    w.config.daily_result_quota = 2;
    for i in 0..8 {
        w.submit(&format!("j{i}"), 1000);
    }
    let mut req = Req::anon(0).versions(vec![Cav::new("a", 100).flops(1e12)]);
    req.ncpus = 1;
    req.cpu_secs = 1e6;
    let out = w.rpc(&req, 2000);
    // two a day for the one CPU
    assert_eq!(out.n_sent, 2);
    let host = out.host_id;
    assert_eq!(w.hav(host, 1_000_002).unwrap().n_jobs_today, 2);

    req.hostid = host;
    req.seqno = 2;
    let out = w.rpc(&req, 2100);
    assert_eq!(out.n_sent, 0);
    assert!(has_message(&out, "Daily job limit (2) has been reached"));

    // The next day the count starts again.
    req.seqno = 3;
    let out = w.rpc(&req, 2000 + 86400);
    assert_eq!(out.n_sent, 2);

    // A failure takes one off the limit of this row; a success doubles it.
    let names: Vec<String> = block_on(query::<JobResult>(
        &w.db,
        stmt!("SELECT * FROM result WHERE server_state = 4 ORDER BY id"),
    ))
    .unwrap()
    .into_iter()
    .map(|r| r.name)
    .collect();
    assert_eq!(w.hav(host, 1_000_002).unwrap().max_jobs_per_day, 2);
    let mut report = Req::anon(host).versions(vec![Cav::new("a", 100)]);
    report.cpu_secs = 0.0;
    report.seqno = 4;
    report.reports = vec![(names[0].clone(), RESULT_COMPUTE_ERROR, 195)];
    w.rpc(&report, 2000 + 86500);
    assert_eq!(w.hav(host, 1_000_002).unwrap().max_jobs_per_day, 1);
    report.seqno = 5;
    report.reports = vec![(names[1].clone(), RESULT_FILES_UPLOADED, 0)];
    w.rpc(&report, 2000 + 86600);
    assert_eq!(w.hav(host, 1_000_002).unwrap().max_jobs_per_day, 2);
}

#[test]
fn jobs_in_progress_are_counted_by_resource() {
    let mut w = World::new();
    w.config.max_wus_in_progress = 1;
    w.config.max_wus_in_progress_gpu = 1;
    for i in 0..6 {
        w.submit(&format!("j{i}"), 1000);
    }
    let versions = vec![
        Cav::new("a", 100).flops(2e9),
        Cav::new("a", 100).gpu("NVIDIA", 1.0).flops(1e11),
    ];
    let mut req = Req::anon(0).versions(versions);
    req.ncpus = 1;
    req.cpu_secs = 1e5;
    req.gpu = Some((1e5, 5e12));
    let out = w.rpc(&req, 2000);
    // one on the CPU and one on the GPU
    assert_eq!(out.n_sent, 2, "{:?}", out.reply.messages);
    let mut kinds: Vec<i64> = block_on(query::<JobResult>(
        &w.db,
        stmt!("SELECT * FROM result WHERE server_state = 4"),
    ))
    .unwrap()
    .iter()
    .map(|r| r.app_version_id)
    .collect();
    kinds.sort_unstable();
    assert_eq!(kinds, vec![ANON_PLATFORM_NVIDIA, ANON_PLATFORM_CPU]);

    req.hostid = out.host_id;
    req.seqno = 2;
    let out = w.rpc(&req, 2100);
    assert_eq!(out.n_sent, 0);
    assert!(has_message(&out, "limit on tasks in progress"));
}

#[test]
fn a_lost_result_is_resent_with_the_clients_version() {
    let mut w = World::new();
    w.config.resend_lost_results = true;
    let wu = w.submit("j1", 1000);
    let req = Req::anon(0).versions(vec![Cav::new("a", 100).platform("client-built")]);
    let out = w.rpc(&req, 2000);
    assert_eq!(out.n_sent, 1);
    let host = out.host_id;

    // The client says it holds nothing.
    let mut again = Req::anon(host).versions(vec![Cav::new("a", 100).platform("client-built")]);
    again.seqno = 2;
    again.other = Some(Vec::new());
    let out2 = w.rpc(&again, 2100);
    assert_eq!(out2.n_resent, 1, "{:?}", out2.reply.messages);
    assert!(has_message(&out2, "Resent lost task j1_0"));
    assert!(out2.reply.app_versions.is_empty());
    let xml = out2.reply.write().unwrap();
    assert!(xml.contains("<platform>client-built</platform>"));
    assert_eq!(w.results(wu)[0].app_version_id, ANON_PLATFORM_CPU);
    // A resend does not count against the day's limit again.
    assert_eq!(w.hav(host, 1_000_002).unwrap().n_jobs_today, 1);
}

#[test]
fn a_job_needing_more_than_32_bit_address_space_wants_a_64_bit_version() {
    let w = World::new();
    w.submit_app("a", "big", 1000, Some(3e9));
    let req = Req::anon(0).versions(vec![Cav::new("a", 100).platform("windows_intelx86")]);
    let out = w.rpc(&req, 2000);
    assert_eq!(out.n_sent, 0);
    assert!(has_message(&out, "doesn't have a usable version of App A."));
    let mut req = Req::anon(out.host_id).versions(vec![
        Cav::new("a", 100).platform("windows_intelx86"),
        Cav::new("a", 101).platform("windows_x86_64").flops(1e9),
    ]);
    req.seqno = 2;
    let out = w.rpc(&req, 2100);
    assert_eq!(out.n_sent, 1);
    assert_eq!(out.reply.results[0].version_num, 101);
}

#[test]
fn history_adjusts_the_speed_and_the_workunit_size_sent() {
    let w = World::new();
    w.submit("j1", 1000);
    let first = w.rpc(
        &Req::anon(0).versions(vec![Cav::new("a", 100).flops(1e9)]),
        1500,
    );
    assert_eq!(first.n_sent, 1);
    let host = first.host_id;
    // Elapsed times say 1e8 FLOPS: the client thinks 1e9, so its idea of the
    // job's size is scaled by 10 (`rsc_fpops_scale`).
    w.sql(&format!(
        "UPDATE host_app_version SET et_n = 20, et_avg = 1e-8 WHERE host_id = {host}"
    ));
    w.sql("UPDATE result SET server_state = 2, hostid = 0, userid = 0 WHERE server_state = 4");
    let mut req = Req::anon(host).versions(vec![Cav::new("a", 100).flops(1e9)]);
    req.seqno = 2;
    let out = w.rpc(&req, 2000);
    assert_eq!(out.n_sent, 1);
    assert!((wu_number(&out, "rsc_fpops_est") - 1e13).abs() < 1.0);
    assert!((wu_number(&out, "rsc_fpops_bound") / 10.0 - w.wu(1).rsc_fpops_bound).abs() < 1.0);

    // Capped at 250 times the stated speed, as upstream.
    w.sql(&format!(
        "UPDATE host_app_version SET et_avg = 1e-15 WHERE host_id = {host}"
    ));
    w.sql("UPDATE result SET server_state = 2, hostid = 0, userid = 0 WHERE server_state = 4");
    req.seqno = 3;
    let out = w.rpc(&req, 3000);
    assert_eq!(out.n_sent, 1);
    assert!((wu_number(&out, "rsc_fpops_est") - 1e12 / 250.0).abs() < 1.0);

    // Too few samples: the client's own figure stands.
    w.sql(&format!(
        "UPDATE host_app_version SET et_n = 5 WHERE host_id = {host}"
    ));
    w.sql("UPDATE result SET server_state = 2, hostid = 0, userid = 0 WHERE server_state = 4");
    req.seqno = 4;
    let out = w.rpc(&req, 4000);
    assert_eq!(wu_number(&out, "rsc_fpops_est"), 1e12);
}

#[test]
fn a_reported_result_is_validated_and_credited() {
    let w = World::new();
    let wu1 = w.submit("j1", 1000);
    let wu2 = w.submit("j2", 1000);
    let versions = vec![Cav::new("a", 100).flops(2e9)];
    let out = w.rpc(&Req::anon(0).versions(versions.clone()), 2000);
    assert_eq!(out.n_sent, 2);
    let host = out.host_id;

    let mut report = Req::anon(host).versions(versions);
    report.seqno = 2;
    report.cpu_secs = 0.0;
    report.reports = vec![
        ("j1_0".into(), RESULT_FILES_UPLOADED, 0),
        ("j2_0".into(), RESULT_COMPUTE_ERROR, 195),
    ];
    let out = w.rpc(&report, 2500);
    assert_eq!(out.n_reported, 2);
    assert_eq!(out.transition, vec![wu1, wu2]);

    // Reporting does not change what the result was sent as.
    let good = &w.results(wu1)[0];
    assert_eq!(good.app_version_id, ANON_PLATFORM_CPU);
    assert_eq!(good.outcome, RESULT_OUTCOME_SUCCESS);
    let bad = &w.results(wu2)[0];
    assert_eq!(bad.app_version_id, ANON_PLATFORM_CPU);
    assert_eq!(bad.outcome, RESULT_OUTCOME_CLIENT_ERROR);
    // One success doubles the limit (capped at the project's), one failure
    // takes one off: 100, then 99.
    let hav = w.hav(host, 1_000_002).unwrap();
    assert_eq!(hav.max_jobs_per_day, 99);
    assert_eq!(hav.consecutive_valid, 0);

    // Validation and credit through the pipeline.
    w.transition(wu1, 2500);
    let good = &w.results(wu1)[0];
    assert_eq!(good.validate_state, VALIDATE_STATE_VALID);
    let credit = 2e9 * 100.0 * 200.0 / 86400e9;
    assert!((good.granted_credit - credit).abs() < 1e-9);
    assert!((w.host(host).total_credit - credit).abs() < 1e-9);
    assert!((w.scalar("SELECT total_credit FROM user WHERE id = 1") - credit).abs() < 1e-9);
    let hav = w.hav(host, 1_000_002).unwrap();
    assert_eq!(hav.max_jobs_per_day, 100);
    assert_eq!(hav.consecutive_valid, 1);
    assert_eq!(hav.et_n, 1.0);
    assert_eq!(hav.turnaround_n, 1.0);
    assert!(hav.et_avg > 0.0);
    // Still the only row of the host: no row for a version.
    assert_eq!(w.havs(host), vec![1_000_002]);

    // The failed one was written off when it was reported, and the streak
    // of valid results it ended is the one that was counted.
    w.transition(wu2, 2500);
    assert_eq!(w.results(wu2)[0].validate_state, VALIDATE_STATE_INVALID);
    assert_eq!(w.hav(host, 1_000_002).unwrap().consecutive_valid, 1);

    // The failure got the workunit a replacement; it goes to the same host
    // and, timing out, lowers the row's limit as well.
    let mut again = Req::anon(host).versions(vec![Cav::new("a", 100).flops(2e9)]);
    again.seqno = 3;
    again.cpu_secs = 300.0;
    assert_eq!(w.rpc(&again, 3100).n_sent, 1);
    assert_eq!(w.results(wu2)[1].app_version_id, ANON_PLATFORM_CPU);
    let before = w.hav(host, 1_000_002).unwrap().max_jobs_per_day;
    w.transition(wu2, 3100 + 36000 + 1);
    assert_eq!(w.results(wu2)[1].outcome, RESULT_OUTCOME_NO_REPLY);
    assert_eq!(w.hav(host, 1_000_002).unwrap().max_jobs_per_day, before - 1);
}

/// Statements one request runs, with the client listing `n` versions of two
/// apps; the host is known already, and five jobs are sent.
fn statements_for(n: i32) -> (u64, usize) {
    let w = World::new();
    for i in 0..5 {
        w.submit(&format!("j{i}"), 1000);
    }
    let mut versions = vec![Cav::new("a", 1).flops(1e12), Cav::new("b", 1).flops(1e12)];
    for v in 2..n {
        versions.push(Cav::new(if v % 2 == 0 { "a" } else { "b" }, v).flops(1e9));
    }
    let mut req = Req::anon(0).versions(versions);
    req.cpu_secs = 0.0;
    let host = w.rpc(&req, 2000).host_id;
    req.hostid = host;
    req.seqno = 2;
    req.cpu_secs = 1e6;
    let before = w.db.statements();
    let out = w.rpc(&req, 2100);
    (w.db.statements() - before, out.n_sent)
}

#[test]
fn the_cost_of_a_request_does_not_grow_with_the_version_list() {
    let (few, sent_few) = statements_for(2);
    let (many, sent_many) = statements_for(300);
    assert_eq!(sent_few, 5);
    assert_eq!(sent_many, 5);
    assert_eq!(few, many);
    assert!(many <= 50, "{many} statements");
}

#[test]
fn apps_with_work_and_no_client_version_cost_one_statement() {
    // The probe for apps the client has no version of is one statement, not
    // one per app.
    let w = World::new();
    w.sql("INSERT INTO app (id, create_time, name, user_friendly_name, validator) VALUES (3, 0, 'c', 'App C', 'trivial'), (4, 0, 'd', 'App D', 'trivial')");
    w.submit("j1", 1000);
    w.submit_app("c", "jc", 1000, None);
    let mut req = Req::anon(0).versions(vec![Cav::new("b", 1)]);
    req.cpu_secs = 0.0;
    let host = w.rpc(&req, 2000).host_id;
    req.hostid = host;
    req.seqno = 2;
    req.cpu_secs = 1000.0;
    let before = w.db.statements();
    let out = w.rpc(&req, 2100);
    let used = w.db.statements() - before;
    assert_eq!(out.n_sent, 0);
    // App b has no work: a, c and d are probed together; a and c have some.
    assert!(has_message(&out, "usable version of App A."));
    assert!(has_message(&out, "usable version of App C."));
    assert!(!has_message(&out, "usable version of App D."));
    assert!(!has_message(&out, "Project has no tasks available"));
    assert!(used <= 12, "{used} statements");
}

#[test]
fn a_resent_result_is_recorded_with_the_version_it_is_resent_with() {
    // `resend_lost_work` calls `add_result_to_reply`, which sets the
    // result's `app_version_id` and `flops_estimate` again.
    let mut w = World::new();
    w.config.resend_lost_results = true;
    let wu = w.submit("j1", 1000);
    let out = w.rpc(
        &Req::anon(0).versions(vec![Cav::new("a", 100).flops(2e9)]),
        2000,
    );
    assert_eq!(out.n_sent, 1);
    let host = out.host_id;
    assert_eq!(w.results(wu)[0].app_version_id, ANON_PLATFORM_CPU);
    assert_eq!(w.results(wu)[0].flops_estimate, 2e9);
    assert_eq!(w.havs(host), vec![1_000_002]);

    // The user drops the CPU version and keeps a GPU one. The client lists
    // no tasks, so the result is sent again, now as a GPU job.
    let mut again =
        Req::anon(host).versions(vec![Cav::new("a", 100).gpu("NVIDIA", 1.0).flops(1e11)]);
    again.seqno = 2;
    again.cpu_secs = 0.0;
    again.gpu = Some((1000.0, 5e12));
    again.other = Some(Vec::new());
    let out2 = w.rpc(&again, 2100);
    assert_eq!(out2.n_resent, 1, "{:?}", out2.reply.messages);
    let r = &w.results(wu)[0];
    assert_eq!(r.app_version_id, ANON_PLATFORM_NVIDIA);
    assert_eq!(r.flops_estimate, 2e9 + 5e12);
    // The new row exists, and the resend did not count against the day.
    assert_eq!(w.havs(host), vec![1_000_002, 1_000_003]);
    assert_eq!(w.hav(host, 1_000_003).unwrap().n_jobs_today, 0);
    assert_eq!(w.hav(host, 1_000_002).unwrap().n_jobs_today, 1);

    // Reporting a failure now lowers the GPU row's limit, not the CPU row's.
    let mut report = Req::anon(host).versions(again.versions.clone());
    report.seqno = 3;
    report.cpu_secs = 0.0;
    report.reports = vec![("j1_0".into(), RESULT_COMPUTE_ERROR, 195)];
    w.rpc(&report, 2200);
    assert_eq!(w.hav(host, 1_000_003).unwrap().max_jobs_per_day, 99);
    assert_eq!(w.hav(host, 1_000_002).unwrap().max_jobs_per_day, 100);
}

#[test]
fn a_resend_past_its_deadline_and_a_new_resource_share_one_statement() {
    let mut w = World::new();
    w.config.resend_lost_results = true;
    let wu = w.submit("j1", 1000);
    let out = w.rpc(
        &Req::anon(0).versions(vec![Cav::new("a", 100).flops(2e9)]),
        2000,
    );
    let host = out.host_id;
    // Past the deadline (36000 s after the send) and sent as a GPU job.
    let mut again =
        Req::anon(host).versions(vec![Cav::new("a", 100).gpu("NVIDIA", 1.0).flops(1e11)]);
    again.seqno = 2;
    again.cpu_secs = 0.0;
    again.gpu = Some((1000.0, 5e12));
    again.other = Some(Vec::new());
    let late = 2000 + 36000 + 100;
    let before = w.db.statements();
    let out2 = w.rpc(&again, late);
    let used = w.db.statements() - before;
    assert_eq!(out2.n_resent, 1);
    let r = &w.results(wu)[0];
    assert_eq!(r.app_version_id, ANON_PLATFORM_NVIDIA);
    assert!(r.report_deadline > late);

    // The same without the change of resource: one statement fewer for the
    // row, none for the result's record.
    let mut w2 = World::new();
    w2.config.resend_lost_results = true;
    w2.submit("j1", 1000);
    let out = w2.rpc(
        &Req::anon(0).versions(vec![Cav::new("a", 100).flops(2e9)]),
        2000,
    );
    let mut same = Req::anon(out.host_id).versions(vec![Cav::new("a", 100).flops(2e9)]);
    same.seqno = 2;
    same.other = Some(Vec::new());
    same.cpu_secs = 1000.0;
    let before = w2.db.statements();
    assert_eq!(w2.rpc(&same, late).n_resent, 1);
    let used_same = w2.db.statements() - before;
    assert_eq!(used, used_same + 1, "{used} {used_same}");
}

/// The first result sent in a world with one job.
fn first_sent(w: &World) -> JobResult {
    block_on(query_one(
        &w.db,
        stmt!("SELECT * FROM result WHERE server_state = 4 ORDER BY id"),
    ))
    .unwrap()
    .expect("a result was sent")
}

#[test]
fn what_the_client_states_cannot_inflate_the_speed_a_result_is_credited_for() {
    // CPUs: bounded by the host's four.
    let w = World::new();
    w.submit("j1", 1000);
    let req = Req::anon(0).versions(vec![Cav::new("a", 100).ncpus(1e6).flops(1e9)]);
    assert_eq!(w.rpc(&req, 2000).n_sent, 1);
    assert_eq!(first_sent(&w).flops_estimate, 4.0 * 2e9);

    // GPU count: bounded by the host's one GPU.
    let w = World::new();
    w.submit("j1", 1000);
    let mut req = Req::anon(0).versions(vec![Cav::new("a", 100).gpu("NVIDIA", 1e6).flops(1e11)]);
    req.cpu_secs = 0.0;
    req.gpu = Some((1000.0, 5e12));
    assert_eq!(w.rpc(&req, 2000).n_sent, 1);
    assert_eq!(first_sent(&w).flops_estimate, 2e9 + 5e12);

    // GPU speed: a figure that cannot be right is replaced as the plan class
    // path replaces it (`COPROC::bad_gpu_peak_flops`).
    let w = World::new();
    w.submit("j1", 1000);
    let mut req = Req::anon(0).versions(vec![Cav::new("a", 100).gpu("NVIDIA", 1.0).flops(1e11)]);
    req.cpu_secs = 0.0;
    req.gpu = Some((1000.0, 1e20));
    assert_eq!(w.rpc(&req, 2000).n_sent, 1);
    assert_eq!(first_sent(&w).flops_estimate, 2e9 + 100e9);

    // A fraction of a GPU stays a fraction.
    let w = World::new();
    w.submit("j1", 1000);
    let mut req = Req::anon(0).versions(vec![Cav::new("a", 100).gpu("NVIDIA", 0.5).flops(1e11)]);
    req.cpu_secs = 0.0;
    req.gpu = Some((1000.0, 4e12));
    assert_eq!(w.rpc(&req, 2000).n_sent, 1);
    assert_eq!(first_sent(&w).flops_estimate, 2e9 + 2e12);

    // The stated speed does not enter it at all.
    let w = World::new();
    w.submit("j1", 1000);
    let req = Req::anon(0).versions(vec![Cav::new("a", 100).flops(1e17)]);
    assert_eq!(w.rpc(&req, 2000).n_sent, 1);
    assert_eq!(first_sent(&w).flops_estimate, 2e9);
}

#[test]
fn a_gpu_that_reports_no_number_for_its_speed_gets_the_default() {
    // `nan` passes a test written as `x <= 0 || x > max`; the speed stored
    // for the result would be NaN, which the database keeps as zero, and a
    // result with no speed earns no credit from its run time. `inf` and a
    // negative figure were already replaced.
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0] {
        let w = World::new();
        w.submit("j1", 1000);
        let mut req =
            Req::anon(0).versions(vec![Cav::new("a", 100).gpu("NVIDIA", 1.0).flops(1e11)]);
        req.cpu_secs = 0.0;
        req.gpu = Some((1000.0, bad));
        assert_eq!(w.rpc(&req, 2000).n_sent, 1, "{bad}");
        let sent = first_sent(&w);
        assert_eq!(sent.flops_estimate, 2e9 + 100e9, "{bad}");
    }

    // The same for the host's benchmark, and for one so large that the
    // product with the CPU count overflows.
    for bad in ["nan", "inf", "1e308"] {
        let w = World::new();
        w.submit("j1", 1000);
        let mut req = Req::anon(0).versions(vec![Cav::new("a", 100).ncpus(4.0)]);
        req.fpops = bad.into();
        assert_eq!(w.rpc(&req, 2000).n_sent, 1, "{bad}");
        let est = first_sent(&w).flops_estimate;
        assert!(est.is_finite() && est > 0.0, "{bad}: {est}");
    }
}

#[test]
fn the_sizes_sent_stay_positive_and_finite_for_any_stated_speed() {
    // Stated speeds from absurdly small to absurdly large, with elapsed-time
    // history from absurdly fast to absurdly slow.
    let stated = [1e-300, 1e-9, 1.0, 1e9, 1e300, f64::MAX];
    let history = [1e-300, 1e-15, 1e-8, 1.0, 1e30, 1e300];
    for s in stated {
        for et_avg in history {
            let mut w = World::new();
            // A stated speed of 1e-300 is a very slow job: not one for the
            // deadline check to turn away here.
            w.config.ignore_delay_bound = true;
            w.submit("j1", 1000);
            let first = w.rpc(
                &Req::anon(0).versions(vec![Cav::new("a", 100).flops(s)]),
                1500,
            );
            let host = first.host_id;
            // Give the one job back, and the host some history.
            w.sql(
                "UPDATE result SET server_state = 2, hostid = 0, userid = 0 WHERE server_state = 4",
            );
            w.sql(&format!(
                "UPDATE host_app_version SET et_n = 20, et_avg = {et_avg:e} WHERE host_id = {host}"
            ));
            let mut req = Req::anon(host).versions(vec![Cav::new("a", 100).flops(s)]);
            req.seqno = 2;
            let out = w.rpc(&req, 2000);
            assert_eq!(out.n_sent, 1, "stated {s:e} history {et_avg:e}");
            for tag in ["rsc_fpops_est", "rsc_fpops_bound"] {
                let x = wu_number(&out, tag);
                assert!(
                    x.is_finite() && x > 0.0,
                    "{tag} {x} for stated {s:e} history {et_avg:e}"
                );
            }
        }
    }
}

#[test]
fn a_negative_version_number_is_not_usable() {
    // `cav.version_num < app.min_version`, and the minimum is 0 by default.
    let w = World::new();
    w.submit("j1", 1000);
    let req = Req::anon(0).versions(vec![Cav::new("a", -5)]);
    let out = w.rpc(&req, 2000);
    assert_eq!(out.n_sent, 0);
    assert!(has_message(&out, "doesn't have a usable version of App A."));
}

#[test]
fn a_client_with_only_a_cpu_version_asking_for_gpu_work_is_told_only_no_tasks_sent() {
    let w = World::new();
    w.submit("j1", 1000);
    let mut req = Req::anon(0).versions(vec![Cav::new("a", 100)]);
    req.cpu_secs = 0.0;
    req.gpu = Some((1000.0, 5e12));
    let out = w.rpc(&req, 2000);
    assert_eq!(out.n_sent, 0);
    assert!(has_message(&out, "No tasks sent"));
    assert!(!has_message(&out, "Project has no tasks available"));
    assert!(!has_message(&out, "app_info.xml"));
    // Only "No tasks sent" and what the request itself earns.
    let texts: Vec<&str> = out
        .reply
        .messages
        .iter()
        .map(|m| m.message.as_str())
        .collect();
    assert_eq!(texts, vec!["No tasks sent"]);
}

#[test]
fn versions_sharing_a_row_share_its_quota_within_one_request() {
    // Two CPU versions of one app share the row of the app and the resource.
    // Quota 3 for one CPU; the fast version fills it, and the slow one must
    // not be offered the same jobs again.
    let versions = vec![
        Cav::new("a", 101).flops(1e10),
        Cav::new("a", 100).flops(5e9),
    ];
    let run = |versions: Vec<Cav>| {
        let mut w = World::new();
        w.config.daily_result_quota = 3;
        for i in 0..8 {
            w.submit(&format!("j{i}"), 1000);
        }
        let mut req = Req::anon(0).versions(versions);
        req.ncpus = 1;
        // 100 s a job with the fast version: room for five by the request.
        req.cpu_secs = 450.0;
        let before = w.db.statements();
        let out = w.rpc(&req, 2000);
        let used = w.db.statements() - before;
        let today = w.hav(out.host_id, 1_000_002).unwrap().n_jobs_today;
        (out.n_sent, used, today, w.havs(out.host_id))
    };
    let (sent, used, today, rows) = run(versions.clone());
    assert_eq!(sent, 3);
    assert_eq!(today, 3);
    assert_eq!(rows, vec![1_000_002]);
    // No more statements than with the fast version alone: one insert for
    // the row, and claims only for what the quota allows.
    let (sent_alone, used_alone, _, _) = run(versions[..1].to_vec());
    assert_eq!(sent_alone, 3);
    assert_eq!(used, used_alone);

    // Both versions out of quota: the message names the limit once.
    let mut w = World::new();
    w.config.daily_result_quota = 3;
    for i in 0..8 {
        w.submit(&format!("j{i}"), 1000);
    }
    let mut req = Req::anon(0).versions(versions);
    req.ncpus = 1;
    req.cpu_secs = 450.0;
    let out = w.rpc(&req, 2000);
    req.hostid = out.host_id;
    req.seqno = 2;
    let out = w.rpc(&req, 2100);
    assert_eq!(out.n_sent, 0);
    let limits = out
        .reply
        .messages
        .iter()
        .filter(|m| m.message.contains("Daily job limit (3) has been reached"))
        .count();
    assert_eq!(limits, 1);
}

#[test]
fn strings_from_the_client_are_escaped_and_bounded() {
    let w = World::new();
    w.submit("j1", 1000);
    // Entities the request parser undoes, to become markup if echoed as is.
    let nasty = Cav::new("a", 100)
        .platform("p&lt;/platform&gt;&lt;injected/&gt;&amp;")
        .plan_class("c&lt;/plan_class&gt;&lt;also/&gt;");
    let out = w.rpc(&Req::anon(0).versions(vec![nasty]), 2000);
    assert_eq!(out.n_sent, 1);
    let xml = out.reply.write().unwrap();
    assert!(!xml.contains("<injected/>"), "{xml}");
    assert!(!xml.contains("<also/>"), "{xml}");
    assert!(
        xml.contains("<platform>p&lt;/platform>&lt;injected/>&amp;</platform>"),
        "{xml}"
    );
    assert!(
        xml.contains("<plan_class>c&lt;/plan_class>&lt;also/></plan_class>"),
        "{xml}"
    );

    // An unknown app name is echoed in a message, escaped too.
    let w = World::new();
    w.submit("j1", 1000);
    let out = w.rpc(
        &Req::anon(0).versions(vec![Cav::new("x&lt;/message&gt;&lt;y/&gt;", 1)]),
        2000,
    );
    let xml = out.reply.write().unwrap();
    assert!(!xml.contains("<y/>"), "{xml}");
    assert!(xml.contains("Unknown app name x&lt;/message>&lt;y/> in app_info.xml"));

    // Megabyte strings: kept to 256 bytes each, so the reply stays small.
    let w = World::new();
    w.submit("j1", 1000);
    let huge = "p".repeat(1 << 20);
    let long = Cav::new("a", 100).platform(&huge).plan_class(&huge);
    let unknown = Cav::new(&huge, 1);
    let out = w.rpc(&Req::anon(0).versions(vec![long, unknown]), 2000);
    assert_eq!(out.n_sent, 1);
    let xml = out.reply.write().unwrap();
    assert!(xml.len() < 20_000, "{} bytes", xml.len());
    assert!(xml.contains(&format!("<platform>{}</platform>", "p".repeat(256))));
    assert!(xml.contains(&format!(
        "Unknown app name {} in app_info.xml",
        "p".repeat(256)
    )));

    // Multi-byte characters are cut between characters and escaped whole.
    let w = World::new();
    w.submit("j1", 1000);
    let euros = "\u{20ac}".repeat(300);
    let out = w.rpc(
        &Req::anon(0).versions(vec![Cav::new("a", 100).platform(&euros)]),
        2000,
    );
    let xml = out.reply.write().unwrap();
    assert_eq!(out.n_sent, 1);
    assert!(xml.len() < 20_000);
    // 85 characters of three bytes each, each byte a reference
    assert!(xml.contains(&format!(
        "<platform>{}</platform>",
        "&#226;&#130;&#172;".repeat(85)
    )));
}

#[test]
fn more_than_a_hundred_apps_without_a_client_version_are_probed_in_one_statement() {
    let w = World::new();
    let mut values = Vec::new();
    for id in 10..170 {
        values.push(format!("({id}, 0, 'app{id}', 'App {id}', 'trivial')"));
    }
    w.sql(&format!(
        "INSERT INTO app (id, create_time, name, user_friendly_name, validator) VALUES {}",
        values.join(", ")
    ));
    w.submit("j1", 1000);
    w.submit_app("app77", "j77", 1000, None);
    w.submit_app("app160", "j160", 1000, None);
    let mut req = Req::anon(0).versions(vec![Cav::new("b", 1)]);
    req.cpu_secs = 0.0;
    let host = w.rpc(&req, 1500).host_id;
    req.hostid = host;
    req.seqno = 2;
    req.cpu_secs = 1000.0;
    let before = w.db.statements();
    let out = w.rpc(&req, 2000);
    let used = w.db.statements() - before;
    assert_eq!(out.n_sent, 0);
    for name in ["App A", "App 77", "App 160"] {
        assert!(
            has_message(&out, &format!("usable version of {name}.")),
            "{name}: {:?}",
            out.reply.messages
        );
    }
    assert!(!has_message(&out, "usable version of App 78."));
    assert!(used <= 12, "{used} statements");
}

#[test]
fn an_app_the_user_deselected_or_a_beta_app_is_not_sent_without_the_preference() {
    let w = World::new();
    w.submit("j1", 1000);
    let req = |host: i64, seqno: i32| {
        let mut r = Req::anon(host).versions(vec![Cav::new("a", 100), Cav::new("b", 100)]);
        r.seqno = seqno;
        r
    };

    // The user selected app b only: no work from a, and no complaint about it.
    w.sql("UPDATE user SET project_prefs = '<project_preferences><app_id>2</app_id></project_preferences>'");
    let out = w.rpc(&req(0, 1), 2000);
    assert_eq!(out.n_sent, 0);
    assert!(!has_message(&out, "app_info.xml"));
    let host = out.host_id;
    // Selecting a lets it through.
    w.sql("UPDATE user SET project_prefs = '<project_preferences><app_id>1</app_id></project_preferences>'");
    let out = w.rpc(&req(host, 2), 2100);
    assert_eq!(out.n_sent, 1);

    // A beta app goes only to users who allow beta work.
    let w = World::new();
    w.submit("j1", 1000);
    w.sql("UPDATE app SET beta = 1 WHERE id = 1");
    let out = w.rpc(&req(0, 1), 2000);
    assert_eq!(out.n_sent, 0);
    assert!(!has_message(&out, "app_info.xml"));
    let host = out.host_id;
    w.sql("UPDATE user SET project_prefs = '<project_preferences><allow_beta_work>1</allow_beta_work></project_preferences>'");
    let out = w.rpc(&req(host, 2), 2100);
    assert_eq!(out.n_sent, 1);
}

/// Statements one request runs when it is sent `jobs` jobs.
fn statements_with_jobs(jobs: usize, anonymous: bool) -> (u64, usize) {
    let mut w = World::new();
    w.config.max_wus_to_send = 50;
    for i in 0..jobs {
        w.submit(&format!("j{i}"), 1000);
    }
    let mut req = Req::anon(0).versions(vec![Cav::new("a", 100).flops(1e11)]);
    if !anonymous {
        req.platform = LINUX.into();
    }
    req.ncpus = 16;
    req.cpu_secs = 1e9;
    let mut first = req.clone();
    first.cpu_secs = 0.0;
    let host = w.rpc(&first, 1500).host_id;
    req.hostid = host;
    req.seqno = 2;
    let before = w.db.statements();
    let out = w.rpc(&req, 2000);
    (w.db.statements() - before, out.n_sent)
}

#[test]
fn a_request_sent_many_jobs_costs_the_same_per_job_as_for_a_normal_client() {
    // The claims of a request go in one write batch, three statements a job
    // (claim, row counter, workunit); that is how the scheduler is built for
    // every client, and is no different on the anonymous platform.
    let (few, n_few) = statements_with_jobs(2, true);
    let (many, n_many) = statements_with_jobs(40, true);
    assert_eq!((n_few, n_many), (2, 40));
    assert_eq!((many - few) / 38, 3, "{few} {many}");
    let (normal_few, _) = statements_with_jobs(2, false);
    let (normal_many, n_normal) = statements_with_jobs(40, false);
    assert_eq!(n_normal, 40);
    // The one statement more is the probe for apps with work that the client
    // has no version of (app b has none here).
    assert_eq!(few, normal_few + 1);
    assert_eq!(many, normal_many + 1);
}

#[test]
fn a_host_with_no_benchmark_gets_a_positive_peak_speed_and_is_credited() {
    // `HostUsage::sequential` gives a missing benchmark 1e9 for an ordinary
    // client; the anonymous path does the same for the CPU part of the peak
    // speed, which is the result's `flops_estimate`.
    for bad in ["0", "-5", "nan", "inf"] {
        let w = World::new();
        let wu = w.submit("j1", 1000);
        let versions = vec![Cav::new("a", 100)];
        let out = w.rpc(&Req::anon(0).fpops(bad).versions(versions.clone()), 2000);
        assert_eq!(out.n_sent, 1, "{bad}");
        assert_eq!(first_sent(&w).flops_estimate, 1e9, "{bad}");
        assert_eq!(wu_number(&out, "rsc_fpops_est"), 1e12, "{bad}");

        let mut report = Req::anon(out.host_id).fpops(bad).versions(versions);
        report.seqno = 2;
        report.cpu_secs = 0.0;
        report.reports = vec![("j1_0".into(), RESULT_FILES_UPLOADED, 0)];
        w.rpc(&report, 2500);
        w.transition(wu, 2500);
        let r = &w.results(wu)[0];
        assert_eq!(r.validate_state, VALIDATE_STATE_VALID, "{bad}");
        // 1e9 is read as an unknown speed, and credited on the average CPU.
        let credit = 4.3e9 * 100.0 * 200.0 / 86400e9;
        assert!(
            (r.granted_credit - credit).abs() < 1e-9,
            "{bad}: {}",
            r.granted_credit
        );
    }

    // A GPU version on such a host: the GPU part is its own figure, the CPU
    // part the default.
    let w = World::new();
    w.submit("j1", 1000);
    let mut req = Req::anon(0)
        .fpops("0")
        .versions(vec![Cav::new("a", 100).gpu("NVIDIA", 1.0).flops(1e11)]);
    req.cpu_secs = 0.0;
    req.gpu = Some((1000.0, 5e12));
    assert_eq!(w.rpc(&req, 2000).n_sent, 1);
    assert_eq!(first_sent(&w).flops_estimate, 1e9 + 5e12);
}

#[test]
fn lost_results_are_left_alone_by_a_request_that_asks_for_no_work() {
    // Upstream looks for lost results only when it may send work
    // (`ok_to_send_work` in `process_request`), so a request that reports
    // and asks for nothing neither resends nor times out a lost result.
    let mut w = World::new();
    w.config.resend_lost_results = true;
    let wu = w.submit("j1", 1000);
    let versions = vec![Cav::new("a", 100).flops(2e9)];
    let out = w.rpc(&Req::anon(0).versions(versions.clone()), 2000);
    assert_eq!(out.n_sent, 1);
    let host = out.host_id;
    let deadline = w.results(wu)[0].report_deadline;

    let mut quiet = Req::anon(host).versions(versions.clone());
    quiet.seqno = 2;
    quiet.cpu_secs = 0.0;
    quiet.other = Some(Vec::new());
    let out = w.rpc(&quiet, 2100);
    assert_eq!(out.n_resent, 0);
    assert!(out.transition.is_empty(), "{:?}", out.transition);
    assert!(!has_message(&out, "lost task"), "{:?}", out.reply.messages);
    let r = &w.results(wu)[0];
    assert_eq!(r.server_state, RESULT_SERVER_STATE_IN_PROGRESS);
    assert_eq!(r.report_deadline, deadline);

    // The same when the client wants work for a resource it has no version
    // of, which is a request for work: the result is not resent on one it
    // has no version for, and times out as upstream does.
    let mut gpu_only = Req::anon(host).versions(versions.clone());
    gpu_only.seqno = 3;
    gpu_only.cpu_secs = 0.0;
    gpu_only.gpu = Some((1000.0, 5e12));
    gpu_only.other = Some(Vec::new());
    let out = w.rpc(&gpu_only, 2200);
    assert_eq!(out.n_resent, 0);
    assert!(has_message(&out, "Didn't resend lost task j1_0 (expired)"));

    // And once it asks for work for the resource, a lost result is resent.
    let mut w = World::new();
    w.config.resend_lost_results = true;
    w.submit("j1", 1000);
    let out = w.rpc(&Req::anon(0).versions(versions.clone()), 2000);
    let mut again = Req::anon(out.host_id).versions(versions);
    again.seqno = 2;
    again.other = Some(Vec::new());
    assert_eq!(w.rpc(&again, 2100).n_resent, 1);
}

/// Two results in progress for a client with a GPU limit of one: `j1` sent
/// as a CPU job, then `j2` as a GPU job. The CPU version is then dropped
/// and `j1` is lost.
fn lost_cpu_result_and_a_gpu_job_in_progress(gpu_limit: i32) -> (World, i64, SchedOutcome) {
    let mut w = World::new();
    w.config.resend_lost_results = true;
    w.config.max_wus_in_progress_gpu = gpu_limit;
    let wu = w.submit("j1", 1000);
    let out = w.rpc(
        &Req::anon(0).versions(vec![Cav::new("a", 100).flops(2e9)]),
        2000,
    );
    assert_eq!(out.n_sent, 1);
    let host = out.host_id;
    w.submit("j2", 2000);
    let gpu = vec![Cav::new("a", 100).gpu("NVIDIA", 1.0).flops(1e11)];
    let mut req = Req::anon(host).versions(gpu.clone());
    req.seqno = 2;
    req.cpu_secs = 0.0;
    req.gpu = Some((1000.0, 5e12));
    assert_eq!(w.rpc(&req, 2100).n_sent, 1);
    assert_eq!(w.results(wu)[0].app_version_id, ANON_PLATFORM_CPU);

    // The client holds j2 only.
    req.seqno = 3;
    req.other = Some(vec!["j2_0".into()]);
    let out = w.rpc(&req, 2200);
    (w, wu, out)
}

#[test]
fn a_lost_cpu_result_is_not_moved_to_a_gpu_that_is_at_its_limit() {
    // One GPU task per GPU: the lost result may not become a second.
    let (w, wu, out) = lost_cpu_result_and_a_gpu_job_in_progress(1);
    assert_eq!(out.n_resent, 0, "{:?}", out.reply.messages);
    assert!(has_message(&out, "Didn't resend lost task j1_0 (expired)"));
    assert_eq!(w.results(wu)[0].app_version_id, ANON_PLATFORM_CPU);
    let on_gpu =
        w.scalar("SELECT count(*) FROM result WHERE server_state = 4 AND app_version_id <= -3");
    assert_eq!(on_gpu, 1.0);
}

#[test]
fn a_lost_cpu_result_moves_to_a_gpu_that_has_room() {
    let (w, wu, out) = lost_cpu_result_and_a_gpu_job_in_progress(2);
    assert_eq!(out.n_resent, 1, "{:?}", out.reply.messages);
    assert_eq!(w.results(wu)[0].app_version_id, ANON_PLATFORM_NVIDIA);
    let on_gpu =
        w.scalar("SELECT count(*) FROM result WHERE server_state = 4 AND app_version_id <= -3");
    assert_eq!(on_gpu, 2.0);

    // The moves in one request count: two lost CPU results and one free GPU
    // slot make one resend, and the other times out.
    let mut w = World::new();
    w.config.resend_lost_results = true;
    w.config.max_wus_in_progress_gpu = 1;
    w.submit("j1", 1000);
    w.submit("j2", 1000);
    let out = w.rpc(
        &Req::anon(0).versions(vec![Cav::new("a", 100).flops(2e9)]),
        2000,
    );
    assert_eq!(out.n_sent, 2);
    let mut req =
        Req::anon(out.host_id).versions(vec![Cav::new("a", 100).gpu("NVIDIA", 1.0).flops(1e11)]);
    req.seqno = 2;
    req.cpu_secs = 0.0;
    req.gpu = Some((1000.0, 5e12));
    req.other = Some(Vec::new());
    let out = w.rpc(&req, 2100);
    assert_eq!(out.n_resent, 1, "{:?}", out.reply.messages);
    assert_eq!(
        w.scalar("SELECT count(*) FROM result WHERE server_state = 4 AND app_version_id <= -3"),
        1.0
    );
    assert_eq!(
        w.scalar("SELECT count(*) FROM result WHERE server_state = 4 AND app_version_id = -2"),
        1.0
    );
    assert_eq!(out.transition.len(), 1);
}

#[test]
fn a_lost_result_stays_on_the_resource_it_was_recorded_on() {
    // The client has a CPU and a faster GPU version and wants work for both:
    // the lost CPU result is sent again as a CPU job, with its record as it
    // was, not moved to the GPU because that version is faster.
    let mut w = World::new();
    w.config.resend_lost_results = true;
    let wu = w.submit("j1", 1000);
    let out = w.rpc(
        &Req::anon(0).versions(vec![Cav::new("a", 100).flops(2e9)]),
        2000,
    );
    assert_eq!(out.n_sent, 1);
    let both = vec![
        Cav::new("a", 100).flops(2e9),
        Cav::new("a", 101).gpu("NVIDIA", 1.0).flops(1e11),
    ];
    let mut req = Req::anon(out.host_id).versions(both);
    req.seqno = 2;
    req.gpu = Some((1000.0, 5e12));
    req.other = Some(Vec::new());
    let out = w.rpc(&req, 2100);
    assert_eq!(out.n_resent, 1, "{:?}", out.reply.messages);
    let r = &w.results(wu)[0];
    assert_eq!(r.app_version_id, ANON_PLATFORM_CPU);
    assert_eq!(r.flops_estimate, 2e9);
    assert!(out
        .reply
        .write()
        .unwrap()
        .contains("<version_num>100</version_num>"));
    assert_eq!(w.havs(out.host_id), vec![1_000_002]);
}

#[test]
fn a_friendly_app_name_with_markup_is_escaped_in_the_notices() {
    let w = World::new();
    w.sql("UPDATE app SET user_friendly_name = 'A & B <x>' WHERE id = 1");

    // The client has no version of the app, which has work.
    w.submit("j1", 1000);
    let mut req = Req::anon(0).versions(vec![Cav::new("b", 1)]);
    req.cpu_secs = 0.0;
    let host = w.rpc(&req, 1500).host_id;
    req.hostid = host;
    req.seqno = 2;
    req.cpu_secs = 1000.0;
    let out = w.rpc(&req, 2000);
    assert!(has_message(&out, "usable version of A & B <x>."));
    let xml = out.reply.write().unwrap();
    assert!(
        xml.contains("doesn't have a usable version of A &amp; B &lt;x>."),
        "{xml}"
    );
    assert!(!xml.contains("A & B"), "{xml}");

    // The client has a version of it, but not one that can run the job.
    let w = World::new();
    w.sql("UPDATE app SET user_friendly_name = 'A & B <x>' WHERE id = 1");
    w.submit_app("a", "big", 1000, Some(3e9));
    let req = Req::anon(0).versions(vec![Cav::new("a", 100).platform("windows_intelx86")]);
    let out = w.rpc(&req, 2000);
    assert_eq!(out.n_sent, 0);
    let xml = out.reply.write().unwrap();
    assert!(
        xml.contains("doesn't have a usable version of A &amp; B &lt;x>."),
        "{xml}"
    );
    assert!(!xml.contains("A & B"), "{xml}");

    // A name the client sends is escaped once, not twice.
    let out = w.rpc(&Req::anon(0).versions(vec![Cav::new("x&amp;y", 1)]), 2100);
    let xml = out.reply.write().unwrap();
    assert!(
        xml.contains("Unknown app name x&amp;y in app_info.xml"),
        "{xml}"
    );
}
