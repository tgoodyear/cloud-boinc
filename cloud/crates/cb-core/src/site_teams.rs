//! The team pages: the list and search, a team and its members, creating,
//! joining, quitting and editing a team, the founder's actions, `top_teams.php`
//! and the `team_lookup.php` RPC. Their behaviour follows upstream
//! `html/user/team*.php`, `top_teams.php` and `html/inc/team.inc`; what
//! differs is in `cloud/docs/compatibility.md` and under "Teams" in
//! `cloud/docs/architecture.md`.
//!
//! The rules the pages share with the other account pages: a page that
//! changes something takes a POST with the form token its form page made
//! (`site_account::token_ok`), and the changing statement names the account
//! key that was read, so a request that began before `reset_account_key.php`
//! writes nothing. Every value a person typed is HTML-escaped where it is
//! shown. With `<disable_teams>` set, every page here answers 404 and the RPC
//! an XML error.

use cb_db::stmt;
use cb_proto::web_rpc::{err, write_error, XML_HEADER_UTF8};

use crate::avg::decayed;
use crate::countries::COUNTRIES;
use crate::model::{Team, User};
use crate::site::{
    date_str, error_page, int_param, label_row, layout, number_format, str_param, to_login, viewer,
    Params, SiteCtx, SitePage,
};
use crate::site_account::{token_fields, token_ok, TIMED_OUT};
use crate::site_boards::{stack_table, StackCol};
use crate::site_ui;
use crate::teams::*;
use crate::web::html_escape;
use crate::CoreResult;

/// The paths of this module's pages, for [`crate::site::SITE_PATHS`] and the
/// check that `<disable_teams>` closes every one.
pub const TEAM_PATHS: [&str; 22] = [
    "/team.php",
    "/team_search.php",
    "/team_display.php",
    "/team_members.php",
    "/team_create_form.php",
    "/team_create_action.php",
    "/team_join.php",
    "/team_join_form.php",
    "/team_join_action.php",
    "/team_quit_form.php",
    "/team_quit_action.php",
    "/team_edit_form.php",
    "/team_edit_action.php",
    "/team_manage.php",
    "/team_remove_inactive_form.php",
    "/team_remove_inactive_action.php",
    "/team_change_founder_form.php",
    "/team_change_founder_action.php",
    "/team_founder_transfer_form.php",
    "/team_founder_transfer_action.php",
    "/team_lookup.php",
    "/top_teams.php",
];

/// Members listed on one page of the founder's forms.
const FORM_PAGE: usize = 50;
/// Members one removal takes at most: below D1's 100 bound parameters.
const REMOVE_MAX: usize = 90;

/// Routes a team page. `None` for a path that is not one.
pub async fn team_page(ctx: &SiteCtx<'_>, path: &str, q: &Params) -> CoreResult<Option<SitePage>> {
    if !TEAM_PATHS.contains(&path) {
        return Ok(None);
    }
    if ctx.project.config.disable_teams {
        return Ok(Some(if path == "/team_lookup.php" && wants_xml(q) {
            SitePage::xml(write_error(-1, Some("Teams are disabled")))
        } else {
            error_page(ctx.project, 404, "Teams are disabled.")
        }));
    }
    Ok(Some(match path {
        "/team.php" => team_home(ctx),
        "/team_search.php" => team_search(ctx, q).await?,
        "/team_display.php" => team_display(ctx, q).await?,
        "/team_members.php" => team_members(ctx, q).await?,
        "/team_create_form.php" => team_create_form(ctx).await?,
        "/team_create_action.php" => team_create_action(ctx, q).await?,
        "/team_join.php" => team_join(ctx, q, true).await?,
        "/team_join_form.php" => team_join_form(ctx, q).await?,
        "/team_join_action.php" => team_join(ctx, q, false).await?,
        "/team_quit_form.php" => team_quit_form(ctx).await?,
        "/team_quit_action.php" => team_quit_action(ctx, q).await?,
        "/team_edit_form.php" => team_edit_form(ctx, q).await?,
        "/team_edit_action.php" => team_edit_action(ctx, q).await?,
        "/team_manage.php" => team_manage(ctx, q).await?,
        "/team_remove_inactive_form.php" => member_form(ctx, q, MemberForm::Remove).await?,
        "/team_remove_inactive_action.php" => team_remove_action(ctx, q).await?,
        "/team_change_founder_form.php" => member_form(ctx, q, MemberForm::Founder).await?,
        "/team_change_founder_action.php" => team_change_founder_action(ctx, q).await?,
        "/team_founder_transfer_form.php" => team_founder_transfer_form(ctx).await?,
        "/team_founder_transfer_action.php" => team_founder_transfer_action(ctx, q).await?,
        "/team_lookup.php" => team_lookup(ctx, q).await?,
        "/top_teams.php" => top_teams(ctx, q).await?,
        _ => return Ok(None),
    }))
}

fn wants_xml(q: &Params) -> bool {
    !str_param(q, "team_id").is_empty()
        || !str_param(q, "team_ids").is_empty()
        || str_param(q, "format") == "xml"
}

fn page(ctx: &SiteCtx<'_>, title: &str, body: &str) -> SitePage {
    SitePage::html(200, layout(ctx.project, title, body))
}

fn team_link(t: &Team) -> String {
    format!(
        "<a href=\"team_display.php?teamid={}\">{}</a>",
        t.id,
        html_escape(&t.name)
    )
}

fn user_link(id: i64, name: &str) -> String {
    format!(
        "<a href=\"show_user.php?userid={id}\">{}</a>",
        if name.trim().is_empty() {
            "Anonymous".to_string()
        } else {
            html_escape(name)
        }
    )
}

fn go_to_team(ctx: &SiteCtx<'_>, id: i64) -> SitePage {
    SitePage::redirect(format!("{}team_display.php?teamid={id}", ctx.base))
}

/// Credit to the whole unit, and to hundredths while it is small.
fn credit(v: f64) -> String {
    number_format(v, if v.abs() < 100.0 { 2 } else { 0 })
}

fn type_select(selected: i64, none_label: &str) -> String {
    let mut out = String::from("<select id=\"type\" name=\"type\">\n");
    for (i, name) in TEAM_TYPES.iter().enumerate() {
        let label = if i == 0 { none_label } else { name };
        out.push_str(&format!(
            "<option value=\"{i}\"{}>{}</option>\n",
            if i as i64 == selected {
                " selected"
            } else {
                ""
            },
            html_escape(label)
        ));
    }
    out.push_str("</select>");
    out
}

fn country_select(selected: &str, blank: bool) -> String {
    let mut out = String::from("<select id=\"country\" name=\"country\">\n");
    if blank {
        out.push_str("<option value=\"\">---</option>\n");
    }
    for c in COUNTRIES {
        out.push_str(&format!(
            "<option value=\"{0}\"{1}>{0}</option>\n",
            html_escape(c),
            if *c == selected { " selected" } else { "" }
        ));
    }
    out.push_str("</select>");
    out
}

