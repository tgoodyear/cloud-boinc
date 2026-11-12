//! The site pages: login and logout, the account page, a user's computers,
//! one computer, task lists, one task and one workunit. Their content follows
//! upstream `html/user/*.php` and `html/inc/{host,result,user}.inc`.
//!
//! A visitor is logged in when the `auth` cookie holds an account's
//! authenticator, as upstream (`get_logged_in_user` in `html/inc/util.inc`).
//! Weak authenticators do not log in: they are for attaching computers
//! only. Fields upstream shows only to the account holder (IP addresses,
//! domain names, disk and swap, the email address, account keys) are left out
//! for everyone else. Every value that came from a client or a user is
//! HTML-escaped.
//!
//! Each page reads the database in at most two round trips of at most three
//! statements, and every list has a `LIMIT`.

use std::collections::HashMap;

use cb_crypto::account::{constant_time_eq, passwd_hash, user_cpid, verify_peppered_passwd_hash};
use cb_db::{stmt, Db, Stmt};
use cb_proto::consts::*;
use cb_proto::web_rpc::{err, write_error};

use crate::mail::{Mail, Mailer};
use crate::model::{Host, JobResult, User, Workunit};
use crate::project::Project;
use crate::site_account;
use crate::site_email;
use crate::site_ui::{self, site_header};
use crate::web::{account_creation_refusal, html_escape, page, show_user_xml, weak_account_key};
use crate::CoreResult;

#[derive(serde::Deserialize)]
struct CountRow {
    n: i64,
}

/// The task IDs a filtered task list looked at.
#[derive(serde::Deserialize)]
struct WindowRow {
    lo: Option<i64>,
    hi: Option<i64>,
    n: i64,
}

/// Request parameters: the query string, then form fields of a POST.
pub type Params = HashMap<String, String>;

/// Name of the login cookie, as upstream.
pub const AUTH_COOKIE: &str = "auth";
/// Lifetime of the cookie when "stay logged in" is ticked (`send_cookie`).
const PERMANENT_COOKIE_SECS: i64 = 365 * 86400;
/// Tasks per page of `results.php`, as upstream.
pub const RESULTS_PER_PAGE: usize = 20;
/// Most tasks a task list with a state or application filter reads for one
/// page. A page that finds fewer matches than fit links on from the last
/// task it looked at.
pub const RESULTS_FILTER_WINDOW: usize = 500;
/// Computers listed by `hosts_user.php`, as upstream's `limit 100`.
pub const MAX_HOSTS_LISTED: usize = 100;
/// Tasks of one workunit listed by `workunit.php`.
pub const MAX_WU_RESULTS: usize = 100;
/// A computer that has not contacted the server for this long is hidden from
/// `hosts_user.php` unless `show_all=1`.
const OLD_HOST_SECS: i64 = 30 * 86400;

const KILO: f64 = 1024.0;
const MEGA: f64 = 1024.0 * 1024.0;
const GIGA: f64 = 1024.0 * 1024.0 * 1024.0;
const TERA: f64 = 1024.0 * 1024.0 * 1024.0 * 1024.0;

/// What a page handler answers; the Worker turns it into a response.
#[derive(Debug, Clone, PartialEq)]
pub struct SitePage {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
    /// A redirect target, sent with status 303.
    pub location: Option<String>,
    pub set_cookie: Option<String>,
}

const HTML: &str = "text/html; charset=utf-8";
const XML: &str = "text/xml; charset=utf-8";

impl SitePage {
    pub(crate) fn html(status: u16, body: String) -> SitePage {
        SitePage {
            status,
            content_type: HTML,
            body,
            location: None,
            set_cookie: None,
        }
    }

    fn xml(body: String) -> SitePage {
        SitePage {
            status: 200,
            content_type: XML,
            body,
            location: None,
            set_cookie: None,
        }
    }

    pub(crate) fn redirect(location: String) -> SitePage {
        SitePage {
            status: 303,
            content_type: HTML,
            body: String::new(),
            location: Some(location),
            set_cookie: None,
        }
    }
}

/// What every page handler gets.
pub struct SiteCtx<'a> {
    pub db: &'a dyn Db,
    pub project: &'a Project,
    /// The value of the `auth` cookie, if the request carried one.
    pub auth: Option<&'a str>,
    /// Whether the request is a POST; passwords are taken from a POST only.
    pub post: bool,
    /// The password pepper, for logins by email address and password.
    pub pepper: Option<&'a [u8]>,
    pub now: i64,
    /// Random numbers, for new account keys.
    pub random: &'a dyn Fn() -> u32,
    /// How the site sends email; `None` for a deployment without a
    /// transport. The pages that need email say so, and the others do not
    /// change. A mailer counts only when the config has `email_from` and an
    /// `email_max_per_day` above 0; see [`SiteCtx::mail`].
    pub mailer: Option<&'a dyn Mailer>,
}

impl<'a> SiteCtx<'a> {
    /// The mailer with the project's sender settings, or `None` when the
    /// site sends no email: no mailer, no `email_from` in the config, or
    /// `email_max_per_day` of 0.
    pub fn mail(&self) -> Option<Mail<'a>> {
        let config = &self.project.config;
        if config.email_from.is_empty() || config.email_max_per_day <= 0 {
            return None;
        }
        self.mailer
            .map(|m| Mail::new(m, &config.email_from, &config.email_from_name))
    }
}

/// The paths [`site_page`] serves.
pub const SITE_PATHS: [&str; 27] = [
    "/login_form.php",
    "/login_action.php",
    "/logout.php",
    "/home.php",
    "/hosts_user.php",
    "/show_host_detail.php",
    "/results.php",
    "/result.php",
    "/workunit.php",
    "/show_user.php",
    "/create_account_form.php",
    "/signup.php",
    "/create_account_action.php",
    "/edit_user_info_form.php",
    "/edit_user_info_action.php",
    "/edit_passwd_form.php",
    "/edit_passwd_action.php",
    "/edit_email_form.php",
    "/edit_email_action.php",
    "/prefs.php",
    "/prefs_edit.php",
    "/prefs_remove.php",
    "/get_passwd.php",
    "/mail_passwd.php",
    "/validate_email_addr.php",
    "/recover_email.php",
    "/reset_account_key.php",
];

/// Headers every site page is sent with: no framing by another site (the
/// pages hold forms that change the account), no content sniffing, and no
/// `Referer`, so that the address of a page that carries a token in its query
/// (the links in mail) does not reach a site it links to.
pub const SITE_HEADERS: [(&str, &str); 4] = [
    ("Content-Security-Policy", "frame-ancestors 'none'"),
    ("X-Frame-Options", "DENY"),
    ("X-Content-Type-Options", "nosniff"),
    ("Referrer-Policy", "no-referrer"),
];

/// Answers one of [`SITE_PATHS`]; `None` for any other path.
pub async fn site_page(ctx: &SiteCtx<'_>, path: &str, q: &Params) -> CoreResult<Option<SitePage>> {
    Ok(Some(match path {
        "/login_form.php" => login_form(ctx.project, q, ctx.mail().is_some()),
        "/login_action.php" => login_action(ctx, q).await?,
        "/logout.php" => site_account::logout(ctx, q).await?,
        "/home.php" => home(ctx).await?,
        "/hosts_user.php" => hosts_user(ctx, q).await?,
        "/show_host_detail.php" => show_host_detail(ctx, q).await?,
        "/results.php" => results(ctx, q).await?,
        "/result.php" => result(ctx, q).await?,
        "/workunit.php" => workunit(ctx, q).await?,
        "/show_user.php" => show_user(ctx, q).await?,
        "/create_account_form.php" | "/signup.php" => {
            site_account::create_account_form(ctx.project, q)
        }
        "/create_account_action.php" => site_account::create_account_action(ctx, q).await?,
        "/edit_user_info_form.php" => site_account::edit_user_info_form(ctx).await?,
        "/edit_user_info_action.php" => site_account::edit_user_info_action(ctx, q).await?,
        "/edit_passwd_form.php" => site_account::edit_passwd_form(ctx).await?,
        "/edit_passwd_action.php" => site_account::edit_passwd_action(ctx, q).await?,
        "/edit_email_form.php" => site_account::edit_email_form(ctx).await?,
        "/edit_email_action.php" => site_account::edit_email_action(ctx, q).await?,
        "/prefs.php" => site_account::prefs(ctx, q).await?,
        "/prefs_edit.php" => site_account::prefs_edit(ctx, q).await?,
        "/prefs_remove.php" => site_account::prefs_remove(ctx, q).await?,
        "/get_passwd.php" => site_email::get_passwd(ctx),
        "/mail_passwd.php" => site_email::mail_passwd(ctx, q).await?,
        "/validate_email_addr.php" => site_email::validate_email_addr(ctx, q).await?,
        "/recover_email.php" => site_email::recover_email(ctx, q).await?,
        "/reset_account_key.php" => site_account::reset_account_key(ctx, q).await?,
        _ => return Ok(None),
    }))
}

// ---------------------------------------------------------------------------
// Formatting, after `html/inc/util_basic.inc` and `util.inc`.

/// `(year, month, day)` of a day count since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `date_str`: `j M Y`, or `---` for 0.
pub fn date_str(t: i64) -> String {
    if t == 0 {
        return "---".into();
    }
    let (y, m, d) = civil_from_days(t.div_euclid(86_400));
    format!("{d} {} {y}", MONTHS[(m - 1) as usize])
}

