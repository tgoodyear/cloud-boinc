//! Site pages that send email: password recovery (`get_passwd.php`,
//! `mail_passwd.php` and the login link that `login_action.php` takes),
//! address verification (`validate_email_addr.php`), and the notices of an
//! address change with `recover_email.php` to undo it. They follow upstream
//! `html/user/{get_passwd,mail_passwd,login_action,validate_email_addr,
//! edit_email_action,recover_email}.php` and `html/inc/{email,token}.inc`.
//!
//! Every page here needs a mailer and `email_from` ([`SiteCtx::mail`]); with
//! neither it answers that the project does not send email. Links in a
//! message are built from the master URL of the config only. A link carries a
//! random token; the database keeps its SHA-256 (the `token` table).

use cb_crypto::account::{pepper_passwd_hash, random_hex_string, token_hash};
use cb_db::stmt;

use crate::mail::Mail;
use crate::model::User;
use crate::site::{
    error_page, layout, login_cookie, logout_cookie, str_param, time_str, to_login, viewer, Params,
    SiteCtx, SitePage,
};
use crate::site_account::{token_fields, token_ok, TIMED_OUT};
use crate::web::{html_escape, is_valid_email, random_bytes};
use crate::CoreResult;

/// Seconds between two messages of one kind to one account (login links,
/// verification).
pub const MAIL_INTERVAL_SECS: i64 = 600;
/// How long a login link and a verification link work, as upstream's
/// "valid for 1 day".
pub const LINK_SECS: i64 = 86400;
/// How long the previous address can undo an address change, and the wait
/// before the next change (`TOKEN_DURATION_ONE_WEEK`, and the `604800` in
/// upstream's `edit_email_action.php`).
pub const EMAIL_CHANGE_WAIT_SECS: i64 = 604800;
/// Rows of the `token` table are kept this long after they were made, then
/// removed by whatever issues the next token.
const TOKEN_KEEP_SECS: i64 = EMAIL_CHANGE_WAIT_SECS + 86400;
/// Expired rows removed by one issue.
const TOKEN_CLEANUP_ROWS: i64 = 20;

/// A kind of token: what its row says, how long it works, how soon another
/// may be issued to the same account, and whether a new one replaces the
/// older ones that are still unused.
pub(crate) struct TokenKind {
    code: &'static str,
    lifetime: i64,
    interval: i64,
    supersede: bool,
}

/// A login link.
pub(crate) const LOGIN: TokenKind = TokenKind {
    code: "L",
    lifetime: LINK_SECS,
    interval: MAIL_INTERVAL_SECS,
    supersede: true,
};
/// An address verification link. Opening it twice does no harm, so an older
/// link keeps working.
pub(crate) const VERIFY: TokenKind = TokenKind {
    code: "V",
    lifetime: LINK_SECS,
    interval: MAIL_INTERVAL_SECS,
    supersede: false,
};
/// The link that undoes an address change. The seven-day wait between
/// changes is its limit.
pub(crate) const CHANGE: TokenKind = TokenKind {
    code: "E",
    lifetime: EMAIL_CHANGE_WAIT_SECS,
    interval: 0,
    supersede: true,
};

fn new_token(random: &dyn Fn() -> u32) -> String {
    format!(
        "{}{}",
        random_hex_string(&random_bytes(random)),
        random_hex_string(&random_bytes(random))
    )
}

