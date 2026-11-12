//! Teams against SQLite: the pages, the RPCs, who may do what, what is
//! stored and shown for text a person typed, the member counts, and the
//! statements each page runs.

use std::cell::{Cell, RefCell};

use async_trait::async_trait;
use cb_core::project::{put_setting, Project, SETTING_CONFIG};
use cb_core::site::{site_page, Params, SiteCtx, SitePage, SITE_PATHS};
use cb_core::site_teams::TEAM_PATHS;
use cb_core::web::{am_get_info, make_user, NewAccount};
use cb_crypto::account::passwd_hash;
use cb_db::{stmt, Db, DbResult, Rows, Stmt};
use cb_sqlite::SqliteDb;
use futures_lite::future::block_on;

const PEPPER: &[u8] = b"pepper for tests";
const NOW: i64 = 2_000_000_000;

fn config(extra: &str) -> String {
    format!(
        "<config>\n<long_name>Test Project</long_name>\n<master_url>https://p.example/</master_url>\n\
         <user_country>1</user_country>\n{extra}</config>\n"
    )
}

struct Account {
    id: i64,
    auth: String,
}

struct World {
    db: SqliteDb,
    project: Project,
    seq: Cell<u32>,
    now: Cell<i64>,
    last: RefCell<Option<u64>>,
}

impl World {
    fn new() -> World {
        World::with_config("")
    }

    fn with_config(extra: &str) -> World {
        let db = SqliteDb::with_schema().unwrap();
        block_on(put_setting(&db, SETTING_CONFIG, &config(extra), 10)).unwrap();
        let project = block_on(Project::load(&db)).unwrap();
        World {
            db,
            project,
            seq: Cell::new(0),
            now: Cell::new(NOW),
            last: RefCell::new(None),
        }
    }

    fn rand(&self) -> u32 {
        self.seq.set(self.seq.get().wrapping_add(2_654_435_761));
        self.seq.get()
    }

    fn account(&self, name: &str) -> Account {
        let local: String = name
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '.' })
            .collect();
        let email = format!("{local}@example.org");
        let random = || self.rand();
        let hash = passwd_hash("secret", &email);
        let auth = block_on(make_user(
            &self.db,
            PEPPER,
            100,
            &random,
            &NewAccount {
                email_addr: &email,
                passwd_hash: &hash,
                user_name: "someone",
                ..Default::default()
            },
        ))
        .unwrap()
        .unwrap();
        let id = block_on(self.db.run(stmt!(
            "UPDATE user SET name = ?, country = 'Canada' WHERE authenticator = ? RETURNING id",
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

    fn n(&self, sql: &str) -> i64 {
        block_on(self.db.run(stmt!(sql)))
            .unwrap()
            .scalar_i64()
            .unwrap()
    }

    fn f(&self, sql: &str) -> f64 {
        block_on(self.db.run(stmt!(sql))).unwrap().rows[0]
            .values()
            .next()
            .and_then(|v| v.as_f64())
            .unwrap()
    }

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
            base: "https://p.example/",
            auth,
            post,
            pepper: Some(PEPPER),
            now: self.now.get(),
            random: &random,
            mailer: None,
        };
        let before = self.db.statements();
        let page = block_on(site_page(&ctx, path, &q)).unwrap().unwrap();
        let n = self.db.statements() - before;
        *self.last.borrow_mut() = Some(n);
        (page, n)
    }

    fn get(&self, path: &str, auth: Option<&Account>, params: &[(&str, &str)]) -> SitePage {
        self.page(path, auth.map(|a| a.auth.as_str()), false, params)
            .0
    }

    /// A form as a browser sends it: the hidden fields of `form`, then `changes`.
    fn submit(
        &self,
        form: &SitePage,
        action: &str,
        who: &Account,
        changes: &[(&str, &str)],
    ) -> (SitePage, u64) {
        let mut q = hidden_fields(&form.body);
        for (k, v) in changes {
            q.retain(|(name, _)| name != k);
            q.push((k.to_string(), v.to_string()));
        }
        let pairs: Vec<(&str, &str)> = q.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        self.page(action, Some(&who.auth), true, &pairs)
    }

    /// Creates a team through the pages and returns its ID.
    fn create(&self, who: &Account, name: &str, extra: &[(&str, &str)]) -> i64 {
        let form = self.get("/team_create_form.php", Some(who), &[]);
        assert_eq!(form.status, 200, "{}", form.body);
        let mut changes = vec![("name", name)];
        changes.extend_from_slice(extra);
        let (done, _) = self.submit(&form, "/team_create_action.php", who, &changes);
        assert_eq!(done.status, 303, "{}", done.body);
        self.n(&format!(
            "SELECT id FROM team WHERE name_lc = '{}'",
            name.to_lowercase().replace('\'', "''")
        ))
    }

    fn join(&self, who: &Account, team: i64) -> SitePage {
        let page = self.get(
            "/team_display.php",
            Some(who),
            &[("teamid", &team.to_string())],
        );
        assert!(
            page.body.contains("action=\"team_join.php\""),
            "{}",
            page.body
        );
        self.submit(&page, "/team_join.php", who, &[]).0
    }

    fn team_of(&self, who: &Account) -> i64 {
        self.n(&format!("SELECT teamid FROM user WHERE id = {}", who.id))
    }
}

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

fn tag(xml: &str, name: &str) -> Vec<String> {
    xml.split(&format!("<{name}>"))
        .skip(1)
        .map(|p| p[..p.find(&format!("</{name}>")).unwrap()].to_string())
        .collect()
}

// ---------------------------------------------------------------------------

#[test]
fn every_team_path_is_a_site_path() {
    for p in TEAM_PATHS {
        assert!(SITE_PATHS.contains(&p), "{p}");
    }
    assert_eq!(SITE_PATHS.len(), 34 + TEAM_PATHS.len());
}

#[test]
fn creating_a_team_makes_the_founder_a_member() {
    let w = World::new();
    let ada = w.account("Ada");
    let id = w.create(
        &ada,
        "Team \"Alpha\" & Co",
        &[
            ("url", "example.org/team"),
            (
                "description",
                "We <b>compute</b>.\r\nTogether.\r\n\r\nSecond paragraph.",
            ),
            ("type", "6"),
            ("country", "Canada"),
            ("joinable", "on"),
        ],
    );
    assert_eq!(w.team_of(&ada), id);
    assert_eq!(w.n(&format!("SELECT nusers FROM team WHERE id = {id}")), 1);
    assert_eq!(
        w.n(&format!("SELECT userid FROM team WHERE id = {id}")),
        ada.id
    );
    // Stored as typed (no HTML is taken or removed), lower-cased for lookups.
    let row = block_on(cb_db::query_one::<cb_core::model::Team>(
        &w.db,
        stmt!("SELECT * FROM team WHERE id = ?", id),
    ))
    .unwrap()
    .unwrap();
    assert_eq!(row.name, "Team \"Alpha\" & Co");
    assert_eq!(
        row.description,
        "We <b>compute</b>.\nTogether.\n\nSecond paragraph."
    );
    assert_eq!(
        (row.team_type, row.joinable, row.country.as_str()),
        (6, 1, "Canada")
    );
    assert_eq!(row.url, "example.org/team");
    assert_eq!(row.expavg_time, NOW as f64);

    // Everything shown is escaped; the address is a link to https, not
    // followed by crawlers.
    let page = w.get("/team_display.php", None, &[("teamid", &id.to_string())]);
    assert_eq!(page.status, 200);
    assert!(page.body.contains("Team &quot;Alpha&quot; &amp; Co"));
    assert!(!page.body.contains("& Co") && !page.body.contains("<b>compute"));
    assert!(page
        .body
        .contains("We &lt;b&gt;compute&lt;/b&gt;.<br>\nTogether.</p>"));
    assert!(page.body.contains("<p>Second paragraph.</p>"));
    assert!(page.body.contains(
        "<a href=\"https://example.org/team\" rel=\"nofollow ugc noopener noreferrer\">example.org/team</a>"
    ));
    assert!(page.body.contains("University or department"));
    assert!(page.body.contains("show_user.php?userid=1"));
    // A visitor is asked to log in; no form for a stranger.
    assert!(page
        .body
        .contains("login_form.php?next_url=team_display.php%3Fteamid%3D1"));
    assert!(!page.body.contains("team_join.php"));
}

#[test]
fn creating_needs_a_login_a_token_a_post_and_a_new_name() {
    let w = World::new();
    let ada = w.account("Ada");
    let bob = w.account("Bob");
    assert_eq!(w.get("/team_create_form.php", None, &[]).status, 303);
    let form = w.get("/team_create_form.php", Some(&ada), &[]);
    assert!(form.body.contains("<textarea"));
    assert!(form.body.contains("name=\"ttok\""));

    // No token, a token of another account, a GET: nothing is made.
    let fields = [("name", "T")];
    assert_eq!(
        w.page("/team_create_action.php", Some(&ada.auth), true, &fields)
            .0
            .status,
        403
    );
    let (page, _) = w.submit(&form, "/team_create_action.php", &bob, &[("name", "T")]);
    assert_eq!(page.status, 403);
    let mut q = hidden_fields(&form.body);
    q.push(("name".into(), "T".into()));
    let pairs: Vec<(&str, &str)> = q.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    assert_eq!(
        w.page("/team_create_action.php", Some(&ada.auth), false, &pairs)
            .0
            .status,
        405
    );
    assert_eq!(
        w.page("/team_create_action.php", None, true, &pairs)
            .0
            .status,
        303
    );
    // A form older than a day is refused.
    w.now.set(NOW + 86_401);
    assert_eq!(
        w.page("/team_create_action.php", Some(&ada.auth), true, &pairs)
            .0
            .status,
        403
    );
    w.now.set(NOW);
    assert_eq!(w.n("SELECT count(*) FROM team"), 0);

    // Names are compared without regard to case.
    w.create(&ada, "Alpha", &[]);
    let form = w.get("/team_create_form.php", Some(&bob), &[]);
    let (page, _) = w.submit(&form, "/team_create_action.php", &bob, &[("name", "ALPHA")]);
    assert_eq!(page.status, 409, "{}", page.body);
    assert!(page.body.contains("already exists"));
    assert_eq!(w.team_of(&bob), 0);
    assert_eq!(w.n("SELECT count(*) FROM team"), 1);

    // An account in a team must quit it first, on the form and in the action.
    let page = w.get("/team_create_form.php", Some(&ada), &[]);
    assert!(page.body.contains("You belong to a team"));
    let (page, _) = w.submit(
        &form,
        "/team_create_action.php",
        &ada,
        &[("name", "Second")],
    );
    assert_eq!(page.status, 403);
}

