//! A client attached to a stock server, talking to this server for the first time with the
//! rows of the stock project copied across. The requests are the first ones stock clients of
//! several versions sent after the switch of `cloud/tests/migration-check/` (scrubbed with
//! `cloud/dev/scrub-sched-request.py`, kept in `cloud/tests/migration-check/captures/`). Each
//! one reports a task that finished before the switch and still holds two tasks, one running
//! and one not started.
//!
//! What the tests pin is what the live check relied on, in `docs/compatibility.md`, "Migration
//! check":
//!
//! * with the user, host and job rows copied, the client keeps its host ID, nothing is
//!   aborted, the finished task is acknowledged and recorded, and work is sent;
//! * with the job rows not copied, the server tells the client to abort the tasks it holds,
//!   as `send_result_abort` in `sched/handle_request.cpp` does for a result not in the
//!   database, unless the project turns `send_result_abort` off;
//! * with the host row not copied, the client gets a new host ID, or the ID of a row that has
//!   its host CPID;
//! * a client whose code-signing key is the project's keeps its project.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use cb_core::plan_class::PlanClassSpecs;
use cb_core::scheduler::{handle_request, Catalog, SchedCtx, SchedOutcome};
use cb_core::work::{create_work, JobSpec, Submission};
use cb_core::Config;
use cb_crypto::account::host_cpid;
use cb_crypto::Signer;
use cb_db::{stmt, Db};
use cb_proto::consts::*;
use cb_proto::sched_request::{parse, SchedulerRequest};
use cb_sqlite::SqliteDb;
use futures_lite::future::block_on;
use regex::Regex;

const LINUX: &str = "x86_64-pc-linux-gnu";
/// The placeholders `dev/scrub-sched-request.py` writes.
const AUTH: &str = "fixture-authenticator";
const CLIENT_CPID: &str = "fixture-host-cpid";
const CODE_SIGN_KEY: &str = "1024\nfixture-code-sign-key\n.\n";
const EMAIL: &str = "migrate.two@example.org";
const INPUT_TEMPLATE: &str = "<input_template>\n<workunit>\n<rsc_fpops_est>1e10</rsc_fpops_est>\n<rsc_memory_bound>1e8</rsc_memory_bound>\n<rsc_disk_bound>1e8</rsc_disk_bound>\n<delay_bound>7200</delay_bound>\n</workunit>\n</input_template>\n";
const RESULT_TEMPLATE: &str = "<output_template>\n<file_info>\n    <name><OUTFILE_0/></name>\n    <generated_locally/>\n    <upload_when_present/>\n    <max_nbytes>5000000</max_nbytes>\n    <url><UPLOAD_URL/></url>\n</file_info>\n<result>\n    <file_ref>\n        <file_name><OUTFILE_0/></file_name>\n        <open_name>out</open_name>\n    </file_ref>\n</result>\n</output_template>\n";
const APP_VERSION_XML: &str = "<app_version>\n    <app_name>upper</app_name>\n    <version_num>100</version_num>\n    <file_ref>\n        <file_name>upper_1.00_x86_64-pc-linux-gnu</file_name>\n        <main_program/>\n    </file_ref>\n</app_version>\n";

/// The time of the request, and of the rows' last stock request an hour before.
const NOW: i64 = 1_700_003_600;
const STOCK_LAST_RPC: i64 = NOW - 3600;
/// The host ID the stock server gave the client; the fixtures carry the one of the live run, which
/// differs between files, so the tests read it from the request.
const OTHER_HOST_ID: i64 = 900;

fn captures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/migration-check/captures")
}

/// `(client version, text)` of every capture, oldest client first.
fn captures() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for dir in std::fs::read_dir(captures_dir()).unwrap() {
        let dir = dir.unwrap().path();
        if !dir.is_dir() {
            continue;
        }
        let text = std::fs::read_to_string(dir.join("first-request-after-switch.xml")).unwrap();
        out.push((dir.file_name().unwrap().to_str().unwrap().to_string(), text));
    }
    out.sort();
    assert!(out.len() >= 6, "only {} captures", out.len());
    out
}

fn signer() -> &'static Signer {
    static SIGNER: OnceLock<Signer> = OnceLock::new();
    SIGNER.get_or_init(|| {
        let (private, _public) = cb_crypto::generate_key_pair().unwrap();
        Signer::new(&private).unwrap()
    })
}

