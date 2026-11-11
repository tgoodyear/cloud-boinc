//! The job pipeline: a consumer of the `transitions` queue, a cron sweep
//! for workunits whose `transition_time` has passed, and a second cron
//! that purges finished workunits.

use std::collections::BTreeSet;

use cb_core::pipeline::{due_workunits, transition, Ctx, TransitionReport};
use cb_core::purge::{purge_pass, MAX_PURGE_BATCH};
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
/// Purge passes in one run; a pass costs three database round trips and
/// holds at most `cb_core::purge::MAX_PURGE_BYTES` of rows.
const PURGE_PASSES: usize = 8;

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
            webhook: FetchWebhook::from_env(env),
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

async fn purge(env: Env) -> Result<()> {
    let db = db(&env)?;
    let project = project(&db).await.map_err(cb_worker::core_err)?;
    if project.config.purge_days <= 0.0 {
        return Ok(());
    }
    let files = R2Files(env.bucket(BUCKET)?);
    let (mut workunits, mut results) = (0, 0);
    for _ in 0..PURGE_PASSES {
        let report = purge_pass(&db, &files, &project.config, now(), MAX_PURGE_BATCH)
            .await
            .map_err(cb_worker::core_err)?;
        workunits += report.workunits;
        results += report.results;
        if report.archive.is_none() {
            break;
        }
    }
    if workunits > 0 {
        console_log!("purge: {workunits} workunits, {results} results");
    }
    Ok(())
}

#[event(scheduled)]
async fn scheduled(event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    if event.cron() == PURGE_CRON {
        if let Err(e) = purge(env).await {
            console_error!("purge: {e}");
        }
    } else if let Err(e) = sweep(env).await {
        console_error!("sweep: {e}");
    }
}
