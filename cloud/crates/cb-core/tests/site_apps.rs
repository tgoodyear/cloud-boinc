//! `apps.php` and the 404 page of the site: what the page lists, what it
//! leaves out, escaping, the statements it runs, and who gets the styled 404
//! and who the plain text.

use cb_core::project::{put_setting, Project, SETTING_CONFIG};
use cb_core::site::{
    is_rpc_path, not_found_page, site_page, wants_html_404, Params, SiteCtx, SitePage, SITE_HEADERS,
};
use cb_db::{stmt, Db};
use cb_sqlite::SqliteDb;
use futures_lite::future::block_on;

const CONFIG: &str = "<config>\n<long_name>Test &lt;Project&gt;</long_name>\n<master_url>https://p.example/</master_url>\n<download_url>https://p.example/download</download_url>\n<upload_url>https://p.example/upload/file_upload_handler</upload_url>\n</config>\n";

fn sql(db: &SqliteDb, s: &str) {
    block_on(db.run(stmt!(s))).unwrap();
}

fn world() -> SqliteDb {
    let db = SqliteDb::with_schema().unwrap();
    block_on(put_setting(&db, SETTING_CONFIG, CONFIG, 10)).unwrap();
    db
}

fn platform(db: &SqliteDb, id: i64, name: &str, friendly: &str, deprecated: i32) {
    sql(
        db,
        &format!(
            "INSERT INTO platform (id, create_time, name, user_friendly_name, deprecated) \
             VALUES ({id}, 1, '{name}', '{friendly}', {deprecated})"
        ),
    );
}

fn app(db: &SqliteDb, id: i64, name: &str, friendly: &str, deprecated: i32, beta: i32) {
    sql(
        db,
        &format!(
            "INSERT INTO app (id, create_time, name, user_friendly_name, deprecated, beta) \
             VALUES ({id}, 1700000000, '{name}', '{friendly}', {deprecated}, {beta})"
        ),
    );
}

#[allow(clippy::too_many_arguments)]
fn version(
    db: &SqliteDb,
    id: i64,
    appid: i64,
    platformid: i64,
    num: i32,
    plan: &str,
    deprecated: i32,
    beta: i32,
) {
    sql(
        db,
        &format!(
            "INSERT INTO app_version (id, create_time, appid, version_num, platformid, xml_doc, \
             plan_class, deprecated, beta) VALUES ({id}, {}, {appid}, {num}, {platformid}, '', \
             '{plan}', {deprecated}, {beta})",
            1_700_000_000 + id * 86_400
        ),
    );
}

/// One request, and the statements it ran.
fn get(db: &SqliteDb, params: &[(&str, &str)]) -> (SitePage, u64) {
    let project = block_on(Project::load(db)).unwrap();
    let q: Params = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let random = || 1u32;
    let ctx = SiteCtx {
        db,
        project: &project,
        base: &project.config.master_url,
        auth: None,
        post: false,
        pepper: None,
        now: 1_800_000_000,
        random: &random,
        mailer: None,
    };
    let before = db.statements();
    let page = block_on(site_page(&ctx, "/apps.php", &q)).unwrap().unwrap();
    (page, db.statements() - before)
}

/// A project with several applications, platforms, plan classes and retired
/// versions.
fn busy_world() -> SqliteDb {
    let db = world();
    platform(&db, 1, "x86_64-pc-linux-gnu", "Linux", 0);
    platform(&db, 2, "windows_x86_64", "Windows", 0);
    platform(&db, 3, "i686-pc-linux-gnu", "Old Linux", 1);
    app(&db, 1, "worker", "Worker", 0, 0);
    app(&db, 2, "gpu_app", "GPU app", 0, 1);
    app(&db, 3, "retired", "Retired app", 1, 0);
    // Worker on Linux: 1.00 is replaced by 1.02; 'mt' has its own 1.01.
    version(&db, 1, 1, 1, 100, "", 0, 0);
    version(&db, 2, 1, 1, 102, "", 0, 0);
    version(&db, 3, 1, 1, 101, "mt", 0, 0);
    // The same on Windows, and a deprecated 1.03 that must not show.
    version(&db, 4, 1, 2, 102, "", 0, 0);
    version(&db, 5, 1, 2, 103, "", 1, 0);
    // A beta version next to the release of the same plan class stays.
    version(&db, 6, 1, 1, 200, "", 0, 1);
    // A version on a deprecated platform and one of a deprecated app.
    version(&db, 7, 1, 3, 102, "", 0, 0);
    version(&db, 8, 3, 1, 100, "", 0, 0);
    // The beta app has a GPU version.
    version(&db, 9, 2, 2, 1234, "cuda<1>", 0, 0);
    db
}