/// How much of the stock project's rows reached the database.
#[derive(Clone, Copy, PartialEq)]
enum Rows {
    /// User, host and the job rows of the tasks the client holds.
    All,
    /// User and host; the job rows were not copied.
    NoJobs,
    /// User and the job rows; the host row was not copied.
    NoHost,
}

struct World {
    db: SqliteDb,
    config: Config,
    plan_classes: PlanClassSpecs,
    seq: Cell<u32>,
    code_sign_key: String,
}

impl World {
    fn new(rows: Rows, req: &SchedulerRequest, config_extra: &str) -> World {
        let db = SqliteDb::with_schema().unwrap();
        let mut config = Config::parse(&format!(
            "<config><master_url>http://project.test/proj/</master_url>\
             <stock_short_name>proj</stock_short_name><uldl_dir_fanout>1024</uldl_dir_fanout>\
             <min_sendwork_interval>31</min_sendwork_interval>\
             <send_result_abort>1</send_result_abort>{config_extra}</config>"
        ))
        .unwrap();
        config.long_name = "Migration check project".into();
        config.daily_result_quota = 1000;
        config.version_select_random_factor = 0.0;
        block_on(db.batch(vec![
            stmt!("INSERT INTO platform (id, create_time, name, user_friendly_name) VALUES (1, 0, ?, 'Linux')", LINUX),
            stmt!("INSERT INTO app (id, create_time, name, user_friendly_name, validator) VALUES (1, 0, 'upper', 'Upper-case test app', 'trivial')"),
            stmt!("INSERT INTO app_version (id, create_time, appid, version_num, platformid, xml_doc) VALUES (1, 0, 1, 100, 1, ?)", APP_VERSION_XML),
            // The account as the row copy leaves it: the key, the address, the stock hash.
            stmt!("INSERT INTO user (id, create_time, email_addr, name, authenticator, cross_project_id, passwd_hash) VALUES (2, 1700000002, ?, 'Migrate Two', ?, 'fixture-cross-project-id', '')", EMAIL, AUTH),
        ]))
        .unwrap();
        let w = World {
            db,
            config,
            plan_classes: PlanClassSpecs::default(),
            seq: Cell::new(0),
            code_sign_key: CODE_SIGN_KEY.to_string(),
        };
        if rows == Rows::NoHost {
            // Other accounts' computers came across, so a new ID is not the one the client
            // holds by chance (the first host of a project has ID 1).
            block_on(w.db.run(stmt!(
                "INSERT INTO host (id, create_time, userid, host_cpid, domain_name) \
                 VALUES (500, 1700000100, 99, 'another-cpid', 'another-host')"
            )))
            .unwrap();
        } else {
            // The host row as the stock server left it after the client's last request: the
            // ID the client holds, the sequence number it sent then (the next request sends one
            // more), and the CPID the stock server derives from the client's.
            block_on(w.db.run(stmt!(
                "INSERT INTO host (id, create_time, userid, rpc_seqno, rpc_time, host_cpid, domain_name, venue, \
                 p_ncpus, p_vendor, p_model, os_name, os_version) \
                 VALUES (?, 1700000100, 2, ?, ?, ?, 'fixture-host', '', ?, ?, ?, ?, ?)",
                req.hostid,
                i64::from(req.rpc_seqno) - 1,
                STOCK_LAST_RPC,
                host_cpid(CLIENT_CPID, EMAIL),
                req.host_info.p_ncpus,
                req.host_info.p_vendor.as_str(),
                req.host_info.p_model.as_str(),
                req.host_info.os_name.as_str(),
                req.host_info.os_version.as_str()
            )))
            .unwrap();
        }
        if rows != Rows::NoJobs {
            let held: Vec<String> = req
                .results
                .iter()
                .map(|r| r.name.clone())
                .chain(req.other_results.iter().map(|r| r.name.clone()))
                .collect();
            let wus: Vec<String> = held.iter().map(|n| workunit_of(n)).collect();
            w.submit(&wus);
            let host = if rows == Rows::NoHost {
                OTHER_HOST_ID
            } else {
                req.hostid
            };
            for name in &held {
                w.mark_sent(name, host);
            }
        }
        // Work the project has made since: the request asks for some.
        w.submit(&["new_0".into(), "new_1".into(), "new_2".into()]);
        w
    }