/// `time_str`: `j M Y, G:i:s UTC`, or `---` for 0.
pub fn time_str(t: i64) -> String {
    if t == 0 {
        return "---".into();
    }
    let secs = t.rem_euclid(86_400);
    format!(
        "{}, {}:{:02}:{:02} UTC",
        date_str(t),
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// PHP `number_format($x, $decimals)`: thousands separated by commas.
pub fn number_format(x: f64, decimals: usize) -> String {
    let x = if x.is_finite() { x } else { 0.0 };
    let s = format!("{:.*}", decimals, x.abs());
    let (int, frac) = match s.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (s.as_str(), None),
    };
    let mut out = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if let Some(f) = frac {
        out.push('.');
        out.push_str(f);
    }
    if x < 0.0 && out.bytes().any(|b| (b'1'..=b'9').contains(&b)) {
        out.insert(0, '-');
    }
    out
}

/// `time_diff`: days, hours, minutes and seconds.
fn time_diff(nsec: f64) -> String {
    let x = nsec.max(0.0) as i64;
    let (days, hours, minutes, seconds) = (x / 86400, x % 86400 / 3600, x % 3600 / 60, x % 60);
    let mut s = String::new();
    if days > 0 {
        s.push_str(&format!("{days} days "));
    }
    if hours > 0 || !s.is_empty() {
        s.push_str(&format!("{hours} hours "));
    }
    if minutes > 0 || !s.is_empty() {
        s.push_str(&format!("{minutes} min "));
    }
    let frac = nsec.max(0.0) - x as f64;
    s.push_str(&format!("{} sec", number_format(seconds as f64 + frac, 2)));
    s
}

/// `time_diff2`: one unit, two decimals.
fn time_diff2(nsec: f64) -> String {
    if nsec >= 86400.0 {
        format!("{:.2} days", nsec / 86400.0)
    } else if nsec >= 3600.0 {
        format!("{:.2} hours", nsec / 3600.0)
    } else if nsec >= 60.0 {
        format!("{:.2} min", nsec / 60.0)
    } else {
        format!("{nsec:.2} sec")
    }
}

/// `size_string`.
fn size_string(x: f64) -> String {
    if x > TERA {
        format!("{} TB", number_format(x / TERA, 2))
    } else if x > GIGA {
        format!("{} GB", number_format(x / GIGA, 2))
    } else if x > MEGA {
        format!("{} MB", number_format(x / MEGA, 2))
    } else if x > KILO {
        format!("{} KB", number_format(x / KILO, 2))
    } else {
        format!("{x} bytes")
    }
}

pub(crate) fn row2(out: &mut String, label: &str, value: &str) {
    out.push_str(&format!("<tr><th>{label}</th><td>{value}</td></tr>\n"));
}

/// A row of a form: the caption is the `<label>` of the control with ID `id`
/// (which `control` must carry), so that the control has a name.
pub(crate) fn label_row(out: &mut String, id: &str, caption: &str, control: &str) {
    out.push_str(&format!(
        "<tr><th scope=\"row\"><label for=\"{id}\">{caption}</label></th><td>{control}</td></tr>\n"
    ));
}

fn host_link(hostid: i64) -> String {
    if hostid > 0 {
        format!("<a href=\"show_host_detail.php?hostid={hostid}\">{hostid}</a>")
    } else {
        "---".into()
    }
}

/// A page in the site layout: the given body under a short navigation bar.
pub(crate) fn layout(project: &Project, title: &str, body: &str) -> String {
    let nav = format!("{}<h1>{}</h1>\n", site_header(project), html_escape(title));
    page(title, "", &format!("{nav}{body}"))
}

pub(crate) fn error_page(project: &Project, status: u16, msg: &str) -> SitePage {
    SitePage::html(
        status,
        layout(
            project,
            "Unable to handle request",
            &format!("<p class=\"error\">{}</p>\n", html_escape(msg)),
        ),
    )
}

fn int_param(q: &Params, name: &str) -> Option<i64> {
    q.get(name).and_then(|v| v.trim().parse().ok())
}

pub(crate) fn str_param<'a>(q: &'a Params, name: &str) -> &'a str {
    q.get(name).map(|s| s.trim()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Login.

/// Whether `s` could be an authenticator. Weak authenticators (with `_`)
/// are not, so they never reach the database here.
pub(crate) fn plausible_account_key(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The statement that finds the account an authenticator belongs to. A
/// blocked account cannot log in.
pub(crate) fn user_by_auth_stmt(auth: &str) -> Stmt {
    stmt!(
        "SELECT * FROM user WHERE authenticator = ? AND blocked = 0",
        auth
    )
}

/// The account an authenticator belongs to, by exact match. One statement,
/// none when `auth` cannot be an authenticator.
pub async fn user_by_auth(db: &dyn Db, auth: &str) -> CoreResult<Option<User>> {
    if !plausible_account_key(auth) {
        return Ok(None);
    }
    Ok(db.run(user_by_auth_stmt(auth)).await?.first()?)
}

/// The `auth` value of a `Cookie` header.
pub fn auth_from_cookie(header: &str) -> Option<String> {
    header.split(';').find_map(|part| {
        let (k, v) = part.trim().split_once('=')?;
        (k.trim() == AUTH_COOKIE && !v.trim().is_empty()).then(|| v.trim().to_string())
    })
}

/// The path of the master URL, where the cookie applies (`send_cookie`).
fn cookie_path(project: &Project) -> String {
    let url = &project.config.master_url;
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    match rest.find('/') {
        Some(i) => rest[i..].to_string(),
        None => "/".into(),
    }
}

/// `Set-Cookie` value that logs a browser in.
pub fn login_cookie(project: &Project, authenticator: &str, permanent: bool) -> String {
    let lifetime = if permanent {
        format!("; Max-Age={PERMANENT_COOKIE_SECS}")
    } else {
        String::new()
    };
    format!(
        "{AUTH_COOKIE}={authenticator}; Path={}; HttpOnly; Secure; SameSite=Lax{lifetime}",
        cookie_path(project)
    )
}

/// `Set-Cookie` value that logs a browser out.
pub fn logout_cookie(project: &Project) -> String {
    format!(
        "{AUTH_COOKIE}=; Path={}; HttpOnly; Secure; SameSite=Lax; Max-Age=0",
        cookie_path(project)
    )
}

/// `sanitize_local_url`, stricter: a page of this site and its query, or
/// nothing.
pub(crate) fn local_url(next: &str) -> Option<&str> {
    let next = next.trim().trim_matches('/');
    let (page, query) = next.split_once('?').unwrap_or((next, ""));
    let page_ok = page.ends_with(".php")
        && page
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.');
    let query_ok = query
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"_=&%.-".contains(&b));
    (page_ok && query_ok).then_some(next)
}

/// `login_form.php`. With `can_mail`, a link to the page that mails a login
/// link to an account's address.
pub fn login_form(project: &Project, q: &Params, can_mail: bool) -> SitePage {
    let next = local_url(str_param(q, "next_url")).unwrap_or("home.php");
    let body = format!(
        "<form method=\"post\" action=\"login_action.php\">\n\
         <input type=\"hidden\" name=\"next_url\" value=\"{}\">\n\
         <p><label>Email address<br><input type=\"email\" name=\"email_addr\" autocomplete=\"username\"></label></p>\n\
         <p><label>Password<br><input type=\"password\" name=\"passwd\" autocomplete=\"current-password\"></label></p>\n\
         <p>or</p>\n\
         <p><label>Account key<br><input type=\"password\" name=\"authenticator\" size=\"40\"></label></p>\n\
         <p><label><input type=\"checkbox\" name=\"stay_logged_in\" value=\"1\"> Stay logged in</label></p>\n\
         <p><input type=\"submit\" value=\"Log in\"></p>\n</form>\n\
         <p>Your account key is in the file <code>account_*.xml</code> for this project in the \
         BOINC data directory of a computer attached to your account.</p>\n{}{}",
        html_escape(next),
        if can_mail {
            "<p>Forgot your password? <a href=\"get_passwd.php\">Get a login link by email</a>.</p>\n"
        } else {
            ""
        },
        if account_creation_refusal(&project.config).is_none() {
            "<p>No account yet? <a href=\"create_account_form.php\">Create one</a>.</p>\n"
        } else {
            ""
        }
    );
    SitePage::html(200, layout(project, "Log in", &body))
}

/// `login_action.php`: by account key (`key` in the query or
/// `authenticator` in the form), or by email address and password in a POST.
/// On success the browser gets the `auth` cookie and goes to `next_url`. With
/// `id` and `t` it is the target of a mailed login link
/// ([`site_email::login_via_link`]).
pub async fn login_action(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    if !str_param(q, "id").is_empty() && !str_param(q, "t").is_empty() {
        return site_email::login_via_link(ctx, q).await;
    }
    let project = ctx.project;
    let next = local_url(str_param(q, "next_url")).unwrap_or("home.php");
    let permanent = q.get("stay_logged_in").is_some_and(|v| !v.is_empty());
    let key = match str_param(q, "key") {
        "" => str_param(q, "authenticator"),
        k => k,
    };
    let user = if !key.is_empty() {
        user_by_auth(ctx.db, key).await?
    } else {
        let email = str_param(q, "email_addr").to_lowercase();
        let passwd = q.get("passwd").map(String::as_str).unwrap_or_default();
        if !ctx.post || email.is_empty() || passwd.is_empty() {
            return Ok(error_page(
                project,
                400,
                "You must supply an email address and password.",
            ));
        }
        let found: Option<User> = ctx
            .db
            .run(stmt!(
                "SELECT * FROM user WHERE email_addr = ? AND blocked = 0",
                email.as_str()
            ))
            .await?
            .first()?;
        found.filter(|u| {
            // Upstream accepts the account key in place of the password.
            constant_time_eq(u.authenticator.as_bytes(), passwd.as_bytes())
                || (!u.passwd_hash.is_empty()
                    && ctx.pepper.is_some_and(|pepper| {
                        verify_peppered_passwd_hash(
                            pepper,
                            &passwd_hash(passwd, &email),
                            &u.passwd_hash,
                        )
                    }))
        })
    };
    let Some(user) = user else {
        return Ok(error_page(project, 403, "Login failed."));
    };
    let mut out = SitePage::redirect(format!("{}{next}", project.config.master_url));
    out.set_cookie = Some(login_cookie(project, &user.authenticator, permanent));
    Ok(out)
}

pub(crate) fn to_login(project: &Project, next: &str) -> SitePage {
    let next: String = next
        .bytes()
        .map(|b| match b {
            b'?' => "%3F".to_string(),
            b'&' => "%26".to_string(),
            b'=' => "%3D".to_string(),
            _ => (b as char).to_string(),
        })
        .collect();
    SitePage::redirect(format!(
        "{}login_form.php?next_url={next}",
        project.config.master_url
    ))
}

pub(crate) async fn viewer(ctx: &SiteCtx<'_>) -> CoreResult<Option<User>> {
    match ctx.auth {
        Some(a) => user_by_auth(ctx.db, a).await,
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Account.

/// `home.php`: the logged-in account. One statement.
pub async fn home(ctx: &SiteCtx<'_>) -> CoreResult<SitePage> {
    let project = ctx.project;
    let Some(user) = viewer(ctx).await? else {
        return Ok(to_login(project, "home.php"));
    };
    let weak = weak_account_key(&user);
    let account_file = account_file_name(&project.config.master_url);
    let master = html_escape(&project.config.master_url);

    // Attaching a computer. The keys are shown here as they were before; this
    // only lays them out. The key never goes in a URL.
    let attach = format!(
        "<p>To attach a computer to this account, give the BOINC client the project URL and \
         either your email address and password, or the account key.</p>\n\
         {}<ol>\n\
         <li>In the BOINC Manager choose Add project, enter the project URL, then your email \
         address and password.</li>\n\
         <li>Or on the command line: <code>boinccmd --project_attach {master} ACCOUNT_KEY</code>, \
         with the account key above.</li>\n</ol>\n\
         <details>\n<summary>Weak account key</summary>\n\
         <p><code class=\"key\">{}</code></p>\n\
         <p class=\"muted\">Attaches computers but cannot log in. To attach a computer without \
         the BOINC Manager, put this in <code>{}</code> in its BOINC data directory:</p>\n\
         <pre>{}</pre>\n</details>\n\
         {}",
        site_ui::kv(&[
            ("Project URL", format!("<code class=\"key\">{master}</code>")),
            (
                "Account key",
                format!("<code class=\"key\">{}</code>", html_escape(&user.authenticator))
            ),
        ]),
        html_escape(&weak),
        html_escape(&account_file),
        html_escape(&format!(
            "<account>\n    <master_url>{}</master_url>\n    <authenticator>{}</authenticator>\n</account>",
            project.config.master_url, weak
        )),
        site_ui::actions(&[site_ui::button_link(
            "Replace the account key",
            "reset_account_key.php",
            true
        )]),
    );

    let mut email_cell = html_escape(&user.email_addr);
    if ctx.mail().is_some() {
        email_cell.push_str(&site_email::verify_cell(&user, ctx.now));
    }
    let info = site_ui::kv(&[
        ("Name", html_escape(&user.name)),
        ("Email address", email_cell),
        (
            &format!("{} member since", project.config.long_name),
            date_str(user.create_time),
        ),
        ("User ID", user.id.to_string()),
        (
            "Cross-project ID",
            user_cpid(&user.cross_project_id, &user.email_addr),
        ),
    ]);

    let numbers = site_ui::stats(&[
        site_ui::stat("Total credit", &number_format(user.total_credit, 0), ""),
        site_ui::stat(
            "Recent average credit",
            &number_format(user.expavg_credit, 2),
            "",
        ),
    ]);

    let mut account_links: Vec<(&str, String)> = vec![
        ("Account information", "edit_user_info_form.php".into()),
        ("Email address", "edit_email_form.php".into()),
        ("Password", "edit_passwd_form.php".into()),
    ];
    if ctx.mail().is_some() {
        account_links.push((
            "Forgot your password? Get a login link by email",
            "get_passwd.php".into(),
        ));
        if user.email_validated == 0 {
            account_links.push((
                "Verify your email address",
                "validate_email_addr.php".into(),
            ));
        }
    }
    let computing_links: Vec<(&str, String)> = vec![
        ("Computers on this account", "hosts_user.php".into()),
        ("Tasks", format!("results.php?userid={}", user.id)),
        ("Computing preferences", "prefs.php?subset=global".into()),
        ("Project preferences", "prefs.php?subset=project".into()),
    ];
    let mut logout = String::from("<form method=\"post\" action=\"logout.php\">\n");
    logout.push_str(&site_account::token_fields(&user, ctx.now));
    logout.push_str("<p><input type=\"submit\" value=\"Log out\"></p>\n</form>\n");

    let mut b = numbers;
    b.push_str(&site_ui::card_accent("Attach a computer", &attach));
    b.push_str(&site_ui::grid(&[
        site_ui::card("Account functions", &site_ui::link_list(&account_links)),
        site_ui::card("Computing", &site_ui::link_list(&computing_links)),
    ]));
    b.push_str(&site_ui::card("Account information", &info));
    b.push_str(&logout);
    Ok(SitePage::html(200, layout(project, "Your account", &b)))
}

/// The account file name `weak_auth.php` tells users to create.
fn account_file_name(master_url: &str) -> String {
    let url = master_url
        .split_once("://")
        .map(|(_, r)| r)
        .unwrap_or(master_url);
    let mapped: String = url
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("account_{}.xml", mapped.trim_end_matches('_'))
}

/// `show_user.php`: an account's public summary as HTML, or as XML with
/// `format=xml` (by `userid`, or by `auth` with the account's computers).
pub async fn show_user(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    if str_param(q, "format") == "xml" {
        let auth = str_param(q, "auth");
        let found = if !auth.is_empty() {
            match user_by_auth(ctx.db, auth).await? {
                Some(user) => {
                    let hosts: Vec<Host> = ctx
                        .db
                        .run(stmt!(
                            "SELECT * FROM host WHERE userid = ? ORDER BY id LIMIT ?",
                            user.id,
                            MAX_HOSTS_LISTED
                        ))
                        .await?
                        .decode()?;
                    Some((user, Some(hosts)))
                }
                None => None,
            }
        } else {
            match int_param(q, "userid") {
                Some(id) => public_user(ctx.db, id).await?.map(|u| (u, None)),
                None => None,
            }
        };
        return Ok(SitePage::xml(match found {
            Some((user, hosts)) => show_user_xml(&project.config, &user, hosts.as_deref()),
            None => write_error(err::ERR_DB_NOT_FOUND, None),
        }));
    }
    let Some(user) = (match int_param(q, "userid") {
        Some(id) => public_user(ctx.db, id).await?,
        None => None,
    }) else {
        return Ok(error_page(project, 404, "No such user"));
    };
    let mut b = String::from("<table>\n");
    row2(
        &mut b,
        &format!("{} member since", project.config.long_name),
        &date_str(user.create_time),
    );
    if project.config.user_country {
        row2(&mut b, "Country", &html_escape(&user.country));
    }
    row2(&mut b, "Total credit", &number_format(user.total_credit, 0));
    row2(
        &mut b,
        "Recent average credit",
        &number_format(user.expavg_credit, 2),
    );
    if user.show_hosts != 0 {
        row2(
            &mut b,
            "Computers",
            &format!("<a href=\"hosts_user.php?userid={}\">View</a>", user.id),
        );
    }
    b.push_str("</table>\n");
    Ok(SitePage::html(200, layout(project, &user.name, &b)))
}

/// The columns of an account anyone may see.
async fn public_user(db: &dyn Db, id: i64) -> CoreResult<Option<User>> {
    Ok(db
        .run(stmt!(
            "SELECT id, create_time, name, country, url, total_credit, expavg_credit, expavg_time, \
             teamid, show_hosts, cross_project_id, email_addr FROM user WHERE id = ?",
            id
        ))
        .await?
        .first()?)
}

// ---------------------------------------------------------------------------
// Computers.

/// The parts of the host summary the scheduler keeps in `host.serialnum`
/// (`host_summary` in `scheduler.rs`): `[BOINC|8.0.4][CUDA|1|8192MB]...`.
fn summary_parts(serialnum: &str) -> Vec<Vec<&str>> {
    serialnum
        .split('[')
        .filter_map(|p| p.strip_suffix(']'))
        .map(|p| p.split('|').collect())
        .collect()
}

/// `boinc_version`, from the host summary. Escaped.
fn boinc_version(serialnum: &str) -> String {
    summary_parts(serialnum)
        .into_iter()
        .find(|p| p.first() == Some(&"BOINC"))
        .and_then(|p| p.get(1).map(|v| html_escape(v)))
        .unwrap_or_else(|| "---".into())
}

/// `gpu_desc`, from the host summary. Escaped.
fn gpu_desc(serialnum: &str) -> String {
    let gpus: Vec<String> = summary_parts(serialnum)
        .into_iter()
        .filter(|p| p.len() == 3 && !matches!(p[0], "BOINC" | "vbox" | "docker"))
        .map(|p| {
            format!(
                "[{}] {} ({})",
                html_escape(p[1]),
                html_escape(p[0]),
                html_escape(p[2])
            )
        })
        .collect();
    if gpus.is_empty() {
        "---".into()
    } else {
        gpus.join("<br>")
    }
}

/// The version a host reported for `vbox` or `docker`. Escaped.
fn runtime_desc(serialnum: &str, name: &str) -> String {
    summary_parts(serialnum)
        .into_iter()
        .find(|p| p.first() == Some(&name))
        .and_then(|p| p.get(1).map(|v| html_escape(v)))
        .unwrap_or_else(|| "---".into())
}

fn cpu_desc(h: &Host) -> String {
    format!(
        "{}<br>{}<br>({} cores)",
        html_escape(&h.p_vendor),
        html_escape(&h.p_model),
        h.p_ncpus
    )
}

/// `hosts_user.php`: the logged-in account's computers, or another
/// account's when it shows them. Two round trips, three statements at most.
pub async fn hosts_user(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    let show_all = int_param(q, "show_all") == Some(1);
    let rev = int_param(q, "rev") == Some(1);
    let sort = str_param(q, "sort");
    let asked = int_param(q, "userid").filter(|&id| id > 0);

    let mut reads = Vec::new();
    let auth = ctx.auth.filter(|a| plausible_account_key(a));
    if let Some(a) = auth {
        reads.push(user_by_auth_stmt(a));
    }
    if let Some(id) = asked {
        reads.push(stmt!(
            "SELECT id, name, show_hosts FROM user WHERE id = ?",
            id
        ));
    }
    let mut out = if reads.is_empty() {
        Vec::new()
    } else {
        ctx.db.batch(reads).await?
    }
    .into_iter();
    let me: Option<User> = match auth {
        Some(_) => out.next().unwrap_or_default().first()?,
        None => None,
    };
    let other: Option<User> = match asked {
        Some(_) => out.next().unwrap_or_default().first()?,
        None => None,
    };

    let (user, private) = match (asked, me) {
        (Some(id), Some(me)) if me.id == id => (me, true),
        (Some(_), _) => {
            let Some(other) = other else {
                return Ok(error_page(project, 404, "No such user"));
            };
            if other.show_hosts == 0 {
                return Ok(SitePage::html(
                    200,
                    layout(
                        project,
                        "Computers hidden",
                        "<p>This user has chosen not to show information about their computers.</p>\n",
                    ),
                ));
            }
            (other, false)
        }
        (None, Some(me)) => (me, true),
        (None, None) => return Ok(to_login(project, "hosts_user.php")),
    };

    let (column, desc) = match sort {
        "total_credit" => ("total_credit", true),
        "expavg_credit" => ("expavg_credit", true),
        "name" => ("domain_name", false),
        "id" => ("id", false),
        "cpu" => ("p_vendor", false),
        "os" => ("os_name", false),
        "venue" => ("venue", false),
        _ => ("rpc_time", true),
    };
    let sort = if column == "rpc_time" {
        "rpc_time"
    } else {
        sort
    };
    let order = if rev != desc { "DESC" } else { "ASC" };
    // Without `show_all`, computers out of contact for 30 days are left out
    // by the query, before its limit, and counted by a second statement.
    let cutoff = if show_all {
        i64::MIN
    } else {
        ctx.now - OLD_HOST_SECS
    };
    let mut reads = vec![stmt!(
        format!(
            "SELECT * FROM host WHERE userid = ? AND rpc_time >= ? \
             ORDER BY {column} {order}, id LIMIT ?"
        ),
        user.id,
        cutoff,
        MAX_HOSTS_LISTED
    )];
    if !show_all {
        reads.push(stmt!(
            "SELECT count(*) AS n FROM host WHERE userid = ? AND rpc_time < ?",
            user.id,
            cutoff
        ));
    }
    let mut out = ctx.db.batch(reads).await?.into_iter();
    let hosts: Vec<Host> = out.next().unwrap_or_default().decode()?;
    let hidden = match out.next() {
        Some(rows) => rows.first::<CountRow>()?.map_or(0, |r| r.n),
        None => 0,
    };

    let user_q = if private {
        String::new()
    } else {
        format!("userid={}&amp;", user.id)
    };
    let link = |s: &str, label: &str| {
        let r = if s == sort { u8::from(!rev) } else { 0 };
        format!(
            "<a href=\"hosts_user.php?{user_q}sort={s}&amp;rev={r}&amp;show_all={}\">{label}</a>",
            u8::from(show_all)
        )
    };
    let any_product = private && hosts.iter().any(|h| !h.product_name.is_empty());
    let mut b = String::from("<table>\n<tr>");
    b.push_str(&format!("<th>{}</th>", link("id", "Computer ID")));
    if private {
        b.push_str(&format!("<th>{}</th>", link("name", "Name")));
        if any_product {
            b.push_str("<th>Product name</th>");
        }
        b.push_str(&format!("<th>{}</th>", link("venue", "Location")));
    } else {
        b.push_str("<th>Rank</th>");
    }
    b.push_str(&format!(
        "<th>{}</th><th>{}</th><th>BOINC version</th><th>{}</th><th>GPU</th><th>{}</th><th>{}</th></tr>\n",
        link("expavg_credit", "Average credit"),
        link("total_credit", "Total credit"),
        link("cpu", "CPU"),
        link("os", "Operating system"),
        link("rpc_time", "Last contact"),
    ));
    let mut i = 0;
    for h in &hosts {
        i += 1;
        b.push_str(&format!(
            "<tr><td>{0}<br><a href=\"show_host_detail.php?hostid={0}\">Details</a> | \
             <a href=\"results.php?hostid={0}\">Tasks</a></td>",
            h.id
        ));
        if private {
            b.push_str(&format!("<td>{}</td>", html_escape(&h.domain_name)));
            if any_product {
                b.push_str(&format!("<td>{}</td>", html_escape(&h.product_name)));
            }
            b.push_str(&format!("<td>{}</td>", html_escape(&h.venue)));
        } else {
            b.push_str(&format!("<td>{i}</td>"));
        }
        b.push_str(&format!(
            "<td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}<br><small>{}</small></td><td>{}</td></tr>\n",
            number_format(h.expavg_credit, 2),
            number_format(h.total_credit, 0),
            boinc_version(&h.serialnum),
            cpu_desc(h),
            gpu_desc(&h.serialnum),
            html_escape(&h.os_name),
            html_escape(&h.os_version),
            date_str(h.rpc_time),
        ));
    }
    b.push_str("</table>\n");
    if hidden > 0 {
        b.push_str(&format!(
            "<p>{hidden} computers that have not contacted the server for 30 days are hidden. \
             <a href=\"hosts_user.php?{user_q}sort={sort}&amp;rev={}&amp;show_all=1\">Show all computers</a></p>\n",
            u8::from(rev)
        ));
    } else if show_all {
        b.push_str(&format!(
            "<p><a href=\"hosts_user.php?{user_q}sort={sort}&amp;rev={}&amp;show_all=0\">Show only active computers</a></p>\n",
            u8::from(rev)
        ));
    }
    let title = if private {
        "Your computers".to_string()
    } else {
        format!("Computers belonging to {}", user.name)
    };
    Ok(SitePage::html(200, layout(project, &title, &b)))
}

/// `show_host_detail.php`: one computer. Private fields only for the
/// account it belongs to; its IP addresses only with `ipprivate=1`. One
/// round trip of three statements at most.
pub async fn show_host_detail(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    let Some(hostid) = int_param(q, "hostid").filter(|&id| id > 0) else {
        return Ok(error_page(project, 404, "No such computer"));
    };
    let ipprivate = !str_param(q, "ipprivate").is_empty();
    let mut reads = vec![
        stmt!("SELECT * FROM host WHERE id = ?", hostid),
        stmt!(
            "SELECT id, name, show_hosts FROM user WHERE id = (SELECT userid FROM host WHERE id = ?)",
            hostid
        ),
    ];
    let auth = ctx.auth.filter(|a| plausible_account_key(a));
    if let Some(a) = auth {
        reads.push(user_by_auth_stmt(a));
    }
    let mut out = ctx.db.batch(reads).await?.into_iter();
    let host: Option<Host> = out.next().unwrap_or_default().first()?;
    let holder: Option<User> = out.next().unwrap_or_default().first()?;
    let me: Option<User> = match out.next() {
        Some(r) => r.first()?,
        None => None,
    };
    let Some(h) = host else {
        return Ok(error_page(project, 404, "No such computer"));
    };
    let private = me.is_some_and(|u| u.id == h.userid);

    let mut b = String::from("<table>\n");
    if private {
        if ipprivate {
            row2(
                &mut b,
                "IP address",
                &format!(
                    "{}<br>(same the last {} times)",
                    html_escape(&h.last_ip_addr),
                    h.nsame_ip_addr
                ),
            );
            if h.last_ip_addr != h.external_ip_addr {
                row2(
                    &mut b,
                    "External IP address",
                    &html_escape(&h.external_ip_addr),
                );
            }
        } else {
            row2(
                &mut b,
                "IP address",
                &format!(
                    "<a href=\"show_host_detail.php?hostid={}&amp;ipprivate=1\">Show IP address</a>",
                    h.id
                ),
            );
        }
        row2(&mut b, "Domain name", &html_escape(&h.domain_name));
        if !h.product_name.is_empty() {
            row2(&mut b, "Product name", &html_escape(&h.product_name));
        }
        let tz = f64::from(h.timezone) / 3600.0;
        row2(
            &mut b,
            "Local Standard Time",
            &format!("UTC {}{} hours", if tz >= 0.0 { "+" } else { "" }, tz),
        );
        row2(&mut b, "Location", &html_escape(&h.venue));
    } else {
        let shown = holder.filter(|u| u.show_hosts != 0);
        row2(
            &mut b,
            "User",
            &match shown {
                Some(u) => format!(
                    "<a href=\"show_user.php?userid={}\">{}</a>",
                    u.id,
                    html_escape(&u.name)
                ),
                None => "Anonymous".into(),
            },
        );
    }
    row2(&mut b, "Created", &time_str(h.create_time));
    row2(&mut b, "Total credit", &number_format(h.total_credit, 0));
    row2(&mut b, "Average credit", &number_format(h.expavg_credit, 2));
    row2(
        &mut b,
        "CPU type",
        &format!(
            "{}<br>{}",
            html_escape(&h.p_vendor),
            html_escape(&h.p_model)
        ),
    );
    row2(&mut b, "Number of cores", &h.p_ncpus.to_string());
    row2(&mut b, "Coprocessors", &gpu_desc(&h.serialnum));
    row2(
        &mut b,
        "Virtualization",
        &runtime_desc(&h.serialnum, "vbox"),
    );
    row2(&mut b, "Docker", &runtime_desc(&h.serialnum, "docker"));
    row2(
        &mut b,
        "Operating System",
        &format!(
            "{}<br>{}",
            html_escape(&h.os_name),
            html_escape(&h.os_version)
        ),
    );
    row2(&mut b, "BOINC version", &boinc_version(&h.serialnum));
    row2(&mut b, "Memory", &format!("{:.2} GB", h.m_nbytes / GIGA));
    if h.m_cache > 0.0 {
        row2(&mut b, "Cache", &format!("{:.2} KB", h.m_cache / KILO));
    }
    if private {
        row2(&mut b, "Swap space", &format!("{:.2} GB", h.m_swap / GIGA));
        row2(
            &mut b,
            "Total disk space",
            &format!("{:.2} GB", h.d_total / GIGA),
        );
        row2(
            &mut b,
            "Free Disk Space",
            &format!("{:.2} GB", h.d_free / GIGA),
        );
    }
    row2(
        &mut b,
        "Measured floating point speed",
        &format!("{:.2} billion ops/sec", h.p_fpops / 1e9),
    );
    row2(
        &mut b,
        "Measured integer speed",
        &format!("{:.2} billion ops/sec", h.p_iops / 1e9),
    );
    for (label, rate) in [
        ("Average upload rate", h.n_bwup),
        ("Average download rate", h.n_bwdown),
    ] {
        let v = rate / MEGA;
        if v >= 0.005 {
            row2(&mut b, label, &format!("{v:.2} MB/sec"));
        } else {
            row2(&mut b, label, "Unknown");
        }
    }
    if h.avg_turnaround > 0.0 {
        row2(
            &mut b,
            "Average turnaround time",
            &time_diff(h.avg_turnaround),
        );
    }
    row2(
        &mut b,
        "Tasks",
        &format!("<a href=\"results.php?hostid={}\">View</a>", h.id),
    );
    row2(
        &mut b,
        "Number of times client has contacted server",
        &h.rpc_seqno.to_string(),
    );
    if private {
        row2(&mut b, "Last time contacted server", &time_str(h.rpc_time));
        let pct = |x: f64| format!("{}%", number_format(100.0 * x, 2));
        row2(&mut b, "Fraction of time BOINC is running", &pct(h.on_frac));
        if h.connected_frac > 0.0 {
            row2(
                &mut b,
                "While BOINC is running, fraction of time computer has an Internet connection",
                &pct(h.connected_frac),
            );
        }
        row2(
            &mut b,
            "While BOINC is running, fraction of time computing is allowed",
            &pct(h.active_frac),
        );
        row2(
            &mut b,
            "While BOINC is running, fraction of time GPU computing is allowed",
            &pct(h.gpu_active_frac),
        );
        if h.cpu_efficiency > 0.0 {
            row2(
                &mut b,
                "Average CPU efficiency",
                &h.cpu_efficiency.to_string(),
            );
        }
        if h.duration_correction_factor > 0.0 {
            row2(
                &mut b,
                "Task duration correction factor",
                &h.duration_correction_factor.to_string(),
            );
        }
    } else {
        row2(&mut b, "Last contact", &date_str(h.rpc_time));
    }
    b.push_str("</table>\n");
    Ok(SitePage::html(
        200,
        layout(project, &format!("Computer {}", h.id), &b),
    ))
}

// ---------------------------------------------------------------------------
// Tasks.

/// The `result` columns the task list shows; not the XML documents or
/// stderr.
const RESULT_LIST_COLS: &str = "id, workunitid, server_state, outcome, client_state, hostid, \
     userid, report_deadline, sent_time, received_time, name, cpu_time, validate_state, \
     granted_credit, appid, exit_status, elapsed_time, app_version_id, app_version_num";

/// The task states of upstream's task list filter (`$state_name`).
const STATE_NAMES: [&str; 7] = [
    "All",
    "In progress",
    "Validation pending",
    "Validation inconclusive",
    "Valid",
    "Invalid",
    "Error",
];

/// `$state_clause` for a state of [`STATE_NAMES`].
fn state_clause(state: usize) -> String {
    let over = RESULT_SERVER_STATE_OVER;
    let success = RESULT_OUTCOME_SUCCESS;
    match state {
        1 => format!(" AND server_state = {RESULT_SERVER_STATE_IN_PROGRESS}"),
        2 => format!(
            " AND server_state = {over} AND outcome = {success} AND validate_state = {VALIDATE_STATE_INIT}"
        ),
        3 => format!(
            " AND server_state = {over} AND outcome = {success} AND validate_state = {VALIDATE_STATE_INCONCLUSIVE}"
        ),
        4 => format!(
            " AND server_state = {over} AND outcome = {success} AND validate_state = {VALIDATE_STATE_VALID}"
        ),
        5 => format!(
            " AND server_state = {over} AND (outcome = {RESULT_OUTCOME_VALIDATE_ERROR} OR \
             (outcome = {success} AND validate_state IN ({VALIDATE_STATE_INVALID}, \
             {VALIDATE_STATE_NO_CHECK}, {VALIDATE_STATE_TOO_LATE})))"
        ),
        6 => format!(
            " AND server_state = {over} AND outcome IN ({RESULT_OUTCOME_CLIENT_ERROR}, \
             {RESULT_OUTCOME_NO_REPLY}, {RESULT_OUTCOME_CLIENT_DETACHED})"
        ),
        _ => String::new(),
    }
}

/// `state_string`.
fn state_string(r: &JobResult) -> &'static str {
    match r.server_state {
        RESULT_SERVER_STATE_INACTIVE => "Inactive",
        RESULT_SERVER_STATE_UNSENT => "Unsent",
        RESULT_SERVER_STATE_IN_PROGRESS => "In progress",
        RESULT_SERVER_STATE_OVER => match r.outcome {
            RESULT_OUTCOME_SUCCESS => match r.validate_state {
                VALIDATE_STATE_INIT => "Completed, waiting for validation",
                VALIDATE_STATE_VALID => "Completed and validated",
                VALIDATE_STATE_INVALID => "Completed, marked as invalid",
                VALIDATE_STATE_NO_CHECK => "Completed, can't validate",
                VALIDATE_STATE_INCONCLUSIVE => "Completed, validation inconclusive",
                VALIDATE_STATE_TOO_LATE => "Completed, too late to validate",
                _ => "Completed",
            },
            RESULT_OUTCOME_COULDNT_SEND => "Couldn't send",
            RESULT_OUTCOME_CLIENT_ERROR => {
                if r.exit_status == -221 || r.exit_status == 202 {
                    "Cancelled by server"
                } else if r.exit_status == -233 || r.exit_status == 200 {
                    "Not started by deadline - canceled"
                } else {
                    match r.client_state {
                        RESULT_FILES_DOWNLOADING => "Error while downloading",
                        RESULT_FILES_DOWNLOADED | RESULT_COMPUTE_ERROR => "Error while computing",
                        RESULT_FILES_UPLOADING => "Error while uploading",
                        RESULT_ABORTED => "Aborted",
                        RESULT_UPLOAD_FAILED => "Upload failed",
                        _ => "Error",
                    }
                }
            }
            RESULT_OUTCOME_NO_REPLY => "Timed out - no response",
            RESULT_OUTCOME_DIDNT_NEED => "Didn't need",
            RESULT_OUTCOME_VALIDATE_ERROR => "Validate error",
            RESULT_OUTCOME_CLIENT_DETACHED => "Abandoned",
            _ => "Unknown",
        },
        _ => "Unknown",
    }
}

fn server_state_string(r: &JobResult) -> &'static str {
    match r.server_state {
        RESULT_SERVER_STATE_INACTIVE => "Inactive",
        RESULT_SERVER_STATE_UNSENT => "Unsent",
        RESULT_SERVER_STATE_IN_PROGRESS => "In progress",
        RESULT_SERVER_STATE_OVER => "Over",
        _ => "Unknown",
    }
}

fn outcome_string(r: &JobResult) -> &'static str {
    match r.outcome {
        RESULT_OUTCOME_INIT => "---",
        RESULT_OUTCOME_SUCCESS => "Success",
        RESULT_OUTCOME_COULDNT_SEND => "Couldn't send",
        RESULT_OUTCOME_CLIENT_ERROR if r.exit_status == -221 => "Redundant result",
        RESULT_OUTCOME_CLIENT_ERROR => "Computation error",
        RESULT_OUTCOME_NO_REPLY => "No reply",
        RESULT_OUTCOME_DIDNT_NEED => "Didn't need",
        RESULT_OUTCOME_VALIDATE_ERROR => "Validate error",
        RESULT_OUTCOME_CLIENT_DETACHED => "Abandoned",
        _ => "Unknown",
    }
}

