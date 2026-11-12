//! The account manager RPCs and the site pages against SQLite: who may see
//! what, escaping of values that came from clients, paging, and how many
//! statements each page runs.

use std::cell::{Cell, RefCell};

use async_trait::async_trait;
use cb_core::mail::{MailError, MailMessage, Mailer};
use cb_core::project::{put_setting, Project, SETTING_CONFIG};
use cb_core::site::{
    site_page, Params, SiteCtx, SitePage, RESULTS_FILTER_WINDOW, RESULTS_PER_PAGE,
};
use cb_core::web::{am_get_info, am_set_host_info, make_user, weak_account_key, NewAccount};
use cb_crypto::account::passwd_hash;
use cb_db::{stmt, Db, DbResult, Rows, Stmt};
use cb_sqlite::SqliteDb;
use futures_lite::future::block_on;

const PEPPER: &[u8] = b"pepper for tests";
const CONFIG: &str = "<config>\n<long_name>Test Project</long_name>\n<master_url>https://p.example/</master_url>\n<download_url>https://p.example/download</download_url>\n<upload_url>https://p.example/upload/file_upload_handler</upload_url>\n<user_country>1</user_country>\n</config>\n";
const NOW: i64 = 2_000_000_000;
/// The same project with a sender address, so that the site sends email.
const MAIL_CONFIG: &str = "<config>\n<long_name>Test Project</long_name>\n<master_url>https://p.example/</master_url>\n<download_url>https://p.example/download</download_url>\n<upload_url>https://p.example/upload/file_upload_handler</upload_url>\n<user_country>1</user_country>\n<account_creation>1</account_creation>\n<email_from>boinc@p.example</email_from>\n<email_from_name>Test Project</email_from_name>\n</config>\n";

/// A transport that keeps what it is given.
#[derive(Default)]
struct Capture {
    sent: RefCell<Vec<MailMessage>>,
    fail: Cell<bool>,
}

#[async_trait(?Send)]
impl Mailer for Capture {
    async fn send(&self, message: &MailMessage) -> Result<(), MailError> {
        if self.fail.get() {
            return Err(MailError::Send("refused".into()));
        }
        self.sent.borrow_mut().push(message.clone());
        Ok(())
    }
}

struct World {
    db: SqliteDb,
    seq: Cell<u32>,
    project: Project,
    mail: Capture,
    /// Whether pages get the transport.
    use_mail: Cell<bool>,
    now: Cell<i64>,
}

struct Account {
    id: i64,
    auth: String,
}

impl World {
    fn new() -> World {
        World::with_config(CONFIG)
    }

    fn with_config(config: &str) -> World {
        let db = SqliteDb::with_schema().unwrap();
        block_on(put_setting(&db, SETTING_CONFIG, config, 10)).unwrap();
        block_on(db.run(stmt!(
            "INSERT INTO platform (id, create_time, name, user_friendly_name) \
             VALUES (1, 1, 'x86_64-pc-linux-gnu', 'Linux')"
        )))
        .unwrap();
        block_on(db.run(stmt!(
            "INSERT INTO app (id, create_time, name, user_friendly_name) VALUES (1, 1, 'worker', 'Worker <one>')"
        )))
        .unwrap();
        block_on(db.run(stmt!(
            "INSERT INTO app (id, create_time, name, user_friendly_name, target_nresults) \
             VALUES (2, 1, 'adaptive', 'Adaptive', 2)"
        )))
        .unwrap();
        block_on(db.run(stmt!(
            "INSERT INTO app_version (id, create_time, appid, version_num, platformid, xml_doc, plan_class) \
             VALUES (1, 1, 1, 102, 1, '', 'mt')"
        )))
        .unwrap();
        let project = block_on(Project::load(&db)).unwrap();
        World {
            db,
            seq: Cell::new(0),
            project,
            mail: Capture::default(),
            use_mail: Cell::new(false),
            now: Cell::new(NOW),
        }
    }

    /// A project that sends email through a capturing transport.
    fn with_mail() -> World {
        let w = World::with_config(MAIL_CONFIG);
        w.use_mail.set(true);
        w
    }

    /// The messages sent so far.
    fn mails(&self) -> Vec<MailMessage> {
        self.mail.sent.borrow().clone()
    }

    /// Moves the clock to `secs` after the start.
    fn at(&self, secs: i64) {
        self.now.set(NOW + secs);
    }

    fn rand(&self) -> u32 {
        self.seq.set(self.seq.get().wrapping_add(2_654_435_761));
        self.seq.get()
    }

    fn account(&self, email: &str, password: &str, name: &str) -> Account {
        let random = || self.rand();
        let hash = passwd_hash(password, email);
        let auth = block_on(make_user(
            &self.db,
            PEPPER,
            100,
            &random,
            &NewAccount {
                email_addr: email,
                passwd_hash: &hash,
                user_name: "someone",
                ..Default::default()
            },
        ))
        .unwrap()
        .unwrap();
        // Names with markup cannot be made through the RPC; older accounts
        // and imports can still hold them.
        let id = block_on(self.db.run(stmt!(
            "UPDATE user SET name = ? WHERE authenticator = ? RETURNING id",
            name,
            auth.as_str()
        )))
        .unwrap()
        .scalar_i64()
        .unwrap();
        Account { id, auth }
    }

    fn sql(&self, sql: &str) {
        block_on(self.db.run(stmt!(sql))).unwrap();
    }

    fn host(&self, userid: i64, domain: &str, rpc_time: i64) -> i64 {
        block_on(self.db.run(stmt!(
            "INSERT INTO host (create_time, userid, rpc_time, domain_name, last_ip_addr, \
             external_ip_addr, p_vendor, p_model, p_ncpus, os_name, os_version, m_nbytes, d_total, \
             serialnum) VALUES (1, ?, ?, ?, '192.0.2.7', '198.51.100.7', 'Vendor', \
             'Model <script>alert(1)</script>', 4, 'Linux', '6.1', 8e9, 1e11, \
             '[BOINC|8.0.4][CUDA|1|8192MB]') RETURNING id",
            userid,
            rpc_time,
            domain
        )))
        .unwrap()
        .scalar_i64()
        .unwrap()
    }

    fn workunit(&self, name: &str, appid: i64) -> i64 {
        let doc = "<file_info>\n<name>in_1</name>\n<url>https://p.example/download/in_1</url>\n\
                   <nbytes>12</nbytes>\n</file_info>\n<workunit>\n<file_ref>\n<file_name>in_1</file_name>\n\
                   <open_name>in&lt;put</open_name>\n</file_ref>\n<command_line>--n 1</command_line>\n</workunit>\n";
        block_on(self.db.run(stmt!(
            "INSERT INTO workunit (create_time, appid, name, xml_doc, rsc_fpops_est, rsc_fpops_bound, \
             rsc_memory_bound, rsc_disk_bound, transition_time, delay_bound, min_quorum, \
             target_nresults, max_error_results, max_total_results, max_success_results, \
             result_template) VALUES (1, ?, ?, ?, 1e12, 1e13, 1e8, 1e8, 2147483647, 3600, 1, 1, \
             3, 5, 3, '') RETURNING id",
            appid,
            name,
            doc
        )))
        .unwrap()
        .scalar_i64()
        .unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    fn result(
        &self,
        wuid: i64,
        name: &str,
        hostid: i64,
        userid: i64,
        server_state: i32,
        stderr: &str,
    ) -> i64 {
        block_on(self.db.run(stmt!(
            "INSERT INTO result (create_time, workunitid, server_state, outcome, hostid, userid, \
             sent_time, report_deadline, name, xml_doc_in, stderr_out, appid, app_version_id, \
             app_version_num, validate_state, elapsed_time, cpu_time) \
             VALUES (1, ?, ?, ?, ?, ?, 1000, 5000, ?, '', ?, 1, 1, 102, 1, 120, 100) RETURNING id",
            wuid,
            server_state,
            if server_state == 5 { 1 } else { 0 },
            hostid,
            userid,
            name,
            stderr
        )))
        .unwrap()
        .scalar_i64()
        .unwrap()
    }

    /// One page, and the statements it ran.
    fn page(
        &self,
        path: &str,
        auth: Option<&str>,
        post: bool,
        params: &[(&str, &str)],
    ) -> (SitePage, u64) {
        self.page_on(&self.db, path, auth, post, params)
    }

    /// The same, through another handle on the database.
    fn page_on(
        &self,
        db: &dyn Db,
        path: &str,
        auth: Option<&str>,
        post: bool,
        params: &[(&str, &str)],
    ) -> (SitePage, u64) {
        let q: Params = params
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let random = || self.rand();
        let ctx = SiteCtx {
            db,
            project: &self.project,
            auth,
            post,
            pepper: Some(PEPPER),
            now: self.now.get(),
            random: &random,
            mailer: self.use_mail.get().then_some(&self.mail as &dyn Mailer),
        };
        let before = self.db.statements();
        let page = block_on(site_page(&ctx, path, &q)).unwrap().unwrap();
        (page, self.db.statements() - before)
    }

    fn get(&self, path: &str, auth: Option<&str>, params: &[(&str, &str)]) -> SitePage {
        self.page(path, auth, false, params).0
    }

    fn post(&self, path: &str, auth: Option<&str>, params: &[(&str, &str)]) -> SitePage {
        self.page(path, auth, true, params).0
    }

    /// The form fields of a page, as a browser would send them back, with
    /// `changes` applied.
    fn submit(
        &self,
        form_page: &SitePage,
        action: &str,
        auth: Option<&str>,
        changes: &[(&str, &str)],
    ) -> (SitePage, u64) {
        let mut q = hidden_fields(&form_page.body);
        for (k, v) in changes {
            q.push((k.to_string(), v.to_string()));
        }
        let pairs: Vec<(&str, &str)> = q.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        self.page(action, auth, true, &pairs)
    }

    fn user(&self, id: i64) -> cb_core::model::User {
        block_on(cb_db::query_one(
            &self.db,
            stmt!("SELECT * FROM user WHERE id = ?", id),
        ))
        .unwrap()
        .unwrap()
    }
}

/// The hidden inputs of a form.
fn hidden_fields(html: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for part in html.split("<input type=\"hidden\" ").skip(1) {
        let attr = |name: &str| {
            let key = format!("{name}=\"");
            let at = part.find(&key)? + key.len();
            Some(part[at..at + part[at..].find('"')?].to_string())
        };
        if let (Some(k), Some(v)) = (attr("name"), attr("value")) {
            out.push((k, v));
        }
    }
    out
}

fn error_num(xml: &str) -> Option<i32> {
    let start = xml.find("<error_num>")? + "<error_num>".len();
    let end = xml[start..].find("</error_num>")? + start;
    xml[start..end].parse().ok()
}

#[test]
fn am_get_info_takes_the_account_key_only() {
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada & Co <x>");
    w.sql("UPDATE user SET country = 'United Kingdom', venue = 'home', show_hosts = 1");
    let reply = block_on(am_get_info(&w.db, &ada.auth)).unwrap();
    assert!(reply.starts_with("<?xml"));
    assert!(reply.contains("<am_get_info_reply>\n    <success/>\n"));
    assert!(reply.contains(&format!("<id>{}</id>", ada.id)));
    assert!(reply.contains("<name>Ada+%26+Co+%3Cx%3E</name>"));
    assert!(reply.contains("<country>United+Kingdom</country>"));
    assert!(reply.contains("<show_hosts>1</show_hosts>"));
    assert!(reply.contains("<venue>home</venue>"));
    // A non-ASCII venue stays UTF-8 under a UTF-8 declaration.
    assert!(reply.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\""));
    w.sql("UPDATE user SET venue = 'Jos\u{e9} \u{65e5}\u{672c}'");
    let reply = block_on(am_get_info(&w.db, &ada.auth)).unwrap();
    assert!(reply.contains("<venue>Jos\u{e9} \u{65e5}\u{672c}</venue>"));
    let user: cb_core::model::User = block_on(cb_db::query_one(
        &w.db,
        stmt!("SELECT * FROM user WHERE id = ?", ada.id),
    ))
    .unwrap()
    .unwrap();
    let weak = weak_account_key(&user);
    assert!(reply.contains(&format!("<weak_auth>{weak}</weak_auth>")));

    // The weak key, a wrong key and a blocked account are not found.
    for key in [weak.as_str(), "0123456789abcdef0123456789abcdef", ""] {
        assert_eq!(
            error_num(&block_on(am_get_info(&w.db, key)).unwrap()),
            Some(-136)
        );
    }
    w.sql("UPDATE user SET blocked = 1");
    assert_eq!(
        error_num(&block_on(am_get_info(&w.db, &ada.auth)).unwrap()),
        Some(-136)
    );
}

fn am_set_info(w: &World, params: &[(&str, &str)]) -> (String, u64) {
    let q: Params = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let before = w.db.statements();
    let reply = block_on(cb_core::web::am_set_info(
        &w.db,
        &w.project.config,
        PEPPER,
        w.now.get(),
        &q,
    ))
    .unwrap();
    (reply, w.db.statements() - before)
}

#[test]
fn am_set_info_changes_the_account() {
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let bob = w.account("bob@example.com", "secret", "Bob");
    w.sql(
        "UPDATE user SET project_prefs = '<project_preferences>\n<project_specific>\n<x>1</x>\n</project_specific>\n</project_preferences>\n'",
    );
    let global = "<global_preferences>\n<mod_time>1900000000</mod_time>\n<max_ncpus_pct>50</max_ncpus_pct>\n</global_preferences>\n";
    let project =
        "<project_preferences>\n<resource_share>7</resource_share>\n</project_preferences>\n";
    let new_hash = passwd_hash("newer", "ada@new.example");
    let (reply, n) = am_set_info(
        &w,
        &[
            ("account_key", &ada.auth),
            ("name", "Ada L"),
            ("country", "United Kingdom"),
            ("postal_code", "N1 9GU"),
            ("global_prefs", global),
            ("project_prefs", project),
            ("url", "https://ada.example/"),
            ("send_email", "1"),
            ("show_hosts", "1"),
            ("teamid", "5"),
            ("venue", "work"),
            ("email_addr", "Ada@New.example"),
            ("password_hash", &new_hash),
        ],
    );
    // The account, the update, and the end of the login links mailed before.
    assert_eq!(n, 3);
    assert!(
        reply.contains("<am_set_info_reply>\n    <success/>"),
        "{reply}"
    );
    let u = w.user(ada.id);
    assert_eq!(
        (u.name.as_str(), u.country.as_str(), u.postal_code.as_str()),
        ("Ada L", "United Kingdom", "N1 9GU")
    );
    assert_eq!(u.global_prefs, global);
    // The stored project-specific part is kept.
    assert!(u
        .project_prefs
        .contains("<project_specific>\n<x>1</x>\n</project_specific>"));
    assert!(u
        .project_prefs
        .contains("<resource_share>7</resource_share>"));
    assert_eq!((u.send_email, u.show_hosts, u.teamid), (1, 1, 0));
    assert_eq!(
        (u.url.as_str(), u.venue.as_str()),
        ("https://ada.example/", "work")
    );
    assert_eq!(u.email_addr, "ada@new.example");
    let found = block_on(cb_core::web::lookup_account(
        &w.db,
        PEPPER,
        "ada@new.example",
        &new_hash,
    ))
    .unwrap();
    assert!(found.contains(&ada.auth));

    // Empty fields change nothing; refused values change nothing either.
    let (reply, _) = am_set_info(&w, &[("account_key", &ada.auth), ("name", "")]);
    assert!(reply.contains("<success/>"));
    assert_eq!(w.user(ada.id).name, "Ada L");
    // The address was just changed: another change is refused for a week,
    // and after it the checks of the address itself are what refuse.
    let (reply, _) = am_set_info(
        &w,
        &[
            ("account_key", &ada.auth),
            ("email_addr", "ada@again.example"),
        ],
    );
    assert_eq!(error_num(&reply), Some(-1), "{reply}");
    assert_eq!(w.user(ada.id).email_addr, "ada@new.example");
    w.at(604_800);
    for (field, value, num) in [
        ("country", "Atlantis", -1),
        ("name", "<b>", -1),
        ("url", "javascript:alert(1)", -1),
        ("venue", "a<b", -1),
        (
            "global_prefs",
            "<global_preferences></global_preferences>",
            -112,
        ),
        ("project_prefs", "<x>\n</x>", -112),
        ("email_addr", "not an address", -205),
        ("password_hash", "short", -1),
        ("email_addr", "bob@example.com", -137),
    ] {
        let (reply, _) = am_set_info(
            &w,
            &[
                ("account_key", &ada.auth),
                ("show_hosts", "0"),
                (field, value),
            ],
        );
        assert_eq!(error_num(&reply), Some(num), "{field}: {reply}");
        assert_eq!(w.user(ada.id).show_hosts, 1, "{field}");
    }
    assert_eq!(w.user(bob.id).email_addr, "bob@example.com");

    // Only the account key itself is accepted.
    let weak = weak_account_key(&w.user(bob.id));
    for key in [weak.as_str(), "0123456789abcdef0123456789abcdef", ""] {
        let (reply, _) = am_set_info(&w, &[("account_key", key), ("name", "x")]);
        assert_eq!(error_num(&reply), Some(-136));
    }
    assert_eq!(w.user(bob.id).name, "Bob");
}

#[test]
fn am_set_host_info_changes_only_the_accounts_own_computers() {
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let bob = w.account("bob@example.com", "secret", "Bob");
    let mine = w.host(ada.id, "ada-box", NOW);
    let theirs = w.host(bob.id, "bob-box", NOW);
    let venue = |id: i64| {
        block_on(w.db.run(stmt!("SELECT venue FROM host WHERE id = ?", id)))
            .unwrap()
            .rows[0]["venue"]
            .as_str()
            .unwrap()
            .to_string()
    };

    let ok = block_on(am_set_host_info(
        &w.db,
        &ada.auth,
        &mine.to_string(),
        "work",
    ))
    .unwrap();
    assert!(ok.contains("<am_set_host_info_reply>\n    <success/>"));
    assert_eq!(venue(mine), "work");

    let other = block_on(am_set_host_info(
        &w.db,
        &ada.auth,
        &theirs.to_string(),
        "work",
    ))
    .unwrap();
    assert_eq!(error_num(&other), Some(-136));
    assert_eq!(venue(theirs), "");
    for (key, host) in [
        ("0123456789abcdef0123456789abcdef", mine.to_string()),
        ("", mine.to_string()),
        (ada.auth.as_str(), "x".to_string()),
    ] {
        let r = block_on(am_set_host_info(&w.db, key, &host, "school")).unwrap();
        assert_eq!(error_num(&r), Some(-136));
    }
    let bad = block_on(am_set_host_info(&w.db, &ada.auth, &mine.to_string(), "<b>")).unwrap();
    assert_eq!(error_num(&bad), Some(-1));
    assert_eq!(venue(mine), "work");
}

#[test]
fn show_user_xml_lists_computers_only_with_the_key() {
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada <b>");
    w.host(ada.id, "ada & box", NOW);
    let id = ada.id.to_string();
    let public = w.get(
        "/show_user.php",
        None,
        &[("userid", &id), ("format", "xml")],
    );
    assert_eq!(public.content_type, "text/xml; charset=utf-8");
    assert!(public
        .body
        .starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\""));
    assert!(public.body.contains("<name>Ada &lt;b></name>"));
    assert!(public.body.contains("<country></country>"));
    assert!(!public.body.contains("<host>"));
    let private = w.get(
        "/show_user.php",
        None,
        &[("auth", &ada.auth), ("format", "xml")],
    );
    assert!(private
        .body
        .contains("<domain_name>ada &amp; box</domain_name>"));
    let missing = w.get(
        "/show_user.php",
        None,
        &[("userid", "999"), ("format", "xml")],
    );
    assert_eq!(error_num(&missing.body), Some(-136));

    // Non-ASCII text is kept as UTF-8, not split into one reference per byte.
    let jose = w.account("jose@example.com", "secret", "Jos\u{e9} \u{65e5}\u{672c}");
    w.host(jose.id, "caf\u{e9}-pc", NOW);
    let reply = w.get(
        "/show_user.php",
        None,
        &[("auth", &jose.auth), ("format", "xml")],
    );
    assert!(reply
        .body
        .contains("<name>Jos\u{e9} \u{65e5}\u{672c}</name>"));
    assert!(reply
        .body
        .contains("<domain_name>caf\u{e9}-pc</domain_name>"));
    assert!(!reply.body.contains("&#195;"));

    let html = w.get("/show_user.php", None, &[("userid", &id)]);
    assert!(html.body.contains("Ada &lt;b&gt;"));
    assert!(!html.body.contains("ada@example.com"));
    assert!(!html.body.contains("hosts_user.php?userid"));
}