#[test]
fn what_may_not_be_stored_is_refused() {
    let w = World::new();
    let ada = w.account("Ada");
    let form = w.get("/team_create_form.php", Some(&ada), &[]);
    let bad: &[&[(&str, &str)]] = &[
        &[("name", "")],
        &[("name", "   ")],
        &[("name", "a<b>")],
        &[("name", "a\nb")],
        &[("name", "ok"), ("url", "javascript:alert(1)")],
        &[("name", "ok"), ("url", "data:text/html,x")],
        &[("name", "ok"), ("url", "//evil.example")],
        &[("name", "ok"), ("url", "http://a.example/\"onclick=\"x")],
        &[("name", "ok"), ("type", "99")],
        &[("name", "ok"), ("country", "Atlantis")],
        &[("name", "ok"), ("description", "a\u{0}b")],
    ];
    for fields in bad {
        let (page, _) = w.submit(&form, "/team_create_action.php", &ada, fields);
        assert_eq!(page.status, 400, "{fields:?}: {}", page.body);
    }
    let long = "x".repeat(192);
    assert_eq!(
        w.submit(&form, "/team_create_action.php", &ada, &[("name", &long)])
            .0
            .status,
        400
    );
    let long = "x".repeat(4001);
    assert_eq!(
        w.submit(
            &form,
            "/team_create_action.php",
            &ada,
            &[("name", "ok"), ("description", &long)]
        )
        .0
        .status,
        400
    );
    assert_eq!(w.n("SELECT count(*) FROM team"), 0);
    // An empty country is "International".
    let id = w.create(&ada, "Fine", &[("country", "")]);
    assert_eq!(
        w.n(&format!(
            "SELECT count(*) FROM team WHERE id = {id} AND country = 'International'"
        )),
        1
    );
}

#[test]
fn joining_and_quitting_keep_the_member_counts() {
    let w = World::new();
    let ada = w.account("Ada");
    let bob = w.account("Bob");
    let cy = w.account("Cy");
    let t1 = w.create(&ada, "One", &[("joinable", "on")]);
    let t2 = w.create(&bob, "Two", &[("joinable", "on")]);
    let t3 = w.create(&cy, "Closed", &[]);
    let count = |t: i64| w.n(&format!("SELECT nusers FROM team WHERE id = {t}"));
    assert_eq!((count(t1), count(t2), count(t3)), (1, 1, 1));

    // Joining a team from the page, through the form token, replaces the
    // account's team and moves the counts. Joining takes the account, the team,
    // and the update.
    let form = w.get(
        "/team_display.php",
        Some(&bob),
        &[("teamid", &t1.to_string())],
    );
    assert!(form.body.contains("action=\"team_join.php\""));
    let (done, n) = w.submit(&form, "/team_join.php", &bob, &[]);
    assert_eq!(done.status, 303);
    assert_eq!(done.location.as_deref(), Some("https://p.example/home.php"));
    assert_eq!(n, 3);
    assert_eq!(w.team_of(&bob), t1);
    assert_eq!((count(t1), count(t2)), (2, 0));

    // Joining it again changes nothing.
    let (done, _) = w.submit(&form, "/team_join_action.php", &bob, &[]);
    assert!(done.body.contains("You are already a member"));
    assert_eq!(count(t1), 2);

    // A closed team cannot be joined, from the page or by a made-up POST.
    let page = w.get(
        "/team_display.php",
        Some(&ada),
        &[("teamid", &t3.to_string())],
    );
    assert!(page.body.contains("Not accepting new members"));
    assert!(!page.body.contains("team_join.php"));
    let ada_form = w.get("/team_quit_form.php", Some(&ada), &[]);
    let (done, _) = w.submit(
        &ada_form,
        "/team_join_action.php",
        &ada,
        &[("teamid", &t3.to_string())],
    );
    assert_eq!(done.status, 403);
    assert!(done.body.contains("is not joinable"));
    assert_eq!(w.team_of(&ada), t1);

    // Tokens and methods.
    assert_eq!(
        w.page(
            "/team_join_action.php",
            Some(&bob.auth),
            true,
            &[("teamid", &t2.to_string())]
        )
        .0
        .status,
        403
    );
    assert_eq!(
        w.page(
            "/team_join_action.php",
            Some(&bob.auth),
            false,
            &[("teamid", "1")]
        )
        .0
        .status,
        405
    );
    // The stock link (a GET of `team_join.php`) goes to the form, which joins nothing.
    let page = w
        .page(
            "/team_join.php",
            Some(&bob.auth),
            false,
            &[("teamid", &t2.to_string())],
        )
        .0;
    assert_eq!(
        page.location.as_deref(),
        Some(&*format!("https://p.example/team_join_form.php?id={t2}"))
    );
    let page = w.get(
        "/team_join_form.php",
        Some(&bob),
        &[("id", &t2.to_string())],
    );
    assert!(page
        .body
        .contains("Joining a team does not change your account's credit"));
    assert!(page.body.contains("action=\"team_join_action.php\""));
    assert_eq!(w.team_of(&bob), t1);

    // Quitting: the form says what happens, the action needs the token and the
    // team it names.
    let form = w.get("/team_quit_form.php", Some(&bob), &[]);
    assert!(form.body.contains("does not affect your personal credit"));
    let stale = w
        .page(
            "/team_quit_action.php",
            Some(&bob.auth),
            true,
            &[("id", &t2.to_string())],
        )
        .0;
    assert_eq!(stale.status, 403);
    let (wrong, _) = w.submit(
        &form,
        "/team_quit_action.php",
        &bob,
        &[("id", &t2.to_string())],
    );
    assert_eq!(wrong.status, 409);
    assert_eq!(w.team_of(&bob), t1);
    let (done, _) = w.submit(&form, "/team_quit_action.php", &bob, &[]);
    assert_eq!(done.status, 303);
    assert_eq!(w.team_of(&bob), 0);
    assert_eq!((count(t1), count(t2)), (1, 0));
    // Not in a team: nothing to quit.
    assert_eq!(w.get("/team_quit_form.php", Some(&bob), &[]).status, 404);
    // A team this server does not have (an account moved from elsewhere) can be quit.
    w.sql(&format!(
        "UPDATE user SET teamid = 4000 WHERE id = {}",
        bob.id
    ));
    let form = w.get("/team_quit_form.php", Some(&bob), &[]);
    assert_eq!(form.status, 200);
    assert_eq!(
        w.submit(&form, "/team_quit_action.php", &bob, &[]).0.status,
        303
    );
    assert_eq!(w.team_of(&bob), 0);
}

#[test]
fn the_member_count_follows_every_way_teamid_changes() {
    let w = World::new();
    let ada = w.account("Ada");
    let bob = w.account("Bob");
    let t = w.create(&ada, "One", &[("joinable", "on")]);
    w.join(&bob, t);
    let count = || w.n(&format!("SELECT nusers FROM team WHERE id = {t}"));
    assert_eq!(count(), 2);
    // Writing the same value is not a change; deleting a member is one.
    w.sql(&format!(
        "UPDATE user SET teamid = {t} WHERE id = {}",
        bob.id
    ));
    assert_eq!(count(), 2);
    w.sql(&format!("DELETE FROM user WHERE id = {}", bob.id));
    assert_eq!(count(), 1);
    w.sql(&format!("UPDATE user SET teamid = 0 WHERE id = {}", ada.id));
    assert_eq!(count(), 0);
    w.sql(&format!("UPDATE user SET teamid = 0 WHERE id = {}", ada.id));
    assert_eq!(count(), 0);
}

