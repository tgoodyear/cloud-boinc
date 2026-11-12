//! The master URL page, the web RPCs of clients and account managers, the
//! server status and the site pages. PHP file names stay in the URLs because
//! clients have them built in. With a stock layout in the config
//! (`cb_core::stock`) the pages also answer under its page prefix. Everything
//! past routing is in `cb_core::web` and `cb_core::site`.

use std::collections::HashMap;

use cb_core::mail::Mailer;
use cb_core::site::{auth_from_cookie, site_page, SiteCtx, SitePage, SITE_HEADERS, SITE_PATHS};
use cb_core::stock::{route_web, url_origin, WebRoute};
use cb_core::web::{
    am_get_info, am_set_host_info, am_set_info, create_account, html_cache_control, lookup_account,
    master_page, project_config, server_status, server_status_page, server_status_xml, NewAccount,
    FAVICON_SVG, MAX_FORM_BYTES,
};
use cb_proto::web_rpc::AccountError;
use cb_worker::{
    db, mailer, now, project, random_u32, read_body, secret_bytes, text, xml, xml_utf8,
    SECRET_PASSWD_PEPPER,
};
use worker::{
    console_error, event, Cache, Context, Env, Headers, Method, Request, Response, Result,
};

/// How long the edge cache keeps the master page and the project config.
/// Both change only when the settings do.
const PAGE_CACHE_SECS: u32 = 30;
fn html(body: String) -> Result<Response> {
    Ok(Response::builder()
        .with_header("Content-Type", "text/html; charset=utf-8")?
        .with_header(
            "Cache-Control",
            &html_cache_control(&format!("public, max-age={PAGE_CACHE_SECS}")),
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
    let headers = Headers::new();
    headers.set("Content-Type", page.content_type)?;
    headers.set("Cache-Control", &html_cache_control("private, no-store"))?;
    for (name, value) in SITE_HEADERS {
        headers.set(name, value)?;
    }
    if let Some(location) = &page.location {
        headers.set("Location", location)?;
    }
    // One header line per cookie: they cannot be joined.
    for cookie in page.set_cookie.iter().chain(&page.more_cookies) {
        headers.append("Set-Cookie", cookie)?;
    }
    Ok(Response::builder()
        .with_status(page.status)
        .with_headers(headers)
        .fixed(page.body.into_bytes()))
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

/// The edge cache's copy of a static page, with the headers a client gets.
async fn cached(key: &str) -> Result<Option<Response>> {
    let Some(hit) = Cache::default().get(key, false).await? else {
        return Ok(None);
    };
    // The zone's browser cache setting rewrites Cache-Control on a stored
    // response; clients get the lifetime the page was built with.
    let headers = Headers::new();
    if let Some(kind) = hit.headers().get("Content-Type")? {
        headers.set("Content-Type", &kind)?;
    }
    let base = format!("public, max-age={PAGE_CACHE_SECS}");
    let is_html = hit
        .headers()
        .get("Content-Type")?
        .is_some_and(|k| k.starts_with("text/html"));
    headers.set(
        "Cache-Control",
        &if is_html {
            html_cache_control(&base)
        } else {
            base
        },
    )?;
    Ok(Some(
        Response::from_body(hit.body().clone())?.with_headers(headers),
    ))
}

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, ctx: Context) -> Result<Response> {
    if !matches!(req.method(), Method::Get | Method::Head | Method::Post) {
        return text(405, "method not allowed\n");
    }
    let raw_path = req.path();
    if raw_path == "/favicon.ico" && matches!(req.method(), Method::Get | Method::Head) {
        return Ok(Response::builder()
            .with_header("Content-Type", "image/svg+xml")?
            .with_header("Cache-Control", "public, max-age=86400")?
            .with_header("X-Content-Type-Options", "nosniff")?
            .fixed(FAVICON_SVG.as_bytes().to_vec()));
    }
    // Pages at the root need no database read when the cache has them.
    let mut cache_key = is_static_page(&req, &raw_path)
        .then(|| req.url().map(|u| u.to_string()))
        .transpose()?;
    if let Some(key) = &cache_key {
        if let Some(hit) = cached(key).await? {
            return Ok(hit);
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
    // Pages answer at the root and, with a stock layout, under its page
    // prefix. The handlers below name pages as they are at the root.
    let stock = project.config.stock.as_ref();
    let (path, prefixed) = match route_web(&raw_path, stock) {
        WebRoute::Page { path, prefixed } => (path.to_string(), prefixed),
        WebRoute::Redirect(mut to) => {
            if let Some(q) = req.url()?.query() {
                to.push('?');
                to.push_str(q);
            }
            return Ok(Response::builder()
                .with_status(301)
                .with_header("Location", &to)?
                .empty());
        }
    };
    if prefixed && cache_key.is_none() && is_static_page(&req, &path) {
        let key = req.url()?.to_string();
        if let Some(hit) = cached(&key).await? {
            return Ok(hit);
        }
        cache_key = Some(key);
    }
    // Where redirects and the login cookie point: the directory the request
    // came in under.
    let base = match (stock, url_origin(&project.config.master_url)) {
        (Some(stock), Some(origin)) if prefixed => stock.page_base(origin),
        _ => project.config.master_url.clone(),
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
            match am_set_info(&db, &project.config, &pepper, now(), &q).await {
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
            // `None` without an `EMAIL` binding: the pages that need email
            // say the project sends none.
            let mail = mailer(&env);
            let ctx = SiteCtx {
                db: &db,
                project: &project,
                base: &base,
                auth: cookie.as_deref(),
                post,
                pepper: pepper.as_deref(),
                now: now(),
                random: &random,
                mailer: mail.as_ref().map(|m| m as &dyn Mailer),
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