#[test]
fn login_sets_a_cookie_and_logout_clears_it() {
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada");

    let (by_key, n) = w.page(
        "/login_action.php",
        None,
        true,
        &[
            ("authenticator", &ada.auth),
            ("next_url", "results.php?userid=1"),
        ],
    );
    assert_eq!(n, 1);
    assert_eq!(by_key.status, 303);
    assert_eq!(
        by_key.location.as_deref(),
        Some("https://p.example/results.php?userid=1")
    );
    assert_eq!(
        by_key.set_cookie.as_deref(),
        Some(format!("auth={}; Path=/; HttpOnly; Secure; SameSite=Lax", ada.auth).as_str())
    );

    // The link form, by GET, and "stay logged in".
    let by_link = w.get(
        "/login_action.php",
        None,
        &[
            ("key", &ada.auth),
            ("stay_logged_in", "1"),
            ("next_url", "https://evil.example/"),
        ],
    );
    assert_eq!(
        by_link.location.as_deref(),
        Some("https://p.example/home.php")
    );
    assert!(by_link.set_cookie.unwrap().ends_with("; Max-Age=31536000"));

    // Email address and password, in a POST only.
    let by_password = w.page(
        "/login_action.php",
        None,
        true,
        &[("email_addr", "Ada@Example.com"), ("passwd", "secret")],
    );
    assert_eq!(by_password.0.status, 303);
    let wrong = w.page(
        "/login_action.php",
        None,
        true,
        &[("email_addr", "ada@example.com"), ("passwd", "nope")],
    );
    assert_eq!(wrong.0.status, 403);
    assert!(wrong.0.set_cookie.is_none());
    let in_query = w.get(
        "/login_action.php",
        None,
        &[("email_addr", "ada@example.com"), ("passwd", "secret")],
    );
    assert_eq!(in_query.status, 400);
    assert!(in_query.set_cookie.is_none());

    // The weak key cannot log in, and is not even looked up.
    let user: cb_core::model::User = block_on(cb_db::query_one(
        &w.db,
        stmt!("SELECT * FROM user WHERE id = ?", ada.id),
    ))
    .unwrap()
    .unwrap();
    let (weak, n) = w.page(
        "/login_action.php",
        None,
        true,
        &[("authenticator", &weak_account_key(&user))],
    );
    assert_eq!((weak.status, n), (403, 0));
    assert!(weak.set_cookie.is_none());

    // The account page's "Log out" form carries the token.
    let home = w.get("/home.php", Some(&ada.auth), &[]);
    let (out, _) = w.submit(&home, "/logout.php", Some(&ada.auth), &[]);
    assert_eq!(out.status, 303);
    assert_eq!(out.location.as_deref(), Some("https://p.example/"));
    assert!(out.set_cookie.unwrap().starts_with("auth=; Path=/;"));

    let form = w.get(
        "/login_form.php",
        None,
        &[("next_url", "home.php?x=\"><script>")],
    );
    assert!(form.body.contains("value=\"home.php\""));
    assert!(!form.body.contains("<script>"));
}

#[test]
fn logout_needs_a_post_a_login_and_the_form_token() {
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let bob = w.account("bob@example.com", "secret", "Bob");
    let home = w.get("/home.php", Some(&ada.auth), &[]);
    assert!(home
        .body
        .contains("<form method=\"post\" action=\"logout.php\">"));
    assert!(home.body.contains("name=\"ttok\""));
    let token = hidden_fields(&home.body);
    let pairs: Vec<(&str, &str)> = token
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    // A GET, even with a valid token in the query, does not log out.
    let by_get = w.get("/logout.php", Some(&ada.auth), &pairs);
    assert_eq!(by_get.status, 403);
    assert!(by_get.set_cookie.is_none());
    assert!(by_get.body.contains("Link has timed out"));

    // A POST without a token, or with another account's token, does not.
    let bare = w.post("/logout.php", Some(&ada.auth), &[]);
    assert_eq!(bare.status, 403);
    assert!(bare.set_cookie.is_none());
    let bobs_home = w.get("/home.php", Some(&bob.auth), &[]);
    let (foreign, _) = w.submit(&bobs_home, "/logout.php", Some(&ada.auth), &[]);
    assert_eq!(foreign.status, 403);
    assert!(foreign.set_cookie.is_none());

    // A POST with no login has nothing to log out of, and sets no cookie.
    let anon = w.post("/logout.php", None, &pairs);
    assert_eq!(anon.status, 303);
    assert_eq!(anon.location.as_deref(), Some("https://p.example/"));
    assert!(anon.set_cookie.is_none());
    let unknown = w.post(
        "/logout.php",
        Some("0123456789abcdef0123456789abcdef"),
        &pairs,
    );
    assert_eq!(unknown.status, 303);
    assert!(unknown.set_cookie.is_none());

    // A POST with the cookie and a valid token clears the cookie.
    let ok = w.post("/logout.php", Some(&ada.auth), &pairs);
    assert_eq!(ok.status, 303);
    assert_eq!(ok.location.as_deref(), Some("https://p.example/"));
    assert!(ok.set_cookie.unwrap().starts_with("auth=; Path=/;"));
}

#[test]
fn home_shows_the_account_to_its_holder_only() {
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada <i>x</i>");

    let anon = w.get("/home.php", None, &[]);
    assert_eq!(anon.status, 303);
    assert_eq!(
        anon.location.as_deref(),
        Some("https://p.example/login_form.php?next_url=home.php")
    );
    let wrong = w.get("/home.php", Some("0123456789abcdef0123456789abcdef"), &[]);
    assert_eq!(wrong.status, 303);

    let (page, n) = w.page("/home.php", Some(&ada.auth), false, &[]);
    assert_eq!(n, 1);
    assert_eq!(page.status, 200);
    assert!(page.body.contains("ada@example.com"));
    assert!(page.body.contains("Ada &lt;i&gt;x&lt;/i&gt;"));
    assert!(!page.body.contains("<i>x</i>"));
    assert!(page
        .body
        .contains(&format!("results.php?userid={}", ada.id)));
    assert!(page.body.contains("account_p.example.xml"));

    // The card for attaching a computer: the project URL, the key shown once
    // as before, the weak key and the page that replaces the key. The key is
    // never in a link.
    let body = &page.body;
    assert!(body.contains("<h2>Attach a computer</h2>"));
    assert!(body.contains("<code class=\"key\">https://p.example/</code>"));
    assert!(body.contains("boinccmd --project_attach https://p.example/ ACCOUNT_KEY"));
    assert!(body.contains("href=\"reset_account_key.php\""));
    assert_eq!(body.matches(ada.auth.as_str()).count(), 1);
    assert!(!body.contains(&format!("={}", ada.auth)));
    for link in [
        "edit_user_info_form.php",
        "edit_email_form.php",
        "edit_passwd_form.php",
        "hosts_user.php",
        "prefs.php?subset=global",
        "prefs.php?subset=project",
    ] {
        assert!(body.contains(&format!("href=\"{link}\"")), "{link}");
    }
    assert!(
        !body.contains("get_passwd.php"),
        "no mail, no recovery link"
    );
}

#[test]
fn computers_are_private_unless_the_account_shows_them() {
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let bob = w.account("bob@example.com", "secret", "Bob");
    let h1 = w.host(ada.id, "ada-<b>desk</b>", NOW - 100);
    w.host(ada.id, "ada-old-laptop", NOW - 40 * 86400);
    let bobs = w.host(bob.id, "bob-desk", NOW);

    // The holder sees names; old computers only with show_all.
    let (mine, n) = w.page("/hosts_user.php", Some(&ada.auth), false, &[]);
    assert_eq!(n, 3);
    assert!(mine.body.contains("Your computers"));
    assert!(mine.body.contains("ada-&lt;b&gt;desk&lt;/b&gt;"));
    assert!(!mine.body.contains("<b>desk"));
    assert!(!mine.body.contains("ada-old-laptop"));
    assert!(mine.body.contains("Model &lt;script&gt;"));
    assert!(!mine.body.contains("<script>"));
    assert!(mine.body.contains("[1] CUDA (8192MB)"));
    let all = w.get("/hosts_user.php", Some(&ada.auth), &[("show_all", "1")]);
    assert!(all.body.contains("ada-old-laptop"));
    assert!(!all.body.contains("bob-desk"));

    // Someone else: hidden by default, and never with names.
    let id = ada.id.to_string();
    let hidden = w.get("/hosts_user.php", Some(&bob.auth), &[("userid", &id)]);
    assert!(hidden.body.contains("Computers hidden"));
    assert!(!hidden.body.contains("ada-"));
    w.sql(&format!(
        "UPDATE user SET show_hosts = 1 WHERE id = {}",
        ada.id
    ));
    let shown = w.get("/hosts_user.php", None, &[("userid", &id)]);
    assert!(shown.body.contains("Computers belonging to Ada"));
    assert!(shown
        .body
        .contains(&format!("show_host_detail.php?hostid={h1}")));
    assert!(!shown.body.contains("ada-"));

    // Nobody logged in and no account asked for.
    let anon = w.get("/hosts_user.php", None, &[]);
    assert_eq!(anon.status, 303);

    // One computer: the holder gets private fields, IP only on request.
    let hid = h1.to_string();
    let (detail, n) = w.page(
        "/show_host_detail.php",
        Some(&ada.auth),
        false,
        &[("hostid", &hid)],
    );
    assert!(n <= 3);
    assert!(detail.body.contains("ada-&lt;b&gt;desk"));
    assert!(detail.body.contains("Show IP address"));
    assert!(!detail.body.contains("192.0.2.7"));
    let ip = w.get(
        "/show_host_detail.php",
        Some(&ada.auth),
        &[("hostid", &hid), ("ipprivate", "1")],
    );
    assert!(ip.body.contains("192.0.2.7"));
    assert!(ip.body.contains("198.51.100.7"));
    for viewer in [Some(bob.auth.as_str()), None] {
        let other = w.get(
            "/show_host_detail.php",
            viewer,
            &[("hostid", &hid), ("ipprivate", "1")],
        );
        assert_eq!(other.status, 200);
        assert!(!other.body.contains("ada-"));
        assert!(!other.body.contains("192.0.2.7"));
        assert!(!other.body.contains("Free Disk Space"));
        assert!(other.body.contains("Model &lt;script&gt;"));
        assert!(other.body.contains("show_user.php?userid="));
    }
    let bob_host = w.get(
        "/show_host_detail.php",
        None,
        &[("hostid", &bobs.to_string())],
    );
    assert!(bob_host.body.contains("Anonymous"));
    let missing = w.get("/show_host_detail.php", None, &[("hostid", "999")]);
    assert_eq!(missing.status, 404);
}

#[test]
fn the_row_limit_counts_only_the_computers_listed() {
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada");
    for i in 0..105 {
        w.host(ada.id, &format!("old-{i}"), NOW - 40 * 86400);
    }
    let active = w.host(ada.id, "ada-desk", NOW - 100);

    // The oldest IDs come first, and all of them are out of contact.
    let (mine, n) = w.page("/hosts_user.php", Some(&ada.auth), false, &[("sort", "id")]);
    assert_eq!(n, 3);
    assert!(mine
        .body
        .contains(&format!("show_host_detail.php?hostid={active}")));
    assert!(!mine.body.contains("old-"));
    assert!(mine.body.contains("105"), "{}", mine.body);
    let (all, n) = w.page(
        "/hosts_user.php",
        Some(&ada.auth),
        false,
        &[("sort", "id"), ("show_all", "1")],
    );
    assert_eq!(n, 2);
    assert!(all.body.contains("old-0"));
}