/// The form of `team_edit_form` in `html/inc/team.inc`, for a new team or an
/// existing one. Plain text only: there is no HTML version of the name.
fn team_form(user: &User, now: i64, team: Option<&Team>, action: &str, label: &str) -> String {
    let mut b = format!(
        "<form method=\"post\" action=\"{action}\">\n{}",
        token_fields(user, now)
    );
    if let Some(t) = team {
        b.push_str(&format!(
            "<input type=\"hidden\" name=\"teamid\" value=\"{}\">\n",
            t.id
        ));
    }
    b.push_str(
        "<p class=\"muted\">Privacy note: if you create a team, your name, credit and country \
         are shown on the team's member list.</p>\n<table>\n",
    );
    let (name, url, description, team_type, country, joinable) = match team {
        Some(t) => (
            t.name.as_str(),
            t.url.as_str(),
            t.description.as_str(),
            t.team_type,
            t.country.as_str(),
            t.joinable != 0,
        ),
        None => ("", "", "", 0, "International", true),
    };
    label_row(
        &mut b,
        "name",
        "Team name",
        &format!(
            "<input type=\"text\" id=\"name\" name=\"name\" maxlength=\"{MAX_NAME}\" value=\"{}\">",
            html_escape(name)
        ),
    );
    label_row(
        &mut b,
        "url",
        "Web address of the team, if any",
        &format!(
            "<input type=\"text\" id=\"url\" name=\"url\" maxlength=\"{MAX_URL}\" value=\"{}\">\
             <small>http:// or https://, or just the host name.</small>",
            html_escape(url)
        ),
    );
    label_row(
        &mut b,
        "description",
        "Description",
        &format!(
            "<textarea id=\"description\" name=\"description\" rows=\"8\">{}</textarea>\
             <small>Plain text. Tags are shown as typed.</small>",
            html_escape(description)
        ),
    );
    label_row(
        &mut b,
        "type",
        "Type of team",
        &type_select(team_type, "---"),
    );
    label_row(
        &mut b,
        "country",
        "Country",
        &country_select(country, false),
    );
    label_row(
        &mut b,
        "joinable",
        "Accept new members?",
        &format!(
            "<input type=\"checkbox\" id=\"joinable\" name=\"joinable\"{}>",
            if joinable { " checked" } else { "" }
        ),
    );
    b.push_str(&format!(
        "</table>\n<p><input type=\"submit\" value=\"{}\"></p>\n</form>\n",
        html_escape(label)
    ));
    b
}

fn search_form(s: &Search) -> String {
    format!(
        "<form method=\"get\" action=\"team_search.php\">\n\
         <p><label for=\"keywords\">Key words</label>\n\
         <input type=\"text\" id=\"keywords\" name=\"keywords\" maxlength=\"{MAX_NAME}\" value=\"{}\">\n\
         <small>Find teams with these words in their names or descriptions.</small></p>\n\
         <p><label for=\"country\">Country</label>{}</p>\n\
         <p><label for=\"type\">Type of team</label>{}</p>\n\
         <p><label><input type=\"checkbox\" name=\"active\"{}> Show only active teams</label></p>\n\
         <p><input type=\"submit\" name=\"submit\" value=\"Search\"></p>\n</form>\n",
        html_escape(&s.keywords),
        country_select(&s.country, true),
        type_select(s.team_type, "---"),
        if s.active { " checked" } else { "" }
    )
}

// ---------------------------------------------------------------------------
// The team list.

/// `team.php`.
pub fn team_home(ctx: &SiteCtx<'_>) -> SitePage {
    let project = ctx.project;
    let mut types: Vec<(String, String)> = vec![("All teams".into(), "top_teams.php".into())];
    for (i, name) in TEAM_TYPES.iter().enumerate().skip(1) {
        types.push((format!("{name} teams"), format!("top_teams.php?type={i}")));
    }
    let types: Vec<(&str, String)> = types.iter().map(|(a, b)| (a.as_str(), b.clone())).collect();
    let mut b = format!(
        "<p>People who run {} may form teams. You may belong to only one team, and may join or \
         quit at any time.</p>\n\
         <p>Each team has a founder, who may:</p>\n\
         <ul>\n<li>edit the team's name, description and other details</li>\n\
         <li>remove members from the team</li>\n\
         <li>hand foundership to another member</li>\n\
         <li>remove the team, if it has no members</li>\n</ul>\n\
         <p>To join a team, open its page and choose Join this team.</p>\n",
        html_escape(&project.config.long_name)
    );
    b.push_str(&site_ui::card(
        "Find a team",
        &search_form(&Search::default()),
    ));
    b.push_str(&site_ui::card("Top teams", &site_ui::link_list(&types)));
    b.push_str(&site_ui::card(
        "Create a new team",
        &format!(
            "<p>If you cannot find a team that is right for you, you can create one.</p>\n{}",
            site_ui::actions(&[site_ui::button_link(
                "Create a team",
                "team_create_form.php",
                false
            )])
        ),
    ));
    page(ctx, "Teams", &b)
}

fn search_params(q: &Params) -> Search {
    Search {
        keywords: str_param(q, "keywords").chars().take(MAX_NAME).collect(),
        country: str_param(q, "country").to_string(),
        team_type: int_param(q, "type").unwrap_or(0).clamp(0, 12),
        active: !str_param(q, "active").is_empty(),
    }
}

fn short(text: &str, n: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > n {
        let cut: String = flat.chars().take(n).collect();
        format!("{}...", cut.trim_end())
    } else {
        flat
    }
}

/// `team_search.php`: the form, or with `submit` or `xml` the matching teams as
/// a page or as XML.
pub async fn team_search(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let xml = !str_param(q, "xml").is_empty();
    if str_param(q, "submit").is_empty() && !xml {
        let b = format!(
            "<p>You can team up with other people with similar interests, or from the same \
             country, company or school. Use this form to find teams that might be right \
             for you.</p>\n{}",
            search_form(&Search::default())
        );
        return Ok(page(ctx, "Find a team", &b));
    }
    let s = search_params(q);
    let teams = search_teams(ctx.db, &s, ctx.now).await?;
    if xml {
        let mut out = format!("{XML_HEADER_UTF8}<teams>\n");
        for t in &teams {
            out.push_str(&team_xml(t));
        }
        out.push_str("</teams>\n");
        return Ok(SitePage::xml(out));
    }
    let mut b = String::new();
    if teams.is_empty() {
        b.push_str(&site_ui::empty(
            "No teams were found matching your criteria. Try another search.",
        ));
        b.push_str("<p><a href=\"team_create_form.php\">Create a new team</a>.</p>\n");
    } else {
        b.push_str(
            "<p>The following teams match one or more of your search criteria. To join one, open \
             its page and choose Join this team.</p>\n",
        );
        let cols = [
            StackCol::who("Team"),
            StackCol::text("Description"),
            StackCol::num("Recent average credit"),
            StackCol::text("Type"),
            StackCol::text("Country"),
        ];
        let now = ctx.now as f64;
        let rows: Vec<Vec<String>> = teams
            .iter()
            .map(|t| {
                vec![
                    team_link(t),
                    html_escape(&short(&t.description, 120)),
                    number_format(decayed(t.expavg_credit, t.expavg_time, now), 2),
                    html_escape(team_type_name(t.team_type)),
                    html_escape(&t.country),
                ]
            })
            .collect();
        b.push_str(&stack_table(&cols, &rows));
    }
    b.push_str("<h2>Change your search</h2>\n");
    b.push_str(&search_form(&s));
    Ok(page(ctx, "Team search results", &b))
}

