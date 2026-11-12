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

/// Why an account may not be made here, if it may not. Shared by
/// `create_account.php` and the site's sign-up form, as upstream's
/// `disable_account_creation` applies to both.
pub fn account_creation_refusal(config: &Config) -> Option<AccountError> {
    (!config.account_creation).then_some(AccountError::CreationDisabled)
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
    let inserted = db
        .run(stmt!(
            "INSERT INTO user (create_time, email_addr, name, authenticator, cross_project_id, \
             passwd_hash, expavg_time, country, postal_code) SELECT ?, ?, ?, ?, ?, ?, ?, ?, ? \
             WHERE NOT EXISTS (SELECT 1 FROM user WHERE previous_email_addr = ? \
             AND previous_email_addr != '' AND email_addr_change_time > ?) \
             ON CONFLICT (email_addr) DO NOTHING RETURNING id",
            now,
            email.as_str(),
            name.as_str(),
            authenticator.as_str(),
            cross_project_id.as_str(),
            stored.as_str(),
            now,
            account.country,
            account.postal_code,
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
    body.push_str("</table>\n<p><a href=\"home.php\">Your account</a> &middot; ");
    if account_creation_refusal(c).is_none() {
        body.push_str("<a href=\"create_account_form.php\">Create an account</a> &middot; ");
    }
    body.push_str("<a href=\"server_status.php\">Server status</a></p>\n");
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
