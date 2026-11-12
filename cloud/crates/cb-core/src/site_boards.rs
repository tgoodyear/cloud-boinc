//! The public leaderboards, `top_users.php` and `top_hosts.php`, and what the
//! dashboard pages share: the page paths that are public and cacheable, their
//! canonical query, the stacking table and the page shell.
//!
//! Upstream's pages are `html/user/top_users.php` and `top_hosts.php`. Names,
//! credit and the country of an account are public there and here. A computer
//! is listed with its owner's name when the owner has `show_hosts` on or the
//! project has `host_owner_public` on (the default, unlike upstream), and as
//! "Anonymous" otherwise (the stock rule). A computer's domain name and
//! addresses are never selected.
//!
//! Each page reads the first rows of one partial index (`user_total_credit` and
//! its siblings in `migrations/0008_dashboards.sql`): at most [`PAGE_SIZE`] + 1
//! index entries and as many rows, from at most [`MAX_PAGE`] pages in, so the
//! most a request can make the database read is about [`MAX_PAGE`] x
//! [`PAGE_SIZE`] index entries. The query for a computer adds one lookup of
//! each owner by ID. The response is cached for [`CACHE_SECS`].

use cb_db::Stmt;

use crate::avg::decayed;
use crate::host_desc::HostMisc;
use crate::model::User;
use crate::project::Project;
use crate::site::{layout, number_format, str_param, Params, SiteCtx, SitePage};
use crate::web::html_escape;
use crate::CoreResult;

/// Rows on a page of a leaderboard.
pub const PAGE_SIZE: usize = 50;
/// Deepest page that can be asked for. A deeper request gets this one.
pub const MAX_PAGE: usize = 40;
/// How long a browser or the edge cache may keep a dashboard page.
pub const CACHE_SECS: u32 = 60;

/// The dashboard pages. They are the same for every visitor and are served
/// with `Cache-Control: public`; the Worker also keeps them in the edge cache
/// under [`public_cache_key`].
pub const PUBLIC_PATHS: [&str; 4] = [
    "/top_users.php",
    "/top_hosts.php",
    "/work.php",
    "/activity.php",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sort {
    TotalCredit,
    Recent,
    Jobs,
}

impl Sort {
    const ALL: [Sort; 3] = [Sort::TotalCredit, Sort::Recent, Sort::Jobs];

    fn parse(s: &str) -> Sort {
        match s {
            "expavg_credit" => Sort::Recent,
            "valid_results" => Sort::Jobs,
            _ => Sort::TotalCredit,
        }
    }

    /// The query value, which is also the column that is sorted.
    fn key(self) -> &'static str {
        match self {
            Sort::TotalCredit => "total_credit",
            Sort::Recent => "expavg_credit",
            Sort::Jobs => "valid_results",
        }
    }

    /// The indexed column the sort uses. "Recent average" is ordered by
    /// `recent_key`, which compares averages as they stand now (see
    /// `migrations/0008_dashboards.sql`), not by the stored average.
    fn column(self) -> &'static str {
        match self {
            Sort::TotalCredit => "total_credit",
            Sort::Recent => "recent_key",
            Sort::Jobs => "valid_results",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Sort::TotalCredit => "Total credit",
            Sort::Recent => "Recent average credit",
            Sort::Jobs => "Jobs completed",
        }
    }
}

fn page_param(q: &Params) -> usize {
    str_param(q, "page")
        .parse::<usize>()
        .unwrap_or(1)
        .clamp(1, MAX_PAGE)
}

/// The cache key of a request for a public page: its path and the parameters
/// the page reads, in canonical form, so that other query strings do not
/// each get an entry. `None` for a path that is not one of [`PUBLIC_PATHS`].
pub fn public_cache_key(path: &str, q: &Params) -> Option<String> {
    match path {
        "/top_users.php" | "/top_hosts.php" => Some(format!(
            "{path}?sort={}&page={}",
            Sort::parse(str_param(q, "sort")).key(),
            page_param(q)
        )),
        "/work.php" | "/activity.php" => Some(path.to_string()),
        _ => None,
    }
}

/// A dashboard page: the site layout.
pub(crate) fn shell(project: &Project, title: &str, body: &str) -> String {
    layout(project, title, body)
}

/// One column of a [`stack_table`].
pub(crate) struct StackCol {
    pub label: &'static str,
    pub num: bool,
    /// The cell is the row's name: shown first and bold on a phone.
    pub who: bool,
}

impl StackCol {
    pub const fn text(label: &'static str) -> StackCol {
        StackCol {
            label,
            num: false,
            who: false,
        }
    }
    pub const fn num(label: &'static str) -> StackCol {
        StackCol {
            label,
            num: true,
            who: false,
        }
    }
    pub const fn who(label: &'static str) -> StackCol {
        StackCol {
            label,
            num: false,
            who: true,
        }
    }
}