/// `team_lookup.php`: by `team_id` (one team), `team_ids` (up to 100, comma
/// separated) or `team_name` (a substring of the name, best recent average
/// first, at most 100), as XML when asked with `format=xml` and always for the
/// IDs; a page of links for a name otherwise. Elements and the errors are
/// upstream's (`show_team_xml` in `html/inc/xml.inc`, `xml_error`).
pub async fn team_lookup(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let xml_error = |num: i32, msg: Option<&str>| SitePage::xml(write_error(num, msg));
    let team_id = str_param(q, "team_id");
    let team_ids = str_param(q, "team_ids");
    if !team_id.is_empty() {
        let Ok(id) = team_id.parse::<i64>() else {
            return Ok(xml_error(-1, Some("missing or bad parameter: team_id")));
        };
        if id != 0 {
            return Ok(match team_by_id(ctx.db, id).await? {
                Some(t) => SitePage::xml(format!("{XML_HEADER_UTF8}{}", team_xml(&t))),
                None => xml_error(err::ERR_DB_NOT_FOUND, None),
            });
        }
    }
    if !team_ids.is_empty() {
        // `is_numeric` ids, in the order given; the first 100.
        let ids: Vec<i64> = team_ids
            .split(',')
            .filter_map(|s| s.trim().parse::<i64>().ok())
            .take(MAX_SEARCH_RESULTS)
            .collect();
        let mut found = std::collections::HashMap::new();
        if !ids.is_empty() {
            let marks = vec!["?"; ids.len()].join(",");
            let mut st = cb_db::Stmt::new(format!(
                "SELECT {LIST_COLUMNS} FROM team WHERE id IN ({marks})"
            ));
            for id in &ids {
                st = st.bind(*id);
            }
            for t in ctx.db.run(st).await?.decode::<Team>()? {
                found.insert(t.id, t);
            }
        }
        let mut out = format!("{XML_HEADER_UTF8}<teams>\n");
        for id in &ids {
            if let Some(t) = found.get(id) {
                out.push_str(&team_xml(t));
            }
        }
        out.push_str("</teams>\n");
        return Ok(SitePage::xml(out));
    }
    let xml = str_param(q, "format") == "xml";
    let Some(name) = q.get("team_name") else {
        return Ok(if xml {
            xml_error(-1, Some("missing or bad parameter: team_name"))
        } else {
            error_page(ctx.project, 400, "missing or bad parameter: team_name")
        });
    };
    let pattern = like_contains(&name.trim().to_lowercase());
    let teams: Vec<Team> = ctx
        .db
        .run(stmt!(
            format!(
                "SELECT {LIST_COLUMNS} FROM team WHERE name_lc LIKE ? ESCAPE '\\' \
                 ORDER BY recent_key DESC, id LIMIT {MAX_SEARCH_RESULTS}"
            ),
            pattern
        ))
        .await?
        .decode()?;
    if xml {
        let mut out = format!("{XML_HEADER_UTF8}<teams>\n");
        for t in &teams {
            out.push_str(&team_xml(t));
        }
        out.push_str("</teams>\n");
        return Ok(SitePage::xml(out));
    }
    let mut b = String::new();
    if teams.is_empty() {
        b.push_str(&site_ui::empty("No team has that name."));
    } else {
        b.push_str(&format!(
            "<h2>Search results for '{}'</h2>\n<p>You may view these teams' members, statistics \
             and information.</p>\n<ul>\n",
            html_escape(name.trim())
        ));
        for t in &teams {
            b.push_str(&format!("<li>{}</li>\n", team_link(t)));
        }
        b.push_str("</ul>\n");
        if teams.len() == MAX_SEARCH_RESULTS {
            b.push_str("<p>More than 100 teams match your search. The first 100 are shown.</p>\n");
        }
    }
    b.push_str(
        "<p>If you cannot find the team you are looking for, you may \
         <a href=\"team_create_form.php\">create a team</a> yourself.</p>\n",
    );
    Ok(page(ctx, "Search results", &b))
}

// ---------------------------------------------------------------------------
// The leaderboard.

#[derive(Clone, Copy, PartialEq, Eq)]
enum TopSort {
    Recent,
    Total,
}

impl TopSort {
    fn parse(s: &str) -> TopSort {
        if s == "total_credit" {
            TopSort::Total
        } else {
            TopSort::Recent
        }
    }
    fn key(self) -> &'static str {
        match self {
            TopSort::Recent => "expavg_credit",
            TopSort::Total => "total_credit",
        }
    }
    /// The indexed column. "Recent average" is ordered by `recent_key`, as
    /// the account leaderboard does (see `cloud/docs/architecture.md`).
    fn column(self) -> &'static str {
        match self {
            TopSort::Recent => "recent_key",
            TopSort::Total => "total_credit",
        }
    }
}

/// Stock's `offset`: a multiple of [`PAGE_SIZE`], else 0; clamped.
fn top_offset(q: &Params) -> usize {
    let n = int_param(q, "offset").unwrap_or(0).max(0) as usize;
    if !n.is_multiple_of(PAGE_SIZE) || n >= MAX_TOP_OFFSET {
        0
    } else {
        n
    }
}

fn top_type(q: &Params) -> i64 {
    let t = int_param(q, "type").unwrap_or(0);
    if (0..TEAM_TYPES.len() as i64).contains(&t) {
        t
    } else {
        0
    }
}

/// The edge cache key of `top_teams.php`: its path and the parameters it
/// reads, in canonical form (see `site_boards::public_cache_key`).
pub fn top_teams_cache_key(q: &Params) -> String {
    format!(
        "/top_teams.php?sort_by={}&type={}&offset={}",
        TopSort::parse(str_param(q, "sort_by")).key(),
        top_type(q),
        top_offset(q)
    )
}

/// `top_teams.php`: teams by recent average credit (default) or total credit,
/// optionally of one type, [`PAGE_SIZE`] to a page. One statement that reads
/// the first rows of one index (`team_recent_key` or `team_total_credit`, in
/// `migrations/0009_teams.sql`); with a type it reads the index in order and
/// stops after the page.
pub async fn top_teams(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let sort = TopSort::parse(str_param(q, "sort_by"));
    let team_type = top_type(q);
    let offset = top_offset(q);
    let (cond, mut params): (&str, Vec<cb_db::Value>) = if team_type > 0 {
        ("WHERE type = ?", vec![team_type.into()])
    } else {
        ("", vec![])
    };
    params.push(((PAGE_SIZE + 1) as i64).into());
    params.push((offset as i64).into());
    let rows: Vec<Team> = ctx
        .db
        .run(cb_db::Stmt {
            sql: format!(
                "SELECT {LIST_COLUMNS} FROM team {cond} ORDER BY {} DESC, id LIMIT ? OFFSET ?",
                sort.column()
            ),
            params,
        })
        .await?
        .decode()?;
    let has_next = rows.len() > PAGE_SIZE && offset + PAGE_SIZE < MAX_TOP_OFFSET;
    let mut rows = rows;
    rows.truncate(PAGE_SIZE);
    let type_url = if team_type > 0 {
        format!("&amp;type={team_type}")
    } else {
        String::new()
    };
    let title = if team_type > 0 {
        format!(
            "Top {} teams",
            TEAM_TYPES[team_type as usize].to_lowercase()
        )
    } else {
        "Top teams".to_string()
    };
    let mut b = format!(
        "<p>Sort by: {} &middot; {}</p>\n",
        sort_link(sort, TopSort::Recent, "Recent average credit", &type_url),
        sort_link(sort, TopSort::Total, "Total credit", &type_url)
    );
    if rows.is_empty() {
        b.push_str(&site_ui::empty(if offset > 0 {
            "There is nothing on this page."
        } else {
            "There are no teams yet."
        }));
    } else {
        let now = ctx.now as f64;
        let cols = [
            StackCol::num("Rank"),
            StackCol::who("Team"),
            StackCol::num("Members"),
            StackCol::num("Recent average credit"),
            StackCol::num("Total credit"),
            StackCol::text("Country"),
            StackCol::text("Type"),
        ];
        let table: Vec<Vec<String>> = rows
            .iter()
            .enumerate()
            .map(|(i, t)| {
                vec![
                    (offset + i + 1).to_string(),
                    team_link(t),
                    number_format(t.nusers as f64, 0),
                    number_format(decayed(t.expavg_credit, t.expavg_time, now), 2),
                    credit(t.total_credit),
                    html_escape(&t.country),
                    html_escape(team_type_name(t.team_type)),
                ]
            })
            .collect();
        b.push_str(&stack_table(&cols, &table));
    }
    if offset > 0 || has_next {
        b.push_str("<p>");
        if offset > 0 {
            b.push_str(&format!(
                "<a href=\"top_teams.php?sort_by={}&amp;offset={}{type_url}\">Previous {PAGE_SIZE}</a> ",
                sort.key(),
                offset - PAGE_SIZE
            ));
        }
        if has_next {
            b.push_str(&format!(
                "<a href=\"top_teams.php?sort_by={}&amp;offset={}{type_url}\">Next {PAGE_SIZE}</a>",
                sort.key(),
                offset + PAGE_SIZE
            ));
        }
        b.push_str("</p>\n");
    }
    b.push_str(
        "<p><small>Recent average credit is credit per day, and halves every week a team earns \
         nothing. A team's credit is what its members earned while they were members.</small></p>\n",
    );
    Ok(page(ctx, &title, &b))
}

