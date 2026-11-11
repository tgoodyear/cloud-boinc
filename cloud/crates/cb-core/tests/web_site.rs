//! The account manager RPCs and the site pages against SQLite: who may see
//! what, escaping of values that came from clients, paging, and how many
//! statements each page runs.

use std::cell::{Cell, RefCell};

use async_trait::async_trait;
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

struct World {
    db: SqliteDb,
    seq: Cell<u32>,
    project: Project,
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
        }
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
            now: NOW,
            random: &random,
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
    assert_eq!(n, 2);
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
    assert_eq!((ok.status, n), (200, 2));
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
        (200, 3),
        "the account, then one batch of two"
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

/// A database that, before the first write to `user`, runs one statement of
/// another request: that request and this one start from the same read.
struct ChangedMeanwhile<'a> {
    inner: &'a SqliteDb,
    other: RefCell<Option<Stmt>>,
}

#[async_trait(?Send)]
impl Db for ChangedMeanwhile<'_> {
    async fn batch(&self, stmts: Vec<Stmt>) -> DbResult<Vec<Rows>> {
        if stmts.iter().any(|s| s.sql.starts_with("UPDATE user SET")) {
            let other = self.other.borrow_mut().take();
            if let Some(other) = other {
                self.inner.run(other).await?;
            }
        }
        self.inner.batch(stmts).await
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
        other: RefCell::new(Some(stmt!(
            "UPDATE user SET email_addr = 'ada@site.example', passwd_hash = ? WHERE id = ?",
            site_hash.as_str(),
            ada.id
        ))),
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
        other: RefCell::new(Some(stmt!(
            "UPDATE user SET passwd_hash = 'changed' WHERE id = ?",
            ada.id
        ))),
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
        &params,
    ))
    .unwrap();
    assert_eq!(error_num(&reply), Some(-199), "{reply}");
    assert_eq!(w.user(ada.id).email_addr, "ada@site.example");

    // A change that does not touch the credentials is not held up by one.
    let racing = ChangedMeanwhile {
        inner: &w.db,
        other: RefCell::new(Some(stmt!(
            "UPDATE user SET passwd_hash = 'again' WHERE id = ?",
            ada.id
        ))),
    };
    let params = q(&[("account_key", &ada.auth), ("name", "Ada L")]);
    let reply = block_on(cb_core::web::am_set_info(
        &racing,
        &w.project.config,
        PEPPER,
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
        other: RefCell::new(Some(sql)),
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