#[test]
fn a_filtered_task_list_reads_a_bounded_window() {
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let host = w.host(ada.id, "ada-desk", NOW);
    let wu = w.workunit("wu", 1);
    // Three tasks in progress, then more finished ones than two windows hold.
    let mut running = Vec::new();
    for i in 0..3 {
        running.push(w.result(wu, &format!("running_{i}"), host, ada.id, 4, ""));
    }
    let mut done = Vec::new();
    for i in 0..RESULTS_FILTER_WINDOW * 2 + 10 {
        done.push(w.result(wu, &format!("done_{i}"), host, ada.id, 5, ""));
    }
    let hid = host.to_string();
    let rows = |p: &SitePage| p.body.matches("<a href=\"result.php?resultid=").count();
    let next = |p: &SitePage, key: &str| -> Option<String> {
        let at = p.body.find(&format!("&amp;{key}="))? + key.len() + 6;
        let digits: String = p.body[at..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        Some(digits)
    };

    // The newest window holds no task in progress: an empty page that links
    // on from the last task read, in three statements.
    let filter = [("hostid", hid.as_str()), ("state", "1")];
    let (first, n) = w.page("/results.php", None, false, &filter);
    assert_eq!(n, 3);
    assert_eq!(rows(&first), 0);
    assert!(first.body.contains("older or newer ones may match"));
    let before = next(&first, "before").unwrap();
    assert_eq!(before, done[done.len() - RESULTS_FILTER_WINDOW].to_string());
    assert!(!first.body.contains("Previous 20"));

    let second = w.get(
        "/results.php",
        None,
        &[filter[0], filter[1], ("before", &before)],
    );
    assert_eq!(rows(&second), 0);
    let before = next(&second, "before").unwrap();
    // Back from an empty page.
    assert!(next(&second, "after").is_some());

    let third = w.get(
        "/results.php",
        None,
        &[filter[0], filter[1], ("before", &before)],
    );
    assert_eq!(rows(&third), 3);
    assert!(third.body.contains(&format!("resultid={}\"", running[0])));
    assert!(!third.body.contains("Next 20"));

    // Upwards from the oldest task the same way.
    let after = running[2].to_string();
    let up = w.get(
        "/results.php",
        None,
        &[filter[0], filter[1], ("after", &after)],
    );
    assert_eq!(rows(&up), 0);
    assert_eq!(
        next(&up, "after").unwrap(),
        done[RESULTS_FILTER_WINDOW - 1].to_string()
    );

    // A filter that fills its page links from the last task shown.
    let valid = w.get("/results.php", None, &[filter[0], ("state", "4")]);
    assert_eq!(rows(&valid), RESULTS_PER_PAGE);
    assert_eq!(
        next(&valid, "before").unwrap(),
        done[done.len() - RESULTS_PER_PAGE].to_string()
    );
}

#[test]
fn task_lists_page_by_id_and_keep_accounts_apart() {
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let bob = w.account("bob@example.com", "secret", "Bob");
    let host = w.host(ada.id, "ada-desk", NOW);
    let bob_host = w.host(bob.id, "bob-desk", NOW);
    let wu = w.workunit("wu", 1);
    let mut ids = Vec::new();
    for i in 0..45 {
        let state = if i % 2 == 0 { 5 } else { 4 };
        ids.push(w.result(wu, &format!("ada_task_{i}"), host, ada.id, state, ""));
    }
    w.result(wu, "bob_task", bob_host, bob.id, 5, "");
    let hid = host.to_string();

    // Newest first, 20 to a page, two statements however deep.
    let (first, n) = w.page("/results.php", None, false, &[("hostid", &hid)]);
    assert_eq!(n, 2);
    let rows = |p: &SitePage| p.body.matches("<a href=\"result.php?resultid=").count();
    assert_eq!(rows(&first), RESULTS_PER_PAGE);
    assert!(first.body.contains(&format!("resultid={}\"", ids[44])));
    assert!(!first.body.contains("Previous 20"));
    let before = ids[25].to_string();
    assert!(first.body.contains(&format!("before={before}")));

    let (second, n) = w.page(
        "/results.php",
        None,
        false,
        &[("hostid", &hid), ("before", &before)],
    );
    assert_eq!(n, 2);
    assert_eq!(rows(&second), RESULTS_PER_PAGE);
    assert!(second.body.contains(&format!("resultid={}\"", ids[24])));
    assert!(second.body.contains(&format!("after={}", ids[24])));
    let before = ids[5].to_string();
    let third = w.get(
        "/results.php",
        None,
        &[("hostid", &hid), ("before", &before)],
    );
    assert_eq!(rows(&third), 5);
    assert!(!third.body.contains("Next 20"));

    // Back from the second page to the first.
    let after = ids[24].to_string();
    let back = w.get("/results.php", None, &[("hostid", &hid), ("after", &after)]);
    assert_eq!(rows(&back), RESULTS_PER_PAGE);
    assert!(back.body.contains(&format!("resultid={}\"", ids[44])));
    assert!(!back.body.contains("Previous 20"));

    // A state filter.
    let in_progress = w.get("/results.php", None, &[("hostid", &hid), ("state", "1")]);
    assert_eq!(rows(&in_progress), RESULTS_PER_PAGE);
    assert!(!in_progress
        .body
        .contains(&format!("resultid={}\"", ids[44])));
    assert!(in_progress
        .body
        .contains(&format!("resultid={}\"", ids[43])));

    // An account's tasks: its holder only.
    let uid = ada.id.to_string();
    let (own, n) = w.page("/results.php", Some(&ada.auth), false, &[("userid", &uid)]);
    assert_eq!(n, 2);
    assert_eq!(rows(&own), RESULTS_PER_PAGE);
    assert!(!own.body.contains("bob_task"));
    assert!(own
        .body
        .contains(&format!("show_host_detail.php?hostid={host}")));
    let names = w.get(
        "/results.php",
        Some(&ada.auth),
        &[("userid", &uid), ("show_names", "1")],
    );
    assert!(names.body.contains(">ada_task_44<"));
    let other = w.get("/results.php", Some(&bob.auth), &[("userid", &uid)]);
    assert_eq!(other.status, 403);
    assert!(!other.body.contains("ada_task"));
    let anon = w.get("/results.php", None, &[("userid", &uid)]);
    assert_eq!(anon.status, 303);
    let none = w.get("/results.php", None, &[]);
    assert_eq!(none.status, 400);
    let missing = w.get("/results.php", None, &[("hostid", "999")]);
    assert_eq!(missing.status, 404);
}

#[test]
fn a_task_and_its_workunit() {
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let host = w.host(ada.id, "ada-desk", NOW);
    let wu = w.workunit("wu<1>", 1);
    let rid = w.result(
        wu,
        "task<1>",
        host,
        ada.id,
        5,
        "line 1\n<script>alert(1)</script>\n",
    );

    let (page, n) = w.page(
        "/result.php",
        None,
        false,
        &[("resultid", &rid.to_string())],
    );
    assert_eq!(n, 1);
    assert!(page.body.contains("task&lt;1&gt;"));
    assert!(page.body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(!page.body.contains("<script>"));
    assert!(page
        .body
        .contains("Worker &lt;one&gt; v1.02 (mt)<br>x86_64-pc-linux-gnu"));
    assert!(page.body.contains("<th>Outcome</th><td>Success</td>"));
    assert!(page.body.contains("<th>Validate state</th><td>Valid</td>"));
    let by_name = w.get("/result.php", None, &[("result_name", "task<1>")]);
    assert_eq!(by_name.status, 200);
    let missing = w.get("/result.php", None, &[("result_name", "<b>nope</b>")]);
    assert_eq!(missing.status, 404);
    assert!(missing.body.contains("&lt;b&gt;nope"));

    let (wpage, n) = w.page("/workunit.php", None, false, &[("wuid", &wu.to_string())]);
    assert_eq!(n, 2);
    assert!(wpage.body.contains("wu&lt;1&gt;"));
    assert!(wpage.body.contains(&format!("result.php?resultid={rid}")));
    assert!(wpage
        .body
        .contains("<a href=\"https://p.example/download/in_1\">in&lt;put</a>"));
    assert!(wpage.body.contains("--n 1"));

    // Adaptive replication: tasks hidden until there is a canonical result.
    let quiet = w.workunit("quiet", 2);
    let qr = w.result(quiet, "quiet_0", host, ada.id, 4, "");
    let hidden = w.get("/workunit.php", None, &[("wuid", &quiet.to_string())]);
    assert!(hidden.body.contains("suppressed pending completion"));
    assert!(!hidden.body.contains(&format!("resultid={qr}")));
    let missing = w.get("/workunit.php", None, &[("wuid", "999")]);
    assert_eq!(missing.status, 404);
}

#[test]
fn a_task_of_the_anonymous_platform_names_its_resource() {
    // `anon_platform_string` in `html/inc/result.inc`, for the negative
    // `result.app_version_id` the scheduler stores.
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let host = w.host(ada.id, "ada-desk", NOW);
    let wu = w.workunit("wu_anon", 1);
    for (id, shown) in [
        (-2, "Worker &lt;one&gt;<br>Anonymous platform (CPU)"),
        (-3, "Worker &lt;one&gt;<br>Anonymous platform (NVIDIA GPU)"),
        (-4, "Worker &lt;one&gt;<br>Anonymous platform (AMD GPU)"),
        (-5, "Worker &lt;one&gt;<br>Anonymous platform (Intel GPU)"),
        (-6, "Worker &lt;one&gt;<br>Anonymous platform (Apple GPU)"),
        (-1, "Worker &lt;one&gt;<br>Anonymous platform<"),
    ] {
        let rid = w.result(wu, &format!("anon_{}", -id), host, ada.id, 5, "");
        w.sql(&format!(
            "UPDATE result SET app_version_id = {id} WHERE id = {rid}"
        ));
        let page = w.get("/result.php", None, &[("resultid", &rid.to_string())]);
        assert!(page.body.contains(shown), "{id}: {}", page.body);
    }
}

#[test]
fn task_lists_read_their_indexes() {
    let w = World::new();
    let plan = |sql: &str, params: Vec<cb_db::Value>| {
        let rows = block_on(w.db.run(cb_db::Stmt {
            sql: format!("EXPLAIN QUERY PLAN {sql}"),
            params,
        }))
        .unwrap();
        rows.rows
            .iter()
            .map(|r| r["detail"].as_str().unwrap_or_default().to_string())
            .collect::<Vec<_>>()
            .join("; ")
    };
    // A filtered list: the window walks the index, the filter sees only it.
    let by_host = plan(
        "SELECT id FROM (SELECT id, server_state FROM result WHERE hostid = ? AND hostid > 0 \
         AND id < ? ORDER BY id DESC LIMIT 500) WHERE 1 = 1 AND server_state = 4 \
         ORDER BY id DESC LIMIT ?",
        vec![5.into(), 100.into(), 21.into()],
    );
    assert!(by_host.contains("INDEX res_host"), "{by_host}");
    assert!(!by_host.contains("TEMP B-TREE"), "{by_host}");
    let window = plan(
        "SELECT min(id) AS lo, max(id) AS hi, count(*) AS n FROM (SELECT id FROM result \
         WHERE hostid = ? AND hostid > 0 AND id < ? ORDER BY id DESC LIMIT 500)",
        vec![5.into(), 100.into()],
    );
    assert!(window.contains("INDEX res_host"), "{window}");
    assert!(!window.contains("TEMP B-TREE"), "{window}");
    let all_by_host = plan(
        "SELECT id FROM result WHERE hostid = ? AND hostid > 0 ORDER BY id DESC LIMIT ?",
        vec![5.into(), 21.into()],
    );
    assert!(all_by_host.contains("INDEX res_host "), "{all_by_host}");
    assert!(!all_by_host.contains("TEMP B-TREE"), "{all_by_host}");
    let by_user = plan(
        "SELECT id FROM result WHERE userid = ? AND userid > 0 AND userid = \
         (SELECT id FROM user WHERE authenticator = ? AND blocked = 0) AND id > ? \
         ORDER BY id ASC LIMIT ?",
        vec![5.into(), "x".into(), 3.into(), 21.into()],
    );
    assert!(by_user.contains("INDEX res_user"), "{by_user}");
    assert!(!by_user.contains("TEMP B-TREE"), "{by_user}");
    let by_wu = plan(
        "SELECT id FROM result WHERE workunitid = ? ORDER BY id LIMIT ?",
        vec![5.into(), 100.into()],
    );
    assert!(by_wu.contains("INDEX res_wuid"), "{by_wu}");
    let hosts = plan(
        "SELECT * FROM host WHERE userid = ? AND rpc_time >= ? ORDER BY rpc_time DESC, id LIMIT ?",
        vec![5.into(), 0.into(), 100.into()],
    );
    assert!(hosts.contains("INDEX host_user"), "{hosts}");
}

const OPEN_CONFIG: &str = "<config>\n<long_name>Test Project</long_name>\n<master_url>https://p.example/</master_url>\n<user_country>1</user_country>\n<account_creation>1</account_creation>\n</config>\n";

#[test]
fn accounts_are_created_on_the_site_only_when_creation_is_open() {
    let closed = World::new();
    assert_eq!(
        closed.get("/create_account_form.php", None, &[]).status,
        403
    );
    let refused = closed.post(
        "/create_account_action.php",
        None,
        &[
            ("new_name", "Ada"),
            ("new_email_addr", "ada@example.com"),
            ("passwd", "secret1"),
            ("passwd2", "secret1"),
        ],
    );
    assert_eq!(refused.status, 403);
    assert!(!closed
        .get("/login_form.php", None, &[])
        .body
        .contains("create_account_form.php"));

    let w = World::with_config(OPEN_CONFIG);
    let form = w.get(
        "/signup.php",
        None,
        &[("next_url", "prefs.php?subset=global")],
    );
    assert_eq!(form.status, 200);
    assert!(form.body.contains("action=\"create_account_action.php\""));
    assert!(form.body.contains("<option value=\"United Kingdom\">"));
    assert!(w
        .get("/login_form.php", None, &[])
        .body
        .contains("create_account_form.php"));

    let good = [
        ("new_name", "Ada"),
        ("new_email_addr", "Ada@Example.com"),
        ("passwd", "secret1"),
        ("passwd2", "secret1"),
        ("country", "United Kingdom"),
        ("postal_code", "N1"),
        ("next_url", "prefs.php?subset=global"),
    ];
    let with = |k: &str, v: &'static str| -> Vec<(&str, &str)> {
        good.iter()
            .map(|&(a, b)| if a == k { (a, v) } else { (a, b) })
            .collect()
    };
    for (k, v, status) in [
        ("passwd", "short", 400),
        ("passwd2", "secret2", 400),
        ("new_name", "12345", 400),
        ("new_name", "<b>Ada</b>", 400),
        ("new_email_addr", "ada", 400),
        ("country", "Atlantis", 400),
    ] {
        let page = w.post("/create_account_action.php", None, &with(k, v));
        assert_eq!(page.status, status, "{k}={v}");
        assert!(page.set_cookie.is_none());
    }
    // A GET does not make an account.
    assert_eq!(w.get("/create_account_action.php", None, &good).status, 405);

    let (made, n) = w.page("/create_account_action.php", None, true, &good);
    assert_eq!(n, 2, "lookup by email, then the insert");
    assert_eq!(made.status, 303);
    assert_eq!(
        made.location.as_deref(),
        Some("https://p.example/prefs.php?subset=global")
    );
    let cookie = made.set_cookie.unwrap();
    assert!(cookie.ends_with("; Max-Age=31536000"));
    let auth = cookie
        .trim_start_matches("auth=")
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let home = w.get("/home.php", Some(&auth), &[]);
    assert!(home.body.contains("ada@example.com"));
    let id = block_on(w.db.run(stmt!(
        "SELECT id FROM user WHERE authenticator = ?",
        auth.as_str()
    )))
    .unwrap()
    .scalar_i64()
    .unwrap();
    let u = w.user(id);
    assert_eq!(
        (u.name.as_str(), u.country.as_str(), u.postal_code.as_str()),
        ("Ada", "United Kingdom", "N1")
    );
    // The password works for the client's lookup.
    let found = block_on(cb_core::web::lookup_account(
        &w.db,
        PEPPER,
        "ada@example.com",
        &passwd_hash("secret1", "ada@example.com"),
    ))
    .unwrap();
    assert!(found.contains(&auth));

    let again = w.post(
        "/create_account_action.php",
        None,
        &with("passwd", "another1")
            .iter()
            .map(|&(a, b)| {
                if a == "passwd2" {
                    (a, "another1")
                } else {
                    (a, b)
                }
            })
            .collect::<Vec<_>>(),
    );
    assert_eq!(again.status, 409);
}

#[test]
fn account_changes_need_the_form_token() {
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada \"<x>\"");
    let bob = w.account("bob@example.com", "secret", "Bob");
    assert_eq!(w.get("/edit_user_info_form.php", None, &[]).status, 303);

    let form = w.get("/edit_user_info_form.php", Some(&ada.auth), &[]);
    assert!(form.body.contains("value=\"Ada &quot;&lt;x&gt;&quot;\""));
    let change = [
        ("user_name", "Ada L"),
        ("country", "France"),
        ("url", "ada.example/x"),
        ("postal_code", "75001"),
    ];

    // No token, a token for another account, an old token, or a GET: refused.
    assert_eq!(
        w.post("/edit_user_info_action.php", Some(&ada.auth), &change)
            .status,
        403
    );
    let bobs = w.get("/edit_user_info_form.php", Some(&bob.auth), &[]);
    assert_eq!(
        w.submit(
            &bobs,
            "/edit_user_info_action.php",
            Some(&ada.auth),
            &change
        )
        .0
        .status,
        403
    );
    let mut old = hidden_fields(&form.body);
    let tnow = (NOW - 86_401).to_string();
    let ttok = cb_crypto::account::form_token(&ada.auth, NOW - 86_401);
    old.retain(|(k, _)| k != "tnow" && k != "ttok");
    let mut q: Vec<(&str, &str)> = change.to_vec();
    q.push(("tnow", &tnow));
    q.push(("ttok", &ttok));
    assert_eq!(
        w.post("/edit_user_info_action.php", Some(&ada.auth), &q)
            .status,
        403
    );
    let mut get_q: Vec<(String, String)> = hidden_fields(&form.body);
    get_q.extend(change.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let get_pairs: Vec<(&str, &str)> = get_q
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(
        w.get("/edit_user_info_action.php", Some(&ada.auth), &get_pairs)
            .status,
        403
    );
    assert_eq!(w.user(ada.id).name, "Ada \"<x>\"");

    let (ok, n) = w.submit(
        &form,
        "/edit_user_info_action.php",
        Some(&ada.auth),
        &change,
    );
    assert_eq!((ok.status, n), (303, 2));
    let u = w.user(ada.id);
    assert_eq!(
        (
            u.name.as_str(),
            u.country.as_str(),
            u.url.as_str(),
            u.postal_code.as_str()
        ),
        ("Ada L", "France", "ada.example/x", "75001")
    );
    for (k, v) in [
        ("user_name", ""),
        ("url", "ftp://x"),
        ("country", "Narnia"),
        ("postal_code", "<b>"),
    ] {
        let page = w
            .submit(
                &form,
                "/edit_user_info_action.php",
                Some(&ada.auth),
                &[("user_name", "Ada"), (k, v)],
            )
            .0;
        assert_eq!(page.status, 400, "{k}");
    }
    assert_eq!(w.user(ada.id).name, "Ada L");
}

#[test]
fn password_and_email_changes_need_the_password() {
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada");
    w.account("bob@example.com", "secret", "Bob");
    let weak_before = weak_account_key(&w.user(ada.id));

    let form = w.get("/edit_passwd_form.php", Some(&ada.auth), &[]);
    let wrong = w
        .submit(
            &form,
            "/edit_passwd_action.php",
            Some(&ada.auth),
            &[
                ("old_passwd", "nope"),
                ("passwd", "better1"),
                ("passwd2", "better1"),
            ],
        )
        .0;
    assert_eq!(wrong.status, 403);
    let short = w
        .submit(
            &form,
            "/edit_passwd_action.php",
            Some(&ada.auth),
            &[("old_passwd", "secret"), ("passwd", "x"), ("passwd2", "x")],
        )
        .0;
    assert_eq!(short.status, 400);
    let (ok, n) = w.submit(
        &form,
        "/edit_passwd_action.php",
        Some(&ada.auth),
        &[
            ("old_passwd", "secret"),
            ("passwd", "better1"),
            ("passwd2", "better1"),
        ],
    );
    assert_eq!(
        (ok.status, n),
        (200, 3),
        "the account, then one batch of two: the change and the end of login links"
    );
    assert_ne!(weak_account_key(&w.user(ada.id)), weak_before);
    let login = |email: &str, pw: &str| {
        w.post(
            "/login_action.php",
            None,
            &[("email_addr", email), ("passwd", pw)],
        )
        .status
    };
    assert_eq!(login("ada@example.com", "secret"), 403);
    assert_eq!(login("ada@example.com", "better1"), 303);
    // The account key does instead of the current password.
    let by_key = w
        .submit(
            &form,
            "/edit_passwd_action.php",
            Some(&ada.auth),
            &[
                ("old_passwd", &ada.auth),
                ("passwd", "best12"),
                ("passwd2", "best12"),
            ],
        )
        .0;
    assert_eq!(by_key.status, 200);

    let form = w.get("/edit_email_form.php", Some(&ada.auth), &[]);
    assert!(form.body.contains("ada@example.com"));
    let wrong = w
        .submit(
            &form,
            "/edit_email_action.php",
            Some(&ada.auth),
            &[("email_addr", "ada@new.example"), ("passwd", "better1")],
        )
        .0;
    assert_eq!(wrong.status, 403);
    let taken = w
        .submit(
            &form,
            "/edit_email_action.php",
            Some(&ada.auth),
            &[("email_addr", "Bob@example.com"), ("passwd", "best12")],
        )
        .0;
    assert_eq!(taken.status, 409);
    let (ok, n) = w.submit(
        &form,
        "/edit_email_action.php",
        Some(&ada.auth),
        &[("email_addr", "Ada@New.example"), ("passwd", "best12")],
    );
    assert_eq!(
        (ok.status, n),
        (200, 4),
        "the account, then one batch of three: the count, the change, the login links"
    );
    assert!(ok.body.contains("ada@new.example"));
    // The same password works with the new address, for the site and the client.
    assert_eq!(login("ada@new.example", "best12"), 303);
    assert_eq!(login("ada@example.com", "best12"), 403);
    let found = block_on(cb_core::web::lookup_account(
        &w.db,
        PEPPER,
        "ada@new.example",
        &passwd_hash("best12", "ada@new.example"),
    ))
    .unwrap();
    assert!(found.contains(&ada.auth));
}

#[test]
fn preferences_are_edited_per_venue_and_reach_clients() {
    use cb_core::prefs::{PrefsForm, Subset};
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada");
    assert_eq!(
        w.get("/prefs.php", Some(&ada.auth), &[("subset", "other")])
            .status,
        400
    );
    let shown = w.get("/prefs.php", Some(&ada.auth), &[("subset", "global")]);
    assert_eq!(shown.status, 200);
    assert!(shown.body.contains("Store at least"));
    assert!(shown
        .body
        .contains("prefs_edit.php?subset=global&amp;venue=work"));

    let global = PrefsForm::new(Subset::Global, &w.project);
    let mut fields: Vec<(String, String)> =
        global.form_values(&global.defaults()).into_iter().collect();
    let set = |fields: &mut Vec<(String, String)>, k: &str, v: &str| {
        fields.retain(|(a, _)| a != k);
        fields.push((k.into(), v.into()));
    };
    set(&mut fields, "max_ncpus_pct", "60");
    let form = w.get("/prefs_edit.php", Some(&ada.auth), &[("subset", "global")]);
    assert_eq!(form.status, 200);
    let pairs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let (saved, n) = w.submit(&form, "/prefs_edit.php", Some(&ada.auth), &pairs);
    assert_eq!((saved.status, n), (303, 2));
    assert_eq!(
        saved.location.as_deref(),
        Some("https://p.example/prefs.php?subset=global&updated=1")
    );
    let stored = w.user(ada.id).global_prefs;
    assert!(stored.starts_with(&format!(
        "<global_preferences>\n<mod_time>{NOW}</mod_time>\n"
    )));
    assert!(stored.contains("<max_ncpus_pct>60</max_ncpus_pct>"));
    let p = cb_proto::sched_request::GlobalPrefs::parse(&stored, "");
    assert_eq!((p.mod_time, p.max_ncpus_pct), (NOW as f64, 60.0));

    // A separate set for work starts from the main one.
    let form = w.get(
        "/prefs_edit.php",
        Some(&ada.auth),
        &[("subset", "global"), ("venue", "work")],
    );
    assert!(form.body.contains("name=\"max_ncpus_pct\" value=\"60\""));
    set(&mut fields, "max_ncpus_pct", "20");
    let pairs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(
        w.submit(&form, "/prefs_edit.php", Some(&ada.auth), &pairs)
            .0
            .status,
        303
    );
    let stored = w.user(ada.id).global_prefs;
    assert_eq!(
        cb_proto::sched_request::GlobalPrefs::parse(&stored, "work").max_ncpus_pct,
        20.0
    );
    assert_eq!(
        cb_proto::sched_request::GlobalPrefs::parse(&stored, "home").max_ncpus_pct,
        60.0
    );

    // Out-of-range values show the form again and store nothing.
    set(&mut fields, "max_ncpus_pct", "500");
    let pairs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let refused = w
        .submit(&form, "/prefs_edit.php", Some(&ada.auth), &pairs)
        .0;
    assert_eq!(refused.status, 400);
    assert!(refused.body.contains("value=\"500\""));
    assert_eq!(w.user(ada.id).global_prefs, stored);
    // A GET with the fields, tokens included, changes nothing either.
    let mut get_q: Vec<(String, String)> = hidden_fields(&form.body);
    get_q.extend(fields.iter().cloned());
    set(&mut get_q, "max_ncpus_pct", "30");
    let pairs: Vec<(&str, &str)> = get_q
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let page = w.get("/prefs_edit.php", Some(&ada.auth), &pairs);
    assert_eq!(page.status, 200);
    assert_eq!(w.user(ada.id).global_prefs, stored);

    // Removing the work set: confirm by GET, remove by POST.
    let confirm = w.get(
        "/prefs_remove.php",
        Some(&ada.auth),
        &[("subset", "global"), ("venue", "work")],
    );
    assert!(confirm.body.contains("Are you sure"));
    assert!(w
        .user(ada.id)
        .global_prefs
        .contains("<venue name=\"work\">"));
    assert_eq!(
        w.submit(&confirm, "/prefs_remove.php", Some(&ada.auth), &[])
            .0
            .status,
        303
    );
    assert!(!w.user(ada.id).global_prefs.contains("<venue"));

    // Project preferences, with the account's settings beside them.
    let project = PrefsForm::new(Subset::Project, &w.project);
    let mut fields: Vec<(String, String)> = project
        .form_values(&project.defaults())
        .into_iter()
        .collect();
    set(&mut fields, "resource_share", "250");
    fields.retain(|(k, _)| k != "app_id_2");
    set(&mut fields, "show_hosts", "1");
    set(&mut fields, "default_venue", "school");
    let form = w.get("/prefs_edit.php", Some(&ada.auth), &[("subset", "project")]);
    assert!(form.body.contains("Worker &lt;one&gt;"));
    assert!(form.body.contains("name=\"default_venue\""));
    let pairs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(
        w.submit(&form, "/prefs_edit.php", Some(&ada.auth), &pairs)
            .0
            .status,
        303
    );
    let u = w.user(ada.id);
    assert_eq!(
        (u.show_hosts, u.send_email, u.venue.as_str()),
        (1, 0, "school")
    );
    assert!(u
        .project_prefs
        .starts_with("<project_preferences>\n<resource_share>250</resource_share>\n"));
    assert!(u
        .project_prefs
        .contains("<apps_selected>\n<app_id>1</app_id>\n</apps_selected>"));
    let shown = w.get("/prefs.php", Some(&ada.auth), &[("subset", "project")]);
    assert!(shown.body.contains("Worker &lt;one&gt;"));
    assert!(shown.body.contains("school"));

    // A bad default venue is refused.
    set(&mut fields, "default_venue", "moon");
    let pairs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(
        w.submit(&form, "/prefs_edit.php", Some(&ada.auth), &pairs)
            .0
            .status,
        400
    );
}

/// A database that runs one statement of another request next to the first
/// batch with a write it watches for (a statement starting with one of
/// `trigger`): before it, so that this request and the other start from the
/// same read, or after it, for a request whose later statement then sees what
/// the other changed.
struct ChangedMeanwhile<'a> {
    inner: &'a SqliteDb,
    other: RefCell<Vec<Stmt>>,
    trigger: &'static [&'static str],
    after: bool,
}