fn client_state_string(r: &JobResult) -> &'static str {
    match r.client_state {
        RESULT_NEW => "New",
        RESULT_FILES_DOWNLOADING => "Downloading",
        RESULT_FILES_DOWNLOADED => "Processing",
        RESULT_COMPUTE_ERROR => "Compute error",
        RESULT_FILES_UPLOADING => "Uploading",
        RESULT_FILES_UPLOADED => "Done",
        RESULT_ABORTED if r.exit_status == -221 || r.exit_status == 202 => "Cancelled by server",
        RESULT_ABORTED => "Aborted by user",
        RESULT_UPLOAD_FAILED => "Upload failed",
        _ => "Unknown",
    }
}

fn validate_state_string(r: &JobResult) -> &'static str {
    match r.validate_state {
        VALIDATE_STATE_INIT => "Initial",
        VALIDATE_STATE_VALID => "Valid",
        VALIDATE_STATE_INVALID if r.exit_status == -221 => "Not necessary",
        VALIDATE_STATE_INVALID => "Invalid",
        VALIDATE_STATE_NO_CHECK => "Workunit error - check skipped",
        VALIDATE_STATE_INCONCLUSIVE => "Checked, but no consensus yet",
        VALIDATE_STATE_TOO_LATE => "Task was reported too late to validate",
        _ => "Unknown",
    }
}

