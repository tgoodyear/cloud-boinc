//! The web RPCs a client uses to attach, the master page and the server
//! status. Replies have the shapes of upstream `html/user/*.php`.

use cb_crypto::account::{
    pepper_passwd_hash, random_hex_string, user_cpid, verify_peppered_passwd_hash, weak_auth,
};
use cb_db::{query_one, stmt, Db, Stmt};
use cb_proto::consts::*;
use cb_proto::web_rpc::{
    err, write_create_account, write_error, write_lookup_account, write_lookup_account_exists,
    AccountError, ConfigPlatform, ProjectConfig, XML_HEADER, XML_HEADER_UTF8,
};
use cb_proto::xml::{xml_escape, xml_escape_text};
use serde::Serialize;

use crate::model::{Host, User};
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

/// PHP `urlencode`: letters, digits and `-_.` kept, space as `+`, every
/// other byte as `%XX`.
pub fn php_urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `am_get_info.php`: an account manager reads the account a key belongs
/// to. Only the account key itself is accepted, not the weak key, as
/// upstream (`BoincUser::lookup_auth`). One statement.
///
/// Profiles and teams are not offered, so `has_profile` and `teamid` are 0
/// and there is no `<teamfounder/>`; `postal_code` is left out as upstream
/// does by default.
pub async fn am_get_info(db: &dyn Db, account_key: &str) -> CoreResult<String> {
    let Some(user) = crate::site::user_by_auth(db, account_key.trim()).await? else {
        return Ok(write_error(err::ERR_DB_NOT_FOUND, None));
    };
    Ok(format!(
        "{XML_HEADER_UTF8}<am_get_info_reply>\n    <success/>\n    <id>{}</id>\n<name>{}</name>\n\
         <country>{}</country>\n<weak_auth>{}</weak_auth>\n<cpid>{}</cpid>\n\
         <has_profile>0</has_profile>\n<create_time>{}</create_time>\n\
         <global_prefs>\n{}\n</global_prefs>\n<project_prefs>\n{}\n</project_prefs>\n\
         <url>{}</url>\n<send_email>{}</send_email>\n<show_hosts>{}</show_hosts>\n\
         <teamid>{}</teamid>\n<venue>{}</venue>\n</am_get_info_reply>\n",
        user.id,
        php_urlencode(&user.name),
        php_urlencode(&user.country),
        weak_account_key(&user),
        user_cpid(&user.cross_project_id, &user.email_addr),
        user.create_time,
        user.global_prefs,
        user.project_prefs,
        php_urlencode(&user.url),
        user.send_email,
        user.show_hosts,
        user.teamid,
        xml_escape_text(&user.venue),
    ))
}

/// `am_set_info.php`. Upstream switched this RPC off ("This probably
/// shouldn't exist") and answers every request with this error.
pub fn am_set_info() -> String {
    write_error(-1, Some("not supported"))
}

/// Longest venue `am_set_host_info.php` stores.
const MAX_VENUE: usize = 64;

/// `am_set_host_info.php`: sets the venue of one of the account's
/// computers. An unknown key and a computer of another account both get
/// `ERR_DB_NOT_FOUND`, as upstream. One statement: the update matches only
/// when the key and the computer agree.
pub async fn am_set_host_info(
    db: &dyn Db,
    account_key: &str,
    hostid: &str,
    venue: &str,
) -> CoreResult<String> {
    let account_key = account_key.trim();
    let Ok(hostid) = hostid.trim().parse::<i64>() else {
        return Ok(write_error(err::ERR_DB_NOT_FOUND, None));
    };
    if !crate::site::plausible_account_key(account_key) || hostid <= 0 {
        return Ok(write_error(err::ERR_DB_NOT_FOUND, None));
    }
    if venue.len() > MAX_VENUE
        || venue
            .chars()
            .any(|c| c.is_control() || c == '<' || c == '>' || c == '&')
    {
        return Ok(write_error(-1, Some("invalid venue")));
    }
    let updated = db
        .run(stmt!(
            "UPDATE host SET venue = ? WHERE id = ? AND userid = \
             (SELECT id FROM user WHERE authenticator = ? AND blocked = 0) RETURNING id",
            venue,
            hostid,
            account_key
        ))
        .await?;
    if updated.rows.is_empty() {
        return Ok(write_error(err::ERR_DB_NOT_FOUND, None));
    }
    Ok(format!(
        "{XML_HEADER}<am_set_host_info_reply>\n    <success/>\n</am_set_host_info_reply>\n"
    ))
}