/// Whether `s` has the form of a token this module issues.
fn is_token(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Makes a token for `userid` and stores its hash, in one batch: the rows
/// kept too long go, the row is added unless this account had a token of the
/// kind within `interval` (the limit on mail), and older unused tokens of the
/// kind stop working if the kind replaces them. `data` is stored with the
/// row (see `migrations/0004_email.sql`). `None` when the limit applied.
pub(crate) async fn issue_token(
    ctx: &SiteCtx<'_>,
    userid: i64,
    kind: &TokenKind,
    data: &str,
) -> CoreResult<Option<String>> {
    let token = new_token(ctx.random);
    let hash = token_hash(&token);
    let mut batch = vec![
        stmt!(
            "DELETE FROM token WHERE id IN \
             (SELECT id FROM token WHERE create_time <= ? LIMIT ?)",
            ctx.now - TOKEN_KEEP_SECS,
            TOKEN_CLEANUP_ROWS
        ),
        stmt!(
            "INSERT INTO token (userid, type, token_hash, data, create_time, expire_time) \
             SELECT ?, ?, ?, ?, ?, ? WHERE NOT EXISTS \
             (SELECT 1 FROM token WHERE userid = ? AND type = ? AND create_time > ?) \
             RETURNING id",
            userid,
            kind.code,
            hash.as_str(),
            data,
            ctx.now,
            ctx.now + kind.lifetime,
            userid,
            kind.code,
            ctx.now - kind.interval
        ),
    ];
    if kind.supersede {
        batch.push(stmt!(
            "UPDATE token SET used_time = ? WHERE userid = ? AND type = ? AND used_time = 0 \
             AND token_hash != ? AND EXISTS (SELECT 1 FROM token WHERE token_hash = ?)",
            ctx.now,
            userid,
            kind.code,
            hash.as_str(),
            hash.as_str()
        ));
    }
    let out = ctx.db.batch(batch).await?;
    let issued = out.get(1).is_some_and(|r| !r.rows.is_empty());
    Ok(issued.then_some(token))
}

/// Removes a token whose message could not be sent, so that the account is
/// not held to the limit for a message it never got.
async fn revoke_token(ctx: &SiteCtx<'_>, token: &str) -> CoreResult<()> {
    ctx.db
        .run(stmt!(
            "DELETE FROM token WHERE token_hash = ?",
            token_hash(token).as_str()
        ))
        .await?;
    Ok(())
}

fn no_mail_page(ctx: &SiteCtx<'_>) -> SitePage {
    SitePage::html(
        404,
        layout(
            ctx.project,
            "Email",
            "<p>This project does not send email, so this page is not available.</p>\n",
        ),
    )
}

fn page(ctx: &SiteCtx<'_>, status: u16, title: &str, body: &str) -> SitePage {
    SitePage::html(status, layout(ctx.project, title, body))
}

fn name(ctx: &SiteCtx<'_>) -> String {
    ctx.project.config.long_name.clone()
}

/// What `home.php` adds to the email address row when the site sends email:
/// whether the address is verified, or the button that sends the message.
pub(crate) fn verify_cell(user: &User, now: i64) -> String {
    if user.email_validated != 0 {
        return " <small>(verified)</small>".into();
    }
    format!(
        " <small>(not verified)</small>\n\
         <form method=\"post\" action=\"validate_email_addr.php\" style=\"display:inline\">\n{}\
         <input type=\"submit\" value=\"Send verification email\"></form>",
        token_fields(user, now)
    )
}

// ---------------------------------------------------------------------------
// Password recovery.

/// `get_passwd.php`: asks for the address.
pub fn get_passwd(ctx: &SiteCtx<'_>) -> SitePage {
    if ctx.mail().is_none() {
        return no_mail_page(ctx);
    }
    page(
        ctx,
        200,
        "Reset password",
        "<p>Enter your account's email address below, and click OK. You will be sent \
         instructions for logging in and resetting your password.</p>\n\
         <form method=\"post\" action=\"mail_passwd.php\">\n\
         <p><label>Email address<br><input type=\"email\" name=\"email_addr\" autocomplete=\"username\" maxlength=\"254\"></label></p>\n\
         <p><input type=\"submit\" value=\"OK\"></p>\n</form>\n",
    )
}

/// `mail_passwd.php`: mails a login link to the address of an account.
///
/// Unlike upstream, the answer is the same whether or not an account has the
/// address (and whether or not the message was sent), and an account gets at
/// most one message every [`MAIL_INTERVAL_SECS`]; a request inside that time
/// sends nothing and gets the same answer. A blocked account is sent nothing.
pub async fn mail_passwd(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let Some(mail) = ctx.mail() else {
        return Ok(no_mail_page(ctx));
    };
    let project = ctx.project;
    if !ctx.post {
        return Ok(error_page(
            project,
            405,
            "Use the form to reset a password.",
        ));
    }
    let email = str_param(q, "email_addr").to_lowercase();
    if email.is_empty() {
        return Ok(error_page(project, 400, "No address given."));
    }
    if !is_valid_email(&email) {
        return Ok(error_page(
            project,
            400,
            "Please enter a valid address of the form name@xxx.yyy",
        ));
    }
    let user: Option<User> = ctx
        .db
        .run(stmt!(
            "SELECT * FROM user WHERE email_addr = ? AND blocked = 0",
            email.as_str()
        ))
        .await?
        .first()?;
    if let Some(user) = user {
        if let Some(token) = issue_token(ctx, user.id, &LOGIN, "").await? {
            let n = name(ctx);
            let body = format!(
                "This email was sent in response to a request on the {n} web site.\n\n\
                 To log in to your {n} account, visit:\n\
                 {master}login_action.php?id={id}&t={token}\n\
                 (This link is valid for 1 day and works once.)\n\
                 After logging in, you can change your account's password or email address.\n\n\
                 If you did not ask for this, you can ignore this message: nobody can log in \
                 without the link.\n\n\
                 For further information and assistance with {n}, visit\n{master}\n",
                master = project.config.master_url,
                id = user.id,
            );
            if mail
                .send(&user.email_addr, &format!("{n} account information"), &body)
                .await
                .is_err()
            {
                revoke_token(ctx, &token).await?;
            }
        }
    }
    Ok(page(
        ctx,
        200,
        "Email sent",
        &format!(
            "<p>If an account has the email address {}, instructions for logging in and \
             resetting your password have been sent to it. At most one message is sent \
             every {} minutes.</p>\n\
             <p>If the email doesn't arrive in a few minutes, check your spam folder.</p>\n",
            html_escape(&email),
            MAIL_INTERVAL_SECS / 60
        ),
    ))
}

const BAD_LINK: &str = "This login link is not valid: it may have expired or been used, or \
                        your email program may have cut it short.";

/// `login_action.php?id=&t=`: the login link of a recovery message.
///
/// Upstream's link has a timestamp and an MD5 of it with the account key, and
/// works any number of times in its day. Here `t` is a random token that
/// works once. A GET only shows a button (if the link is still good), and the
/// POST of that button logs in, so a program that fetches the links in a
/// message (a mail scanner) does not use the token up. As upstream, the cookie lasts a year, and the
/// browser goes to the account page.
pub async fn login_via_link(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    if ctx.mail().is_none() {
        return Ok(no_mail_page(ctx));
    }
    let project = ctx.project;
    let t = str_param(q, "t");
    let id = str_param(q, "id").parse::<i64>().ok();
    let bad = || {
        page(
            ctx,
            403,
            "Login link",
            &format!(
                "<p>{}</p>\n<p><a href=\"get_passwd.php\">Get a new login link by email</a></p>\n",
                html_escape(BAD_LINK)
            ),
        )
    };
    let (Some(id), true) = (id, is_token(t)) else {
        return Ok(bad());
    };
    if !ctx.post {
        let live = ctx
            .db
            .run(stmt!(
                "SELECT id FROM token WHERE token_hash = ? AND userid = ? AND type = ? \
                 AND used_time = 0 AND expire_time > ?",
                token_hash(t).as_str(),
                id,
                LOGIN.code,
                ctx.now
            ))
            .await?;
        if live.rows.is_empty() {
            return Ok(bad());
        }
        return Ok(page(
            ctx,
            200,
            "Log in",
            &format!(
                "<form method=\"post\" action=\"login_action.php\">\n\
                 <input type=\"hidden\" name=\"id\" value=\"{id}\">\n\
                 <input type=\"hidden\" name=\"t\" value=\"{}\">\n\
                 <p>Log in to your account with this link.</p>\n\
                 <p><input type=\"submit\" value=\"Log in\"></p>\n</form>\n",
                html_escape(t)
            ),
        ));
    }
    let used = ctx
        .db
        .run(stmt!(
            "UPDATE token SET used_time = ? WHERE token_hash = ? AND userid = ? AND type = ? \
             AND used_time = 0 AND expire_time > ? RETURNING id",
            ctx.now,
            token_hash(t).as_str(),
            id,
            LOGIN.code,
            ctx.now
        ))
        .await?;
    if used.rows.is_empty() {
        return Ok(bad());
    }
    let user: Option<User> = ctx
        .db
        .run(stmt!("SELECT * FROM user WHERE id = ? AND blocked = 0", id))
        .await?
        .first()?;
    let Some(user) = user else {
        return Ok(bad());
    };
    let mut out = SitePage::redirect(format!("{}home.php", project.config.master_url));
    out.set_cookie = Some(login_cookie(project, &user.authenticator, true));
    Ok(out)
}

// ---------------------------------------------------------------------------
// Address verification.

/// What came of sending a verification message.
#[derive(Debug, PartialEq)]
pub(crate) enum Sent {
    Delivered,
    /// One was sent within [`MAIL_INTERVAL_SECS`]; nothing was sent now.
    Recent,
    Failed,
}

/// Mails the account the link that verifies its current address, unless it
/// had one within [`MAIL_INTERVAL_SECS`].
pub(crate) async fn send_verification(
    ctx: &SiteCtx<'_>,
    mail: &Mail<'_>,
    user: &User,
) -> CoreResult<Sent> {
    let Some(token) = issue_token(ctx, user.id, &VERIFY, &user.email_addr).await? else {
        return Ok(Sent::Recent);
    };
    let n = name(ctx);
    let body = format!(
        "Please visit the following link to validate the email address of your {n} account:\n\
         {}validate_email_addr.php?validate=1&u={}&x={token}\n\
         (This link is valid for 1 day.)\n",
        ctx.project.config.master_url, user.id
    );
    if mail
        .send(&user.email_addr, "Validate BOINC email address", &body)
        .await
        .is_err()
    {
        revoke_token(ctx, &token).await?;
        return Ok(Sent::Failed);
    }
    Ok(Sent::Delivered)
}

/// `validate_email_addr.php`. Logged in, a GET shows the button and a POST
/// with the form token sends the message (upstream sends on a GET). The link
/// in the message, `?validate=1&u=&x=`, needs no login, as upstream; it sets
/// `email_validated` while the account still has the address the message went
/// to, and works again until it expires.
pub async fn validate_email_addr(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let Some(mail) = ctx.mail() else {
        return Ok(no_mail_page(ctx));
    };
    let project = ctx.project;
    if !str_param(q, "validate").is_empty() {
        let x = str_param(q, "x");
        let Some(u) = str_param(q, "u")
            .parse::<i64>()
            .ok()
            .filter(|_| is_token(x))
        else {
            return Ok(error_page(
                project,
                400,
                "Error in URL data - can't validate email address",
            ));
        };
        let done = ctx
            .db
            .run(stmt!(
                "UPDATE user SET email_validated = 1 WHERE id = ? AND email_addr = \
                 (SELECT data FROM token WHERE token_hash = ? AND userid = ? AND type = ? \
                 AND used_time = 0 AND expire_time > ?) RETURNING id",
                u,
                token_hash(x).as_str(),
                u,
                VERIFY.code,
                ctx.now
            ))
            .await?;
        if done.rows.is_empty() {
            return Ok(error_page(
                project,
                403,
                "This link is not valid, has expired, or is for an address the account no \
                 longer has. Log in and ask for a new message.",
            ));
        }
        return Ok(page(
            ctx,
            200,
            "Validate email address",
            "<p>The email address of your account has been validated.</p>\n",
        ));
    }
    let Some(user) = viewer(ctx).await? else {
        return Ok(to_login(project, "validate_email_addr.php"));
    };
    if user.email_validated != 0 {
        return Ok(page(
            ctx,
            200,
            "Validate email address",
            "<p>The email address of your account is already validated.</p>\n",
        ));
    }
    if !ctx.post {
        return Ok(page(
            ctx,
            200,
            "Validate email address",
            &format!(
                "<form method=\"post\" action=\"validate_email_addr.php\">\n{}\
                 <p>Send a message with a link that validates {}.</p>\n\
                 <p><input type=\"submit\" value=\"Send verification email\"></p>\n</form>\n",
                token_fields(&user, ctx.now),
                html_escape(&user.email_addr)
            ),
        ));
    }
    if !token_ok(ctx, &user, q) {
        return Ok(error_page(project, 403, TIMED_OUT));
    }
    Ok(match send_verification(ctx, &mail, &user).await? {
        Sent::Delivered => page(
            ctx,
            200,
            "Validate email address",
            &format!(
                "<p>An email has been sent to {}. Visit the link it contains to validate your \
                 email address.</p>\n<p>If you don't receive this email, check your spam folder.</p>\n",
                html_escape(&user.email_addr)
            ),
        ),
        Sent::Recent => page(
            ctx,
            429,
            "Validate email address",
            &format!(
                "<p>A message was sent to {} a few minutes ago. Wait {} minutes from then \
                 before asking for another.</p>\n",
                html_escape(&user.email_addr),
                MAIL_INTERVAL_SECS / 60
            ),
        ),
        Sent::Failed => error_page(
            project,
            503,
            &format!("Can't send email to {}.", user.email_addr),
        ),
    })
}

// ---------------------------------------------------------------------------
// Address change.

/// The messages of an address change that `edit_email_action.php` has made:
/// a notice to the new address, and to the previous one the link that undoes
/// the change. `true` when both were sent.
pub(crate) async fn send_change_notices(
    ctx: &SiteCtx<'_>,
    mail: &Mail<'_>,
    userid: i64,
    previous: &str,
    new: &str,
) -> CoreResult<bool> {
    let n = name(ctx);
    let subject = format!("{n} email address change.");
    let to_new = format!(
        "Your email address was changed from {previous} to {new} on {}. You will not be able to \
         change your email address again until {}. If you need to undo this immediately, \
         please look for an email from us at your {previous} address.\n",
        time_str(ctx.now),
        time_str(ctx.now + EMAIL_CHANGE_WAIT_SECS)
    );
    let sent_new = mail.send(new, &subject, &to_new).await.is_ok();
    let Some(token) = issue_token(ctx, userid, &CHANGE, previous).await? else {
        return Ok(false);
    };
    let to_old = format!(
        "Your email address has been changed. If you did not take this action,\n\
         then please visit the link below to reverse this process and change your password.\n\n\
         {}recover_email.php?id={userid}&token={token}\n\n\
         Note: Your password will need to be recovered after using this link.\n\
         (This link is valid for 7 days and works once.)\n",
        ctx.project.config.master_url
    );
    let sent_old = mail.send(previous, &subject, &to_old).await.is_ok();
    if !sent_old {
        revoke_token(ctx, &token).await?;
    }
    Ok(sent_new && sent_old)
}

/// `recover_email.php?id=&token=`: puts the previous address back, within
/// seven days of a change. As upstream, the password becomes one nobody knows
/// (the account is then used through a login link, which asks nothing, or its
/// account key) and the address is not verified. A GET only shows a button (if the
/// link is still good) and the POST of it makes the change, so that a program fetching the links in a
/// message cannot undo a change. The link works once. The change also ends
/// every login link and verification link the account had, and the browser is
/// logged out, as upstream.
pub async fn recover_email(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    if ctx.mail().is_none() {
        return Ok(no_mail_page(ctx));
    }
    let project = ctx.project;
    let token = str_param(q, "token");
    let Some(id) = str_param(q, "id")
        .parse::<i64>()
        .ok()
        .filter(|_| is_token(token))
    else {
        return Ok(error_page(project, 400, "Invalid token."));
    };
    if !ctx.post {
        let live = ctx
            .db
            .run(stmt!(
                "SELECT id FROM token WHERE token_hash = ? AND userid = ? AND type = ? \
                 AND used_time = 0 AND expire_time > ?",
                token_hash(token).as_str(),
                id,
                CHANGE.code,
                ctx.now
            ))
            .await?;
        if live.rows.is_empty() {
            return Ok(error_page(project, 403, "Invalid token."));
        }
        return Ok(page(
            ctx,
            200,
            "Recover email address",
            &format!(
                "<form method=\"post\" action=\"recover_email.php\">\n\
                 <input type=\"hidden\" name=\"id\" value=\"{id}\">\n\
                 <input type=\"hidden\" name=\"token\" value=\"{}\">\n\
                 <p>Change the email address of the account back to the one it had before \
                 the last change. You will then need to reset the password.</p>\n\
                 <p><input type=\"submit\" value=\"Recover email address\"></p>\n</form>\n",
                html_escape(token)
            ),
        ));
    }
    let Some(pepper) = ctx.pepper else {
        return Ok(error_page(
            project,
            503,
            "Email changes are not available now.",
        ));
    };
    let unusable = pepper_passwd_hash(pepper, &random_hex_string(&random_bytes(ctx.random)));
    let hash = token_hash(token);
    let out = ctx
        .db
        .batch(vec![
            stmt!(
                "UPDATE user SET email_addr = previous_email_addr, previous_email_addr = '', \
                 email_addr_change_time = 0, passwd_hash = ?, email_validated = 0 \
                 WHERE id = ? AND previous_email_addr != '' \
                 AND NOT EXISTS (SELECT 1 FROM user o WHERE o.email_addr = user.previous_email_addr) \
                 AND EXISTS (SELECT 1 FROM token t WHERE t.token_hash = ? AND t.userid = ? \
                 AND t.type = ? AND t.used_time = 0 AND t.expire_time > ? \
                 AND t.data = user.previous_email_addr) RETURNING id",
                unusable.as_str(),
                id,
                hash.as_str(),
                id,
                CHANGE.code,
                ctx.now
            ),
            // The links that other types of token made stay valid only if the
            // change was not undone; this runs while the token is unused.
            stmt!(
                "UPDATE token SET used_time = ? WHERE userid = ? AND used_time = 0 AND type != ? \
                 AND EXISTS (SELECT 1 FROM user WHERE id = ? AND previous_email_addr = '') \
                 AND EXISTS (SELECT 1 FROM token t WHERE t.token_hash = ? AND t.userid = ? \
                 AND t.type = ? AND t.used_time = 0 AND t.expire_time > ?)",
                ctx.now,
                id,
                CHANGE.code,
                id,
                hash.as_str(),
                id,
                CHANGE.code,
                ctx.now
            ),
            stmt!(
                "UPDATE token SET used_time = ? WHERE token_hash = ? AND userid = ? AND type = ? \
                 AND used_time = 0 AND expire_time > ? \
                 AND EXISTS (SELECT 1 FROM user WHERE id = ? AND previous_email_addr = '')",
                ctx.now,
                hash.as_str(),
                id,
                CHANGE.code,
                ctx.now,
                id
            ),
        ])
        .await?;
    if !out.first().is_some_and(|r| !r.rows.is_empty()) {
        return Ok(error_page(
            project,
            403,
            "Invalid token. If it was valid, another account may now have the previous \
             address; please contact the project.",
        ));
    }
    let mut done = page(
        ctx,
        200,
        "Recover email address",
        "<p>Email address has been reverted.</p>\n\
         <p>You need to reset your password: <a href=\"get_passwd.php\">get a login link by email</a>.</p>\n",
    );
    done.set_cookie = Some(logout_cookie(project));
    Ok(done)
}
