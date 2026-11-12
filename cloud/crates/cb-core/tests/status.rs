//! `server_status.php` and the operator summary: the numbers, the snapshot,
//! and that every query that touches `result` or `workunit` uses an index.

use cb_core::project::{put_setting, Project, SETTING_CONFIG};
use cb_core::status::{
    operator_summary, record_sweep, server_status, server_status_within, snapshot_statements,
    STATUS_COUNTS_TTL, SWEEP_STALE_SECS,
};
use cb_core::web::{server_status_page, server_status_xml};
use cb_db::{stmt, Db, Stmt};
use cb_sqlite::SqliteDb;
use futures_lite::future::block_on;

const NOW: i64 = 2_000_000_000;
const CONFIG: &str = "<config>\n<long_name>Test &amp; Project</long_name>\n<master_url>https://p.example/</master_url>\n</config>\n";

fn sql(db: &SqliteDb, text: &str) {
    block_on(db.run(Stmt::new(text))).unwrap();
}

fn workunit(db: &SqliteDb, name: &str, appid: i64, transition: i64, extra: &str) {
    sql(
        db,
        &format!(
            "INSERT INTO workunit (create_time, appid, name, xml_doc, rsc_fpops_est, rsc_fpops_bound, \
             rsc_memory_bound, rsc_disk_bound, transition_time, delay_bound, min_quorum, \
             target_nresults, max_error_results, max_total_results, max_success_results, \
             result_template, need_validate, assimilate_state, file_delete_state) \
             VALUES (1, {appid}, '{name}', '', 1e12, 1e13, 1e8, 1e8, {transition}, 3600, 1, 1, 3, 5, 3, '', {extra})"
        ),
    );
}

/// `columns`: server_state, outcome, validate_state, received_time,
/// elapsed_time, userid, appid, file_delete_state, create_time.
#[allow(clippy::too_many_arguments)]
fn result(
    db: &SqliteDb,
    name: &str,
    wu: i64,
    state: i32,
    outcome: i32,
    validate: i32,
    received: i64,
    elapsed: f64,
    userid: i64,
    appid: i64,
    create_time: i64,
) {
    sql(
        db,
        &format!(
            "INSERT INTO result (create_time, workunitid, server_state, outcome, validate_state, \
             received_time, elapsed_time, userid, hostid, name, xml_doc_in, appid, file_delete_state) \
             VALUES ({create_time}, {wu}, {state}, {outcome}, {validate}, {received}, {elapsed}, \
             {userid}, {userid}, '{name}', '', {appid}, 0)"
        ),
    );
}

fn world() -> (SqliteDb, Project) {
    let db = SqliteDb::with_schema().unwrap();
    block_on(put_setting(&db, SETTING_CONFIG, CONFIG, 10)).unwrap();
    sql(
        &db,
        "INSERT INTO app (id, create_time, name, user_friendly_name) VALUES (1, 1, 'worker', 'Worker <one>')",
    );
    sql(
        &db,
        "INSERT INTO app (id, create_time, name, user_friendly_name, deprecated) VALUES (2, 1, 'old', 'Old', 1)",
    );
    sql(
        &db,
        "INSERT INTO app (id, create_time, name, user_friendly_name) VALUES (3, 1, 'idle', 'Idle')",
    );
    let h = 3600;
    // Workunits 1..: in flight, waiting to validate, to assimilate (two), and
    // one that is due with its files ready to go.
    workunit(&db, "wu_flight", 1, NOW + h, "0, 0, 0");
    workunit(&db, "wu_validate", 1, NOW + h, "1, 0, 0");
    workunit(&db, "wu_assim1", 1, 2147483647, "0, 1, 0");
    workunit(&db, "wu_assim2", 1, 2147483647, "0, 1, 0");
    workunit(&db, "wu_delete", 1, NOW - 2 * h, "0, 2, 1");
    // Unsent: three of app 1 (the oldest created at 100), one of the
    // deprecated app.
    for (i, t) in [100, 200, 300].iter().enumerate() {
        result(&db, &format!("u{i}"), 1, 2, 0, 0, 0, 0.0, 0, 1, *t);
    }
    result(&db, "u_old", 1, 2, 0, 0, 0, 0.0, 0, 2, 50);
    // In progress: two.
    result(&db, "p0", 1, 4, 0, 0, 0, 0.0, 1, 1, 10);
    result(&db, "p1", 1, 4, 0, 0, 0, 0.0, 2, 1, 10);
    // Received in the last day: four valid (users 1, 1, 2, 3), a client error
    // and an invalid one; and a valid one from two days ago.
    for (i, (user, secs)) in [(1, 3600.0), (1, 7200.0), (2, 3600.0), (3, 10800.0)]
        .iter()
        .enumerate()
    {
        result(
            &db,
            &format!("v{i}"),
            2,
            5,
            1,
            1,
            NOW - 100 - i as i64,
            *secs,
            *user,
            1,
            20,
        );
    }
    result(&db, "e0", 2, 5, 3, 0, NOW - 50, 5.0, 1, 1, 20);
    result(&db, "i0", 2, 5, 1, 2, NOW - 60, 9.0, 2, 1, 20);
    result(
        &db,
        "old_valid",
        2,
        5,
        1,
        1,
        NOW - 2 * 86400,
        100.0,
        4,
        1,
        20,
    );
    // Files ready to be deleted, of the due workunit.
    result(&db, "d0", 5, 5, 1, 1, NOW - 3 * 86400, 1.0, 1, 1, 20);
    sql(
        &db,
        "UPDATE result SET file_delete_state = 1 WHERE name = 'd0'",
    );
    sql(
        &db,
        "UPDATE result SET file_delete_state = 1 WHERE name = 'v0'",
    );
    // Accounts and computers.
    sql(
        &db,
        &format!(
            "INSERT INTO user (create_time, email_addr, authenticator, total_credit, expavg_credit) VALUES \
             (1, 'a@x.example', 'k1', 500, 30), (1, 'b@x.example', 'k2', 5, 0), ({}, 'c@x.example', 'k3', 0, 0)",
            NOW - 100
        ),
    );
    sql(
        &db,
        &format!(
            "INSERT INTO host (create_time, userid, total_credit, expavg_credit) VALUES \
             (1, 1, 500, 30), ({}, 3, 0, 0)",
            NOW - 100
        ),
    );
    let project = block_on(Project::load(&db)).unwrap();
    (db, project)
}