fn sort_link(current: TopSort, this: TopSort, label: &str, type_url: &str) -> String {
    if current == this {
        format!("<strong aria-current=\"true\">{label}</strong>")
    } else {
        format!(
            "<a href=\"top_teams.php?sort_by={}{type_url}\">{label}</a>",
            this.key()
        )
    }
}

// ---------------------------------------------------------------------------
// One team.

/// A team that was asked for by `teamid`, or the page that says there is none.
async fn team_from_query(
    ctx: &SiteCtx<'_>,
    q: &Params,
    name: &str,
) -> CoreResult<Result<Team, SitePage>> {
    let Some(id) = int_param(q, name) else {
        return Ok(Err(error_page(
            ctx.project,
            400,
            "missing or bad parameter: teamid",
        )));
    };
    Ok(team_by_id(ctx.db, id)
        .await?
        .ok_or_else(|| error_page(ctx.project, 404, "No such team.")))
}

fn login_link(path_and_query: &str) -> String {
    let next: String = path_and_query
        .bytes()
        .map(|b| match b {
            b'?' => "%3F".to_string(),
            b'&' => "%26".to_string(),
            b'=' => "%3D".to_string(),
            _ => (b as char).to_string(),
        })
        .collect();
    format!("login_form.php?next_url={next}")
}

/// `team_display.php`: a team, with what the visitor may do with it. Two
/// statements: the visitor, then the team with its founder's name.
pub async fn team_display(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let viewer = viewer(ctx).await?;
    let team = match team_from_query(ctx, q, "teamid").await? {
        Ok(t) => t,
        Err(page) => return Ok(page),
    };
    let now = ctx.now as f64;
    let mut info: Vec<(&str, String)> = Vec::new();
    if !team.description.is_empty() {
        info.push(("Description", description_html(&team.description)));
    }
    info.push(("Created", date_str(team.create_time)));
    if !team.url.is_empty() {
        let href = html_escape(&team_href(&team.url));
        info.push((
            "Web site",
            format!(
                "<a href=\"{href}\" rel=\"nofollow ugc noopener noreferrer\">{}</a>",
                html_escape(&team.url)
            ),
        ));
    }
    info.push(("Country", html_escape(&team.country)));
    info.push(("Type", html_escape(team_type_name(team.team_type))));
    let founder = match (&team.founder_name, team.userid) {
        (Some(name), id) if id > 0 => user_link(id, name),
        _ => "---".to_string(),
    };
    info.push(("Founder", founder));
    info.push((
        "Members",
        format!(
            "{} (<a href=\"team_members.php?teamid={}&amp;offset=0&amp;sort_by=expavg_credit\">view</a>)",
            number_format(team.nusers as f64, 0),
            team.id
        ),
    ));
    let numbers = site_ui::stats(&[
        site_ui::stat("Total credit", &number_format(team.total_credit, 0), ""),
        site_ui::stat(
            "Recent average credit",
            &number_format(decayed(team.expavg_credit, team.expavg_time, now), 2),
            "",
        ),
        site_ui::stat("Members", &number_format(team.nusers as f64, 0), ""),
    ]);
    let mut b = numbers;
    b.push_str(&site_ui::card_accent("Team info", &site_ui::kv(&info)));

    let mut actions = String::new();
    match &viewer {
        None => {
            actions.push_str(&format!(
                "<p><a href=\"{}\">Log in</a> to join this team.</p>\n",
                login_link(&format!("team_display.php?teamid={}", team.id))
            ));
        }
        Some(user) => {
            let member = user.teamid == team.id;
            if !member {
                if team.joinable != 0 {
                    actions.push_str(&format!(
                        "<form method=\"post\" action=\"team_join.php\">\n{}\
                         <input type=\"hidden\" name=\"teamid\" value=\"{}\">\n\
                         <p><input type=\"submit\" value=\"Join this team\"></p>\n</form>\n",
                        token_fields(user, ctx.now),
                        team.id
                    ));
                } else {
                    actions.push_str(&site_ui::notice("Not accepting new members."));
                }
            }
            let mut links = Vec::new();
            if member {
                links.push(site_ui::button_link(
                    "Quit this team",
                    "team_quit_form.php",
                    true,
                ));
            }
            if team.userid == user.id {
                links.push(site_ui::button_link(
                    "Administer this team",
                    &format!("team_manage.php?teamid={}", team.id),
                    true,
                ));
            } else if member && team.userid == 0 {
                links.push(site_ui::button_link(
                    "Become the founder",
                    "team_founder_transfer_form.php",
                    true,
                ));
            }
            if !links.is_empty() {
                actions.push_str(&site_ui::actions(&links));
            }
        }
    }
    if !actions.is_empty() {
        b.push_str(&site_ui::card("Your membership", &actions));
    }
    Ok(page(ctx, &team.name, &b))
}

