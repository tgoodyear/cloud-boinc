//! The leaderboards, the jobs overview, the activity page and the rollup
//! behind them, against SQLite: what each page shows, what it must not show,
//! that the queries use their indexes, and that the rollup is repeatable.

use std::cell::RefCell;

use async_trait::async_trait;
use cb_core::avg::recent_key;
use cb_core::feed::parse_feed;
use cb_core::project::{put_setting, Project, SETTING_CONFIG};
use cb_core::site::{site_page, Params, SiteCtx, SITE_PATHS};
use cb_core::site_boards::{public_cache_key, MAX_PAGE, PAGE_SIZE, PUBLIC_PATHS};
use cb_core::site_work::{activity, work, FeedState};
use cb_core::stats::{
    load_inflight, load_work, rollup_pass, BUCKET_SECS, MIN_BUDGET, MIN_INTERVAL_SECS,
    UNSENT_COUNT_CAP,
};
use cb_db::{stmt, Db, DbResult, Rows, Stmt};
use cb_sqlite::SqliteDb;
use futures_lite::future::block_on;

const CONFIG: &str = "<config>\n<long_name>Test Project</long_name>\n<master_url>https://p.example/</master_url>\n<user_country>1</user_country>\n</config>\n";
const NOW: i64 = 2_000_000_000;
/// Statements a pass may use: what the Free plan leaves after a purge.
const BUDGET: usize = 40;
/// Start of the three-hour period NOW is in.
const CUR: i64 = NOW - NOW % BUCKET_SECS;

/// The stock rule: an owner is named only if the account shows its computers.
const STOCK_CONFIG: &str = "<config>\n<long_name>Test Project</long_name>\n<master_url>https://p.example/</master_url>\n<user_country>1</user_country>\n<host_owner_public>0</host_owner_public>\n</config>\n";

fn setup() -> (SqliteDb, Project) {
    setup_with(CONFIG)
}

fn setup_with(config: &str) -> (SqliteDb, Project) {
    let db = SqliteDb::with_schema().unwrap();
    block_on(put_setting(&db, SETTING_CONFIG, config, 10)).unwrap();
    block_on(db.batch(vec![
        stmt!("INSERT INTO platform (id, create_time, name, user_friendly_name) VALUES (1, 1, 'x86_64-pc-linux-gnu', 'Linux')"),
        stmt!("INSERT INTO app (id, create_time, name, user_friendly_name) VALUES (1, 1, 'worker', 'Worker <one>')"),
        stmt!("INSERT INTO app (id, create_time, name, user_friendly_name) VALUES (2, 1, 'other', 'Other')"),
    ]))
    .unwrap();
    let project = block_on(Project::load(&db)).unwrap();
    (db, project)
}

#[allow(clippy::too_many_arguments)]
fn add_user(
    db: &SqliteDb,
    id: i64,
    name: &str,
    credit: f64,
    recent: f64,
    valid: i64,
    show_hosts: i64,
    blocked: i64,
) {
    block_on(db.run(stmt!(
        "INSERT INTO user (id, create_time, email_addr, name, authenticator, country, total_credit, \
         expavg_credit, expavg_time, valid_results, show_hosts, blocked, recent_key) \
         VALUES (?, 1, ?, ?, ?, 'US', ?, ?, ?, ?, ?, ?, ?)",
        id,
        format!("secret{id}@example.com"),
        name,
        format!("AUTHKEY{id:0>20}"),
        credit,
        recent,
        NOW as f64,
        valid,
        show_hosts,
        blocked,
        recent_key(recent, NOW as f64)
    )))
    .unwrap();
}

fn add_host(db: &SqliteDb, id: i64, userid: i64, credit: f64, valid: i64) {
    block_on(db.run(stmt!(
        "INSERT INTO host (id, create_time, userid, total_credit, expavg_credit, expavg_time, valid_results, \
         domain_name, last_ip_addr, external_ip_addr, p_vendor, p_model, p_ncpus, os_name, os_version, \
         serialnum, rpc_time, recent_key) VALUES (?, 1, ?, ?, ?, ?, ?, 'secret-pc.lan', '10.9.8.7', '203.0.113.9', \
         'GenuineIntel', 'Core <i7>', 8, 'Linux', '6.1', '[BOINC|8.0.4][CUDA|1|8192MB]', ?, ?)",
        id,
        userid,
        credit,
        credit / 10.0,
        NOW as f64,
        valid,
        NOW - 120,
        recent_key(credit / 10.0, NOW as f64)
    )))
    .unwrap();
}

fn ctx<'a>(db: &'a dyn Db, project: &'a Project, now: i64) -> SiteCtx<'a> {
    static RANDOM: fn() -> u32 = || 4;
    SiteCtx {
        db,
        project,
        base: "https://p.example/",
        auth: None,
        post: false,
        pepper: None,
        now,
        random: &RANDOM,
        mailer: None,
    }
}

fn query(pairs: &[(&str, &str)]) -> Params {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn get(db: &dyn Db, project: &Project, path: &str, q: &Params) -> String {
    let page = block_on(site_page(&ctx(db, project, NOW), path, q))
        .unwrap()
        .unwrap();
    assert_eq!(page.status, 200, "{path}");
    page.body
}

/// A database that remembers the statements it was given, so that their
/// plans can be checked.
struct Spy<'a> {
    inner: &'a SqliteDb,
    log: RefCell<Vec<Stmt>>,
}

#[async_trait(?Send)]
impl Db for Spy<'_> {
    async fn batch(&self, stmts: Vec<Stmt>) -> DbResult<Vec<Rows>> {
        self.log.borrow_mut().extend(stmts.iter().cloned());
        self.inner.batch(stmts).await
    }
}

/// Tables a statement must not read in full. `stats_bucket` is left out: the
/// rollup sums its few rows (a few hundred at most, kept 64 periods).
const BIG: [&str; 4] = ["user", "host", "result", "workunit"];

/// The plan lines of every SELECT the spy saw; panics on a full scan of a
/// large table, and returns the lines for further checks.
fn plans(spy: &Spy<'_>) -> Vec<String> {
    let mut all = Vec::new();
    for s in spy.log.borrow().iter() {
        if !s.sql.trim_start().starts_with("SELECT") {
            continue;
        }
        let rows = block_on(spy.inner.run(Stmt {
            sql: format!("EXPLAIN QUERY PLAN {}", s.sql),
            params: s.params.clone(),
        }))
        .unwrap();
        for r in rows.rows {
            let d = r["detail"].as_str().unwrap().to_string();
            if let Some(rest) = d.strip_prefix("SCAN ") {
                let table = rest.split_whitespace().next().unwrap_or("");
                assert!(
                    !BIG.contains(&table) || d.contains(" USING "),
                    "full scan of {table}: {d}\nin: {}",
                    s.sql
                );
            }
            all.push(d);
        }
    }
    all
}