#[async_trait(?Send)]
impl Db for ChangedMeanwhile<'_> {
    async fn batch(&self, stmts: Vec<Stmt>) -> DbResult<Vec<Rows>> {
        let fires = stmts
            .iter()
            .any(|s| self.trigger.iter().any(|t| s.sql.starts_with(t)));
        let other = if fires {
            std::mem::take(&mut *self.other.borrow_mut())
        } else {
            Vec::new()
        };
        if !self.after && !other.is_empty() {
            self.inner.batch(other.clone()).await?;
        }
        let out = self.inner.batch(stmts).await?;
        if self.after && !other.is_empty() {
            self.inner.batch(other).await?;
        }
        Ok(out)
    }
}

#[test]
fn am_set_info_does_not_pair_a_hash_with_a_new_address() {
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let stale = passwd_hash("newer", "ada@example.com");
    let q = |pairs: &[(&str, &str)]| -> Params {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    // The site changes the address (and so the hash) after the RPC read the
    // account and before it wrote.
    let site_hash = passwd_hash("secret", "ada@site.example");
    let racing = ChangedMeanwhile {
        inner: &w.db,
        other: RefCell::new(vec![stmt!(
            "UPDATE user SET email_addr = 'ada@site.example', passwd_hash = ? WHERE id = ?",
            site_hash.as_str(),
            ada.id
        )]),
        trigger: &["UPDATE user SET"],
        after: false,
    };
    let params = q(&[
        ("account_key", &ada.auth),
        ("name", "Ada L"),
        ("password_hash", &stale),
    ]);
    let reply = block_on(cb_core::web::am_set_info(
        &racing,
        &w.project.config,
        PEPPER,
        NOW,
        &params,
    ))
    .unwrap();
    assert_eq!(error_num(&reply), Some(-199), "{reply}");
    let u = w.user(ada.id);
    // Nothing was written: not the hash, and not the name beside it.
    assert_eq!(u.email_addr, "ada@site.example");
    assert_eq!(u.name, "Ada");
    assert_eq!(u.passwd_hash, site_hash);

    // A new address with its hash, against a hash changed meanwhile.
    let racing = ChangedMeanwhile {
        inner: &w.db,
        other: RefCell::new(vec![stmt!(
            "UPDATE user SET passwd_hash = 'changed' WHERE id = ?",
            ada.id
        )]),
        trigger: &["UPDATE user SET"],
        after: false,
    };
    let params = q(&[
        ("account_key", &ada.auth),
        ("email_addr", "ada@new.example"),
        ("password_hash", &passwd_hash("newer", "ada@new.example")),
    ]);
    let reply = block_on(cb_core::web::am_set_info(
        &racing,
        &w.project.config,
        PEPPER,
        NOW,
        &params,
    ))
    .unwrap();
    assert_eq!(error_num(&reply), Some(-199), "{reply}");
    assert_eq!(w.user(ada.id).email_addr, "ada@site.example");

    // A change that does not touch the credentials is not held up by one.
    let racing = ChangedMeanwhile {
        inner: &w.db,
        other: RefCell::new(vec![stmt!(
            "UPDATE user SET passwd_hash = 'again' WHERE id = ?",
            ada.id
        )]),
        trigger: &["UPDATE user SET"],
        after: false,
    };
    let params = q(&[("account_key", &ada.auth), ("name", "Ada L")]);
    let reply = block_on(cb_core::web::am_set_info(
        &racing,
        &w.project.config,
        PEPPER,
        NOW,
        &params,
    ))
    .unwrap();
    assert!(reply.contains("<success/>"), "{reply}");
    assert_eq!(w.user(ada.id).name, "Ada L");
}

/// What a form encoder sends for `text`: every byte but a few becomes `%XX`.
fn percent_encode(text: &str) -> String {
    text.bytes().map(|b| format!("%{b:02X}")).collect()
}

#[test]
fn am_set_info_takes_two_preference_documents_of_the_largest_size() {
    use cb_core::prefs::MAX_PREFS_BYTES;
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let fill = |root: &str, extra: usize| {
        let (open, close) = (format!("<{root}>\n"), format!("</{root}>"));
        let pad = "<".repeat(MAX_PREFS_BYTES - open.len() - close.len() + extra);
        format!("{open}{pad}{close}")
    };
    let global = fill("global_preferences", 0);
    let project = fill("project_preferences", 0);
    assert_eq!(
        (global.len(), project.len()),
        (MAX_PREFS_BYTES, MAX_PREFS_BYTES)
    );

    // The Worker reads a form of at most `MAX_FORM_BYTES`, and drops the
    // body of a longer one: the largest valid request must fit.
    let body = format!(
        "account_key={}&global_prefs={}&project_prefs={}&name=Ada+L&url=https%3A%2F%2Fada.example%2F&postal_code=N1+9GU&email_addr={}",
        ada.auth,
        percent_encode(&global),
        percent_encode(&project),
        percent_encode("ada@example.com"),
    );
    assert!(
        body.len() <= cb_core::web::MAX_FORM_BYTES,
        "{} > {}",
        body.len(),
        cb_core::web::MAX_FORM_BYTES
    );
    let (reply, _) = am_set_info(
        &w,
        &[
            ("account_key", &ada.auth),
            ("global_prefs", &global),
            ("project_prefs", &project),
        ],
    );
    assert!(reply.contains("<success/>"), "{reply}");
    assert_eq!(w.user(ada.id).global_prefs.len(), MAX_PREFS_BYTES);
    // One byte more is refused.
    let (reply, _) = am_set_info(
        &w,
        &[
            ("account_key", &ada.auth),
            ("global_prefs", &fill("global_preferences", 1)),
        ],
    );
    assert_eq!(error_num(&reply), Some(-118), "{reply}");
}

/// Whether every control of a form page has a name: a `<label for>` for its
/// ID, `aria-labelledby`, or a `<label>` around it. Returns the ones that
/// have none.
fn unnamed_controls(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(i) = html[at..].find(['<']) {
        let start = at + i;
        let end = start + html[start..].find('>').unwrap();
        let tag = &html[start..=end];
        at = end;
        let is_control = tag.starts_with("<select")
            || tag.starts_with("<textarea")
            || (tag.starts_with("<input")
                && !tag.contains("type=\"hidden\"")
                && !tag.contains("type=\"submit\""));
        if !is_control {
            continue;
        }
        let attr = |name: &str| {
            let key = format!(" {name}=\"");
            let a = tag.find(&key)? + key.len();
            Some(&tag[a..a + tag[a..].find('"')?])
        };
        let before = &html[..start];
        let inside_label = before
            .rfind("<label")
            .is_some_and(|o| before[o..].find("</label>").is_none());
        let by_for = attr("id").is_some_and(|id| {
            html.contains(&format!("<label for=\"{id}\""))
                || html.contains(&format!(" for=\"{id}\""))
        });
        if !(inside_label || by_for || attr("aria-labelledby").is_some()) {
            out.push(tag.to_string());
        }
    }
    out
}

#[test]
fn form_controls_have_names() {
    let w = World::with_config(
        &OPEN_CONFIG.replace("<user_country>1", "<user_url>1</user_url>\n<user_country>1"),
    );
    let ada = w.account("ada@example.com", "secret", "Ada");
    let auth = Some(ada.auth.as_str());
    let mut pages = vec![
        ("login", w.get("/login_form.php", None, &[])),
        ("sign-up", w.get("/create_account_form.php", None, &[])),
        ("account", w.get("/edit_user_info_form.php", auth, &[])),
        ("password", w.get("/edit_passwd_form.php", auth, &[])),
        ("email", w.get("/edit_email_form.php", auth, &[])),
        ("home", w.get("/home.php", auth, &[])),
        (
            "global prefs",
            w.get("/prefs_edit.php", auth, &[("subset", "global")]),
        ),
        (
            "project prefs",
            w.get("/prefs_edit.php", auth, &[("subset", "project")]),
        ),
        (
            "venue prefs",
            w.get(
                "/prefs_edit.php",
                auth,
                &[("subset", "project"), ("venue", "work")],
            ),
        ),
        (
            "remove",
            w.get(
                "/prefs_remove.php",
                auth,
                &[("subset", "project"), ("venue", "work")],
            ),
        ),
    ];
    let form = w.get("/prefs_edit.php", auth, &[("subset", "project")]);
    pages.push((
        "refused prefs",
        w.submit(&form, "/prefs_edit.php", auth, &[("resource_share", "x")])
            .0,
    ));
    for (name, page) in &pages {
        assert!(page.body.contains("<form"), "{name}");
        assert_eq!(unnamed_controls(&page.body), Vec::<String>::new(), "{name}");
    }
    let account = &pages[2].1.body;
    for id in ["user_name", "url", "country", "postal_code"] {
        assert!(
            account.contains(&format!("<label for=\"{id}\">"))
                && account.contains(&format!("id=\"{id}\"")),
            "{id}"
        );
    }
    let prefs = &pages[7].1.body;
    for id in ["send_email", "show_hosts", "default_venue"] {
        assert!(prefs.contains(&format!("<label for=\"{id}\">")), "{id}");
    }
    // The two hour selectors have a label each.
    let global = &pages[6].1.body;
    assert!(global.contains("for=\"pref_start_hour\""));
    assert!(global.contains("for=\"pref_end_hour\""));
    assert!(global.contains("Compute only between: start hour"));
    assert!(global.contains("Compute only between: end hour"));
    assert!(global.contains("Transfer files only between: end hour"));
    // Fields with the same caption in two groups are told apart.
    assert!(global.contains(
        "id=\"pref_max_ncpus_pct\" aria-labelledby=\"pref_group_1 pref_max_ncpus_pct_label pref_max_ncpus_pct_unit\""
    ));
    assert!(global.contains(
        "id=\"pref_niu_max_ncpus_pct\" aria-labelledby=\"pref_group_2 pref_niu_max_ncpus_pct_label pref_niu_max_ncpus_pct_unit\""
    ));
}

#[test]
fn preferences_refused_for_one_field_keep_the_others() {
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let auth = Some(ada.auth.as_str());
    let before = w.user(ada.id);

    // Computing: one valid edit beside one refused value.
    let form = w.get("/prefs_edit.php", auth, &[("subset", "global")]);
    let (page, _) = w.submit(
        &form,
        "/prefs_edit.php",
        auth,
        &[
            ("subset", "global"),
            ("max_ncpus_pct", "60"),
            ("cpu_usage_limit", "500"),
            ("run_on_batteries", "1"),
            ("start_hour", "8"),
            ("end_hour", "17"),
            ("max_bytes_sec_down", "250"),
        ],
    );
    assert_eq!(page.status, 400);
    assert!(page.body.contains("(not accepted)"));
    assert!(page.body.contains("name=\"cpu_usage_limit\" value=\"500\""));
    assert!(page.body.contains("name=\"max_ncpus_pct\" value=\"60\""));
    assert!(page
        .body
        .contains("name=\"max_bytes_sec_down\" value=\"250\""));
    assert!(page
        .body
        .contains("name=\"run_on_batteries\" value=\"1\" checked"));
    // The other checkboxes were not sent, so they are not ticked.
    assert!(!page
        .body
        .contains("name=\"leave_apps_in_memory\" value=\"1\" checked"));
    assert!(page
        .body
        .contains("<option value=\"8\" selected>8:00</option>"));
    assert!(page
        .body
        .contains("<option value=\"17\" selected>17:00</option>"));
    assert_eq!(w.user(ada.id).global_prefs, before.global_prefs);

    // Project: the account's settings, the applications and the flags too.
    let form = w.get("/prefs_edit.php", auth, &[("subset", "project")]);
    let (page, _) = w.submit(
        &form,
        "/prefs_edit.php",
        auth,
        &[
            ("subset", "project"),
            ("resource_share", "300"),
            ("max_jobs", "0.5"),
            ("no_cpu", "1"),
            ("app_id_2", "1"),
            ("show_hosts", "1"),
            ("default_venue", "school"),
        ],
    );
    assert_eq!(page.status, 400);
    assert!(page.body.contains("name=\"max_jobs\" value=\"0.5\""));
    assert!(page.body.contains("name=\"resource_share\" value=\"300\""));
    assert!(page.body.contains("name=\"no_cpu\" value=\"1\" checked"));
    assert!(page.body.contains("name=\"app_id_2\" value=\"1\" checked"));
    assert!(!page.body.contains("name=\"app_id_1\" value=\"1\" checked"));
    assert!(page
        .body
        .contains("name=\"show_hosts\" value=\"1\" checked"));
    assert!(!page
        .body
        .contains("name=\"send_email\" value=\"1\" checked"));
    assert!(page.body.contains("<option value=\"school\" selected>"));
    assert_eq!(w.user(ada.id).project_prefs, before.project_prefs);
    assert_eq!(w.user(ada.id).venue, before.venue);
}

#[test]
fn preferences_read_as_integers_are_whole_numbers() {
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let auth = Some(ada.auth.as_str());
    let form = w.get("/prefs_edit.php", auth, &[("subset", "project")]);
    for bad in ["0.5", "2.5", "1e-1", "abc"] {
        let (page, _) = w.submit(
            &form,
            "/prefs_edit.php",
            auth,
            &[("max_jobs", bad), ("app_id_1", "1"), ("app_id_2", "1")],
        );
        assert_eq!(page.status, 400, "{bad}");
        assert!(page.body.contains("(not accepted)"), "{bad}");
    }
    assert_eq!(w.user(ada.id).project_prefs, "");
    let (page, _) = w.submit(
        &form,
        "/prefs_edit.php",
        auth,
        &[("max_jobs", "3"), ("app_id_1", "1"), ("app_id_2", "1")],
    );
    assert_eq!(page.status, 303);
    assert!(w
        .user(ada.id)
        .project_prefs
        .contains("<max_jobs>3</max_jobs>"));

    // `daily_xfer_period_days` is read as an integer by the client.
    let form = w.get("/prefs_edit.php", auth, &[("subset", "global")]);
    let global = cb_core::prefs::PrefsForm::new(cb_core::prefs::Subset::Global, &w.project);
    let mut fields: Vec<(String, String)> =
        global.form_values(&global.defaults()).into_iter().collect();
    fields.retain(|(k, _)| k != "daily_xfer_period_days");
    for (value, status) in [("1.5", 400), ("2", 303)] {
        let mut sent = fields.clone();
        sent.push(("daily_xfer_period_days".into(), value.into()));
        let pairs: Vec<(&str, &str)> = sent.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let (page, _) = w.submit(&form, "/prefs_edit.php", auth, &pairs);
        assert_eq!(page.status, status, "{value}");
    }
}

#[test]
fn clearing_every_application_is_refused() {
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let auth = Some(ada.auth.as_str());
    let form = w.get("/prefs_edit.php", auth, &[("subset", "project")]);
    // A browser sends no field for an unticked box; resource_share and
    // max_jobs stay as the form had them.
    let (page, _) = w.submit(
        &form,
        "/prefs_edit.php",
        auth,
        &[("resource_share", "400"), ("max_jobs", "")],
    );
    assert_eq!(page.status, 400);
    assert!(page.body.contains("choose at least one application"));
    assert!(page.body.contains("name=\"resource_share\" value=\"400\""));
    assert!(!page.body.contains("name=\"app_id_1\" value=\"1\" checked"));
    assert_eq!(w.user(ada.id).project_prefs, "");
    // One application is a selection.
    let (page, _) = w.submit(
        &form,
        "/prefs_edit.php",
        auth,
        &[
            ("resource_share", "400"),
            ("max_jobs", ""),
            ("app_id_1", "1"),
        ],
    );
    assert_eq!(page.status, 303);
    assert!(w
        .user(ada.id)
        .project_prefs
        .contains("<apps_selected>\n<app_id>1</app_id>\n</apps_selected>"));
}

/// The statement of another request that lands between the read and the
/// write of the one under test.
fn changed_meanwhile(w: &World, sql: Stmt) -> ChangedMeanwhile<'_> {
    ChangedMeanwhile {
        inner: &w.db,
        other: RefCell::new(vec![sql]),
        trigger: &["UPDATE user SET"],
        after: false,
    }
}

/// `changed_meanwhile` with several statements, run as one batch: a whole
/// request that commits before this one writes.
fn changed_meanwhile_by(w: &World, stmts: Vec<Stmt>) -> ChangedMeanwhile<'_> {
    ChangedMeanwhile {
        inner: &w.db,
        other: RefCell::new(stmts),
        trigger: &["UPDATE user SET"],
        after: false,
    }
}

/// `changed_meanwhile` for the batch that stores a token or uses one.
fn changed_around_tokens(w: &World, sql: Stmt, after: bool) -> ChangedMeanwhile<'_> {
    ChangedMeanwhile {
        inner: &w.db,
        other: RefCell::new(vec![sql]),
        trigger: &["INSERT INTO token", "UPDATE token SET"],
        after,
    }
}

#[test]
fn preference_saves_do_not_revert_a_change_made_meanwhile() {
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let auth = Some(ada.auth.as_str());
    let global_other =
        "<global_preferences>\n<mod_time>1</mod_time>\n<venue name=\"home\">\n<max_ncpus_pct>11</max_ncpus_pct>\n</venue>\n</global_preferences>\n";
    let project_other = "<project_preferences>\n<project_specific>\n<max_jobs>9</max_jobs>\n</project_specific>\n</project_preferences>\n";

    // Computing preferences, a venue's set: the form is read, then another
    // request saves the home set, then this one writes the work set.
    let form = w.get(
        "/prefs_edit.php",
        auth,
        &[("subset", "global"), ("venue", "work")],
    );
    let global = cb_core::prefs::PrefsForm::new(cb_core::prefs::Subset::Global, &w.project);
    let fields: Vec<(String, String)> =
        global.form_values(&global.defaults()).into_iter().collect();
    let mut sent = hidden_fields(&form.body);
    sent.extend(fields.iter().cloned());
    let pairs: Vec<(&str, &str)> = sent.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let racing = changed_meanwhile(
        &w,
        stmt!(
            "UPDATE user SET global_prefs = ? WHERE id = ?",
            global_other,
            ada.id
        ),
    );
    let (page, _) = w.page_on(&racing, "/prefs_edit.php", auth, true, &pairs);
    assert_eq!(page.status, 409);
    assert!(page.body.contains("Reload the preferences and try again"));
    assert_eq!(w.user(ada.id).global_prefs, global_other);

    // Project preferences, a venue's set and the main set.
    let project = cb_core::prefs::PrefsForm::new(cb_core::prefs::Subset::Project, &w.project);
    let fields: Vec<(String, String)> = project
        .form_values(&project.defaults())
        .into_iter()
        .collect();
    for venue in [Some("school"), None] {
        w.sql("UPDATE user SET project_prefs = ''");
        let mut q = vec![("subset", "project")];
        q.extend(venue.map(|v| ("venue", v)));
        let form = w.get("/prefs_edit.php", auth, &q);
        let mut sent = hidden_fields(&form.body);
        sent.extend(fields.iter().cloned());
        let pairs: Vec<(&str, &str)> = sent.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let racing = changed_meanwhile(
            &w,
            stmt!(
                "UPDATE user SET project_prefs = ? WHERE id = ?",
                project_other,
                ada.id
            ),
        );
        let (page, _) = w.page_on(&racing, "/prefs_edit.php", auth, true, &pairs);
        assert_eq!(page.status, 409, "{venue:?}");
        assert_eq!(w.user(ada.id).project_prefs, project_other, "{venue:?}");
        // Nor are the account's settings written by the refused save.
        assert_eq!(w.user(ada.id).show_hosts, 0);
    }

    // Removing a venue's set.
    let stored = "<global_preferences>\n<mod_time>1</mod_time>\n<venue name=\"work\">\n<max_ncpus_pct>20</max_ncpus_pct>\n</venue>\n</global_preferences>\n";
    w.sql(&format!("UPDATE user SET global_prefs = '{stored}'"));
    let confirm = w.get(
        "/prefs_remove.php",
        auth,
        &[("subset", "global"), ("venue", "work")],
    );
    let mut sent = hidden_fields(&confirm.body);
    sent.push(("subset".into(), "global".into()));
    sent.push(("venue".into(), "work".into()));
    let pairs: Vec<(&str, &str)> = sent.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let racing = changed_meanwhile(
        &w,
        stmt!(
            "UPDATE user SET global_prefs = ? WHERE id = ?",
            global_other,
            ada.id
        ),
    );
    let (page, _) = w.page_on(&racing, "/prefs_remove.php", auth, true, &pairs);
    assert_eq!(page.status, 409);
    assert_eq!(w.user(ada.id).global_prefs, global_other);

    // Unhindered, the same save works.
    w.sql(&format!("UPDATE user SET global_prefs = '{stored}'"));
    let (page, n) = w.page("/prefs_remove.php", auth, true, &pairs);
    assert_eq!(page.status, 303);
    assert_eq!(n, 2);
}