/// `show_host_xml` in `html/inc/xml.inc`. Strings are escaped with
/// [`xml_escape_text`]; the reply is UTF-8.
fn host_xml(out: &mut String, h: &Host) {
    out.push_str(&format!(
        "    <host>\n        <id>{}</id>\n        <create_time>{}</create_time>\n        \
         <rpc_seqno>{}</rpc_seqno>\n        <rpc_time>{}</rpc_time>\n        \
         <host_cpid>{}</host_cpid>\n        <total_credit>{}</total_credit>\n        \
         <expavg_credit>{}</expavg_credit>\n        <expavg_time>{}</expavg_time>\n        \
         <domain_name>{}</domain_name>\n        <p_ncpus>{}</p_ncpus>\n        \
         <p_vendor>{}</p_vendor>\n        <p_model>{}</p_model>\n        <p_fpops>{}</p_fpops>\n        \
         <p_iops>{}</p_iops>\n        <os_name>{}</os_name>\n        <os_version>{}</os_version>\n        \
         <m_nbytes>{}</m_nbytes>\n        <d_free>{}</d_free>\n        <d_total>{}</d_total>\n        \
         <venue>{}</venue>\n    </host>\n",
        h.id,
        h.create_time,
        h.rpc_seqno,
        h.rpc_time,
        xml_escape_text(&h.host_cpid),
        h.total_credit,
        h.expavg_credit,
        h.expavg_time,
        xml_escape_text(&h.domain_name),
        h.p_ncpus,
        xml_escape_text(&h.p_vendor),
        xml_escape_text(&h.p_model),
        h.p_fpops,
        h.p_iops,
        xml_escape_text(&h.os_name),
        xml_escape_text(&h.os_version),
        h.m_nbytes,
        h.d_free,
        h.d_total,
        xml_escape_text(&h.venue),
    ));
}

/// `show_user.php?format=xml` (`show_user_xml` in `html/inc/xml.inc`).
/// `hosts` is given when the request carried the account key. The reply is
/// UTF-8 and says so, unlike the other web RPC replies.
pub fn show_user_xml(config: &Config, user: &User, hosts: Option<&[Host]>) -> String {
    let mut out = format!(
        "{XML_HEADER_UTF8}<user>\n    <id>{}</id>\n    <cpid>{}</cpid>\n    <create_time>{}</create_time>\n    \
         <name>{}</name>\n    <total_credit>{}</total_credit>\n    <expavg_credit>{}</expavg_credit>\n    \
         <expavg_time>{}</expavg_time>\n    <teamid>{}</teamid>\n    <has_profile>0</has_profile>\n",
        user.id,
        user_cpid(&user.cross_project_id, &user.email_addr),
        user.create_time,
        xml_escape_text(&user.name),
        user.total_credit,
        user.expavg_credit,
        user.expavg_time,
        user.teamid,
    );
    if let Some(hosts) = hosts {
        out.push_str(&format!(
            "   <venue>{}</venue>\n",
            xml_escape_text(&user.venue)
        ));
        for h in hosts {
            host_xml(&mut out, h);
        }
    }
    if config.user_country {
        out.push_str(&format!(
            "     <country>{}</country>\n",
            xml_escape_text(&user.country)
        ));
    }
    if config.user_url && !user.url.is_empty() {
        out.push_str(&format!("    <url>{}</url>\n", xml_escape_text(&user.url)));
    }
    out.push_str("</user>\n");
    out
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
            name: xml_escape(&p.name),
            user_friendly_name: xml_escape(&p.user_friendly_name),
            plan_class: xml_escape(&av.plan_class),
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

/// Escapes text for an HTML element or a double- or single-quoted attribute.
pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub(crate) fn page(title: &str, head: &str, body: &str) -> String {
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
    body.push_str(
        "</table>\n<p><a href=\"home.php\">Your account</a> &middot; \
         <a href=\"server_status.php\">Server status</a></p>\n",
    );
    page(&c.long_name, &head, &body)
}

/// Job counts by state, plus account and computer counts from a snapshot at
/// most `STATUS_COUNTS_TTL` seconds old.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ServerStatus {
    pub results_ready_to_send: i64,
    pub results_in_progress: i64,
    pub workunits_waiting_for_assimilation: i64,
    pub transitions_due: i64,
    pub users: i64,
    pub hosts: i64,
    /// Accounts and computers with `expavg_credit` over 1, as upstream
    /// counts "recent credit".
    pub users_with_recent_credit: i64,
    pub hosts_with_recent_credit: i64,
}

