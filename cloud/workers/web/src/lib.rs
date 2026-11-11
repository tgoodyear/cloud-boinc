//! The master URL page, the web RPCs of clients and account managers, the
//! server status and the site pages. PHP file names stay in the URLs because
//! clients have them built in. Everything past routing is in
//! `cb_core::web` and `cb_core::site`.

use std::collections::HashMap;

use cb_core::site::{auth_from_cookie, site_page, SiteCtx, SitePage, SITE_PATHS};
use cb_core::web::{
    am_get_info, am_set_host_info, am_set_info, create_account, lookup_account, master_page,
    project_config, server_status, server_status_page, server_status_xml, NewAccount,
};
use cb_proto::web_rpc::AccountError;
use cb_worker::{
    db, now, project, random_u32, read_body, secret_bytes, text, xml, xml_utf8,
    SECRET_PASSWD_PEPPER,
};
use worker::{
    console_error, event, Cache, Context, Env, Headers, Method, Request, Response, Result,
};

/// How long the edge cache keeps the master page and the project config.
/// Both change only when the settings do.
const PAGE_CACHE_SECS: u32 = 30;
/// Largest form read. `am_set_info.php` can carry both preference
/// documents (up to 64 KB each), URL-encoded.
const MAX_FORM_BYTES: usize = 192 * 1024;

fn html(body: String) -> Result<Response> {
    Ok(Response::builder()
        .with_header("Content-Type", "text/html; charset=utf-8")?
        .with_header(
            "Cache-Control",
            &format!("public, max-age={PAGE_CACHE_SECS}"),
        )?
        .fixed(body.into_bytes()))
}

fn cacheable_xml(body: String) -> Result<Response> {
    Ok(Response::builder()
        .with_header("Content-Type", "text/xml")?
        .with_header(
            "Cache-Control",
            &format!("public, max-age={PAGE_CACHE_SECS}"),
        )?
        .fixed(body.into_bytes()))
}

/// A site page. These depend on the visitor's cookie, so no cache keeps them.
fn site_response(page: SitePage) -> Result<Response> {
    let mut builder = Response::builder()
        .with_status(page.status)
        .with_header("Content-Type", page.content_type)?
        .with_header("Cache-Control", "private, no-store")?
        .with_header("X-Content-Type-Options", "nosniff")?
        // The pages hold forms that change the account; no other site may
        // put them in a frame.
        .with_header("Content-Security-Policy", "frame-ancestors 'none'")?
        .with_header("X-Frame-Options", "DENY")?;
    if let Some(location) = &page.location {
        builder = builder.with_header("Location", location)?;
    }
    if let Some(cookie) = &page.set_cookie {
        builder = builder.with_header("Set-Cookie", cookie)?;
    }
    Ok(builder.fixed(page.body.into_bytes()))
}

/// Pages that are the same for every client and need no database read when
/// the edge cache has them.
fn is_static_page(req: &Request, path: &str) -> bool {
    req.method() == Method::Get && matches!(path, "/" | "/index.php" | "/get_project_config.php")
}