#[test]
fn the_members_page_lists_by_credit_and_pages() {
    let w = World::new();
    let ada = w.account("Ada");
    let t = w.create(&ada, "One", &[("joinable", "on")]);
    for i in 0..24 {
        let m = w.account(&format!("Member {i:02}"));
        w.join(&m, t);
        w.sql(&format!(
            "UPDATE user SET total_credit = {}, expavg_credit = {}, expavg_time = {NOW}, recent_key = {} WHERE id = {}",
            100 + i,
            24 - i,
            cb_core::avg::recent_key((24 - i) as f64, NOW as f64),
            m.id
        ));
    }
    let hidden = w.account("Blocked <i>");
    w.join(&hidden, t);
    w.sql(&format!(
        "UPDATE user SET blocked = 1, total_credit = 9999 WHERE id = {}",
        hidden.id
    ));
    let id = t.to_string();
    // By total credit, 20 to a page.
    let (page, n) = w.page(
        "/team_members.php",
        None,
        false,
        &[("teamid", &id), ("sort_by", "total_credit")],
    );
    assert_eq!(n, 2);
    let body = &page.body;
    assert!(
        body.contains("Member 23") && body.contains("Member 04") && !body.contains("Member 03")
    );
    assert!(!body.contains("Blocked"));
    assert!(
        !body.contains("[Founder]"),
        "the founder has no credit and is on page 2"
    );
    assert!(body.find("Member 23").unwrap() < body.find("Member 22").unwrap());
    assert!(body.contains("Next 20"));
    let page2 = w.get(
        "/team_members.php",
        None,
        &[
            ("teamid", &id),
            ("sort_by", "total_credit"),
            ("offset", "20"),
        ],
    );
    assert!(page2.body.contains("Member 03") && page2.body.contains("[Founder]"));
    assert!(page2.body.contains("Previous 20") && !page2.body.contains("Next 20"));
    // By recent average the order is the other way round.
    let page = w.get("/team_members.php", None, &[("teamid", &id)]);
    assert!(page.body.find("Member 00").unwrap() < page.body.find("Member 01").unwrap());
    // The depth is capped as upstream's.
    let deep = w.get(
        "/team_members.php",
        None,
        &[("teamid", &id), ("offset", "1001")],
    );
    assert_eq!(deep.status, 400);
    assert_eq!(
        w.get("/team_members.php", None, &[("teamid", "999")])
            .status,
        404
    );
    assert_eq!(w.get("/team_members.php", None, &[]).status, 400);
}

#[test]
fn top_teams_orders_filters_pages_and_uses_its_indexes() {
    let w = World::new();
    let ada = w.account("Ada");
    for (i, (name, total, recent, ty)) in [
        ("Alpha <b>", 500.0, 5.0, 3),
        ("Beta", 900.0, 1.0, 6),
        ("Gamma", 100.0, 9.0, 3),
        ("Idle", 0.0, 0.0, 6),
    ]
    .iter()
    .enumerate()
    {
        w.sql(&format!(
            "INSERT INTO team (id, create_time, userid, name, name_lc, type, country, nusers, \
             total_credit, expavg_credit, expavg_time, recent_key) VALUES ({}, 1, {}, '{name}', '{}', {ty}, 'Canada', 3, \
             {total}, {recent}, {NOW}, {})",
            i + 1,
            ada.id,
            name.to_lowercase(),
            cb_core::avg::recent_key(*recent, NOW as f64)
        ));
    }
    let page = w.get("/top_teams.php", None, &[]);
    assert_eq!(page.status, 200);
    let b = &page.body;
    // Recent average first by default: Gamma, Alpha, Beta, Idle.
    let at = |s: &str| b.find(s).unwrap();
    assert!(
        at("Gamma") < at("Alpha &lt;b&gt;")
            && at("Alpha &lt;b&gt;") < at("Beta")
            && at("Beta") < at("Idle")
    );
    assert!(!b.contains("<b>"));
    assert!(b.contains("<strong aria-current=\"true\">Recent average credit</strong>"));
    let page = w.get("/top_teams.php", None, &[("sort_by", "total_credit")]);
    let b = &page.body;
    assert!(
        b.find("Beta").unwrap() < b.find("Alpha").unwrap()
            && b.find("Alpha").unwrap() < b.find("Gamma").unwrap()
    );
    let page = w.get("/top_teams.php", None, &[("type", "6")]);
    assert!(
        page.body.contains("Beta") && page.body.contains("Idle") && !page.body.contains("Gamma")
    );
    assert!(page.body.contains("Top university or department teams"));
    // A bad offset is the first page; the stock links keep their parameters.
    let page = w.get("/top_teams.php", None, &[("offset", "7"), ("type", "99")]);
    assert!(page.body.contains("Gamma"));
    // The cache key is the same whatever else the query says.
    let key = |pairs: &[(&str, &str)]| {
        let q: Params = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        cb_core::site_boards::public_cache_key("/top_teams.php", &q).unwrap()
    };
    assert_eq!(
        key(&[("x", "1")]),
        key(&[("offset", "7"), ("sort_by", "bogus")])
    );
    assert_ne!(key(&[("type", "3")]), key(&[]));
    assert!(cb_core::site_boards::PUBLIC_PATHS.contains(&"/top_teams.php"));

    // Paging: 20 to a page.
    for i in 5..=30 {
        w.sql(&format!(
            "INSERT INTO team (id, create_time, userid, name, name_lc, expavg_time) VALUES ({i}, 1, 1, 'T{i:02}', 't{i:02}', 1)"
        ));
    }
    let page = w.get("/top_teams.php", None, &[("sort_by", "total_credit")]);
    assert!(page.body.contains("Next 20") && !page.body.contains("Previous 20"));
    let page = w.get(
        "/top_teams.php",
        None,
        &[("sort_by", "total_credit"), ("offset", "20")],
    );
    assert!(page.body.contains("Previous 20") && !page.body.contains("Next 20"));

    // Each ordering reads one index and sorts nothing.
    for (sort, ty) in [
        ("expavg_credit", ""),
        ("total_credit", ""),
        ("total_credit", "6"),
    ] {
        let spy = Spy {
            inner: &w.db,
            log: RefCell::new(Vec::new()),
        };
        let q: Params = [("sort_by", sort), ("type", ty)]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let random = || 1;
        let ctx = SiteCtx {
            db: &spy,
            project: &w.project,
            base: "https://p.example/",
            auth: None,
            post: false,
            pepper: None,
            now: NOW,
            random: &random,
            mailer: None,
        };
        block_on(site_page(&ctx, "/top_teams.php", &q)).unwrap();
        let plan = plans(&spy);
        assert!(plan.iter().any(|d| d.contains("team_")), "{plan:?}");
        assert!(plan.iter().all(|d| !d.contains("TEMP B-TREE")), "{plan:?}");
    }
}

/// A database that remembers the statements it was given.
struct Spy<'a> {
    inner: &'a SqliteDb,
    log: RefCell<Vec<Stmt>>,
}

#[async_trait(?Send)]
impl Db for Spy<'_> {
    async fn batch(&self, stmts: Vec<Stmt>) -> DbResult<Vec<Rows>> {
        self.log.borrow_mut().extend(stmts.iter().cloned());
        self.inner.batch(stmts).await
    }
}

/// The plan lines of the statements the spy saw; panics on a full scan of a
/// table the size of the project's.
fn plans(spy: &Spy<'_>) -> Vec<String> {
    let mut all = Vec::new();
    for s in spy.log.borrow().iter() {
        if !s.sql.trim_start().starts_with("SELECT") {
            continue;
        }
        let rows = block_on(spy.inner.run(Stmt {
            sql: format!("EXPLAIN QUERY PLAN {}", s.sql),
            params: s.params.clone(),
        }))
        .unwrap();
        for r in rows.rows {
            let d = r["detail"].as_str().unwrap().to_string();
            if let Some(rest) = d.strip_prefix("SCAN ") {
                let table = rest.split_whitespace().next().unwrap_or("");
                assert!(
                    !["user", "host", "result", "workunit"].contains(&table)
                        || d.contains(" USING "),
                    "full scan of {table}: {d}\nin: {}",
                    s.sql
                );
            }
            all.push(d);
        }
    }
    all
}

#[test]
fn the_search_scores_by_name_words_type_and_country() {
    let w = World::new();
    let ada = w.account("Ada");
    let add = |id: i64, name: &str, desc: &str, ty: i64, country: &str, recent: f64| {
        w.sql(&format!(
            "INSERT INTO team (id, create_time, userid, name, name_lc, description, type, country, \
             expavg_credit, expavg_time, recent_key) VALUES ({id}, 1, {}, '{name}', '{}', '{desc}', {ty}, '{country}', {recent}, {NOW}, {})",
            ada.id,
            name.to_lowercase(),
            cb_core::avg::recent_key(recent, NOW as f64)
        ));
    };
    add(1, "Prime Finders", "hunting primes", 6, "Canada", 1.0);
    add(2, "The Prime Team", "", 2, "France", 9.0);
    add(
        3,
        "Climate",
        "we study prime climate data",
        6,
        "France",
        5.0,
    );
    add(4, "Other", "", 6, "Canada", 0.0);
    add(5, "100% Real", "", 0, "", 0.0);
    add(6, "Prime", "", 0, "", 0.0);
    let names = |q: &[(&str, &str)]| -> Vec<String> {
        let mut pairs = vec![("submit", "Search")];
        pairs.extend_from_slice(q);
        let page = w.get("/team_search.php", None, &pairs);
        assert_eq!(page.status, 200);
        let results = page
            .body
            .split("<h2>Change your search</h2>")
            .next()
            .unwrap();
        results
            .split("<a href=\"team_display.php?teamid=")
            .skip(1)
            .map(|p| p[p.find('>').unwrap() + 1..p.find("</a>").unwrap()].to_string())
            .collect()
    };
    // The exact name first (20 + 5 + 3), then the name that begins with the
    // words (5 + 3, by recent average), then the words in a name or description.
    assert_eq!(
        names(&[("keywords", "prime")]),
        ["Prime", "Prime Finders", "The Prime Team", "Climate"]
    );
    // Case does not matter; a pattern character is just a character.
    assert_eq!(names(&[("keywords", "PRIME F")]), ["Prime Finders"]);
    assert_eq!(names(&[("keywords", "%")]), ["100% Real"]);
    assert_eq!(names(&[("keywords", "_")]), Vec::<String>::new());
    // Type adds 2 and country 1 to a team in the list; each also adds its own teams.
    assert_eq!(
        names(&[("keywords", "prime"), ("type", "6"), ("country", "France")])[..3],
        ["Prime", "Prime Finders", "Climate"]
    );
    assert_eq!(
        names(&[("country", "France")]),
        ["The Prime Team", "Climate"]
    );
    assert_eq!(names(&[("type", "2")]), ["The Prime Team"]);
    // Only active teams: a recent average above 0.1 now.
    assert_eq!(
        names(&[("keywords", "prime"), ("active", "on")]),
        ["Prime Finders", "The Prime Team", "Climate"]
    );
    // No criteria: the teams with the best averages.
    assert_eq!(names(&[]).len(), 6);
    assert!(names(&[]).first().unwrap() == "The Prime Team");
    // The form without `submit` is the form.
    assert!(w
        .get("/team_search.php", None, &[])
        .body
        .contains("name=\"keywords\""));
    // XML, in the same order.
    let xml = w.get(
        "/team_search.php",
        None,
        &[("xml", "1"), ("keywords", "prime")],
    );
    assert!(xml.content_type.starts_with("text/xml"));
    assert_eq!(
        tag(&xml.body, "name"),
        ["Prime", "Prime Finders", "The Prime Team", "Climate"]
    );
    // Nothing found says so and links to creating a team.
    let page = w.get(
        "/team_search.php",
        None,
        &[("submit", "x"), ("keywords", "zzzz")],
    );
    assert!(page.body.contains("No teams were found"));
    // The search is one round trip.
    let (_, n) = w.page(
        "/team_search.php",
        None,
        false,
        &[
            ("submit", "x"),
            ("keywords", "prime"),
            ("type", "6"),
            ("country", "France"),
        ],
    );
    assert_eq!(n, 5);
}