/// A table that becomes one block per row, label over value, under 40rem (the
/// `table.stack` rules in `site.css`). The header row is in `<thead>`, hidden
/// from sight there, and each cell carries its column label as `data-label`.
/// `web::page` puts it in the scroll box of the site kit, as every table. Cells
/// are HTML.
pub(crate) fn stack_table(cols: &[StackCol], rows: &[Vec<String>]) -> String {
    let mut o = String::from("<table class=\"stack\">\n<thead><tr>");
    for c in cols {
        o.push_str(&format!(
            "<th{}>{}</th>",
            if c.num { " class=\"num\"" } else { "" },
            html_escape(c.label)
        ));
    }
    o.push_str("</tr></thead>\n<tbody>\n");
    for row in rows {
        o.push_str("<tr>");
        for (i, c) in cols.iter().enumerate() {
            let class = match (c.num, c.who) {
                (true, _) => " class=\"num\"",
                (_, true) => " class=\"who\"",
                _ => "",
            };
            o.push_str(&format!(
                "<td{class} data-label=\"{}\">{}</td>",
                html_escape(c.label),
                row.get(i).map_or("", String::as_str)
            ));
        }
        o.push_str("</tr>\n");
    }
    o.push_str("</tbody></table>\n");
    o
}

/// The sort links and the previous and next links of a leaderboard.
fn controls(path: &str, sort: Sort, page: usize, has_next: bool) -> (String, String) {
    let mut sorts = String::from("<p>Sort by: ");
    for (i, s) in Sort::ALL.iter().enumerate() {
        if i > 0 {
            sorts.push_str(" &middot; ");
        }
        if *s == sort {
            sorts.push_str(&format!(
                "<strong aria-current=\"true\">{}</strong>",
                s.label()
            ));
        } else {
            sorts.push_str(&format!(
                "<a href=\"{}?sort={}\">{}</a>",
                &path[1..],
                s.key(),
                s.label()
            ));
        }
    }
    sorts.push_str("</p>\n");
    let mut pager = String::new();
    if page > 1 || has_next {
        pager.push_str("<p>");
        if page > 1 {
            pager.push_str(&format!(
                "<a href=\"{}?sort={}&amp;page={}\">Previous</a> ",
                &path[1..],
                sort.key(),
                page - 1
            ));
        }
        pager.push_str(&format!("Page {page}"));
        if has_next {
            pager.push_str(&format!(
                " <a href=\"{}?sort={}&amp;page={}\">Next</a>",
                &path[1..],
                sort.key(),
                page + 1
            ));
        }
        pager.push_str("</p>\n");
    }
    (sorts, pager)
}

/// Credit to the whole unit, and to hundredths while it is small.
fn credit(v: f64) -> String {
    number_format(v, if v.abs() < 100.0 { 2 } else { 0 })
}

fn name_or_anonymous(name: &str) -> String {
    if name.trim().is_empty() {
        "Anonymous".into()
    } else {
        html_escape(name)
    }
}

const RECENT_NOTE: &str = "Recent average credit is credit per day, and halves every week an \
     account or computer earns nothing. Jobs completed counts tasks that passed validation.";

/// `top_users.php`: accounts with credit, by total credit, recent average
/// credit or jobs completed. One statement.
pub async fn top_users(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    let sort = Sort::parse(str_param(q, "sort"));
    let page = page_param(q);
    let offset = (page - 1) * PAGE_SIZE;
    // The literal `total_credit > 0` is the partial indexes' condition.
    let rows: Vec<User> = ctx
        .db
        .run(Stmt {
            sql: format!(
                "SELECT id, name, country, total_credit, expavg_credit, expavg_time, valid_results \
                 FROM user WHERE total_credit > 0 AND blocked = 0 \
                 ORDER BY {} DESC, id LIMIT ? OFFSET ?",
                sort.column()
            ),
            params: vec![((PAGE_SIZE + 1) as i64).into(), (offset as i64).into()],
        })
        .await?
        .decode()?;
    let has_next = rows.len() > PAGE_SIZE && page < MAX_PAGE;
    let mut rows = rows;
    rows.truncate(PAGE_SIZE);
    let now = ctx.now as f64;
    let recent = |u: &User| decayed(u.expavg_credit, u.expavg_time, now);

    let show_country = project.config.user_country;
    let mut cols = vec![
        StackCol::num("Rank"),
        StackCol::who("User"),
        StackCol::num("Total credit"),
        StackCol::num("Recent average credit"),
        StackCol::num("Jobs completed"),
    ];
    if show_country {
        cols.push(StackCol::text("Country"));
    }
    let table: Vec<Vec<String>> = rows
        .iter()
        .enumerate()
        .map(|(i, u)| {
            let mut r = vec![
                (offset + i + 1).to_string(),
                format!(
                    "<a href=\"show_user.php?userid={}\">{}</a>",
                    u.id,
                    name_or_anonymous(&u.name)
                ),
                credit(u.total_credit),
                number_format(recent(u), 2),
                number_format(u.valid_results as f64, 0),
            ];
            if show_country {
                r.push(html_escape(&u.country));
            }
            r
        })
        .collect();
    let (sorts, pager) = controls("/top_users.php", sort, page, has_next);
    let mut body = sorts;
    if rows.is_empty() {
        body.push_str(&crate::site_ui::empty(if page > 1 {
            "There is nothing on this page."
        } else {
            "No account has credit yet."
        }));
    } else {
        body.push_str(&stack_table(&cols, &table));
    }
    body.push_str(&pager);
    body.push_str(&format!(
        "<p><small>{}</small></p>\n",
        html_escape(RECENT_NOTE)
    ));
    Ok(SitePage::html(200, shell(project, "Top users", &body)))
}

