//! The purge cron's three jobs against SQLite, through a database that counts
//! the statements it is sent: a purge backlog, a queued account deletion and the
//! rollup share the 50 queries of an invocation, and each of them makes progress
//! in every run, whatever the settings load cost and however large the backlog.

use std::cell::{Cell, RefCell};

use async_trait::async_trait;
use cb_core::cron::{
    deletions_budget, purge_budget, rollup_budget, run_cron, DELETIONS_WANTED, FREE_PLAN_QUERIES,
    PURGE_PASS_STATEMENTS,
};
use cb_core::site_delete::DELETION_PROGRESS_STATEMENTS;
use cb_core::stats::MIN_BUDGET;
use cb_core::{Config, CoreResult, FileKind, FileStat, Files};
use cb_db::{stmt, Db, DbResult, Rows, Stmt};
use cb_sqlite::SqliteDb;
use futures_lite::future::block_on;

const NOW: i64 = 2_000_000_000;

/// Counts what is sent, and keeps it for a look.
struct Counting<'a> {
    inner: &'a SqliteDb,
    sent: Cell<usize>,
    log: RefCell<Vec<Stmt>>,
}

#[async_trait(?Send)]
impl Db for Counting<'_> {
    async fn batch(&self, stmts: Vec<Stmt>) -> DbResult<Vec<Rows>> {
        self.sent.set(self.sent.get() + stmts.len());
        self.log.borrow_mut().extend(stmts.iter().cloned());
        self.inner.batch(stmts).await
    }
}

#[derive(Default)]
struct Archive {
    objects: Cell<usize>,
}

#[async_trait(?Send)]
impl Files for Archive {
    async fn stat(&self, _: FileKind, _: &str) -> CoreResult<Option<FileStat>> {
        Ok(None)
    }
    async fn delete(&self, _: FileKind, _: &[String]) -> CoreResult<()> {
        Ok(())
    }
    async fn put(&self, kind: FileKind, _: &str, _: Vec<u8>) -> CoreResult<()> {
        assert_eq!(kind, FileKind::Archive);
        self.objects.set(self.objects.get() + 1);
        Ok(())
    }
}

fn n(db: &SqliteDb, sql: &str) -> i64 {
    block_on(db.run(Stmt::new(sql)))
        .unwrap()
        .scalar_i64()
        .unwrap()
}

/// 600 finished workunits old enough to purge, an account being deleted with
/// thousands of tasks and computers, and tasks returned in the last hours.
fn world() -> SqliteDb {
    let db = SqliteDb::with_schema().unwrap();
    block_on(db.batch(vec![
        stmt!("INSERT INTO app (id, create_time, name, user_friendly_name) VALUES (1, 1, 'a', 'A')"),
        stmt!(
            "INSERT INTO user (id, create_time, email_addr, name, authenticator, blocked) \
             VALUES (1, 1, 'deleted_1_x', 'gone', 'deleted_1_x', 1), \
             (2, 1, 'two@x', 'Two', 'k2', 0)"
        ),
        stmt!(
            "INSERT INTO user_deletion (userid, mode, create_time, gen) \
             VALUES (1, 2, 1, 'deleted_1_x')"
        ),
        // Workunits that are finished and long unchanged, each with one task.
        stmt!(
            "WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 600) \
             INSERT INTO workunit (id, create_time, appid, name, xml_doc, rsc_fpops_est, \
             rsc_fpops_bound, rsc_memory_bound, rsc_disk_bound, transition_time, delay_bound, \
             min_quorum, target_nresults, max_error_results, max_total_results, \
             max_success_results, result_template, file_delete_state, mod_time) \
             SELECT i, 1, 1, 'wu' || i, '', 1, 1, 1, 1, 2147483647, 1, 1, 1, 1, 1, 1, '', 2, 1 FROM c"
        ),
        stmt!(
            "WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 600) \
             INSERT INTO result (id, create_time, workunitid, server_state, outcome, name, \
             xml_doc_in, appid, received_time, userid, hostid, validate_state) \
             SELECT i, 1, i, 5, 1, 'r' || i, '', 1, 100, 2, 0, 1 FROM c"
        ),
        // The account being deleted: tasks and computers.
        stmt!(
            "WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 450) \
             INSERT INTO host (id, create_time, userid) SELECT i, 1, 1 FROM c"
        ),
        stmt!(
            "WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 12000) \
             INSERT INTO result (id, create_time, workunitid, server_state, outcome, name, \
             xml_doc_in, appid, received_time, userid, hostid) \
             SELECT 10000 + i, 1, 601, 5, 1, 'd' || i, '', 1, ?, 1, 1 + i % 450 FROM c",
            NOW - 600
        ),
        stmt!(
            "INSERT INTO workunit (id, create_time, appid, name, xml_doc, rsc_fpops_est, \
             rsc_fpops_bound, rsc_memory_bound, rsc_disk_bound, transition_time, delay_bound, \
             min_quorum, target_nresults, max_error_results, max_total_results, \
             max_success_results, result_template) \
             VALUES (601, 1, 1, 'wu601', '', 1, 1, 1, 1, 100, 1, 1, 1, 1, 1, 1, '')"
        ),
    ]))
    .unwrap();
    db
}

