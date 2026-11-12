//! What the pipeline's five-minute cron does, with one budget.
//!
//! A Worker invocation may make 50 queries on the Free plan. The cron does
//! three jobs in it: purges finished workunits (`purge`), carries on the
//! account deletions the site queued (`site_delete`), and runs the rollup behind
//! the dashboards (`stats`). Each has a count of statements it needs to make
//! progress, and what is left after the settings load is split before any of
//! them starts: the deletions and the rollup are reserved their minimum, the
//! purge takes the rest and leaves what it does not finish for the next run, and
//! the deletions take what the purge left, less the rollup's minimum.
//!
//! The statements are counted where they are sent ([`Counting`]), not assumed,
//! so a purge pass that finds nothing costs one and a full one seven.

use std::cell::Cell;

use async_trait::async_trait;
use cb_db::{Db, DbResult, Rows, Stmt};

use crate::config::Config;
use crate::purge::{purge_pass, PurgeReport, MAX_PURGE_BATCH};
use crate::site_delete::{delete_account_steps, DeletionReport, DELETION_PROGRESS_STATEMENTS};
use crate::stats::{rollup_pass, RollupReport, MIN_BUDGET};
use crate::{CoreResult, Files};

/// Queries a Worker invocation may make on the Free plan.
pub const FREE_PLAN_QUERIES: usize = 50;
/// Statements a purge pass sends at most: the candidates, the read of the rows,
/// and the four deletes of the commit, with the read batch counting two.
pub const PURGE_PASS_STATEMENTS: usize = 7;
/// Passes one run may make, whatever the budget.
const PURGE_PASSES: usize = 8;
/// Statements the account deletions may use in one run, at most.
pub const DELETIONS_WANTED: usize = 22;
/// Kept back for slack.
const MARGIN: usize = 2;

/// A database handle that counts the statements sent through it.
pub struct Counting<'a> {
    inner: &'a dyn Db,
    sent: Cell<usize>,
}

impl<'a> Counting<'a> {
    pub fn new(inner: &'a dyn Db) -> Self {
        Counting {
            inner,
            sent: Cell::new(0),
        }
    }

    pub fn sent(&self) -> usize {
        self.sent.get()
    }
}

#[async_trait(?Send)]
impl Db for Counting<'_> {
    async fn batch(&self, stmts: Vec<Stmt>) -> DbResult<Vec<Rows>> {
        self.sent.set(self.sent.get() + stmts.len());
        self.inner.batch(stmts).await
    }

    fn rows_written(&self) -> u64 {
        self.inner.rows_written()
    }
}

/// How many statements the purge may use when `used` have been sent already:
/// what is left of the 50 once the account deletions have what one step needs,
/// the rollup its minimum, and the margin.
pub fn purge_budget(used: usize) -> usize {
    FREE_PLAN_QUERIES
        .saturating_sub(used)
        .saturating_sub(DELETION_PROGRESS_STATEMENTS + MIN_BUDGET + MARGIN)
}

/// How many the account deletions may use after `used`: at most
/// [`DELETIONS_WANTED`], and never so many that the rollup has less than its
/// minimum.
pub fn deletions_budget(used: usize) -> usize {
    FREE_PLAN_QUERIES
        .saturating_sub(used)
        .saturating_sub(MIN_BUDGET + MARGIN)
        .min(DELETIONS_WANTED)
}

/// How many the rollup may use after `used`.
pub fn rollup_budget(used: usize) -> usize {
    FREE_PLAN_QUERIES.saturating_sub(used + MARGIN)
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct CronReport {
    pub purge: PurgeReport,
    pub purge_passes: usize,
    pub deletions: DeletionReport,
    pub rollup: RollupReport,
    /// Statements the three sent, not counting what was used before.
    pub statements: usize,
}

/// The cron: purge, then account deletions, then the rollup. `used_before` is
/// how many statements the invocation has sent already (the settings load).
pub async fn run_cron(
    db: &dyn Db,
    files: &dyn Files,
    config: &Config,
    now: i64,
    used_before: usize,
) -> CoreResult<CronReport> {
    let counted = Counting::new(db);
    let used = || used_before + counted.sent();
    let mut report = CronReport::default();

    // The purge, in passes while the budget holds the worst a pass can cost.
    if config.purge_days > 0.0 {
        for _ in 0..PURGE_PASSES {
            if purge_budget(used()) < PURGE_PASS_STATEMENTS {
                break;
            }
            let pass = purge_pass(&counted, files, config, now, MAX_PURGE_BATCH).await?;
            report.purge_passes += 1;
            report.purge.workunits += pass.workunits;
            report.purge.results += pass.results;
            if pass.archive.is_none() {
                break;
            }
            report.purge.archive = pass.archive;
        }
    }

    // The account deletions, with what the purge left.
    let room = deletions_budget(used());
    report.deletions = delete_account_steps(&counted, now, room).await?;

    // The rollup, with the rest. Periods that ended before the purge horizon
    // cannot be counted from what is left.
    let not_before = if config.purge_days > 0.0 {
        now - (config.purge_days * 86_400.0) as i64
    } else {
        0
    };
    report.rollup = rollup_pass(&counted, now, not_before, rollup_budget(used())).await?;
    report.statements = counted.sent();
    Ok(report)
}
