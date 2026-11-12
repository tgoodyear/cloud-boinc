//! Account deletion: `delete_account_request.php`, `delete_account_confirm.php`
//! and the bounded steps that remove what refers to the account afterwards.
//! They follow upstream `html/user/delete_account_request.php`,
//! `delete_account_confirm.php`, `html/inc/delete_account.inc` and
//! `send_confirm_delete_email` in `html/inc/email.inc`.
//!
//! `enable_delete_account` in the config (see [`crate::Config`]) chooses the
//! method, as upstream: 1 obfuscates the account, 2 wipes it. The request page
//! checks the password and mails a link; the link's page asks for the password
//! again and deletes.
//!
//! **Bounded work.** D1 binds at most 100 values to a statement and allows
//! about 50 queries to an invocation, and an account can have many computers
//! and hundreds of thousands of tasks. The deletion is therefore in two parts:
//!
//! 1. One guarded batch ([`start_deletion`], run by the confirm page) that
//!    needs the live link token, ends the login (the account key and email
//!    address become `deleted_<time>_<random>`), clears the personal fields,
//!    removes every token of the account and queues the account in the
//!    `user_deletion` table. The batch is one transaction, so an account is
//!    either untouched or logged out, cleared and queued.
//! 2. Steps ([`delete_account_steps`]) that handle at most
//!    [`TASKS_PER_STATEMENT`] tasks and [`HOSTS_PER_STATEMENT`] computers per
//!    statement, with no bound parameter per row. The confirm page runs a few;
//!    the pipeline Worker's cron runs them until the queue is empty.

use cb_crypto::account::{
    constant_time_eq, passwd_hash, random_hex_string, token_hash, verify_peppered_passwd_hash,
};
use cb_db::{query, stmt, Db, Stmt};
use cb_proto::consts::{
    ASSIGN_HOST, ASSIGN_USER, RESULT_OUTCOME_CLIENT_DETACHED, RESULT_OUTCOME_SUCCESS,
    RESULT_SERVER_STATE_IN_PROGRESS, RESULT_SERVER_STATE_OVER, VALIDATE_STATE_INCONCLUSIVE,
    VALIDATE_STATE_INIT, VALIDATE_STATE_INVALID,
};
use serde::Deserialize;

use crate::model::User;
use crate::site::{error_page, layout, str_param, to_login, viewer, Params, SiteCtx, SitePage};
use crate::site_account::{token_fields, token_ok, TIMED_OUT};
use crate::site_email::{self, is_token, Sent, DELETE, MAIL_INTERVAL_SECS};
use crate::web::{html_escape, random_bytes, EMAIL_CHANGE_WAIT_SECS};
use crate::{CoreError, CoreResult};

/// Tasks one statement of a step detaches or cancels.
pub const TASKS_PER_STATEMENT: i64 = 500;
/// Computers one statement of a step anonymizes or deletes.
pub const HOSTS_PER_STATEMENT: i64 = 100;
/// `enable_delete_account` values.
pub const METHOD_OBFUSCATE: i32 = 1;
pub const METHOD_WIPE: i32 = 2;
/// Statements the confirm page spends on steps before it answers; the cron
/// takes the rest.
const INLINE_STATEMENTS: usize = 14;
/// Queue rows one pass of [`delete_account_steps`] looks at.
const QUEUE_ROWS: i64 = 5;

/// The first statement of a batch that writes on behalf of an authenticated
/// account (the scheduler's report, host and claim writes): it fails, and
/// with it the whole batch, unless the account still has the key the request
/// authenticated with and is not blocked. A deletion changes the key and sets
/// `blocked` in one transaction (`start_deletion`), so a write that was
/// authenticated before it and commits after it does nothing. `abs` of the
/// smallest integer is an error in SQLite and in D1 ("integer overflow"),
/// which is how a statement stops a batch; the reply is "Invalid or missing
/// account key", see [`is_guard_failure`].
pub(crate) fn account_guard(id: i64, authenticator: &str) -> Stmt {
    stmt!(
        "SELECT CASE WHEN EXISTS (SELECT 1 FROM user WHERE id = ? AND authenticator = ? \
         AND blocked = 0) THEN 1 ELSE abs(-9223372036854775807 - 1) END",
        id,
        authenticator
    )
}

