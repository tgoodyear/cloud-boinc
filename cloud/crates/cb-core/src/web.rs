//! The web RPCs a client uses to attach, the master page and the server
//! status. Replies have the shapes of upstream `html/user/*.php`.

use cb_crypto::account::{
    pepper_passwd_hash, random_hex_string, user_cpid, verify_peppered_passwd_hash, weak_auth,
};
use cb_db::{query_one, stmt, Db, Stmt};
use cb_proto::web_rpc::{
    err, write_create_account, write_error, write_lookup_account, write_lookup_account_exists,
    AccountError, ConfigPlatform, ProjectConfig, XML_HEADER, XML_HEADER_UTF8,
};
use cb_proto::xml::{xml_escape, xml_escape_text};

use crate::model::{Host, User};
use crate::project::{scheduler_url, Project};
use crate::site_ui::{self, site_header};
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

/// A tighter test than [`is_valid_email`], for addresses the site is given
/// from now on (sign-up and address changes) and the ones it mails: a local
/// part of at most 64 characters from letters, digits and `._+-` that does not
/// start with `-` or `.`, end with `.` or have `..`; a domain of at least two
/// non-empty labels of at most 63 letters, digits and hyphens that neither
/// start nor end with a hyphen, whose last label has a letter (so no IP-like
/// domain); at most 254 characters in all. Accounts made under the looser rule
/// keep working; they just get no mail.
pub fn is_strict_email(addr: &str) -> bool {
    if addr.len() > MAX_EMAIL {
        return false;
    }
    let Some((local, domain)) = addr.split_once('@') else {
        return false;
    };
    let local_ok = !local.is_empty()
        && local.len() <= 64
        && local
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c))
        && !local.starts_with(['-', '.'])
        && !local.ends_with('.')
        && !local.contains("..");
    let labels: Vec<&str> = domain.split('.').collect();
    local_ok
        && labels.len() >= 2
        && labels.iter().all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                && !l.starts_with('-')
                && !l.ends_with('-')
        })
        && labels
            .last()
            .is_some_and(|l| l.chars().any(|c| c.is_ascii_alphabetic()))
}

/// How long the address an account has just changed away from stays reserved
/// for it, and the wait before its next change: seven days, as upstream's
/// `edit_email_action.php`.
pub const EMAIL_CHANGE_WAIT_SECS: i64 = 604800;

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