/// `result_granted_credit_string`.
fn credit_string(r: &JobResult) -> String {
    let credit = number_format(r.granted_credit, 2);
    if r.server_state == RESULT_SERVER_STATE_IN_PROGRESS && r.granted_credit > 0.0 {
        return credit;
    }
    if r.server_state != RESULT_SERVER_STATE_OVER {
        return "---".into();
    }
    if r.outcome == RESULT_OUTCOME_SUCCESS {
        return match r.validate_state {
            VALIDATE_STATE_INIT | VALIDATE_STATE_INCONCLUSIVE => "pending".into(),
            _ => credit,
        };
    }
    if r.granted_credit > 0.0 {
        credit
    } else {
        "---".into()
    }
}

/// `app_version_string`. Escaped.
fn app_version_string(project: &Project, r: &JobResult) -> String {
    let catalog = &project.catalog;
    let app_name = |appid: i64| {
        catalog
            .apps
            .iter()
            .find(|a| a.id == appid)
            .map(|a| html_escape(&a.user_friendly_name))
            .unwrap_or_else(|| format!("app {appid}"))
    };
    if r.app_version_id == 0 {
        return "---".into();
    }
    if r.app_version_id < 0 {
        // `anon_platform_string` in `html/inc/result.inc`: the resource type
        // is shown for the types the scheduler writes, and not for the
        // unknown one.
        let resource = match r.app_version_id {
            ANON_PLATFORM_CPU => " (CPU)",
            ANON_PLATFORM_NVIDIA => " (NVIDIA GPU)",
            ANON_PLATFORM_ATI => " (AMD GPU)",
            ANON_PLATFORM_INTEL_GPU => " (Intel GPU)",
            ANON_PLATFORM_APPLE_GPU => " (Apple GPU)",
            _ => "",
        };
        return format!("{}<br>Anonymous platform{resource}", app_name(r.appid));
    }
    let Some(av) = catalog
        .app_versions
        .iter()
        .find(|v| v.id == r.app_version_id)
    else {
        return format!(
            "{} v{}.{:02}",
            app_name(r.appid),
            r.app_version_num / 100,
            r.app_version_num % 100
        );
    };
    let platform = catalog
        .platforms
        .iter()
        .find(|p| p.id == av.platformid)
        .map(|p| html_escape(&p.name))
        .unwrap_or_default();
    let class = if av.plan_class.is_empty() {
        String::new()
    } else {
        format!(" ({})", html_escape(&av.plan_class))
    };
    format!(
        "{} v{}.{:02}{class}<br>{platform}",
        app_name(av.appid),
        av.version_num / 100,
        av.version_num % 100
    )
}

