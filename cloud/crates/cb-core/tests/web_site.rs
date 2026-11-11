//! The account manager RPCs and the site pages against SQLite: who may see
//! what, escaping of values that came from clients, paging, and how many
//! statements each page runs.

use std::cell::Cell;

use cb_core::project::{put_setting, Project, SETTING_CONFIG};
use cb_core::site::{
    site_page, Params, SiteCtx, SitePage, RESULTS_FILTER_WINDOW, RESULTS_PER_PAGE,
};
use cb_core::web::{
    am_get_info, am_set_host_info, am_set_info, make_user, weak_account_key, NewAccount,
};
use cb_crypto::account::passwd_hash;
use cb_db::{stmt, Db};
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
        let db = SqliteDb::with_schema().unwrap();
        block_on(put_setting(&db, SETTING_CONFIG, CONFIG, 10)).unwrap();
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
        let q: Params = params
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let ctx = SiteCtx {
            db: &self.db,
            project: &self.project,
            auth,
            post,
            pepper: Some(PEPPER),
            now: NOW,
        };
        let before = self.db.statements();
        let page = block_on(site_page(&ctx, path, &q)).unwrap().unwrap();
        (page, self.db.statements() - before)
    }

    fn get(&self, path: &str, auth: Option<&str>, params: &[(&str, &str)]) -> SitePage {
        self.page(path, auth, false, params).0
    }
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

#[test]
fn am_set_info_is_switched_off_as_upstream() {
    let reply = am_set_info();
    assert_eq!(error_num(&reply), Some(-1));
    assert!(reply.contains("<error_msg>not supported</error_msg>"));
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

    let out = w.get("/logout.php", Some(&ada.auth), &[]);
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
