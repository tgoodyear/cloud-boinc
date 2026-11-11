//! `POST /sched`: the scheduler RPC.

use cb_core::scheduler::{handle_request, SchedCtx};
use cb_proto::sched_reply::write_project_down;
use cb_worker::{db, enqueue, now, project, random_u32, text, xml};
use worker::{console_error, console_log, event, Context, Env, Method, Request, Response, Result};

/// A scheduler request with a full host description and a few hundred
/// reported results is well under this.
const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;
const DOWN_DELAY_SECS: i32 = 600;

fn down(message: &str) -> Result<Response> {
    xml(write_project_down(message, DOWN_DELAY_SECS, ""))
}

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, ctx: Context) -> Result<Response> {
    if req.method() != Method::Post {
        return text(
            200,
            "This is the scheduler of a BOINC project. It answers POST requests from BOINC clients.\n",
        );
    }
    let started = cb_worker::now_ms();
    let body = req.bytes().await?;
    if body.len() > MAX_REQUEST_BYTES {
        return down("The request is too large.");
    }
    let body = String::from_utf8_lossy(&body);
    let db = db(&env)?;
    let project = match project(&db).await {
        Ok(p) => p,
        Err(e) => {
            console_error!("settings: {e}");
            return down("The project is temporarily unavailable.");
        }
    };
    let client_ip = req.headers().get("CF-Connecting-IP")?.unwrap_or_default();
    let random = random_u32;
    let sched = SchedCtx {
        db: &db,
        config: &project.config,
        plan_classes: &project.plan_classes,
        catalog: &project.catalog,
        code_sign_key: &project.code_sign_key,
        now: now(),
        random: &random,
        client_ip: &client_ip,
    };
    let out = match handle_request(&sched, &body).await {
        Ok(out) => out,
        Err(e) => {
            console_error!("scheduler: {e}");
            return down("The project is temporarily unavailable.");
        }
    };
    let reply = match out.reply.write() {
        Ok(r) => r,
        Err(e) => {
            console_error!("reply: {e}");
            return down("The project is temporarily unavailable.");
        }
    };
    console_log!(
        "{} ms={} stmts={} trips={} read={} written={}",
        out.log,
        cb_worker::now_ms().saturating_sub(started),
        db.statements(),
        db.round_trips(),
        db.rows_read(),
        cb_db::Db::rows_written(&db),
    );
    if !out.transition.is_empty() {
        let ids = out.transition;
        ctx.wait_until(async move { enqueue(&env, &ids).await });
    }
    xml(reply)
}