    fn rand(&self) -> u32 {
        self.seq.set(self.seq.get().wrapping_add(2_654_435_761));
        self.seq.get()
    }

    fn submit(&self, names: &[String]) {
        let sub = Submission {
            app: "upper".into(),
            input_template: INPUT_TEMPLATE.into(),
            result_template: RESULT_TEMPLATE.into(),
            jobs: names
                .iter()
                .map(|n| JobSpec {
                    name: n.clone(),
                    min_quorum: Some(1),
                    target_nresults: Some(1),
                    ..Default::default()
                })
                .collect(),
        };
        let random = || self.rand();
        let out = block_on(create_work(
            &self.db,
            &self.config,
            Some(signer()),
            STOCK_LAST_RPC - 1000,
            &random,
            &sub,
        ))
        .unwrap();
        for o in out {
            assert_eq!(o.error, None);
        }
    }

    /// Makes the result `name` one the stock server sent to `host`.
    fn mark_sent(&self, name: &str, host: i64) {
        block_on(self.db.run(stmt!(
            "UPDATE result SET server_state = ?, hostid = ?, userid = 2, sent_time = ?, \
             report_deadline = ?, app_version_id = 1, app_version_num = 100 WHERE name = ?",
            RESULT_SERVER_STATE_IN_PROGRESS,
            host,
            STOCK_LAST_RPC - 500,
            NOW + 7200,
            name
        )))
        .unwrap();
    }

    fn rpc(&self, xml: &str) -> SchedOutcome {
        let catalog = block_on(Catalog::load(&self.db)).unwrap();
        let random = || self.rand();
        let ctx = SchedCtx {
            db: &self.db,
            config: &self.config,
            plan_classes: &self.plan_classes,
            routing: &cb_core::routing::NONE,
            catalog: &catalog,
            code_sign_key: &self.code_sign_key,
            now: NOW,
            random: &random,
            client_ip: "198.51.100.9",
        };
        block_on(handle_request(&ctx, xml)).unwrap()
    }

    fn count(&self, sql: &str) -> i64 {
        block_on(self.db.run(stmt!(sql))).unwrap().rows[0]["n"]
            .as_i64()
            .unwrap()
    }

    fn host_rows(&self) -> i64 {
        self.count("SELECT COUNT(*) AS n FROM host")
    }

    fn result_row(&self, name: &str) -> (i32, i32) {
        let rows = block_on(self.db.run(stmt!(
            "SELECT server_state, outcome FROM result WHERE name = ?",
            name
        )))
        .unwrap()
        .rows;
        (
            rows[0]["server_state"].as_i64().unwrap() as i32,
            rows[0]["outcome"].as_i64().unwrap() as i32,
        )
    }
}

fn workunit_of(result: &str) -> String {
    result.rsplit_once('_').unwrap().0.to_string()
}

fn has_message(out: &SchedOutcome, text: &str) -> bool {
    out.reply.messages.iter().any(|m| m.message.contains(text))
}

fn held(req: &SchedulerRequest) -> Vec<String> {
    req.other_results.iter().map(|r| r.name.clone()).collect()
}

/// What the captured requests have to be for the tests to mean anything.
#[test]
fn captures_are_first_requests_after_the_switch() {
    for (version, text) in captures() {
        let req = parse(&text).unwrap_or_else(|e| panic!("{version}: {e}"));
        assert_eq!(req.authenticator, AUTH, "{version}");
        assert_eq!(req.platform_name, LINUX, "{version}");
        assert_eq!(req.host_info.host_cpid, CLIENT_CPID, "{version}");
        assert!(req.hostid > 0, "{version}: a client that has a host ID");
        assert!(
            req.rpc_seqno > 0,
            "{version}: a client that has asked before"
        );
        assert_eq!(req.results.len(), 1, "{version}: reports the finished task");
        assert_eq!(held(&req).len(), 2, "{version}: holds two tasks");
        assert!(req.have_other_results_list, "{version}");
        assert!(req.work_req_seconds > 0.0, "{version}");
        assert!(
            text.contains("fixture-code-sign-key"),
            "{version}: carries the project's code-signing key"
        );
    }
}