fn rows_in(html: &str) -> usize {
    html.matches("<tr>").count().saturating_sub(1)
}

#[test]
fn top_users_orders_pages_and_hides_what_is_private() {
    let (db, project) = setup();
    add_user(&db, 1, "Alice", 500.0, 5.0, 10, 1, 0);
    add_user(&db, 2, "Bob <b>bold</b>", 900.0, 1.0, 30, 0, 0);
    add_user(&db, 3, "Carol", 100.0, 9.0, 20, 0, 0);
    add_user(&db, 4, "Nobody", 0.0, 0.0, 0, 0, 0);
    add_user(&db, 5, "Blocked", 9999.0, 9.0, 99, 0, 1);

    let page = get(&db, &project, "/top_users.php", &query(&[]));
    // Total credit is the default order: Bob, Alice, Carol.
    let bob = page
        .find("Bob &lt;b&gt;bold&lt;/b&gt;")
        .expect("escaped name");
    let alice = page.find("Alice").unwrap();
    let carol = page.find("Carol").unwrap();
    assert!(bob < alice && alice < carol);
    assert!(!page.contains("<b>bold"));
    assert!(!page.contains("Nobody") && !page.contains("Blocked"));
    assert!(page.contains("900") && page.contains("show_user.php?userid=2"));
    assert!(page.contains("<strong aria-current=\"true\">Total credit</strong>"));
    // Nothing private.
    assert!(!page.contains("example.com") && !page.contains("AUTHKEY"));

    let by_recent = get(
        &db,
        &project,
        "/top_users.php",
        &query(&[("sort", "expavg_credit")]),
    );
    let (c, a, b) = (
        by_recent.find("Carol").unwrap(),
        by_recent.find("Alice").unwrap(),
        by_recent.find("Bob").unwrap(),
    );
    assert!(c < a && a < b);
    let by_jobs = get(
        &db,
        &project,
        "/top_users.php",
        &query(&[("sort", "valid_results")]),
    );
    let (b, c, a) = (
        by_jobs.find("Bob").unwrap(),
        by_jobs.find("Carol").unwrap(),
        by_jobs.find("Alice").unwrap(),
    );
    assert!(b < c && c < a);
    // An unknown sort is the default; the column name never reaches SQL.
    let odd = get(
        &db,
        &project,
        "/top_users.php",
        &query(&[("sort", "total_credit; DROP TABLE user"), ("page", "-3")]),
    );
    assert_eq!(rows_in(&odd), 3);
    assert_eq!(
        block_on(db.run(stmt!("SELECT count(*) AS n FROM user")))
            .unwrap()
            .scalar_i64(),
        Some(5)
    );
}

#[test]
fn top_users_pages_are_bounded() {
    let (db, project) = setup();
    for i in 1..=120 {
        add_user(&db, i, &format!("User{i}"), 1000.0 - i as f64, 1.0, 1, 0, 0);
    }
    let first = get(&db, &project, "/top_users.php", &query(&[]));
    assert_eq!(rows_in(&first), PAGE_SIZE);
    assert!(first.contains("page=2\">Next"));
    assert!(!first.contains("Previous"));
    let second = get(&db, &project, "/top_users.php", &query(&[("page", "2")]));
    assert_eq!(rows_in(&second), PAGE_SIZE);
    assert!(second.contains("<td class=\"num\" data-label=\"Rank\">51</td>"));
    assert!(second.contains("Previous"));
    let third = get(&db, &project, "/top_users.php", &query(&[("page", "3")]));
    assert_eq!(rows_in(&third), 20);
    assert!(!third.contains("Next"));
    // A page far past the end is the deepest allowed page, and empty here.
    let far = get(
        &db,
        &project,
        "/top_users.php",
        &query(&[("page", "100000")]),
    );
    assert!(far.contains("There is nothing on this page."));
    assert_eq!(rows_in(&far), 0);
    assert_eq!(
        public_cache_key("/top_users.php", &query(&[("page", "100000"), ("x", "1")])).unwrap(),
        format!("/top_users.php?sort=total_credit&page={MAX_PAGE}")
    );
}

#[test]
fn recent_average_orders_as_the_averages_stand_now() {
    let (db, project) = setup();
    // Fifty idle accounts whose stored average is high but decayed to almost
    // nothing, and one active account with a modest average: the active one
    // ranks first, on the first page, whatever the stored averages say.
    for i in 1..=50 {
        add_user(&db, i, &format!("Idle{i}"), 500.0, 100.0, 1, 0, 0);
        let time = (NOW - 12 * 7 * 86400) as f64;
        block_on(db.run(stmt!(
            "UPDATE user SET expavg_time = ?, recent_key = ? WHERE id = ?",
            time,
            recent_key(100.0, time),
            i
        )))
        .unwrap();
    }
    add_user(&db, 99, "Active", 20.0, 10.0, 1, 0, 0);
    let page = get(
        &db,
        &project,
        "/top_users.php",
        &query(&[("sort", "expavg_credit")]),
    );
    let (active, idle) = (page.find("Active").unwrap(), page.find("Idle").unwrap());
    assert!(active < idle, "{page}");
    assert!(page.contains("<td class=\"num\" data-label=\"Rank\">1</td><td class=\"who\" data-label=\"User\"><a href=\"show_user.php?userid=99\">Active"));
    // The displayed figure is the decayed one: 100 halved twelve times.
    assert!(page.contains(">0.02<"), "{page}");
    // The same for computers.
    add_user(&db, 200, "Owner", 1.0, 1.0, 1, 1, 0);
    add_host(&db, 1, 200, 5000.0, 1);
    add_host(&db, 2, 200, 40.0, 1);
    block_on(db.run(stmt!(
        "UPDATE host SET expavg_time = ?, recent_key = ? WHERE id = 1",
        (NOW - 20 * 7 * 86400) as f64,
        recent_key(500.0, (NOW - 20 * 7 * 86400) as f64)
    )))
    .unwrap();
    let page = get(
        &db,
        &project,
        "/top_hosts.php",
        &query(&[("sort", "expavg_credit")]),
    );
    assert!(page.find("hostid=2\">#2<").unwrap() < page.find("hostid=1\">#1<").unwrap());
}