/// Whether `e` is the failure of [`account_guard`].
pub(crate) fn is_guard_failure(e: &CoreError) -> bool {
    matches!(e, CoreError::Db(cb_db::DbError::Backend(m)) if m.contains("integer overflow"))
}

const DISABLED: &str = "This feature is disabled. Please contact the project administrator.";
const BAD_LINK: &str = "The token you used has expired or is otherwise not valid. Please \
                        request a new one on the delete account page.";

fn page(ctx: &SiteCtx<'_>, status: u16, title: &str, body: &str) -> SitePage {
    SitePage::html(status, layout(ctx.project, title, body))
}

/// Whether `given` is the account's password, or, for an account with no
/// password, its account key. Upstream's `check_passwd_ui` takes the password
/// only; an account made by an account manager may have none.
fn proves_password(pepper: &[u8], user: &User, given: &str) -> bool {
    if user.passwd_hash.is_empty() {
        return !given.is_empty()
            && constant_time_eq(user.authenticator.as_bytes(), given.as_bytes());
    }
    verify_peppered_passwd_hash(
        pepper,
        &passwd_hash(given, &user.email_addr),
        &user.passwd_hash,
    )
}

fn password_input() -> &'static str {
    "<p><label>Password (the account key, for an account with no password)<br>\
     <input type=\"password\" name=\"passwd\" autocomplete=\"current-password\"></label></p>\n"
}

fn changed_recently(user: &User, now: i64) -> bool {
    user.email_addr_change_time + EMAIL_CHANGE_WAIT_SECS > now
}

// ---------------------------------------------------------------------------
// The request.

/// `delete_account_request.php`. Logged in, a GET describes the process and a
/// POST with the form token and the password mails the confirmation link.
/// Refused within seven days of an address change, as upstream.
pub async fn delete_account_request(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    if project.config.enable_delete_account == 0 {
        return Ok(error_page(project, 404, DISABLED));
    }
    let Some(user) = viewer(ctx).await? else {
        return Ok(to_login(ctx.base, "delete_account_request.php"));
    };
    if changed_recently(&user, ctx.now) {
        return Ok(error_page(
            project,
            403,
            "You are not allowed to delete your account until after 7 days from when you \
             last changed your email address.",
        ));
    }
    let Some(mail) = ctx.mail() else {
        return Ok(error_page(
            project,
            503,
            "This project does not send email, so it cannot send the link that confirms \
             deleting an account. Please contact the project administrator.",
        ));
    };
    if !ctx.post {
        return Ok(page(
            ctx,
            200,
            "Delete account",
            &format!(
                "<p>You can delete your account. This <strong>cannot be undone</strong>.</p>\n\
                 <ul>\n\
                 <li>Enter your password below and click \"Send confirmation email\".</li>\n\
                 <li>You will get an email with a link. Open it.</li>\n\
                 <li>On that page, enter your password again and click \"Delete account\".</li>\n\
                 <li>Your account is then deleted at once.{}</li>\n</ul>\n\
                 <form method=\"post\" action=\"delete_account_request.php\">\n{}{}\
                 <p><input type=\"submit\" value=\"Send confirmation email\"></p>\n</form>\n",
                if project.config.enable_delete_account == METHOD_WIPE {
                    " Your computers are removed and your tasks are detached from them."
                } else {
                    " Your name, email address and location are removed and your computers \
                     lose their names and addresses; your credit stays in the project's totals."
                },
                token_fields(&user, ctx.now),
                password_input()
            ),
        ));
    }
    if !token_ok(ctx, &user, q) {
        return Ok(error_page(project, 403, TIMED_OUT));
    }
    let Some(pepper) = ctx.pepper else {
        return Ok(error_page(
            project,
            503,
            "Account deletion is not available now.",
        ));
    };
    if !proves_password(
        pepper,
        &user,
        q.get("passwd").map(String::as_str).unwrap_or(""),
    ) {
        return Ok(error_page(project, 403, "Invalid password."));
    }
    Ok(
        match site_email::send_delete_confirmation(ctx, &mail, &user).await? {
            Sent::Delivered => page(
                ctx,
                200,
                "Delete account request",
                &format!(
                    "<p>The email to confirm your request to delete your account has been \
                     sent to {}. Its link works for one day.</p>\n",
                    html_escape(&user.email_addr)
                ),
            ),
            Sent::Recent => page(
                ctx,
                429,
                "Delete account request",
                &format!(
                    "<p>A message was sent a few minutes ago. Wait {} minutes from then \
                     before asking for another.</p>\n",
                    MAIL_INTERVAL_SECS / 60
                ),
            ),
            Sent::Failed => error_page(
                project,
                503,
                &format!(
                    "Can't send confirmation email to {}. Please try again later.",
                    user.email_addr
                ),
            ),
            Sent::Undeliverable => error_page(
                project,
                400,
                &format!(
                    "{} Change the address of your account on the account page, then ask again.",
                    crate::mail_guard::CANNOT_RECEIVE
                ),
            ),
        },
    )
}