/// `team_members.php`: the members of a team, 20 to a page, by recent average
/// (default) or total credit; the first 1000 only, as upstream. Accounts that
/// are blocked are left out, as on the account leaderboard. Two statements.
pub async fn team_members(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let team = match team_from_query(ctx, q, "teamid").await? {
        Ok(t) => t,
        Err(page) => return Ok(page),
    };
    let sort = TopSort::parse(str_param(q, "sort_by"));
    let offset = int_param(q, "offset").unwrap_or(0).max(0) as usize;
    if offset > MAX_MEMBER_OFFSET {
        return Ok(error_page(
            ctx.project,
            400,
            "Limit exceeded: can only display the first 1000 members.",
        ));
    }
    let rows: Vec<User> = ctx
        .db
        .run(stmt!(
            format!(
                "SELECT id, name, country, total_credit, expavg_credit, expavg_time FROM user \
                 WHERE teamid = ? AND teamid > 0 AND blocked = 0 ORDER BY {} DESC, id LIMIT ? OFFSET ?",
                match sort {
                    TopSort::Total => "total_credit",
                    TopSort::Recent => "recent_key",
                }
            ),
            team.id,
            (PAGE_SIZE + 1) as i64,
            offset as i64
        ))
        .await?
        .decode()?;
    let has_next = rows.len() > PAGE_SIZE && offset + PAGE_SIZE <= MAX_MEMBER_OFFSET;
    let mut rows = rows;
    rows.truncate(PAGE_SIZE);
    let now = ctx.now as f64;
    let show_country = ctx.project.config.user_country;
    let mut cols = vec![
        StackCol::num("Rank"),
        StackCol::who("Name"),
        StackCol::num("Total credit"),
        StackCol::num("Recent average credit"),
    ];
    if show_country {
        cols.push(StackCol::text("Country"));
    }
    let table: Vec<Vec<String>> = rows
        .iter()
        .enumerate()
        .map(|(i, u)| {
            let mut name = user_link(u.id, &u.name);
            if u.id == team.userid {
                name.push_str(" [Founder]");
            }
            let mut r = vec![
                (offset + i + 1).to_string(),
                name,
                credit(u.total_credit),
                number_format(decayed(u.expavg_credit, u.expavg_time, now), 2),
            ];
            if show_country {
                r.push(html_escape(&u.country));
            }
            r
        })
        .collect();
    let mut b = format!(
        "<p><a href=\"team_display.php?teamid={}\">Back to the team</a></p>\n<p>Sort by: {} &middot; {}</p>\n",
        team.id,
        member_sort_link(&team, sort, TopSort::Recent, "Recent average credit"),
        member_sort_link(&team, sort, TopSort::Total, "Total credit"),
    );
    if rows.is_empty() {
        b.push_str(&site_ui::empty(if offset > 0 {
            "There is nothing on this page."
        } else {
            "This team has no members."
        }));
    } else {
        b.push_str(&stack_table(&cols, &table));
    }
    if offset > 0 || has_next {
        b.push_str("<p>");
        if offset > 0 {
            b.push_str(&format!(
                "<a href=\"team_members.php?teamid={}&amp;sort_by={}&amp;offset={}\">Previous {PAGE_SIZE}</a> ",
                team.id,
                sort.key(),
                offset.saturating_sub(PAGE_SIZE)
            ));
        }
        if has_next {
            b.push_str(&format!(
                "<a href=\"team_members.php?teamid={}&amp;sort_by={}&amp;offset={}\">Next {PAGE_SIZE}</a>",
                team.id,
                sort.key(),
                offset + PAGE_SIZE
            ));
        }
        b.push_str("</p>\n");
    }
    Ok(page(ctx, &format!("Members of {}", team.name), &b))
}

fn member_sort_link(team: &Team, current: TopSort, this: TopSort, label: &str) -> String {
    if current == this {
        format!("<strong aria-current=\"true\">{label}</strong>")
    } else {
        format!(
            "<a href=\"team_members.php?teamid={}&amp;sort_by={}\">{label}</a>",
            team.id,
            this.key()
        )
    }
}

// ---------------------------------------------------------------------------
// Create, join, quit, edit.

/// Why the account may not create a team now, from the settings and the
/// account: `disable_team_creation` and `team_create_need_credit`, and being
/// in a team already (`team_create_form.php` asks to quit first; here the
/// action refuses too).
fn creation_refusal(ctx: &SiteCtx<'_>, user: &User) -> Option<&'static str> {
    let c = &ctx.project.config;
    if c.disable_team_creation {
        Some("Team creation is disabled.")
    } else if c.team_create_need_credit && user.total_credit == 0.0 {
        Some("You must complete a task to create a team.")
    } else if user.teamid != 0 {
        Some("You belong to a team. You must quit it before creating a new one.")
    } else {
        None
    }
}

/// `team_create_form.php`.
pub async fn team_create_form(ctx: &SiteCtx<'_>) -> CoreResult<SitePage> {
    let Some(user) = viewer(ctx).await? else {
        return Ok(to_login(ctx.base, "team_create_form.php"));
    };
    if let Some(why) = creation_refusal(ctx, &user) {
        let mut b = site_ui::notice(why);
        if user.teamid != 0 {
            b.push_str(&format!(
                "<p><a href=\"team_display.php?teamid={}\">Your team</a> &middot; \
                 <a href=\"team_quit_form.php\">Quit this team</a></p>\n",
                user.teamid
            ));
        }
        return Ok(page(ctx, "Create a team", &b));
    }
    Ok(page(
        ctx,
        "Create a team",
        &team_form(
            &user,
            ctx.now,
            None,
            "team_create_action.php",
            "Create a team",
        ),
    ))
}

fn form_get<'a>(q: &'a Params) -> impl Fn(&str) -> String + 'a {
    move |k: &str| q.get(k).cloned().unwrap_or_default()
}

fn need_post(ctx: &SiteCtx<'_>) -> Option<SitePage> {
    (!ctx.post).then(|| error_page(ctx.project, 405, "This page takes a POST."))
}

/// `team_create_action.php`: makes the team and puts the account in it. One
/// batch of two statements after the account is read; a name another team has
/// (without regard to case) is refused.
pub async fn team_create_action(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    if let Some(p) = need_post(ctx) {
        return Ok(p);
    }
    let Some(user) = viewer(ctx).await? else {
        return Ok(to_login(ctx.base, "team_create_form.php"));
    };
    if !token_ok(ctx, &user, q) {
        return Ok(error_page(project, 403, TIMED_OUT));
    }
    if let Some(why) = creation_refusal(ctx, &user) {
        return Ok(error_page(project, 403, why));
    }
    let input = match check_team_input(&form_get(q)) {
        Ok(i) => i,
        Err(why) => return Ok(error_page(project, 400, why)),
    };
    let out = ctx
        .db
        .batch(create_statements(
            user.id,
            &user.authenticator,
            &input,
            ctx.now,
        ))
        .await?;
    match out.first().and_then(|r| r.scalar_i64()) {
        Some(id) => Ok(go_to_team(ctx, id)),
        None => Ok(error_page(
            project,
            409,
            &format!(
                "A team named {} already exists, or your account changed meanwhile. Try another name.",
                input.name
            ),
        )),
    }
}

/// The notes of `team_join_form.php`, with the form.
pub async fn team_join_form(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let Some(user) = viewer(ctx).await? else {
        return Ok(to_login(ctx.base, "team_join_form.php"));
    };
    let id = if q.contains_key("id") { "id" } else { "teamid" };
    let team = match team_from_query(ctx, q, id).await? {
        Ok(t) => t,
        Err(page) => return Ok(page),
    };
    if team.joinable == 0 {
        return Ok(error_page(
            ctx.project,
            403,
            &format!("The team {} is not joinable.", team.name),
        ));
    }
    let b = format!(
        "<p>Please note:</p>\n<ul>\n\
         <li>Joining a team does not change your account's credit. Credit you earn from now on \
         also counts toward the team.</li>\n\
         <li>The team's page lists you among its members with your name, credit and country.</li>\n\
         <li>You may belong to only one team: joining this one takes you out of your current team, \
         if you have one.</li>\n</ul>\n\
         <form method=\"post\" action=\"team_join_action.php\">\n{}\
         <input type=\"hidden\" name=\"teamid\" value=\"{}\">\n\
         <p><input type=\"submit\" value=\"Join team\"></p>\n</form>\n",
        token_fields(&user, ctx.now),
        team.id
    );
    Ok(page(ctx, &format!("Join {}", team.name), &b))
}