/// `config` row holding the account and computer counts. `Project::load`
/// ignores names it does not know, so the row is safe beside the settings.
const STATUS_COUNTS_SETTING: &str = "status_counts";

/// Seconds a stored count snapshot is reused. The counts scan the whole
/// `user` and `host` tables, so a public poll must not run them each time.
pub const STATUS_COUNTS_TTL: i64 = 600;

#[derive(Serialize, serde::Deserialize)]
struct StatusCounts {
    at: i64,
    users: i64,
    users_recent: i64,
    hosts: i64,
    hosts_recent: i64,
}

#[derive(serde::Deserialize)]
struct CountsRow {
    value: String,
}

async fn refresh_status_counts(db: &dyn Db, now: i64) -> CoreResult<StatusCounts> {
    let out = db
        .batch(vec![
            Stmt::new(
                "SELECT count(*) AS n, coalesce(sum(expavg_credit > 1), 0) AS recent FROM user",
            ),
            Stmt::new(
                "SELECT count(*) AS n, coalesce(sum(expavg_credit > 1), 0) AS recent FROM host",
            ),
        ])
        .await?;
    let get = |i: usize, col: &str| {
        out.get(i)
            .and_then(|r| r.rows.first())
            .and_then(|row| row.get(col))
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
    };
    let counts = StatusCounts {
        at: now,
        users: get(0, "n"),
        users_recent: get(0, "recent"),
        hosts: get(1, "n"),
        hosts_recent: get(1, "recent"),
    };
    let json = serde_json::to_string(&counts).map_err(|e| CoreError::Invalid(e.to_string()))?;
    db.run(stmt!(
        "INSERT INTO config (name, value, mod_time) VALUES (?, ?, ?) \
         ON CONFLICT (name) DO UPDATE SET value = excluded.value, mod_time = excluded.mod_time",
        STATUS_COUNTS_SETTING,
        json,
        now
    ))
    .await?;
    Ok(counts)
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
                "SELECT count(*) AS n FROM workunit WHERE transition_time < ? AND transition_time < 2147483647",
                now
            ),
            stmt!(
                "SELECT value FROM config WHERE name = ?",
                STATUS_COUNTS_SETTING
            ),
        ])
        .await?;
    let n = |i: usize| out.get(i).and_then(|r| r.scalar_i64()).unwrap_or(0);
    let cached = match out.get(4).cloned() {
        Some(rows) => rows
            .first::<CountsRow>()?
            .and_then(|r| serde_json::from_str::<StatusCounts>(&r.value).ok()),
        None => None,
    };
    let counts = match cached {
        Some(c) if c.at <= now && now - c.at < STATUS_COUNTS_TTL => c,
        _ => refresh_status_counts(db, now).await?,
    };
    Ok(ServerStatus {
        results_ready_to_send: n(0),
        results_in_progress: n(1),
        workunits_waiting_for_assimilation: n(2),
        transitions_due: n(3),
        users: counts.users,
        hosts: counts.hosts,
        users_with_recent_credit: counts.users_recent,
        hosts_with_recent_credit: counts.hosts_recent,
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
        s.users_with_recent_credit,
        s.hosts_with_recent_credit,
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
        ("Accounts with recent credit", s.users_with_recent_credit),
        ("Computers", s.hosts),
        ("Computers with recent credit", s.hosts_with_recent_credit),
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