// ---------------------------------------------------------------------------
// The confirmation.

fn form_page(ctx: &SiteCtx<'_>, id: i64, token: &str) -> SitePage {
    page(
        ctx,
        200,
        "Delete account",
        &format!(
            "<p>Thank you for verifying ownership of your account.</p>\n\
             <p>You can now delete it by entering your password below and clicking \"Delete \
             account\". As a reminder, your account <strong>cannot be recovered</strong> once \
             you delete it.</p>\n\
             <form method=\"post\" action=\"delete_account_confirm.php\">\n\
             <input type=\"hidden\" name=\"id\" value=\"{id}\">\n\
             <input type=\"hidden\" name=\"token\" value=\"{}\">\n{}\
             <p><input type=\"submit\" class=\"danger\" value=\"Delete account\"></p>\n</form>\n",
            html_escape(token),
            password_input()
        ),
    )
}

/// `delete_account_confirm.php?id=&token=`. The link in the message needs no
/// login, as upstream's. A GET shows the form if the link is still good; the
/// POST with the password deletes the account. A wrong password does not use
/// the link up.
pub async fn delete_account_confirm(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    let method = project.config.enable_delete_account;
    if method == 0 {
        return Ok(error_page(project, 404, DISABLED));
    }
    let token = str_param(q, "token");
    let Some(id) = str_param(q, "id")
        .parse::<i64>()
        .ok()
        .filter(|_| is_token(token))
    else {
        return Ok(error_page(project, 400, BAD_LINK));
    };
    let hash = token_hash(token);
    let live = ctx
        .db
        .batch(vec![
            stmt!(
                "SELECT id FROM token WHERE token_hash = ? AND userid = ? AND type = ? \
                 AND used_time = 0 AND expire_time > ?",
                hash.as_str(),
                id,
                DELETE.code,
                ctx.now
            ),
            stmt!("SELECT * FROM user WHERE id = ? AND blocked = 0", id),
        ])
        .await?;
    let user: Option<User> = live
        .get(1)
        .cloned()
        .map(|r| r.first())
        .transpose()?
        .flatten();
    let (true, Some(user)) = (live.first().is_some_and(|r| !r.rows.is_empty()), user) else {
        return Ok(error_page(project, 403, BAD_LINK));
    };
    if !ctx.post {
        return Ok(form_page(ctx, id, token));
    }
    let Some(pepper) = ctx.pepper else {
        return Ok(error_page(
            project,
            503,
            "Account deletion is not available now.",
        ));
    };
    if !proves_password(
        pepper,
        &user,
        q.get("passwd").map(String::as_str).unwrap_or(""),
    ) {
        return Ok(error_page(project, 403, "Invalid password."));
    }
    if changed_recently(&user, ctx.now) {
        return Ok(error_page(
            project,
            403,
            "You are not allowed to delete your account until after 7 days from when you \
             last changed your email address.",
        ));
    }
    let x = format!(
        "deleted_{}_{}",
        ctx.now,
        random_hex_string(&random_bytes(ctx.random))
    );
    if !start_deletion(ctx.db, &user, &hash, &x, method, ctx.now).await? {
        return Ok(error_page(
            project,
            409,
            "The account changed meanwhile, or the link was used; please try again.",
        ));
    }
    // Some of the work now, so that a small account is gone when the page
    // answers; a failure here leaves the rest to the cron.
    let _ = delete_account_steps(ctx.db, ctx.now, INLINE_STATEMENTS).await;
    Ok(page(
        ctx,
        200,
        "Account deleted",
        &format!(
            "<p>Your account has been deleted. If you want to contribute to {} in the future \
             you will need to create a new account.</p>\n\
             <p>The project removes the computers and tasks of an account in the background; \
             an account with many may take a few minutes.</p>\n",
            html_escape(&project.config.long_name)
        ),
    )
    .with_logout(ctx.base))
}

