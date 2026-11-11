//! The web RPCs a client uses to attach, the master page and the server
//! status. Replies have the shapes of upstream `html/user/*.php`.

use cb_crypto::account::{
    pepper_passwd_hash, random_hex_string, verify_peppered_passwd_hash, weak_auth,
};
use cb_db::{query_one, stmt, Db, Stmt};
use cb_proto::consts::*;
use cb_proto::web_rpc::{
    write_create_account, write_error, write_lookup_account, write_lookup_account_exists,
    AccountError, ConfigPlatform, ProjectConfig, XML_HEADER,
};
use cb_proto::xml::xml_escape;
use serde::Serialize;

use crate::model::User;
use crate::project::{scheduler_url, Project};
use crate::{Config, CoreError, CoreResult};

/// Upper bounds on what an account RPC stores.
const MAX_EMAIL: usize = 254;
const MAX_NAME: usize = 254;

/// Sixteen bytes from the caller's random source.
pub fn random_bytes(random: &dyn Fn() -> u32) -> [u8; 16] {
    let mut out = [0u8; 16];
    for chunk in out.chunks_mut(4) {
        chunk.copy_from_slice(&random().to_le_bytes());
    }
    out
}

/// `is_valid_email_syntax` in spirit: one `@`, a dot in the domain, and no
/// character that could break out of XML or a header.
pub fn is_valid_email(addr: &str) -> bool {
    if addr.is_empty() || addr.len() > MAX_EMAIL {
        return false;
    }
    let Some((local, domain)) = addr.split_once('@') else {
        return false;
    };
    let ok = |c: char| c.is_ascii_alphanumeric() || "._%+-".contains(c);
    !local.is_empty()
        && local.chars().all(ok)
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && domain
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// `is_valid_user_name` (`html/inc/user_util.inc`): printable, no markup,
/// no leading or trailing space.
pub fn is_valid_user_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name.trim() == name
        && !name
            .chars()
            .any(|c| c.is_control() || c == '<' || c == '>' || c == '&')
}

fn is_passwd_hash(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

async fn user_by_email(db: &dyn Db, email: &str) -> CoreResult<Option<User>> {
    Ok(query_one(
        db,
        stmt!(
            "SELECT id, authenticator, passwd_hash, blocked FROM user WHERE email_addr = ?",
            email
        ),
    )
    .await?)
}

/// `lookup_account.php`. The reply is always a complete XML document.
pub async fn lookup_account(
    db: &dyn Db,
    pepper: &[u8],
    email_addr: &str,
    passwd_hash: &str,
) -> CoreResult<String> {
    let email = email_addr.trim().to_lowercase();
    if !is_valid_email(&email) {
        return Ok(AccountError::NotFound.reply());
    }
    let Some(user) = user_by_email(db, &email).await? else {
        return Ok(AccountError::NotFound.reply());
    };
    if passwd_hash.is_empty() {
        return Ok(write_lookup_account_exists());
    }
    let passwd_hash = passwd_hash.to_lowercase();
    if !is_passwd_hash(&passwd_hash)
        || user.passwd_hash.is_empty()
        || !verify_peppered_passwd_hash(pepper, &passwd_hash, &user.passwd_hash)
    {
        return Ok(AccountError::BadPassword.reply());
    }
    Ok(write_lookup_account(&user.authenticator))
}

/// What to make an account from.
#[derive(Debug, Clone, Default)]
pub struct NewAccount<'a> {
    pub email_addr: &'a str,
    /// `md5(password + lowercase email)`, as the client computes it; empty
    /// for an account that can only use its authenticator.
    pub passwd_hash: &'a str,
    pub user_name: &'a str,
}

