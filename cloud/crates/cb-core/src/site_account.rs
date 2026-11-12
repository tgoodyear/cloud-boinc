//! Site pages that change an account: sign-up, account details, password,
//! email address, and computing and project preferences. Their behaviour
//! follows upstream `html/user/create_account_*.php`, `edit_*_form.php`,
//! `edit_*_action.php`, `prefs.php`, `prefs_edit.php` and `prefs_remove.php`.
//!
//! Every page that changes something takes a POST only, and every one used
//! while logged in also needs the form token its form page put in the form
//! (`form_tokens` and `check_tokens` upstream). Each page runs the account by
//! cookie, then one write or one batch (the address change, which also ends
//! login links and stores the undo token, runs a batch of up to five).

use cb_crypto::account::{
    constant_time_eq, form_token, passwd_hash, pepper_passwd_hash, random_hex_string,
    verify_peppered_passwd_hash,
};
use cb_db::stmt;

use crate::countries::{is_valid_country, COUNTRIES};
use crate::mail_guard::{cannot_receive, CANNOT_RECEIVE};
use crate::model::User;
use crate::prefs::{PrefsForm, Subset, VENUES};
use crate::project::Project;
use crate::site::{
    error_page, label_row, layout, local_url, login_cookie, row2, str_param, to_login,
    user_by_auth, viewer, Params, SiteCtx, SitePage,
};
use crate::site_email::{self, EMAIL_CHANGE_WAIT_SECS};
use crate::web::{
    account_creation_refusal, html_escape, is_strict_email, is_valid_postal_code,
    is_valid_user_name, is_valid_user_url, make_user, random_bytes, NewAccount,
};
use crate::CoreResult;

/// `min_passwd_length` default upstream.
pub const MIN_PASSWD_LENGTH: usize = 6;
/// Longest password taken; the client hashes it, so this bounds only the
/// work done here.
const MAX_PASSWD_LENGTH: usize = 256;
/// How long a form token is accepted, as upstream's `valid_tokens`.
pub const FORM_TOKEN_SECS: i64 = 86400;

pub(crate) const TIMED_OUT: &str =
    "Link has timed out. Please go back, reload the page, and try again.";

/// The hidden fields that tie a form to the logged-in account.
pub(crate) fn token_fields(user: &User, now: i64) -> String {
    format!(
        "<input type=\"hidden\" name=\"tnow\" value=\"{now}\">\n\
         <input type=\"hidden\" name=\"ttok\" value=\"{}\">\n",
        form_token(&user.authenticator, now)
    )
}

/// Whether the request is a POST carrying a current token for `user`.
pub(crate) fn token_ok(ctx: &SiteCtx<'_>, user: &User, q: &Params) -> bool {
    let Ok(tnow) = str_param(q, "tnow").parse::<i64>() else {
        return false;
    };
    ctx.post
        && tnow <= ctx.now
        && ctx.now - tnow <= FORM_TOKEN_SECS
        && constant_time_eq(
            form_token(&user.authenticator, tnow).as_bytes(),
            str_param(q, "ttok").as_bytes(),
        )
}

/// `is_valid_user_name` with the site's extra rule that a name is not a
/// number.
fn site_user_name_error(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        Some("You must supply a name for your account.")
    } else if !is_valid_user_name(name) {
        Some("A name may not have special characters, or leading or trailing spaces.")
    } else if name.parse::<f64>().is_ok() {
        Some("A name may not be a number.")
    } else {
        None
    }
}

fn passwd_error(passwd: &str) -> Option<String> {
    if !passwd.is_ascii() {
        Some("Passwords may only include ASCII characters.".into())
    } else if passwd.len() < MIN_PASSWD_LENGTH {
        Some(format!(
            "New password is too short: minimum password length is {MIN_PASSWD_LENGTH} characters."
        ))
    } else if passwd.len() > MAX_PASSWD_LENGTH {
        Some("New password is too long.".into())
    } else {
        None
    }
}

fn country_select(selected: &str) -> String {
    let mut out =
        String::from("<select id=\"country\" name=\"country\">\n<option value=\"\">---</option>\n");
    for c in COUNTRIES {
        let sel = if *c == selected { " selected" } else { "" };
        out.push_str(&format!(
            "<option value=\"{0}\"{sel}>{0}</option>\n",
            html_escape(c)
        ));
    }
    out.push_str("</select>");
    out
}

fn page(project: &Project, title: &str, body: &str) -> SitePage {
    SitePage::html(200, layout(project, title, body))
}

fn done(project: &Project, title: &str, msg: &str) -> SitePage {
    page(
        project,
        title,
        &format!(
            "<p class=\"notice\">{}</p>\n<p><a href=\"home.php\">Your account</a></p>\n",
            html_escape(msg)
        ),
    )
}

fn passwd_input(name: &str, label: &str, autocomplete: &str) -> String {
    format!(
        "<p><label>{label}<br><input type=\"password\" name=\"{name}\" autocomplete=\"{autocomplete}\"></label></p>\n"
    )
}

// ---------------------------------------------------------------------------
// Log out.