impl SitePage {
    fn with_logout(mut self, base: &str) -> SitePage {
        self.set_cookie = Some(crate::site::logout_cookie(base));
        self
    }
}

/// The batch that starts a deletion: only if the account is still the one
/// that was read (`user`), is not too recent a change of address, and the
/// `token` (by hash) is still unused and live, it ends the login, clears the
/// personal fields, removes the account's tokens and queues it. Later
/// statements run only when the update before them changed a row (`changes()`
/// refers to the statement just before), so a refused update stores nothing.
/// `x` is the value the account key and address become. `true` when it ran.
pub async fn start_deletion(
    db: &dyn Db,
    user: &User,
    token_hash: &str,
    x: &str,
    method: i32,
    now: i64,
) -> CoreResult<bool> {
    let out = db
        .batch(vec![
            // The password hash and the weak key go with the account key.
            stmt!(
                "UPDATE user SET authenticator = ?, email_addr = ?, name = 'deleted', \
                 country = '', postal_code = '', url = '', global_prefs = '', project_prefs = '', \
                 passwd_hash = '', venue = '', send_email = 0, show_hosts = 0, email_validated = 0, \
                 previous_email_addr = '', cross_project_id = '', teamid = 0, blocked = 1 \
                 WHERE id = ? AND authenticator = ? AND email_addr = ? AND passwd_hash = ? \
                 AND email_addr_change_time = ? AND blocked = 0 \
                 AND EXISTS (SELECT 1 FROM token WHERE token_hash = ? AND userid = ? AND type = ? \
                 AND used_time = 0 AND expire_time > ?) RETURNING id",
                x,
                x,
                user.id,
                user.authenticator.as_str(),
                user.email_addr.as_str(),
                user.passwd_hash.as_str(),
                user.email_addr_change_time,
                token_hash,
                user.id,
                DELETE.code,
                now
            ),
            stmt!(
                "INSERT INTO user_deletion (userid, mode, create_time, gen) \
                 SELECT ?, ?, ?, ? WHERE changes() > 0 AND EXISTS \
                 (SELECT 1 FROM user WHERE id = ? AND authenticator = ?) \
                 ON CONFLICT (userid) DO NOTHING",
                user.id,
                method,
                now,
                x,
                user.id,
                x
            ),
            stmt!(
                "DELETE FROM token WHERE userid = ? AND EXISTS \
                 (SELECT 1 FROM user_deletion WHERE userid = ? AND gen = ?) \
                 AND EXISTS (SELECT 1 FROM user WHERE id = ? AND authenticator = ?)",
                user.id,
                user.id,
                x,
                user.id,
                x
            ),
            // Teams the account founded have no founder, as `user_erase_team_owner`
            // in `html/inc/team.inc` leaves a team (it does this for the team the
            // account is a member of; this covers every team it founded). The
            // member count moves by the trigger on `user.teamid`.
            stmt!(
                "UPDATE team SET userid = 0 WHERE userid = ? AND EXISTS \
                 (SELECT 1 FROM user_deletion WHERE userid = ? AND gen = ?) \
                 AND EXISTS (SELECT 1 FROM user WHERE id = ? AND authenticator = ?)",
                user.id,
                user.id,
                x,
                user.id,
                x
            ),
        ])
        .await?;
    Ok(out.first().is_some_and(|r| !r.rows.is_empty()))
}