#[test]
fn the_status_has_the_numbers_of_the_upstream_page() {
    let (db, project) = world();
    let s = block_on(server_status(&db, NOW)).unwrap();
    assert_eq!((s.results_ready_to_send, s.results_in_progress), (4, 2));
    assert_eq!(
        (
            s.workunits_waiting_for_validation,
            s.workunits_waiting_for_assimilation
        ),
        (1, 2)
    );
    assert_eq!(
        (
            s.workunits_waiting_for_deletion,
            s.results_waiting_for_deletion
        ),
        (1, 1)
    );
    assert_eq!(s.transitions_due, 1);
    assert!((s.transitioner_backlog_hours - 2.0).abs() < 1e-9);
    assert_eq!(
        (s.users, s.users_with_credit, s.users_with_recent_credit),
        (3, 2, 1)
    );
    assert_eq!(s.users_past_24_hours, 1);
    assert_eq!(
        (s.hosts, s.hosts_with_credit, s.hosts_with_recent_credit),
        (2, 1, 1)
    );
    assert_eq!(s.hosts_past_24_hours, 1);
    assert!((s.flops - 30.0 / 200.0).abs() < 1e-9);

    let xml = server_status_xml(&project, &s, NOW);
    for want in [
        "<results_ready_to_send>4</results_ready_to_send>",
        "<results_in_progress>2</results_in_progress>",
        "<workunits_waiting_for_validation>1</workunits_waiting_for_validation>",
        "<workunits_waiting_for_assimilation>2</workunits_waiting_for_assimilation>",
        "<workunits_waiting_for_deletion>1</workunits_waiting_for_deletion>",
        "<results_waiting_for_deletion>1</results_waiting_for_deletion>",
        "<transitioner_backlog_hours>2</transitioner_backlog_hours>",
        "<users_with_recent_credit>1</users_with_recent_credit>",
        "<users_with_credit>2</users_with_credit>",
        "<users_registered_in_past_24_hours>1</users_registered_in_past_24_hours>",
        "<hosts_with_recent_credit>1</hosts_with_recent_credit>",
        "<hosts_with_credit>1</hosts_with_credit>",
        "<hosts_registered_in_past_24_hours>1</hosts_registered_in_past_24_hours>",
        "<current_floating_point_speed>0.15</current_floating_point_speed>",
        "<tasks_by_app>",
    ] {
        assert!(xml.contains(want), "{want}\n{xml}");
    }
    // The applications that are not deprecated, with upstream's element
    // names; runtimes only for one with valid tasks in the day.
    let apps: Vec<&str> = xml
        .split("<app>\n")
        .skip(1)
        .map(|a| a.split("</app>").next().unwrap())
        .collect();
    assert_eq!(apps.len(), 2, "{xml}");
    assert_eq!(
        apps[0],
        "   <id>1</id>\n   <name>worker</name>\n   <unsent>3</unsent>\n   <in_progress>2</in_progress>\n\
         \x20  <avg_runtime>1.75</avg_runtime>\n   <min_runtime>1</min_runtime>\n\
         \x20  <max_runtime>3</max_runtime>\n   <users>3</users>\n"
    );
    assert_eq!(
        apps[1],
        "   <id>3</id>\n   <name>idle</name>\n   <unsent>0</unsent>\n   <in_progress>0</in_progress>\n"
    );
    assert!(xml.find("<tasks_by_app>").unwrap() < xml.find("</database_file_states>").unwrap());

    let page = server_status_page(&project, &s);
    assert!(page.contains("Tasks by application"));
    // One row per application that is not deprecated, the cells in order.
    let row = |name: &str| -> Vec<String> {
        let at = page
            .find(&format!("<td>{name}</td>"))
            .unwrap_or_else(|| panic!("{name}"));
        let end = page[at..].find("</tr>").unwrap() + at;
        page[at..end]
            .split("</td>")
            .filter_map(|c| c.rsplit_once('>').map(|(_, t)| t.to_string()))
            .collect()
    };
    assert_eq!(
        row("Worker &lt;one&gt;"),
        [
            "Worker &lt;one&gt;",
            "3",
            "2",
            "1.75 hours (1.00 hours - 3.00 hours)",
            "3"
        ]
    );
    assert_eq!(row("Idle"), ["Idle", "0", "0", "---", "---"]);
    assert!(
        !page.contains(">Old<"),
        "a deprecated application is not listed"
    );
    assert!(page.contains("Workunits waiting for validation"));
    assert!(page.contains("Test &amp; Project server status"));
}

