//! The job pipeline: a consumer of the `transitions` queue, a cron sweep
//! for workunits whose `transition_time` has passed, and a second cron
//! that purges finished workunits, carries on account deletions and runs the
//! rollup behind the dashboards.

use std::collections::BTreeSet;

use cb_core::cron::run_cron;
use cb_core::pipeline::{due_workunits, transition, Ctx, TransitionReport};
use cb_core::status::record_sweep;
use cb_core::CoreResult;
use cb_worker::{db, enqueue, now, project, random_u32, signer, FetchWebhook, R2Files, BUCKET};
use worker::{
    console_error, console_log, event, Context, Env, MessageBatch, MessageExt, Result,
    ScheduleContext, ScheduledEvent,
};

/// Transitions the cron sweep runs itself; the rest go to the queue.
const SWEEP_INLINE: usize = 25;
const SWEEP_LIMIT: i64 = 500;
/// The cron expression of the purge trigger in `wrangler.toml`. Every other
/// trigger runs the sweep. The purge has an invocation of its own so that
/// its queries do not come out of the sweep's allowance.
const PURGE_CRON: &str = "*/5 * * * *";

struct Runner {
    db: cb_worker::D1Db,
    files: R2Files,
    webhook: FetchWebhook,
    project: std::rc::Rc<cb_core::project::Project>,
    signer: Option<std::rc::Rc<cb_crypto::Signer>>,
}

impl Runner {
    async fn new(env: &Env) -> Result<Runner> {
        let db = db(env)?;
        let project = project(&db).await.map_err(cb_worker::core_err)?;
        Ok(Runner {
            files: R2Files(env.bucket(BUCKET)?),
            webhook: FetchWebhook::from_env(env, project.config.webhook_timeout_secs),
            signer: signer(env)?,
            project,
            db,
        })
    }

    async fn run(&self, id: i64) -> CoreResult<TransitionReport> {
        let random = random_u32;
        let ctx = Ctx {
            db: &self.db,
            files: &self.files,
            webhook: &self.webhook,
            config: &self.project.config,
            signer: self.signer.as_deref(),
            now: now(),
            random: &random,
        };
        transition(&ctx, id).await
    }
}

#[event(queue)]
async fn queue(batch: MessageBatch<i64>, env: Env, _ctx: Context) -> Result<()> {
    let runner = Runner::new(&env).await?;
    let mut done = BTreeSet::new();
    let mut failed = BTreeSet::new();
    for message in batch.messages()? {
        let id = *message.body();
        if failed.contains(&id) {
            message.retry();
            continue;
        }
        if !done.insert(id) {
            message.ack();
            continue;
        }
        match runner.run(id).await {
            Ok(report) => {
                console_log!("transition {report:?}");
                message.ack();
            }
            Err(e) => {
                console_error!("transition {id}: {e}");
                done.remove(&id);
                failed.insert(id);
                message.retry();
            }
        }
    }
    Ok(())
}

async fn sweep(env: Env) -> Result<()> {
    let runner = Runner::new(&env).await?;
    // The status pages report from this whether the pipeline runs.
    if let Err(e) = record_sweep(&runner.db, now()).await {
        console_error!("sweep time: {e}");
    }
    let due = due_workunits(&runner.db, now(), SWEEP_LIMIT)
        .await
        .map_err(cb_worker::core_err)?;
    if due.is_empty() {
        return Ok(());
    }
    let inline = due.len().min(SWEEP_INLINE);
    enqueue(&env, &due[inline..]).await;
    let mut errors = 0;
    for id in &due[..inline] {
        if let Err(e) = runner.run(*id).await {
            console_error!("transition {id}: {e}");
            errors += 1;
        }
    }
    console_log!(
        "sweep: {} due, {} run here, {} errors",
        due.len(),
        inline,
        errors
    );
    Ok(())
}

/// The purge trigger's work: the purge, the account deletions the site queued
/// and the rollup behind the dashboards, sharing the 50 queries of an
/// invocation on the Free plan (`cb_core::cron`).
async fn purge_cron(env: &Env) -> Result<()> {
    let db = db(env)?;
    let project = project(&db).await.map_err(cb_worker::core_err)?;
    // What the settings load sent (nothing when this isolate has them).
    let used = db.statements() as usize;
    let files = R2Files(env.bucket(BUCKET)?);
    let report = run_cron(&db, &files, &project.config, now(), used)
        .await
        .map_err(cb_worker::core_err)?;
    if report.purge.workunits > 0 {
        console_log!(
            "purge: {} workunits, {} results in {} passes",
            report.purge.workunits,
            report.purge.results,
            report.purge_passes
        );
    }
    if report.deletions.finished > 0 || report.deletions.pending > 0 {
        console_log!(
            "account deletions: {} finished, {} pending",
            report.deletions.finished,
            report.deletions.pending
        );
    }
    if report.rollup.skipped_for_budget {
        console_log!("rollup: skipped for budget");
    }
    if report.rollup.ran {
        console_log!(
            "rollup: {} periods ({} postponed), {} apps, stmts={} trips={} read={} written={}",
            report.rollup.recounted.len(),
            report.rollup.postponed,
            report.rollup.apps,
            db.statements(),
            db.round_trips(),
            db.rows_read(),
            cb_db::Db::rows_written(&db),
        );
    }
    Ok(())
}

#[event(scheduled)]
async fn scheduled(event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    if event.cron() == PURGE_CRON {
        if let Err(e) = purge_cron(&env).await {
            console_error!("purge cron: {e}");
        }
    } else if let Err(e) = sweep(env).await {
        console_error!("sweep: {e}");
    }
}