#[test]
fn top_hosts_names_only_owners_who_show_their_computers() {
    // With the stock rule.
    let (db, project) = setup_with(STOCK_CONFIG);
    add_user(&db, 1, "Shown", 1.0, 1.0, 1, 1, 0);
    add_user(&db, 2, "Hidden", 1.0, 1.0, 1, 0, 0);
    add_host(&db, 10, 1, 900.0, 9);
    add_host(&db, 11, 2, 800.0, 8);
    add_host(&db, 12, 1, 0.0, 0);
    let page = get(&db, &project, "/top_hosts.php", &query(&[]));
    assert!(page.contains("show_user.php?userid=1\">Shown</a>"));
    assert!(!page.contains("Hidden"));
    assert!(page.contains("Anonymous"));
    assert!(page.contains("show_host_detail.php?hostid=10\">#10</a>"));
    assert!(!page.contains("hostid=12"));
    // The hardware is public; the machine's name and addresses are not.
    assert!(page.contains("Core &lt;i7&gt; (8 cores)"));
    assert!(page.contains("[1] CUDA (8192MB)"));
    assert!(page.contains("Linux 6.1"));
    for secret in [
        "secret-pc.lan",
        "10.9.8.7",
        "203.0.113.9",
        "secret1@example.com",
        "AUTHKEY",
    ] {
        assert!(!page.contains(secret), "{secret} is on the page");
    }
    assert_eq!(rows_in(&page), 2);
}

#[test]
fn top_hosts_names_every_owner_by_default() {
    let (db, project) = setup();
    add_user(&db, 1, "Shown", 1.0, 1.0, 1, 1, 0);
    add_user(&db, 2, "Hidden", 1.0, 1.0, 1, 0, 0);
    add_host(&db, 10, 1, 900.0, 9);
    add_host(&db, 11, 2, 800.0, 8);
    let page = get(&db, &project, "/top_hosts.php", &query(&[]));
    // The owner who chose not to show computers is named too, and linked.
    assert!(page.contains("show_user.php?userid=2\">Hidden</a>"));
    assert!(page.contains("show_user.php?userid=1\">Shown</a>"));
    assert!(!page.contains("Anonymous"));
    // The machine itself is still not identified.
    for secret in [
        "secret-pc.lan",
        "10.9.8.7",
        "203.0.113.9",
        "example.com",
        "AUTHKEY",
    ] {
        assert!(!page.contains(secret), "{secret} is on the page");
    }
    assert!(page.contains("show_host_detail.php?hostid=11\">#11</a>"));
}

#[test]
fn leaderboard_queries_read_indexes() {
    let (db, project) = setup();
    add_user(&db, 1, "A", 5.0, 1.0, 1, 1, 0);
    add_host(&db, 10, 1, 5.0, 1);
    let spy = Spy {
        inner: &db,
        log: RefCell::new(Vec::new()),
    };
    for sort in ["total_credit", "expavg_credit", "valid_results"] {
        for path in ["/top_users.php", "/top_hosts.php"] {
            let page = block_on(site_page(
                &ctx(&spy, &project, NOW),
                path,
                &query(&[("sort", sort), ("page", "3")]),
            ))
            .unwrap()
            .unwrap();
            assert_eq!(page.status, 200);
        }
    }
    let lines = plans(&spy).join("\n");
    for index in [
        "user_total_credit",
        "user_recent_key",
        "user_valid_results",
        "host_total_credit",
        "host_recent_key",
        "host_valid_results",
    ] {
        assert!(lines.contains(index), "{index} is not used:\n{lines}");
    }
    // The users query sorts nothing outside the index.
    for s in spy
        .log
        .borrow()
        .iter()
        .filter(|s| s.sql.contains("FROM user WHERE"))
    {
        let rows = block_on(db.run(Stmt {
            sql: format!("EXPLAIN QUERY PLAN {}", s.sql),
            params: s.params.clone(),
        }))
        .unwrap();
        for r in rows.rows {
            assert!(!r["detail"].as_str().unwrap().contains("TEMP B-TREE"));
        }
    }
    // One statement each, with a row limit.
    assert!(spy.log.borrow().iter().all(|s| s.sql.contains("LIMIT")));
    assert_eq!(spy.log.borrow().len(), 6);
}

#[allow(clippy::too_many_arguments)]
fn insert_result(
    db: &SqliteDb,
    id: i64,
    app: i64,
    state: i32,
    outcome: i32,
    validate: i32,
    received: i64,
    user: i64,
    host: i64,
) {
    block_on(db.run(stmt!(
        "INSERT INTO result (id, create_time, workunitid, server_state, outcome, validate_state, \
         received_time, userid, hostid, name, xml_doc_in, appid) \
         VALUES (?, 1, 1, ?, ?, ?, ?, ?, ?, ?, '', ?)",
        id,
        state,
        outcome,
        validate,
        received,
        user,
        host,
        format!("r{id}"),
        app
    )))
    .unwrap();
}

fn insert_workunit(db: &SqliteDb, id: i64, app: i64, need_validate: i32, assimilate: i32) {
    block_on(db.run(stmt!(
        "INSERT INTO workunit (id, create_time, appid, name, xml_doc, rsc_fpops_est, rsc_fpops_bound, \
         rsc_memory_bound, rsc_disk_bound, transition_time, delay_bound, min_quorum, target_nresults, \
         max_error_results, max_total_results, max_success_results, result_template, need_validate, \
         assimilate_state) VALUES (?, 1, ?, ?, '', 1, 1, 1, 1, 2147483647, 1, 1, 1, 1, 1, 1, '', ?, ?)",
        id,
        app,
        format!("wu{id}"),
        need_validate,
        assimilate
    )))
    .unwrap();
}

/// Results spread over periods, with tasks waiting and in progress.
fn busy(db: &SqliteDb) {
    add_user(db, 1, "Alice", 10.0, 1.0, 1, 1, 0);
    add_user(db, 2, "Bob", 10.0, 1.0, 1, 0, 0);
    add_host(db, 1, 1, 5.0, 1);
    add_host(db, 2, 1, 5.0, 1);
    add_host(db, 3, 2, 5.0, 1);
    let (s, o, v) = (5, 1, 1);
    // The open period: two successes (one valid, one invalid) and an error.
    insert_result(db, 1, 1, s, o, v, CUR + 100, 1, 1);
    insert_result(db, 2, 1, s, o, 2, CUR + 200, 2, 3);
    insert_result(db, 3, 2, s, 3, 2, CUR + 300, 1, 1);
    // The period before: a success not yet validated.
    insert_result(db, 4, 1, s, o, 0, CUR - BUCKET_SECS + 500, 1, 1);
    // Nine periods back: a week's figure but not a day's, and past what the
    // first pass backfills.
    insert_result(db, 5, 1, s, o, v, CUR - 9 * BUCKET_SECS + 500, 2, 3);
    // Waiting to send, and in progress.
    for id in 10..13 {
        insert_result(db, id, 1, 2, 0, 0, 0, 0, 0);
    }
    insert_result(db, 13, 2, 2, 0, 0, 0, 0, 0);
    insert_result(db, 20, 1, 4, 0, 0, 0, 1, 1);
    insert_result(db, 21, 1, 4, 0, 0, 0, 1, 1);
    insert_result(db, 22, 1, 4, 0, 0, 0, 1, 2);
    insert_result(db, 23, 2, 4, 0, 0, 0, 2, 3);
    insert_workunit(db, 1, 1, 1, 0);
    insert_workunit(db, 2, 2, 0, 1);
}