#[test]
fn team_lookup_answers_with_the_stock_elements() {
    let w = World::new();
    let ada = w.account("Ada");
    w.sql(&format!(
        "INSERT INTO team (id, create_time, userid, name, name_lc, url, type, country, total_credit, \
         expavg_credit, expavg_time, recent_key) VALUES (3, 10, {}, 'Fast & <Loose>', 'fast & <loose>', 'x.org/?a=1&b=2', 4, 'Canada', 12.5, 3, 99, 1), \
         (4, 11, {}, 'Fast', 'fast', '', 0, 'France', 0, 0, 0, -1e9), (5, 12, 0, '50% off', '50% off', '', 0, '', 0, 0, 0, -1e9)",
        ada.id, ada.id
    ));
    let lookup = |q: &[(&str, &str)]| w.get("/team_lookup.php", None, q);
    // By ID: one team, no wrapper, the elements and their order are upstream's.
    let page = lookup(&[("team_id", "3")]);
    assert!(page.content_type.starts_with("text/xml"));
    assert!(page.body.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<team>\n    <id>3</id>\n    <create_time>10</create_time>\n    <userid>1</userid>\n"));
    assert!(page.body.contains("<name>Fast &amp; &lt;Loose></name>"));
    assert!(page.body.contains("    <url>x.org/?a=1&amp;b=2</url>\n    <type>4</type>\n    <country>Canada</country>\n    <total_credit>12.5</total_credit>\n    <expavg_credit>3</expavg_credit>\n    <expavg_time>99</expavg_time>\n</team>\n"));
    assert_eq!(error_num(&lookup(&[("team_id", "77")]).body), Some(-136));
    assert_eq!(error_num(&lookup(&[("team_id", "x")]).body), Some(-1));
    // By IDs: in the order given, duplicates repeated, what is not a team skipped.
    let page = lookup(&[("team_ids", "4, 3,zz,99,4")]);
    assert!(page.body.contains("<teams>\n<team>"));
    assert_eq!(tag(&page.body, "id"), ["4", "3", "4"]);
    let many = (1..=150)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(tag(&lookup(&[("team_ids", &many)]).body, "id").len(), 3);
    // By name: a substring, best recent average first; `%` and `_` are characters.
    let page = lookup(&[("team_name", "fast"), ("format", "xml")]);
    assert_eq!(tag(&page.body, "id"), ["3", "4"]);
    assert_eq!(
        tag(&lookup(&[("team_name", "%"), ("format", "xml")]).body, "id"),
        ["5"]
    );
    assert_eq!(
        tag(
            &lookup(&[("team_name", "FAST & <"), ("format", "xml")]).body,
            "id"
        ),
        ["3"]
    );
    assert_eq!(
        tag(
            &lookup(&[("team_name", "nope"), ("format", "xml")]).body,
            "id"
        )
        .len(),
        0
    );
    assert!(lookup(&[("team_name", "nope"), ("format", "xml")])
        .body
        .contains("<teams>\n</teams>"));
    // Without `format=xml` a name gets a page of links, escaped.
    let page = lookup(&[("team_name", "fast")]);
    assert!(page.content_type.starts_with("text/html"));
    assert!(page.body.contains("Fast &amp; &lt;Loose&gt;") && page.body.contains("teamid=4"));
    assert!(!page.body.contains("<Loose>"));
    // A missing name is an error, as XML when XML is asked for.
    assert_eq!(lookup(&[]).status, 400);
    assert_eq!(error_num(&lookup(&[("format", "xml")]).body), Some(-1));
    // One statement.
    let (_, n) = w.page("/team_lookup.php", None, false, &[("team_id", "3")]);
    assert_eq!(n, 1);
}

#[test]
fn the_founders_pages_are_the_founders() {
    let w = World::new();
    let ada = w.account("Ada");
    let bob = w.account("Bob");
    let cy = w.account("Cy");
    let t = w.create(&ada, "Alpha", &[("joinable", "on")]);
    w.create(&cy, "Beta", &[("joinable", "on")]);
    w.join(&bob, t);
    let id = t.to_string();
    let q = [("teamid", id.as_str())];

    // Only the founder: others get 403, a visitor the login page.
    for path in [
        "/team_manage.php",
        "/team_edit_form.php",
        "/team_remove_inactive_form.php",
        "/team_change_founder_form.php",
    ] {
        assert_eq!(w.get(path, Some(&bob), &q).status, 403, "{path}");
        assert_eq!(w.get(path, None, &q).status, 303, "{path}");
        assert_eq!(w.get(path, Some(&ada), &q).status, 200, "{path}");
    }
    assert_eq!(
        w.get("/team_manage.php", Some(&ada), &[("teamid", "99")])
            .status,
        404
    );
    // The menu does not offer the removal of a team with members.
    let page = w.get("/team_manage.php", Some(&ada), &q);
    assert!(page.body.contains("team_edit_form.php") && !page.body.contains("action=delete"));

    // Editing: the form carries the values, a name another team has is refused,
    // the owner of nothing cannot post.
    let form = w.get("/team_edit_form.php", Some(&ada), &q);
    assert!(form.body.contains("value=\"Alpha\"") && form.body.contains("name=\"teamid\""));
    let (page, _) = w.submit(&form, "/team_edit_action.php", &ada, &[("name", "beta")]);
    assert_eq!(page.status, 409, "{}", page.body);
    let (page, _) = w.submit(&form, "/team_edit_action.php", &bob, &[("name", "Mine")]);
    assert_eq!(page.status, 403);
    let (page, _) = w.submit(
        &form,
        "/team_edit_action.php",
        &ada,
        &[
            ("name", "Alpha Prime"),
            ("description", "New <i>text</i>"),
            ("type", "9"),
            ("country", "France"),
        ],
    );
    assert_eq!(page.status, 303);
    assert_eq!(
        w.n("SELECT count(*) FROM team WHERE id = 1 AND name = 'Alpha Prime' AND name_lc = 'alpha prime' \
             AND type = 9 AND country = 'France' AND joinable = 0 AND description = 'New <i>text</i>'"),
        1
    );
    // The same name in another case is the team's own name.
    let (page, _) = w.submit(
        &form,
        "/team_edit_action.php",
        &ada,
        &[("name", "ALPHA PRIME"), ("joinable", "on")],
    );
    assert_eq!(page.status, 303);
    assert_eq!(w.n("SELECT joinable FROM team WHERE id = 1"), 1);

    // Removing members: the founder is not listed, and cannot be removed by
    // naming the ID; a member of another team is not touched.
    let form = w.get("/team_remove_inactive_form.php", Some(&ada), &q);
    assert!(form.body.contains("Bob") && !form.body.contains("show_user.php?userid=1\""));
    assert!(form.body.contains("name=\"remove_0\""));
    let (page, _) = w.submit(
        &form,
        "/team_remove_inactive_action.php",
        &ada,
        &[
            ("ninactive_users", "3"),
            ("remove_1", &ada.id.to_string()),
            ("remove_2", &cy.id.to_string()),
        ],
    );
    assert_eq!(page.status, 200);
    assert!(page.body.contains("0 members removed"));
    assert_eq!(
        (w.team_of(&ada), w.team_of(&bob), w.team_of(&cy)),
        (1, 1, 2)
    );
    let (page, n) = w.submit(
        &form,
        "/team_remove_inactive_action.php",
        &ada,
        &[("remove_0", &bob.id.to_string())],
    );
    assert!(page.body.contains("1 member removed"), "{}", page.body);
    // The account, the team, the update.
    assert_eq!(n, 3);
    assert_eq!(
        (w.team_of(&bob), w.n("SELECT nusers FROM team WHERE id = 1")),
        (0, 1)
    );
    // Nobody left to remove.
    assert!(w
        .get("/team_remove_inactive_form.php", Some(&ada), &q)
        .body
        .contains("No members are eligible"));
    let (page, _) = w.submit(&form, "/team_remove_inactive_action.php", &bob, &[]);
    assert_eq!(page.status, 403);

    // Changing the founder: only to a member.
    w.join(&bob, t);
    let form = w.get("/team_change_founder_form.php", Some(&ada), &q);
    assert!(form.body.contains("type=\"radio\""));
    let (page, _) = w.submit(
        &form,
        "/team_change_founder_action.php",
        &ada,
        &[("userid", &cy.id.to_string())],
    );
    assert_eq!(page.status, 409);
    let (page, _) = w.submit(
        &form,
        "/team_change_founder_action.php",
        &bob,
        &[("userid", &bob.id.to_string())],
    );
    assert_eq!(page.status, 403);
    let (page, _) = w.submit(
        &form,
        "/team_change_founder_action.php",
        &ada,
        &[("userid", &bob.id.to_string())],
    );
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(page.body.contains("Bob is now the founder of ALPHA PRIME"));
    assert_eq!(w.n("SELECT userid FROM team WHERE id = 1"), bob.id);
    assert_eq!(w.get("/team_manage.php", Some(&ada), &q).status, 403);
    assert_eq!(w.get("/team_manage.php", Some(&bob), &q).status, 200);
}

#[test]
fn a_team_with_no_members_can_be_removed_and_its_id_is_not_reused() {
    let w = World::new();
    let ada = w.account("Ada");
    let bob = w.account("Bob");
    let t = w.create(&ada, "Alpha", &[("joinable", "on")]);
    w.join(&bob, t);
    let id = t.to_string();
    let q = [("teamid", id.as_str()), ("action", "delete")];
    // Members: refused, on the page and in the action.
    assert_eq!(w.get("/team_manage.php", Some(&ada), &q).status, 409);
    w.sql(&format!("UPDATE user SET teamid = 0 WHERE id = {}", bob.id));
    // The founder quits too (a founder who is not a member keeps the team).
    let form = w.get("/team_quit_form.php", Some(&ada), &[]);
    w.submit(&form, "/team_quit_action.php", &ada, &[]);
    assert_eq!(w.n("SELECT nusers FROM team WHERE id = 1"), 0);
    // The account page lists the team it founded and is not in.
    let home = w.get("/home.php", Some(&ada), &[]);
    assert!(
        home.body.contains("Founder but not member of")
            && home.body.contains("team_manage.php?teamid=1")
    );
    // Another account asking is refused; the page asks first (a GET removes nothing).
    assert_eq!(w.get("/team_manage.php", Some(&bob), &q).status, 403);
    let form = w.get("/team_manage.php", Some(&ada), &q);
    assert!(form.body.contains("Remove team") && form.body.contains("name=\"ttok\""));
    assert_eq!(w.n("SELECT count(*) FROM team"), 1);
    // No token: nothing happens.
    assert_eq!(
        w.page("/team_manage.php", Some(&ada.auth), true, &q)
            .0
            .status,
        403
    );
    let (page, _) = w.page_with(&w, &form, "/team_manage.php", &ada, &q);
    assert_eq!(page.status, 200, "{}", page.body);
    assert_eq!(w.n("SELECT count(*) FROM team"), 0);
    // The next team does not get the removed team's ID.
    let next = w.create(&ada, "Beta", &[]);
    assert_eq!(next, 2);
    assert_eq!(w.n("SELECT last_id FROM id_floor WHERE name = 'team'"), 1);
}

impl World {
    /// A POST of a form page to `path?query` with the query carried in the URL
    /// as well as the fields (`team_manage.php?...&action=delete`).
    fn page_with(
        &self,
        _w: &World,
        form: &SitePage,
        path: &str,
        who: &Account,
        query: &[(&str, &str)],
    ) -> (SitePage, u64) {
        let mut q = hidden_fields(&form.body);
        for (k, v) in query {
            q.push((k.to_string(), v.to_string()));
        }
        let pairs: Vec<(&str, &str)> = q.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        self.page(path, Some(&who.auth), true, &pairs)
    }
}

#[test]
fn a_team_without_a_founder_can_be_taken_by_a_member_and_not_otherwise() {
    let w = World::new();
    let ada = w.account("Ada");
    let bob = w.account("Bob");
    let cy = w.account("Cy");
    let t = w.create(&ada, "Alpha", &[("joinable", "on")]);
    w.join(&bob, t);
    // With a founder, a member cannot ask: this site sends no request mail.
    let page = w.get("/team_founder_transfer_form.php", Some(&bob), &[]);
    assert!(page.body.contains("This team has a founder"));
    assert!(!page.body.contains("<form"));
    let form = w.get("/team_quit_form.php", Some(&bob), &[]);
    let (page, _) = w.submit(
        &form,
        "/team_founder_transfer_action.php",
        &bob,
        &[("action", "initiate_transfer")],
    );
    assert_eq!(page.status, 409, "{}", page.body);
    // The other actions of the request flow are not offered.
    let (page, _) = w.submit(
        &form,
        "/team_founder_transfer_action.php",
        &bob,
        &[("action", "finalize_transfer")],
    );
    assert_eq!(page.status, 400);
    // Not a member: nothing.
    assert_eq!(
        w.get("/team_founder_transfer_form.php", Some(&cy), &[])
            .status,
        403
    );
    // The founder's account is deleted: the team has none, and a member takes it.
    w.sql(&format!("UPDATE team SET userid = 0 WHERE id = {t}"));
    let page = w.get("/team_founder_transfer_form.php", Some(&bob), &[]);
    assert!(
        page.body.contains("This team has no founder") && page.body.contains("initiate_transfer")
    );
    let team_page = w.get(
        "/team_display.php",
        Some(&bob),
        &[("teamid", &t.to_string())],
    );
    assert!(team_page.body.contains("Become the founder"));
    let (page, _) = w.submit(&page, "/team_founder_transfer_action.php", &bob, &[]);
    assert_eq!(page.status, 303);
    assert_eq!(
        w.n(&format!("SELECT userid FROM team WHERE id = {t}")),
        bob.id
    );
    // Only once.
    let ada_form = w.get("/team_quit_form.php", Some(&ada), &[]);
    let (page, _) = w.submit(
        &ada_form,
        "/team_founder_transfer_action.php",
        &ada,
        &[("action", "initiate_transfer")],
    );
    assert_eq!(page.status, 409);
}

#[test]
fn deleting_an_account_leaves_its_teams_without_a_founder_and_counts_right() {
    let w = World::new();
    let ada = w.account("Ada");
    let bob = w.account("Bob");
    let t = w.create(&ada, "Alpha", &[("joinable", "on")]);
    w.join(&bob, t);
    // Ada founded a second team and is not in it; Bob founded a third.
    w.sql(&format!(
        "INSERT INTO team (id, create_time, userid, name, name_lc, expavg_time) VALUES (2, 1, {}, 'Beta', 'beta', 1), (3, 1, {}, 'Gamma', 'gamma', 1)",
        ada.id, bob.id
    ));
    let t2 = 3;
    assert_eq!(w.n("SELECT nusers FROM team WHERE id = 1"), 2);
    let hash = "tokenhash";
    w.sql(&format!(
        "INSERT INTO token (userid, type, token_hash, create_time, expire_time) VALUES ({}, 'D', '{hash}', 1, {})",
        ada.id,
        NOW + 1000
    ));
    let user: cb_core::model::User = block_on(cb_db::query_one(
        &w.db,
        stmt!("SELECT * FROM user WHERE id = ?", ada.id),
    ))
    .unwrap()
    .unwrap();
    let started = block_on(cb_core::site_delete::start_deletion(
        &w.db,
        &user,
        hash,
        "deleted_1_x",
        2,
        NOW,
    ))
    .unwrap();
    assert!(started);
    // The deletion queue row was written (the statement after the account's
    // update still saw its change), the team has no founder, the count is down.
    assert_eq!(w.n("SELECT count(*) FROM user_deletion"), 1);
    assert_eq!(w.n("SELECT userid FROM team WHERE id = 1"), 0);
    assert_eq!(w.n("SELECT userid FROM team WHERE id = 2"), 0);
    assert_eq!(w.n("SELECT nusers FROM team WHERE id = 1"), 1);
    assert_eq!(
        w.n(&format!("SELECT userid FROM team WHERE id = {t2}")),
        bob.id
    );
    let page = w.get("/team_display.php", None, &[("teamid", "1")]);
    assert!(page.body.contains("<dt>Founder</dt><dd>---</dd>"));
}

#[test]
fn the_account_pages_show_the_team() {
    let w = World::new();
    let ada = w.account("Ada");
    let bob = w.account("Bob");
    let home = w.get("/home.php", Some(&ada), &[]);
    assert!(home.body.contains("find a team"));
    let t = w.create(&ada, "Alpha \"x\"", &[("joinable", "on")]);
    let home = w.get("/home.php", Some(&ada), &[]);
    assert!(home.body.contains(&format!(
        "<a href=\"team_display.php?teamid={t}\">Alpha &quot;x&quot;</a> &middot; <a href=\"team_quit_form.php\">Quit team</a> &middot; <a href=\"team_manage.php?teamid={t}\">Administer</a>"
    )));
    w.join(&bob, t);
    let home = w.get("/home.php", Some(&bob), &[]);
    assert!(home.body.contains("Member of team") && !home.body.contains("Administer"));
    // The public page of an account links to its team, and a dash when none.
    let page = w.get("/show_user.php", None, &[("userid", &bob.id.to_string())]);
    assert!(page.body.contains(&format!(
        "<th>Team</th><td><a href=\"team_display.php?teamid={t}\">Alpha &quot;x&quot;</a></td>"
    )));
    w.sql(&format!("UPDATE user SET teamid = 0 WHERE id = {}", bob.id));
    let page = w.get("/show_user.php", None, &[("userid", &bob.id.to_string())]);
    assert!(page.body.contains("<th>Team</th><td>&mdash;</td>"));
    // The XML carries the ID as upstream's (and no more).
    let xml = w.get(
        "/show_user.php",
        None,
        &[("userid", &ada.id.to_string()), ("format", "xml")],
    );
    assert!(xml.body.contains(&format!("<teamid>{t}</teamid>")));
    // The navigation has one Teams entry.
    let nav = w.get("/top_teams.php", None, &[]).body;
    assert_eq!(nav.matches("<a href=\"team.php\">Teams</a>").count(), 1);
    let nav = w.get("/team.php", None, &[]);
    assert!(nav.body.contains("Find a team") && nav.body.contains("team_create_form.php"));
    assert!(nav.body.contains("top_teams.php?type=12"));
}

#[test]
fn account_managers_join_and_quit_teams() {
    let w = World::new();
    let ada = w.account("Ada");
    let bob = w.account("Bob");
    let t = w.create(&ada, "Alpha", &[("joinable", "on")]);
    let closed = w.create(&bob, "Closed", &[]);
    let set = |who: &Account, params: &[(&str, &str)]| -> (String, u64) {
        let mut pairs = vec![("account_key", who.auth.as_str())];
        pairs.extend_from_slice(params);
        let q: Params = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let before = w.db.statements();
        let reply = block_on(cb_core::web::am_set_info(
            &w.db,
            &w.project.config,
            PEPPER,
            NOW,
            &q,
        ))
        .unwrap();
        (reply, w.db.statements() - before)
    };
    let cy = w.account("Cy");
    // Join: the account and the update, nothing more.
    let (reply, n) = set(&cy, &[("teamid", &t.to_string())]);
    assert!(reply.contains("<success/>"), "{reply}");
    assert_eq!(n, 2);
    assert_eq!(w.team_of(&cy), t);
    assert_eq!(w.n(&format!("SELECT nusers FROM team WHERE id = {t}")), 2);
    // A team that is not joinable, one that does not exist, and a value that is
    // not a number leave the account where it is, as upstream's silent skip.
    for v in [
        closed.to_string(),
        "999".to_string(),
        "x".to_string(),
        "-3".to_string(),
    ] {
        let (reply, _) = set(&cy, &[("teamid", &v)]);
        assert!(reply.contains("<success/>"), "{reply}");
        assert_eq!(w.team_of(&cy), t, "{v}");
    }
    // Joining with other fields in the same request is the same update.
    let (reply, n) = set(&cy, &[("teamid", &t.to_string()), ("venue", "work")]);
    assert!(reply.contains("<success/>"));
    assert_eq!(n, 2);
    // am_get_info says the account is in the team, and a founder as such.
    let info = block_on(am_get_info(&w.db, &cy.auth)).unwrap();
    assert!(info.contains(&format!("<teamid>{t}</teamid>")) && !info.contains("<teamfounder/>"));
    let info = block_on(am_get_info(&w.db, &ada.auth)).unwrap();
    assert!(
        info.contains("<venue></venue>\n<teamfounder/>\n</am_get_info_reply>"),
        "{info}"
    );
    // A founder who is not a member is not reported as founder, as upstream's
    // check is made only for an account with a team.
    w.sql(&format!("UPDATE user SET teamid = 0 WHERE id = {}", ada.id));
    assert!(!block_on(am_get_info(&w.db, &ada.auth))
        .unwrap()
        .contains("teamfounder"));
    // Quit.
    let (reply, n) = set(&cy, &[("teamid", "0")]);
    assert!(reply.contains("<success/>"));
    assert_eq!((n, w.team_of(&cy)), (2, 0));
    assert_eq!(w.n(&format!("SELECT nusers FROM team WHERE id = {t}")), 0);
    // No teamid: no change.
    let (_, n) = set(&cy, &[]);
    assert_eq!(n, 1);
}

#[test]
fn a_project_can_turn_teams_off() {
    let w = World::with_config("<disable_teams/>\n");
    let ada = w.account("Ada");
    w.sql(&format!(
        "INSERT INTO team (id, create_time, userid, name, name_lc, joinable, expavg_time) VALUES (1, 1, {}, 'Alpha', 'alpha', 1, 1)",
        ada.id
    ));
    for path in TEAM_PATHS {
        let q: &[(&str, &str)] = if path == "/team_lookup.php" {
            &[]
        } else {
            &[("teamid", "1")]
        };
        let page = w.get(path, Some(&ada), q);
        assert_eq!(page.status, 404, "{path}");
        assert!(page.body.contains("Teams are disabled"), "{path}");
        let page = w.page(path, Some(&ada.auth), true, q).0;
        assert_eq!(page.status, 404, "{path}");
    }
    // The RPC answers an XML error.
    for q in [
        &[("team_id", "1")][..],
        &[("team_name", "a"), ("format", "xml")],
    ] {
        let page = w.get("/team_lookup.php", None, q);
        assert_eq!(page.status, 200);
        assert_eq!(error_num(&page.body), Some(-1));
        assert!(page.body.contains("Teams are disabled"));
    }
    // No navigation entry, no row on the account pages, no team in `am_set_info`.
    for body in [
        w.get("/home.php", Some(&ada), &[]).body,
        w.get("/top_users.php", None, &[]).body,
        cb_core::web::master_page_with(&w.project, None, NOW),
    ] {
        assert!(
            !body.contains("team.php") && !body.contains("Teams"),
            "{body}"
        );
    }
    let page = w.get("/show_user.php", None, &[("userid", &ada.id.to_string())]);
    assert!(!page.body.contains("Team"));
    let q: Params = [("account_key", ada.auth.as_str()), ("teamid", "1")]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let (project_config, _) = (&w.project.config, 0);
    let reply = block_on(cb_core::web::am_set_info(
        &w.db,
        project_config,
        PEPPER,
        NOW,
        &q,
    ))
    .unwrap();
    assert!(reply.contains("<success/>"));
    assert_eq!(w.team_of(&ada), 0);
    // On by default.
    assert!(!World::new().project.config.disable_teams);
}

#[test]
fn team_creation_can_be_restricted() {
    let w = World::with_config("<team_create_need_credit/>\n");
    let ada = w.account("Ada");
    let page = w.get("/team_create_form.php", Some(&ada), &[]);
    assert!(page
        .body
        .contains("You must complete a task to create a team"));
    assert!(!page.body.contains("<form"));
    w.sql("UPDATE user SET total_credit = 1");
    assert!(w
        .get("/team_create_form.php", Some(&ada), &[])
        .body
        .contains("<form"));
    w.create(&ada, "Alpha", &[]);

    let w = World::with_config("<disable_team_creation/>\n");
    let ada = w.account("Ada");
    let page = w.get("/team_create_form.php", Some(&ada), &[]);
    assert!(page.body.contains("Team creation is disabled") && !page.body.contains("<form"));
    let q = [("name", "x")];
    let page = w
        .page("/team_create_action.php", Some(&ada.auth), true, &q)
        .0;
    assert_eq!(page.status, 403);
    // Joining and the other pages are unaffected.
    assert_eq!(w.get("/team.php", None, &[]).status, 200);
}

#[test]
fn page_costs_are_bounded() {
    let w = World::new();
    let ada = w.account("Ada");
    let t = w.create(&ada, "Alpha", &[("joinable", "on")]);
    let id = t.to_string();
    // The visitor, then the team with its founder.
    assert_eq!(
        w.page(
            "/team_display.php",
            Some(&ada.auth),
            false,
            &[("teamid", &id)]
        )
        .1,
        2
    );
    assert_eq!(
        w.page("/team_display.php", None, false, &[("teamid", &id)])
            .1,
        1
    );
    // The viewer and the team's members, and the list.
    assert_eq!(
        w.page("/team_members.php", None, false, &[("teamid", &id)])
            .1,
        2
    );
    assert_eq!(w.page("/top_teams.php", None, false, &[]).1, 1);
    assert_eq!(w.page("/team.php", None, false, &[]).1, 0);
    // The public page of an account with a team costs one more.
    assert_eq!(
        w.page(
            "/show_user.php",
            None,
            false,
            &[("userid", &ada.id.to_string())]
        )
        .1,
        2
    );
    // Creating takes the account, then the one batch (two statements).
    let bob = w.account("Bob");
    let form = w.get("/team_create_form.php", Some(&bob), &[]);
    let (_, n) = w.submit(&form, "/team_create_action.php", &bob, &[("name", "Beta")]);
    assert_eq!(n, 1 + 2);
    // The member lists are indexed by team.
    let plan = block_on(w.db.run(stmt!(
        "EXPLAIN QUERY PLAN SELECT id FROM user WHERE teamid = ? AND teamid > 0 AND blocked = 0 ORDER BY recent_key DESC LIMIT 21 OFFSET 0",
        1
    )))
    .unwrap();
    let text: Vec<String> = plan
        .rows
        .iter()
        .map(|r| r["detail"].as_str().unwrap().to_string())
        .collect();
    assert!(text.iter().any(|d| d.contains("user_team")), "{text:?}");
    // The founder's teams and the deletion check use their indexes too.
    let plan_of = |sql: &str, params: Vec<cb_db::Value>| -> Vec<String> {
        block_on(w.db.run(Stmt {
            sql: format!("EXPLAIN QUERY PLAN {sql}"),
            params,
        }))
        .unwrap()
        .rows
        .iter()
        .map(|r| r["detail"].as_str().unwrap().to_string())
        .collect()
    };
    let text = plan_of(
        "SELECT id, name, userid FROM team WHERE id = ? OR userid = ? ORDER BY id LIMIT 20",
        vec![0.into(), 1.into()],
    );
    assert!(text.iter().all(|d| !d.starts_with("SCAN team")), "{text:?}");
    let text = plan_of(
        "DELETE FROM team WHERE id = ? AND userid = ? AND NOT EXISTS (SELECT 1 FROM user WHERE teamid = ? AND teamid > 0)",
        vec![1.into(), 1.into(), 1.into()],
    );
    assert!(text.iter().any(|d| d.contains("user_team")), "{text:?}");
    // A credit grant does not touch the member index: it is partial and its
    // column is not in the grant's `SET`.
    assert_eq!(w.f("SELECT total_credit FROM team WHERE id = 1"), 0.0);
}

// ---------------------------------------------------------------------------
// The writes of the team pages compare the account key they read.

/// A database that, just before the first batch that starts with `prefix`,
/// runs `change` (one `?`: the account's ID), as a request that arrived
/// between the page's read of the account and its write would.
struct Between<'a> {
    inner: &'a SqliteDb,
    prefix: &'static str,
    change: &'static str,
    user_id: i64,
    fired: Cell<bool>,
}