// ---------------------------------------------------------------------------
// The steps.

#[derive(Deserialize)]
struct Queued {
    userid: i64,
    mode: i32,
    /// The queue row's identity: what the account's key was set to.
    gen: String,
}

/// What a pass of [`delete_account_steps`] did.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct DeletionReport {
    /// Accounts whose deletion finished in the pass.
    pub finished: u64,
    /// Accounts still queued, which have more to do.
    pub pending: u64,
    /// Statements run, queue reads included.
    pub statements: usize,
}

/// A task a user has, as upstream's `is_in_progress` or
/// `is_over_but_not_validated` sees it: it is cancelled (the workunit is
/// reconsidered) instead of only being detached.
fn cancelable() -> String {
    format!(
        "(server_state = {RESULT_SERVER_STATE_IN_PROGRESS} OR (server_state = {RESULT_SERVER_STATE_OVER} \
         AND outcome = {RESULT_OUTCOME_SUCCESS} AND validate_state IN ({VALIDATE_STATE_INIT}, \
         {VALIDATE_STATE_INCONCLUSIVE})))"
    )
}

/// Runs queued deletions until `max_statements` statements have been spent or
/// the queue is empty. Each account is handled in this order: its tasks
/// (wipe only, as upstream), then its computers, then the account row (wipe)
/// or the queue row (obfuscate). A round of the first two is one batch of
/// statements that each touch at most [`TASKS_PER_STATEMENT`] or
/// [`HOSTS_PER_STATEMENT`] rows, whatever the size of the account. Safe to run
/// concurrently and to repeat: every statement matches only rows that still
/// need it, and the last one only fires when nothing refers to the account.
pub async fn delete_account_steps(
    db: &dyn Db,
    now: i64,
    max_statements: usize,
) -> CoreResult<DeletionReport> {
    let mut report = DeletionReport::default();
    if max_statements == 0 {
        return Ok(report);
    }
    let queue: Vec<Queued> = query(
        db,
        stmt!(
            "SELECT userid, mode, gen FROM user_deletion ORDER BY userid LIMIT ?",
            QUEUE_ROWS
        ),
    )
    .await?;
    report.statements += 1;
    let mut spent = 1;
    for q in queue {
        let (mut done, mut gone) = (false, false);
        while spent + MAX_STEP_STATEMENTS <= max_statements {
            let (n, progress) = step(db, &q, now).await?;
            spent += n;
            match progress {
                Progress::More => {}
                Progress::Done => {
                    done = true;
                    break;
                }
                Progress::Gone => {
                    gone = true;
                    break;
                }
            }
        }
        if done {
            report.finished += 1;
        } else if !gone {
            report.pending += 1;
        }
        if spent + MAX_STEP_STATEMENTS > max_statements {
            break;
        }
    }
    report.statements = spent;
    Ok(report)
}

/// Most statements one call of [`step`] runs: a round of eight and the last
/// batch of five.
const MAX_STEP_STATEMENTS: usize = 13;