fn result_table_start(b: &mut String, show_wu: bool, show_host: bool, names_link: Option<&str>) {
    b.push_str("<table>\n<tr><th>Task");
    if let Some(link) = names_link {
        b.push_str(&format!("<br><small>{link}</small>"));
    }
    b.push_str("</th>");
    if show_wu {
        b.push_str("<th>Work unit</th>");
    }
    if show_host {
        b.push_str("<th>Computer</th>");
    }
    b.push_str(
        "<th>Sent</th><th>Time reported<br>or deadline</th><th>Status</th><th>Run time</th>\
         <th>CPU time</th><th>Credit</th><th>Application</th></tr>\n",
    );
}

/// `show_result_row`.
fn result_row(
    b: &mut String,
    project: &Project,
    r: &JobResult,
    now: i64,
    show_wu: bool,
    show_host: bool,
    show_name: bool,
) {
    let reported = if r.received_time > 0 {
        time_str(r.received_time)
    } else if r.report_deadline > 0 {
        let class = if r.report_deadline > now {
            "due"
        } else {
            "overdue"
        };
        format!(
            "<span class=\"{class}\">{}</span>",
            time_str(r.report_deadline)
        )
    } else {
        "---".into()
    };
    let label = if show_name {
        html_escape(&r.name)
    } else {
        r.id.to_string()
    };
    b.push_str(&format!(
        "<tr><td><a href=\"result.php?resultid={}\">{label}</a></td>",
        r.id
    ));
    if show_wu {
        b.push_str(&format!(
            "<td><a href=\"workunit.php?wuid={0}\">{0}</a></td>",
            r.workunitid
        ));
    }
    if show_host {
        b.push_str(&format!("<td>{}</td>", host_link(r.hostid)));
    }
    let (elapsed, cpu) = if r.server_state == RESULT_SERVER_STATE_OVER {
        (time_diff2(r.elapsed_time), time_diff2(r.cpu_time))
    } else {
        ("---".into(), "---".into())
    };
    b.push_str(&format!(
        "<td>{}</td><td>{reported}</td><td>{}</td><td>{elapsed}</td><td>{cpu}</td><td>{}</td><td>{}</td></tr>\n",
        time_str(r.sent_time),
        state_string(r),
        credit_string(r),
        app_version_string(project, r),
    ));
}