fn rows(body: &str) -> Vec<String> {
    body.lines()
        .filter(|l| l.starts_with("<tr><td"))
        .map(|l| {
            l.replace("</td>", "|")
                .split('<')
                .filter_map(|p| p.split_once('>').map(|(_, t)| t.to_string()))
                .collect::<Vec<_>>()
                .concat()
        })
        .collect()
}

#[test]
fn the_page_lists_current_versions_per_platform_and_plan_class() {
    let db = busy_world();
    let (page, statements) = get(&db, &[]);
    assert_eq!(page.status, 200);
    assert_eq!(page.content_type, "text/html; charset=utf-8");
    let body = &page.body;
    assert!(body.contains("<h1>Applications</h1>"));
    // Names are escaped, in the intro and the headings.
    assert!(body.contains("Test &lt;Project&gt; currently has the following applications."));
    assert!(!body.contains("Test <Project>"));
    assert!(body.contains("<h2>Worker</h2>"));
    assert!(body.contains("<h2>GPU app (beta test)</h2>"));
    // A deprecated application is not listed.
    assert!(!body.contains("Retired app"));
    let r = rows(body);
    // Worker: Linux 1.02 and mt 1.01 and beta 2.00, Windows 1.02; GPU app: 12.34.
    assert_eq!(r.len(), 5, "{r:?}");
    assert!(r[0].starts_with("Linux|1.02|---|"), "{}", r[0]);
    assert!(r[1].starts_with("Linux|1.01|mt|"), "{}", r[1]);
    assert!(r[2].starts_with("Linux|2.00 (beta test)|---|"), "{}", r[2]);
    assert!(r[3].starts_with("Windows|1.02|---|"), "{}", r[3]);
    assert!(r[4].starts_with("Windows|12.34|cuda&lt;1&gt;|"), "{}", r[4]);
    // Replaced, deprecated, and on a deprecated platform: none of these.
    assert!(!body.contains("1.00"));
    assert!(!body.contains("1.03"));
    assert!(!body.contains("Old Linux"));
    // Creation dates are written as upstream's `pretty_time_str`.
    assert!(r[0].ends_with("|16 Nov 2023, 22:13:20 UTC|"), "{}", r[0]);
    assert!(body.contains("Added 14 Nov 2023, 22:13:20 UTC"));
    // The cells carry their labels, so that the table stacks on a phone.
    assert!(body.contains("<table class=\"stack\">"));
    assert!(body.contains("data-label=\"Plan class\""));
    // No statement of its own: the catalog came with the settings.
    assert_eq!(statements, 0);
}

#[test]
fn statements_do_not_grow_with_the_applications() {
    let db = world();
    platform(&db, 1, "x86_64-pc-linux-gnu", "Linux", 0);
    for i in 1..=60 {
        app(&db, i, &format!("app{i}"), &format!("App {i}"), 0, 0);
        version(&db, i, i, 1, 100, "", 0, 0);
    }
    let (page, statements) = get(&db, &[]);
    assert_eq!(rows(&page.body).len(), 60);
    assert_eq!(statements, 0);
}

#[test]
fn an_empty_project_says_so() {
    let db = world();
    let (page, _) = get(&db, &[]);
    assert_eq!(page.status, 200);
    assert!(page.body.contains("No applications are available yet."));
    assert!(rows(&page.body).is_empty());
    let (xml, _) = get(&db, &[("xml", "1")]);
    assert_eq!(
        xml.body,
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<app_versions>\n</app_versions>\n"
    );
}

