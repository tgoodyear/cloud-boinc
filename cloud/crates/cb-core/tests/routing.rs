//! Routing rules in the scheduler RPC, against SQLite.

use std::cell::{Cell, RefCell};
use std::sync::OnceLock;

use async_trait::async_trait;
use cb_core::model::JobResult;
use cb_core::plan_class::PlanClassSpecs;
use cb_core::project::{
    delete_setting, put_setting, Project, SETTING_CONFIG, SETTING_ROUTING_RULES,
};
use cb_core::routing::RoutingRules;
use cb_core::scheduler::{handle_request, Catalog, SchedCtx, SchedOutcome};
use cb_core::work::{create_work, Assign, AssignTarget, JobSpec, Submission};
use cb_core::Config;
use cb_crypto::Signer;
use cb_db::{query, stmt, Db, DbResult, Rows, Stmt};
use cb_proto::consts::*;
use cb_sqlite::SqliteDb;
use futures_lite::future::block_on;

const LINUX: &str = "x86_64-pc-linux-gnu";
const MAC: &str = "arm64-apple-darwin";
const INPUT_TEMPLATE: &str = "<input_template>\n<workunit>\n<rsc_fpops_est>1e12</rsc_fpops_est>\n<rsc_memory_bound>1e8</rsc_memory_bound>\n<rsc_disk_bound>1e8</rsc_disk_bound>\n<delay_bound>3600</delay_bound>\n</workunit>\n</input_template>\n";
const RESULT_TEMPLATE: &str = "<output_template>\n<file_info>\n    <name><OUTFILE_0/></name>\n    <generated_locally/>\n    <upload_when_present/>\n    <max_nbytes>5000000</max_nbytes>\n    <url><UPLOAD_URL/></url>\n</file_info>\n<result>\n    <file_ref>\n        <file_name><OUTFILE_0/></file_name>\n        <open_name>out</open_name>\n    </file_ref>\n</result>\n</output_template>\n";

fn version_xml(n: i32) -> String {
    format!("<app_version>\n    <app_name>a</app_name>\n    <version_num>{n}</version_num>\n    <file_ref>\n        <file_name>a_{n}</file_name>\n        <main_program/>\n    </file_ref>\n</app_version>\n")
}

fn signer() -> &'static Signer {
    static SIGNER: OnceLock<Signer> = OnceLock::new();
    SIGNER.get_or_init(|| {
        let (private, _public) = cb_crypto::generate_key_pair().unwrap();
        Signer::new(&private).unwrap()
    })
}

/// One app with a version for Linux (ID 7, number 100) and one for macOS (ID 8,
/// number 200); the test host can run both.
/// Passes statements on and keeps their text.
struct Recording<'a> {
    inner: &'a SqliteDb,
    log: &'a RefCell<Vec<String>>,
}

#[async_trait(?Send)]
impl Db for Recording<'_> {
    async fn batch(&self, stmts: Vec<Stmt>) -> DbResult<Vec<Rows>> {
        self.log
            .borrow_mut()
            .extend(stmts.iter().map(|s| s.sql.clone()));
        self.inner.batch(stmts).await
    }
}

struct World {
    db: SqliteDb,
    log: RefCell<Vec<String>>,
    config: Config,
    routing: RefCell<RoutingRules>,
    seq: Cell<u32>,
}

impl World {
    fn new(rules: &str) -> World {
        let db = SqliteDb::with_schema().unwrap();
        let mut config =
            Config::parse("<config><master_url>https://p.example/</master_url></config>").unwrap();
        config.long_name = "Test".into();
        config.daily_result_quota = 1000;
        config.version_select_random_factor = 0.0;
        config.resend_lost_results = true;
        config.enable_assignment = true;
        block_on(db.batch(vec![
            stmt!("INSERT INTO platform (id, create_time, name, user_friendly_name) VALUES (1, 0, ?, 'Linux'), (2, 0, ?, 'Mac')", LINUX, MAC),
            stmt!("INSERT INTO app (id, create_time, name, user_friendly_name, validator) VALUES (1, 0, 'a', 'A', 'trivial')"),
            stmt!("INSERT INTO app_version (id, create_time, appid, version_num, platformid, xml_doc) VALUES (7, 0, 1, 100, 1, ?), (8, 0, 1, 200, 2, ?)", version_xml(100), version_xml(200)),
            stmt!("INSERT INTO user (id, create_time, email_addr, name, authenticator, cross_project_id) VALUES (1, 0, 'u@example.com', 'u', 'auth1', 'cpid1')"),
        ]))
        .unwrap();
        World {
            db,
            log: RefCell::new(Vec::new()),
            config,
            routing: RefCell::new(RoutingRules::parse(rules).unwrap()),
            seq: Cell::new(0),
        }
    }