/// `team_join_action.php`, and `team_join.php` (upstream's link with the tokens
/// in the query; here a POST, and the page goes on to the account page when
/// `redirect_home` is set, as `team_join.php` does). A GET of `team_join.php`
/// shows the form instead. Statements: the account, the team, the update.
pub async fn team_join(ctx: &SiteCtx<'_>, q: &Params, redirect_home: bool) -> CoreResult<SitePage> {
    let project = ctx.project;
    if !ctx.post {
        if redirect_home {
            if let Some(id) = int_param(q, "teamid") {
                return Ok(SitePage::redirect(format!(
                    "{}team_join_form.php?id={id}",
                    ctx.base
                )));
            }
        }
        return Ok(
            need_post(ctx).unwrap_or_else(|| error_page(project, 405, "This page takes a POST."))
        );
    }
    let Some(user) = viewer(ctx).await? else {
        return Ok(to_login(ctx.base, "team_search.php"));
    };
    if !token_ok(ctx, &user, q) {
        return Ok(error_page(project, 403, TIMED_OUT));
    }
    let team = match team_from_query(ctx, q, "teamid").await? {
        Ok(t) => t,
        Err(page) => return Ok(page),
    };
    if team.joinable == 0 {
        return Ok(error_page(
            project,
            403,
            &format!("The team {} is not joinable.", team.name),
        ));
    }
    if user.teamid == team.id {
        return Ok(page(
            ctx,
            "Already a member",
            &format!("<p>You are already a member of {}.</p>\n", team_link(&team)),
        ));
    }
    let done = ctx
        .db
        .run(join_statement(user.id, &user.authenticator, team.id))
        .await?;
    if done.rows.is_empty() {
        return Ok(error_page(
            project,
            409,
            "Could not join the team: it stopped taking members, or your account changed meanwhile.",
        ));
    }
    if redirect_home {
        return Ok(SitePage::redirect(format!("{}home.php", ctx.base)));
    }
    Ok(page(
        ctx,
        &format!("Joined {}", team.name),
        &format!("<p>You have joined {}.</p>\n", team_link(&team)),
    ))
}

/// `team_quit_form.php`.
pub async fn team_quit_form(ctx: &SiteCtx<'_>) -> CoreResult<SitePage> {
    let Some(user) = viewer(ctx).await? else {
        return Ok(to_login(ctx.base, "team_quit_form.php"));
    };
    if user.teamid == 0 {
        return Ok(error_page(ctx.project, 404, "You are not in a team."));
    }
    let team = team_by_id(ctx.db, user.teamid).await?;
    let name = team
        .as_ref()
        .map_or("your team".to_string(), |t| t.name.clone());
    let b = format!(
        "<p>Please note before quitting a team:</p>\n<ul>\n\
         <li>If you quit a team, you may rejoin later, or join any other team you like.</li>\n\
         <li>Quitting a team does not affect your personal credit statistics.</li>\n</ul>\n\
         <form method=\"post\" action=\"team_quit_action.php\">\n{}\
         <input type=\"hidden\" name=\"id\" value=\"{}\">\n\
         <p><input type=\"submit\" class=\"danger\" value=\"Quit team\"></p>\n</form>\n",
        token_fields(&user, ctx.now),
        user.teamid
    );
    Ok(page(ctx, &format!("Quit {name}"), &b))
}

/// `team_quit_action.php`. A team that no longer exists can be quit too, so an
/// account that carries the ID of a team this server does not have (one
/// moved from a stock server) can clear it.
pub async fn team_quit_action(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    if let Some(p) = need_post(ctx) {
        return Ok(p);
    }
    let Some(user) = viewer(ctx).await? else {
        return Ok(to_login(ctx.base, "team_quit_form.php"));
    };
    if !token_ok(ctx, &user, q) {
        return Ok(error_page(ctx.project, 403, TIMED_OUT));
    }
    let id = int_param(q, "id").unwrap_or(-1);
    if user.teamid == 0 || user.teamid != id {
        return Ok(error_page(
            ctx.project,
            409,
            "Unable to quit team. Team doesn't exist, or you don't belong to it.",
        ));
    }
    let done = ctx
        .db
        .run(quit_statement(user.id, &user.authenticator, id))
        .await?;
    if done.rows.is_empty() {
        return Ok(error_page(
            ctx.project,
            409,
            "The account changed meanwhile; please try again.",
        ));
    }
    Ok(SitePage::redirect(format!("{}home.php", ctx.base)))
}

/// A team and the account when the account is its founder, or the page that
/// says why not: not logged in, no such team, not the founder
/// (`require_founder_login`).
async fn founder_of(
    ctx: &SiteCtx<'_>,
    q: &Params,
    form_path: &str,
) -> CoreResult<Result<(User, Team), SitePage>> {
    let Some(user) = viewer(ctx).await? else {
        return Ok(Err(to_login(ctx.base, form_path)));
    };
    let team = match team_from_query(ctx, q, "teamid").await? {
        Ok(t) => t,
        Err(page) => return Ok(Err(page)),
    };
    if team.userid != user.id {
        return Ok(Err(error_page(
            ctx.project,
            403,
            "This operation requires foundership.",
        )));
    }
    Ok(Ok((user, team)))
}

/// `team_edit_form.php`. Upstream lets team admins edit too; this site has the
/// founder only.
pub async fn team_edit_form(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let (user, team) = match founder_of(ctx, q, "team_manage.php").await? {
        Ok(x) => x,
        Err(p) => return Ok(p),
    };
    Ok(page(
        ctx,
        &format!("Edit {}", team.name),
        &team_form(
            &user,
            ctx.now,
            Some(&team),
            "team_edit_action.php",
            "Update team info",
        ),
    ))
}

/// `team_edit_action.php`. The update names the founder and refuses a name
/// another team has; either shows as no row.
pub async fn team_edit_action(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    if let Some(p) = need_post(ctx) {
        return Ok(p);
    }
    let (user, team) = match founder_of(ctx, q, "team_manage.php").await? {
        Ok(x) => x,
        Err(p) => return Ok(p),
    };
    if !token_ok(ctx, &user, q) {
        return Ok(error_page(project, 403, TIMED_OUT));
    }
    let input = match check_team_input(&form_get(q)) {
        Ok(i) => i,
        Err(why) => return Ok(error_page(project, 400, why)),
    };
    let done = ctx
        .db
        .run(stmt!(
            "UPDATE OR IGNORE team SET name = ?, name_lc = ?, url = ?, description = ?, type = ?, \
             country = ?, joinable = ? WHERE id = ? AND userid = ? RETURNING id",
            input.name.as_str(),
            input.name.to_lowercase(),
            input.url.as_str(),
            input.description.as_str(),
            input.team_type,
            input.country.as_str(),
            input.joinable,
            team.id,
            user.id
        ))
        .await?;
    if done.rows.is_empty() {
        return Ok(error_page(
            project,
            409,
            &format!("The name '{}' is being used by another team.", input.name),
        ));
    }
    Ok(go_to_team(ctx, team.id))
}

// ---------------------------------------------------------------------------
// The founder's pages.