fn dump(db: &SqliteDb) -> Vec<String> {
    let rows = block_on(db.run(stmt!(
        "SELECT appid, bucket, returned_ok, valid, invalid, failed, timed_out, queued, users, hosts \
         FROM stats_bucket ORDER BY appid, bucket"
    )))
    .unwrap();
    rows.rows.iter().map(|r| format!("{r:?}")).collect()
}

fn figure(db: &SqliteDb, sql: &str) -> i64 {
    block_on(db.run(Stmt::new(sql)))
        .unwrap()
        .scalar_i64()
        .unwrap()
}

#[test]
fn the_rollup_counts_periods_and_snapshots() {
    let (db, project) = setup();
    busy(&db);
    let report = block_on(rollup_pass(&db, NOW, 0, BUDGET)).unwrap();
    assert!(report.ran);
    assert_eq!(report.recounted[0], CUR);
    assert_eq!(report.recounted[1], CUR - BUCKET_SECS);

    let cur = |col: &str, app: i64| {
        figure(
            &db,
            &format!("SELECT {col} FROM stats_bucket WHERE appid = {app} AND bucket = {CUR}"),
        )
    };
    assert_eq!(cur("returned_ok", 0), 2);
    assert_eq!(cur("valid", 0), 1);
    assert_eq!(cur("invalid", 0), 1);
    assert_eq!(cur("failed", 0), 1);
    assert_eq!((cur("users", 0), cur("hosts", 0)), (2, 2));
    assert_eq!((cur("returned_ok", 1), cur("failed", 2)), (2, 1));
    assert_eq!(cur("queued", 0), 4);
    assert_eq!(cur("queued", 1), 3);
    // A period counted only by backfill has no queue figure: unknown, -1.
    assert_eq!(
        figure(
            &db,
            &format!(
                "SELECT queued FROM stats_bucket WHERE appid = 0 AND bucket = {}",
                CUR - BUCKET_SECS
            )
        ),
        -1
    );

    let (overview, history) = block_on(cb_core::stats::load_work(&db, NOW)).unwrap();
    let o = overview.unwrap();
    let app1 = o.apps.iter().find(|a| a.id == 1).unwrap();
    assert_eq!(
        (app1.unsent, app1.in_progress, app1.wait_validate),
        (3, 3, 1)
    );
    assert_eq!(app1.ok_24h, 3);
    assert_eq!(app1.ok_7d, 3);
    assert_eq!((app1.err_24h, app1.err_7d), (1, 1));
    let app2 = o.apps.iter().find(|a| a.id == 2).unwrap();
    assert_eq!(
        (app2.unsent, app2.wait_assimilate, app2.in_progress),
        (1, 1, 1)
    );
    let total = o.apps.iter().find(|a| a.id == 0).unwrap();
    assert_eq!((total.unsent, total.in_progress, total.ok_7d), (4, 4, 3));
    assert_eq!((o.working_users, o.working_hosts), (2, 3));
    assert_eq!(history.len(), 56);
    assert!(history[55].is_some() && history[0].is_none());

    // Not again within the interval.
    let again = block_on(rollup_pass(&db, NOW + MIN_INTERVAL_SECS - 1, 0, BUDGET)).unwrap();
    assert!(!again.ran);
    // Repeating it leaves the same rows.
    let before = dump(&db);
    let again = block_on(rollup_pass(&db, NOW + MIN_INTERVAL_SECS + 10, 0, BUDGET)).unwrap();
    assert!(again.ran);
    // The same rows, plus the older periods the second pass backfilled.
    let after = dump(&db);
    assert!(after.len() > before.len());
    assert!(before.iter().all(|r| after.contains(r)));
    let o = block_on(cb_core::stats::load_work(&db, NOW))
        .unwrap()
        .0
        .unwrap();
    let app1 = o.apps.iter().find(|a| a.id == 1).unwrap();
    assert_eq!((app1.ok_24h, app1.ok_7d), (3, 4));
    let before = after;
    let again = block_on(rollup_pass(
        &db,
        NOW + 2 * MIN_INTERVAL_SECS + 20,
        0,
        BUDGET,
    ))
    .unwrap();
    assert!(again.ran);
    // Rows already counted do not change when counted again.
    let now_rows = dump(&db);
    assert!(before.iter().all(|r| now_rows.contains(r)));

    // After the period closes it is counted once more, once an hour has
    // passed, and then left alone.
    let late = CUR + BUCKET_SECS + 4000;
    let r = block_on(rollup_pass(&db, late, 0, BUDGET)).unwrap();
    assert!(r.recounted.contains(&CUR));
    let r = block_on(rollup_pass(&db, late + 300, 0, BUDGET)).unwrap();
    assert!(!r.recounted.contains(&CUR));
    assert_eq!(cur("returned_ok", 0), 2);
    let _ = project;
}

#[test]
fn the_rollup_leaves_out_periods_before_the_purge_horizon() {
    let (db, _) = setup();
    busy(&db);
    let r = block_on(rollup_pass(&db, NOW, CUR - BUCKET_SECS, BUDGET)).unwrap();
    assert_eq!(r.recounted, vec![CUR, CUR - BUCKET_SECS]);
    assert_eq!(
        figure(&db, "SELECT count(*) FROM stats_bucket WHERE appid = 0"),
        2
    );
}

#[test]
fn rollup_queries_read_indexes_and_stay_bounded() {
    let (db, _) = setup();
    busy(&db);
    let spy = Spy {
        inner: &db,
        log: RefCell::new(Vec::new()),
    };
    block_on(rollup_pass(&spy, NOW, 0, BUDGET)).unwrap();
    let lines = plans(&spy).join("\n");
    for index in [
        "res_received",
        "res_unsent",
        "res_host_inprog",
        "wu_assim",
        "wu_need_validate",
    ] {
        assert!(lines.contains(index), "{index} is not used:\n{lines}");
    }
    // The in-progress counts come from the index alone.
    assert!(lines.contains("COVERING INDEX res_host_inprog"), "{lines}");
    // Few statements: first reads, counts (two per period, one per app, four
    // more), the writes and the sums, the snapshots.
    let n = spy.log.borrow().len();
    assert!(n < 50, "{n} statements");
}