    fn rand(&self) -> u32 {
        self.seq.set(self.seq.get().wrapping_add(2_654_435_761));
        self.seq.get()
    }

    fn submit(&self, names: &[&str], batch: i64, assign: Option<Assign>) -> Vec<i64> {
        let sub = Submission {
            app: "a".into(),
            input_template: INPUT_TEMPLATE.into(),
            result_template: RESULT_TEMPLATE.into(),
            jobs: names
                .iter()
                .map(|n| JobSpec {
                    name: (*n).into(),
                    min_quorum: Some(1),
                    target_nresults: Some(1),
                    batch: Some(batch),
                    assign,
                    ..Default::default()
                })
                .collect(),
        };
        let random = || self.rand();
        block_on(create_work(
            &self.db,
            &self.config,
            Some(signer()),
            1000,
            &random,
            &sub,
        ))
        .unwrap()
        .iter()
        .map(|o| {
            assert_eq!(o.error, None);
            o.workunit_id
        })
        .collect()
    }

    fn rpc(&self, hostid: i64, seqno: i32, other: Option<&[&str]>, now: i64) -> SchedOutcome {
        let mut xml = format!(
            "<scheduler_request>\n<authenticator>auth1</authenticator>\n<hostid>{hostid}</hostid>\n<rpc_seqno>{seqno}</rpc_seqno>\n<core_client_major_version>8</core_client_major_version>\n<core_client_minor_version>0</core_client_minor_version>\n<core_client_release>4</core_client_release>\n<client_cap_plan_class>1</client_cap_plan_class>\n<work_req_seconds>100000</work_req_seconds>\n<cpu_req_secs>100000</cpu_req_secs>\n<cpu_req_instances>1</cpu_req_instances>\n<estimated_delay>0</estimated_delay>\n<platform_name>{LINUX}</platform_name>\n<alt_platform><name>{MAC}</name></alt_platform>\n<host_info>\n<domain_name>box</domain_name>\n<ip_addr>192.0.2.17</ip_addr>\n<host_cpid>c0ffee</host_cpid>\n<p_ncpus>4</p_ncpus>\n<p_vendor>GenuineIntel</p_vendor>\n<p_model>cpu</p_model>\n<p_fpops>2e9</p_fpops>\n<m_nbytes>8e9</m_nbytes>\n<d_total>1e11</d_total>\n<d_free>5e10</d_free>\n<os_name>Linux</os_name>\n<os_version>6.1</os_version>\n</host_info>\n"
        );
        if let Some(names) = other {
            xml.push_str("<other_results>\n");
            for n in names {
                xml.push_str(&format!(
                    "<other_result>\n<name>{n}</name>\n</other_result>\n"
                ));
            }
            xml.push_str("</other_results>\n");
        }
        xml.push_str("</scheduler_request>\n");
        let catalog = block_on(Catalog::load(&self.db)).unwrap();
        let random = || self.rand();
        let routing = self.routing.borrow();
        let plan_classes = PlanClassSpecs::default();
        let recording = Recording {
            inner: &self.db,
            log: &self.log,
        };
        let ctx = SchedCtx {
            db: &recording,
            config: &self.config,
            plan_classes: &plan_classes,
            routing: &routing,
            catalog: &catalog,
            code_sign_key: "",
            now,
            random: &random,
            client_ip: "198.51.100.9",
        };
        block_on(handle_request(&ctx, &xml)).unwrap()
    }

    /// A host that has made its first request and asked for nothing.
    fn host(&self) -> i64 {
        let out = self.rpc(0, 1, None, 1500);
        out.host_id
    }

    fn results(&self, wu: i64) -> Vec<JobResult> {
        block_on(query(
            &self.db,
            stmt!("SELECT * FROM result WHERE workunitid = ? ORDER BY id", wu),
        ))
        .unwrap()
    }
}