/// `logout.php`: clears the cookie and goes to the master page. As upstream
/// it needs a logged-in account and the form token (`check_tokens`), and
/// takes a POST only, so a link or an image on another site cannot log a
/// browser out. With no login there is nothing to clear; a missing, old or
/// foreign token, or a GET, is refused as the other actions refuse it.
pub async fn logout(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    let Some(user) = viewer(ctx).await? else {
        return Ok(SitePage::redirect(ctx.base.to_string()));
    };
    if !token_ok(ctx, &user, q) {
        return Ok(error_page(project, 403, TIMED_OUT));
    }
    let mut out = SitePage::redirect(ctx.base.to_string());
    out.clear_login(ctx.base);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Sign-up.

/// `create_account_form.php` (and `signup.php`).
pub fn create_account_form(project: &Project, q: &Params) -> SitePage {
    if account_creation_refusal(&project.config).is_some() {
        return error_page(project, 403, "Account creation is disabled.");
    }
    let next = local_url(str_param(q, "next_url")).unwrap_or("home.php");
    let mut b = format!(
        "<form method=\"post\" action=\"create_account_action.php\">\n\
         <input type=\"hidden\" name=\"next_url\" value=\"{}\">\n\
         <p><label>Screen name<br><input type=\"text\" name=\"new_name\" autocomplete=\"nickname\" maxlength=\"254\"></label></p>\n\
         <p><label>Email address<br><input type=\"email\" name=\"new_email_addr\" autocomplete=\"email\" maxlength=\"254\"></label></p>\n",
        html_escape(next)
    );
    b.push_str(&passwd_input(
        "passwd",
        &format!("Password (at least {MIN_PASSWD_LENGTH} characters)"),
        "new-password",
    ));
    b.push_str(&passwd_input("passwd2", "Password again", "new-password"));
    if project.config.user_country {
        b.push_str(&format!(
            "<p><label>Country<br>{}</label></p>\n",
            country_select("")
        ));
    }
    b.push_str(
        "<p><label>Postal code (optional)<br><input type=\"text\" name=\"postal_code\" maxlength=\"64\"></label></p>\n\
         <p><input type=\"submit\" value=\"Create account\"></p>\n</form>\n\
         <p>Already have an account? <a href=\"login_form.php\">Log in</a>.</p>\n",
    );
    page(project, "Create an account", &b)
}

/// `create_account_action.php`: makes the account with the same code as
/// `create_account.php`, logs the browser in and goes to `next_url`. As
/// that RPC, an email address that has an account with this password logs
/// in to it.
pub async fn create_account_action(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    if account_creation_refusal(&project.config).is_some() {
        return Ok(error_page(project, 403, "Account creation is disabled."));
    }
    if !ctx.post {
        return Ok(error_page(
            project,
            405,
            "Use the form to create an account.",
        ));
    }
    let Some(pepper) = ctx.pepper else {
        return Ok(error_page(
            project,
            503,
            "Account creation is not available now.",
        ));
    };
    let name = str_param(q, "new_name");
    if let Some(e) = site_user_name_error(name) {
        return Ok(error_page(project, 400, e));
    }
    let email = str_param(q, "new_email_addr").to_lowercase();
    if cannot_receive(ctx, &email) {
        return Ok(error_page(project, 400, CANNOT_RECEIVE));
    }
    if !is_strict_email(&email) {
        return Ok(error_page(
            project,
            400,
            "Invalid email address: please enter a valid address of the form name@xxx.yyy",
        ));
    }
    let passwd = q.get("passwd").map(String::as_str).unwrap_or_default();
    if let Some(e) = passwd_error(passwd) {
        return Ok(error_page(project, 400, &e));
    }
    if q.get("passwd2").map(String::as_str) != Some(passwd) {
        return Ok(error_page(
            project,
            400,
            "The two passwords are not the same.",
        ));
    }
    let country = if project.config.user_country {
        str_param(q, "country")
    } else {
        ""
    };
    if !is_valid_country(country) {
        return Ok(error_page(project, 400, "Unknown country."));
    }
    let postal_code = str_param(q, "postal_code");
    if !is_valid_postal_code(postal_code) {
        return Ok(error_page(project, 400, "Invalid postal code."));
    }
    let hash = passwd_hash(passwd, &email);
    let random = ctx.random;
    let made = make_user(
        ctx.db,
        pepper,
        ctx.now,
        random,
        &NewAccount {
            email_addr: &email,
            passwd_hash: &hash,
            user_name: name,
            country,
            postal_code,
        },
    )
    .await?;
    let authenticator = match made {
        Ok(a) => a,
        Err(cb_proto::web_rpc::AccountError::NotUnique) => {
            return Ok(error_page(
                project,
                409,
                "There's already an account with that email address.",
            ))
        }
        Err(e) => return Ok(error_page(project, 400, e.message())),
    };
    if let Some(mail) = ctx.mail() {
        // The account exists whether or not this works; the account page can
        // ask for the message again.
        if let Ok(Some(user)) = user_by_auth(ctx.db, &authenticator).await {
            if user.email_validated == 0 {
                let _ =
                    site_email::send_verification(ctx, &mail, &user, site_email::Budget::Signup)
                        .await;
            }
        }
    }
    let next = local_url(str_param(q, "next_url")).unwrap_or("home.php");
    let mut out = SitePage::redirect(format!("{}{next}", ctx.base));
    out.set_cookie = Some(login_cookie(ctx.base, &authenticator, true));
    Ok(out)
}

// ---------------------------------------------------------------------------
// Account details.

/// `edit_user_info_form.php`.
pub async fn edit_user_info_form(ctx: &SiteCtx<'_>) -> CoreResult<SitePage> {
    let project = ctx.project;
    let Some(user) = viewer(ctx).await? else {
        return Ok(to_login(ctx.base, "edit_user_info_form.php"));
    };
    let mut b = format!(
        "<form method=\"post\" action=\"edit_user_info_action.php\">\n{}<table>\n",
        token_fields(&user, ctx.now)
    );
    label_row(
        &mut b,
        "user_name",
        "Name",
        &format!(
            "<input type=\"text\" id=\"user_name\" name=\"user_name\" maxlength=\"254\" value=\"{}\">",
            html_escape(&user.name)
        ),
    );
    if project.config.user_url {
        label_row(
            &mut b,
            "url",
            "URL",
            &format!(
                "<input type=\"text\" id=\"url\" name=\"url\" maxlength=\"254\" value=\"{}\">",
                html_escape(&user.url)
            ),
        );
    }
    if project.config.user_country {
        label_row(&mut b, "country", "Country", &country_select(&user.country));
    }
    label_row(
        &mut b,
        "postal_code",
        "Postal code",
        &format!(
            "<input type=\"text\" id=\"postal_code\" name=\"postal_code\" maxlength=\"64\" value=\"{}\">",
            html_escape(&user.postal_code)
        ),
    );
    b.push_str("</table>\n<p><input type=\"submit\" value=\"Update info\"></p>\n</form>\n");
    Ok(page(project, "Edit account information", &b))
}

/// `edit_user_info_action.php`. As upstream, a field the project does not
/// show (`user_url`, `user_country` off) is cleared. Upstream also refuses
/// a name another account has; names are not unique here, and checking
/// would scan the `user` table.
pub async fn edit_user_info_action(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    let Some(user) = viewer(ctx).await? else {
        return Ok(to_login(ctx.base, "edit_user_info_form.php"));
    };
    if !token_ok(ctx, &user, q) {
        return Ok(error_page(project, 403, TIMED_OUT));
    }
    let name = str_param(q, "user_name");
    if let Some(e) = site_user_name_error(name) {
        return Ok(error_page(project, 400, e));
    }
    let url = if project.config.user_url {
        str_param(q, "url")
    } else {
        ""
    };
    if !is_valid_user_url(url) {
        return Ok(error_page(project, 400, "Invalid URL"));
    }
    let country = if project.config.user_country {
        str_param(q, "country")
    } else {
        ""
    };
    if !is_valid_country(country) {
        return Ok(error_page(project, 400, "bad country"));
    }
    let postal_code = str_param(q, "postal_code");
    if !is_valid_postal_code(postal_code) {
        return Ok(error_page(project, 400, "Invalid postal code."));
    }
    // Every write below is conditional on the account key read with the
    // account: a request that authenticated before `reset_account_key.php`
    // finds the key changed and writes nothing.
    let updated = ctx
        .db
        .run(stmt!(
            "UPDATE user SET name = ?, url = ?, country = ?, postal_code = ? \
             WHERE id = ? AND authenticator = ? RETURNING id",
            name,
            url,
            country,
            postal_code,
            user.id,
            user.authenticator.as_str()
        ))
        .await?;
    if updated.rows.is_empty() {
        return Ok(error_page(
            project,
            409,
            "The account changed meanwhile; please try again.",
        ));
    }
    Ok(SitePage::redirect(format!("{}home.php", ctx.base)))
}

// ---------------------------------------------------------------------------
// Password and email address.

/// Whether `given` is the account's password or its account key.
fn proves_account(pepper: &[u8], user: &User, given: &str) -> bool {
    !given.is_empty()
        && (constant_time_eq(user.authenticator.as_bytes(), given.as_bytes())
            || (!user.passwd_hash.is_empty()
                && verify_peppered_passwd_hash(
                    pepper,
                    &passwd_hash(given, &user.email_addr),
                    &user.passwd_hash,
                )))
}

/// `edit_passwd_form.php`. Unlike current upstream, which asks only for the
/// login cookie and a fresh token, the current password or the account key
/// is asked for too.
pub async fn edit_passwd_form(ctx: &SiteCtx<'_>) -> CoreResult<SitePage> {
    let project = ctx.project;
    let Some(user) = viewer(ctx).await? else {
        return Ok(to_login(ctx.base, "edit_passwd_form.php"));
    };
    let mut b = format!(
        "<form method=\"post\" action=\"edit_passwd_action.php\">\n{}",
        token_fields(&user, ctx.now)
    );
    b.push_str(&passwd_input(
        "old_passwd",
        "Current password, or your account key",
        "current-password",
    ));
    b.push_str(&passwd_input(
        "passwd",
        &format!("New password (at least {MIN_PASSWD_LENGTH} characters)"),
        "new-password",
    ));
    b.push_str(&passwd_input(
        "passwd2",
        "New password again",
        "new-password",
    ));
    b.push_str(
        "<p><input type=\"submit\" value=\"Change password\"></p>\n</form>\n\
         <p>Changing the password changes your weak account key.</p>\n",
    );
    Ok(page(project, "Change password", &b))
}

/// `edit_passwd_action.php`.
pub async fn edit_passwd_action(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    let Some(user) = viewer(ctx).await? else {
        return Ok(to_login(ctx.base, "edit_passwd_form.php"));
    };
    if !token_ok(ctx, &user, q) {
        return Ok(error_page(project, 403, TIMED_OUT));
    }
    let Some(pepper) = ctx.pepper else {
        return Ok(error_page(
            project,
            503,
            "Password changes are not available now.",
        ));
    };
    let get = |k: &str| q.get(k).map(String::as_str).unwrap_or_default();
    if !proves_account(pepper, &user, get("old_passwd")) {
        return Ok(error_page(
            project,
            403,
            "The current password (or account key) you entered is incorrect.",
        ));
    }
    let passwd = get("passwd");
    if let Some(e) = passwd_error(passwd) {
        return Ok(error_page(project, 400, &e));
    }
    if get("passwd2") != passwd {
        return Ok(error_page(
            project,
            400,
            "The two passwords are not the same.",
        ));
    }
    let stored = pepper_passwd_hash(pepper, &passwd_hash(passwd, &user.email_addr));
    // Conditional on the row read, so a change made meanwhile (a new email
    // address, which the hash depends on) is not overwritten.
    // The login links mailed before the change stop working with it.
    let out = ctx
        .db
        .batch(vec![
            stmt!(
                "UPDATE user SET passwd_hash = ? WHERE id = ? AND email_addr = ? \
                 AND passwd_hash = ? AND authenticator = ? RETURNING id",
                stored.as_str(),
                user.id,
                user.email_addr.as_str(),
                user.passwd_hash.as_str(),
                user.authenticator.as_str()
            ),
            stmt!(
                // Only if the update before it changed a row: a newer change
                // by another request can leave the account looking as this one
                // meant it, and its links must stay.
                "UPDATE token SET used_time = ? WHERE userid = ? AND type = 'L' AND used_time = 0 \
                 AND changes() > 0 AND EXISTS (SELECT 1 FROM user WHERE id = ? \
                 AND passwd_hash = ? AND authenticator = ?)",
                ctx.now,
                user.id,
                user.id,
                stored.as_str(),
                user.authenticator.as_str()
            ),
        ])
        .await?;
    if !out.first().is_some_and(|r| !r.rows.is_empty()) {
        return Ok(error_page(
            project,
            409,
            "The account changed meanwhile; please try again.",
        ));
    }
    Ok(done(
        project,
        "Change password",
        "Your password has been changed.",
    ))
}

/// `edit_email_form.php`.
pub async fn edit_email_form(ctx: &SiteCtx<'_>) -> CoreResult<SitePage> {
    let project = ctx.project;
    let Some(user) = viewer(ctx).await? else {
        return Ok(to_login(ctx.base, "edit_email_form.php"));
    };
    let mut b = format!(
        "<form method=\"post\" action=\"edit_email_action.php\">\n{}\
         <p>Current address: {}</p>\n\
         <p><label>New email address<br><input type=\"email\" name=\"email_addr\" autocomplete=\"email\" maxlength=\"254\"></label></p>\n",
        token_fields(&user, ctx.now),
        html_escape(&user.email_addr)
    );
    let label = if user.passwd_hash.is_empty() {
        "Account key (this account has no password)"
    } else {
        "Password"
    };
    b.push_str(&passwd_input("passwd", label, "current-password"));
    b.push_str("<p><input type=\"submit\" value=\"Change email address\"></p>\n</form>\n");
    // The weak account key is made from the stored password hash, which the
    // change makes again; an account with no password has none to change.
    let changes = if user.passwd_hash.is_empty() {
        "your cross-project ID"
    } else {
        "your cross-project ID and weak account key"
    };
    if ctx.mail().is_some() {
        b.push_str(&format!(
            "<p>A message about the change is sent to both addresses; the one to the previous \
             address has a link that undoes the change for seven days. You cannot change the \
             address again for seven days. The new address is not verified until you ask for \
             a verification message on your account page. The change also changes {changes}.</p>\n"
        ));
    } else {
        b.push_str(&format!(
            "<p>No message is sent to either address, and the new address is not verified. \
             The change also changes {changes}.</p>\n"
        ));
    }
    Ok(page(project, "Change email address", &b))
}

/// `edit_email_action.php`. The stored password hash is made from the
/// address, so it is made again from the password given. An account with
/// no password proves itself with its account key and stays without one;
/// upstream would make the account key its password instead.
///
/// With email ([`SiteCtx::mail`]) it does what upstream does besides: refuses
/// a change within seven days of the last, keeps the previous address and the
/// time of the change, and sends a notice to the new address and the link
/// that undoes the change (`recover_email.php`) to the previous one
/// ([`site_email::send_change_notices`]). Without email only the address,
/// the hash and `email_validated` change, as before.
pub async fn edit_email_action(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    let mail = ctx.mail();
    let Some(user) = viewer(ctx).await? else {
        return Ok(to_login(ctx.base, "edit_email_form.php"));
    };
    if !token_ok(ctx, &user, q) {
        return Ok(error_page(project, 403, TIMED_OUT));
    }
    let Some(pepper) = ctx.pepper else {
        return Ok(error_page(
            project,
            503,
            "Email changes are not available now.",
        ));
    };
    let email = str_param(q, "email_addr").to_lowercase();
    if cannot_receive(ctx, &email) {
        return Ok(error_page(project, 400, CANNOT_RECEIVE));
    }
    if !is_strict_email(&email) {
        return Ok(error_page(
            project,
            400,
            "The new email address is invalid.",
        ));
    }
    if email == user.email_addr {
        return Ok(error_page(
            project,
            400,
            "New email address is same as existing address. Nothing is changed.",
        ));
    }
    if mail.is_some() && user.email_addr_change_time + EMAIL_CHANGE_WAIT_SECS > ctx.now {
        return Ok(error_page(
            project,
            429,
            &format!(
                "The email address was changed within the past 7 days; please look for a \
                 message at {} if that change is not yours.",
                user.previous_email_addr
            ),
        ));
    }
    let passwd = q.get("passwd").map(String::as_str).unwrap_or_default();
    let new_hash = if user.passwd_hash.is_empty() {
        if !constant_time_eq(user.authenticator.as_bytes(), passwd.as_bytes()) {
            return Ok(error_page(project, 403, "Invalid account key."));
        }
        String::new()
    } else {
        if !verify_peppered_passwd_hash(
            pepper,
            &passwd_hash(passwd, &user.email_addr),
            &user.passwd_hash,
        ) {
            return Ok(error_page(project, 403, "Invalid password."));
        }
        pepper_passwd_hash(pepper, &passwd_hash(passwd, &email))
    };
    // One batch: whether the address is taken, the change, the end of the
    // login links mailed to the old address, and with email the undo token.
    // The change is refused for an address another account has or has just
    // changed away from (its undo link has to find it free). Without email
    // nothing new is remembered, and the undo links of an earlier change end
    // (they are bound to the previous address, which this change makes
    // stale); the earlier state itself is left, so that it still reserves its
    // address for the rest of its week.
    let mut batch = vec![
        stmt!(
            "SELECT (SELECT count(*) FROM user WHERE email_addr = ?) + \
             (SELECT count(*) FROM user WHERE previous_email_addr = ? \
             AND previous_email_addr != '' AND email_addr_change_time > ? AND id != ?) AS n",
            email.as_str(),
            email.as_str(),
            ctx.now - EMAIL_CHANGE_WAIT_SECS,
            user.id
        ),
        if mail.is_some() {
            stmt!(
                "UPDATE user SET email_addr = ?, passwd_hash = ?, email_validated = 0, \
                 previous_email_addr = ?, email_addr_change_time = ? \
                 WHERE id = ? AND email_addr = ? AND passwd_hash = ? AND email_addr_change_time = ? \
                 AND authenticator = ? AND NOT EXISTS (SELECT 1 FROM user WHERE email_addr = ?) \
                 AND NOT EXISTS (SELECT 1 FROM user WHERE previous_email_addr = ? \
                 AND previous_email_addr != '' AND email_addr_change_time > ? AND id != ?) \
                 RETURNING id",
                email.as_str(),
                new_hash.as_str(),
                user.email_addr.as_str(),
                ctx.now,
                user.id,
                user.email_addr.as_str(),
                user.passwd_hash.as_str(),
                user.email_addr_change_time,
                user.authenticator.as_str(),
                email.as_str(),
                email.as_str(),
                ctx.now - EMAIL_CHANGE_WAIT_SECS,
                user.id
            )
        } else {
            stmt!(
                "UPDATE user SET email_addr = ?, passwd_hash = ?, email_validated = 0 \
                 WHERE id = ? AND email_addr = ? AND passwd_hash = ? AND authenticator = ? \
                 AND NOT EXISTS (SELECT 1 FROM user WHERE email_addr = ?) \
                 AND NOT EXISTS (SELECT 1 FROM user WHERE previous_email_addr = ? \
                 AND previous_email_addr != '' AND email_addr_change_time > ? AND id != ?) \
                 RETURNING id",
                email.as_str(),
                new_hash.as_str(),
                user.id,
                user.email_addr.as_str(),
                user.passwd_hash.as_str(),
                user.authenticator.as_str(),
                email.as_str(),
                email.as_str(),
                ctx.now - EMAIL_CHANGE_WAIT_SECS,
                user.id
            )
        },
    ];
    // What follows the update runs only if this request's update changed a
    // row. With email the undo token is stored right after it (`changes()`
    // refers to the statement before), and the login links end after that,
    // on the token being there; without email the links end right after it.
    let undo_token = if mail.is_some() {
        let (token, stmts) = site_email::change_token_stmts(ctx, user.id, &user.email_addr, &email);
        batch.extend(stmts);
        Some(token)
    } else {
        batch.push(stmt!(
            "UPDATE token SET used_time = ? WHERE userid = ? AND type IN ('L', 'E') \
             AND used_time = 0 AND changes() > 0 \
             AND EXISTS (SELECT 1 FROM user WHERE id = ? AND email_addr = ? AND authenticator = ?)",
            ctx.now,
            user.id,
            user.id,
            email.as_str(),
            user.authenticator.as_str()
        ));
        None
    };
    let out = ctx.db.batch(batch).await?;
    let taken = out.first().and_then(|r| r.scalar_i64()).unwrap_or(0) > 0;
    let updated = out.get(1).is_some_and(|r| !r.rows.is_empty());
    if taken {
        return Ok(error_page(
            project,
            409,
            "There's already an account with that email address.",
        ));
    }
    if !updated {
        return Ok(error_page(
            project,
            409,
            "The account changed meanwhile; please try again.",
        ));
    }
    let mut msg = format!("The email address of your account is now {email}.");
    if let (Some(mail), Some(token)) = (&mail, &undo_token) {
        let sent =
            site_email::send_change_notices(ctx, mail, token, user.id, &user.email_addr, &email)
                .await?;
        if !sent {
            msg.push_str(&format!(
                " The address was changed, but we were unable to send the confirmation \
                 email to {email} or {}. Please note this change yourself.",
                user.email_addr
            ));
        }
    }
    Ok(done(project, "Change email address", &msg))
}

// ---------------------------------------------------------------------------
// Account key.

/// `reset_account_key.php`, not in upstream: replaces the account key, which
/// is also what the login cookie holds, so it is the way to put out someone
/// else who has the key or a session. A GET shows what it does; a POST with
/// the form token makes a new random key, which changes the weak key too (it
/// is derived from the key), ends the account's unused login and verification
/// links, and logs this browser in with the new key. It asks for the session
/// and the token only, since after an address undo the password is not known.
/// The undo link of an address change is left alone: whoever has the key could
/// otherwise end it. Computers attached with the old key stop working until
/// they are attached again, and so do account managers that hold it.
pub async fn reset_account_key(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    let Some(user) = viewer(ctx).await? else {
        return Ok(to_login(ctx.base, "reset_account_key.php"));
    };
    if !ctx.post {
        return Ok(page(
            project,
            "Replace the account key",
            &format!(
                "<p>Your account key is what logs this browser in and what the computers \
                 attached to your account use. Replace it if someone else may have it: the \
                 old key and every session with it stop working, and so does the weak \
                 account key. <strong>Every computer attached with the old key has to be \
                 attached again</strong>, with the new key shown on your account page, and \
                 so does every account manager that has it.</p>\n\
                 <form method=\"post\" action=\"reset_account_key.php\">\n{}\
                 <p><input type=\"submit\" value=\"Replace the account key\"></p>\n</form>\n",
                token_fields(&user, ctx.now)
            ),
        ));
    }
    if !token_ok(ctx, &user, q) {
        return Ok(error_page(project, 403, TIMED_OUT));
    }
    let new_key = random_hex_string(&random_bytes(ctx.random));
    let out = ctx
        .db
        .batch(vec![
            stmt!(
                "UPDATE user SET authenticator = ? WHERE id = ? AND authenticator = ? RETURNING id",
                new_key.as_str(),
                user.id,
                user.authenticator.as_str()
            ),
            stmt!(
                "UPDATE token SET used_time = ? WHERE userid = ? AND type != 'E' AND used_time = 0 \
                 AND EXISTS (SELECT 1 FROM user WHERE id = ? AND authenticator = ?)",
                ctx.now,
                user.id,
                user.id,
                new_key.as_str()
            ),
        ])
        .await?;
    if !out.first().is_some_and(|r| !r.rows.is_empty()) {
        return Ok(error_page(
            project,
            409,
            "The account changed meanwhile; please try again.",
        ));
    }
    let mut out = done(
        project,
        "Replace the account key",
        "Your account key has been replaced. Attach your computers again with the new key shown on your account page.",
    );
    // The server does not know how long the cookie it replaces was meant to
    // last (it only gets the value), so this one ends with the browser.
    out.set_cookie = Some(login_cookie(ctx.base, &new_key, false));
    Ok(out)
}

// ---------------------------------------------------------------------------
// Preferences.

fn subset_param(q: &Params) -> Option<Subset> {
    Subset::parse(str_param(q, "subset"))
}

/// The venue named in the request: `Ok(None)` for the main set.
fn venue_param(q: &Params) -> Result<Option<usize>, ()> {
    match str_param(q, "venue") {
        "" => Ok(None),
        v => VENUES.iter().position(|x| *x == v).map(Some).ok_or(()),
    }
}

fn stored_prefs(user: &User, subset: Subset) -> &str {
    match subset {
        Subset::Global => &user.global_prefs,
        Subset::Project => &user.project_prefs,
    }
}

/// `prefs.php?subset=global|project`: the main set and each venue's.
pub async fn prefs(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    let Some(subset) = subset_param(q) else {
        return Ok(error_page(project, 400, "bad subset"));
    };
    let Some(user) = viewer(ctx).await? else {
        return Ok(to_login(
            ctx.base,
            &format!("prefs.php?subset={}", subset.name()),
        ));
    };
    let form = PrefsForm::new(subset, project);
    let prefs = form.parse(stored_prefs(&user, subset));
    let s = subset.name();
    let mut b = String::new();
    if str_param(q, "updated") == "1" {
        b.push_str("<p><strong>Your preferences have been updated.</strong></p>\n");
    }
    if subset == Subset::Global {
        b.push_str(
            "<p>These preferences apply to all the BOINC projects in which you take part. \
             A computer gets them the next time it contacts this project.</p>\n",
        );
        if prefs.mod_time > 0.0 {
            b.push_str(&format!(
                "<p>Last modified: {}</p>\n",
                crate::site::time_str(prefs.mod_time as i64)
            ));
        }
    }
    b.push_str("<h2>Main preferences</h2>\n<table>\n");
    b.push_str(&form.show(&prefs.main));
    if subset == Subset::Project {
        let yes_no = |x: i32| if x != 0 { "yes" } else { "no" };
        row2(
            &mut b,
            "Email from this project allowed",
            yes_no(user.send_email),
        );
        row2(
            &mut b,
            "Computers shown on this site",
            if project.config.host_owner_public {
                "yes, every computer is shown with its owner's name"
            } else {
                yes_no(user.show_hosts)
            },
        );
        row2(
            &mut b,
            "Default computer location",
            &if user.venue.is_empty() {
                "---".into()
            } else {
                html_escape(&user.venue)
            },
        );
    }
    b.push_str(&format!(
        "</table>\n<p><a href=\"prefs_edit.php?subset={s}\">Edit preferences</a></p>\n"
    ));
    for (i, venue) in VENUES.iter().enumerate() {
        match &prefs.venues[i] {
            Some(set) => {
                b.push_str(&format!(
                    "<h2>Separate preferences for {venue}</h2>\n<table>\n{}</table>\n\
                     <p><a href=\"prefs_edit.php?subset={s}&amp;venue={venue}\">Edit preferences</a> &middot; \
                     <a href=\"prefs_remove.php?subset={s}&amp;venue={venue}\">Remove</a></p>\n",
                    form.show(set)
                ));
            }
            None => b.push_str(&format!(
                "<p><a href=\"prefs_edit.php?subset={s}&amp;venue={venue}\">Add separate preferences for {venue}</a></p>\n"
            )),
        }
    }
    let other = match subset {
        Subset::Global => "<a href=\"prefs.php?subset=project\">Project preferences</a>",
        Subset::Project => "<a href=\"prefs.php?subset=global\">Computing preferences</a>",
    };
    b.push_str(&format!(
        "<p>{other} &middot; <a href=\"home.php\">Your account</a></p>\n"
    ));
    Ok(page(
        project,
        &format!("{} preferences", subset.title()),
        &b,
    ))
}

/// The page for a save that found the stored document changed since it was
/// read.
fn prefs_conflict(project: &Project) -> SitePage {
    error_page(
        project,
        409,
        "Your preferences changed while this was being saved, and nothing was saved. \
         Reload the preferences and try again.",
    )
}

fn venue_title(subset: Subset, venue: Option<usize>) -> String {
    match venue {
        Some(i) => format!(
            "Edit {} preferences for {}",
            subset.title().to_lowercase(),
            VENUES[i]
        ),
        None => format!("Edit {} preferences", subset.title().to_lowercase()),
    }
}

/// The form of `prefs_edit.php`, with the values given; with `sent`, the ones a
/// browser submitted, for a page that reports an error.
#[allow(clippy::too_many_arguments)]
fn prefs_form_page(
    ctx: &SiteCtx<'_>,
    user: &User,
    form: &PrefsForm,
    subset: Subset,
    venue: Option<usize>,
    set: &crate::prefs::PrefSet,
    sent: Option<&Params>,
    bad: &[&str],
) -> SitePage {
    let project = ctx.project;
    let mut b = String::new();
    if !bad.is_empty() {
        b.push_str(
            "<p><strong>Unable to update preferences.</strong> The values marked below were not \
             accepted: a number out of range, not numeric or not whole where a whole number is \
             needed, or no application chosen. Nothing was saved; what you entered is shown, so \
             correct the marked values and send it again.</p>\n",
        );
    }
    b.push_str(&format!(
        "<form method=\"post\" action=\"prefs_edit.php\">\n{}\
         <input type=\"hidden\" name=\"subset\" value=\"{}\">\n",
        token_fields(user, ctx.now),
        subset.name()
    ));
    if let Some(i) = venue {
        b.push_str(&format!(
            "<input type=\"hidden\" name=\"venue\" value=\"{}\">\n",
            VENUES[i]
        ));
    }
    b.push_str("<table>\n");
    b.push_str(&form.form(set, sent, bad));
    if subset == Subset::Project && venue.is_none() {
        // What was submitted stays on an error page, as the preference fields.
        let flag = |k: &str, stored: i32| match sent {
            Some(q) => q.get(k).is_some_and(|v| !v.is_empty()),
            None => stored != 0,
        };
        let checked = |on: bool| if on { " checked" } else { "" };
        label_row(
            &mut b,
            "send_email",
            "Is it OK for this project to email you?",
            &format!(
                "<input type=\"checkbox\" id=\"send_email\" name=\"send_email\" value=\"1\"{}>",
                checked(flag("send_email", user.send_email))
            ),
        );
        if project.config.host_owner_public {
            // The choice has no effect here, so it is not offered; the stored
            // value is sent back unchanged.
            b.push_str(&format!(
                "<tr><th scope=\"row\">Your computers</th><td>This project shows every computer with its \
                 owner's name, so there is nothing to choose.{}</td></tr>\n",
                if user.show_hosts != 0 {
                    "<input type=\"hidden\" name=\"show_hosts\" value=\"1\">"
                } else {
                    ""
                }
            ));
        } else {
            label_row(
                &mut b,
                "show_hosts",
                "Should this project show your computers on its web site?",
                &format!(
                    "<input type=\"checkbox\" id=\"show_hosts\" name=\"show_hosts\" value=\"1\"{}>",
                    checked(flag("show_hosts", user.show_hosts))
                ),
            );
        }
        let venue_now = sent.map_or(user.venue.as_str(), |q| str_param(q, "default_venue"));
        let mut select = String::from("<select id=\"default_venue\" name=\"default_venue\">");
        for v in ["", "home", "school", "work"] {
            let sel = if venue_now == v { " selected" } else { "" };
            let label = if v.is_empty() { "---" } else { v };
            select.push_str(&format!("<option value=\"{v}\"{sel}>{label}</option>"));
        }
        select.push_str("</select>");
        label_row(
            &mut b,
            "default_venue",
            "Default computer location",
            &select,
        );
    }
    b.push_str(&format!(
        "</table>\n<p><input type=\"submit\" value=\"Update preferences\"></p>\n</form>\n\
         <p><a href=\"prefs.php?subset={}\">Back to preferences</a></p>\n",
        subset.name()
    ));
    SitePage::html(
        if bad.is_empty() { 200 } else { 400 },
        layout(project, &venue_title(subset, venue), &b),
    )
}

/// `prefs_edit.php`: the form by GET, the change by POST. A venue that has
/// no separate set yet starts from the main set, as upstream's
/// `add_venue.php`. Saving the computing preferences sets their
/// `<mod_time>` to now. Saving the main project preferences also sets
/// `send_email`, `show_hosts` and the default venue for new computers.
pub async fn prefs_edit(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    let (Some(subset), Ok(venue)) = (subset_param(q), venue_param(q)) else {
        return Ok(error_page(project, 400, "bad subset or venue"));
    };
    let Some(user) = viewer(ctx).await? else {
        return Ok(to_login(
            ctx.base,
            &format!("prefs.php?subset={}", subset.name()),
        ));
    };
    let form = PrefsForm::new(subset, project);
    let mut prefs = form.parse(stored_prefs(&user, subset));
    let current = match venue {
        Some(i) => prefs.venues[i]
            .clone()
            .unwrap_or_else(|| prefs.main.clone()),
        None => prefs.main.clone(),
    };
    if !ctx.post {
        return Ok(prefs_form_page(
            ctx,
            &user,
            &form,
            subset,
            venue,
            &current,
            None,
            &[],
        ));
    }
    if !token_ok(ctx, &user, q) {
        return Ok(error_page(project, 403, TIMED_OUT));
    }
    let set = match form.parse_form(&current, q) {
        Ok(set) => set,
        Err(bad) => {
            return Ok(prefs_form_page(
                ctx,
                &user,
                &form,
                subset,
                venue,
                &current,
                Some(q),
                &bad,
            ));
        }
    };
    match venue {
        Some(i) => prefs.venues[i] = Some(set),
        None => prefs.main = set,
    }
    let xml = form.write(&prefs, ctx.now);
    if xml.len() > crate::prefs::MAX_PREFS_BYTES {
        return Ok(error_page(
            project,
            400,
            "These preferences are too long to store.",
        ));
    }
    // The new document is the one read with the account plus this edit, so
    // it replaces only that document: if another request changed it
    // meanwhile (another venue's set, say), this one would revert that edit.
    let observed = stored_prefs(&user, subset);
    let update = match (subset, venue) {
        (Subset::Global, _) => stmt!(
            "UPDATE user SET global_prefs = ? WHERE id = ? AND global_prefs IS ? \
             AND authenticator = ? RETURNING id",
            xml.as_str(),
            user.id,
            observed,
            user.authenticator.as_str()
        ),
        (Subset::Project, Some(_)) => stmt!(
            "UPDATE user SET project_prefs = ? WHERE id = ? AND project_prefs IS ? \
             AND authenticator = ? RETURNING id",
            xml.as_str(),
            user.id,
            observed,
            user.authenticator.as_str()
        ),
        (Subset::Project, None) => {
            let default_venue = str_param(q, "default_venue");
            if !matches!(default_venue, "" | "home" | "school" | "work") {
                return Ok(error_page(project, 400, "bad venue"));
            }
            let flag = |k: &str| i32::from(q.get(k).is_some_and(|v| !v.is_empty()));
            stmt!(
                "UPDATE user SET project_prefs = ?, send_email = ?, show_hosts = ?, venue = ? \
                 WHERE id = ? AND project_prefs IS ? AND authenticator = ? RETURNING id",
                xml.as_str(),
                flag("send_email"),
                flag("show_hosts"),
                default_venue,
                user.id,
                observed,
                user.authenticator.as_str()
            )
        }
    };
    if ctx.db.run(update).await?.rows.is_empty() {
        return Ok(prefs_conflict(project));
    }
    Ok(SitePage::redirect(format!(
        "{}prefs.php?subset={}&updated=1",
        ctx.base,
        subset.name()
    )))
}

/// `prefs_remove.php`: a confirmation by GET, the removal of one venue's
/// set by POST.
pub async fn prefs_remove(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    let (Some(subset), Ok(Some(venue))) = (subset_param(q), venue_param(q)) else {
        return Ok(error_page(project, 400, "bad subset or venue"));
    };
    let Some(user) = viewer(ctx).await? else {
        return Ok(to_login(
            ctx.base,
            &format!("prefs.php?subset={}", subset.name()),
        ));
    };
    let name = VENUES[venue];
    if !ctx.post {
        let b = format!(
            "<p>Are you sure you want to delete your separate {} preferences for {name}?</p>\n\
             <form method=\"post\" action=\"prefs_remove.php\">\n{}\
             <input type=\"hidden\" name=\"subset\" value=\"{}\">\n\
             <input type=\"hidden\" name=\"venue\" value=\"{name}\">\n\
             <p><input type=\"submit\" value=\"Remove preferences\"> \
             <a href=\"prefs.php?subset={}\">Cancel</a></p>\n</form>\n",
            subset.title().to_lowercase(),
            token_fields(&user, ctx.now),
            subset.name(),
            subset.name()
        );
        return Ok(page(project, "Confirm delete preferences", &b));
    }
    if !token_ok(ctx, &user, q) {
        return Ok(error_page(project, 403, TIMED_OUT));
    }
    let form = PrefsForm::new(subset, project);
    let mut prefs = form.parse(stored_prefs(&user, subset));
    if prefs.venues[venue].take().is_some() {
        let xml = form.write(&prefs, ctx.now);
        let col = match subset {
            Subset::Global => "global_prefs",
            Subset::Project => "project_prefs",
        };
        let updated = ctx
            .db
            .run(stmt!(
                format!(
                    "UPDATE user SET {col} = ? WHERE id = ? AND {col} IS ? \
                     AND authenticator = ? RETURNING id"
                ),
                xml.as_str(),
                user.id,
                stored_prefs(&user, subset),
                user.authenticator.as_str()
            ))
            .await?;
        if updated.rows.is_empty() {
            return Ok(prefs_conflict(project));
        }
    }
    Ok(SitePage::redirect(format!(
        "{}prefs.php?subset={}",
        ctx.base,
        subset.name()
    )))
}