#[test]
fn work_page_shows_counts_charts_and_unknowns() {
    let (db, project) = setup();
    let before = get(&db, &project, "/work.php", &query(&[]));
    assert!(before.contains("The counts are not available yet"));
    busy(&db);
    block_on(rollup_pass(&db, NOW, 0, BUDGET)).unwrap();
    let page = get(&db, &project, "/work.php", &query(&[]));
    assert!(page.contains("Worker &lt;one&gt;"));
    assert!(!page.contains("Worker <one>"));
    assert!(page.contains("Waiting to send") && page.contains("To validate"));
    assert!(page.contains("counted just now"));
    assert_eq!(page.matches("<svg class=\"chart\"").count(), 3);
    // Each chart has a table of its values, and periods without a count say
    // "unknown" there instead of 0.
    assert_eq!(page.matches("<details>").count(), 3);
    assert!(page.contains("class=\"num muted\">unknown"));
    assert!(!page.contains("style="));
    assert!(!page.contains("<script"));
    // No project panels without a feed.
    assert!(!page.contains("Project panels"));
}

#[test]
fn project_panels_are_shown_hidden_or_replaced_by_a_note() {
    let (db, project) = setup();
    let feed = parse_feed(
        &format!(
            r#"{{"schema":1,"updated":{},"sections":[{{"title":"Corpus <x>","figures":[
            {{"label":"Pages","value":1234567,"unit":"pages"}},{{"label":"Accuracy","value":null}}]}}]}}"#,
            NOW - 30
        ),
        NOW,
    )
    .unwrap();
    let render = |state: FeedState| {
        block_on(work(&ctx(&db, &project, NOW), &query(&[]), &state))
            .unwrap()
            .body
    };
    let ready = render(FeedState::Ready(feed));
    assert!(ready.contains("Corpus &lt;x&gt;"));
    assert!(ready.contains("1,234,567"));
    assert!(ready.contains("muted\">unknown<"));
    assert!(ready.contains("badge ok\">updated just now"));
    // The rest of the page does not depend on the feed.
    assert!(ready.contains("not available yet"));
    let gone = render(FeedState::Unavailable);
    assert!(gone.contains("not available right now"));
    assert!(!gone.contains("Corpus"));
    assert!(!render(FeedState::NotConfigured).contains("Project panels"));
}

#[test]
fn activity_names_computers_only_for_owners_who_show_them() {
    let (db, project) = setup_with(STOCK_CONFIG);
    busy(&db);
    add_user(&db, 3, "Gone", 1.0, 1.0, 1, 1, 1);
    insert_result(&db, 30, 1, 4, 0, 0, 0, 3, 3);
    block_on(rollup_pass(&db, NOW, 0, BUDGET)).unwrap();
    let page = block_on(activity(&ctx(&db, &project, NOW), &query(&[])))
        .unwrap()
        .body;
    assert!(page.contains("show_user.php?userid=1\">Alice</a>"));
    assert!(page.contains("show_user.php?userid=2\">Bob</a>"));
    // Alice shows her computers: both are listed, with their tasks.
    assert!(page.contains("show_host_detail.php?hostid=1\">Computer #1</a>"));
    assert!(page.contains("show_host_detail.php?hostid=2\">Computer #2</a>"));
    // Bob does not: his computer is counted and not named.
    assert!(!page.contains("hostid=3"));
    // A blocked account is left out.
    assert!(!page.contains("Gone"));
    for secret in ["secret-pc.lan", "10.9.8.7", "example.com", "AUTHKEY"] {
        assert!(!page.contains(secret), "{secret}");
    }
    assert!(page.contains("Worker &lt;one&gt;: 3"));
    // By default Bob's computer is listed too.
    let (db3, project3) = setup();
    busy(&db3);
    block_on(rollup_pass(&db3, NOW, 0, BUDGET)).unwrap();
    let page = block_on(activity(&ctx(&db3, &project3, NOW), &query(&[])))
        .unwrap()
        .body;
    assert!(page.contains("show_host_detail.php?hostid=3\">Computer #3</a>"));
    assert!(!page.contains("secret-pc.lan") && !page.contains("10.9.8.7"));
    // And a project with nothing in progress says so.
    let (db2, project2) = setup();
    block_on(rollup_pass(&db2, NOW, 0, BUDGET)).unwrap();
    let page = block_on(activity(&ctx(&db2, &project2, NOW), &query(&[])))
        .unwrap()
        .body;
    assert!(page.contains("No task is in progress"));
}

#[test]
fn activity_reads_one_snapshot_and_looks_accounts_up_by_id() {
    let (db, project) = setup();
    busy(&db);
    block_on(rollup_pass(&db, NOW, 0, BUDGET)).unwrap();
    let spy = Spy {
        inner: &db,
        log: RefCell::new(Vec::new()),
    };
    block_on(activity(&ctx(&spy, &project, NOW), &query(&[]))).unwrap();
    let log = spy.log.borrow();
    assert_eq!(log.len(), 2);
    assert!(log[0].sql.contains("stats_snapshot"));
    assert!(log[1].sql.contains("FROM user WHERE id IN"));
    assert!(log[1].params.len() <= 100);
    drop(log);
    plans(&spy);
}

#[test]
fn the_paths_are_routed_and_cacheable() {
    for p in PUBLIC_PATHS {
        assert!(SITE_PATHS.contains(&p), "{p}");
        assert!(public_cache_key(p, &query(&[("junk", "1")])).is_some());
    }
    assert_eq!(public_cache_key("/home.php", &query(&[])), None);
    assert_eq!(
        public_cache_key("/work.php", &query(&[("a", "b")])).unwrap(),
        "/work.php"
    );
    assert_eq!(
        public_cache_key(
            "/top_hosts.php",
            &query(&[("sort", "valid_results"), ("page", "2")])
        )
        .unwrap(),
        "/top_hosts.php?sort=valid_results&page=2"
    );
}

#[test]
fn the_option_is_read_from_the_config() {
    let p = |c: &str| cb_core::Config::parse(c).unwrap().host_owner_public;
    assert!(p("<config></config>"));
    assert!(p(
        "<config><host_owner_public>1</host_owner_public></config>"
    ));
    assert!(!p(
        "<config><host_owner_public>0</host_owner_public></config>"
    ));
}