#[test]
fn a_client_of_the_stock_server_keeps_its_host_and_its_tasks() {
    for (version, text) in captures() {
        let req = parse(&text).unwrap();
        let w = World::new(Rows::All, &req, "");
        let out = w.rpc(&text);
        let xml = out.reply.write().unwrap();
        let ctx = format!("{version}\n{xml}");

        // The host it had, not a new one, and not announced again.
        assert_eq!(out.host_id, req.hostid, "{ctx}");
        assert_eq!(out.reply.hostid, 0, "{ctx}");
        assert_eq!(w.host_rows(), 1, "{ctx}");
        assert_eq!(out.user_id, 2, "{ctx}");
        assert_eq!(
            w.count(&format!(
                "SELECT rpc_seqno AS n FROM host WHERE id = {}",
                req.hostid
            )),
            i64::from(req.rpc_seqno),
            "{version}: the stock sequence number carries on"
        );

        // The project's key is the client's: nothing is asked of the volunteer.
        assert!(out.reply.code_sign_key.is_empty(), "{ctx}");
        assert!(!has_message(&out, "security key"), "{ctx}");
        assert!(!has_message(&out, "remove and add"), "{ctx}");
        assert!(!out.reply.nucleus_only, "{ctx}");

        // The finished task is taken and acknowledged; the held ones are left alone.
        let done = &req.results[0].name;
        assert_eq!(out.reply.result_acks, vec![done.clone()], "{ctx}");
        assert_eq!(
            w.result_row(done),
            (RESULT_SERVER_STATE_OVER, RESULT_OUTCOME_SUCCESS),
            "{ctx}"
        );
        assert!(out.reply.result_aborts.is_empty(), "{ctx}");
        assert!(out.reply.result_abort_if_not_starteds.is_empty(), "{ctx}");
        for name in held(&req) {
            assert_eq!(
                w.result_row(&name),
                (RESULT_SERVER_STATE_IN_PROGRESS, RESULT_OUTCOME_INIT),
                "{ctx}"
            );
        }

        // And it gets the new work, of the application version it already has.
        assert!(out.n_sent >= 1, "{ctx}");
        assert!(xml.contains("<version_num>100</version_num>"), "{ctx}");
        assert!(xml.contains("<app_name>upper</app_name>"), "{ctx}");
        // The delay before its next request is `min_sendwork_interval` of the carried config plus a second.
        assert_eq!(out.reply.request_delay, 32.0, "{ctx}");
    }
}

#[test]
fn tasks_whose_rows_were_not_copied_are_aborted() {
    for (version, text) in captures() {
        let req = parse(&text).unwrap();
        let w = World::new(Rows::NoJobs, &req, "");
        let out = w.rpc(&text);
        let ctx = format!("{version}\n{}", out.reply.write().unwrap());

        // The host is kept, and the task that finished is acknowledged although the server has
        // no row for it, as the stock `handle_results` does.
        assert_eq!(out.host_id, req.hostid, "{ctx}");
        assert_eq!(w.host_rows(), 1, "{ctx}");
        assert_eq!(
            out.reply.result_acks,
            vec![req.results[0].name.clone()],
            "{ctx}"
        );
        assert_eq!(out.n_reported, 0, "{ctx}: nothing to record it against");

        // Both tasks it holds are not in the database: it is told to abort them, and the
        // volunteer is told so.
        let mut aborted = out.reply.result_aborts.clone();
        aborted.sort();
        let mut expected = held(&req);
        expected.sort();
        assert_eq!(aborted, expected, "{ctx}");
        for name in &expected {
            assert!(
                has_message(&out, &format!("Result {name} is no longer usable")),
                "{ctx}"
            );
        }
        assert!(out.reply.result_abort_if_not_starteds.is_empty(), "{ctx}");
    }
}

#[test]
fn turning_send_result_abort_off_lets_unknown_tasks_run() {
    for (version, text) in captures() {
        let req = parse(&text).unwrap();
        let w = World::new(
            Rows::NoJobs,
            &req,
            "<send_result_abort>0</send_result_abort>",
        );
        let out = w.rpc(&text);
        let ctx = format!("{version}\n{}", out.reply.write().unwrap());
        assert!(out.reply.result_aborts.is_empty(), "{ctx}");
        assert!(out.reply.result_abort_if_not_starteds.is_empty(), "{ctx}");
        assert_eq!(
            out.reply.result_acks,
            vec![req.results[0].name.clone()],
            "{ctx}"
        );
        assert_eq!(out.host_id, req.hostid, "{ctx}");
    }
}