const BY_NAME: &str = r#"{"rules":[
    {"app":"a","name":"lin_*","allow":[{"platform":"x86_64-pc-linux-gnu"}]},
    {"app":"a","name":"mac_*","allow":[{"platform":"arm64-apple-darwin","min_version":150}]},
    {"app":"a","name":"none_*","allow":[]}
]}"#;

fn sent_with(w: &World, wu: i64) -> Option<i64> {
    w.results(wu)
        .first()
        .filter(|r| r.server_state == RESULT_SERVER_STATE_IN_PROGRESS)
        .map(|r| r.app_version_id)
}

#[test]
fn a_workunit_goes_only_with_a_version_its_rule_allows() {
    for (name, version) in [("lin_1", 7), ("mac_1", 8)] {
        let w = World::new(BY_NAME);
        let host = w.host();
        let wu = w.submit(&[name], 0, None)[0];
        let out = w.rpc(host, 2, None, 2000);
        assert_eq!(out.n_sent, 1, "{name}");
        assert_eq!(sent_with(&w, wu), Some(version), "{name}");
    }
}

#[test]
fn a_workunit_no_version_may_take_is_skipped_and_others_still_go() {
    let w = World::new(BY_NAME);
    let host = w.host();
    let refused = w.submit(&["none_1"], 0, None)[0];
    let open = w.submit(&["free_1"], 0, None)[0];
    let out = w.rpc(host, 2, None, 2000);
    assert_eq!(out.n_sent, 1);
    assert_eq!(sent_with(&w, refused), None);
    assert!(sent_with(&w, open).is_some());
}