#[test]
fn the_shares_hold_whatever_the_purge_and_the_settings_cost() {
    // Whatever was used before, the three never ask for more than is left, and
    // while there is room the deletions and the rollup get their minimum.
    for before in 0..=8 {
        let mut used = before;
        let purge = purge_budget(used);
        // A purge that spends all it is given, in passes of the worst size.
        used += purge - purge % PURGE_PASS_STATEMENTS;
        let deletions = deletions_budget(used);
        assert!(deletions >= DELETION_PROGRESS_STATEMENTS, "before {before}");
        assert!(deletions <= DELETIONS_WANTED);
        used += deletions;
        let rollup = rollup_budget(used);
        assert!(rollup >= MIN_BUDGET, "before {before}: rollup {rollup}");
        assert!(used + rollup <= FREE_PLAN_QUERIES, "before {before}");
    }
    // A cold isolate with a four-statement settings load: one full pass of the
    // purge (7), the deletions 19, and the rollup the 20 that are left.
    assert_eq!(purge_budget(4), 12);
    assert_eq!(deletions_budget(4 + 7), 19);
    assert_eq!(rollup_budget(4 + 7 + 19), 18);
    // An ordinary run, where the purge has nothing: the deletions get all they want.
    assert_eq!(deletions_budget(4 + 1), DELETIONS_WANTED);
}

#[test]
fn purge_deletions_and_rollup_all_make_progress_every_run_within_fifty_statements() {
    let db = world();
    let files = Archive::default();
    let config = Config::parse(
        "<config><master_url>https://p.example/</master_url><purge_days>1</purge_days></config>",
    )
    .unwrap();
    let mut now = NOW;
    let mut runs = 0;
    let mut purged_total = 0;
    let (mut wu_left, mut tasks_left) = (
        n(&db, "SELECT count(*) FROM workunit"),
        n(&db, "SELECT count(*) FROM result WHERE userid = 1"),
    );
    // As long as any of the three has work, a run does some of each.
    while runs < 80 {
        let queued = n(&db, "SELECT count(*) FROM user_deletion");
        let purgeable = n(
            &db,
            "SELECT count(*) FROM workunit WHERE file_delete_state = 2 AND mod_time < 1000",
        );
        if queued == 0 && purgeable <= 2 {
            break;
        }
        runs += 1;
        // The settings load of a cold isolate: four statements, six as the worst.
        let before = if runs % 2 == 0 { 4 } else { 6 };
        let counting = Counting {
            inner: &db,
            sent: Cell::new(0),
            log: RefCell::new(Vec::new()),
        };
        let report = block_on(run_cron(&counting, &files, &config, now, before)).unwrap();
        let sent = counting.sent.get();
        assert_eq!(sent, report.statements, "run {runs}");
        assert!(
            before + sent <= FREE_PLAN_QUERIES,
            "run {runs}: {} statements",
            before + sent
        );
        // Each of the three made progress.
        if purgeable > 2 {
            assert!(report.purge.workunits > 0, "run {runs}: no purge progress");
            purged_total += report.purge.workunits;
        }
        if queued > 0 {
            // The queue was read and a step run, and the account has less left.
            assert!(
                report.deletions.statements > 1,
                "run {runs}: the deletion did not take a step: {:?}",
                report.deletions
            );
        }
        assert!(report.rollup.ran, "run {runs}: {:?}", report.rollup);
        let (wu_now, tasks_now) = (
            n(&db, "SELECT count(*) FROM workunit"),
            n(&db, "SELECT count(*) FROM result WHERE userid = 1"),
        );
        assert!(wu_now <= wu_left && tasks_now <= tasks_left);
        if queued > 0 && tasks_left > 0 {
            assert!(
                tasks_now < tasks_left,
                "run {runs}: the account's tasks stayed"
            );
        }
        if purgeable > 2 {
            assert!(wu_now < wu_left, "run {runs}: the backlog stayed");
        }
        (wu_left, tasks_left) = (wu_now, tasks_now);
        now += 300;
    }
    assert!(runs < 80, "still not done after {runs} runs");
    // The backlog went, in more than one run (a run purges at most a pass or
    // two), and the account is gone, in more than one.
    assert!(runs > 2, "{runs}");
    assert!(purged_total >= 590, "{purged_total}");
    assert_eq!(n(&db, "SELECT count(*) FROM user_deletion"), 0);
    assert_eq!(n(&db, "SELECT count(*) FROM result WHERE userid = 1"), 0);
    assert!(files.objects.get() > 1);
    // The rollup kept counting throughout.
    assert!(
        n(
            &db,
            "SELECT count(*) FROM stats_bucket WHERE computed_at > 0"
        ) > 0
    );
}