#[test]
fn a_host_row_that_did_not_come_across_makes_a_new_host() {
    for (version, text) in captures() {
        let req = parse(&text).unwrap();
        let w = World::new(Rows::NoHost, &req, "");
        let out = w.rpc(&text);
        let ctx = format!("{version}\n{}", out.reply.write().unwrap());
        // No row under the client's ID and none with its CPID: a new host, and the reply
        // carries the new ID for the client to adopt.
        assert_eq!(w.host_rows(), 2, "{ctx}");
        assert_eq!(out.host_id, 501, "{ctx}");
        assert_eq!(out.reply.hostid, 501, "{ctx}");
        // Its task was sent to a host that is not this one but is the same user's, so the
        // report is accepted, as `handle_results` allows.
        assert_eq!(
            out.reply.result_acks,
            vec![req.results[0].name.clone()],
            "{ctx}"
        );
        assert_eq!(
            w.result_row(&req.results[0].name),
            (RESULT_SERVER_STATE_OVER, RESULT_OUTCOME_SUCCESS),
            "{ctx}"
        );
        // The tasks it holds are not on its new host, but they are known, so nothing is aborted.
        assert!(out.reply.result_aborts.is_empty(), "{ctx}");
    }
}

#[test]
fn a_renumbered_host_row_is_found_by_its_cpid() {
    for (version, text) in captures() {
        let req = parse(&text).unwrap();
        let w = World::new(Rows::NoHost, &req, "");
        // The row came across under another ID, with the CPID and the hardware the stock server
        // stored (a CPID match that describes other hardware is not taken, `obviously_different`).
        block_on(w.db.run(stmt!(
            "INSERT INTO host (id, create_time, userid, rpc_seqno, rpc_time, host_cpid, domain_name, \
             p_ncpus, p_vendor, p_model, os_name, os_version) \
             VALUES (?, 1700000100, 2, 0, ?, ?, 'fixture-host', ?, ?, ?, ?, ?)",
            req.hostid + 1000,
            STOCK_LAST_RPC,
            host_cpid(CLIENT_CPID, EMAIL),
            req.host_info.p_ncpus,
            req.host_info.p_vendor.as_str(),
            req.host_info.p_model.as_str(),
            req.host_info.os_name.as_str(),
            req.host_info.os_version.as_str()
        )))
        .unwrap();
        let out = w.rpc(&text);
        let ctx = format!("{version}\n{}", out.reply.write().unwrap());
        assert_eq!(out.host_id, req.hostid + 1000, "{ctx}");
        assert_eq!(w.host_rows(), 2, "{ctx}: a host row was added");
        assert_eq!(out.reply.hostid, (req.hostid + 1000) as u64, "{ctx}");
    }
}

#[test]
fn another_code_signing_key_asks_the_client_to_re_attach() {
    let (version, text) = captures().remove(0);
    let req = parse(&text).unwrap();
    let mut w = World::new(Rows::All, &req, "");
    w.code_sign_key = "1024\nanother-key\n.\n".into();
    let out = w.rpc(&text);
    assert!(
        has_message(&out, "has changed its security key"),
        "{version}: a client holding another key is told to remove and add the project"
    );
}

/// The key is compared as text with the whitespace around it left out, so a key file that
/// ends in a newline, or does not, is the same key.
#[test]
fn the_code_signing_key_compares_without_surrounding_whitespace() {
    let (version, text) = captures().remove(0);
    let req = parse(&text).unwrap();
    let mut w = World::new(Rows::All, &req, "");
    w.code_sign_key = format!("\n\n{}\n\n", CODE_SIGN_KEY.trim());
    let out = w.rpc(&text);
    assert!(!has_message(&out, "security key"), "{version}");
}

#[test]
fn fixtures_hold_no_key_address_or_path() {
    let hex = Regex::new(r"[0-9a-fA-F]{32,}").unwrap();
    let ipv4 = Regex::new(r"(?:^|[^0-9.])((?:[0-9]{1,3}\.){3}[0-9]{1,3})(?:[^0-9]|$)").unwrap();
    for (version, text) in captures() {
        assert!(!hex.is_match(&text), "{version}: a run of 32 hex digits");
        for m in ipv4.captures_iter(&text) {
            assert_eq!(&m[1], "192.0.2.1", "{version}: an address");
        }
        for word in ["/root/", "/home/", "usnewsmap", "cb-mig-", "project.test"] {
            assert!(!text.contains(word), "{version}: {word}");
        }
    }
}
