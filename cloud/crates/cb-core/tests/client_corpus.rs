//! Scheduler requests recorded from stock clients of several versions, replayed
//! against this server. The files are in `cloud/tests/client-corpus/` (see its
//! README for how they were captured and scrubbed).
//!
//! Three things are checked:
//!
//! * Each request parses with `cb_proto::sched_request::parse`, and the elements the parser
//!   reads are the ones the stock server reads (`SCHEDULER_REQUEST::parse` in
//!   `sched/sched_types.cpp`). `unread_elements_are_the_documented_ones` finds, for every
//!   value in every fixture, whether changing it changes what the parser returns. A value that
//!   changes nothing is an element the server ignores. The list of those is in `UNREAD`, with the
//!   clients that send each and what the stock server does with it. A client that sends a new
//!   one makes the test fail, so the list cannot go stale.
//! * Each request goes through `handle_request` against SQLite, in a world that holds what the
//!   recorded exchange needed: the host the client already had, the tasks it holds or reports,
//!   and unsent jobs when it asked for work. The reply is well formed, and it has what the
//!   scenario in `manifest.json` calls for: a host ID for a first contact, work for a work
//!   request, an acknowledgement of every reported task, no work when there is none.
//! * `manifest.json` names every file in the directory and every file it names exists.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use cb_core::plan_class::PlanClassSpecs;
use cb_core::scheduler::{handle_request, Catalog, SchedCtx, SchedOutcome};
use cb_core::work::{create_work, JobSpec, Submission};
use cb_core::Config;
use cb_crypto::Signer;
use cb_db::{stmt, Db};
use cb_proto::consts::*;
use cb_proto::sched_request::{parse, SchedulerRequest};
use cb_sqlite::SqliteDb;
use futures_lite::future::block_on;
use regex::Regex;
use serde_json::Value;

const LINUX: &str = "x86_64-pc-linux-gnu";
const AUTH: &str = "fixture-authenticator";
const INPUT_TEMPLATE: &str = "<input_template>\n<workunit>\n<rsc_fpops_est>1e10</rsc_fpops_est>\n<rsc_memory_bound>1e8</rsc_memory_bound>\n<rsc_disk_bound>1e8</rsc_disk_bound>\n<delay_bound>3600</delay_bound>\n</workunit>\n</input_template>\n";
const RESULT_TEMPLATE: &str = "<output_template>\n<file_info>\n    <name><OUTFILE_0/></name>\n    <generated_locally/>\n    <upload_when_present/>\n    <max_nbytes>5000000</max_nbytes>\n    <url><UPLOAD_URL/></url>\n</file_info>\n<result>\n    <file_ref>\n        <file_name><OUTFILE_0/></file_name>\n        <open_name>out</open_name>\n    </file_ref>\n</result>\n</output_template>\n";
const APP_VERSION_XML: &str = "<app_version>\n    <app_name>upper2</app_name>\n    <version_num>101</version_num>\n    <file_ref>\n        <file_name>upper2_1.01_x86_64-pc-linux-gnu</file_name>\n        <main_program/>\n    </file_ref>\n</app_version>\n";

/// The project directory of the fixtures.
fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/client-corpus")
}