#[test]
fn the_pages_answer_under_a_stock_page_prefix_and_link_relatively() {
    use cb_core::stock::{route_web, WebRoute};
    let config = cb_core::Config::parse(
        "<config><master_url>https://p.example/</master_url><stock_short_name>testproj</stock_short_name></config>",
    )
    .unwrap();
    let stock = config.stock.as_ref().unwrap();
    assert_eq!(stock.page_prefix, "/testproj");
    let (db, project) = setup();
    busy(&db);
    block_on(rollup_pass(&db, NOW, 0, BUDGET)).unwrap();
    for path in PUBLIC_PATHS {
        // The web Worker routes the prefixed path to the page, which is
        // named at the root, and the cache key is the root's.
        assert_eq!(
            route_web(&format!("/testproj{path}"), Some(stock)),
            WebRoute::Page {
                path,
                prefixed: true
            }
        );
        // A page links only relatively, so the same HTML serves both places.
        let body = get(&db, &project, path, &query(&[]));
        for bad in ["href=\"/", "src=\"/", "action=\"/", "https://p.example"] {
            assert!(!body.contains(bad), "{path}: {bad}");
        }
    }
}

#[test]
fn feed_url_is_checked_when_the_config_is_stored() {
    let db = SqliteDb::with_schema().unwrap();
    for (url, ok) in [
        ("https://example.org/feed.json", true),
        ("http://example.org/feed.json", false),
        ("https://user:pw@example.org/feed.json", false),
        ("", true),
    ] {
        let config = format!(
            "<config><master_url>https://p.example/</master_url><dashboard_feed_url>{url}</dashboard_feed_url></config>"
        );
        assert_eq!(
            block_on(put_setting(&db, SETTING_CONFIG, &config, 1)).is_ok(),
            ok,
            "{url}"
        );
    }
}

/// Whether every element that has a closing tag is closed, in order. Void
/// elements and self-closed ones (SVG shapes) are not tracked.
fn balanced(html: &str) -> Result<(), String> {
    const VOID: [&str; 6] = ["meta", "link", "br", "input", "img", "hr"];
    let mut stack: Vec<String> = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find('<') {
        rest = &rest[i + 1..];
        if rest.starts_with("!--") {
            rest = &rest[rest.find("-->").map_or(rest.len(), |j| j + 3)..];
            continue;
        }
        if rest.starts_with('!') || rest.starts_with('?') {
            continue;
        }
        let end = rest.find('>').ok_or("unterminated tag")?;
        let tag = &rest[..end];
        rest = &rest[end + 1..];
        let closing = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        if tag.ends_with('/') || VOID.contains(&name.as_str()) {
            continue;
        }
        if closing {
            match stack.pop() {
                Some(open) if open == name => {}
                other => return Err(format!("</{name}> closes {other:?}")),
            }
        } else {
            stack.push(name.clone());
            // Text of these is not markup.
            if name == "style" {
                rest = &rest[rest.find("</style>").ok_or("open style")?..];
            }
        }
    }
    if stack.is_empty() {
        Ok(())
    } else {
        Err(format!("unclosed {stack:?}"))
    }
}

#[test]
fn every_dashboard_page_has_balanced_markup() {
    use cb_core::web::master_page_with;
    let (db, project) = setup();
    busy(&db);
    add_user(&db, 7, "Showy", 5.0, 1.0, 1, 1, 0);
    block_on(rollup_pass(&db, NOW, 0, BUDGET)).unwrap();
    let feed = parse_feed(
        &format!(
            r#"{{"schema":1,"updated":{},"sections":[{{"title":"A","figures":[{{"label":"L","value":1}}],
            "series":[{{"title":"S","start":{},"interval_secs":86400,"values":[1,null,3]}}]}}]}}"#,
            NOW - 5,
            NOW - 4 * 86400
        ),
        NOW,
    )
    .unwrap();
    let overview = block_on(cb_core::stats::load_overview(&db)).unwrap();
    let mut pages = vec![
        (
            "work",
            block_on(work(
                &ctx(&db, &project, NOW),
                &query(&[]),
                &FeedState::Ready(feed),
            ))
            .unwrap()
            .body,
        ),
        (
            "work without feed",
            get(&db, &project, "/work.php", &query(&[])),
        ),
        ("activity", get(&db, &project, "/activity.php", &query(&[]))),
        ("home", master_page_with(&project, overview.as_ref())),
    ];
    for sort in ["total_credit", "expavg_credit", "valid_results"] {
        for path in ["/top_users.php", "/top_hosts.php"] {
            pages.push((path, get(&db, &project, path, &query(&[("sort", sort)]))));
        }
    }
    // And the empty states.
    let (db2, project2) = setup();
    for path in [
        "/top_users.php",
        "/top_hosts.php",
        "/work.php",
        "/activity.php",
    ] {
        pages.push((path, get(&db2, &project2, path, &query(&[]))));
    }
    for (name, html) in &pages {
        balanced(html).unwrap_or_else(|e| panic!("{name}: {e}"));
        // Every table of a dashboard page is in exactly one scroll box.
        assert_eq!(
            html.matches("<table").count(),
            html.matches("<div class=\"scroll\"><table").count(),
            "{name}"
        );
    }
    // The checker itself.
    assert!(balanced("<div><p></div>").is_err());
    assert!(balanced("<div><svg><path d=\"\"/></svg></div>").is_ok());
}

#[test]
fn a_period_with_tasks_waiting_for_validation_is_recounted() {
    let (db, _) = setup();
    add_user(&db, 1, "A", 1.0, 1.0, 1, 0, 0);
    add_host(&db, 1, 1, 1.0, 1);
    let prev = CUR - BUCKET_SECS;
    // Returned in the previous period, not yet judged.
    insert_result(&db, 1, 1, 5, 1, 0, prev + 500, 1, 1);
    // Counted twice at the start (for the backfill, then the early recount),
    // then the period is closed and settled.
    let mut t = NOW;
    for _ in 0..3 {
        block_on(rollup_pass(&db, t, 0, BUDGET)).unwrap();
        t += 600;
    }
    let pending = figure(
        &db,
        &format!("SELECT pending FROM stats_bucket WHERE appid = 0 AND bucket = {prev}"),
    );
    assert_eq!(pending, 1);
    // A day later the validator calls it invalid (it waited for a partner).
    block_on(db.run(stmt!("UPDATE result SET validate_state = 2 WHERE id = 1"))).unwrap();
    let day_later = NOW + 86_400;
    let r = block_on(rollup_pass(&db, day_later, 0, BUDGET)).unwrap();
    assert!(r.recounted.contains(&prev), "{:?}", r.recounted);
    let row = |col: &str| {
        figure(
            &db,
            &format!("SELECT {col} FROM stats_bucket WHERE appid = 0 AND bucket = {prev}"),
        )
    };
    assert_eq!((row("invalid"), row("pending")), (1, 0));
    // Nothing waits now: it is final and is not counted again.
    let r = block_on(rollup_pass(&db, day_later + 7200, 0, BUDGET)).unwrap();
    assert!(!r.recounted.contains(&prev));
}