/// `team_manage.php`: the founder's menu, and with `action=delete` the removal
/// of a team that has no members: a GET asks, a POST with the form token does
/// it (upstream does it on the GET, with the tokens in the link).
pub async fn team_manage(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    let (user, team) = match founder_of(ctx, q, "team_manage.php").await? {
        Ok(x) => x,
        Err(p) => return Ok(p),
    };
    if str_param(q, "action") == "delete" {
        if team.nusers > 0 {
            return Ok(error_page(project, 409, "Can't delete non-empty team."));
        }
        if !ctx.post {
            let b = format!(
                "<p>Remove the team {}? Nothing else is changed.</p>\n\
                 <form method=\"post\" action=\"team_manage.php?teamid={}&amp;action=delete\">\n{}\
                 <p><input type=\"submit\" class=\"danger\" value=\"Remove team\"></p>\n</form>\n",
                html_escape(&team.name),
                team.id,
                token_fields(&user, ctx.now)
            );
            return Ok(page(ctx, "Remove team", &b));
        }
        if !token_ok(ctx, &user, q) {
            return Ok(error_page(project, 403, TIMED_OUT));
        }
        // Only a team nobody is in, by the member index.
        let done = ctx
            .db
            .run(stmt!(
                "DELETE FROM team WHERE id = ? AND userid = ? \
                 AND NOT EXISTS (SELECT 1 FROM user WHERE teamid = ? AND teamid > 0) RETURNING id",
                team.id,
                user.id,
                team.id
            ))
            .await?;
        if done.rows.is_empty() {
            return Ok(error_page(project, 409, "Can't delete non-empty team."));
        }
        return Ok(page(
            ctx,
            "Team removed",
            &format!(
                "<p class=\"notice\">The team {} was removed.</p>\n<p><a href=\"home.php\">Your account</a></p>\n",
                html_escape(&team.name)
            ),
        ));
    }
    let mut links = vec![
        (
            "Edit team info",
            format!("team_edit_form.php?teamid={}", team.id),
        ),
        (
            "Remove members",
            format!("team_remove_inactive_form.php?teamid={}", team.id),
        ),
        (
            "Change founder",
            format!("team_change_founder_form.php?teamid={}", team.id),
        ),
    ];
    if team.nusers == 0 {
        links.push((
            "Remove team",
            format!("team_manage.php?teamid={}&action=delete", team.id),
        ));
    }
    let b = format!(
        "<p><a href=\"team_display.php?teamid={}\">Back to the team</a></p>\n{}\
         <p class=\"muted\">Removing a team is allowed only while it has no members.</p>\n",
        team.id,
        site_ui::link_list(&links)
    );
    Ok(page(
        ctx,
        &format!("Team administration for {}", team.name),
        &b,
    ))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MemberForm {
    Remove,
    Founder,
}

/// `team_remove_inactive_form.php` and `team_change_founder_form.php`: the
/// members, [`FORM_PAGE`] to a page by name, with a checkbox or a radio button.
/// The founder is not listed, nor, in the removal, the account itself.
async fn member_form(ctx: &SiteCtx<'_>, q: &Params, kind: MemberForm) -> CoreResult<SitePage> {
    let me = match kind {
        MemberForm::Remove => "team_remove_inactive_form.php",
        MemberForm::Founder => "team_change_founder_form.php",
    };
    let (user, team) = match founder_of(ctx, q, me).await? {
        Ok(x) => x,
        Err(p) => return Ok(p),
    };
    let offset = int_param(q, "offset").unwrap_or(0).max(0) as usize;
    let rows: Vec<User> = ctx
        .db
        .run(stmt!(
            "SELECT id, name, total_credit, expavg_credit, expavg_time FROM user \
             WHERE teamid = ? AND teamid > 0 AND id != ? AND id != ? AND blocked = 0 \
             ORDER BY name, id LIMIT ? OFFSET ?",
            team.id,
            team.userid,
            user.id,
            (FORM_PAGE + 1) as i64,
            offset as i64
        ))
        .await?
        .decode()?;
    let has_next = rows.len() > FORM_PAGE;
    let rows = &rows[..rows.len().min(FORM_PAGE)];
    let now = ctx.now as f64;
    let (action, title, button, intro) = match kind {
        MemberForm::Remove => (
            "team_remove_inactive_action.php",
            format!("Remove members from {}", team.name),
            "Remove members",
            "Check the members to remove. A removed member keeps their credit and may join again.",
        ),
        MemberForm::Founder => (
            "team_change_founder_action.php",
            format!("Change founder of {}", team.name),
            "Change founder",
            "Choose the member who becomes the founder.",
        ),
    };
    if rows.is_empty() && offset == 0 {
        return Ok(page(
            ctx,
            &title,
            &site_ui::empty(match kind {
                MemberForm::Remove => "No members are eligible for removal.",
                MemberForm::Founder => "There are no members to hand the team to.",
            }),
        ));
    }
    let mut b = format!(
        "<p>{intro}</p>\n<form method=\"post\" action=\"{action}\">\n{}\
         <input type=\"hidden\" name=\"teamid\" value=\"{}\">\n\
         <input type=\"hidden\" name=\"ninactive_users\" value=\"{}\">\n<table>\n",
        token_fields(&user, ctx.now),
        team.id,
        rows.len()
    );
    b.push_str(&format!(
        "<tr><th>{}</th><th>Name</th><th class=\"num\">Total credit</th><th class=\"num\">Recent average credit</th></tr>\n",
        if kind == MemberForm::Remove { "Remove?" } else { "New founder?" }
    ));
    for (i, u) in rows.iter().enumerate() {
        let control = match kind {
            MemberForm::Remove => format!(
                "<input type=\"checkbox\" name=\"remove_{i}\" value=\"{}\" aria-label=\"Remove {}\">",
                u.id,
                html_escape(&u.name)
            ),
            MemberForm::Founder => format!(
                "<input type=\"radio\" name=\"userid\" value=\"{}\" aria-label=\"Make {} the founder\">",
                u.id,
                html_escape(&u.name)
            ),
        };
        b.push_str(&format!(
            "<tr><td>{control}</td><td>{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td></tr>\n",
            user_link(u.id, &u.name),
            credit(u.total_credit),
            number_format(decayed(u.expavg_credit, u.expavg_time, now), 2)
        ));
    }
    b.push_str(&format!(
        "</table>\n<p><input type=\"submit\" class=\"{}\" value=\"{button}\"></p>\n</form>\n",
        if kind == MemberForm::Remove {
            "danger"
        } else {
            "secondary"
        }
    ));
    if offset > 0 || has_next {
        b.push_str("<p>");
        if offset > 0 {
            b.push_str(&format!(
                "<a href=\"{me}?teamid={}&amp;offset={}\">Previous {FORM_PAGE}</a> ",
                team.id,
                offset.saturating_sub(FORM_PAGE)
            ));
        }
        if has_next {
            b.push_str(&format!(
                "<a href=\"{me}?teamid={}&amp;offset={}\">Next {FORM_PAGE}</a>",
                team.id,
                offset + FORM_PAGE
            ));
        }
        b.push_str("</p>\n");
    }
    Ok(page(ctx, &title, &b))
}

/// `team_remove_inactive_action.php`: takes the checked members out of the
/// team, in one statement that names the team and its founder and leaves the
/// founder in. At most [`REMOVE_MAX`] to a request.
pub async fn team_remove_action(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    if let Some(p) = need_post(ctx) {
        return Ok(p);
    }
    let (user, team) = match founder_of(ctx, q, "team_remove_inactive_form.php").await? {
        Ok(x) => x,
        Err(p) => return Ok(p),
    };
    if !token_ok(ctx, &user, q) {
        return Ok(error_page(project, 403, TIMED_OUT));
    }
    let n = int_param(q, "ninactive_users")
        .unwrap_or(0)
        .clamp(0, REMOVE_MAX as i64);
    let mut ids: Vec<i64> = (0..n)
        .filter_map(|i| int_param(q, &format!("remove_{i}")))
        .filter(|id| *id > 0)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Ok(page(
            ctx,
            &format!("Removing users from {}", team.name),
            &format!(
                "<p>No one was selected.</p>\n<p><a href=\"team_display.php?teamid={}\">Back to the team</a></p>\n",
                team.id
            ),
        ));
    }
    let marks = vec!["?"; ids.len()].join(",");
    let mut st = cb_db::Stmt::new(format!(
        "UPDATE user SET teamid = 0 WHERE teamid = ? AND teamid > 0 AND id != ? AND id IN ({marks}) \
         AND EXISTS (SELECT 1 FROM team WHERE id = ? AND userid = ?) RETURNING id"
    ))
    .bind(team.id)
    .bind(team.userid);
    for id in &ids {
        st = st.bind(*id);
    }
    st = st.bind(team.id).bind(user.id);
    let removed = ctx.db.run(st).await?.rows.len();
    Ok(page(
        ctx,
        &format!("Removing users from {}", team.name),
        &format!(
            "<p class=\"notice\">{removed} member{} removed.</p>\n<p><a href=\"team_display.php?teamid={}\">Back to the team</a></p>\n",
            if removed == 1 { "" } else { "s" },
            team.id
        ),
    ))
}