/// Whether `s` looks like the client's `md5(password + email)`.
pub(crate) fn is_passwd_hash(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Longest postal code stored.
const MAX_POSTAL_CODE: usize = 64;
/// Longest URL stored for an account.
const MAX_URL: usize = 254;

/// A postal code: short, printable, no markup (`sanitize_tags` upstream
/// strips tags; here they are refused).
pub fn is_valid_postal_code(s: &str) -> bool {
    s.len() <= MAX_POSTAL_CODE
        && !s
            .chars()
            .any(|c| c.is_control() || c == '<' || c == '>' || c == '&')
}

/// `sanitize_user_url` (`html/inc/user_util.inc`): an optional `http://` or
/// `https://`, then only letters, digits and `-_./#?&=`. Empty is allowed.
pub fn is_valid_user_url(url: &str) -> bool {
    if url.len() > MAX_URL {
        return false;
    }
    let parts: Vec<&str> = url.split("//").collect();
    let rest = match parts.as_slice() {
        [only] => *only,
        [scheme, rest] => {
            let scheme = scheme.to_ascii_lowercase();
            if scheme != "http:" && scheme != "https:" {
                return false;
            }
            *rest
        }
        _ => return false,
    };
    rest.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-_./#?&=".contains(&b))
}

/// Longest venue name stored, for an account or a computer.
const MAX_VENUE: usize = 64;

/// A venue name an account manager may set. The site offers only `home`,
/// `school` and `work` (and none), as upstream's `check_venue`.
pub fn is_valid_venue(venue: &str) -> bool {
    venue.len() <= MAX_VENUE
        && !venue
            .chars()
            .any(|c| c.is_control() || c == '<' || c == '>' || c == '&' || c == '"')
}

/// Why `create_account.php` refuses, if it does. As upstream's
/// `html/user/create_account.php`: `disable_account_creation` (here also an
/// unstated option, see [`Config::account_creation`]) says the project takes
/// no new accounts; `disable_account_creation_rpc` says to use the web site,
/// or, with the web site closed too, that the project takes none.
pub fn account_creation_refusal(config: &Config) -> Option<AccountError> {
    if !config.account_creation {
        Some(AccountError::CreationDisabled)
    } else if config.disable_account_creation_rpc {
        Some(if config.disable_web_account_creation {
            AccountError::CreationDisabled
        } else {
            AccountError::CreationOnWebSite
        })
    } else {
        None
    }
}

/// Whether the site's sign-up page is closed: all account creation is, or
/// `no_web_account_creation` says so (`create_account_form.php`,
/// `validate_post_make_user`).
pub fn web_account_creation_closed(config: &Config) -> bool {
    !config.account_creation || config.disable_web_account_creation
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
    /// One of [`crate::countries::COUNTRIES`], or empty.
    pub country: &'a str,
    pub postal_code: &'a str,
    /// `show_hosts` of the new account: the callers pass
    /// [`Config::default_show_hosts`] (upstream's `default_show_hosts`).
    pub show_hosts: bool,
}

/// An SQL expression for the ID of a new row of `table` (`user` or `host`): the
/// largest ID the table has or has had (`id_floor`, kept by triggers on delete)
/// plus one, so that an ID is never given out twice. It is meant for the
/// `SELECT` of one `INSERT`, which runs as one statement, so two inserts cannot
/// get the same ID. Every insert into those two tables uses it: `make_user`
/// here and the scheduler's computer insert in `scheduler.rs`.
pub fn next_id_sql(table: &str) -> String {
    format!(
        "(SELECT max(coalesce((SELECT max(id) FROM {table}), 0), \
         coalesce((SELECT last_id FROM id_floor WHERE name = '{table}'), 0)) + 1)"
    )
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
    if !crate::countries::is_valid_country(account.country)
        || !is_valid_postal_code(account.postal_code)
    {
        // The site checks both before it gets here; the RPC sends neither.
        return Err(CoreError::Invalid("bad country or postal code".into()));
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
    // Not an address another account changed away from within the week: that
    // account's undo link has to find it free.
    let sql = format!(
        "INSERT INTO user (id, create_time, email_addr, name, authenticator, cross_project_id, \
         passwd_hash, expavg_time, country, postal_code, show_hosts) \
         SELECT {}, ?, ?, ?, ?, ?, ?, ?, ?, ?, ? \
         WHERE NOT EXISTS (SELECT 1 FROM user WHERE previous_email_addr = ? \
         AND previous_email_addr != '' AND email_addr_change_time > ?) \
         ON CONFLICT (email_addr) DO NOTHING RETURNING id",
        next_id_sql("user")
    );
    let inserted = db
        .run(stmt!(
            sql,
            now,
            email.as_str(),
            name.as_str(),
            authenticator.as_str(),
            cross_project_id.as_str(),
            stored.as_str(),
            now,
            account.country,
            account.postal_code,
            account.show_hosts,
            email.as_str(),
            now - EMAIL_CHANGE_WAIT_SECS
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
    if let Some(refusal) = account_creation_refusal(config) {
        return Ok(refusal.reply());
    }
    if account.passwd_hash.is_empty() {
        return Ok(AccountError::BadPasswdHashLength.reply());
    }
    let account = NewAccount {
        show_hosts: config.default_show_hosts,
        ..account.clone()
    };
    Ok(match make_user(db, pepper, now, random, &account).await? {
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

/// Largest form body read, for `am_set_info.php`, which can carry both
/// preference documents (up to [`crate::prefs::MAX_PREFS_BYTES`] each) with
/// every byte URL-encoded to three, plus the account key, name, URL and the
/// other fields. A longer body is not from a BOINC client; its fields are
/// ignored.
pub const MAX_FORM_BYTES: usize = 2 * 3 * crate::prefs::MAX_PREFS_BYTES + 64 * 1024;

/// `am_set_info.php`: an account manager changes an account. Upstream
/// switched this RPC off ("This probably shouldn't exist") but its
/// code is still there; this follows that code. Parameters come from the
/// query or a POST form, as upstream reads either.
///
/// Each of `name`, `country`, `postal_code`, `global_prefs`,
/// `project_prefs`, `url` and `venue` is changed when given and not empty;
/// `send_email` and `show_hosts` when given and not empty; `email_addr` when
/// it differs from the current address; `password_hash` (the client's
/// `md5(password + email)`) when given. Values are checked before anything is
/// written, and a refused value changes nothing. `teamid` is accepted and
/// ignored, since there are no teams; the consent fields are ignored, since
/// there is no consent table. As upstream, a new email address without a new
/// `password_hash` leaves the stored hash, which was made with the old
/// address, so the password stops working until it is set again.
///
/// Two statements: the account by key, then one `UPDATE` whose `WHERE`
/// clause refuses an email address another account has. The stored hash
/// depends on the address, so a write of either (or a request that names the
/// address) is also conditional on the address and hash read with the
/// account, and project preferences that inherit the stored
/// `<project_specific>` block are conditional on `project_prefs` being still
/// what was read; when the account changed meanwhile, nothing is written
/// and the reply is `ERR_RETRY`; a refused write costs one more statement, to tell
/// that from a taken address.
pub async fn am_set_info(
    db: &dyn Db,
    config: &Config,
    pepper: &[u8],
    now: i64,
    q: &crate::site::Params,
) -> CoreResult<String> {
    let get = |k: &str| q.get(k).map(String::as_str).unwrap_or_default();
    let Some(user) = crate::site::user_by_auth(db, get("account_key").trim()).await? else {
        return Ok(write_error(err::ERR_DB_NOT_FOUND, None));
    };
    let mut update = Stmt::new("");
    let mut sets: Vec<&str> = Vec::new();
    let mut set = |col: &'static str, v: cb_db::Value| {
        sets.push(col);
        update.params.push(v);
    };

    let name = get("name");
    if !name.is_empty() {
        if !is_valid_user_name(name) {
            return Ok(write_error(-1, Some("invalid name")));
        }
        set("name", name.into());
    }
    let country = if config.user_country {
        get("country")
    } else {
        ""
    };
    if !crate::countries::is_valid_country(country) {
        return Ok(write_error(-1, Some("invalid country")));
    }
    if !country.is_empty() {
        set("country", country.into());
    }
    let postal_code = get("postal_code");
    if !postal_code.is_empty() {
        if !is_valid_postal_code(postal_code) {
            return Ok(write_error(-1, Some("invalid postal code")));
        }
        set("postal_code", postal_code.into());
    }
    let global_prefs = get("global_prefs").replace("\r\n", "\n");
    if !global_prefs.is_empty() {
        if let Some(why) = crate::prefs::bad_xml(&global_prefs, "global_preferences") {
            return Ok(write_error(
                err::ERR_XML_PARSE,
                Some(&format!("Invalid global preferences: {why}")),
            ));
        }
        if global_prefs.len() > crate::prefs::MAX_PREFS_BYTES {
            return Ok(write_error(err::ERR_BUFFER_OVERFLOW, None));
        }
        if let Some(why) = crate::prefs::mod_time_error(&global_prefs, now) {
            return Ok(write_error(
                err::ERR_XML_PARSE,
                Some(&format!("Invalid global preferences: {why}")),
            ));
        }
        set("global_prefs", global_prefs.into());
    }
    let mut keeps_stored_prefs = false;
    let project_prefs = get("project_prefs").replace("\r\n", "\n");
    if !project_prefs.is_empty() {
        if let Some(why) = crate::prefs::bad_xml(&project_prefs, "project_preferences") {
            return Ok(write_error(
                err::ERR_XML_PARSE,
                Some(&format!("Invalid project preferences: {why}")),
            ));
        }
        let merged = crate::prefs::keep_project_specific(&project_prefs, &user.project_prefs);
        // A document without its own `<project_specific>` replaces whatever
        // the account has there now: the block read above if there was one
        // (it is kept), or none (a site save that adds the first task limit
        // or application selection since would be lost). Either way the write
        // is conditional on `project_prefs` being still what was read.
        keeps_stored_prefs = !project_prefs
            .to_ascii_lowercase()
            .contains("<project_specific>");
        let project_prefs = merged;
        if project_prefs.len() > crate::prefs::MAX_PREFS_BYTES {
            return Ok(write_error(err::ERR_BUFFER_OVERFLOW, None));
        }
        set("project_prefs", project_prefs.into());
    }
    let url = if config.user_url { get("url") } else { "" };
    if !url.is_empty() {
        if !is_valid_user_url(url) {
            return Ok(write_error(-1, Some("invalid URL")));
        }
        set("url", url.into());
    }
    for col in ["send_email", "show_hosts"] {
        let v = get(col).trim();
        if !v.is_empty() {
            set(col, i32::from(v != "0").into());
        }
    }
    let venue = get("venue");
    if !venue.is_empty() {
        if !is_valid_venue(venue) {
            return Ok(write_error(-1, Some("invalid venue")));
        }
        set("venue", venue.into());
    }
    let email = get("email_addr").trim().to_lowercase();
    let new_email = !email.is_empty() && email != user.email_addr;
    if new_email {
        if !is_valid_email(&email) {
            return Ok(write_error(
                AccountError::BadEmailAddr.num(),
                Some("Invalid email address"),
            ));
        }
        // The week's wait of the site's change applies here too: this call
        // needs only the account key, and must not get round the window in
        // which the previous address can undo a change.
        if user.email_addr_change_time + EMAIL_CHANGE_WAIT_SECS > now {
            return Ok(write_error(
                -1,
                Some("The email address was changed within the past 7 days"),
            ));
        }
        set("email_addr", email.as_str().into());
        set("email_validated", 0.into());
        // Recorded as the site's change records it, so the old address stays
        // reserved; no undo mail goes from here (there is no mailer in this
        // call), so no undo token is made.
        set("previous_email_addr", user.email_addr.as_str().into());
        set("email_addr_change_time", now.into());
    }
    let password_hash = get("password_hash").trim().to_lowercase();
    if !password_hash.is_empty() {
        if !is_passwd_hash(&password_hash) {
            return Ok(AccountError::BadPasswdHashLength.reply());
        }
        set(
            "passwd_hash",
            pepper_passwd_hash(pepper, &password_hash).into(),
        );
    }

    let credentials_changed = new_email || !password_hash.is_empty();
    let new_stored_hash = if password_hash.is_empty() {
        String::new()
    } else {
        pepper_passwd_hash(pepper, &password_hash)
    };
    if !sets.is_empty() {
        let assignments: Vec<String> = sets.iter().map(|c| format!("{c} = ?")).collect();
        let target = if new_email {
            email.as_str()
        } else {
            user.email_addr.as_str()
        };
        // `password_hash` was made for an address, and the address and the
        // hash are what this RPC compared with. If a site request changed
        // either since, the pair written would not match.
        let credentials = !password_hash.is_empty() || !email.is_empty();
        // The account key too, every time: a request that authenticated before
        // `reset_account_key.php` replaced it must not write afterwards.
        let mut guard = String::from(" AND authenticator = ?");
        let mut observed: Vec<&str> = vec![user.authenticator.as_str()];
        if credentials {
            guard.push_str(" AND email_addr = ? AND passwd_hash = ?");
            observed.extend([user.email_addr.as_str(), user.passwd_hash.as_str()]);
        }
        if keeps_stored_prefs {
            guard.push_str(" AND project_prefs IS ?");
            observed.push(user.project_prefs.as_str());
        }
        update.sql = format!(
            "UPDATE user SET {} WHERE id = ?{guard} AND NOT EXISTS \
             (SELECT 1 FROM user WHERE email_addr = ? AND id != ?) AND NOT EXISTS \
             (SELECT 1 FROM user WHERE previous_email_addr = ? AND previous_email_addr != '' \
             AND email_addr_change_time > ? AND id != ?) RETURNING id",
            assignments.join(", ")
        );
        update = update.bind(user.id);
        for v in observed {
            update = update.bind(v);
        }
        update = update
            .bind(target)
            .bind(user.id)
            .bind(target)
            .bind(now - EMAIL_CHANGE_WAIT_SECS)
            .bind(user.id);
        // The update and, for a change of credentials, the end of the login
        // links mailed before it (and of undo links, for an address) go in
        // one batch. The second statement runs only if the update just before
        // it changed a row (`changes()`, which is for the statement before),
        // not merely because the account looks like the target: a newer
        // change by another request can make it look so while this update was
        // refused, and its tokens must stay.
        let mut batch = vec![update];
        if credentials_changed {
            batch.push(stmt!(
                "UPDATE token SET used_time = ? WHERE userid = ? AND type IN ('L', ?) \
                 AND used_time = 0 AND changes() > 0 \
                 AND EXISTS (SELECT 1 FROM user WHERE id = ? AND email_addr = ? \
                 AND authenticator = ? AND (? = '' OR passwd_hash = ?))",
                now,
                user.id,
                if new_email { "E" } else { "L" },
                user.id,
                target,
                user.authenticator.as_str(),
                new_stored_hash.as_str(),
                new_stored_hash.as_str()
            ));
        }
        if !db
            .batch(batch)
            .await?
            .first()
            .is_some_and(|r| !r.rows.is_empty())
        {
            // Refused: the address is taken, or the account changed since it
            // was read. Only this path asks which.
            let taken = db
                .run(stmt!(
                    "SELECT (SELECT count(*) FROM user WHERE email_addr = ? AND id != ?) + \
                     (SELECT count(*) FROM user WHERE previous_email_addr = ? \
                     AND previous_email_addr != '' AND email_addr_change_time > ? \
                     AND id != ?) AS n",
                    target,
                    user.id,
                    target,
                    now - EMAIL_CHANGE_WAIT_SECS,
                    user.id
                ))
                .await?
                .scalar_i64()
                .unwrap_or(0);
            if taken > 0 {
                return Ok(write_error(
                    err::ERR_DB_NOT_UNIQUE,
                    Some("There's already an account with that email address."),
                ));
            }
            return Ok(write_error(
                err::ERR_RETRY,
                Some("The account changed meanwhile; please try again."),
            ));
        }
    }
    Ok(format!(
        "{XML_HEADER}<am_set_info_reply>\n    <success/>\n    \n</am_set_info_reply>\n"
    ))
}

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
    if !is_valid_venue(venue) {
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

/// The stylesheet of every page, inline in [`page`]: all pages come from this
/// one shell, so there is no second request and no route to keep in step
/// with the URL layout. The site's `Content-Security-Policy` only sets
/// `frame-ancestors`, so inline styles are allowed.
pub const SITE_CSS: &str = include_str!("site.css");

/// `Cache-Control` value of an HTML page built from `base`: the page lifetime
/// as it was, plus `no-transform`, so that a CDN or proxy in front does not
/// rewrite the page (the master URL page carries the `boinc_scheduler` link
/// clients read, and the site pages carry forms).
pub fn html_cache_control(base: &str) -> String {
    format!("{base}, no-transform")
}

/// The icon of every page: a small SVG, used by the pages as a data URL and
/// served at `/favicon.ico` for the clients that ask for that path.
pub const FAVICON_SVG: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 32 32\">\
<rect width=\"32\" height=\"32\" rx=\"7\" fill=\"#0b5cad\"/>\
<circle cx=\"16\" cy=\"16\" r=\"6\" fill=\"none\" stroke=\"#fff\" stroke-width=\"3\"/></svg>";

/// [`FAVICON_SVG`] as the `href` of a `<link rel="icon">`.
fn favicon_href() -> String {
    let mut out = String::from("data:image/svg+xml,");
    for c in FAVICON_SVG.chars() {
        match c {
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            '#' => out.push_str("%23"),
            '"' => out.push('\''),
            c => out.push(c),
        }
    }
    out
}

/// The HTML shell. A body that starts with the site header (see
/// `site_ui::site_header`) has the rest put in `<main>`. Tables are wrapped
/// in a `scroll` box, so that a wide table scrolls sideways inside its own
/// container and not the page. Page text is escaped, so `<table>` in a body
/// is always a real table.
pub(crate) fn page(title: &str, head: &str, body: &str) -> String {
    let body = body
        .replace("<table>", "<div class=\"scroll\"><table>")
        .replace(
            "<table class=\"stack\">",
            "<div class=\"scroll\"><table class=\"stack\">",
        )
        .replace("</table>", "</table></div>");
    let (header, main) = match body.strip_prefix("<header class=\"top\">") {
        Some(rest) => {
            let (inner, after) = rest.split_once("</header>\n").unwrap_or((rest, ""));
            (
                format!("<header class=\"top\">{inner}</header>\n"),
                after.to_string(),
            )
        }
        None => (String::new(), body.clone()),
    };
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{}</title>\n{head}<link rel=\"icon\" href=\"{}\">\n\
         <style>{SITE_CSS}</style>\n\
         </head>\n<body>\n{header}<main>\n{main}</main>\n</body>\n</html>\n",
        html_escape(title),
        favicon_href()
    )
}

/// The "Right now" card of the home page: what the project is doing, from the
/// overview snapshot, with links to the pages behind each figure.
fn summary_card(o: &crate::stats::Overview, now: i64) -> String {
    let n = |v: i64| crate::site::number_format(v as f64, 0);
    let total = o.apps.iter().find(|a| a.id == 0);
    let mut items = Vec::new();
    if let Some(t) = total {
        let plus = |v: i64, exact: bool| if exact { n(v) } else { format!("{}+", n(v)) };
        let waiting = plus(t.unsent, !t.unsent_capped);
        items.push(site_ui::stat("Waiting to send", &waiting, ""));
        items.push(site_ui::stat("In progress", &n(t.in_progress), ""));
        items.push(site_ui::stat(
            "Completed, 24 hours",
            &plus(t.ok_24h, o.day_complete()),
            "",
        ));
    }
    items.push(site_ui::stat("Volunteers working", &n(o.working_users), ""));
    items.push(site_ui::stat("Computers working", &n(o.working_hosts), ""));
    // How old the counts are, so that a pipeline that stopped is not "right now".
    let age = (now - o.at).max(0);
    let badge = if age <= 20 * 60 {
        site_ui::badge(
            &format!("counted {}", crate::feed::age_text(age)),
            site_ui::Tone::Good,
        )
    } else {
        site_ui::badge(
            &format!("stale, counted {}", crate::feed::age_text(age)),
            site_ui::Tone::Warning,
        )
    };
    let body = format!("<p>{badge}</p>\n{}", site_ui::stats(&items));
    site_ui::card("Right now", &body)
}

/// The master URL page. The client finds the scheduler through the
/// `<link rel="boinc_scheduler">` element, and older clients through the
/// `<scheduler>` element in a comment (`SCHEDULER_OP::parse_master_file`).
pub fn master_page(project: &Project) -> String {
    master_page_with(project, None, 0)
}

/// [`master_page`] with the headline figures of the rollup (see
/// `cb_core::stats`) in a card under the join box. The page is cached for
/// every visitor, so the figures are the project's and nothing on it depends
/// on who is looking.
pub fn master_page_with(
    project: &Project,
    summary: Option<&crate::stats::Overview>,
    now: i64,
) -> String {
    let c = &project.config;
    let sched = scheduler_url(c);
    let head = format!(
        "<link rel=\"boinc_scheduler\" href=\"{0}\">\n<!--\n<scheduler>{0}</scheduler>\n-->\n",
        html_escape(&sched)
    );
    let mut join = format!(
        "<p>This is a BOINC project. To take part, install the \
         <a href=\"https://boinc.berkeley.edu/download.php\">BOINC client</a> and attach it to \
         <code class=\"key\">{}</code></p>\n",
        html_escape(&c.master_url)
    );
    let mut buttons = Vec::new();
    if !web_account_creation_closed(c) {
        buttons.push(site_ui::button_link(
            "Create an account",
            "create_account_form.php",
            false,
        ));
    }
    buttons.push(site_ui::button_link(
        "Your account",
        "home.php",
        !buttons.is_empty(),
    ));
    join.push_str(&site_ui::actions(&buttons));
    let links: Vec<(&str, String)> = site_ui::NAV
        .iter()
        .filter(|i| !i.account)
        .map(|i| (i.label, i.path.to_string()))
        .collect();
    let mut body = format!(
        "{}<h1>{}</h1>\n",
        site_header(project),
        html_escape(&c.long_name)
    );
    body.push_str(&site_ui::card_accent("Join this project", &join));
    if let Some(o) = summary {
        body.push_str(&summary_card(o, now));
    }
    body.push_str(&site_ui::card("Explore", &site_ui::link_list(&links)));
    let mut apps = Vec::new();
    for av in &project.catalog.app_versions {
        let app = project.catalog.apps.iter().find(|a| a.id == av.appid);
        let platform = project
            .catalog
            .platforms
            .iter()
            .find(|p| p.id == av.platformid);
        if let (Some(app), Some(platform)) = (app, platform) {
            apps.push(vec![
                html_escape(&app.user_friendly_name),
                html_escape(&platform.user_friendly_name),
                format!("{}.{:02}", av.version_num / 100, av.version_num % 100),
                html_escape(&av.plan_class),
            ]);
        }
    }
    let table = if apps.is_empty() {
        site_ui::empty("No applications are available yet.")
    } else {
        site_ui::data_table(
            &[
                site_ui::Col::text("Application"),
                site_ui::Col::text("Platform"),
                site_ui::Col::num("Version"),
                site_ui::Col::text("Plan class"),
            ],
            &apps,
        )
    };
    body.push_str(&site_ui::card("Applications", &table));
    page(&c.long_name, &head, &body)
}

use crate::status::pipeline_running;
pub use crate::status::{server_status, ServerStatus, STATUS_COUNTS_TTL};

fn item_xml(out: &mut String, name: &str, value: impl std::fmt::Display) {
    out.push_str(&format!("   <{name}>{value}</{name}>\n"));
}

/// Upstream's `item_xml` writes a missing or zero value as 0.
fn hours_xml(x: f64) -> String {
    if x == 0.0 {
        "0".into()
    } else {
        format!("{x}")
    }
}

/// `server_status.php?xml=1`, in the element names of the upstream page
/// (`show_status_xml`), with `update_time` and `software_version` besides.
/// The `<daemon_status>` has the scheduler, which is the Worker answering, and
/// the pipeline, which is "running" while its cron's last sweep is recent
/// (see [`crate::status::record_sweep`]) or none was recorded yet. The
/// numbers are the snapshot of `cached_time`.
pub fn server_status_xml(project: &Project, s: &ServerStatus, now: i64) -> String {
    let mut o = format!(
        "{XML_HEADER}<server_status>\n<update_time>{now}</update_time>\n<software_version>cloud-boinc {}</software_version>\n\
         <daemon_status>\n<daemon><host>cloudflare</host><command>scheduler</command><status>{}</status></daemon>\n\
         <daemon><host>cloudflare</host><command>pipeline</command><status>{}</status></daemon>\n</daemon_status>\n\
         <database_file_states>\n",
        env!("CARGO_PKG_VERSION"),
        if project.config.ended {
            "disabled"
        } else {
            "running"
        },
        if pipeline_running(s.last_sweep, now) {
            "running"
        } else {
            "not running"
        },
    );
    item_xml(&mut o, "results_ready_to_send", s.results_ready_to_send);
    item_xml(&mut o, "results_in_progress", s.results_in_progress);
    item_xml(
        &mut o,
        "workunits_waiting_for_validation",
        s.workunits_waiting_for_validation,
    );
    item_xml(
        &mut o,
        "workunits_waiting_for_assimilation",
        s.workunits_waiting_for_assimilation,
    );
    item_xml(
        &mut o,
        "workunits_waiting_for_deletion",
        s.workunits_waiting_for_deletion,
    );
    item_xml(
        &mut o,
        "results_waiting_for_deletion",
        s.results_waiting_for_deletion,
    );
    item_xml(
        &mut o,
        "transitioner_backlog_hours",
        hours_xml(s.transitioner_backlog_hours),
    );
    item_xml(
        &mut o,
        "users_with_recent_credit",
        s.users_with_recent_credit,
    );
    item_xml(&mut o, "users_with_credit", s.users_with_credit);
    item_xml(
        &mut o,
        "users_registered_in_past_24_hours",
        s.users_past_24_hours,
    );
    item_xml(
        &mut o,
        "hosts_with_recent_credit",
        s.hosts_with_recent_credit,
    );
    item_xml(&mut o, "hosts_with_credit", s.hosts_with_credit);
    item_xml(
        &mut o,
        "hosts_registered_in_past_24_hours",
        s.hosts_past_24_hours,
    );
    item_xml(&mut o, "current_floating_point_speed", hours_xml(s.flops));
    o.push_str("<tasks_by_app>\n");
    // Upstream lists the applications that are not deprecated.
    for a in s.apps.iter().filter(|a| !a.deprecated) {
        o.push_str("<app>\n");
        item_xml(&mut o, "id", a.id);
        item_xml(&mut o, "name", xml_escape_text(&a.name));
        item_xml(&mut o, "unsent", a.unsent);
        item_xml(&mut o, "in_progress", a.in_progress);
        if let (Some(avg), Some(min), Some(max)) = (a.avg_hours, a.min_hours, a.max_hours) {
            item_xml(&mut o, "avg_runtime", hours_xml(avg));
            item_xml(&mut o, "min_runtime", hours_xml(min));
            item_xml(&mut o, "max_runtime", hours_xml(max));
            item_xml(&mut o, "users", a.users_24h);
        }
        o.push_str("</app>\n");
    }
    o.push_str("</tasks_by_app>\n</database_file_states>\n</server_status>\n");
    o
}

/// `time_diff2` of upstream for the runtimes in the table.
fn duration_hours(hours: f64) -> String {
    let secs = hours * 3600.0;
    if secs >= 86400.0 {
        format!("{:.2} days", secs / 86400.0)
    } else if secs >= 3600.0 {
        format!("{:.2} hours", secs / 3600.0)
    } else if secs >= 60.0 {
        format!("{:.2} min", secs / 60.0)
    } else {
        format!("{secs:.2} sec")
    }
}

/// `server_status.php` as a page: the counts and the table of tasks by
/// application (`show_status_html`).
pub fn server_status_page(project: &Project, s: &ServerStatus) -> String {
    let group = |rows: &[(&str, String)]| {
        site_ui::stats(
            &rows
                .iter()
                .map(|(label, n)| site_ui::stat(label, n, ""))
                .collect::<Vec<_>>(),
        )
    };
    let n = |x: i64| x.to_string();
    let mut body = format!(
        "{}<h1>{} server status</h1>\n",
        site_header(project),
        html_escape(&project.config.long_name)
    );
    body.push_str("<h2>Tasks</h2>\n");
    body.push_str(&group(&[
        ("Tasks ready to send", n(s.results_ready_to_send)),
        ("Tasks in progress", n(s.results_in_progress)),
        (
            "Workunits waiting for validation",
            n(s.workunits_waiting_for_validation),
        ),
        (
            "Workunits waiting for assimilation",
            n(s.workunits_waiting_for_assimilation),
        ),
        (
            "Workunits waiting for file deletion",
            n(s.workunits_waiting_for_deletion),
        ),
        (
            "Tasks waiting for file deletion",
            n(s.results_waiting_for_deletion),
        ),
        ("Transitions due", n(s.transitions_due)),
        (
            "Transitioner backlog (hours)",
            format!("{:.2}", s.transitioner_backlog_hours),
        ),
    ]));
    body.push_str("<h2>Accounts and computers</h2>\n");
    body.push_str(&group(&[
        ("Accounts", n(s.users)),
        ("Accounts with credit", n(s.users_with_credit)),
        ("Accounts with recent credit", n(s.users_with_recent_credit)),
        (
            "Accounts registered in past 24 hours",
            n(s.users_past_24_hours),
        ),
        ("Computers", n(s.hosts)),
        ("Computers with credit", n(s.hosts_with_credit)),
        (
            "Computers with recent credit",
            n(s.hosts_with_recent_credit),
        ),
        (
            "Computers registered in past 24 hours",
            n(s.hosts_past_24_hours),
        ),
        ("Current GigaFLOPS", format!("{:.2}", s.flops)),
    ]));
    let apps: Vec<Vec<String>> = s
        .apps
        .iter()
        .filter(|a| !a.deprecated)
        .map(|a| {
            let (runtime, users) = match (a.avg_hours, a.min_hours, a.max_hours) {
                (Some(avg), Some(min), Some(max)) if max > 0.0 => (
                    format!(
                        "{} ({} - {})",
                        duration_hours(avg),
                        duration_hours(min),
                        duration_hours(max)
                    ),
                    a.users_24h.to_string(),
                ),
                _ => ("---".into(), "---".into()),
            };
            vec![
                html_escape(&a.user_friendly_name),
                a.unsent.to_string(),
                a.in_progress.to_string(),
                runtime,
                users,
            ]
        })
        .collect();
    let table = if apps.is_empty() {
        site_ui::empty("No applications are available yet.")
    } else {
        site_ui::data_table(
            &[
                site_ui::Col::text("Application"),
                site_ui::Col::num("Unsent"),
                site_ui::Col::num("In progress"),
                site_ui::Col::text("Runtime of recent tasks: average, min, max"),
                site_ui::Col::num("Users in last 24 hours"),
            ],
            &apps,
        )
    };
    body.push_str(&site_ui::card("Tasks by application", &table));
    body.push_str(&format!(
        "<p>Task data as of {}.</p>\n",
        crate::site::time_str(s.cached_time)
    ));
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