fn manifest() -> Value {
    let text = std::fs::read_to_string(corpus_dir().join("manifest.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// `(client version, scenario, file name relative to the corpus, text)`.
fn fixtures() -> Vec<(String, String, String, String)> {
    let mut out = Vec::new();
    for client in manifest()["clients"].as_array().unwrap() {
        let version = client["version"].as_str().unwrap();
        for f in client["files"].as_array().unwrap() {
            let file = f["file"].as_str().unwrap();
            let text = std::fs::read_to_string(corpus_dir().join(file))
                .unwrap_or_else(|e| panic!("{file}: {e}"));
            out.push((
                version.to_string(),
                f["scenario"].as_str().unwrap().to_string(),
                file.to_string(),
                text,
            ));
        }
    }
    out
}

#[test]
fn manifest_names_every_file() {
    let mut named = BTreeSet::new();
    for client in manifest()["clients"].as_array().unwrap() {
        for f in client["files"].as_array().unwrap() {
            assert!(
                named.insert(f["file"].as_str().unwrap().to_string()),
                "{} is listed twice",
                f["file"]
            );
        }
    }
    let mut found = BTreeSet::new();
    for dir in std::fs::read_dir(corpus_dir()).unwrap() {
        let dir = dir.unwrap().path();
        if !dir.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(&dir).unwrap() {
            let file = file.unwrap().path();
            found.insert(format!(
                "{}/{}",
                dir.file_name().unwrap().to_str().unwrap(),
                file.file_name().unwrap().to_str().unwrap()
            ));
        }
    }
    assert_eq!(named, found, "manifest.json and the directory differ");
    // Every client version has the scenarios the matrix is about.
    for client in manifest()["clients"].as_array().unwrap() {
        let scenarios: BTreeSet<&str> = client["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["scenario"].as_str().unwrap())
            .collect();
        for needed in [
            "first_contact",
            "work_request",
            "report",
            "no_work",
            "reattach",
        ] {
            assert!(
                scenarios.contains(needed),
                "client {} has no {needed} fixture",
                client["version"]
            );
        }
    }
}

/// The checks `dev/scrub-sched-request.py` makes, run over the committed files, so that a
/// fixture added by hand cannot carry a key, a CPID, an address or a path.
#[test]
fn fixtures_are_scrubbed() {
    let hex = Regex::new(r"[0-9a-fA-F]{32,}").unwrap();
    let ipv4 = Regex::new(r"(?:^|[^0-9.])((?:[0-9]{1,3}\.){3}[0-9]{1,3})(?:[^0-9]|$)").unwrap();
    let email = Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+\.[A-Za-z]{2,}").unwrap();
    for (_, _, file, text) in fixtures() {
        assert!(
            !hex.is_match(&text),
            "{file}: a run of 32 or more hex digits"
        );
        for m in ipv4.captures_iter(&text) {
            let a = &m[1];
            assert!(
                ["192.0.2.", "198.51.100.", "203.0.113."]
                    .iter()
                    .any(|p| a.starts_with(p)),
                "{file}: IPv4 address {a}"
            );
        }
        for word in ["usnewsmap", "/root/", "/home/"] {
            assert!(!text.contains(word), "{file}: contains {word:?}");
        }
        assert!(!email.is_match(&text), "{file}: an e-mail address");
        let r = parse(&text).unwrap();
        assert_eq!(r.host_info.domain_name, "fixture-host", "{file}");
        assert_eq!(r.host_info.host_cpid, "fixture-host-cpid", "{file}");
    }
}

// ---------------------------------------------------------------------------------------------
// The elements the parser does not read.
// ---------------------------------------------------------------------------------------------

/// The leaf elements (no child elements) of `xml`, as `(path, start, end)` with the byte range
/// of the text between the tags. Comments, CDATA and the declaration are skipped.
fn leaves(xml: &str) -> Vec<(String, usize, usize)> {
    let b = xml.as_bytes();
    let mut stack: Vec<(String, usize, bool)> = Vec::new(); // name, text start, has child
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        if xml[i..].starts_with("<![CDATA[") {
            i = xml[i..].find("]]>").map_or(b.len(), |n| i + n + 3);
            continue;
        }
        if xml[i..].starts_with("<!--") {
            i = xml[i..].find("-->").map_or(b.len(), |n| i + n + 3);
            continue;
        }
        let Some(close) = xml[i..].find('>') else {
            break;
        };
        let tag = &xml[i + 1..i + close];
        let next = i + close + 1;
        if tag.starts_with('?') || tag.ends_with('/') {
            // Declaration or self-closing element: a child of what is open.
            if let Some(top) = stack.last_mut() {
                top.2 = true;
            }
        } else if let Some(name) = tag.strip_prefix('/') {
            let (open, start, has_child) = stack.pop().unwrap_or_default();
            assert_eq!(open, name, "unbalanced tags near byte {i}");
            if !has_child {
                let path: Vec<&str> = stack
                    .iter()
                    .map(|s| s.0.as_str())
                    .chain(std::iter::once(open.as_str()))
                    .collect();
                out.push((path.join("/"), start, i));
            }
        } else {
            if let Some(top) = stack.last_mut() {
                top.2 = true;
            }
            let name = tag.split_whitespace().next().unwrap_or("").to_string();
            stack.push((name, next, false));
        }
        i = next;
    }
    out
}

/// What the parser returns. The text of `<working_global_preferences>` is left out: the server
/// keeps it whole and reads eight values from it, which `working_global_prefs` has, so a change
/// to any other preference must not count as read. Text the server keeps as it was (the
/// top-level `<global_preferences>`, the code-signing key) counts as read.
fn parsed(xml: &str) -> Option<SchedulerRequest> {
    let mut r = parse(xml).ok()?;
    r.working_global_prefs_xml.clear();
    Some(r)
}

/// Elements the parser reads as containers or as text it keeps, so that a leaf of them with
/// nothing in it says nothing about whether they are read.
const CONTAINERS: &[&str] = &[
    "scheduler_request/app_versions",
    "scheduler_request/other_results",
];

/// For one request: the paths of the leaf elements whose value the parser does not use, and the
/// preferences under `<working_global_preferences>` whose value it does use.
fn unread_in(xml: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let base = parsed(xml).expect("fixture must parse");
    let mut unread = BTreeSet::new();
    let mut prefs_read = BTreeSet::new();
    for (path, start, end) in leaves(xml) {
        if path.contains("stderr_out") || CONTAINERS.contains(&path.as_str()) {
            // Kept as text; the parser does not read inside it.
            continue;
        }
        let original = &xml[start..end];
        let read = ["0", "1", "2", "7", "0.5", "zz"]
            .iter()
            .filter(|c| **c != original.trim())
            .any(|c| {
                let changed = format!("{}{c}{}", &xml[..start], &xml[end..]);
                parsed(&changed).as_ref() != Some(&base)
            });
        if let Some(pref) = path.strip_prefix(PREFS_PREFIX) {
            if read {
                prefs_read.insert(pref.trim_start_matches("global_preferences/").to_string());
            }
        } else if !read {
            unread.insert(path);
        }
    }
    (unread, prefs_read)
}

const DROPPED: &str = "the stock parser names it and drops the value";
const SKIPPED: &str = "the stock parser does not name it and skips it without a log message";

/// Leaf elements that stock clients send and the parser does not use, with what the stock server
/// does with each (`sched/sched_types.cpp`: `HOST::parse`, `HOST::parse_time_stats`,
/// `HOST::parse_net_stats`, `SCHED_DB_RESULT::parse_from_client`). Paths are `/`-separated from
/// the root element. Every client of the corpus sends all of them, except `wsl_available`, which
/// the clients up to 8.0.4 send, and the top-level `<global_preferences>` of 7.16 to 7.20 (it is
/// kept whole, so it is not here).
///
/// None of them is read by the stock server, so the server loses nothing a stock server keeps.
const UNREAD: &[(&str, &str)] = &[
    ("scheduler_request/host_info/n_usable_coprocs", DROPPED),
    ("scheduler_request/host_info/p_calculated", DROPPED),
    ("scheduler_request/host_info/wsl_available", DROPPED),
    ("scheduler_request/net_stats/avg_down", DROPPED),
    ("scheduler_request/net_stats/avg_time_down", DROPPED),
    ("scheduler_request/net_stats/avg_time_up", DROPPED),
    ("scheduler_request/net_stats/avg_up", DROPPED),
    ("scheduler_request/result/platform", DROPPED),
    ("scheduler_request/result/version_num", DROPPED),
    ("scheduler_request/time_stats/now", SKIPPED),
    (
        "scheduler_request/time_stats/session_active_duration",
        SKIPPED,
    ),
    (
        "scheduler_request/time_stats/session_gpu_active_duration",
        SKIPPED,
    ),
    (
        "scheduler_request/time_stats/total_active_duration",
        SKIPPED,
    ),
    ("scheduler_request/time_stats/total_duration", SKIPPED),
    (
        "scheduler_request/time_stats/total_gpu_active_duration",
        SKIPPED,
    ),
    ("scheduler_request/time_stats/total_start_time", SKIPPED),
];

/// The preferences the server reads from `<working_global_preferences>`: the same eight as the
/// stock `GLOBAL_PREFS::parse` in `sched/sched_types.cpp`. Every other preference in the request
/// is kept with the XML and not read, by both.
const PREFS_READ: &[&str] = &[
    "disk_max_used_gb",
    "disk_max_used_pct",
    "disk_min_free_gb",
    "max_ncpus_pct",
    "mod_time",
    "ram_max_used_busy_pct",
    "ram_max_used_idle_pct",
    "work_buf_min_days",
];

const PREFS_PREFIX: &str = "scheduler_request/working_global_preferences/";

#[test]
fn every_fixture_parses() {
    for (version, scenario, file, text) in fixtures() {
        let r = parse(&text).unwrap_or_else(|e| panic!("{file}: {e}"));
        let v: Vec<i32> = version.split('.').map(|n| n.parse().unwrap()).collect();
        assert_eq!(
            (
                r.core_client_major_version,
                r.core_client_minor_version,
                r.core_client_release
            ),
            (v[0], v[1], v[2]),
            "{file}: the version in the request is not the client's"
        );
        assert_eq!(r.authenticator, AUTH, "{file}");
        assert_eq!(r.platform_name, LINUX, "{file}");
        assert!(!r.host_info.host_cpid.is_empty(), "{file}");
        if scenario == "first_contact" {
            assert_eq!(r.hostid, 0, "{file}");
        }
        if scenario == "report" {
            assert!(!r.results.is_empty(), "{file}: no reported task");
        } else {
            assert!(r.results.is_empty(), "{file}: reports a task");
        }
    }
}

#[test]
fn unread_elements_are_the_documented_ones() {
    // Path -> clients that send it.
    let mut seen: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut prefs_read = BTreeSet::new();
    for (version, _, _, text) in fixtures() {
        let (unread, prefs) = unread_in(&text);
        for path in unread {
            seen.entry(path).or_default().insert(version.clone());
        }
        prefs_read.extend(prefs);
    }
    let documented: BTreeSet<&str> = UNREAD.iter().map(|(p, _)| *p).collect();
    let found: BTreeSet<&str> = seen.keys().map(String::as_str).collect();
    let report: Vec<String> = seen
        .iter()
        .map(|(p, v)| {
            format!(
                "{p}  [{}]",
                v.iter().cloned().collect::<Vec<_>>().join(", ")
            )
        })
        .collect();
    assert_eq!(
        found,
        documented,
        "elements the parser ignores differ from UNREAD:\n{}",
        report.join("\n")
    );
    // `wsl_available` is the only one some clients do not send.
    for (path, clients) in &seen {
        if path != "scheduler_request/host_info/wsl_available" {
            assert_eq!(
                clients.len(),
                manifest()["clients"].as_array().unwrap().len(),
                "{path} is not sent by every client"
            );
        }
    }
    let prefs_read: BTreeSet<&str> = prefs_read.iter().map(String::as_str).collect();
    let expected: BTreeSet<&str> = PREFS_READ.iter().copied().collect();
    assert_eq!(prefs_read, expected, "preferences the server reads");
}

// ---------------------------------------------------------------------------------------------
// Replay through handle_request.
// ---------------------------------------------------------------------------------------------

/// One upload key for all tests; generating one takes a while.
fn signer() -> &'static Signer {
    static SIGNER: OnceLock<Signer> = OnceLock::new();
    SIGNER.get_or_init(|| {
        let (private, _public) = cb_crypto::generate_key_pair().unwrap();
        Signer::new(&private).unwrap()
    })
}

struct World {
    db: SqliteDb,
    config: Config,
    plan_classes: PlanClassSpecs,
    seq: Cell<u32>,
}

/// Time of the request that creates the host, and of the replayed one an hour later.
const T0: i64 = 1_700_000_000;

impl World {
    fn new() -> World {
        let db = SqliteDb::with_schema().unwrap();
        let mut config =
            Config::parse("<config><master_url>https://project.example/</master_url></config>")
                .unwrap();
        config.long_name = "Fixture project".into();
        config.daily_result_quota = 1000;
        config.version_select_random_factor = 0.0;
        block_on(db.batch(vec![
            stmt!("INSERT INTO platform (id, create_time, name, user_friendly_name) VALUES (1, 0, ?, 'Linux')", LINUX),
            stmt!("INSERT INTO app (id, create_time, name, user_friendly_name, validator) VALUES (1, 0, 'upper2', 'Upper-case test app', 'trivial')"),
            stmt!("INSERT INTO app_version (id, create_time, appid, version_num, platformid, xml_doc) VALUES (7, 0, 1, 101, 1, ?)", APP_VERSION_XML),
            stmt!("INSERT INTO user (id, create_time, email_addr, name, authenticator, cross_project_id) VALUES (1, 0, 'tester@example.org', 'tester', ?, 'fixture-cross-project-id')", AUTH),
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

    fn submit(&self, names: &[String]) {
        if names.is_empty() {
            return;
        }
        let sub = Submission {
            app: "upper2".into(),
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
            T0,
            &random,
            &sub,
        ))
        .unwrap();
        for o in out {
            assert_eq!(o.error, None);
        }
    }

    /// Makes the result `name` one the server sent to `host` earlier.
    fn mark_sent(&self, name: &str, host: i64) {
        block_on(self.db.run(stmt!(
            "UPDATE result SET server_state = ?, hostid = ?, userid = 1, sent_time = ?, \
             report_deadline = ?, app_version_id = 7, app_version_num = 101 WHERE name = ?",
            RESULT_SERVER_STATE_IN_PROGRESS,
            host,
            T0,
            T0 + 3600 * 10,
            name
        )))
        .unwrap();
    }

    fn rpc(&self, xml: &str, now: i64) -> SchedOutcome {
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
        block_on(handle_request(&ctx, xml)).unwrap()
    }

    fn result_row(&self, name: &str) -> (i32, i32, i32) {
        let rows = block_on(self.db.run(stmt!(
            "SELECT server_state, outcome, client_state FROM result WHERE name = ?",
            name
        )))
        .unwrap()
        .rows;
        let r = &rows[0];
        (
            r["server_state"].as_i64().unwrap() as i32,
            r["outcome"].as_i64().unwrap() as i32,
            r["client_state"].as_i64().unwrap() as i32,
        )
    }

    fn host_count(&self) -> i64 {
        let rows = block_on(self.db.run(stmt!("SELECT COUNT(*) AS n FROM host")))
            .unwrap()
            .rows;
        rows[0]["n"].as_i64().unwrap()
    }
}

/// `<scheduler_request>` as the client sent it, with the host ID set and the reported and
/// held tasks taken out: what the same client would have sent the first time.
fn first_contact_of(xml: &str) -> String {
    let results = Regex::new(r"(?s)<result>.*?</result>\s*").unwrap();
    let others = Regex::new(r"(?s)<other_results>.*?</other_results>\s*").unwrap();
    let hostid = Regex::new(r"<hostid>[0-9]+</hostid>").unwrap();
    let seqno = Regex::new(r"<rpc_seqno>[0-9]+</rpc_seqno>").unwrap();
    let work = Regex::new(r"<work_req_seconds>[^<]*</work_req_seconds>").unwrap();
    let x = results.replace_all(xml, "");
    let x = others.replace_all(&x, "");
    let x = hostid.replace(&x, "<hostid>0</hostid>");
    let x = seqno.replace(&x, "<rpc_seqno>0</rpc_seqno>");
    work.replace(&x, "<work_req_seconds>0</work_req_seconds>")
        .into_owned()
}

fn with_hostid(xml: &str, id: i64) -> String {
    Regex::new(r"<hostid>[0-9]+</hostid>")
        .unwrap()
        .replace(xml, format!("<hostid>{id}</hostid>"))
        .into_owned()
}

/// The workunit of a result name: the stock and this server both name results
/// `<workunit>_<n>`.
fn workunit_of(result: &str) -> String {
    result.rsplit_once('_').unwrap().0.to_string()
}

/// A reply is well formed when its tags nest. This is not a schema check; it catches text that
/// was not escaped and elements that were not closed.
fn well_formed(xml: &str) -> Result<(), String> {
    let mut stack: Vec<String> = Vec::new();
    let mut i = 0;
    while let Some(n) = xml[i..].find('<') {
        i += n;
        if xml[i..].starts_with("<![CDATA[") {
            i = xml[i..]
                .find("]]>")
                .map(|n| i + n + 3)
                .ok_or("unterminated CDATA")?;
            continue;
        }
        let close = xml[i..].find('>').ok_or("unterminated tag")?;
        let tag = &xml[i + 1..i + close];
        if tag.starts_with('?') || tag.ends_with('/') {
        } else if let Some(name) = tag.strip_prefix('/') {
            match stack.pop() {
                Some(open) if open == name => {}
                other => return Err(format!("</{name}> closes {other:?}")),
            }
        } else {
            stack.push(tag.split_whitespace().next().unwrap_or("").to_string());
        }
        i += close + 1;
    }
    if stack.is_empty() {
        Ok(())
    } else {
        Err(format!("not closed: {stack:?}"))
    }
}

fn replay(scenario: &str, file: &str, text: &str) {
    let req = parse(text).unwrap();
    let w = World::new();

    // The host the client had before this request: made by the same client's first contact.
    let mut text = text.to_string();
    let mut host = 0;
    if scenario != "first_contact" {
        let primer = w.rpc(&first_contact_of(&text), T0 - 100);
        assert!(primer.host_id > 0, "{file}: the first contact made no host");
        host = primer.host_id;
        if req.hostid != 0 {
            text = with_hostid(&text, host);
        }
        assert_eq!(w.host_count(), 1);
    }

    // The tasks the client reports or still holds were sent to it before.
    let reported: Vec<String> = req.results.iter().map(|r| r.name.clone()).collect();
    let mut held: Vec<String> = req.other_results.iter().map(|r| r.name.clone()).collect();
    held.extend(reported.iter().cloned());
    let wus: Vec<String> = held.iter().map(|n| workunit_of(n)).collect();
    w.submit(&wus);
    for name in &held {
        w.mark_sent(name, host);
    }
    // Work for the request, unless the scenario is one where there is none.
    let wants_work = req.work_req_seconds > 0.0;
    let has_work = matches!(scenario, "first_contact" | "work_request" | "reattach");
    if has_work {
        w.submit(&["extra_0".into(), "extra_1".into(), "extra_2".into()]);
    }

    let out = w.rpc(&text, T0 + 3600);
    let xml = out.reply.write().unwrap_or_else(|e| panic!("{file}: {e}"));
    well_formed(&xml).unwrap_or_else(|e| panic!("{file}: reply is not well formed: {e}\n{xml}"));
    assert!(
        xml.starts_with("<scheduler_reply>") && xml.trim_end().ends_with("</scheduler_reply>"),
        "{file}"
    );
    assert_eq!(out.reply.user.id, 1, "{file}: wrong user\n{xml}");
    assert_eq!(out.user_id, 1, "{file}");

    // Acknowledgements: one for each reported task, in the request's order.
    assert_eq!(out.reply.result_acks, reported, "{file}: acknowledgements");
    for name in &reported {
        let (state, outcome, _) = w.result_row(name);
        assert_eq!(state, RESULT_SERVER_STATE_OVER, "{file}: {name}");
        assert_eq!(outcome, RESULT_OUTCOME_SUCCESS, "{file}: {name}");
    }
    assert_eq!(out.n_reported, reported.len(), "{file}");

    match scenario {
        "first_contact" => {
            assert!(out.host_id > 0, "{file}");
            assert_eq!(out.reply.hostid, out.host_id as u64, "{file}: host ID");
        }
        "reattach" => {
            // The client lost its host ID and sent 0. The server finds the host again by
            // the user and the host CPID, as the stock server does, and does not make a
            // second one.
            assert_eq!(out.host_id, host, "{file}: a second host was made");
            assert_eq!(w.host_count(), 1, "{file}");
        }
        _ => {
            assert_eq!(out.host_id, host, "{file}");
            assert_eq!(out.reply.hostid, 0, "{file}: the host ID is not repeated");
        }
    }
    if wants_work && has_work {
        assert!(out.n_sent >= 1, "{file}: asked for work, got none\n{xml}");
        assert_eq!(out.reply.results.len(), out.n_sent, "{file}");
        assert!(xml.contains("<app_name>upper2</app_name>"), "{file}");
        assert!(xml.contains("<version_num>101</version_num>"), "{file}");
        assert!(
            xml.contains(&format!("<platform>{LINUX}</platform>")),
            "{file}"
        );
    } else {
        assert_eq!(out.n_sent, 0, "{file}: sent work that did not exist\n{xml}");
        assert!(out.reply.results.is_empty(), "{file}");
    }
    if wants_work && !has_work {
        assert!(
            out.reply
                .messages
                .iter()
                .any(|m| m.message.contains("Project has no tasks available")),
            "{file}: a client that asked for work and got none is told so"
        );
    }
}

#[test]
fn every_fixture_replays_through_handle_request() {
    let all = fixtures();
    assert!(all.len() >= 30, "only {} fixtures", all.len());
    for (_, scenario, file, text) in all {
        replay(&scenario, &file, &text);
    }
}