#[test]
fn an_application_without_versions_says_so() {
    let db = world();
    platform(&db, 1, "x86_64-pc-linux-gnu", "Linux", 0);
    app(&db, 1, "worker", "Worker", 0, 0);
    // Its only version is deprecated.
    version(&db, 1, 1, 1, 100, "", 1, 0);
    let (page, _) = get(&db, &[]);
    assert!(page.body.contains("<h2>Worker</h2>"));
    assert!(page.body.contains("No versions are available yet."));
}

#[test]
fn markup_in_names_is_escaped() {
    let db = world();
    platform(&db, 1, "p", "Lin<b>ux & co", 0);
    app(&db, 1, "w", "<script>alert(1)</script>", 0, 0);
    version(&db, 1, 1, 1, 100, "a\"b<i>", 0, 0);
    let (page, _) = get(&db, &[]);
    assert!(!page.body.contains("<script>"));
    assert!(!page.body.contains("<b>"));
    assert!(!page.body.contains("<i>"));
    assert!(page.body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(page.body.contains("Lin&lt;b&gt;ux &amp; co"));
    assert!(page.body.contains("a&quot;b&lt;i&gt;"));
    let (xml, _) = get(&db, &[("xml", "1")]);
    assert!(!xml.body.contains("<script>"));
    assert!(xml.body.contains("&lt;script>alert(1)&lt;/script>"));
    assert!(xml
        .body
        .contains("<platform_long>Lin&lt;b>ux &amp; co</platform_long>"));
}

#[test]
fn the_xml_form_follows_upstream() {
    let db = busy_world();
    let (page, statements) = get(&db, &[("xml", "1")]);
    assert_eq!(statements, 0);
    assert_eq!(page.status, 200);
    assert_eq!(page.content_type, "text/xml; charset=utf-8");
    let ver = |short: &str, long: &str, num: i32, plan: &str, date: &str, unix: i64, beta: bool| {
        format!(
            "    <version>\n        <platform_short>{short}</platform_short>\n        \
             <platform_long>{long}</platform_long>\n        <version_num>{num}</version_num>\n        \
             <plan_class>{plan}</plan_class>\n        <date>{date} UTC</date>\n        \
             <date_unix>{unix}</date_unix>\n{}    </version>\n",
            if beta { "        <beta/>\n" } else { "" }
        )
    };
    let linux = "x86_64-pc-linux-gnu";
    let want = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<app_versions>\n\
         <application>\n    <user_friendly_name>Worker</user_friendly_name>\n    \
         <name>worker</name>\n    <id>1</id>\n{}{}{}{}</application>\n\
         <application>\n    <user_friendly_name>GPU app</user_friendly_name>\n    \
         <name>gpu_app</name>\n    <id>2</id>\n        <beta/>\n{}</application>\n\
         </app_versions>\n",
        ver(
            linux,
            "Linux",
            102,
            "",
            "16 Nov 2023, 22:13:20",
            1_700_172_800,
            false
        ),
        ver(
            linux,
            "Linux",
            101,
            "mt",
            "17 Nov 2023, 22:13:20",
            1_700_259_200,
            false
        ),
        ver(
            linux,
            "Linux",
            200,
            "",
            "20 Nov 2023, 22:13:20",
            1_700_518_400,
            true
        ),
        ver(
            "windows_x86_64",
            "Windows",
            102,
            "",
            "18 Nov 2023, 22:13:20",
            1_700_345_600,
            false
        ),
        ver(
            "windows_x86_64",
            "Windows",
            1234,
            "cuda&lt;1>",
            "23 Nov 2023, 22:13:20",
            1_700_777_600,
            false
        ),
    );
    assert_eq!(page.body, want);
    // `xml=0` and an empty `xml` are false, as in PHP.
    for v in ["0", ""] {
        let (page, _) = get(&db, &[("xml", v)]);
        assert_eq!(page.content_type, "text/html; charset=utf-8", "{v:?}");
    }
}

