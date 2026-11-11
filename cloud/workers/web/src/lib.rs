//! The master URL page, the web RPCs a client uses to attach, and the
//! server status. PHP file names stay in the URLs because clients have them
//! built in.

use std::collections::HashMap;

use cb_core::web::{
    create_account, lookup_account, master_page, not_implemented, project_config, server_status,
    server_status_page, server_status_xml, NewAccount,
};
use cb_proto::web_rpc::AccountError;
use cb_worker::{db, now, project, random_u32, secret_bytes, text, xml, SECRET_PASSWD_PEPPER};
use worker::{console_error, event, Context, Env, Method, Request, Response, Result};

fn html(body: String) -> Result<Response> {
    Ok(Response::builder()
        .with_header("Content-Type", "text/html; charset=utf-8")?
        .with_header("Cache-Control", "public, max-age=30")?
        .fixed(body.into_bytes()))
}

/// Query parameters, then form fields for a POST.
async fn params(req: &mut Request) -> Result<HashMap<String, String>> {
    let mut out: HashMap<String, String> = req
        .url()?
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if req.method() == Method::Post {
        let body = req.text().await?;
        if body.len() <= 16 * 1024 {
            for (k, v) in worker::Url::parse(&format!("http://x/?{body}"))?.query_pairs() {
                out.insert(k.into_owned(), v.into_owned());
            }
        }
    }
    Ok(out)
}

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    if !matches!(req.method(), Method::Get | Method::Head | Method::Post) {
        return text(405, "method not allowed\n");
    }
    let path = req.path();
    let db = db(&env)?;
    let project = match project(&db).await {
        Ok(p) => p,
        Err(e) => {
            console_error!("settings: {e}");
            return text(503, "The project is temporarily unavailable.\n");
        }
    };
    match path.as_str() {
        "/" | "/index.php" => html(master_page(&project)),
        "/get_project_config.php" => xml(project_config(&project)),
        "/server_status.php" => {
            let q = params(&mut req).await?;
            let now = now();
            let status = match server_status(&db, now).await {
                Ok(s) => s,
                Err(e) => {
                    console_error!("status: {e}");
                    return text(503, "The project is temporarily unavailable.\n");
                }
            };
            if q.get("xml").is_some_and(|v| v != "0") {
                xml(server_status_xml(&project, &status, now))
            } else {
                html(server_status_page(&project, &status))
            }
        }
        "/lookup_account.php" => {
            let q = params(&mut req).await?;
            let pepper = secret_bytes(&env, SECRET_PASSWD_PEPPER)?;
            let get = |k: &str| q.get(k).map(String::as_str).unwrap_or_default();
            match lookup_account(&db, &pepper, get("email_addr"), get("passwd_hash")).await {
                Ok(reply) => xml(reply),
                Err(e) => {
                    console_error!("lookup_account: {e}");
                    xml(AccountError::DbError.reply())
                }
            }
        }
        "/create_account.php" => {
            let q = params(&mut req).await?;
            let pepper = secret_bytes(&env, SECRET_PASSWD_PEPPER)?;
            let get = |k: &str| q.get(k).map(String::as_str).unwrap_or_default();
            let random = random_u32;
            let account = NewAccount {
                email_addr: get("email_addr"),
                passwd_hash: get("passwd_hash"),
                user_name: get("user_name"),
            };
            match create_account(&db, &project.config, &pepper, now(), &random, &account).await {
                Ok(reply) => xml(reply),
                Err(e) => {
                    console_error!("create_account: {e}");
                    xml(AccountError::DbError.reply())
                }
            }
        }
        "/am_get_info.php" | "/am_set_info.php" | "/am_set_host_info.php" | "/show_user.php" => {
            xml(not_implemented())
        }
        _ => text(404, "not found\n"),
    }
}