/// Inserts a user. `Ok(Err(..))` is a refusal with the upstream error.
pub async fn make_user(
    db: &dyn Db,
    pepper: &[u8],
    now: i64,
    random: &dyn Fn() -> u32,
    account: &NewAccount<'_>,
) -> CoreResult<Result<String, AccountError>> {
    let email = account.email_addr.trim().to_lowercase();
    if !is_valid_email(&email) {
        return Ok(Err(AccountError::BadEmailAddr));
    }
    let passwd_hash = account.passwd_hash.to_lowercase();
    if !passwd_hash.is_empty() && !is_passwd_hash(&passwd_hash) {
        return Ok(Err(AccountError::BadPasswdHashLength));
    }
    let name = if account.user_name.is_empty() {
        email.split('@').next().unwrap_or_default().to_string()
    } else {
        account.user_name.to_string()
    };
    if !is_valid_user_name(&name) {
        return Ok(Err(AccountError::BadUserName));
    }
    if let Some(user) = user_by_email(db, &email).await? {
        // Creating an account that exists, with its password, is a lookup.
        if !passwd_hash.is_empty()
            && !user.passwd_hash.is_empty()
            && verify_peppered_passwd_hash(pepper, &passwd_hash, &user.passwd_hash)
        {
            return Ok(Ok(user.authenticator));
        }
        return Ok(Err(AccountError::NotUnique));
    }
    let authenticator = random_hex_string(&random_bytes(random));
    let cross_project_id = random_hex_string(&random_bytes(random));
    let stored = if passwd_hash.is_empty() {
        String::new()
    } else {
        pepper_passwd_hash(pepper, &passwd_hash)
    };
    let inserted = db
        .run(stmt!(
            "INSERT INTO user (create_time, email_addr, name, authenticator, cross_project_id, \
             passwd_hash, expavg_time) VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (email_addr) DO NOTHING RETURNING id",
            now,
            email.as_str(),
            name.as_str(),
            authenticator.as_str(),
            cross_project_id.as_str(),
            stored.as_str(),
            now
        ))
        .await?;
    if inserted.rows.is_empty() {
        return Ok(Err(AccountError::NotUnique));
    }
    Ok(Ok(authenticator))
}

/// `create_account.php`.
pub async fn create_account(
    db: &dyn Db,
    config: &Config,
    pepper: &[u8],
    now: i64,
    random: &dyn Fn() -> u32,
    account: &NewAccount<'_>,
) -> CoreResult<String> {
    if !config.account_creation {
        return Ok(AccountError::CreationDisabled.reply());
    }
    if account.passwd_hash.is_empty() {
        return Ok(AccountError::BadPasswdHashLength.reply());
    }
    Ok(match make_user(db, pepper, now, random, account).await? {
        Ok(authenticator) => write_create_account(&authenticator),
        Err(e) => e.reply(),
    })
}

/// `am_get_info.php` is not offered; clients that ask get the stock
/// "not implemented" error.
pub fn not_implemented() -> String {
    write_error(cb_proto::web_rpc::err::ERR_NOT_IMPLEMENTED, None)
}

/// `get_project_config.php`.
pub fn project_config(project: &Project) -> String {
    let c = &project.config;
    let mut platforms: Vec<ConfigPlatform> = Vec::new();
    for av in &project.catalog.app_versions {
        let Some(p) = project
            .catalog
            .platforms
            .iter()
            .find(|p| p.id == av.platformid)
        else {
            continue;
        };
        let entry = ConfigPlatform {
            name: p.name.clone(),
            user_friendly_name: p.user_friendly_name.clone(),
            plan_class: av.plan_class.clone(),
        };
        if !platforms.contains(&entry) {
            platforms.push(entry);
        }
    }
    ProjectConfig {
        name: xml_escape(&c.long_name),
        master_url: c.master_url.clone(),
        web_rpc_url_base: c.master_url.clone(),
        account_creation_disabled: !c.account_creation,
        sched_stopped: c.dont_send_jobs && c.ended,
        min_client_version: (c.min_core_client_version > 0).then_some(c.min_core_client_version),
        platforms,
        ..Default::default()
    }
    .write()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn page(title: &str, head: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{}</title>\n{head}\
         <style>body{{font-family:system-ui,sans-serif;max-width:46rem;margin:2rem auto;padding:0 1rem;line-height:1.5}}\
         table{{border-collapse:collapse}}td,th{{padding:.25rem .75rem;border-bottom:1px solid #ccc;text-align:left}}</style>\n\
         </head>\n<body>\n{body}</body>\n</html>\n",
        html_escape(title)
    )
}

/// The master URL page. The client finds the scheduler through the
/// `<link rel="boinc_scheduler">` element, and older clients through the
/// `<scheduler>` element in a comment (`SCHEDULER_OP::parse_master_file`).
pub fn master_page(project: &Project) -> String {
    let c = &project.config;
    let sched = scheduler_url(c);
    let head = format!(
        "<link rel=\"boinc_scheduler\" href=\"{0}\">\n<!--\n<scheduler>{0}</scheduler>\n-->\n",
        html_escape(&sched)
    );
    let mut body = format!(
        "<h1>{}</h1>\n<p>This is a BOINC project. To take part, install the \
         <a href=\"https://boinc.berkeley.edu/download.php\">BOINC client</a> and attach it to \
         <code>{}</code>.</p>\n",
        html_escape(&c.long_name),
        html_escape(&c.master_url)
    );
    body.push_str("<h2>Applications</h2>\n<table>\n<tr><th>Application</th><th>Platform</th><th>Version</th><th>Plan class</th></tr>\n");
    for av in &project.catalog.app_versions {
        let app = project.catalog.apps.iter().find(|a| a.id == av.appid);
        let platform = project
            .catalog
            .platforms
            .iter()
            .find(|p| p.id == av.platformid);
        if let (Some(app), Some(platform)) = (app, platform) {
            body.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}.{:02}</td><td>{}</td></tr>\n",
                html_escape(&app.user_friendly_name),
                html_escape(&platform.user_friendly_name),
                av.version_num / 100,
                av.version_num % 100,
                html_escape(&av.plan_class)
            ));
        }
    }
    body.push_str("</table>\n<p><a href=\"server_status.php\">Server status</a></p>\n");
    page(&c.long_name, &head, &body)
}