#[test]
fn the_summary_has_one_entry_per_application() {
    let (db, _) = world();
    block_on(record_sweep(&db, NOW - 30)).unwrap();
    let s = block_on(operator_summary(&db, NOW + 60, STATUS_COUNTS_TTL)).unwrap();
    // The snapshot was computed now and is a minute old when read.
    assert_eq!(s.as_of, NOW + 60);
    assert_eq!(s.age_secs, 0);
    let s = block_on(operator_summary(&db, NOW + 120, STATUS_COUNTS_TTL)).unwrap();
    assert_eq!((s.as_of, s.age_secs), (NOW + 60, 60));
    let json = serde_json::to_value(&s).unwrap();
    let app = &json["apps"][0];
    assert_eq!(app["name"], "worker");
    assert_eq!(
        (app["unsent"].as_i64(), app["in_progress"].as_i64()),
        (Some(3), Some(2))
    );
    // The oldest unsent task was made at 100; the summary is read at NOW + 120.
    assert_eq!(
        app["oldest_unsent_age_secs"].as_i64(),
        Some(NOW + 120 - 100)
    );
    assert_eq!(app["received_24h"], 6);
    assert_eq!(app["validated_24h"], 4);
    assert_eq!(
        app["errors_24h"],
        serde_json::json!({"client_error": 1, "invalid": 1})
    );
    assert_eq!(app["workunits_awaiting_validation"], 1);
    assert_eq!(app["workunits_awaiting_assimilation"], 2);
    assert_eq!(json["apps"][1]["name"], "old");
    assert_eq!(json["apps"][1]["deprecated"], true);
    assert_eq!(json["apps"][1]["unsent"], 1);
    // No unsent task: no age.
    assert!(json["apps"][2]["oldest_unsent_age_secs"].is_null());
    assert_eq!(json["totals"]["unsent"], 4);
    assert_eq!(json["totals"]["received_24h"], 6);
    assert_eq!(json["pipeline"]["running"], true);
    assert_eq!(json["pipeline"]["transitions_due"], 1);

    // A sweep that is too old means the pipeline is not running; none
    // recorded yet means nothing is known.
    let s = block_on(operator_summary(&db, NOW + SWEEP_STALE_SECS + 100, 0)).unwrap();
    assert!(!s.pipeline.running);
    assert_eq!(s.pipeline.sweep_age_secs, Some(SWEEP_STALE_SECS + 130));
    sql(&db, "DELETE FROM config WHERE name = 'last_sweep'");
    let s = block_on(operator_summary(&db, NOW + SWEEP_STALE_SECS + 100, 0)).unwrap();
    assert!(s.pipeline.running);
    assert_eq!(s.pipeline.sweep_age_secs, None);
}