#[test]
fn am_set_info_keeps_a_site_change_to_the_stored_project_specific_part() {
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada");
    w.sql("UPDATE user SET project_prefs = '<project_preferences>\n<project_specific>\n<max_jobs>2</max_jobs>\n</project_specific>\n</project_preferences>\n'");
    let site = "<project_preferences>\n<project_specific>\n<max_jobs>8</max_jobs>\n</project_specific>\n</project_preferences>\n";
    let params: Params = [
        ("account_key", ada.auth.as_str()),
        (
            "project_prefs",
            "<project_preferences>\n<resource_share>5</resource_share>\n</project_preferences>\n",
        ),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let racing = changed_meanwhile(
        &w,
        stmt!(
            "UPDATE user SET project_prefs = ? WHERE id = ?",
            site,
            ada.id
        ),
    );
    let reply = block_on(cb_core::web::am_set_info(
        &racing,
        &w.project.config,
        PEPPER,
        NOW,
        &params,
    ))
    .unwrap();
    assert_eq!(error_num(&reply), Some(-199), "{reply}");
    assert_eq!(w.user(ada.id).project_prefs, site);

    // A document with its own `<project_specific>` does not use the stored
    // one, so a change meanwhile is not a conflict.
    let own: Params = [
        ("account_key", ada.auth.as_str()),
        (
            "project_prefs",
            "<project_preferences>\n<project_specific>\n<max_jobs>3</max_jobs>\n</project_specific>\n</project_preferences>\n",
        ),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let racing = changed_meanwhile(
        &w,
        stmt!("UPDATE user SET send_email = 1 WHERE id = ?", ada.id),
    );
    let reply = block_on(cb_core::web::am_set_info(
        &racing,
        &w.project.config,
        PEPPER,
        NOW,
        &own,
    ))
    .unwrap();
    assert!(reply.contains("<success/>"), "{reply}");
    assert!(w
        .user(ada.id)
        .project_prefs
        .contains("<max_jobs>3</max_jobs>"));
}

#[test]
fn a_blank_idle_threshold_stays_blank() {
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let auth = Some(ada.auth.as_str());
    let global = cb_core::prefs::PrefsForm::new(cb_core::prefs::Subset::Global, &w.project);
    let mut fields: Vec<(String, String)> =
        global.form_values(&global.defaults()).into_iter().collect();
    let mut set = |k: &str, v: &str| {
        fields.retain(|(a, _)| a != k);
        fields.push((k.into(), v.into()));
    };
    set("suspend_cpu_usage", "25");
    set("niu_suspend_cpu_usage", "");
    let pairs = |f: &[(String, String)]| -> Vec<(String, String)> { f.to_vec() };
    let form = w.get("/prefs_edit.php", auth, &[("subset", "global")]);
    let send = |form: &SitePage, f: &[(String, String)]| {
        let f = pairs(f);
        let p: Vec<(&str, &str)> = f.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        w.submit(form, "/prefs_edit.php", auth, &p).0
    };
    assert_eq!(send(&form, &fields).status, 303);
    let stored = w.user(ada.id).global_prefs;
    assert!(stored.contains("<niu_suspend_cpu_usage>0</niu_suspend_cpu_usage>"));
    // The form shows it blank, not as the in-use value.
    let form = w.get("/prefs_edit.php", auth, &[("subset", "global")]);
    assert!(form
        .body
        .contains("name=\"niu_suspend_cpu_usage\" value=\"\""));
    assert!(form
        .body
        .contains("name=\"suspend_cpu_usage\" value=\"25\""));
    let shown = w.get("/prefs.php", auth, &[("subset", "global")]);
    // Only the in-use threshold shows 25.
    assert_eq!(shown.body.matches("25 %").count(), 1);
    // Saving something else, as the browser sends the form back, keeps it.
    let mut again = hidden_fields(&form.body);
    again.extend(fields.iter().filter(|(k, _)| k != "max_ncpus_pct").cloned());
    again.push(("max_ncpus_pct".into(), "80".into()));
    let p: Vec<(&str, &str)> = again
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(w.post("/prefs_edit.php", auth, &p).status, 303);
    assert!(w
        .user(ada.id)
        .global_prefs
        .contains("<niu_suspend_cpu_usage>0</niu_suspend_cpu_usage>"));
    // And so does adding and removing a venue.
    let form = w.get(
        "/prefs_edit.php",
        auth,
        &[("subset", "global"), ("venue", "work")],
    );
    let mut work = hidden_fields(&form.body);
    work.extend(fields.iter().cloned());
    let p: Vec<(&str, &str)> = work.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    assert_eq!(w.post("/prefs_edit.php", auth, &p).status, 303);
    let confirm = w.get(
        "/prefs_remove.php",
        auth,
        &[("subset", "global"), ("venue", "work")],
    );
    let (page, _) = w.submit(
        &confirm,
        "/prefs_remove.php",
        auth,
        &[("subset", "global"), ("venue", "work")],
    );
    assert_eq!(page.status, 303);
    let stored = w.user(ada.id).global_prefs;
    assert!(stored.contains("<niu_suspend_cpu_usage>0</niu_suspend_cpu_usage>"));
    assert!(!stored.contains("<niu_suspend_cpu_usage>25"));
}

// ---------------------------------------------------------------------------
// Email.

type Link = (String, Vec<(String, String)>);

/// The link to `page` in a message: its path and query parameters.
fn link(msg: &MailMessage, page: &str) -> Link {
    let prefix = format!("https://p.example/{page}");
    let start = msg
        .body()
        .find(&prefix)
        .unwrap_or_else(|| panic!("no link to {page} in {:?}", msg.body()));
    let url = msg.body()[start..].split_whitespace().next().unwrap();
    let (path, query) = url["https://p.example".len()..].split_once('?').unwrap();
    let pairs = query
        .split('&')
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap();
            (k.to_string(), v.to_string())
        })
        .collect();
    (path.to_string(), pairs)
}

fn param<'a>(link: &'a Link, name: &str) -> &'a str {
    link.1
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
        .unwrap()
}

/// `link` with one parameter changed.
fn with_param(link: &Link, name: &str, value: &str) -> Link {
    let mut out = link.clone();
    for (k, v) in out.1.iter_mut() {
        if k == name {
            *v = value.to_string();
        }
    }
    out
}

/// A token with its last character changed.
fn altered(t: &str) -> String {
    let (head, last) = t.split_at(t.len() - 1);
    format!("{head}{}", if last == "0" { "1" } else { "0" })
}

impl World {
    fn follow(&self, link: &Link, auth: Option<&str>, post: bool) -> SitePage {
        let pairs: Vec<(&str, &str)> = link
            .1
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        self.page(&link.0, auth, post, &pairs).0
    }

    fn count(&self, sql: &str, p: &str) -> i64 {
        block_on(self.db.run(stmt!(sql, p)))
            .unwrap()
            .scalar_i64()
            .unwrap()
    }

    /// Asks for a recovery message for `email`.
    fn request_login_link(&self, email: &str) -> SitePage {
        self.post("/mail_passwd.php", None, &[("email_addr", email)])
    }

    fn login_by_password(&self, email: &str, pw: &str) -> u16 {
        self.post(
            "/login_action.php",
            None,
            &[("email_addr", email), ("passwd", pw)],
        )
        .status
    }
}

fn logged_in_as(page: &SitePage, account: &Account) -> bool {
    page.status == 303
        && page
            .set_cookie
            .as_deref()
            .is_some_and(|c| c.starts_with(&format!("auth={};", account.auth)))
}

#[test]
fn pages_that_need_email_say_so_without_a_mailer() {
    let no_sender = World::new();
    no_sender.use_mail.set(true); // a transport, but no `email_from`
    for w in [World::new(), no_sender] {
        let ada = w.account("ada@example.com", "secret", "Ada");
        let t = "a".repeat(64);
        for (path, post, params) in [
            ("/get_passwd.php", false, vec![]),
            (
                "/mail_passwd.php",
                true,
                vec![("email_addr", "ada@example.com")],
            ),
            ("/validate_email_addr.php", false, vec![]),
            (
                "/validate_email_addr.php",
                false,
                vec![("validate", "1"), ("u", "1"), ("x", t.as_str())],
            ),
            (
                "/recover_email.php",
                false,
                vec![("id", "1"), ("token", t.as_str())],
            ),
            (
                "/login_action.php",
                false,
                vec![("id", "1"), ("t", t.as_str())],
            ),
        ] {
            let page = w.page(path, Some(&ada.auth), post, &params).0;
            assert_eq!(page.status, 404, "{path}");
            assert!(page.body.contains("does not send email"), "{path}");
            assert!(page.set_cookie.is_none());
        }
        assert!(w.mails().is_empty());
        assert_eq!(
            w.count("SELECT count(*) AS n FROM token WHERE type != ?", "x"),
            0
        );
        assert!(!w
            .get("/login_form.php", None, &[])
            .body
            .contains("get_passwd.php"));
        let home = w.get("/home.php", Some(&ada.auth), &[]);
        assert!(!home.body.contains("validate_email_addr.php"));
        assert!(!home.body.contains("not verified"));
        let form = w.get("/edit_email_form.php", Some(&ada.auth), &[]);
        assert!(form.body.contains("No message is sent"));
        // A change of address is as it was: no wait, nothing kept.
        for (new, pw) in [
            ("ada2@example.com", "secret"),
            ("ada3@example.com", "secret"),
        ] {
            let (page, _) = w.submit(
                &form,
                "/edit_email_action.php",
                Some(&ada.auth),
                &[("email_addr", new), ("passwd", pw)],
            );
            assert_eq!(page.status, 200, "{new}");
        }
        let u = w.user(ada.id);
        assert_eq!(u.email_addr, "ada3@example.com");
        assert_eq!(
            (u.previous_email_addr.as_str(), u.email_addr_change_time),
            ("", 0)
        );
        assert!(w.mails().is_empty());
    }
}

#[test]
fn a_recovery_link_logs_in_once() {
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    assert!(w
        .get("/login_form.php", None, &[])
        .body
        .contains("get_passwd.php"));
    let form = w.get("/get_passwd.php", None, &[]);
    assert_eq!(form.status, 200);
    assert!(form.body.contains("action=\"mail_passwd.php\""));
    // A GET sends nothing.
    assert_eq!(
        w.get(
            "/mail_passwd.php",
            None,
            &[("email_addr", "ada@example.com")]
        )
        .status,
        405
    );
    assert!(w.mails().is_empty());

    let asked = w.request_login_link("Ada@Example.com");
    assert_eq!(asked.status, 200);
    let mails = w.mails();
    assert_eq!(mails.len(), 1);
    let m = &mails[0];
    assert_eq!(m.to(), "ada@example.com");
    assert_eq!(m.from_addr(), "boinc@p.example");
    assert_eq!(m.from_name(), "Test Project");
    assert_eq!(m.subject(), "Test Project account information");
    let l = link(m, "login_action.php");
    assert_eq!(l.0, "/login_action.php");
    assert_eq!(param(&l, "id"), ada.id.to_string());
    let token = param(&l, "t").to_string();
    assert_eq!(token.len(), 64);
    assert!(m.body().contains("valid for 1 day"));
    // Only a hash of the token is stored.
    assert_eq!(
        w.count(
            "SELECT count(*) AS n FROM token WHERE token_hash = ?",
            &token
        ),
        0
    );
    assert_eq!(
        w.count("SELECT count(*) AS n FROM token WHERE type != ?", "x"),
        1
    );

    // A GET (a mail scanner, a preview) shows a button and uses nothing up.
    let shown = w.follow(&l, None, false);
    assert_eq!(shown.status, 200);
    assert!(shown.set_cookie.is_none());
    assert!(shown.body.contains("method=\"post\""));
    // The POST of that button logs in.
    let ok = w.follow(&l, None, true);
    assert!(logged_in_as(&ok, &ada));
    assert_eq!(ok.location.as_deref(), Some("https://p.example/home.php"));
    assert!(ok.set_cookie.unwrap().ends_with("; Max-Age=31536000"));
    let home = w.get("/home.php", Some(&ada.auth), &[]);
    assert!(home.body.contains("ada@example.com"));
}

#[test]
fn recovery_answers_the_same_for_every_address() {
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let eve = w.account("eve@example.com", "secret", "Eve");
    w.sql(&format!(
        "UPDATE user SET blocked = 1 WHERE id = {}",
        eve.id
    ));
    let known = w.request_login_link("ada@example.com");
    assert_eq!(w.mails().len(), 1);
    w.at(5000);
    let unknown = w.request_login_link("nobody@example.com");
    let blocked = w.request_login_link("eve@example.com");
    assert_eq!(
        w.mails().len(),
        1,
        "nothing for an unknown or blocked account"
    );
    assert_eq!(known.status, 200);
    let generic = |p: &SitePage, addr: &str| p.body.replace(addr, "ADDR");
    assert_eq!(
        (unknown.status, generic(&unknown, "nobody@example.com")),
        (known.status, generic(&known, "ada@example.com"))
    );
    assert_eq!(
        (blocked.status, generic(&blocked, "eve@example.com")),
        (known.status, generic(&known, "ada@example.com"))
    );
    // Asking again inside the limit gets the very same answer, and no mail.
    w.at(5060);
    let again = w.request_login_link("ada@example.com");
    assert_eq!(w.mails().len(), 2);
    let limited = w.request_login_link("ada@example.com");
    assert_eq!(w.mails().len(), 2);
    assert_eq!((again.status, &again.body), (limited.status, &limited.body));
    assert_eq!(
        again.body.replace("ada@example.com", "ADDR"),
        generic(&known, "ada@example.com")
    );
    // A malformed address is refused the same way whoever has it.
    assert_eq!(w.request_login_link("").status, 400);
    assert_eq!(w.request_login_link("ada").status, 400);
    let _ = ada;
}

#[test]
fn expired_reused_and_altered_login_links_are_refused() {
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let bob = w.account("bob@example.com", "secret", "Bob");
    let refused = |page: &SitePage| {
        assert_eq!(page.status, 403);
        assert!(page.set_cookie.is_none());
        assert!(page.body.contains("get_passwd.php"));
    };
    // Expired after a day, to the second.
    w.request_login_link("ada@example.com");
    let l1 = link(&w.mails()[0], "login_action.php");
    w.at(86_400);
    refused(&w.follow(&l1, None, true));

    // Valid up to the last second, once.
    w.request_login_link("ada@example.com");
    let l2 = link(&w.mails()[1], "login_action.php");
    w.at(86_400 + 86_399);
    assert!(logged_in_as(&w.follow(&l2, None, true), &ada));
    refused(&w.follow(&l2, None, true));
    // Nor does it get as far as the button.
    refused(&w.follow(&l2, None, false));

    // Altered tokens and ids.
    w.at(200_000);
    w.request_login_link("ada@example.com");
    let l3 = link(&w.mails()[2], "login_action.php");
    let t = param(&l3, "t").to_string();
    refused(&w.follow(&with_param(&l3, "t", &altered(&t)), None, true));
    assert_ne!(t.to_uppercase(), t);
    refused(&w.follow(&with_param(&l3, "t", &t.to_uppercase()), None, true));
    refused(&w.follow(&with_param(&l3, "t", &t[..63]), None, true));
    refused(&w.follow(&with_param(&l3, "t", &format!("{t}0")), None, true));
    refused(&w.follow(&with_param(&l3, "id", &bob.id.to_string()), None, true));
    refused(&w.follow(&with_param(&l3, "id", "x"), None, true));
    refused(&w.follow(&with_param(&l3, "id", "0"), None, true));
    // The right one still works after all that.
    assert!(logged_in_as(&w.follow(&l3, None, true), &ada));

    // A newer message does not end an older link that was not used: asking
    // for one for someone else's account must not be a way to cancel theirs.
    w.at(300_000);
    w.request_login_link("ada@example.com");
    w.at(300_601);
    w.request_login_link("ada@example.com");
    let (l4, l5) = (
        link(&w.mails()[3], "login_action.php"),
        link(&w.mails()[4], "login_action.php"),
    );
    assert!(logged_in_as(&w.follow(&l5, None, true), &ada));
    assert!(logged_in_as(&w.follow(&l4, None, true), &ada));
    refused(&w.follow(&l4, None, true));

    // A blocked account does not log in, with a good link.
    w.at(400_000);
    w.request_login_link("bob@example.com");
    let lb = link(&w.mails()[5], "login_action.php");
    w.sql("UPDATE user SET blocked = 1 WHERE name = 'Bob'");
    refused(&w.follow(&lb, None, true));
}

#[test]
fn at_most_one_recovery_mail_every_ten_minutes() {
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    w.request_login_link("ada@example.com");
    w.at(599);
    w.request_login_link("ada@example.com");
    assert_eq!(w.mails().len(), 1);
    w.at(600);
    w.request_login_link("ada@example.com");
    assert_eq!(w.mails().len(), 2);
    // Using the link does not clear the limit.
    let l = link(&w.mails()[1], "login_action.php");
    assert!(logged_in_as(&w.follow(&l, None, true), &ada));
    w.at(601);
    w.request_login_link("ada@example.com");
    assert_eq!(w.mails().len(), 2);
    w.at(1200);
    w.request_login_link("ada@example.com");
    assert_eq!(w.mails().len(), 3);
    // A message the transport refuses still uses the limit (it may have got
    // there, and its link is kept), and the page does not tell.
    w.at(1800);
    w.mail.fail.set(true);
    let failed = w.request_login_link("ada@example.com");
    assert_eq!((failed.status, w.mails().len()), (200, 3));
    w.mail.fail.set(false);
    w.at(1900);
    w.request_login_link("ada@example.com");
    assert_eq!(w.mails().len(), 3);
    let kept = block_on(w.db.run(stmt!(
        "SELECT count(*) AS n FROM token WHERE create_time = ?",
        NOW + 1800
    )))
    .unwrap()
    .scalar_i64();
    assert_eq!(kept, Some(1), "the link of the message that failed is kept");
    w.at(2400);
    w.request_login_link("ada@example.com");
    assert_eq!(w.mails().len(), 4);
    // Each message is for its own account: another account is not held back.
    let bob = w.account("bob@example.com", "secret", "Bob");
    w.request_login_link("bob@example.com");
    assert_eq!(w.mails().len(), 5);
    assert_eq!(w.mails()[4].to(), "bob@example.com");
    let _ = bob;
}