#[test]
fn refused_workunits_in_front_do_not_hide_the_ones_behind() {
    // More refused results than the four windows of 100 that one request reads,
    // all older than the one the host can take: only the query's own condition
    // lets it reach the allowed one.
    let w = World::new(BY_NAME);
    let host = w.host();
    let names: Vec<String> = (0..450).map(|i| format!("none_{i}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let ids = w.submit(&refs, 0, None);
    let open = w.submit(&["free_1"], 0, None)[0];
    let out = w.rpc(host, 2, None, 2000);
    assert_eq!(out.n_sent, 1);
    assert!(sent_with(&w, open).is_some());
    assert!(ids.iter().all(|id| sent_with(&w, *id).is_none()));
}

#[test]
fn a_batch_range_selects() {
    let w = World::new(
        r#"{"rules":[{"app":"a","min_batch":5,"max_batch":9,"allow":[{"max_version":100}]}]}"#,
    );
    let host = w.host();
    let inside = w.submit(&["x1"], 7, None)[0];
    let outside = w.submit(&["x2"], 10, None)[0];
    let out = w.rpc(host, 2, None, 2000);
    assert_eq!(out.n_sent, 2);
    assert_eq!(sent_with(&w, inside), Some(7));
    // Outside the range any version may do; the fastest is the first of equals.
    assert!(sent_with(&w, outside).is_some());
}

#[test]
fn a_lost_result_is_not_resent_to_a_version_a_rule_refuses() {
    let w = World::new("");
    let host = w.host();
    let wu = w.submit(&["lin_1"], 0, None)[0];
    let out = w.rpc(host, 2, None, 2000);
    assert_eq!(out.n_sent, 1);
    let version = sent_with(&w, wu).unwrap();
    let name = w.results(wu)[0].name.clone();

    // A rule that allows the other version only: the lost result is let go.
    let other = if version == 7 { MAC } else { LINUX };
    *w.routing.borrow_mut() = RoutingRules::parse(&format!(
        r#"{{"rules":[{{"app":"a","name":"lin_*","allow":[{{"platform":"{other}"}}]}}]}}"#
    ))
    .unwrap();
    let out = w.rpc(host, 3, Some(&[]), 2100);
    assert_eq!(out.n_resent, 0);
    assert!(out.reply.messages.iter().any(|m| m
        .message
        .contains(&format!("Didn't resend lost task {name}"))));

    // Another world, where the rule allows the version it went with: resent.
    let w = World::new("");
    let host = w.host();
    let wu = w.submit(&["lin_1"], 0, None)[0];
    assert_eq!(w.rpc(host, 2, None, 2000).n_sent, 1);
    let this = if sent_with(&w, wu) == Some(7) {
        LINUX
    } else {
        MAC
    };
    *w.routing.borrow_mut() = RoutingRules::parse(&format!(
        r#"{{"rules":[{{"app":"a","allow":[{{"platform":"{this}"}}]}}]}}"#
    ))
    .unwrap();
    assert_eq!(w.rpc(host, 3, Some(&[]), 2100).n_resent, 1);
}

#[test]
fn assigned_jobs_obey_a_rule_only_when_it_says_so() {
    for (flag, sent) in [("false", 1), ("true", 0)] {
        let w = World::new("");
        let host = w.host();
        let wu = w.submit(
            &["none_1"],
            0,
            Some(Assign {
                target: AssignTarget::Host,
                id: host,
            }),
        )[0];
        *w.routing.borrow_mut() = RoutingRules::parse(&format!(
            r#"{{"rules":[{{"app":"a","name":"none_*","assigned":{flag},"allow":[]}}]}}"#
        ))
        .unwrap();
        let out = w.rpc(host, 2, None, 2000);
        assert_eq!(out.n_sent, sent, "assigned: {flag}");
        assert_eq!(sent_with(&w, wu).is_some(), sent == 1, "assigned: {flag}");
    }
}

#[test]
fn the_setting_is_checked_loaded_and_removed() {
    let db = SqliteDb::with_schema().unwrap();
    block_on(put_setting(
        &db,
        SETTING_CONFIG,
        "<config><master_url>https://p.example/</master_url></config>",
        1,
    ))
    .unwrap();
    assert!(block_on(put_setting(
        &db,
        SETTING_ROUTING_RULES,
        r#"{"rules":[{"app":""}]}"#,
        2
    ))
    .is_err());
    assert!(block_on(Project::load(&db)).unwrap().routing.is_empty());
    block_on(put_setting(&db, SETTING_ROUTING_RULES, BY_NAME, 3)).unwrap();
    assert_eq!(block_on(Project::load(&db)).unwrap().routing.rules.len(), 3);
    assert!(block_on(delete_setting(&db, SETTING_ROUTING_RULES)).unwrap());
    assert!(!block_on(delete_setting(&db, SETTING_ROUTING_RULES)).unwrap());
    assert!(block_on(Project::load(&db)).unwrap().routing.is_empty());
    assert!(block_on(delete_setting(&db, SETTING_CONFIG)).is_err());
}

#[test]
fn a_negative_batch_is_selected_like_any_other_when_no_minimum_is_set() {
    let w = World::new(r#"{"rules":[{"app":"a","max_batch":9,"allow":[{"max_version":1}]}]}"#);
    let host = w.host();
    let wu = w.submit(&["neg_1"], -3, None)[0];
    // The rule selects batch -3 (no minimum), allows no version the host has, so
    // the workunit is not sent: the query and the loop agree.
    let out = w.rpc(host, 2, None, 2000);
    assert_eq!(out.n_sent, 0);
    assert_eq!(sent_with(&w, wu), None);
}

#[test]
fn assigned_rules_do_not_spend_the_budget_of_queries_that_do_not_run() {
    // Rules that apply to assigned jobs, large enough that the assigned query's
    // conditions would use up most of a shared budget; assignment is off, so
    // that query is never built and the regular one keeps its own conditions.
    let rule = format!(
        r#"{{"app":"a","name":"{}","min_batch":9223372036854775806,"max_batch":9223372036854775807,"assigned":true,"allow":[]}}"#,
        "x".repeat(50)
    );
    let rules = vec![rule; 64].join(",");
    let mut w = World::new(&format!(r#"{{"rules":[{rules}]}}"#));
    w.config.enable_assignment = false;
    let host = w.host();
    w.submit(&["j1"], 0, None);
    w.log.borrow_mut().clear();
    w.rpc(host, 2, None, 2000);
    let log = w.log.borrow();
    let regular: Vec<&String> = log
        .iter()
        .filter(|s| s.contains("r.server_state = 2 AND r.appid"))
        .collect();
    assert!(!regular.is_empty());
    assert!(
        regular.iter().all(|s| s.contains("CASE WHEN")),
        "{regular:?}"
    );
    assert!(log.iter().all(|s| !s.contains("FROM assignment")));
}