/// A leaderboard row of a computer, with its owner's name and `show_hosts`.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct HostRow {
    id: i64,
    userid: i64,
    total_credit: f64,
    expavg_credit: f64,
    expavg_time: f64,
    valid_results: i64,
    p_vendor: String,
    p_model: String,
    p_ncpus: i64,
    os_name: String,
    os_version: String,
    serialnum: String,
    misc: String,
    owner_name: Option<String>,
    owner_show_hosts: Option<i64>,
    owner_blocked: Option<i64>,
}

/// The CPU, operating system and GPUs of a computer, one per line. Escaped.
fn hardware(h: &HostRow) -> String {
    let mut lines = vec![
        format!(
            "{} {} ({} cores)",
            html_escape(&h.p_vendor),
            html_escape(&h.p_model),
            h.p_ncpus
        ),
        format!("{} {}", html_escape(&h.os_name), html_escape(&h.os_version)),
    ];
    let gpu = HostMisc::read(&h.misc, &h.serialnum).gpu_desc();
    if gpu != "---" {
        lines.push(gpu);
    }
    lines.join("<br>")
}

/// `top_hosts.php`: computers with credit. The owner is named only if the
/// account shows its computers. One statement: the page of computers, then each
/// owner by ID.
pub async fn top_hosts(ctx: &SiteCtx<'_>, q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    let sort = Sort::parse(str_param(q, "sort"));
    let page = page_param(q);
    let offset = (page - 1) * PAGE_SIZE;
    let key = sort.column();
    let rows: Vec<HostRow> = ctx
        .db
        .run(Stmt {
            sql: format!(
                "SELECT h.id, h.userid, h.total_credit, h.expavg_credit, h.expavg_time, \
                 h.valid_results, h.recent_key, h.p_vendor, h.p_model, h.p_ncpus, h.os_name, h.os_version, \
                 h.serialnum, h.misc, u.name AS owner_name, u.show_hosts AS owner_show_hosts, \
                 u.blocked AS owner_blocked \
                 FROM (SELECT id, userid, total_credit, expavg_credit, expavg_time, valid_results, \
                       recent_key, p_vendor, p_model, p_ncpus, os_name, os_version, serialnum, misc FROM host \
                       WHERE total_credit > 0 ORDER BY {key} DESC, id LIMIT ? OFFSET ?) h \
                 LEFT JOIN user u ON u.id = h.userid ORDER BY h.{key} DESC, h.id"
            ),
            params: vec![((PAGE_SIZE + 1) as i64).into(), (offset as i64).into()],
        })
        .await?
        .decode()?;
    let has_next = rows.len() > PAGE_SIZE && page < MAX_PAGE;
    let mut rows = rows;
    rows.truncate(PAGE_SIZE);
    let now = ctx.now as f64;
    let recent = |r: &HostRow| decayed(r.expavg_credit, r.expavg_time, now);

    let public = project.config.host_owner_public;
    let cols = [
        StackCol::num("Rank"),
        StackCol::who("Computer"),
        StackCol::text("Owner"),
        StackCol::num("Total credit"),
        StackCol::num("Recent average credit"),
        StackCol::num("Jobs completed"),
        StackCol::text("Hardware"),
    ];
    let table: Vec<Vec<String>> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let h = r;
            let owner = match (r.owner_show_hosts, &r.owner_name) {
                // A blocked account is left out of the account leaderboards, so
                // its computers do not name it either.
                (Some(show), Some(name))
                    if (show != 0 || public) && r.owner_blocked.unwrap_or(0) == 0 =>
                {
                    format!(
                        "<a href=\"show_user.php?userid={}\">{}</a>",
                        h.userid,
                        name_or_anonymous(name)
                    )
                }
                _ => "Anonymous".into(),
            };
            vec![
                (offset + i + 1).to_string(),
                format!("<a href=\"show_host_detail.php?hostid={0}\">#{0}</a>", h.id),
                owner,
                credit(h.total_credit),
                number_format(recent(r), 2),
                number_format(h.valid_results as f64, 0),
                hardware(h),
            ]
        })
        .collect();
    let (sorts, pager) = controls("/top_hosts.php", sort, page, has_next);
    let mut body = sorts;
    if rows.is_empty() {
        body.push_str(&crate::site_ui::empty(if page > 1 {
            "There is nothing on this page."
        } else {
            "No computer has credit yet."
        }));
    } else {
        body.push_str(&stack_table(&cols, &table));
    }
    body.push_str(&pager);
    body.push_str(&format!(
        "<p><small>{}{}</small></p>\n",
        html_escape(RECENT_NOTE),
        if public {
            ""
        } else {
            " The owner of a computer is named only when the account allows its computers to be shown."
        }
    ));
    Ok(SitePage::html(200, shell(project, "Top computers", &body)))
}