#[test]
fn an_address_is_verified_from_the_account_page_and_at_sign_up() {
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let home = w.get("/home.php", Some(&ada.auth), &[]);
    assert!(home.body.contains("not verified"));
    assert!(home.body.contains("action=\"validate_email_addr.php\""));
    assert!(home
        .body
        .contains("<form method=\"post\" action=\"logout.php\">"));

    // No token, no message; a GET shows the button and sends nothing.
    let bare = w.post("/validate_email_addr.php", Some(&ada.auth), &[]);
    assert_eq!(bare.status, 403);
    let shown = w.get("/validate_email_addr.php", Some(&ada.auth), &[]);
    assert_eq!(shown.status, 200);
    assert!(w.mails().is_empty());
    assert_eq!(w.get("/validate_email_addr.php", None, &[]).status, 303);

    let (sent, _) = w.submit(&shown, "/validate_email_addr.php", Some(&ada.auth), &[]);
    assert_eq!(sent.status, 200);
    assert!(sent.body.contains("ada@example.com"));
    let m = w.mails().remove(0);
    assert_eq!(m.to(), "ada@example.com");
    assert_eq!(m.subject(), "Validate BOINC email address");
    let l = link(&m, "validate_email_addr.php");
    assert_eq!(param(&l, "validate"), "1");
    assert_eq!(param(&l, "u"), ada.id.to_string());
    assert_eq!(
        w.count(
            "SELECT count(*) AS n FROM token WHERE token_hash = ?",
            param(&l, "x")
        ),
        0
    );
    // The limit: once every ten minutes per account.
    w.at(599);
    let form = w.get("/validate_email_addr.php", Some(&ada.auth), &[]);
    let (limited, _) = w.submit(&form, "/validate_email_addr.php", Some(&ada.auth), &[]);
    assert_eq!((limited.status, w.mails().len()), (429, 1));
    // Altered, foreign and misshapen links do nothing.
    assert_eq!(w.user(ada.id).email_validated, 0);
    for bad in [
        with_param(&l, "x", &altered(param(&l, "x"))),
        with_param(&l, "u", "9999"),
        with_param(&l, "x", "short"),
        with_param(&l, "u", "x"),
    ] {
        let page = w.follow(&bad, None, false);
        assert!(page.status == 400 || page.status == 403);
    }
    assert_eq!(w.user(ada.id).email_validated, 0);
    // The link needs no login. A GET only shows a button.
    let shown = w.follow(&l, None, false);
    assert_eq!(shown.status, 200);
    assert!(shown.body.contains("method=\"post\""));
    assert_eq!(w.user(ada.id).email_validated, 0);
    let ok = w.follow(&l, None, true);
    assert_eq!(ok.status, 200);
    assert!(ok.body.contains("has been validated"));
    assert_eq!(w.user(ada.id).email_validated, 1);
    // It works once.
    assert_eq!(w.follow(&l, None, true).status, 403);
    assert_eq!(w.follow(&l, None, false).status, 403);
    let home = w.get("/home.php", Some(&ada.auth), &[]);
    assert!(home.body.contains("(verified)"));
    assert!(!home.body.contains("action=\"validate_email_addr.php\""));
    // Nothing more to send once it is verified.
    w.at(2000);
    let done = w.get("/validate_email_addr.php", Some(&ada.auth), &[]);
    assert!(done.body.contains("already validated"));
    assert_eq!(w.mails().len(), 1);

    // After ten minutes another can be sent; the link dies with the address
    // and with time.
    w.sql("UPDATE user SET email_validated = 0");
    w.at(2000);
    let form = w.get("/validate_email_addr.php", Some(&ada.auth), &[]);
    w.submit(&form, "/validate_email_addr.php", Some(&ada.auth), &[]);
    assert_eq!(w.mails().len(), 2);
    let l2 = link(&w.mails()[1], "validate_email_addr.php");
    w.at(2000 + 86_400);
    assert_eq!(w.follow(&l2, None, true).status, 403);
    assert_eq!(w.follow(&l2, None, false).status, 403);
    w.at(2100);
    w.sql("UPDATE user SET email_addr = 'ada2@example.com'");
    assert_eq!(w.follow(&l2, None, true).status, 403);
    assert_eq!(w.user(ada.id).email_validated, 0);

    // Sign-up sends the message, and the link validates.
    let good = [
        ("new_name", "Cy"),
        ("new_email_addr", "Cy@Example.com"),
        ("passwd", "secret1"),
        ("passwd2", "secret1"),
        ("country", "United Kingdom"),
        ("postal_code", ""),
    ];
    let before = w.mails().len();
    let made = w.post("/create_account_action.php", None, &good);
    assert_eq!(made.status, 303);
    assert_eq!(w.mails().len(), before + 1);
    let m = w.mails().pop().unwrap();
    assert_eq!(m.to(), "cy@example.com");
    let cy: i64 = w.count(
        "SELECT id AS n FROM user WHERE email_addr = ?",
        "cy@example.com",
    );
    assert_eq!(w.user(cy).email_validated, 0);
    assert_eq!(
        w.follow(&link(&m, "validate_email_addr.php"), None, true)
            .status,
        200
    );
    assert_eq!(w.user(cy).email_validated, 1);
    // Signing up again with the same password logs in; a verified address
    // gets nothing.
    w.at(5000);
    assert_eq!(
        w.post("/create_account_action.php", None, &good).status,
        303
    );
    assert_eq!(w.mails().len(), before + 1);
    // A transport that refuses does not stop the account from being made.
    w.mail.fail.set(true);
    let dee = [
        ("new_name", "Dee"),
        ("new_email_addr", "dee@example.com"),
        ("passwd", "secret1"),
        ("passwd2", "secret1"),
        ("country", "United Kingdom"),
        ("postal_code", ""),
    ];
    assert_eq!(w.post("/create_account_action.php", None, &dee).status, 303);
    assert_eq!(
        w.count(
            "SELECT count(*) AS n FROM user WHERE email_addr = ?",
            "dee@example.com"
        ),
        1
    );
    assert_eq!(w.mails().len(), before + 1);
}

#[test]
fn an_address_change_tells_both_addresses_and_waits_a_week() {
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let bob = w.account("bob@example.com", "secret", "Bob");
    let _ = bob;
    let form = w.get("/edit_email_form.php", Some(&ada.auth), &[]);
    assert!(form.body.contains("seven days"));
    let change = |to: &str, pw: &str| {
        let form = w.get("/edit_email_form.php", Some(&ada.auth), &[]);
        w.submit(
            &form,
            "/edit_email_action.php",
            Some(&ada.auth),
            &[("email_addr", to), ("passwd", pw)],
        )
        .0
    };
    assert_eq!(change("ada@new.example", "nope").status, 403);
    assert_eq!(change("Bob@example.com", "secret").status, 409);
    assert_eq!(change("ada@example.com", "secret").status, 400);
    assert!(w.mails().is_empty());

    let ok = change("Ada@New.example", "secret");
    assert_eq!(ok.status, 200);
    assert!(ok.body.contains("ada@new.example"));
    assert!(!ok.body.contains("unable to send"));
    let mails = w.mails();
    assert_eq!(mails.len(), 2);
    let to_new = mails.iter().find(|m| m.to() == "ada@new.example").unwrap();
    let to_old = mails.iter().find(|m| m.to() == "ada@example.com").unwrap();
    assert_eq!(to_new.subject(), "Test Project email address change.");
    // The notice to the new address does not name the old one: anyone can
    // make the site mail an address they do not own.
    assert!(!to_new.body().contains("ada@example.com"));
    assert!(to_new.body().contains("If that was not you"));
    assert!(!to_new.body().contains("recover_email"));
    assert!(to_old
        .body()
        .contains("from ada@example.com to ada@new.example"));
    assert!(to_old
        .body()
        .contains("https://p.example/reset_account_key.php"));
    let undo = link(to_old, "recover_email.php");
    assert_eq!(param(&undo, "id"), ada.id.to_string());
    let u = w.user(ada.id);
    assert_eq!(
        (
            u.email_addr.as_str(),
            u.previous_email_addr.as_str(),
            u.email_addr_change_time
        ),
        ("ada@new.example", "ada@example.com", NOW)
    );
    // The password works with the new address, as before.
    assert_eq!(w.login_by_password("ada@new.example", "secret"), 303);

    // Another change inside the week is refused, with nothing sent.
    for secs in [1, 3600, 604_799] {
        w.at(secs);
        let again = change("ada@third.example", "secret");
        assert_eq!(again.status, 429, "{secs}");
        assert!(again.body.contains("ada@example.com"));
    }
    assert_eq!(w.mails().len(), 2);
    assert_eq!(w.user(ada.id).email_addr, "ada@new.example");

    // After the week it goes, and the earlier undo link is dead.
    w.at(604_800);
    assert_eq!(change("ada@third.example", "secret").status, 200);
    assert_eq!(w.mails().len(), 4);
    let undo2 = link(
        w.mails()
            .iter()
            .rev()
            .find(|m| m.to() == "ada@new.example")
            .unwrap(),
        "recover_email.php",
    );
    let page = w.follow(&undo, None, true);
    assert_eq!(page.status, 403);
    assert_eq!(w.user(ada.id).email_addr, "ada@third.example");

    // A GET only shows the button.
    let shown = w.follow(&undo2, None, false);
    assert_eq!(shown.status, 200);
    assert!(shown.body.contains("method=\"post\""));
    assert_eq!(w.user(ada.id).email_addr, "ada@third.example");
    // An altered token undoes nothing.
    let t = param(&undo2, "token").to_string();
    assert_eq!(
        w.follow(&with_param(&undo2, "token", &altered(&t)), None, true)
            .status,
        403
    );
    assert_eq!(
        w.follow(&with_param(&undo2, "id", &bob.id.to_string()), None, true)
            .status,
        403
    );
    assert_eq!(w.user(ada.id).email_addr, "ada@third.example");
    // The real one puts the previous address back, logs the browser out and
    // leaves a password nobody knows.
    let back = w.follow(&undo2, Some(&ada.auth), true);
    assert_eq!(back.status, 200);
    assert!(back.set_cookie.as_deref().unwrap().contains("Max-Age=0"));
    assert!(back.body.contains("get_passwd.php"));
    let u = w.user(ada.id);
    assert_eq!(
        (
            u.email_addr.as_str(),
            u.previous_email_addr.as_str(),
            u.email_addr_change_time,
            u.email_validated
        ),
        ("ada@new.example", "", NOW + 604_800, 0)
    );
    assert_eq!(w.login_by_password("ada@new.example", "secret"), 403);
    assert_eq!(w.login_by_password("ada@new.example", &ada.auth), 303);
    // It works once.
    assert_eq!(w.follow(&undo2, None, true).status, 403);
    // The way back in is a login link.
    w.request_login_link("ada@new.example");
    let l = link(w.mails().last().unwrap(), "login_action.php");
    assert!(logged_in_as(&w.follow(&l, None, true), &ada));
    // The wait runs from the undo, so the change cannot be made again at once.
    assert_eq!(change("ada@fourth.example", "secret").status, 429);
    assert_eq!(w.user(ada.id).email_addr_change_time, NOW + 604_800);
}

#[test]
fn undoing_a_change_stops_when_another_account_has_the_address() {
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let form = w.get("/edit_email_form.php", Some(&ada.auth), &[]);
    w.submit(
        &form,
        "/edit_email_action.php",
        Some(&ada.auth),
        &[("email_addr", "ada@new.example"), ("passwd", "secret")],
    );
    let undo = link(
        w.mails()
            .iter()
            .find(|m| m.to() == "ada@example.com")
            .unwrap(),
        "recover_email.php",
    );
    // The site no longer lets anyone register the freed address, but a row
    // that got there some other way (an import) must still stop the undo.
    w.sql(
        "INSERT INTO user (create_time, email_addr, name, authenticator, cross_project_id) \
         VALUES (1, 'ada@example.com', 'Other', 'abababababababababababababababab', 'x')",
    );
    let refused = w.follow(&undo, None, true);
    assert_eq!(refused.status, 403);
    assert!(refused.body.contains("another account"));
    assert_eq!(w.user(ada.id).email_addr, "ada@new.example");
    assert_eq!(w.user(ada.id).previous_email_addr, "ada@example.com");
    assert_eq!(
        w.count(
            "SELECT count(*) AS n FROM token WHERE used_time = 0 AND type = ?",
            "E"
        ),
        1,
        "the link was not used up"
    );
    // Once the address is free again it works.
    w.sql("DELETE FROM user WHERE name = 'Other'");
    assert_eq!(w.follow(&undo, None, true).status, 200);
    assert_eq!(w.user(ada.id).email_addr, "ada@example.com");
}

#[test]
fn undoing_a_change_ends_the_login_links_made_meanwhile() {
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let bob = w.account("bob@example.com", "secret", "Bob");
    let form = w.get("/edit_email_form.php", Some(&ada.auth), &[]);
    w.submit(
        &form,
        "/edit_email_action.php",
        Some(&ada.auth),
        &[("email_addr", "ada@new.example"), ("passwd", "secret")],
    );
    let undo = link(
        w.mails()
            .iter()
            .find(|m| m.to() == "ada@example.com")
            .unwrap(),
        "recover_email.php",
    );
    // Whoever holds the new address asks for a login link.
    w.request_login_link("ada@new.example");
    let login = link(w.mails().last().unwrap(), "login_action.php");
    assert_eq!(w.follow(&undo, None, true).status, 200);
    assert_eq!(w.follow(&login, None, true).status, 403);

    // A request with a made-up token does nothing to anyone's links.
    w.at(5000);
    w.request_login_link("bob@example.com");
    let bobs = link(w.mails().last().unwrap(), "login_action.php");
    let junk: Link = (
        "/recover_email.php".into(),
        vec![
            ("id".into(), bob.id.to_string()),
            ("token".into(), "0".repeat(64)),
        ],
    );
    assert_eq!(w.follow(&junk, None, true).status, 403);
    assert!(logged_in_as(&w.follow(&bobs, None, true), &bob));
}