/// Statements [`delete_account_steps`] needs to make progress on a queued
/// deletion: the queue read and one step.
pub const DELETION_PROGRESS_STATEMENTS: usize = 1 + MAX_STEP_STATEMENTS;

/// The condition every statement of a step ends with: this deletion is still
/// the one the pass read (the queue row exists with the same `gen`) and its
/// account is still the blocked one that carries it. Two bound values: the
/// user ID and `gen`. A pass that read the queue row before another pass
/// finished it (the confirm page and the cron can overlap), or that is slow
/// enough to see the account deleted, finds no row and every statement does
/// nothing, so what it would delete or cancel is never something that belongs to
/// anyone else, an account that was given the same ID included (IDs are not
/// reused, see `id_floor`, but this does not depend on it).
const LIVE: &str = "EXISTS (SELECT 1 FROM user_deletion d WHERE d.userid = ? AND d.gen = ? \
     AND EXISTS (SELECT 1 FROM user u WHERE u.id = d.userid AND u.blocked = 1 \
     AND u.authenticator = d.gen))";

/// How a round ended.
#[derive(Debug, PartialEq)]
enum Progress {
    /// There is more to do.
    More,
    /// This round finished the deletion.
    Done,
    /// The queue row was not there any more (another pass finished it): this
    /// pass has nothing to do and did nothing.
    Gone,
}