/// `results.php`: tasks of a computer (public, as upstream) or of the
/// logged-in account, newest first, [`RESULTS_PER_PAGE`] to a page.
///
/// Paging is by task ID (`before=<id>` for older, `after=<id>` for newer)
/// rather than upstream's `offset`, so that a page reads the same number
/// of index entries however deep it is.
///
/// The indexes hold the computer or account and the task ID only, so with a
/// state or application filter a page reads at most
/// [`RESULTS_FILTER_WINDOW`] tasks and, when those hold fewer matches than
/// fit, links on from the last one read. One round trip of two statements,
/// three with a filter.
pub async fn results(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    let hostid = int_param(q, "hostid").filter(|&id| id > 0);
    let userid = int_param(q, "userid").filter(|&id| id > 0);
    let state = int_param(q, "state")
        .filter(|&s| (0..STATE_NAMES.len() as i64).contains(&s))
        .unwrap_or(0) as usize;
    let appid = int_param(q, "appid").filter(|&id| id > 0);
    let show_names = int_param(q, "show_names") == Some(1);
    let before = int_param(q, "before").filter(|&id| id > 0);
    let after = int_param(q, "after").filter(|&id| id > 0);

    // The first statement checks the computer or the viewer; the second is
    // the page, which for an account matches only when the cookie belongs
    // to it.
    let (first, mut sql, mut params): (Stmt, String, Vec<cb_db::Value>) = if let Some(h) = hostid {
        (
            stmt!("SELECT id FROM host WHERE id = ?", h),
            "hostid = ? AND hostid > 0".to_string(),
            vec![h.into()],
        )
    } else if let Some(u) = userid {
        let Some(auth) = ctx.auth.filter(|a| plausible_account_key(a)) else {
            return Ok(to_login(project, &format!("results.php?userid={u}")));
        };
        (
            user_by_auth_stmt(auth),
            "userid = ? AND userid > 0 \
             AND userid = (SELECT id FROM user WHERE authenticator = ? AND blocked = 0)"
                .to_string(),
            vec![u.into(), auth.into()],
        )
    } else {
        return Ok(error_page(project, 400, "Missing user ID or host ID"));
    };
    // `sql` is the computer's or account's tasks from the cursor on, in
    // index order (`res_host`, `res_user`).
    let ascending = before.is_none() && after.is_some();
    if let Some(b) = before {
        sql.push_str(" AND id < ?");
        params.push(b.into());
    } else if let Some(a) = after {
        sql.push_str(" AND id > ?");
        params.push(a.into());
    }
    let dir = if ascending { "ASC" } else { "DESC" };
    let filtered = state != 0 || appid.is_some();
    let mut reads = vec![first];
    if filtered {
        // The indexes hold no state or application, so a filter with few
        // matches would read the whole history. It reads the next
        // RESULTS_FILTER_WINDOW tasks instead, and a third statement tells
        // how far that went.
        let window =
            format!("FROM result WHERE {sql} ORDER BY id {dir} LIMIT {RESULTS_FILTER_WINDOW}");
        let mut page_sql = format!(
            "SELECT {RESULT_LIST_COLS} FROM (SELECT {RESULT_LIST_COLS} {window}) WHERE 1 = 1{}",
            state_clause(state)
        );
        let mut page_params = params.clone();
        if let Some(a) = appid {
            page_sql.push_str(" AND appid = ?");
            page_params.push(a.into());
        }
        page_sql.push_str(&format!(" ORDER BY id {dir} LIMIT ?"));
        page_params.push((RESULTS_PER_PAGE + 1).into());
        reads.push(Stmt {
            sql: page_sql,
            params: page_params,
        });
        reads.push(Stmt {
            sql: format!(
                "SELECT min(id) AS lo, max(id) AS hi, count(*) AS n FROM (SELECT id {window})"
            ),
            params,
        });
    } else {
        params.push((RESULTS_PER_PAGE + 1).into());
        reads.push(Stmt {
            sql: format!(
                "SELECT {RESULT_LIST_COLS} FROM result WHERE {sql} ORDER BY id {dir} LIMIT ?"
            ),
            params,
        });
    }

    let mut out = ctx.db.batch(reads).await?.into_iter();
    let first_rows = out.next().unwrap_or_default();
    let mut rows: Vec<JobResult> = out.next().unwrap_or_default().decode()?;
    let window: Option<WindowRow> = match out.next() {
        Some(rows) => rows.first()?,
        None => None,
    };

    let title;
    let clause;
    if let Some(h) = hostid {
        if first_rows.rows.is_empty() {
            return Ok(error_page(
                project,
                404,
                &format!("No computer with ID {h} found"),
            ));
        }
        title = format!("tasks for computer {h}");
        clause = format!("hostid={h}");
    } else {
        let u = userid.unwrap_or_default();
        let me: Option<User> = first_rows.first()?;
        match me {
            Some(me) if me.id == u => {
                title = format!("tasks for {}", me.name);
                clause = format!("userid={u}");
            }
            Some(_) => return Ok(error_page(project, 403, "No access")),
            None => return Ok(to_login(project, &format!("results.php?userid={u}"))),
        }
    }

    let more = rows.len() > RESULTS_PER_PAGE;
    rows.truncate(RESULTS_PER_PAGE);
    if ascending {
        rows.reverse();
    }
    // Where the neighbouring pages start: after the last task shown or,
    // when a filter's window ended before the page was full, after the last
    // task it looked at.
    let window_end = window
        .filter(|w| !more && w.n >= RESULTS_FILTER_WINDOW as i64)
        .and_then(|w| if ascending { w.hi } else { w.lo });
    let newest = rows.first().map(|r| r.id);
    let oldest = rows.last().map(|r| r.id);
    let (newer, older): (Option<i64>, Option<i64>) = if ascending {
        (
            if more { newest } else { window_end },
            oldest.or(after.map(|a| a + 1)),
        )
    } else {
        (
            before.and_then(|b| newest.or(Some(b - 1))),
            if more { oldest } else { window_end },
        )
    };

    let app_name = appid.and_then(|a| project.catalog.apps.iter().find(|x| x.id == a));
    let base = |state: usize, names: bool| {
        let mut s = format!(
            "results.php?{clause}&amp;state={state}&amp;show_names={}",
            u8::from(names)
        );
        if let Some(a) = appid {
            s.push_str(&format!("&amp;appid={a}"));
        }
        s
    };
    let here = |names: bool| {
        let mut s = base(state, names);
        if let Some(b) = before {
            s.push_str(&format!("&amp;before={b}"));
        } else if let Some(a) = after {
            s.push_str(&format!("&amp;after={a}"));
        }
        s
    };

    let mut b = String::from("<p>");
    for (i, name) in STATE_NAMES.iter().enumerate() {
        if i > 0 {
            b.push_str(" &middot; ");
        }
        if i == state {
            b.push_str(&format!("<b>{name}</b>"));
        } else {
            b.push_str(&format!("<a href=\"{}\">{name}</a>", base(i, show_names)));
        }
    }
    b.push_str("</p>\n");
    let mut nav = String::from("<p>");
    if let Some(id) = newer {
        nav.push_str(&format!(
            "<a href=\"{}&amp;after={id}\">Previous {RESULTS_PER_PAGE}</a>",
            base(state, show_names)
        ));
    }
    if let Some(id) = older {
        if newer.is_some() {
            nav.push_str(" &middot; ");
        }
        nav.push_str(&format!(
            "<a href=\"{}&amp;before={id}\">Next {RESULTS_PER_PAGE}</a>",
            base(state, show_names)
        ));
    }
    nav.push_str("</p>\n");

    if rows.is_empty() {
        b.push_str(if window_end.is_some() {
            "<p>No matching tasks among the ones looked at; older or newer ones may match.</p>\n"
        } else {
            "<p>No tasks to display</p>\n"
        });
    } else {
        b.push_str(&nav);
        let names_link = format!(
            "<a href=\"{}\">{}</a>",
            here(!show_names),
            if show_names { "Show IDs" } else { "Show names" }
        );
        result_table_start(
            &mut b,
            true,
            userid.is_some() && hostid.is_none(),
            Some(&names_link),
        );
        for r in &rows {
            result_row(
                &mut b,
                project,
                r,
                ctx.now,
                true,
                hostid.is_none(),
                show_names,
            );
        }
        b.push_str("</table>\n");
    }
    b.push_str(&nav);
    let mut heading = format!("{} {title}", STATE_NAMES[state]);
    if let Some(app) = app_name {
        heading = format!("{} {} {title}", STATE_NAMES[state], app.user_friendly_name);
    }
    Ok(SitePage::html(200, layout(project, &heading, &b)))
}