/// Query parameters, then form fields for a POST.
async fn params(req: &mut Request) -> Result<HashMap<String, String>> {
    let mut out: HashMap<String, String> = req
        .url()?
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if req.method() == Method::Post {
        // A longer form is not from a BOINC client; its fields are ignored.
        if let Some(body) = read_body(req, MAX_FORM_BYTES).await? {
            let body = String::from_utf8_lossy(&body);
            for (k, v) in worker::Url::parse(&format!("http://x/?{body}"))?.query_pairs() {
                out.insert(k.into_owned(), v.into_owned());
            }
        }
    }
    Ok(out)
}

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, ctx: Context) -> Result<Response> {
    if !matches!(req.method(), Method::Get | Method::Head | Method::Post) {
        return text(405, "method not allowed\n");
    }
    let path = req.path();
    let cache_key = is_static_page(&req, &path).then(|| req.url().map(|u| u.to_string()));
    let cache_key = cache_key.transpose()?;
    if let Some(key) = &cache_key {
        if let Some(hit) = Cache::default().get(key.as_str(), false).await? {
            // The zone's browser cache setting rewrites Cache-Control on a
            // stored response; clients get the lifetime the page was built with.
            let headers = Headers::new();
            if let Some(kind) = hit.headers().get("Content-Type")? {
                headers.set("Content-Type", &kind)?;
            }
            headers.set(
                "Cache-Control",
                &format!("public, max-age={PAGE_CACHE_SECS}"),
            )?;
            return Ok(Response::from_body(hit.body().clone())?.with_headers(headers));
        }
    }
    let db = db(&env)?;
    let project = match project(&db).await {
        Ok(p) => p,
        Err(e) => {
            console_error!("settings: {e}");
            return text(503, "The project is temporarily unavailable.\n");
        }
    };
    match path.as_str() {
        "/" | "/index.php" | "/get_project_config.php" => {
            let mut response = if path == "/get_project_config.php" {
                cacheable_xml(project_config(&project))?
            } else {
                html(master_page(&project))?
            };
            if let Some(key) = cache_key {
                let copy = response.cloned()?;
                ctx.wait_until(async move {
                    if let Err(e) = Cache::default().put(key.as_str(), copy).await {
                        console_error!("cache: {e}");
                    }
                });
            }
            Ok(response)
        }
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
                ..Default::default()
            };
            match create_account(&db, &project.config, &pepper, now(), &random, &account).await {
                Ok(reply) => xml(reply),
                Err(e) => {
                    console_error!("create_account: {e}");
                    xml(AccountError::DbError.reply())
                }
            }
        }
        "/am_get_info.php" => {
            let q = params(&mut req).await?;
            let key = q.get("account_key").map(String::as_str).unwrap_or_default();
            match am_get_info(&db, key).await {
                Ok(reply) => xml_utf8(reply),
                Err(e) => {
                    console_error!("am_get_info: {e}");
                    xml(AccountError::DbError.reply())
                }
            }
        }
        "/am_set_info.php" => {
            let q = params(&mut req).await?;
            let pepper = secret_bytes(&env, SECRET_PASSWD_PEPPER)?;
            match am_set_info(&db, &project.config, &pepper, &q).await {
                Ok(reply) => xml(reply),
                Err(e) => {
                    console_error!("am_set_info: {e}");
                    xml(AccountError::DbError.reply())
                }
            }
        }
        "/am_set_host_info.php" => {
            let q = params(&mut req).await?;
            let get = |k: &str| q.get(k).map(String::as_str).unwrap_or_default();
            match am_set_host_info(&db, get("account_key"), get("hostid"), get("venue")).await {
                Ok(reply) => xml(reply),
                Err(e) => {
                    console_error!("am_set_host_info: {e}");
                    xml(AccountError::DbError.reply())
                }
            }
        }
        p if SITE_PATHS.contains(&p) => {
            let post = req.method() == Method::Post;
            let cookie = req
                .headers()
                .get("Cookie")?
                .and_then(|h| auth_from_cookie(&h));
            let q = params(&mut req).await?;
            let pepper = secret_bytes(&env, SECRET_PASSWD_PEPPER).ok();
            let random = random_u32;
            let ctx = SiteCtx {
                db: &db,
                project: &project,
                auth: cookie.as_deref(),
                post,
                pepper: pepper.as_deref(),
                now: now(),
                random: &random,
            };
            match site_page(&ctx, p, &q).await {
                Ok(Some(page)) => site_response(page),
                Ok(None) => text(404, "not found\n"),
                Err(e) => {
                    console_error!("{p}: {e}");
                    text(503, "The project is temporarily unavailable.\n")
                }
            }
        }
        _ => text(404, "not found\n"),
    }
}