#[test]
fn a_name_or_address_cannot_add_mail_headers() {
    // Nothing a user types, and nothing in the config, can end a header.
    let config = MAIL_CONFIG
        .replace(
            "<long_name>Test Project</long_name>",
            "<long_name>Test\r\nBcc: evil@example.org</long_name>",
        )
        .replace(
            "<email_from_name>Test Project</email_from_name>",
            "<email_from_name>Test\r\nBcc: evil@example.org</email_from_name>",
        );
    let w = World::with_config(&config);
    w.use_mail.set(true);
    let ada = w.account("ada@example.com", "secret", "Ada\r\nBcc: evil@example.org");
    w.request_login_link("ada@example.com");
    let home = w.get("/home.php", Some(&ada.auth), &[]);
    w.submit(&home, "/validate_email_addr.php", Some(&ada.auth), &[]);
    let form = w.get("/edit_email_form.php", Some(&ada.auth), &[]);
    w.submit(
        &form,
        "/edit_email_action.php",
        Some(&ada.auth),
        &[("email_addr", "ada@new.example"), ("passwd", "secret")],
    );
    let mails = w.mails();
    assert_eq!(mails.len(), 4);
    for m in &mails {
        assert!(["ada@example.com", "ada@new.example"].contains(&m.to()));
        for line in [m.subject(), m.from_name(), m.to(), m.from_addr()] {
            assert!(!line.contains(['\r', '\n', '\0']), "{line:?}");
        }
        assert!(!m.from_name().contains(['@', ':', '<', '>']));
        assert!(!m.body().contains('\r'));
    }

    // Addresses that are more than one plain address never reach a message.
    let before = w.mails().len();
    w.at(10_000);
    let form = w.get("/edit_email_form.php", Some(&ada.auth), &[]);
    for bad in [
        "x@example.com\r\nBcc: evil@example.org",
        "x@example.com,evil@example.org",
        "x@example.com\nBcc: evil@example.org",
        "<x@example.com>",
        "x@example.com evil@example.org",
        "\"x\"@example.com",
    ] {
        assert_eq!(w.request_login_link(bad).status, 400, "{bad:?}");
        let (page, _) = w.submit(
            &form,
            "/edit_email_action.php",
            Some(&ada.auth),
            &[("email_addr", bad), ("passwd", "secret")],
        );
        assert_eq!(page.status, 400, "{bad:?}");
        let signup = w.post(
            "/create_account_action.php",
            None,
            &[
                ("new_name", "Zed"),
                ("new_email_addr", bad),
                ("passwd", "secret1"),
                ("passwd2", "secret1"),
            ],
        );
        assert_eq!(signup.status, 400, "{bad:?}");
    }
    assert_eq!(w.mails().len(), before);

    // An address already stored with a line break (an import, say) gets no
    // message either: the transport refuses nothing because it is never asked.
    w.sql("UPDATE user SET email_addr = 'x@example.com' || char(13) || char(10) || 'Bcc: evil@example.org', email_validated = 0");
    w.at(20_000);
    let home = w.get("/home.php", Some(&ada.auth), &[]);
    let (page, _) = w.submit(&home, "/validate_email_addr.php", Some(&ada.auth), &[]);
    assert_eq!(page.status, 503);
    assert_eq!(w.mails().len(), before);

    // The sender settings are checked when they are saved.
    for bad in [
        "a@x.example, b@x.example",
        "a@x.example\r\nBcc: b@x.example",
        "a@x.example b@x.example",
    ] {
        let xml = format!(
            "<config><master_url>https://p.example/</master_url><email_from>{bad}</email_from></config>"
        );
        assert!(
            block_on(put_setting(&w.db, SETTING_CONFIG, &xml, 20)).is_err(),
            "{bad:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Email: fixes from review.

impl World {
    fn change_email(&self, auth: &str, to: &str, pw: &str) -> SitePage {
        let form = self.get("/edit_email_form.php", Some(auth), &[]);
        self.submit(
            &form,
            "/edit_email_action.php",
            Some(auth),
            &[("email_addr", to), ("passwd", pw)],
        )
        .0
    }

    /// The link to `page` in the last message to `to`.
    fn last_link(&self, to: &str, page: &str) -> Link {
        let mails = self.mails();
        let m = mails.iter().rev().find(|m| m.to() == to).unwrap();
        link(m, page)
    }

    fn token_rows(&self, kind: &str) -> i64 {
        self.count("SELECT count(*) AS n FROM token WHERE type = ?", kind)
    }
}

fn refused_with_403(page: &SitePage) {
    assert_eq!(page.status, 403);
    assert!(page.set_cookie.is_none());
}

#[test]
fn the_account_key_can_be_replaced_by_the_session_and_the_token() {
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let weak_before = weak_account_key(&w.user(ada.id));
    assert_eq!(w.get("/reset_account_key.php", None, &[]).status, 303);
    assert_eq!(w.post("/reset_account_key.php", None, &[]).status, 303);

    // A GET says what it does and changes nothing; a POST needs the token.
    let form = w.get("/reset_account_key.php", Some(&ada.auth), &[]);
    assert_eq!(form.status, 200);
    assert!(form.body.contains("attached again"));
    assert!(w
        .get("/home.php", Some(&ada.auth), &[])
        .body
        .contains("reset_account_key.php"));
    assert_eq!(w.user(ada.id).authenticator, ada.auth);
    assert_eq!(
        w.post("/reset_account_key.php", Some(&ada.auth), &[])
            .status,
        403
    );
    let bad = [("tnow", "2000000000"), ("ttok", "0000")];
    assert_eq!(
        w.post("/reset_account_key.php", Some(&ada.auth), &bad)
            .status,
        403
    );
    assert_eq!(w.user(ada.id).authenticator, ada.auth);

    // An address change (so there is an undo link) and a login link.
    w.change_email(&ada.auth, "ada@new.example", "secret");
    let undo = w.last_link("ada@example.com", "recover_email.php");
    w.request_login_link("ada@new.example");
    let login = w.last_link("ada@new.example", "login_action.php");

    let (done, _) = w.submit(&form, "/reset_account_key.php", Some(&ada.auth), &[]);
    assert_eq!(done.status, 200);
    let cookie = done.set_cookie.unwrap();
    let new_key = cookie
        .trim_start_matches("auth=")
        .split(';')
        .next()
        .unwrap()
        .to_string();
    assert_ne!(new_key, ada.auth);
    assert_eq!(new_key.len(), 32);
    assert_eq!(w.user(ada.id).authenticator, new_key);
    assert_ne!(weak_account_key(&w.user(ada.id)), weak_before);

    // The old key is no cookie, no login and no RPC key; the new one is all.
    assert_eq!(w.get("/home.php", Some(&ada.auth), &[]).status, 303);
    assert_eq!(w.get("/home.php", Some(&new_key), &[]).status, 200);
    let by_key = |k: &str| {
        w.post("/login_action.php", None, &[("authenticator", k)])
            .status
    };
    assert_eq!((by_key(&ada.auth), by_key(&new_key)), (403, 303));
    let old_info = block_on(am_get_info(&w.db, &ada.auth)).unwrap();
    assert_eq!(
        error_num(&old_info),
        Some(cb_proto::web_rpc::err::ERR_DB_NOT_FOUND)
    );
    let new_info = block_on(am_get_info(&w.db, &new_key)).unwrap();
    assert!(new_info.contains("<success/>"));
    let found = block_on(cb_core::web::lookup_account(
        &w.db,
        PEPPER,
        "ada@new.example",
        &passwd_hash("secret", "ada@new.example"),
    ))
    .unwrap();
    assert!(found.contains(&new_key) && !found.contains(&ada.auth));
    // The form of the old key is no good with the new one.
    assert_eq!(
        w.submit(&form, "/reset_account_key.php", Some(&new_key), &[])
            .0
            .status,
        403
    );

    // Unused login links end; the undo link of the address change is left,
    // since whoever has the key could otherwise end it.
    refused_with_403(&w.follow(&login, None, true));
    assert_eq!(w.follow(&undo, None, true).status, 200);

    // Without email it is the same page.
    let w = World::new();
    let bob = w.account("bob@example.com", "secret", "Bob");
    let form = w.get("/reset_account_key.php", Some(&bob.auth), &[]);
    let (done, _) = w.submit(&form, "/reset_account_key.php", Some(&bob.auth), &[]);
    assert_eq!(done.status, 200);
    assert_ne!(w.user(bob.id).authenticator, bob.auth);
}

#[test]
fn the_address_an_account_left_is_kept_for_its_undo_link() {
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let bob = w.account("bob@example.com", "secret", "Bob");
    // Someone with a session changes the address, then tries to take the old
    // one so that the undo cannot work.
    assert_eq!(
        w.change_email(&ada.auth, "x@evil.example", "secret").status,
        200
    );
    let undo = w.last_link("ada@example.com", "recover_email.php");
    let signup = [
        ("new_name", "Mallory"),
        ("new_email_addr", "Ada@Example.com"),
        ("passwd", "secret1"),
        ("passwd2", "secret1"),
        ("country", "United Kingdom"),
        ("postal_code", ""),
    ];
    assert_eq!(
        w.post("/create_account_action.php", None, &signup).status,
        409
    );
    let random = || w.rand();
    let rpc = block_on(make_user(
        &w.db,
        PEPPER,
        NOW,
        &random,
        &NewAccount {
            email_addr: "ada@example.com",
            passwd_hash: &passwd_hash("secret1", "ada@example.com"),
            user_name: "Mallory",
            ..Default::default()
        },
    ))
    .unwrap();
    assert!(rpc.is_err());
    assert_eq!(
        w.change_email(&bob.auth, "ada@example.com", "secret")
            .status,
        409
    );
    let (reply, _) = am_set_info(
        &w,
        &[
            ("account_key", &bob.auth),
            ("email_addr", "ada@example.com"),
        ],
    );
    assert_eq!(
        error_num(&reply),
        Some(cb_proto::web_rpc::err::ERR_DB_NOT_UNIQUE)
    );
    assert_eq!(
        w.count(
            "SELECT count(*) AS n FROM user WHERE email_addr = ?",
            "ada@example.com"
        ),
        0
    );
    // So the undo works.
    assert_eq!(w.follow(&undo, None, true).status, 200);
    assert_eq!(w.user(ada.id).email_addr, "ada@example.com");

    // The hold lasts the week of the undo link and no longer.
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    w.change_email(&ada.auth, "x@evil.example", "secret");
    w.at(604_799);
    assert_eq!(
        w.post("/create_account_action.php", None, &signup).status,
        409
    );
    w.at(604_800);
    assert_eq!(
        w.post("/create_account_action.php", None, &signup).status,
        303
    );
}

#[test]
fn login_links_end_with_a_change_of_address_or_password() {
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    w.request_login_link("ada@example.com");
    let l1 = w.last_link("ada@example.com", "login_action.php");
    assert_eq!(
        w.change_email(&ada.auth, "ada@new.example", "secret")
            .status,
        200
    );
    refused_with_403(&w.follow(&l1, None, true));

    w.at(700);
    w.request_login_link("ada@new.example");
    let l2 = w.last_link("ada@new.example", "login_action.php");
    let form = w.get("/edit_passwd_form.php", Some(&ada.auth), &[]);
    let (changed, _) = w.submit(
        &form,
        "/edit_passwd_action.php",
        Some(&ada.auth),
        &[
            ("old_passwd", "secret"),
            ("passwd", "better1"),
            ("passwd2", "better1"),
        ],
    );
    assert_eq!(changed.status, 200);
    refused_with_403(&w.follow(&l2, None, true));

    // The account manager RPC does the same.
    w.at(1400);
    w.request_login_link("ada@new.example");
    let l3 = w.last_link("ada@new.example", "login_action.php");
    let (reply, _) = am_set_info(
        &w,
        &[
            ("account_key", &ada.auth),
            ("password_hash", &passwd_hash("best12", "ada@new.example")),
        ],
    );
    assert!(reply.contains("<success/>"), "{reply}");
    refused_with_403(&w.follow(&l3, None, true));
    // A link made after the change is good.
    w.at(2100);
    w.request_login_link("ada@new.example");
    let l4 = w.last_link("ada@new.example", "login_action.php");
    assert!(logged_in_as(&w.follow(&l4, None, true), &ada));
}

#[test]
fn a_failed_send_keeps_the_limit_and_the_link() {
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    w.mail.fail.set(true);
    let home = w.get("/home.php", Some(&ada.auth), &[]);
    let (failed, _) = w.submit(&home, "/validate_email_addr.php", Some(&ada.auth), &[]);
    assert_eq!(failed.status, 503);
    assert_eq!(w.token_rows("V"), 1);
    w.mail.fail.set(false);
    w.at(100);
    let home = w.get("/home.php", Some(&ada.auth), &[]);
    let (again, _) = w.submit(&home, "/validate_email_addr.php", Some(&ada.auth), &[]);
    assert_eq!((again.status, w.mails().len()), (429, 0));
    w.at(600);
    let home = w.get("/home.php", Some(&ada.auth), &[]);
    let (sent, _) = w.submit(&home, "/validate_email_addr.php", Some(&ada.auth), &[]);
    assert_eq!((sent.status, w.mails().len()), (200, 1));
}

#[test]
fn a_daily_cap_limits_all_the_mail_of_the_project() {
    let config = MAIL_CONFIG.replace(
        "<email_from_name>",
        "<email_max_per_day>2</email_max_per_day>\n<email_from_name>",
    );
    let w = World::with_config(&config);
    w.use_mail.set(true);
    for who in ["ada", "bob", "cy"] {
        w.account(&format!("{who}@example.com"), "secret", who);
    }
    let a = w.request_login_link("ada@example.com");
    let b = w.request_login_link("bob@example.com");
    let c = w.request_login_link("cy@example.com");
    assert_eq!(w.mails().len(), 2);
    // The page is the one for a mail that did not get sent, or did.
    assert_eq!(
        (c.status, c.body.replace("cy@", "ada@")),
        (a.status, a.body.clone())
    );
    assert_eq!(b.status, 200);
    // Any other kind counts too.
    w.at(700);
    let ada_key = block_on(w.db.run(stmt!("SELECT authenticator FROM user WHERE name = 'ada'")))
        .unwrap()
        .first::<cb_core::model::User>()
        .unwrap()
        .unwrap()
        .authenticator;
    let home = w.get("/home.php", Some(&ada_key), &[]);
    let (capped, _) = w.submit(&home, "/validate_email_addr.php", Some(&ada_key), &[]);
    assert_eq!((capped.status, w.mails().len()), (503, 2));
    // The next UTC day starts 73,600 s after the start of the test.
    w.at(73_599);
    w.request_login_link("bob@example.com");
    assert_eq!(w.mails().len(), 2);
    w.at(73_600);
    w.request_login_link("cy@example.com");
    assert_eq!(w.mails().len(), 3);
    assert_eq!(w.mails()[2].to(), "cy@example.com");

    // A cap of 0 is no mail at all.
    let none = World::with_config(&MAIL_CONFIG.replace(
        "<email_from_name>",
        "<email_max_per_day>0</email_max_per_day>\n<email_from_name>",
    ));
    none.use_mail.set(true);
    none.account("ada@example.com", "secret", "Ada");
    let page = none.request_login_link("ada@example.com");
    assert_eq!(page.status, 404);
    assert!(page.body.contains("does not send email"));
    assert!(none.mails().is_empty());
}

#[test]
fn an_old_undo_link_does_not_come_back_when_mail_is_switched_on() {
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    assert_eq!(
        w.change_email(&ada.auth, "ada@b.example", "secret").status,
        200
    );
    let old = w.last_link("ada@example.com", "recover_email.php");
    w.use_mail.set(false);
    assert_eq!(
        w.change_email(&ada.auth, "ada@c.example", "secret").status,
        200
    );
    // The state of the first change is left (it still reserves its address
    // for its week), and its undo link is ended instead.
    let u = w.user(ada.id);
    assert_eq!(
        (u.previous_email_addr.as_str(), u.email_addr_change_time),
        ("ada@example.com", NOW)
    );
    w.use_mail.set(true);
    assert_eq!(w.follow(&old, None, true).status, 403);
    assert_eq!(w.user(ada.id).email_addr, "ada@c.example");
    let signup = [
        ("new_name", "Mallory"),
        ("new_email_addr", "ada@example.com"),
        ("passwd", "secret1"),
        ("passwd2", "secret1"),
        ("country", "United Kingdom"),
        ("postal_code", ""),
    ];
    assert_eq!(
        w.post("/create_account_action.php", None, &signup).status,
        409
    );
}

#[test]
fn the_change_and_its_undo_token_are_stored_together() {
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    w.mail.fail.set(true);
    let page = w.change_email(&ada.auth, "ada@new.example", "secret");
    assert_eq!(page.status, 200);
    assert!(page.body.contains("unable to send"));
    assert_eq!(w.user(ada.id).previous_email_addr, "ada@example.com");
    assert_eq!(w.token_rows("E"), 1);
    // A change that did not happen leaves no token.
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    w.account("bob@example.com", "secret", "Bob");
    assert_eq!(
        w.change_email(&ada.auth, "bob@example.com", "secret")
            .status,
        409
    );
    assert_eq!(w.token_rows("E"), 0);
}

#[test]
fn new_addresses_must_be_well_formed_but_existing_ones_keep_working() {
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    // Made under the looser rule of the RPC.
    let loose = w.account("a%b@example.com", "secret", "Loose");
    for bad in [
        "a%b@example.org",
        "-a@example.org",
        ".a@example.org",
        "a..b@example.org",
        "a.@example.org",
        "a@127.0.0.1",
        "a@-x.example",
        "a@x-.example",
        "a@x..example",
        "a@example",
    ] {
        let signup = w.post(
            "/create_account_action.php",
            None,
            &[
                ("new_name", "Zed"),
                ("new_email_addr", bad),
                ("passwd", "secret1"),
                ("passwd2", "secret1"),
            ],
        );
        assert_eq!(signup.status, 400, "{bad}");
        assert_eq!(
            w.change_email(&ada.auth, bad, "secret").status,
            400,
            "{bad}"
        );
    }
    assert_eq!(w.user(ada.id).email_addr, "ada@example.com");
    assert_eq!(w.login_by_password("a%b@example.com", "secret"), 303);
    // It is not mailed.
    let page = w.request_login_link("a%b@example.com");
    assert_eq!((page.status, w.mails().len()), (200, 0));
    let _ = loose;
}

#[test]
fn site_pages_are_sent_without_a_referrer() {
    use cb_core::site::SITE_HEADERS;
    assert!(SITE_HEADERS.contains(&("Referrer-Policy", "no-referrer")));
    assert!(SITE_HEADERS.contains(&("X-Frame-Options", "DENY")));
    assert!(SITE_HEADERS.contains(&("Content-Security-Policy", "frame-ancestors 'none'")));
    assert!(SITE_HEADERS.contains(&("X-Content-Type-Options", "nosniff")));
}

#[test]
fn a_token_works_only_on_the_page_of_its_kind() {
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    assert_eq!(
        w.change_email(&ada.auth, "ada@new.example", "secret")
            .status,
        200
    );
    let undo = w.last_link("ada@example.com", "recover_email.php");
    let home = w.get("/home.php", Some(&ada.auth), &[]);
    w.submit(&home, "/validate_email_addr.php", Some(&ada.auth), &[]);
    let verify = w.last_link("ada@new.example", "validate_email_addr.php");
    w.request_login_link("ada@new.example");
    let login = w.last_link("ada@new.example", "login_action.php");
    let id = ada.id.to_string();
    let (l, v, e) = (
        param(&login, "t").to_string(),
        param(&verify, "x").to_string(),
        param(&undo, "token").to_string(),
    );
    for post in [false, true] {
        for other in [&v, &e] {
            let page = w.follow(&with_param(&login, "t", other), None, post);
            refused_with_403(&page);
        }
        for other in [&l, &v] {
            let link: Link = (
                "/recover_email.php".into(),
                vec![("id".into(), id.clone()), ("token".into(), other.clone())],
            );
            refused_with_403(&w.follow(&link, None, post));
        }
        for other in [&l, &e] {
            refused_with_403(&w.follow(&with_param(&verify, "x", other), None, post));
        }
    }
    assert_eq!(w.user(ada.id).email_addr, "ada@new.example");
    assert_eq!(w.user(ada.id).email_validated, 0);
    // Each is good where it belongs.
    assert!(logged_in_as(&w.follow(&login, None, true), &ada));
    assert_eq!(w.follow(&verify, None, true).status, 200);
    assert_eq!(w.user(ada.id).email_validated, 1);
    assert_eq!(w.follow(&undo, None, true).status, 200);
    assert_eq!(w.user(ada.id).email_addr, "ada@example.com");
}

#[test]
fn am_set_info_keeps_the_window_of_an_address_change() {
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    assert_eq!(
        w.change_email(&ada.auth, "ada@b.example", "secret").status,
        200
    );
    let undo = w.last_link("ada@example.com", "recover_email.php");
    // Inside the week the key alone cannot change the address again.
    for secs in [1, 604_799] {
        w.at(secs);
        let (reply, _) = am_set_info(
            &w,
            &[("account_key", &ada.auth), ("email_addr", "ada@c.example")],
        );
        assert!(reply.contains("<error_num>"), "{reply}");
        let u = w.user(ada.id);
        assert_eq!(
            (
                u.email_addr.as_str(),
                u.previous_email_addr.as_str(),
                u.email_addr_change_time
            ),
            ("ada@b.example", "ada@example.com", NOW),
            "{secs}"
        );
    }
    // A change of password through it leaves the state and the undo link,
    // and ends the login links.
    w.at(700);
    w.request_login_link("ada@b.example");
    let login = w.last_link("ada@b.example", "login_action.php");
    let (reply, _) = am_set_info(
        &w,
        &[
            ("account_key", &ada.auth),
            ("password_hash", &passwd_hash("best12", "ada@b.example")),
        ],
    );
    assert!(reply.contains("<success/>"), "{reply}");
    refused_with_403(&w.follow(&login, None, true));
    assert_eq!(w.user(ada.id).previous_email_addr, "ada@example.com");
    assert_eq!(w.token_rows("E"), 1);
    assert_eq!(
        w.follow(&undo, None, false).status,
        200,
        "the undo link still works"
    );

    // Outside the week it changes the address, records what the site records
    // (the old address stays reserved), and ends the undo links.
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    w.change_email(&ada.auth, "ada@b.example", "secret");
    let undo = w.last_link("ada@example.com", "recover_email.php");
    w.at(604_800);
    let (reply, _) = am_set_info(
        &w,
        &[("account_key", &ada.auth), ("email_addr", "ada@c.example")],
    );
    assert!(reply.contains("<success/>"), "{reply}");
    let u = w.user(ada.id);
    assert_eq!(
        (
            u.email_addr.as_str(),
            u.previous_email_addr.as_str(),
            u.email_addr_change_time
        ),
        ("ada@c.example", "ada@b.example", NOW + 604_800)
    );
    refused_with_403(&w.follow(&undo, None, true));
    let signup = [
        ("new_name", "Mallory"),
        ("new_email_addr", "ada@b.example"),
        ("passwd", "secret1"),
        ("passwd2", "secret1"),
        ("country", "United Kingdom"),
        ("postal_code", ""),
    ];
    assert_eq!(
        w.post("/create_account_action.php", None, &signup).status,
        409
    );
}

#[test]
fn the_notices_go_first_to_the_previous_address_and_are_outside_the_cap() {
    // A cap of 1 is used up by one login-link message; the notices still go.
    let w = World::with_config(&MAIL_CONFIG.replace(
        "<email_from_name>",
        "<email_max_per_day>1</email_max_per_day>\n<email_from_name>",
    ));
    w.use_mail.set(true);
    let ada = w.account("ada@example.com", "secret", "Ada");
    w.account("bob@example.com", "secret", "Bob");
    w.request_login_link("bob@example.com");
    assert_eq!(w.mails().len(), 1);
    w.request_login_link("ada@example.com");
    assert_eq!(w.mails().len(), 1, "the cap is used");
    let page = w.change_email(&ada.auth, "ada@new.example", "secret");
    assert!(!page.body.contains("unable to send"));
    let mails = w.mails();
    assert_eq!(mails.len(), 3);
    assert_eq!(mails[1].to(), "ada@example.com");
    assert!(mails[1].body().contains("recover_email.php"));
    assert_eq!(mails[2].to(), "ada@new.example");
}

#[test]
fn sign_up_mail_may_use_half_of_the_cap() {
    let cap4 = |n: i32| {
        let w = World::with_config(&MAIL_CONFIG.replace(
            "<email_from_name>",
            &format!("<email_max_per_day>{n}</email_max_per_day>\n<email_from_name>"),
        ));
        w.use_mail.set(true);
        w
    };
    let w = cap4(4);
    let mut accounts = 0;
    for who in ["a", "b", "c", "d"] {
        let page = w.post(
            "/create_account_action.php",
            None,
            &[
                ("new_name", who),
                ("new_email_addr", &format!("{who}@example.com")),
                ("passwd", "secret1"),
                ("passwd2", "secret1"),
                ("country", "United Kingdom"),
                ("postal_code", ""),
            ],
        );
        assert_eq!(page.status, 303);
        accounts += 1;
    }
    assert_eq!((accounts, w.mails().len()), (4, 2), "two of four sign-ups");
    // What is left of the cap is for recovery mail.
    w.at(700);
    w.request_login_link("a@example.com");
    w.request_login_link("b@example.com");
    w.request_login_link("c@example.com");
    assert_eq!(w.mails().len(), 4);
    // A cap of 1 leaves no share for sign-up mail.
    let w = cap4(1);
    w.post(
        "/create_account_action.php",
        None,
        &[
            ("new_name", "a"),
            ("new_email_addr", "a@example.com"),
            ("passwd", "secret1"),
            ("passwd2", "secret1"),
            ("country", "United Kingdom"),
            ("postal_code", ""),
        ],
    );
    assert!(w.mails().is_empty());
    w.request_login_link("a@example.com");
    assert_eq!(w.mails().len(), 1);
}

#[test]
fn a_stale_undo_token_ends_nothing_when_the_undo_is_refused() {
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    w.request_login_link("ada@example.com");
    let login = w.last_link("ada@example.com", "login_action.php");
    // An unused undo token the account's state does not match.
    let stale = "ab".repeat(32);
    w.sql(&format!(
        "INSERT INTO token (userid, type, token_hash, data, create_time, expire_time) \
         VALUES ({}, 'E', '{}', 'old@example.com', 1, {})",
        ada.id,
        cb_crypto::account::token_hash(&stale),
        NOW + 1000
    ));
    let undo: Link = (
        "/recover_email.php".into(),
        vec![("id".into(), ada.id.to_string()), ("token".into(), stale)],
    );
    refused_with_403(&w.follow(&undo, None, true));
    assert_eq!(
        w.count(
            "SELECT count(*) AS n FROM token WHERE type = ? AND used_time = 0",
            "E"
        ),
        1,
        "not used up"
    );
    assert!(
        logged_in_as(&w.follow(&login, None, true), &ada),
        "the login link is not ended"
    );
}

#[test]
fn an_address_the_site_would_not_mail_gets_no_token_and_no_cap() {
    let w = World::with_mail();
    let loose = w.account("a%b@example.com", "secret", "Loose");
    let page = w.request_login_link("a%b@example.com");
    assert_eq!(page.status, 200);
    let home = w.get("/home.php", Some(&loose.auth), &[]);
    let (failed, _) = w.submit(&home, "/validate_email_addr.php", Some(&loose.auth), &[]);
    assert_eq!(failed.status, 503);
    assert_eq!(w.token_rows("L") + w.token_rows("V"), 0);
    assert_eq!(
        w.count("SELECT count(*) AS n FROM mail_day WHERE day > ?", "0"),
        0
    );
    assert!(w.mails().is_empty());
}

#[test]
fn the_new_account_key_cookie_is_for_the_session() {
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let form = w.get("/reset_account_key.php", Some(&ada.auth), &[]);
    let (done, _) = w.submit(&form, "/reset_account_key.php", Some(&ada.auth), &[]);
    let cookie = done.set_cookie.unwrap();
    assert!(
        !cookie.contains("Max-Age") && !cookie.contains("Expires"),
        "{cookie}"
    );
    assert!(cookie.contains("HttpOnly") && cookie.contains("Secure"));
}

#[test]
fn the_address_form_says_only_what_the_change_changes() {
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let with_password = w.get("/edit_email_form.php", Some(&ada.auth), &[]);
    assert!(with_password
        .body
        .contains("cross-project ID and weak account key"));
    w.sql("UPDATE user SET passwd_hash = ''");
    let without = w.get("/edit_email_form.php", Some(&ada.auth), &[]);
    assert!(without.body.contains("your cross-project ID."));
    assert!(!without.body.contains("weak account key"));
}

// ---------------------------------------------------------------------------
// Follow-ups to the account and preference pages.

#[test]
fn am_set_info_does_not_undo_a_first_project_specific_block() {
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada");
    w.sql(
        "UPDATE user SET project_prefs = '<project_preferences>\n<resource_share>5</resource_share>\n</project_preferences>\n'",
    );
    let from_the_manager =
        "<project_preferences>\n<resource_share>7</resource_share>\n</project_preferences>\n";
    // A site save adds the first task limit after the RPC read the account:
    // there was no block to keep, but the write must not drop the new one.
    let racing = changed_meanwhile(
        &w,
        stmt!(
            "UPDATE user SET project_prefs = '<project_preferences>\n<resource_share>5</resource_share>\n<project_specific>\n<max_jobs>3</max_jobs>\n</project_specific>\n</project_preferences>\n' WHERE id = ?",
            ada.id
        ),
    );
    let params: Params = [
        ("account_key", ada.auth.as_str()),
        ("project_prefs", from_the_manager),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let reply = block_on(cb_core::web::am_set_info(
        &racing,
        &w.project.config,
        PEPPER,
        NOW,
        &params,
    ))
    .unwrap();
    assert_eq!(error_num(&reply), Some(-199), "{reply}");
    assert!(w
        .user(ada.id)
        .project_prefs
        .contains("<max_jobs>3</max_jobs>"));
    assert!(w.user(ada.id).project_prefs.contains("<resource_share>5<"));
    // Asked again, it writes and keeps the block.
    let (reply, _) = am_set_info(
        &w,
        &[
            ("account_key", &ada.auth),
            ("project_prefs", from_the_manager),
        ],
    );
    assert!(reply.contains("<success/>"), "{reply}");
    let stored = w.user(ada.id).project_prefs;
    assert!(stored.contains("<max_jobs>3</max_jobs>") && stored.contains("<resource_share>7<"));
}

#[test]
fn each_computing_preferences_save_gets_a_later_mod_time() {
    use cb_core::prefs::{PrefsForm, Subset};
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let global = PrefsForm::new(Subset::Global, &w.project);
    let fields: Vec<(String, String)> =
        global.form_values(&global.defaults()).into_iter().collect();
    let pairs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let mod_time = || {
        cb_proto::sched_request::GlobalPrefs::parse(&w.user(ada.id).global_prefs, "").mod_time
            as i64
    };
    let save = |venue: Option<&str>| {
        let mut q = vec![("subset", "global")];
        q.extend(venue.map(|v| ("venue", v)));
        let form = w.get("/prefs_edit.php", Some(&ada.auth), &q);
        assert_eq!(
            w.submit(&form, "/prefs_edit.php", Some(&ada.auth), &pairs)
                .0
                .status,
            303
        );
    };
    // Three saves in the same second: each is newer than the one before,
    // so a client that has the first still takes the next.
    save(None);
    assert_eq!(mod_time(), NOW);
    save(None);
    assert_eq!(mod_time(), NOW + 1);
    save(Some("work"));
    assert_eq!(mod_time(), NOW + 2);
    // So is the removal of a venue's set.
    let confirm = w.get(
        "/prefs_remove.php",
        Some(&ada.auth),
        &[("subset", "global"), ("venue", "work")],
    );
    assert_eq!(
        w.submit(&confirm, "/prefs_remove.php", Some(&ada.auth), &[])
            .0
            .status,
        303
    );
    assert_eq!(mod_time(), NOW + 3);
    // And a later clock still wins.
    w.at(100);
    save(None);
    assert_eq!(mod_time(), NOW + 100);
}

// ---------------------------------------------------------------------------
// Requests under way while the account key or address changes.

const REPLACED_KEY: &str = "ffffffffffffffffffffffffffffffff";

fn key_replaced(id: i64) -> Stmt {
    stmt!(
        "UPDATE user SET authenticator = ? WHERE id = ?",
        REPLACED_KEY,
        id
    )
}

#[test]
fn a_write_that_authenticated_before_a_key_reset_writes_nothing() {
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let auth = Some(ada.auth.as_str());
    let reset = |w: &World| {
        w.sql(&format!(
            "UPDATE user SET authenticator = '{}' WHERE id = {}",
            ada.auth, ada.id
        ))
    };
    let before = w.user(ada.id);

    // The account manager RPC: a new password, from a request that read the
    // account with the old key.
    let racing = changed_meanwhile(&w, key_replaced(ada.id));
    let params: Params = [
        ("account_key", ada.auth.as_str()),
        (
            "password_hash",
            passwd_hash("attacker1", "ada@example.com").as_str(),
        ),
        ("name", "Mallory"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let reply = block_on(cb_core::web::am_set_info(
        &racing,
        &w.project.config,
        PEPPER,
        NOW,
        &params,
    ))
    .unwrap();
    assert_eq!(error_num(&reply), Some(-199), "{reply}");
    let u = w.user(ada.id);
    assert_eq!(
        (u.passwd_hash.as_str(), u.name.as_str()),
        (before.passwd_hash.as_str(), "Ada")
    );
    reset(&w);

    // The password page.
    let form = w.get("/edit_passwd_form.php", auth, &[]);
    let mut sent = hidden_fields(&form.body);
    sent.extend(
        [
            ("old_passwd", "secret"),
            ("passwd", "attacker1"),
            ("passwd2", "attacker1"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string())),
    );
    let pairs: Vec<(&str, &str)> = sent.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let racing = changed_meanwhile(&w, key_replaced(ada.id));
    let (page, _) = w.page_on(&racing, "/edit_passwd_action.php", auth, true, &pairs);
    assert_eq!(page.status, 409);
    assert_eq!(w.user(ada.id).passwd_hash, before.passwd_hash);
    reset(&w);

    // The account details.
    let form = w.get("/edit_user_info_form.php", auth, &[]);
    let mut sent = hidden_fields(&form.body);
    sent.extend(
        [
            ("user_name", "Mallory"),
            ("url", ""),
            ("country", ""),
            ("postal_code", ""),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string())),
    );
    let pairs: Vec<(&str, &str)> = sent.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let racing = changed_meanwhile(&w, key_replaced(ada.id));
    let (page, _) = w.page_on(&racing, "/edit_user_info_action.php", auth, true, &pairs);
    assert_eq!(page.status, 409);
    assert_eq!(w.user(ada.id).name, "Ada");
    reset(&w);

    // The address, with email: no change, and no undo token.
    let form = w.get("/edit_email_form.php", auth, &[]);
    let mut sent = hidden_fields(&form.body);
    sent.extend(
        [("email_addr", "mallory@example.com"), ("passwd", "secret")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string())),
    );
    let pairs: Vec<(&str, &str)> = sent.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let racing = changed_meanwhile(&w, key_replaced(ada.id));
    let (page, _) = w.page_on(&racing, "/edit_email_action.php", auth, true, &pairs);
    assert_eq!(page.status, 409);
    assert_eq!(w.user(ada.id).email_addr, "ada@example.com");
    assert_eq!(w.token_rows("E"), 0);
    assert!(w.mails().is_empty());
    reset(&w);

    // A preferences save.
    let form = w.get("/prefs_edit.php", auth, &[("subset", "global")]);
    let global = cb_core::prefs::PrefsForm::new(cb_core::prefs::Subset::Global, &w.project);
    let mut sent = hidden_fields(&form.body);
    sent.extend(global.form_values(&global.defaults()));
    let pairs: Vec<(&str, &str)> = sent.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let racing = changed_meanwhile(&w, key_replaced(ada.id));
    let (page, _) = w.page_on(&racing, "/prefs_edit.php", auth, true, &pairs);
    assert_eq!(page.status, 409);
    assert_eq!(w.user(ada.id).global_prefs, before.global_prefs);
}

#[test]
fn a_recovery_request_under_way_cannot_make_a_link_for_a_former_account() {
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    // After the lookup of the address, the address changes and the key is
    // replaced; the token batch must find the account is not what was read.
    let racing = changed_around_tokens(
        &w,
        stmt!(
            "UPDATE user SET email_addr = 'ada@new.example', authenticator = ? WHERE id = ?",
            REPLACED_KEY,
            ada.id
        ),
        false,
    );
    let (page, _) = w.page_on(
        &racing,
        "/mail_passwd.php",
        None,
        true,
        &[("email_addr", "ada@example.com")],
    );
    assert_eq!(page.status, 200);
    assert_eq!(w.token_rows("L"), 0);
    assert!(w.mails().is_empty());

    // The same for a verification message asked for from the account page.
    w.sql(&format!(
        "UPDATE user SET email_addr = 'ada@example.com', authenticator = '{}' WHERE id = {}",
        ada.auth, ada.id
    ));
    let home = w.get("/home.php", Some(&ada.auth), &[]);
    let pairs = hidden_fields(&home.body);
    let pairs: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let racing = changed_around_tokens(
        &w,
        stmt!(
            "UPDATE user SET email_addr = 'ada@new.example' WHERE id = ?",
            ada.id
        ),
        false,
    );
    let (page, _) = w.page_on(
        &racing,
        "/validate_email_addr.php",
        Some(&ada.auth),
        true,
        &pairs,
    );
    assert_ne!(page.status, 200);
    assert_eq!(w.token_rows("V"), 0);
    assert!(w.mails().is_empty());
}

#[test]
fn a_login_link_used_during_a_key_reset_gets_the_old_key_or_nothing() {
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    w.request_login_link("ada@example.com");
    let login = w.last_link("ada@example.com", "login_action.php");
    // The key is replaced just after the token was used. The link's own
    // request must not read the new key.
    let racing = changed_around_tokens(&w, key_replaced(ada.id), true);
    let pairs: Vec<(&str, &str)> = login
        .1
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let (page, _) = w.page_on(&racing, &login.0, None, true, &pairs);
    let cookie = page.set_cookie.unwrap_or_default();
    assert!(!cookie.contains(REPLACED_KEY), "{cookie}");
    assert!(
        cookie.starts_with(&format!("auth={};", ada.auth)),
        "{cookie}"
    );
    assert_eq!(w.user(ada.id).authenticator, REPLACED_KEY);
}

// ---------------------------------------------------------------------------
// A refused request must not end the tokens of the change that was not refused.

/// Two unused tokens as another request's change leaves them: a login link
/// and an undo link, made a moment ago.
fn newer_tokens(userid: i64) -> Vec<Stmt> {
    ["L", "E"]
        .iter()
        .enumerate()
        .map(|(i, kind)| {
            let hash = cb_crypto::account::token_hash(&format!("newer-{kind}-{i}"));
            stmt!(
                "INSERT INTO token (userid, type, token_hash, data, create_time, expire_time) \
                 VALUES (?, ?, ?, 'ada@example.com', ?, ?)",
                userid,
                *kind,
                hash.as_str(),
                NOW - 5,
                NOW + 100_000
            )
        })
        .collect()
}

fn unused_tokens(w: &World) -> i64 {
    block_on(w.db.run(stmt!("SELECT count(*) AS n FROM token WHERE used_time = 0")))
        .unwrap()
        .scalar_i64()
        .unwrap()
}

#[test]
fn a_refused_request_leaves_the_tokens_of_the_change_that_won() {
    // am_set_info: both read address A and want B; the other one, a site
    // change, commits first with its login and undo links. The RPC's update is
    // refused, and the account now has B, as it wanted.
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let mut others = vec![stmt!(
        "UPDATE user SET email_addr = 'ada@b.example', previous_email_addr = 'ada@example.com', \
         email_addr_change_time = ? WHERE id = ?",
        NOW,
        ada.id
    )];
    others.extend(newer_tokens(ada.id));
    let racing = changed_meanwhile_by(&w, others);
    let params: Params = [
        ("account_key", ada.auth.as_str()),
        ("email_addr", "ada@b.example"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let reply = block_on(cb_core::web::am_set_info(
        &racing,
        &w.project.config,
        PEPPER,
        NOW,
        &params,
    ))
    .unwrap();
    assert_eq!(error_num(&reply), Some(-199), "{reply}");
    assert_eq!(unused_tokens(&w), 2, "am_set_info");

    // The password page: the other request set the same new password, with
    // its login link.
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let stored = cb_crypto::account::pepper_passwd_hash(
        PEPPER,
        &passwd_hash("newer-pass", "ada@example.com"),
    );
    let mut others = vec![stmt!(
        "UPDATE user SET passwd_hash = ? WHERE id = ?",
        stored.as_str(),
        ada.id
    )];
    others.extend(newer_tokens(ada.id));
    let form = w.get("/edit_passwd_form.php", Some(&ada.auth), &[]);
    let mut sent = hidden_fields(&form.body);
    sent.extend(
        [
            ("old_passwd", "secret"),
            ("passwd", "newer-pass"),
            ("passwd2", "newer-pass"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string())),
    );
    let pairs: Vec<(&str, &str)> = sent.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let racing = changed_meanwhile_by(&w, others);
    let (page, _) = w.page_on(
        &racing,
        "/edit_passwd_action.php",
        Some(&ada.auth),
        true,
        &pairs,
    );
    assert_eq!(page.status, 409);
    assert_eq!(unused_tokens(&w), 2, "edit_passwd_action");

    // The address page, with and without email.
    for with_mail in [true, false] {
        let w = if with_mail {
            World::with_mail()
        } else {
            World::new()
        };
        let ada = w.account("ada@example.com", "secret", "Ada");
        let mut others = vec![stmt!(
            "UPDATE user SET email_addr = 'ada@b.example', previous_email_addr = 'ada@example.com', \
             email_addr_change_time = ? WHERE id = ?",
            NOW,
            ada.id
        )];
        others.extend(newer_tokens(ada.id));
        let form = w.get("/edit_email_form.php", Some(&ada.auth), &[]);
        let mut sent = hidden_fields(&form.body);
        sent.extend(
            [("email_addr", "ada@b.example"), ("passwd", "secret")]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string())),
        );
        let pairs: Vec<(&str, &str)> = sent.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let racing = changed_meanwhile_by(&w, others);
        let (page, _) = w.page_on(
            &racing,
            "/edit_email_action.php",
            Some(&ada.auth),
            true,
            &pairs,
        );
        assert_eq!(page.status, 409, "mail: {with_mail}");
        assert_eq!(unused_tokens(&w), 2, "edit_email_action, mail: {with_mail}");
        assert_eq!(
            w.token_rows("E"),
            1,
            "no second undo token, mail: {with_mail}"
        );
        assert!(w.mails().is_empty());
    }
}

/// Every HTML page of the site, logged in or not, comes out of the one
/// shell: a document with the viewport meta, the inline stylesheet and no
/// external request. Redirects have no body.
#[test]
fn every_site_page_goes_through_the_shell() {
    let w = World::new();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let mut shell_pages = 0;
    for path in cb_core::site::SITE_PATHS {
        for auth in [None, Some(ada.auth.as_str())] {
            for post in [false, true] {
                let (page, _) = w.page(path, auth, post, &[]);
                if page.content_type.starts_with("text/xml") || page.body.is_empty() {
                    continue;
                }
                let body = &page.body;
                assert!(
                    body.starts_with("<!DOCTYPE html>\n<html lang=\"en\">"),
                    "{path}"
                );
                assert!(
                    body.contains(
                        "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">"
                    ),
                    "{path}"
                );
                assert!(body.contains("<style>:root{"), "{path}");
                assert!(
                    body.contains("<link rel=\"icon\" href=\"data:image/svg+xml,%3Csvg"),
                    "{path}"
                );
                assert!(body.contains("<header class=\"top\">"), "{path}");
                assert!(body.contains("<h1>"), "{path}");
                for external in ["src=\"", "url(", "@import", "<link rel=\"stylesheet"] {
                    assert!(!body.contains(external), "{path}: {external}");
                }
                shell_pages += 1;
            }
        }
    }
    assert!(shell_pages >= 40, "only {shell_pages} pages checked");
}

/// With email the account page links the recovery page and, until the
/// address is verified, the verification page.
#[test]
fn the_account_page_links_the_email_functions() {
    let w = World::with_mail();
    let ada = w.account("ada@example.com", "secret", "Ada");
    let body = w.get("/home.php", Some(&ada.auth), &[]).body;
    assert!(body.contains("href=\"get_passwd.php\""));
    assert!(body.contains("href=\"validate_email_addr.php\""));
    assert!(body.contains("(not verified)"));
    w.sql("UPDATE user SET email_validated = 1");
    let body = w.get("/home.php", Some(&ada.auth), &[]).body;
    assert!(body.contains("href=\"get_passwd.php\""));
    assert!(!body.contains("href=\"validate_email_addr.php\""));
    assert!(body.contains("(verified)"));
}

/// A project name with markup or a character reference is shown as the text
/// it is, once escaped: in the header, the title, the account page and the
/// label that carries it (`site_ui::kv` escapes labels itself).
#[test]
fn a_project_name_with_markup_is_shown_as_text() {
    let config = CONFIG.replace(
        "<long_name>Test Project</long_name>",
        "<long_name>A &lt;b&gt;&amp;amp; Co</long_name>",
    );
    let w = World::with_config(&config);
    let ada = w.account("ada@example.com", "secret", "Ada");
    let body = w.get("/home.php", Some(&ada.auth), &[]).body;
    assert!(body.contains("A &lt;b&gt;&amp;amp; Co member since"));
    assert!(body.contains("<a class=\"brand\" href=\"./\">A &lt;b&gt;&amp;amp; Co</a>"));
    assert!(!body.contains("<b>"));
    assert!(!body.contains("&amp;amp;amp;"));
    let home = cb_core::web::master_page(&w.project);
    assert!(home.contains("<title>A &lt;b&gt;&amp;amp; Co</title>"));
    assert!(home.contains("<h1>A &lt;b&gt;&amp;amp; Co</h1>"));
    assert!(!home.contains("<b>"));
}