/// Job counts by state. Each count walks a partial index or a small range.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ServerStatus {
    pub results_ready_to_send: i64,
    pub results_in_progress: i64,
    pub workunits_waiting_for_assimilation: i64,
    pub transitions_due: i64,
    pub users: i64,
    pub hosts: i64,
}

pub async fn server_status(db: &dyn Db, now: i64) -> CoreResult<ServerStatus> {
    let out = db
        .batch(vec![
            stmt!(
                "SELECT count(*) AS n FROM result WHERE server_state = ?",
                RESULT_SERVER_STATE_UNSENT
            ),
            stmt!(
                "SELECT count(*) AS n FROM result WHERE server_state = ? AND hostid > 0",
                RESULT_SERVER_STATE_IN_PROGRESS
            ),
            stmt!(
                "SELECT count(*) AS n FROM workunit WHERE assimilate_state = ?",
                ASSIMILATE_READY
            ),
            stmt!(
                "SELECT count(*) AS n FROM workunit WHERE transition_time < ?",
                now
            ),
            Stmt::new("SELECT coalesce(max(id), 0) AS n FROM user"),
            Stmt::new("SELECT coalesce(max(id), 0) AS n FROM host"),
        ])
        .await?;
    let n = |i: usize| out.get(i).and_then(|r| r.scalar_i64()).unwrap_or(0);
    Ok(ServerStatus {
        results_ready_to_send: n(0),
        results_in_progress: n(1),
        workunits_waiting_for_assimilation: n(2),
        transitions_due: n(3),
        users: n(4),
        hosts: n(5),
    })
}

/// `server_status.php?xml=1`, in the element names of the upstream page.
pub fn server_status_xml(project: &Project, s: &ServerStatus, now: i64) -> String {
    format!(
        "{XML_HEADER}<server_status>\n<update_time>{now}</update_time>\n<software_version>cloud-boinc {}</software_version>\n\
         <daemon_status>\n<daemon><host>cloudflare</host><command>scheduler</command><status>{}</status></daemon>\n\
         <daemon><host>cloudflare</host><command>pipeline</command><status>running</status></daemon>\n</daemon_status>\n\
         <database_file_states>\n<results_ready_to_send>{}</results_ready_to_send>\n\
         <results_in_progress>{}</results_in_progress>\n\
         <workunits_waiting_for_assimilation>{}</workunits_waiting_for_assimilation>\n\
         <transitioner_backlog_hours>{}</transitioner_backlog_hours>\n\
         <users_with_recent_credit>{}</users_with_recent_credit>\n<hosts_with_recent_credit>{}</hosts_with_recent_credit>\n\
         </database_file_states>\n</server_status>\n",
        env!("CARGO_PKG_VERSION"),
        if project.config.ended { "disabled" } else { "running" },
        s.results_ready_to_send,
        s.results_in_progress,
        s.workunits_waiting_for_assimilation,
        if s.transitions_due > 0 { "0.02" } else { "0" },
        s.users,
        s.hosts,
    )
}

pub fn server_status_page(project: &Project, s: &ServerStatus) -> String {
    let rows = [
        ("Tasks ready to send", s.results_ready_to_send),
        ("Tasks in progress", s.results_in_progress),
        (
            "Workunits waiting for assimilation",
            s.workunits_waiting_for_assimilation,
        ),
        ("Transitions due", s.transitions_due),
        ("Accounts", s.users),
        ("Computers", s.hosts),
    ];
    let mut body = format!(
        "<h1>{} server status</h1>\n<table>\n",
        html_escape(&project.config.long_name)
    );
    for (label, n) in rows {
        body.push_str(&format!("<tr><td>{label}</td><td>{n}</td></tr>\n"));
    }
    body.push_str("</table>\n<p><a href=\"./\">Home</a></p>\n");
    page("Server status", "", &body)
}

/// The weak account key the site shows a signed-in user, for use in
/// `account_*.xml` files on shared machines.
pub fn weak_account_key(user: &User) -> String {
    weak_auth(user.id as u64, &user.authenticator, &user.passwd_hash)
}

/// Maps a core failure of an account RPC to the upstream database error.
pub fn account_rpc_failure(_e: &CoreError) -> String {
    AccountError::DbError.reply()
}