/// `exit_status_string`, with the names of the common codes.
fn exit_status_string(x: i32) -> String {
    let name = match x {
        0 => "",
        192..=209 => [
            "EXIT_STATEFILE_WRITE",
            "EXIT_SIGNAL",
            "EXIT_ABORTED_BY_CLIENT",
            "EXIT_CHILD_FAILED",
            "EXIT_DISK_LIMIT_EXCEEDED",
            "EXIT_TIME_LIMIT_EXCEEDED",
            "EXIT_MEM_LIMIT_EXCEEDED",
            "EXIT_CLIENT_EXITING",
            "EXIT_UNSTARTED_LATE",
            "EXIT_MISSING_COPROC",
            "EXIT_ABORTED_BY_PROJECT",
            "EXIT_ABORTED_VIA_GUI",
            "EXIT_UNKNOWN",
            "EXIT_OUT_OF_MEMORY",
            "EXIT_INIT_FAILURE",
            "EXIT_NO_SUB_TASKS",
            "EXIT_SUB_TASK_FAILURE",
            "EXIT_OVERDUE_EXCEEDED",
        ][(x - 192) as usize],
        -221 => "ERR_ABORTED_BY_PROJECT",
        -233 => "ERR_UNSTARTED_LATE",
        -234 => "ERR_MISSING_COPROC",
        -177 => "ERR_RSC_LIMIT_EXCEEDED",
        -186 => "ERR_RESULT_DOWNLOAD",
        -187 => "ERR_RESULT_UPLOAD",
        -161 => "ERR_NOT_FOUND",
        -148 => "ERR_EXEC",
        -131 => "ERR_FILE_TOO_BIG",
        -119 => "ERR_MD5_FAILED",
        -120 => "ERR_RSA_FAILED",
        _ => "Unknown error code",
    };
    format!("{x} (0x{:08X}) {name}", x as u32)
}

/// `result.php`: one task, by `resultid` or `result_name`. Public, as
/// upstream. One statement.
pub async fn result(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    let (read, label) = match int_param(q, "resultid").filter(|&id| id > 0) {
        Some(id) => (
            stmt!("SELECT * FROM result WHERE id = ?", id),
            id.to_string(),
        ),
        None => {
            let name = str_param(q, "result_name");
            (
                stmt!("SELECT * FROM result WHERE name = ?", name),
                name.to_string(),
            )
        }
    };
    let found: Option<JobResult> = ctx.db.run(read).await?.first()?;
    let Some(r) = found else {
        return Ok(error_page(project, 404, &format!("No such task: {label}")));
    };
    let mut b = String::from("<table>\n");
    row2(&mut b, "Name", &html_escape(&r.name));
    row2(
        &mut b,
        "Workunit",
        &format!("<a href=\"workunit.php?wuid={0}\">{0}</a>", r.workunitid),
    );
    row2(&mut b, "Created", &time_str(r.create_time));
    row2(&mut b, "Sent", &time_str(r.sent_time));
    row2(&mut b, "Report deadline", &time_str(r.report_deadline));
    row2(&mut b, "Received", &time_str(r.received_time));
    row2(&mut b, "Server state", server_state_string(&r));
    row2(&mut b, "Outcome", outcome_string(&r));
    row2(&mut b, "Client state", client_state_string(&r));
    row2(&mut b, "Computer ID", &host_link(r.hostid));
    if r.server_state == RESULT_SERVER_STATE_OVER {
        row2(&mut b, "Exit status", &exit_status_string(r.exit_status));
        row2(&mut b, "Run time", &time_diff(r.elapsed_time));
        row2(&mut b, "CPU time", &time_diff(r.cpu_time));
    }
    if r.outcome == RESULT_OUTCOME_SUCCESS {
        row2(&mut b, "Credit", &number_format(r.granted_credit, 2));
        row2(&mut b, "Validate state", validate_state_string(&r));
    }
    row2(&mut b, "Priority", &r.priority.to_string());
    row2(
        &mut b,
        "Device peak FLOPS",
        &format!("{} GFLOPS", number_format(r.flops_estimate / 1e9, 2)),
    );
    row2(
        &mut b,
        "Application version",
        &app_version_string(project, &r),
    );
    if r.peak_working_set_size > 0.0 {
        row2(
            &mut b,
            "Peak working set size",
            &size_string(r.peak_working_set_size),
        );
    }
    if r.peak_swap_size > 0.0 {
        row2(&mut b, "Peak swap size", &size_string(r.peak_swap_size));
    }
    if r.peak_disk_usage > 0.0 {
        row2(&mut b, "Peak disk usage", &size_string(r.peak_disk_usage));
    }
    b.push_str("</table>\n");
    if r.server_state == RESULT_SERVER_STATE_OVER {
        b.push_str(&format!(
            "<h2>Stderr output</h2>\n<pre>{}</pre>\n",
            html_escape(&r.stderr_out)
        ));
    }
    Ok(SitePage::html(
        200,
        layout(project, &format!("Task {}", r.id), &b),
    ))
}

/// An input file of a workunit, from its `xml_doc`.
#[derive(Debug, Default, Clone, PartialEq)]
struct InputFileRow {
    open_name: String,
    url: String,
    nbytes: f64,
}