#[test]
fn a_period_that_straddles_the_purge_horizon_is_left_unknown() {
    let (db, _) = setup();
    busy(&db);
    let horizon = CUR - BUCKET_SECS + 100;
    let r = block_on(rollup_pass(&db, NOW, horizon, BUDGET)).unwrap();
    assert_eq!(r.recounted, vec![CUR]);
    let (overview, history) = block_on(load_work(&db, NOW)).unwrap();
    assert!(history[54].is_none() && history[55].is_some());
    let o = overview.unwrap();
    // Only the open period can be counted, so the sums say they are partial.
    assert_eq!((o.expected_24h, o.periods_24h), (1, 1));
    assert!(o.day_complete());
}

#[test]
fn an_application_with_no_tasks_left_in_a_recount_is_zeroed() {
    let (db, _) = setup();
    let (s, o, v) = (5, 1, 1);
    insert_result(&db, 1, 1, s, o, v, CUR + 10, 1, 1);
    insert_result(&db, 2, 2, s, o, v, CUR + 20, 1, 1);
    block_on(rollup_pass(&db, NOW, 0, BUDGET)).unwrap();
    let get_n = |app: i64| {
        figure(
            &db,
            &format!("SELECT returned_ok FROM stats_bucket WHERE appid = {app} AND bucket = {CUR}"),
        )
    };
    assert_eq!((get_n(0), get_n(1), get_n(2)), (2, 1, 1));
    // A purge takes app 2's task; the next count writes zero for it.
    block_on(db.run(stmt!("DELETE FROM result WHERE id = 2"))).unwrap();
    block_on(rollup_pass(&db, NOW + 300, 0, BUDGET)).unwrap();
    assert_eq!((get_n(0), get_n(1), get_n(2)), (1, 1, 0));
    // Its queue length and timeouts are not touched by a recount.
    block_on(db.run(stmt!(
        "UPDATE stats_bucket SET timed_out = 4 WHERE appid = 2 AND bucket = ?",
        CUR
    )))
    .unwrap();
    block_on(rollup_pass(&db, NOW + 600, 0, BUDGET)).unwrap();
    assert_eq!(
        figure(
            &db,
            &format!("SELECT timed_out FROM stats_bucket WHERE appid = 2 AND bucket = {CUR}")
        ),
        4
    );
}

#[test]
fn a_pass_stays_inside_its_statement_budget_and_carries_the_rest() {
    // Many applications, and tasks in every period of the week.
    let (db, _) = setup();
    add_user(&db, 1, "A", 1.0, 1.0, 1, 0, 0);
    add_host(&db, 1, 1, 1.0, 1);
    let mut id = 1;
    for period in 0..56 {
        for app in 1..=40 {
            insert_result(&db, id, app, 5, 1, 1, CUR - period * BUCKET_SECS + 60, 1, 1);
            id += 1;
        }
    }
    for app in 1..=40 {
        insert_result(&db, 100_000 + app, app, 2, 0, 0, 0, 0, 0);
    }
    for budget in [MIN_BUDGET, 25, BUDGET, 50] {
        let spy = Spy {
            inner: &db,
            log: RefCell::new(Vec::new()),
        };
        let r = block_on(rollup_pass(&spy, NOW + budget as i64 * 1000, 0, budget)).unwrap();
        let used = spy.log.borrow().len();
        assert!(r.ran);
        assert_eq!(used, r.statements, "budget {budget}");
        assert!(used <= budget, "budget {budget}: {used} statements");
        // No statement binds more than D1 allows.
        assert!(spy.log.borrow().iter().all(|s| s.params.len() <= 100));
    }
    // Too little budget does nothing, and says so.
    let r = block_on(rollup_pass(&db, NOW + 90_000, 0, MIN_BUDGET - 1)).unwrap();
    assert!(r.skipped_for_budget && !r.ran);
    // Passes at the smallest budget still count the whole week, a few periods
    // each, newest first, and the open period every time.
    let (db, _) = setup();
    add_user(&db, 1, "A", 1.0, 1.0, 1, 0, 0);
    add_host(&db, 1, 1, 1.0, 1);
    for period in 0..48 {
        insert_result(
            &db,
            period + 1,
            1,
            5,
            1,
            1,
            CUR - period * BUCKET_SECS + 60,
            1,
            1,
        );
    }
    let mut t = NOW;
    let mut first_counted = None;
    for pass in 0..40 {
        let r = block_on(rollup_pass(&db, t, 0, 20)).unwrap();
        assert!(r.ran && r.statements <= 20, "pass {pass}: {}", r.statements);
        assert_eq!(r.recounted[0], cb_core::stats::bucket_start(t));
        first_counted.get_or_insert(r.recounted.len());
        t += 300;
    }
    // Every period that has a task is counted, each with its one task.
    let (_, history) = block_on(load_work(&db, t)).unwrap();
    let counted = history
        .iter()
        .flatten()
        .filter(|p| p.returned_ok == 1)
        .count();
    assert_eq!(counted, 48);
}

#[test]
fn the_figures_say_so_while_the_week_is_still_being_counted() {
    let (db, project) = setup();
    busy(&db);
    block_on(rollup_pass(&db, NOW, 0, BUDGET)).unwrap();
    let page = get(&db, &project, "/work.php", &query(&[]));
    assert!(page.contains("Counting is still catching up"), "{page}");
    assert!(page.contains("3+"));
    let o = block_on(cb_core::stats::load_overview(&db))
        .unwrap()
        .unwrap();
    assert!(!o.week_complete() && !o.day_complete());
    assert_eq!((o.periods_24h, o.expected_24h), (7, 8));
    let mut t = NOW;
    for _ in 0..12 {
        t += 300;
        block_on(rollup_pass(&db, t, 0, BUDGET)).unwrap();
    }
    let o = block_on(cb_core::stats::load_overview(&db))
        .unwrap()
        .unwrap();
    assert!(o.week_complete());
    let page = block_on(work(
        &ctx(&db, &project, t),
        &query(&[]),
        &FeedState::NotConfigured,
    ))
    .unwrap()
    .body;
    assert!(!page.contains("Counting is still catching up"));
}