#[test]
fn the_snapshot_is_reused_until_it_is_old_enough() {
    let (db, project) = world();
    let first = block_on(server_status(&db, NOW)).unwrap();
    sql(&db, "UPDATE result SET server_state = 5 WHERE name = 'u0'");
    let before = db.statements();
    let again = block_on(server_status(&db, NOW + STATUS_COUNTS_TTL - 1)).unwrap();
    assert_eq!(again.results_ready_to_send, first.results_ready_to_send);
    assert_eq!(db.statements() - before, 2);
    let fresh = block_on(server_status(&db, NOW + STATUS_COUNTS_TTL)).unwrap();
    assert_eq!(fresh.results_ready_to_send, first.results_ready_to_send - 1);
    // A caller can ask for a younger one, or for a new one.
    sql(&db, "UPDATE result SET server_state = 5 WHERE name = 'u1'");
    let now = NOW + STATUS_COUNTS_TTL + 10;
    let young = block_on(server_status_within(&db, now, 5)).unwrap();
    assert_eq!(young.results_ready_to_send, first.results_ready_to_send - 2);
    sql(&db, "UPDATE result SET server_state = 5 WHERE name = 'u2'");
    let kept = block_on(server_status_within(&db, now + 1, 100)).unwrap();
    assert_eq!(kept.results_ready_to_send, first.results_ready_to_send - 2);
    let forced = block_on(server_status_within(&db, now + 1, 0)).unwrap();
    assert_eq!(
        forced.results_ready_to_send,
        first.results_ready_to_send - 3
    );
    // A snapshot of another format is not used.
    sql(
        &db,
        "UPDATE config SET value = '{\"at\":1,\"users\":9}' WHERE name = 'status_counts'",
    );
    let s = block_on(server_status_within(&db, now + 2, 100)).unwrap();
    assert_eq!(s.users, 3);
    // The row does not disturb project settings.
    block_on(Project::load(&db)).unwrap();
    assert_eq!(project.config.long_name, "Test & Project");
}

/// Every statement that reads `result` or `workunit` uses an index: no plan
/// has a bare scan of either table. (The two counts over `user` and `host`
/// do scan their tables, as before; the snapshot is what spares the public
/// page from that.)
#[test]
fn the_status_queries_use_indexes() {
    let (db, _) = world();
    let stmts = snapshot_statements(NOW);
    let mut used = Vec::new();
    for (i, s) in stmts.iter().enumerate().take(9) {
        let plan = block_on(db.run(Stmt {
            sql: format!("EXPLAIN QUERY PLAN {}", s.sql),
            params: s.params.clone(),
        }))
        .unwrap();
        for row in &plan.rows {
            let detail = row["detail"].as_str().unwrap().to_string();
            for table in ["result", "workunit"] {
                let bare = detail.starts_with(&format!("SCAN {table}"));
                assert!(
                    !bare || detail.contains("INDEX"),
                    "statement {i} scans {table} without an index: {detail}\n{}",
                    s.sql
                );
            }
            used.push(detail);
        }
    }
    let all = used.join("\n");
    for index in [
        "res_unsent",
        "res_host_inprog",
        "res_received",
        "wu_assim",
        "wu_need_validate",
        "wu_transition",
        "res_wuid",
    ] {
        assert!(all.contains(index), "{index} is not used:\n{all}");
    }
    // A bound state would not do for a partial index: the states are in the SQL.
    assert!(stmts[1].params.is_empty() && stmts[2].params.is_empty());
    let _ = stmt!("SELECT 1");
}

#[test]
fn the_pipeline_is_not_running_when_its_sweeps_stop() {
    let (db, project) = world();
    let status = |now: i64| {
        let s = block_on(server_status_within(&db, now, 0)).unwrap();
        server_status_xml(&project, &s, now)
    };
    // Nothing recorded: no news, as on a deployment whose pipeline predates
    // the record.
    assert!(status(NOW).contains("<command>pipeline</command><status>running</status>"));
    block_on(record_sweep(&db, NOW)).unwrap();
    assert!(status(NOW + 60).contains("<command>pipeline</command><status>running</status>"));
    assert!(status(NOW + SWEEP_STALE_SECS + 1)
        .contains("<command>pipeline</command><status>not running</status>"));
    block_on(record_sweep(&db, NOW + 1000)).unwrap();
    assert!(status(NOW + 1000 + 60).contains("<status>running</status></daemon>\n</daemon_status>"));
}