/// The input files and command line of a workunit document.
fn wu_inputs(xml_doc: &str) -> (Vec<InputFileRow>, String) {
    use cb_proto::xml::XmlParser;
    let mut infos: Vec<(String, String, f64)> = Vec::new();
    let mut refs: Vec<(String, String)> = Vec::new();
    let mut command_line = String::new();
    let mut xp = XmlParser::new(xml_doc);
    while xp.get_tag() {
        if !xp.is_tag() {
            continue;
        }
        if xp.match_tag("file_info") {
            let (mut name, mut url, mut nbytes) = (String::new(), String::new(), 0.0);
            while xp.get_tag() {
                if !xp.is_tag() {
                    continue;
                }
                if xp.match_tag("/file_info") {
                    break;
                }
                if let Some(v) = xp.parse_str("name") {
                    name = v;
                } else if let Some(v) = xp.parse_str("url") {
                    if url.is_empty() {
                        url = v;
                    }
                } else if let Some(v) = xp.parse_double("nbytes") {
                    nbytes = v;
                } else {
                    xp.skip_unexpected();
                }
            }
            infos.push((name, url, nbytes));
            continue;
        }
        if xp.match_tag("file_ref") {
            let (mut file_name, mut open_name) = (String::new(), String::new());
            while xp.get_tag() {
                if !xp.is_tag() {
                    continue;
                }
                if xp.match_tag("/file_ref") {
                    break;
                }
                if let Some(v) = xp.parse_str("file_name") {
                    file_name = v;
                } else if let Some(v) = xp.parse_str("open_name") {
                    open_name = v;
                } else {
                    xp.skip_unexpected();
                }
            }
            refs.push((file_name, open_name));
            continue;
        }
        if let Some(v) = xp.parse_str("command_line") {
            command_line = v;
        }
    }
    let rows = refs
        .into_iter()
        .filter_map(|(file_name, open_name)| {
            let (_, url, nbytes) = infos.iter().find(|(n, _, _)| *n == file_name)?;
            Some(InputFileRow {
                open_name: if open_name.is_empty() {
                    file_name
                } else {
                    open_name
                },
                url: url.clone(),
                nbytes: *nbytes,
            })
        })
        .collect();
    (rows, command_line)
}

/// `wu_error_mask_str`.
fn wu_error_mask_str(mut s: i32) -> String {
    let mut x = String::new();
    for (bit, text) in [
        (WU_ERROR_COULDNT_SEND_RESULT, "Couldn't send result"),
        (
            WU_ERROR_TOO_MANY_ERROR_RESULTS,
            "Too many errors (may have bug)",
        ),
        (
            WU_ERROR_TOO_MANY_SUCCESS_RESULTS,
            "Too many results (may be nondeterministic)",
        ),
        (WU_ERROR_TOO_MANY_TOTAL_RESULTS, "Too many total results"),
        (WU_ERROR_CANCELLED, "WU cancelled"),
        (WU_ERROR_NO_CANONICAL_RESULT, "Canonical result is missing"),
    ] {
        if s & bit != 0 {
            x.push(' ');
            x.push_str(text);
            s &= !bit;
        }
    }
    if s != 0 {
        x.push_str(&format!(" Unrecognized Error: {s}"));
    }
    x
}

/// `workunit.php`: one workunit and its tasks. While an app with adaptive
/// replication (`target_nresults > 0`) has no canonical result, the tasks
/// are not shown, as upstream. One round trip of two statements.
pub async fn workunit(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    let Some(wuid) = int_param(q, "wuid").filter(|&id| id > 0) else {
        return Ok(error_page(project, 404, "can't find workunit"));
    };
    let mut out = ctx
        .db
        .batch(vec![
            stmt!("SELECT * FROM workunit WHERE id = ?", wuid),
            stmt!(
                format!(
                    "SELECT {RESULT_LIST_COLS} FROM result WHERE workunitid = ? ORDER BY id LIMIT ?"
                ),
                wuid,
                MAX_WU_RESULTS
            ),
        ])
        .await?
        .into_iter();
    let wu: Option<Workunit> = out.next().unwrap_or_default().first()?;
    let results: Vec<JobResult> = out.next().unwrap_or_default().decode()?;
    let Some(wu) = wu else {
        return Ok(error_page(project, 404, "can't find workunit"));
    };
    let app = project.catalog.apps.iter().find(|a| a.id == wu.appid);
    let (inputs, command_line) = wu_inputs(&wu.xml_doc);

    let mut b = String::from("<table>\n");
    row2(&mut b, "name", &html_escape(&wu.name));
    row2(
        &mut b,
        "application",
        &app.map(|a| html_escape(&a.user_friendly_name))
            .unwrap_or_else(|| format!("app {}", wu.appid)),
    );
    if wu.batch > 0 {
        row2(&mut b, "batch", &wu.batch.to_string());
    }
    row2(&mut b, "created", &time_str(wu.create_time));
    if wu.canonical_resultid > 0 {
        row2(
            &mut b,
            "canonical result",
            &format!(
                "<a href=\"result.php?resultid={0}\">{0}</a>",
                wu.canonical_resultid
            ),
        );
        row2(
            &mut b,
            "granted credit",
            &number_format(wu.canonical_credit, 2),
        );
    }
    let suppressed = app.is_some_and(|a| a.target_nresults > 0)
        && wu.canonical_resultid == 0
        && wu.error_mask == 0;
    if suppressed {
        row2(&mut b, "Tasks in progress", "suppressed pending completion");
        b.push_str("</table>\n");
    } else {
        row2(&mut b, "minimum quorum", &wu.min_quorum.to_string());
        row2(&mut b, "target #results", &wu.target_nresults.to_string());
        row2(
            &mut b,
            "max # of error/total/success tasks",
            &format!(
                "{}, {}, {}",
                wu.max_error_results, wu.max_total_results, wu.max_success_results
            ),
        );
        if wu.error_mask != 0 {
            row2(&mut b, "errors", &wu_error_mask_str(wu.error_mask));
        }
        if wu.need_validate != 0 {
            row2(&mut b, "validation", "Pending");
        }
        row2(&mut b, "Command line", &html_escape(&command_line));
        row2(&mut b, "Priority", &wu.priority.to_string());
        b.push_str("</table>\n<h2>Job instances</h2>\n");
        result_table_start(&mut b, false, true, None);
        for r in &results {
            result_row(&mut b, project, r, ctx.now, false, true, false);
        }
        b.push_str("</table>\n");
    }
    b.push_str("<h2>Input files</h2>\n<table>\n<tr><th>Name</th><th>Size (bytes)</th></tr>\n");
    for f in &inputs {
        b.push_str(&format!(
            "<tr><td><a href=\"{}\">{}</a></td><td>{}</td></tr>\n",
            html_escape(&f.url),
            html_escape(&f.open_name),
            f.nbytes
        ));
    }
    b.push_str("</table>\n");
    Ok(SitePage::html(
        200,
        layout(project, &format!("Workunit {}", wu.id), &b),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_numbers() {
        assert_eq!(date_str(0), "---");
        assert_eq!(date_str(86_400 * 365), "1 Jan 1971");
        assert_eq!(time_str(1_700_000_000), "14 Nov 2023, 22:13:20 UTC");
        assert_eq!(time_str(951_782_400), "29 Feb 2000, 0:00:00 UTC");
        assert_eq!(number_format(1234567.891, 2), "1,234,567.89");
        assert_eq!(number_format(999.0, 0), "999");
        assert_eq!(number_format(-1000.5, 1), "-1,000.5");
        assert_eq!(number_format(-0.001, 2), "0.00");
        assert_eq!(time_diff(3725.5), "1 hours 2 min 5.50 sec");
        assert_eq!(time_diff2(7200.0), "2.00 hours");
        assert_eq!(size_string(3.0 * MEGA), "3.00 MB");
        assert_eq!(
            exit_status_string(-221),
            "-221 (0xFFFFFF23) ERR_ABORTED_BY_PROJECT"
        );
    }

    #[test]
    fn cookies_and_urls() {
        assert_eq!(
            auth_from_cookie("a=b; auth=abc123 ; c=d").as_deref(),
            Some("abc123")
        );
        assert_eq!(auth_from_cookie("auth=; x=y"), None);
        assert_eq!(auth_from_cookie("xauth=1"), None);
        assert_eq!(
            local_url("results.php?userid=3"),
            Some("results.php?userid=3")
        );
        assert_eq!(local_url("/home.php"), Some("home.php"));
        assert_eq!(local_url("https://evil.example/x.php"), None);
        assert_eq!(local_url("//evil.example/x.php"), None);
        assert_eq!(local_url("home.php?x=\"><script>"), None);
        assert_eq!(
            account_file_name("https://p.example/sub/"),
            "account_p.example_sub.xml"
        );
        assert!(!plausible_account_key("1_abcdef"));
        assert!(plausible_account_key("0123456789abcdef0123456789abcdef"));
    }

    #[test]
    fn host_summary_is_read_and_escaped() {
        let s = "[BOINC|8.0.4][CUDA|2|8192MB][vbox|7.0<b>][docker|24]";
        assert_eq!(boinc_version(s), "8.0.4");
        assert_eq!(gpu_desc(s), "[2] CUDA (8192MB)");
        assert_eq!(runtime_desc(s, "vbox"), "7.0&lt;b&gt;");
        assert_eq!(runtime_desc(s, "docker"), "24");
        assert_eq!(gpu_desc(""), "---");
    }

    #[test]
    fn workunit_inputs() {
        let doc = "<file_info>\n<name>in_1</name>\n<url>https://p.example/download/in_1</url>\n\
                   <md5_cksum>x</md5_cksum>\n<nbytes>12</nbytes>\n</file_info>\n<workunit>\n\
                   <file_ref>\n<file_name>in_1</file_name>\n<open_name>in</open_name>\n</file_ref>\n\
                   <command_line>--x 1</command_line>\n<rsc_fpops_est>1e12</rsc_fpops_est>\n</workunit>\n";
        let (rows, cmd) = wu_inputs(doc);
        assert_eq!(cmd, "--x 1");
        assert_eq!(
            rows,
            vec![InputFileRow {
                open_name: "in".into(),
                url: "https://p.example/download/in_1".into(),
                nbytes: 12.0
            }]
        );
    }
}