#[test]
fn the_xml_parameter_is_true_as_in_php() {
    let db = busy_world();
    // PHP: only "" and "0" are false; "0.0", " 0", "false" and "no" are true.
    for (v, xml) in [
        ("1", true),
        ("true", true),
        ("false", true),
        ("0.0", true),
        (" 0", true),
        ("00", true),
        ("0", false),
        ("", false),
    ] {
        let (page, _) = get(&db, &[("xml", v)]);
        assert_eq!(page.content_type.starts_with("text/xml"), xml, "{v:?}");
    }
}

#[test]
fn the_home_page_links_to_the_applications_page() {
    let db = busy_world();
    let project = block_on(Project::load(&db)).unwrap();
    let home = cb_core::web::master_page(&project);
    assert!(home.contains("<a href=\"apps.php\">Details of the current versions</a>"));
}

#[test]
fn the_applications_page_has_no_login_or_form() {
    let db = busy_world();
    let (page, _) = get(&db, &[]);
    assert!(page.set_cookie.is_none() && page.location.is_none());
    assert!(!page.body.contains("<form"));
}

// ---------------------------------------------------------------------------
// The 404 page.

const BROWSER: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";

#[test]
fn a_browser_get_of_an_unknown_path_gets_the_page() {
    assert!(wants_html_404(true, "/nope.php", Some(BROWSER)));
    assert!(wants_html_404(true, "/a/b/c", Some("text/html")));
    assert!(wants_html_404(true, "/x", Some("TEXT/HTML; charset=utf-8")));
    assert!(wants_html_404(true, "/short/nope.php", Some(BROWSER)));
}

#[test]
fn the_accept_header_is_read_as_the_rfc_has_it() {
    let html = |accept: &str| wants_html_404(true, "/nope.php", Some(accept));
    // Media type without regard to case, and several entries.
    for accept in [
        "text/html",
        "Text/HTML",
        "TEXT/HTML;q=0.5",
        "application/json, text/html",
        "application/json,text/html;q=0.1",
        " text/html ",
        "*/*;q=0.1, text/html",
    ] {
        assert!(html(accept), "{accept}");
    }
    // Optional white space around `;` and `=`; the parameter name in any case.
    for accept in [
        "text/html ; q=0.8",
        "text/html;q = 0.8",
        "text/html ;  Q = 1",
        "text/html;Q=1.000",
        "text/html;q=0.001",
        "text/html;level=1;q=0.5",
        "text/html;charset=utf-8",
    ] {
        assert!(html(accept), "{accept}");
    }
    // A weight of 0 refuses the type, however it is written.
    for accept in [
        "text/html;q=0",
        "text/html;Q=0",
        "text/html;q=0.",
        "text/html;q=0.0",
        "text/html;q=0.00",
        "text/html;q=0.000",
        "text/html;Q=0.000",
        "text/html ; q = 0",
        "text/html;level=1;q=0",
        "text/html;q=0, */*",
        "*/*, text/html;q=0",
    ] {
        assert!(!html(accept), "{accept}");
    }
    // A malformed weight refuses the entry; another entry may still ask.
    for accept in [
        "text/html;q=",
        "text/html;q=abc",
        "text/html;q=nan",
        "text/html;q=-1",
        "text/html;q=+1",
        "text/html;q=1.5",
        "text/html;q=2",
        "text/html;q=0.0001",
        "text/html;q=1e0",
        "text/html;q",
    ] {
        assert!(!html(accept), "{accept}");
    }
    assert!(html("text/html;q=abc, text/html"));
    // Only an explicit text/html asks for the page.
    for accept in [
        "text/*",
        "*/*",
        "text/plain",
        "application/xhtml+xml",
        "",
        " ",
        "text/htm",
        "xtext/html",
    ] {
        assert!(!html(accept), "{accept}");
    }
}