const REKEY: &str =
    "UPDATE user SET authenticator = 'replacedkey0000000000000000000000' WHERE id = ?";
const BLOCK: &str = "UPDATE user SET blocked = 1 WHERE id = ?";

#[async_trait(?Send)]
impl Db for Between<'_> {
    async fn batch(&self, stmts: Vec<Stmt>) -> DbResult<Vec<Rows>> {
        if !self.fired.get()
            && stmts
                .iter()
                .any(|s| s.sql.trim_start().starts_with(self.prefix))
        {
            self.fired.set(true);
            self.inner
                .batch(vec![stmt!(self.change, self.user_id)])
                .await?;
        }
        self.inner.batch(stmts).await
    }
}

/// Submits `form` to `action` as `who`, with the account changed by `change`
/// just before the statement that starts with `prefix`. Returns the page.
fn raced(
    w: &World,
    who: &Account,
    prefix: &'static str,
    change: &'static str,
    form: &SitePage,
    action: &str,
    changes: &[(&str, &str)],
) -> SitePage {
    let db = Between {
        inner: &w.db,
        prefix,
        change,
        user_id: who.id,
        fired: Cell::new(false),
    };
    let mut q = hidden_fields(&form.body);
    for (k, v) in changes {
        q.retain(|(name, _)| name != k);
        q.push((k.to_string(), v.to_string()));
    }
    let pairs: Vec<(&str, &str)> = q.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let page = w.page_on(&db, action, Some(&who.auth), true, &pairs).0;
    assert!(db.fired.get(), "no statement started with {prefix}");
    page
}

