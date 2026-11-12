//! `POST /sched`: the scheduler RPC.

use cb_core::config::DEFAULT_MAINTENANCE_DELAY;
use cb_core::scheduler::{handle_request, SchedCtx};
use cb_core::stock::route_scheduler;
use cb_core::Config;
use cb_proto::sched_reply::write_project_down;
use cb_worker::{config, db, enqueue, now, project, random_u32, read_body, text, xml};
use worker::{console_error, console_log, event, Context, Env, Method, Request, Response, Result};

/// A scheduler request with a full host description and a few hundred
/// reported results is well under this.
const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;

/// The reply for a server that cannot work (`send_message` in
/// `sched/sched_main.cpp`): the config's `maintenance_delay`, and `<ended>`
/// for a project that has ended. Before the config is loaded, or when it
/// cannot be, the default delay.
fn down(message: &str, config: Option<&Config>) -> Result<Response> {
    let delay = config.map_or(DEFAULT_MAINTENANCE_DELAY, |c| c.maintenance_delay);
    let extra = if config.is_some_and(|c| c.ended) {
        "    <ended>1</ended>\n"
    } else {
        ""
    };
    xml(write_project_down(
        message,
        delay.clamp(0.0, f64::from(i32::MAX)) as i32,
        extra,
    ))
}

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, ctx: Context) -> Result<Response> {
    let path = req.path();
    if !route_scheduler(&path, None) {
        // Not the native path: the stock scheduler path answers only when
        // the config names a stock layout.
        let db = db(&env)?;
        let stock = match config(&db).await {
            Ok(c) => c.stock.clone(),
            Err(e) => {
                console_error!("settings: {e}");
                return text(503, "The project is temporarily unavailable.\n");
            }
        };
        if !route_scheduler(&path, stock.as_ref()) {
            return text(404, "not found\n");
        }
    }
    if req.method() != Method::Post {
        return text(
            200,
            "This is the scheduler of a BOINC project. It answers POST requests from BOINC clients.\n",
        );
    }
    let started = cb_worker::now_ms();
    let Some(body) = read_body(&mut req, MAX_REQUEST_BYTES).await? else {
        return down("The request is too large.", None);
    };
    let body = String::from_utf8_lossy(&body);
    let db = db(&env)?;
    let project = match project(&db).await {
        Ok(p) => p,
        Err(e) => {
            console_error!("settings: {e}");
            return down("The project is temporarily unavailable.", None);
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
            return down(
                "The project is temporarily unavailable.",
                Some(&project.config),
            );
        }
    };
    let reply = match out.reply.write() {
        Ok(r) => r,
        Err(e) => {
            console_error!("reply: {e}");
            return down(
                "The project is temporarily unavailable.",
                Some(&project.config),
            );
        }
    };
    let elapsed = cb_worker::now_ms().saturating_sub(started);
    console_log!(
        "{} ms={} stmts={} trips={} read={} written={}",
        out.log,
        elapsed,
        db.statements(),
        db.round_trips(),
        db.rows_read(),
        cb_db::Db::rows_written(&db),
    );
    if !out.transition.is_empty() {
        let ids = out.transition;
        ctx.wait_until(async move { enqueue(&env, &ids).await });
    }
    let mut response = xml(reply)?;
    // Time spent in this Worker and its database round trips, for measuring
    // from outside.
    response.headers_mut().set(
        "Server-Timing",
        &format!("app;dur={elapsed}, db;desc=\"{} trips\"", db.round_trips()),
    )?;
    Ok(response)
}