#[test]
fn rpc_paths_are_found_whatever_the_case_or_trailing_slash() {
    for path in [
        "/AM_get_info.php",
        "/Lookup_Account.php",
        "/am_get_info.php/",
        "/Get_Project_Config.php",
        "/SERVER_STATUS.PHP",
        "/Download/x",
        "/Short_CGI/cgi",
    ] {
        assert!(is_rpc_path(path), "{path}");
        assert!(!wants_html_404(true, path, Some(BROWSER)), "{path}");
    }
    for path in [
        "",
        "/",
        "//",
        "/not_am.php",
        "/amazing.php",
        "/accounts.php",
    ] {
        assert!(!is_rpc_path(path), "{path}");
    }
}

#[test]
fn tools_and_other_methods_keep_the_plain_form() {
    // No Accept, a tool's `*/*`, and a browser that refuses HTML.
    assert!(!wants_html_404(true, "/nope.php", None));
    assert!(!wants_html_404(true, "/nope.php", Some("*/*")));
    assert!(!wants_html_404(true, "/nope.php", Some("application/xml")));
    assert!(!wants_html_404(
        true,
        "/nope.php",
        Some("text/html;q=0, */*")
    ));
    // Anything but GET, a browser's Accept included.
    assert!(!wants_html_404(false, "/nope.php", Some(BROWSER)));
    // The RPC paths, with the Accept of a browser.
    for path in [
        "/lookup_account.php",
        "/create_account.php",
        "/other_account.php",
        "/am_get_info.php",
        "/am_whatever.php",
        "/get_project_config.php",
        "/server_status.php",
        "/short/am_set_info.php",
        "/short_cgi/cgi",
        "/short_cgi/other",
        "/sched",
        "/cgi-bin/x",
        "/upload/file_upload_handler",
        "/download/ab/file",
    ] {
        assert!(is_rpc_path(path), "{path}");
        assert!(!wants_html_404(true, path, Some(BROWSER)), "{path}");
    }
    for path in [
        "/nope.php",
        "/",
        "/apps.php",
        "/results.php",
        "/account.php",
    ] {
        assert!(!is_rpc_path(path), "{path}");
    }
}

#[test]
fn the_404_page_is_in_the_site_shell_and_names_no_path() {
    let db = busy_world();
    let project = block_on(Project::load(&db)).unwrap();
    let before = db.statements();
    let page = not_found_page(&project, &project.config.master_url);
    // No statement beyond what the Worker loaded with the settings.
    assert_eq!(db.statements(), before);
    assert_eq!(page.status, 404);
    assert_eq!(page.content_type, "text/html; charset=utf-8");
    let b = &page.body;
    assert!(b.starts_with("<!DOCTYPE html>\n<html lang=\"en\">"));
    assert!(b.contains("<meta name=\"viewport\""));
    assert!(b.contains("<style>:root{"));
    assert!(b.contains("<title>Page not found</title>"));
    assert!(b.contains("<header class=\"top\">"));
    assert!(b.contains("<h1>Page not found</h1>"));
    assert!(b.contains("Test &lt;Project&gt;"));
    // The header links are relative, so the page sets the base to the root.
    assert!(b.contains("<base href=\"/\">"));
    assert!(b.contains("<a class=\"button\" href=\"./\">Go to the home page</a>"));
    assert!(b.contains("<a href=\"work.php\">Jobs</a>"));
    for external in [
        "src=\"",
        "url(",
        "@import",
        "<link rel=\"stylesheet",
        "<script",
    ] {
        assert!(!b.contains(external), "{external}");
    }
    assert!(page.location.is_none() && page.set_cookie.is_none());
}

#[test]
fn the_404_page_follows_a_stock_page_prefix() {
    let db = world();
    let project = block_on(Project::load(&db)).unwrap();
    let page = not_found_page(&project, "https://p.example/proj/");
    assert!(page.body.contains("<base href=\"/proj/\">"));
    let page = not_found_page(&project, "https://p.example/");
    assert!(page.body.contains("<base href=\"/\">"));
}

#[test]
fn the_security_headers_of_the_site_pages_exist() {
    // The Worker sends the 404 page through `site_response`, which sets these.
    assert!(SITE_HEADERS.contains(&("X-Content-Type-Options", "nosniff")));
    assert!(cb_core::web::html_cache_control("private, no-store").contains("no-transform"));
}