#[test]
fn a_write_after_the_key_was_replaced_or_the_account_blocked_changes_nothing() {
    for change in [REKEY, BLOCK] {
        let w = World::new();
        let ada = w.account("Ada");
        let bob = w.account("Bob");
        let cy = w.account("Cy");
        let t = w.create(&ada, "Alpha", &[("joinable", "on")]);
        w.join(&bob, t);
        let id = t.to_string();
        let q = [("teamid", id.as_str())];
        let team = |w: &World| {
            (
                w.n("SELECT userid FROM team WHERE id = 1"),
                w.n("SELECT nusers FROM team WHERE id = 1"),
            )
        };
        let name = |w: &World| {
            block_on(w.db.run(stmt!("SELECT name FROM team WHERE id = 1")))
                .unwrap()
                .rows[0]["name"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let reset = |w: &World, who: &Account| {
            w.sql(&format!(
                "UPDATE user SET authenticator = '{}', blocked = 0 WHERE id = {}",
                who.auth, who.id
            ));
        };

        // Edit the team.
        let form = w.get("/team_edit_form.php", Some(&ada), &q);
        let page = raced(
            &w,
            &ada,
            "UPDATE OR IGNORE team",
            change,
            &form,
            "/team_edit_action.php",
            &[("name", "Renamed")],
        );
        assert_eq!(page.status, 409, "{}", page.body);
        assert_eq!(name(&w), "Alpha");
        reset(&w, &ada);

        // Remove a member.
        let form = w.get("/team_remove_inactive_form.php", Some(&ada), &q);
        let page = raced(
            &w,
            &ada,
            "UPDATE user SET teamid = 0 WHERE teamid",
            change,
            &form,
            "/team_remove_inactive_action.php",
            &[("remove_0", &bob.id.to_string())],
        );
        assert!(page.body.contains("0 members removed"), "{}", page.body);
        assert_eq!(w.team_of(&bob), t);
        reset(&w, &ada);

        // Hand the team over.
        let form = w.get("/team_change_founder_form.php", Some(&ada), &q);
        let page = raced(
            &w,
            &ada,
            "UPDATE team SET userid = ? WHERE id = ? AND userid = ?",
            change,
            &form,
            "/team_change_founder_action.php",
            &[("userid", &bob.id.to_string())],
        );
        assert_eq!(page.status, 409, "{}", page.body);
        assert_eq!(team(&w), (ada.id, 2));
        reset(&w, &ada);

        // Join a team.
        let form = w.get("/team_display.php", Some(&cy), &q);
        let page = raced(
            &w,
            &cy,
            "UPDATE user SET teamid = ? WHERE id",
            change,
            &form,
            "/team_join.php",
            &[],
        );
        assert_eq!(page.status, 409, "{}", page.body);
        assert_eq!(w.team_of(&cy), 0);
        reset(&w, &cy);

        // Quit it.
        let form = w.get("/team_quit_form.php", Some(&bob), &[]);
        let page = raced(
            &w,
            &bob,
            "UPDATE user SET teamid = 0 WHERE id",
            change,
            &form,
            "/team_quit_action.php",
            &[],
        );
        assert_eq!(page.status, 409, "{}", page.body);
        assert_eq!(w.team_of(&bob), t);
        reset(&w, &bob);

        // Create a team.
        let form = w.get("/team_create_form.php", Some(&cy), &[]);
        let page = raced(
            &w,
            &cy,
            "INSERT OR IGNORE INTO team",
            change,
            &form,
            "/team_create_action.php",
            &[("name", "Beta")],
        );
        assert_eq!(page.status, 409, "{}", page.body);
        assert_eq!((w.n("SELECT count(*) FROM team"), w.team_of(&cy)), (1, 0));
        reset(&w, &cy);

        // Take a team with no founder.
        w.sql("UPDATE team SET userid = 0 WHERE id = 1");
        let form = w.get("/team_founder_transfer_form.php", Some(&bob), &[]);
        let page = raced(
            &w,
            &bob,
            "UPDATE team SET userid = ? WHERE id = ? AND userid = 0",
            change,
            &form,
            "/team_founder_transfer_action.php",
            &[],
        );
        assert_eq!(page.status, 409, "{}", page.body);
        assert_eq!(w.n("SELECT userid FROM team WHERE id = 1"), 0);
        reset(&w, &bob);

        // Remove an empty team: the founder is not a member.
        w.sql("UPDATE team SET userid = 1");
        w.sql("UPDATE user SET teamid = 0");
        let form = w.get(
            "/team_manage.php",
            Some(&ada),
            &[("teamid", &id), ("action", "delete")],
        );
        assert!(form.body.contains("name=\"ttok\""), "{}", form.body);
        let page = raced(
            &w,
            &ada,
            "DELETE FROM team",
            change,
            &form,
            "/team_manage.php",
            &[("teamid", &id), ("action", "delete")],
        );
        assert_eq!(page.status, 409, "{}", page.body);
        assert_eq!(w.n("SELECT count(*) FROM team"), 1);
    }
}

#[test]
fn the_login_returns_to_the_team_asked_for() {
    let w = World::new();
    let next = |path: &str, params: &[(&str, &str)]| {
        let (page, _) = w.page(path, None, false, params);
        assert_eq!(page.status, 303, "{path}");
        page.location.unwrap()
    };
    let login = "https://p.example/login_form.php?next_url=";
    assert_eq!(
        next("/team_join_form.php", &[("id", "7")]),
        format!("{login}team_join_form.php%3Fid%3D7")
    );
    assert_eq!(
        next("/team_join_form.php", &[("teamid", " 8 ")]),
        format!("{login}team_join_form.php%3Fid%3D8")
    );
    for path in [
        "/team_edit_form.php",
        "/team_manage.php",
        "/team_remove_inactive_form.php",
        "/team_change_founder_form.php",
    ] {
        let want = if path == "/team_edit_form.php" {
            "team_manage.php"
        } else {
            &path[1..]
        };
        assert_eq!(
            next(path, &[("teamid", "9")]),
            format!("{login}{want}%3Fteamid%3D9"),
            "{path}"
        );
    }
    // Only the number: text that is not one is dropped, not copied.
    for bad in ["1&x=2", "1/../..", "<script>", "", "1e3"] {
        assert_eq!(
            next("/team_join_form.php", &[("id", bad)]),
            format!("{login}team_join_form.php"),
            "{bad}"
        );
        assert_eq!(
            next("/team_manage.php", &[("teamid", bad)]),
            format!("{login}team_manage.php"),
            "{bad}"
        );
    }
    // The login form accepts the address it is given.
    let page = w.get(
        "/login_form.php",
        None,
        &[("next_url", "team_join_form.php?id=7")],
    );
    assert!(
        page.body.contains("value=\"team_join_form.php?id=7\""),
        "{}",
        page.body
    );
}

#[test]
fn the_account_page_keeps_the_current_team_whatever_the_account_founded() {
    let w = World::new();
    let ada = w.account("Ada");
    // 21 teams founded by Ada, which she is not in, and a newer one by someone else.
    for i in 1..=21 {
        w.sql(&format!(
            "INSERT INTO team (id, create_time, userid, name, name_lc, expavg_time) VALUES ({i}, 1, {}, 'Founded {i:02}', 'founded {i:02}', 1)",
            ada.id
        ));
    }
    w.sql("INSERT INTO team (id, create_time, userid, name, name_lc, expavg_time, joinable) VALUES (22, 1, 99, 'Newest', 'newest', 1, 1)");
    w.join(&ada, 22);
    assert_eq!(w.team_of(&ada), 22);
    let (home, n) = w.page("/home.php", Some(&ada.auth), false, &[]);
    assert_eq!(n, 2, "the account and one statement for the teams");
    assert!(home.body.contains("Member of team"), "{}", home.body);
    assert!(home
        .body
        .contains("teamid=22\">Newest</a> &middot; <a href=\"team_quit_form.php\">Quit team</a>"));
    assert!(!home.body.contains("<dt>Team</dt>"));
    // The others are limited to 20, the oldest first.
    assert_eq!(home.body.matches("Founder but not member of").count(), 20);
    assert!(home.body.contains("Founded 20") && !home.body.contains("Founded 21"));

    // As founder and member of one team too: its own Administer link.
    w.sql(&format!(
        "UPDATE team SET userid = {} WHERE id = 22",
        ada.id
    ));
    let home = w.get("/home.php", Some(&ada), &[]);
    assert!(home.body.contains("team_manage.php?teamid=22\">Administer"));
    assert_eq!(home.body.matches("Founder but not member of").count(), 20);

    // One statement: a primary key lookup and the founder index.
    let plan = block_on(w.db.run(stmt!(
        "EXPLAIN QUERY PLAN SELECT id, name, userid FROM (\
         SELECT id, name, userid, 0 AS rank FROM team WHERE id = ? \
         UNION ALL \
         SELECT id, name, userid, 1 AS rank FROM (SELECT id, name, userid FROM team \
         WHERE userid = ? AND id != ? ORDER BY id LIMIT 20)) ORDER BY rank, id",
        22,
        1,
        22
    )))
    .unwrap();
    let text: Vec<String> = plan
        .rows
        .iter()
        .map(|r| r["detail"].as_str().unwrap().to_string())
        .collect();
    assert!(text.iter().all(|d| !d.starts_with("SCAN team")), "{text:?}");
}

// ---------------------------------------------------------------------------
// Review round 2.

#[test]
fn a_new_team_does_not_take_the_id_an_account_still_names() {
    let w = World::new();
    let ada = w.account("Ada");
    let old = w.account("Imported");
    // An account moved from a stock server names team 1, which this server does not have.
    w.sql(&format!("UPDATE user SET teamid = 1 WHERE id = {}", old.id));
    let t = w.create(&ada, "Alpha", &[]);
    assert_eq!(t, 2);
    assert_eq!(w.team_of(&old), 1);
    assert_eq!(w.n("SELECT nusers FROM team WHERE id = 2"), 1);
    // The ID is also above one an account names higher still, and above the floor.
    w.sql(&format!(
        "UPDATE user SET teamid = 40 WHERE id = {}",
        old.id
    ));
    let bob = w.account("Bob");
    assert_eq!(w.create(&bob, "Beta", &[]), 41);
    // The extra term reads one index entry, not the user table.
    let plan = block_on(w.db.run(stmt!(
        "EXPLAIN QUERY PLAN SELECT max(teamid) FROM user WHERE teamid > 0"
    )))
    .unwrap();
    let text: Vec<String> = plan
        .rows
        .iter()
        .map(|r| r["detail"].as_str().unwrap().to_string())
        .collect();
    assert!(text.iter().any(|d| d.contains("user_team")), "{text:?}");
    assert!(text.iter().all(|d| !d.starts_with("SCAN user")), "{text:?}");
}

#[test]
fn am_set_info_writes_nothing_for_an_account_blocked_after_it_was_read() {
    let w = World::new();
    let ada = w.account("Ada");
    let bob = w.account("Bob");
    let t = w.create(&ada, "Alpha", &[("joinable", "on")]);
    let run = |db: &dyn Db, who: &Account, team: &str| {
        let q: Params = [
            ("account_key", who.auth.as_str()),
            ("teamid", team),
            ("venue", "work"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        block_on(cb_core::web::am_set_info(
            db,
            &w.project.config,
            PEPPER,
            NOW,
            &q,
        ))
        .unwrap()
    };
    // Blocked between the read and the write: no join, and the other fields
    // of the same request are not written either.
    let between = Between {
        inner: &w.db,
        prefix: "UPDATE user SET",
        change: BLOCK,
        user_id: bob.id,
        fired: Cell::new(false),
    };
    let reply = run(&between, &bob, &t.to_string());
    assert!(between.fired.get());
    assert_eq!(error_num(&reply), Some(-199), "{reply}");
    assert_eq!(w.team_of(&bob), 0);
    assert_eq!(
        w.n(&format!(
            "SELECT count(*) FROM user WHERE id = {} AND venue = 'work'",
            bob.id
        )),
        0
    );
    // The read already refuses an account that is blocked: the same reply as an unknown key.
    assert_eq!(error_num(&run(&w.db, &bob, &t.to_string())), Some(-136));
    // Quitting is guarded the same way.
    w.sql(&format!(
        "UPDATE user SET blocked = 0, teamid = {t} WHERE id = {}",
        bob.id
    ));
    let between = Between {
        inner: &w.db,
        prefix: "UPDATE user SET",
        change: BLOCK,
        user_id: bob.id,
        fired: Cell::new(false),
    };
    assert_eq!(error_num(&run(&between, &bob, "0")), Some(-199));
    assert_eq!(w.team_of(&bob), t);
}

#[test]
fn top_teams_past_its_limit_is_an_error_not_the_first_page() {
    let w = World::new();
    let ada = w.account("Ada");
    w.create(&ada, "Alpha", &[]);
    assert_eq!(
        w.get("/top_teams.php", None, &[("offset", "1980")]).status,
        200
    );
    for off in ["2000", "2020", "100000"] {
        let page = w.get("/top_teams.php", None, &[("offset", off)]);
        assert_eq!(page.status, 400, "{off}");
        assert!(page.body.contains("Limit exceeded"), "{off}");
        assert!(!page.body.contains("Alpha"));
    }
    // Not a multiple of the page size: the first page, as upstream.
    assert!(w
        .get("/top_teams.php", None, &[("offset", "7")])
        .body
        .contains("Alpha"));
    // The cache key of an offset past the limit is its own, not the first page's.
    let key = |off: &str| {
        let q: Params = [("offset", off)]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        cb_core::site_boards::public_cache_key("/top_teams.php", &q).unwrap()
    };
    assert_ne!(key("2000"), key("0"));
    assert_eq!(key("2000"), key("100000"));
    assert_eq!(key("7"), key("0"));
}

#[test]
fn taking_a_founderless_team_names_the_team_the_form_showed() {
    let w = World::new();
    let ada = w.account("Ada");
    let bob = w.account("Bob");
    let cy = w.account("Cy");
    let a = w.create(&ada, "Alpha", &[("joinable", "on")]);
    let b = w.create(&cy, "Beta", &[("joinable", "on")]);
    w.join(&bob, a);
    w.sql("UPDATE team SET userid = 0");
    let form = w.get("/team_founder_transfer_form.php", Some(&bob), &[]);
    assert!(
        form.body
            .contains(&format!("name=\"teamid\" value=\"{a}\"")),
        "{}",
        form.body
    );
    // Bob joins Beta, which has no founder either, then submits the old form.
    w.sql(&format!(
        "UPDATE user SET teamid = {b} WHERE id = {}",
        bob.id
    ));
    let (page, _) = w.submit(&form, "/team_founder_transfer_action.php", &bob, &[]);
    assert_eq!(page.status, 409, "{}", page.body);
    assert_eq!(w.n("SELECT count(*) FROM team WHERE userid = 0"), 2);
    // A form for the team he is in works; one naming no team does not.
    let form = w.get("/team_founder_transfer_form.php", Some(&bob), &[]);
    let (page, _) = w.submit(
        &form,
        "/team_founder_transfer_action.php",
        &bob,
        &[("teamid", "")],
    );
    assert_eq!(page.status, 409);
    let (page, _) = w.submit(&form, "/team_founder_transfer_action.php", &bob, &[]);
    assert_eq!(page.status, 303);
    assert_eq!(
        w.n(&format!("SELECT userid FROM team WHERE id = {b}")),
        bob.id
    );
    assert_eq!(w.n(&format!("SELECT userid FROM team WHERE id = {a}")), 0);
}

#[test]
fn an_account_inserted_with_a_team_counts_as_a_member() {
    let w = World::new();
    let ada = w.account("Ada");
    let t = w.create(&ada, "Alpha", &[]);
    assert_eq!(w.n("SELECT nusers FROM team WHERE id = 1"), 1);
    // Rows loaded by SQL: two with the team, one with none, one with a team that is not there.
    w.sql(
        "INSERT INTO user (id, create_time, email_addr, authenticator, teamid) VALUES \
         (50, 1, 'a@x.example', 'k50', 1), (51, 1, 'b@x.example', 'k51', 1), \
         (52, 1, 'c@x.example', 'k52', 0), (53, 1, 'd@x.example', 'k53', 99)",
    );
    assert_eq!(w.n(&format!("SELECT nusers FROM team WHERE id = {t}")), 3);
    // Deleting one takes one away, not one of the others.
    w.sql("DELETE FROM user WHERE id = 50");
    assert_eq!(w.n("SELECT nusers FROM team WHERE id = 1"), 2);
    let counted = "SELECT count(*) FROM user WHERE teamid = 1";
    assert_eq!(w.n(counted), 2);
    // A sign-up (team 0) fires nothing.
    let before = w.n("SELECT nusers FROM team WHERE id = 1");
    w.account("Dee");
    assert_eq!(w.n("SELECT nusers FROM team WHERE id = 1"), before);
    // The recount the docs give for a bulk load.
    w.sql("UPDATE team SET nusers = 40");
    w.sql("UPDATE team SET nusers = (SELECT count(*) FROM user WHERE teamid = team.id AND teamid > 0)");
    assert_eq!(w.n("SELECT nusers FROM team WHERE id = 1"), 2);
}