/// Inserts `n` rows into `result` with a recursive query: unsent tasks of
/// `app`, or in progress on host `ids + i`.
fn bulk_unsent(db: &SqliteDb, first_id: i64, n: i64, app: i64) {
    block_on(db.run(stmt!(
        "WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < ?) \
         INSERT INTO result (id, create_time, workunitid, server_state, name, xml_doc_in, appid) \
         SELECT ? + i, 1, 1, 2, 'u' || (? + i), '', ? FROM c",
        n,
        first_id,
        first_id,
        app
    )))
    .unwrap();
}

#[test]
fn a_queue_past_the_cap_is_a_lower_bound_and_names_what_it_did_not_count() {
    let (db, _) = setup();
    block_on(db.run(stmt!("INSERT INTO app (id, create_time, name, user_friendly_name) VALUES (3, 1, 'third', 'Third')"))).unwrap();
    let project = block_on(Project::load(&db)).unwrap();
    bulk_unsent(&db, 1_000_000, 60_000, 1);
    bulk_unsent(&db, 2_000_000, 40_100, 2);
    bulk_unsent(&db, 3_000_000, 7, 3);
    block_on(rollup_pass(&db, NOW, 0, BUDGET)).unwrap();
    let o = block_on(cb_core::stats::load_overview(&db))
        .unwrap()
        .unwrap();
    let f = |id: i64| {
        o.apps
            .iter()
            .find(|a| a.id == id)
            .cloned()
            .unwrap_or_default()
    };
    // Application 1 is counted in full; 2 is cut at the cap; 3 was not reached.
    assert_eq!((f(1).unsent, f(1).unsent_capped), (60_000, false));
    assert_eq!((f(2).unsent, f(2).unsent_capped), (40_000, true));
    assert_eq!(o.unsent_from, 2);
    assert_eq!((f(0).unsent, f(0).unsent_capped), (UNSENT_COUNT_CAP, true));
    assert!(o.unsent_is_lower_bound(3) && !o.unsent_is_lower_bound(1));
    let page = get(&db, &project, "/work.php", &query(&[]));
    assert!(page.contains(">60,000<"));
    assert!(page.contains(">40,000+<"));
    // The third application is in the table, as "0+": at least none.
    assert!(page.contains("Third"));
    assert!(page.contains(">0+<"));
    assert!(page.contains(">100,000+<"));
    // And the home page's headline says the same.
    let home = cb_core::web::master_page_with(&project, Some(&o));
    assert!(home.contains("100,000+"));
}

#[test]
fn headline_counts_do_not_depend_on_the_rows_the_snapshot_keeps() {
    let (db, project) = setup();
    add_user(&db, 1, "A", 1.0, 1.0, 1, 0, 0);
    add_user(&db, 2, "B", 1.0, 1.0, 1, 0, 0);
    // 4,100 computers (half each of two accounts), one task in progress each.
    block_on(db.run(stmt!(
        "WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 4100) \
         INSERT INTO host (id, create_time, userid, rpc_time) SELECT i, 1, 1 + i % 2, ? FROM c",
        NOW - 60
    )))
    .unwrap();
    block_on(db.run(stmt!(
        "WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 4100) \
         INSERT INTO result (id, create_time, workunitid, server_state, name, xml_doc_in, appid, hostid, userid) \
         SELECT 10000 + i, 1, 1, 4, 'p' || i, '', 1, i, 1 + i % 2 FROM c"
    )))
    .unwrap();
    block_on(rollup_pass(&db, NOW, 0, BUDGET)).unwrap();
    let snap = block_on(load_inflight(&db)).unwrap().unwrap();
    assert!(snap.truncated);
    assert_eq!(snap.rows.len(), 4000);
    assert_eq!((snap.working_hosts, snap.working_users), (4100, 2));
    let o = block_on(cb_core::stats::load_overview(&db))
        .unwrap()
        .unwrap();
    assert_eq!((o.working_hosts, o.working_users), (4100, 2));
    let page = block_on(activity(&ctx(&db, &project, NOW), &query(&[])))
        .unwrap()
        .body;
    assert!(page.contains(">4,100<"));
    // The tasks are summed from the rows kept, so they are a lower bound.
    assert!(page.contains("4,000+"));
    assert!(page.contains("not complete"));
}

#[test]
fn a_row_a_timeout_made_is_not_a_counted_period_and_retention_still_runs() {
    let (db, project) = setup();
    // As `timeout_statements` leaves it: a timeout in the open period, before
    // any pass has counted it; and a period long past the retention.
    block_on(db.batch(vec![
        stmt!("INSERT INTO stats_bucket (appid, bucket, timed_out) VALUES (0, ?, 3)", CUR),
        stmt!("INSERT INTO stats_bucket (appid, bucket, timed_out) VALUES (1, ?, 3)", CUR),
        stmt!("INSERT INTO stats_bucket (appid, bucket, returned_ok, computed_at) VALUES (0, ?, 9, 5)", CUR - 100 * BUCKET_SECS),
    ]))
    .unwrap();
    let (_, history) = block_on(load_work(&db, NOW)).unwrap();
    assert!(history[55].is_none(), "an uncounted period is unknown");
    let page = get(&db, &project, "/work.php", &query(&[]));
    assert!(!page.contains("data-label") || history[55].is_none());
    block_on(rollup_pass(&db, NOW, 0, BUDGET)).unwrap();
    let (_, history) = block_on(load_work(&db, NOW)).unwrap();
    let p = history[55].clone().unwrap();
    assert_eq!(p.timed_out, 3);
    assert_eq!(
        figure(
            &db,
            &format!(
                "SELECT count(*) FROM stats_bucket WHERE bucket = {}",
                CUR - 100 * BUCKET_SECS
            )
        ),
        0,
        "old rows are deleted"
    );
    // The timeout survives the recount, and is in the sums.
    let o = block_on(cb_core::stats::load_overview(&db))
        .unwrap()
        .unwrap();
    assert_eq!(o.apps.iter().find(|a| a.id == 0).unwrap().err_24h, 3);
}

#[test]
fn the_last_leaderboard_page_has_no_next_link() {
    let (db, project) = setup();
    block_on(db.run(stmt!(
        "WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 2100) \
         INSERT INTO user (id, create_time, email_addr, name, authenticator, total_credit, recent_key) \
         SELECT i, 1, 'e' || i, 'U' || i, 'a' || i, 5000 - i, 1.0 FROM c"
    )))
    .unwrap();
    let last = get(
        &db,
        &project,
        "/top_users.php",
        &query(&[("page", &MAX_PAGE.to_string())]),
    );
    assert_eq!(rows_in(&last), PAGE_SIZE);
    assert!(!last.contains("Next"));
    assert!(last.contains("Previous"));
    let before = get(
        &db,
        &project,
        "/top_users.php",
        &query(&[("page", &(MAX_PAGE - 1).to_string())]),
    );
    assert!(before.contains("Next"));
}