/// One round for one account: `(statements run, how it ended)`.
///
/// Wipe: one batch of eight statements handles a window of tasks, a chunk of
/// assignments and a chunk of computers.
///
/// - The window is the next `TASKS_PER_STATEMENT` rows of the account's tasks,
///   by ID, whatever state they are in: `WHERE id IN (SELECT id FROM result
///   WHERE userid = ? AND userid > 0 ORDER BY id LIMIT ?)` reads that many
///   entries of `res_user` and no more. The statements that use it do not change
///   which rows it holds until the last of them, which gives every task of the
///   window `userid` 0, so they leave the index and the next window is the next
///   rows. There is no search for tasks of a state: a task that is in progress
///   or waiting for validation is cancelled by a `CASE` in the same `UPDATE`
///   that detaches it (`server_state` over, outcome `CLIENT_DETACHED`, invalid,
///   as `cancel_results_for_user`), and the others are only detached, so each
///   round reads the same number of rows and the total work is linear in the
///   tasks.
/// - The workunits of the window get `rev + 1` (and the cancelable ones are made
///   due), before the tasks change, so a transition that read the tasks earlier
///   cannot commit over the change (`transition` writes a workunit only `WHERE
///   rev = ?`).
/// - Assignments that target the account, and those that target a computer of
///   the chunk, are deleted.
/// - A computer of the chunk is deleted only when no assignment and no task names
///   it, so that no row is left that names an ID that is gone (checked in the
///   statement, not by the order of the batch: a task that still names it keeps
///   it for a later round).
///
/// When no statement filled its window or chunk there is probably nothing
/// left, and the account row is tried at once: it goes only if no task,
/// computer or assignment refers to it, with the queue row and the tokens, and
/// the `batch` rows it submitted are given no owner. The tables that name a
/// user or computer ID are `host.userid`, `result.userid`, `result.hostid`,
/// `host_app_version.host_id`, `assignment.target_id` (for both target types),
/// `batch.user_id` and `token.userid`; each is dealt with above.
///
/// Obfuscate: a chunk of computers loses its names and addresses
/// (`anonymize_hosts`; `external_ip_addr` is an address of the same kind, so it
/// goes too); tasks are left, as upstream leaves them; then the queue row. The
/// rows stay.
async fn step(db: &dyn Db, q: &Queued, now: i64) -> CoreResult<(usize, Progress)> {
    let (uid, gen) = (q.userid, q.gen.as_str());
    if q.mode != METHOD_WIPE {
        // A window of the account's computers after the cursor, whatever their
        // state, and the cursor moved to its last one in the same batch (the
        // window does not depend on what the first statement changes, so both
        // statements take the same one). The anonymized computers stay in
        // `host_user`, so without the cursor each pass would read them again.
        let window = format!(
            "SELECT id FROM host WHERE userid = ? AND id > {{cursor}} ORDER BY id LIMIT {HOSTS_PER_STATEMENT}"
        );
        let out = db
            .batch(vec![
                stmt!(
                    format!(
                        "UPDATE host SET domain_name = 'deleted', last_ip_addr = '', external_ip_addr = '' \
                         WHERE id IN ({}) AND {LIVE}",
                        window.replace(
                            "{cursor}",
                            "(SELECT host_cursor FROM user_deletion WHERE userid = ? AND gen = ?)"
                        )
                    ),
                    uid,
                    uid,
                    gen,
                    uid,
                    gen
                ),
                stmt!(
                    format!(
                        "UPDATE user_deletion SET host_cursor = coalesce( \
                         (SELECT max(id) FROM ({})), host_cursor) \
                         WHERE userid = ? AND gen = ? AND EXISTS (SELECT 1 FROM user u \
                         WHERE u.id = userid AND u.blocked = 1 AND u.authenticator = gen)",
                        window.replace("{cursor}", "host_cursor")
                    ),
                    uid,
                    uid,
                    gen
                ),
            ])
            .await?;
        if out[0].changes >= HOSTS_PER_STATEMENT as u64 {
            return Ok((2, Progress::More));
        }
        let gone = db
            .run(stmt!(
                "DELETE FROM user_deletion WHERE userid = ? AND gen = ?",
                uid,
                gen
            ))
            .await?
            .changes
            == 0;
        return Ok((3, if gone { Progress::Gone } else { Progress::Done }));
    }
    let c = cancelable();
    let window = format!(
        "SELECT id FROM result WHERE userid = ? AND userid > 0 ORDER BY id LIMIT {TASKS_PER_STATEMENT}"
    );
    let host_chunk =
        format!("SELECT id FROM host WHERE userid = ? ORDER BY id LIMIT {HOSTS_PER_STATEMENT}");
    let out = db
        .batch(vec![
            // 0: the workunits of the cancelable tasks of the window: due.
            stmt!(
                format!(
                    "UPDATE workunit SET transition_time = ? WHERE id IN \
                     (SELECT workunitid FROM result WHERE id IN ({window}) AND {c}) AND {LIVE}"
                ),
                now,
                uid,
                uid,
                gen
            ),
            // 1: the workunits of the window: a new revision.
            stmt!(
                format!(
                    "UPDATE workunit SET rev = rev + 1 WHERE id IN \
                     (SELECT workunitid FROM result WHERE id IN ({window})) AND {LIVE}"
                ),
                uid,
                uid,
                gen
            ),
            // 2: the window: cancel what is cancelable, detach all.
            stmt!(
                format!(
                    "UPDATE result SET \
                     server_state = CASE WHEN {c} THEN {RESULT_SERVER_STATE_OVER} ELSE server_state END, \
                     outcome = CASE WHEN {c} THEN {RESULT_OUTCOME_CLIENT_DETACHED} ELSE outcome END, \
                     validate_state = CASE WHEN {c} THEN {VALIDATE_STATE_INVALID} ELSE validate_state END, \
                     hostid = 0, userid = 0 WHERE id IN ({window}) AND {LIVE}"
                ),
                uid,
                uid,
                gen
            ),
            // 3: assignments to the account.
            stmt!(
                format!(
                    "DELETE FROM assignment WHERE id IN (SELECT id FROM assignment \
                     WHERE target_type = ? AND target_id = ? ORDER BY id LIMIT ?) AND {LIVE}"
                ),
                ASSIGN_USER,
                uid,
                TASKS_PER_STATEMENT,
                uid,
                gen
            ),
            // 4: assignments to the computers of the chunk.
            stmt!(
                format!(
                    "DELETE FROM assignment WHERE id IN (SELECT id FROM assignment \
                     WHERE target_type = ? AND target_id IN ({host_chunk}) LIMIT ?) \
                     AND {LIVE}"
                ),
                ASSIGN_HOST,
                uid,
                TASKS_PER_STATEMENT,
                uid,
                gen
            ),
            // 5: whatever task still names a computer of the chunk, of any
            // user (a task written off at a report keeps both IDs; this one
            // keeps the computer from being kept for ever), by `res_host`.
            stmt!(
                format!(
                    "UPDATE result SET hostid = 0 WHERE id IN (SELECT id FROM result \
                     WHERE hostid > 0 AND hostid IN ({host_chunk}) LIMIT ?) AND {LIVE}"
                ),
                uid,
                TASKS_PER_STATEMENT,
                uid,
                gen
            ),
            // 6, 7: the computers of the chunk that nothing names.
            stmt!(
                format!("DELETE FROM host_app_version WHERE host_id IN ({host_chunk}) AND {LIVE}"),
                uid,
                uid,
                gen
            ),
            stmt!(
                format!(
                    "DELETE FROM host WHERE id IN ({host_chunk}) \
                     AND NOT EXISTS (SELECT 1 FROM assignment WHERE target_type = ? AND target_id = host.id) \
                     AND NOT EXISTS (SELECT 1 FROM result WHERE hostid = host.id AND hostid > 0) \
                     AND {LIVE}"
                ),
                uid,
                ASSIGN_HOST,
                uid,
                gen
            ),
        ])
        .await?;
    let (tasks, hosts) = (TASKS_PER_STATEMENT as u64, HOSTS_PER_STATEMENT as u64);
    if out[2].changes >= tasks
        || out[3].changes >= tasks
        || out[4].changes >= tasks
        || out[5].changes >= tasks
        || out[7].changes >= hosts
    {
        return Ok((8, Progress::More));
    }
    let out = db
        .batch(vec![
            stmt!(
                format!("UPDATE batch SET user_id = 0 WHERE user_id = ? AND user_id > 0 AND {LIVE}"),
                uid,
                uid,
                gen
            ),
            stmt!(
                format!(
                    "DELETE FROM user WHERE id = ? \
                     AND NOT EXISTS (SELECT 1 FROM host WHERE userid = ?) \
                     AND NOT EXISTS (SELECT 1 FROM result WHERE userid = ? AND userid > 0) \
                     AND NOT EXISTS (SELECT 1 FROM assignment WHERE target_type = ? AND target_id = ?) \
                     AND {LIVE}"
                ),
                uid,
                uid,
                uid,
                ASSIGN_USER,
                uid,
                uid,
                gen
            ),
            // The row is gone, and the queue row is still there: this
            // deletion's own.
            stmt!(
                "DELETE FROM token WHERE userid = ? \
                 AND NOT EXISTS (SELECT 1 FROM user WHERE id = ?) \
                 AND EXISTS (SELECT 1 FROM user_deletion WHERE userid = ? AND gen = ?)",
                uid,
                uid,
                uid,
                gen
            ),
            stmt!(
                "DELETE FROM user_deletion WHERE userid = ? AND gen = ? \
                 AND NOT EXISTS (SELECT 1 FROM user WHERE id = ?)",
                uid,
                gen,
                uid
            ),
            // Is the queue row still this one's? Not if the account row went
            // and the queue row with it (done), or if another pass finished it
            // before this round began (gone).
            stmt!(
                "SELECT 1 FROM user_deletion WHERE userid = ? AND gen = ?",
                uid,
                gen
            ),
        ])
        .await?;
    Ok((
        13,
        if out[1].changes > 0 {
            Progress::Done
        } else if out[4].rows.is_empty() {
            Progress::Gone
        } else {
            Progress::More
        },
    ))
}