/// `team_change_founder_action.php`: hands the team to a member, in one
/// statement that checks both the old founder and the new one's membership.
pub async fn team_change_founder_action(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    if let Some(p) = need_post(ctx) {
        return Ok(p);
    }
    let (user, team) = match founder_of(ctx, q, "team_change_founder_form.php").await? {
        Ok(x) => x,
        Err(p) => return Ok(p),
    };
    if !token_ok(ctx, &user, q) {
        return Ok(error_page(project, 403, TIMED_OUT));
    }
    let new_id = int_param(q, "userid").unwrap_or(0);
    // The update, and the new founder's name for the page, in one round trip.
    let out = ctx
        .db
        .batch(vec![
            stmt!(
                "UPDATE team SET userid = ? WHERE id = ? AND userid = ? \
                 AND EXISTS (SELECT 1 FROM user WHERE id = ? AND teamid = ? AND blocked = 0) \
                 RETURNING id",
                new_id,
                team.id,
                user.id,
                new_id,
                team.id
            ),
            stmt!("SELECT name FROM user WHERE id = ?", new_id),
        ])
        .await?;
    if out[0].rows.is_empty() {
        return Ok(error_page(
            project,
            409,
            &format!("User is not a member of {}.", team.name),
        ));
    }
    let new_name = out[1]
        .rows
        .first()
        .and_then(|r| r.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    Ok(page(
        ctx,
        &format!("Changed founder of {}", team.name),
        &format!(
            "<p class=\"notice\">{} is now the founder of {}.</p>\n<p><a href=\"team_display.php?teamid={}\">Back to the team</a></p>\n",
            html_escape(new_name),
            html_escape(&team.name),
            team.id
        ),
    ))
}

/// `team_founder_transfer_form.php`. Upstream's request-and-wait route (a
/// member asks, the founder is told by mail and private message, and after 60
/// days the member may take the team) needs those messages, which this site
/// does not send. What is offered is the case upstream grants at once: a team
/// with no founder (the founder's account was deleted) can be taken by one of
/// its members.
pub async fn team_founder_transfer_form(ctx: &SiteCtx<'_>) -> CoreResult<SitePage> {
    let Some(user) = viewer(ctx).await? else {
        return Ok(to_login(ctx.base, "team_founder_transfer_form.php"));
    };
    let team = if user.teamid > 0 {
        team_by_id(ctx.db, user.teamid).await?
    } else {
        None
    };
    let Some(team) = team else {
        return Ok(error_page(
            ctx.project,
            403,
            "You need to be a member of a team to access this page.",
        ));
    };
    let title = format!("Request foundership of {}", team.name);
    if team.userid == user.id {
        return Ok(page(
            ctx,
            &title,
            &site_ui::notice("You are the founder of this team."),
        ));
    }
    if team.userid != 0 {
        return Ok(page(
            ctx,
            &title,
            "<p>This team has a founder. This site does not send the messages a foundership \
             request needs, so a member cannot ask for the team. The founder can hand it over \
             with Change founder on the team's administration page.</p>\n",
        ));
    }
    let b = format!(
        "<p>This team has no founder. As a member you may become its founder.</p>\n\
         <form method=\"post\" action=\"team_founder_transfer_action.php\">\n{}\
         <input type=\"hidden\" name=\"action\" value=\"initiate_transfer\">\n\
         <p><input type=\"submit\" value=\"Become the founder\"></p>\n</form>\n",
        token_fields(&user, ctx.now)
    );
    Ok(page(ctx, &title, &b))
}

/// `team_founder_transfer_action.php`, for `action=initiate_transfer` on a team
/// with no founder (see [`team_founder_transfer_form`]); the other actions
/// (`finalize_transfer`, `decline`) belong to the request flow and are refused.
/// The update names the account's own team and that it has no founder.
pub async fn team_founder_transfer_action(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    if let Some(p) = need_post(ctx) {
        return Ok(p);
    }
    let Some(user) = viewer(ctx).await? else {
        return Ok(to_login(ctx.base, "team_founder_transfer_form.php"));
    };
    if !token_ok(ctx, &user, q) {
        return Ok(error_page(project, 403, TIMED_OUT));
    }
    if str_param(q, "action") != "initiate_transfer" {
        return Ok(error_page(
            project,
            400,
            "Foundership requests are not supported on this site.",
        ));
    }
    let done = ctx
        .db
        .run(stmt!(
            "UPDATE team SET userid = ? WHERE id = ? AND userid = 0 AND ? > 0 \
             AND EXISTS (SELECT 1 FROM user WHERE id = ? AND teamid = ? AND authenticator = ? \
             AND blocked = 0) RETURNING id",
            user.id,
            user.teamid,
            user.teamid,
            user.id,
            user.teamid,
            user.authenticator.as_str()
        ))
        .await?;
    match done.scalar_i64() {
        Some(id) => Ok(go_to_team(ctx, id)),
        None => Ok(error_page(
            project,
            409,
            "Foundership request not allowed now.",
        )),
    }
}

// ---------------------------------------------------------------------------
// On other pages.

/// What an account's page says about teams: the team it is in with the links
/// that apply, and the teams it founded and is not a member of. Rows of a list
/// of label and HTML. One statement.
pub async fn account_rows(
    ctx: &SiteCtx<'_>,
    user: &User,
) -> CoreResult<Vec<(&'static str, String)>> {
    if ctx.project.config.disable_teams {
        return Ok(Vec::new());
    }
    let teams: Vec<Team> = ctx
        .db
        .run(stmt!(
            "SELECT id, name, userid FROM team WHERE id = ? OR userid = ? ORDER BY id LIMIT 20",
            user.teamid,
            user.id
        ))
        .await?
        .decode()?;
    let mut rows = Vec::new();
    match teams
        .iter()
        .find(|t| t.id == user.teamid && user.teamid > 0)
    {
        Some(t) => {
            let mut x = format!(
                "{} &middot; <a href=\"team_quit_form.php\">Quit team</a>",
                team_link(t)
            );
            if t.userid == user.id {
                x.push_str(&format!(
                    " &middot; <a href=\"team_manage.php?teamid={}\">Administer</a>",
                    t.id
                ));
            }
            rows.push(("Member of team", x));
        }
        None => rows.push((
            "Team",
            "None &middot; <a href=\"team_search.php\">find a team</a>".to_string(),
        )),
    }
    for t in teams
        .iter()
        .filter(|t| t.userid == user.id && t.id != user.teamid)
    {
        rows.push((
            "Founder but not member of",
            format!(
                "{} &middot; <a href=\"team_manage.php?teamid={}\">Administer</a>",
                team_link(t),
                t.id
            ),
        ));
    }
    Ok(rows)
}

/// The team row of an account's public page: its team with a link, or nothing
/// when the account has none or teams are off. One statement for an account
/// that has a team.
pub async fn public_team_row(ctx: &SiteCtx<'_>, teamid: i64) -> CoreResult<Option<String>> {
    if ctx.project.config.disable_teams || teamid <= 0 {
        return Ok(None);
    }
    Ok(team_by_id(ctx.db, teamid).await?.map(|t| team_link(&t)))
}
