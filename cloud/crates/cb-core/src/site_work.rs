//! `work.php`, the jobs overview, and `activity.php`, who is working on what.
//!
//! Neither page counts tasks. Both read what the pipeline's rollup wrote
//! (`cloud/crates/cb-core/src/stats.rs`): `work.php` the `overview` snapshot and
//! the 56 project-wide period rows, `activity.php` the `inflight` snapshot and,
//! to name the volunteers on the page, their accounts by ID.
//!
//! Privacy follows the stock site: a volunteer's name is public; a computer is
//! listed under its owner when the owner has `show_hosts` on or the project has
//! `host_owner_public` on (the default), and its domain name and addresses are
//! not read at all.

use std::collections::{BTreeMap, BTreeSet};

use cb_db::Stmt;
use serde::Deserialize;

use crate::charts::{Axis, Chart, Kind, Series};
use crate::config::Config;
use crate::feed::{age_text, render_feed, Feed};
use crate::project::Project;
use crate::site::{number_format, Params, SiteCtx, SitePage};
use crate::site_boards::{shell, stack_table, StackCol};
use crate::site_ui::{badge, card, card_accent, empty, notice, stat, stats, Tone};
use crate::stats::{
    load_inflight, load_work, week_bounds, AppFigures, Inflight, Overview, Period, BUCKET_SECS,
};
use crate::web::html_escape;
use crate::CoreResult;

/// Volunteers listed on `activity.php`, largest first.
pub const MAX_VOLUNTEERS: usize = 50;
/// Volunteers listed under each application.
pub const MAX_PER_APP: usize = 10;
/// Applications with a card of their own on `activity.php`, the ones with the
/// most tasks in progress.
pub const MAX_APP_CARDS: usize = 12;
/// Holders kept per application while the page is built (more than are shown).
const MAX_HOLDERS_KEPT: usize = MAX_PER_APP;
/// Statements the account lookups of `activity.php` may take: with the snapshot
/// read, the page sends at most `1 + MAX_ACCOUNT_STATEMENTS` of its own.
pub const MAX_ACCOUNT_STATEMENTS: usize = 3;
/// Computers listed under one volunteer.
pub const MAX_COMPUTERS: usize = 20;
/// Accounts looked up by one statement; D1 allows 100 bound values. The page
/// shows at most `MAX_VOLUNTEERS + MAX_APP_CARDS * MAX_PER_APP` = 170 accounts,
/// which is three statements.
const LOOKUP_CHUNK: usize = 80;
/// The counts are made every five minutes; older than this the page says so.
const STALE_SECS: i64 = 20 * 60;

/// What the project panels are, when `work.php` is built.
#[derive(Debug, Clone)]
pub enum FeedState {
    /// The config names no feed: the panels are left out.
    NotConfigured,
    /// It names one and the Worker could not use it: a note takes their place.
    Unavailable,
    Ready(Feed),
}

impl FeedState {
    /// What a caller that does not fetch the feed should pass.
    pub fn without_fetch(config: &Config) -> FeedState {
        if config.dashboard_feed_url.is_empty() {
            FeedState::NotConfigured
        } else {
            FeedState::Unavailable
        }
    }
}

fn app_name(project: &Project, id: i64) -> String {
    if id == crate::stats::OTHER_APP {
        return "Other applications".into();
    }
    project
        .catalog
        .apps
        .iter()
        .find(|a| a.id == id)
        .map(|a| {
            if a.user_friendly_name.is_empty() {
                a.name.clone()
            } else {
                a.user_friendly_name.clone()
            }
        })
        .unwrap_or_else(|| format!("Application {id}"))
}

fn app_deprecated(project: &Project, id: i64) -> bool {
    if id == crate::stats::OTHER_APP {
        return false;
    }
    project
        .catalog
        .apps
        .iter()
        .find(|a| a.id == id)
        .is_none_or(|a| a.deprecated != 0)
}

/// `1 task`, `2 tasks`.
fn plural(n: i64, noun: &str) -> String {
    format!("{} {noun}{}", count(n), if n == 1 { "" } else { "s" })
}

fn count(n: i64) -> String {
    number_format(n as f64, 0)
}

/// A count that is at least what is shown when `exact` is false: `1,234+`.
fn at_least(n: i64, exact: bool) -> String {
    if exact {
        count(n)
    } else {
        format!("{}+", count(n))
    }
}

/// The queue length of an application (0 for the project): a lower bound when
/// the count stopped at the cap before or inside it.
fn unsent_text(o: &Overview, f: &AppFigures) -> String {
    at_least(f.unsent, !o.unsent_is_lower_bound(f.id))
}

/// The note that says the 24-hour and 7-day figures do not cover every period
/// yet, or none when they do.
fn coverage_note(o: &Overview) -> String {
    if o.day_complete() && o.week_complete() {
        return String::new();
    }
    notice(&format!(
        "Counting is still catching up, or part of the history was purged: the last 24 hours \
         cover {} of {} three-hour periods and the last 7 days {} of {}. Figures marked + are \
         at least that many.",
        o.periods_24h, o.expected_24h, o.periods_7d, o.expected_7d
    ))
}

/// A badge for how old a snapshot is.
fn snapshot_badge(at: i64, now: i64) -> String {
    let age = (now - at).max(0);
    if age <= STALE_SECS {
        badge(&format!("counted {}", age_text(age)), Tone::Good)
    } else {
        badge(&format!("stale, counted {}", age_text(age)), Tone::Warning)
    }
}

fn applications_card(project: &Project, o: &Overview, now: i64) -> String {
    let cols = [
        StackCol::who("Application"),
        StackCol::num("To send"),
        StackCol::num("In progress"),
        StackCol::num("To validate"),
        StackCol::num("To assimilate"),
        StackCol::num("Done, 24 h"),
        StackCol::num("Done, 7 d"),
        StackCol::num("Failed, 24 h"),
        StackCol::num("Failed, 7 d"),
    ];
    let (day, week) = (o.day_complete(), o.week_complete());
    let row = |name: String, f: &AppFigures| {
        vec![
            name,
            unsent_text(o, f),
            count(f.in_progress),
            count(f.wait_validate),
            count(f.wait_assimilate),
            at_least(f.ok_24h, day),
            at_least(f.ok_7d, week),
            at_least(f.err_24h, day),
            at_least(f.err_7d, week),
        ]
    };
    let busy = |f: &AppFigures| {
        f.unsent + f.in_progress + f.wait_validate + f.wait_assimilate + f.ok_7d + f.err_7d > 0
    };
    // Every application the snapshot has, and every live one of the catalog: a
    // queue past the cap may leave an application out of the count.
    let mut figures: BTreeMap<i64, AppFigures> = o
        .apps
        .iter()
        .filter(|f| f.id != 0)
        .map(|f| (f.id, f.clone()))
        .collect();
    for a in project.catalog.apps.iter().filter(|a| a.deprecated == 0) {
        figures.entry(a.id).or_insert_with(|| AppFigures {
            id: a.id,
            ..Default::default()
        });
    }
    let mut rows = Vec::new();
    for f in figures.values() {
        if app_deprecated(project, f.id) && !busy(f) {
            continue;
        }
        rows.push(row(html_escape(&app_name(project, f.id)), f));
    }
    if let Some(total) = o.apps.iter().find(|f| f.id == 0) {
        if rows.len() != 1 {
            rows.push(row("<strong>All applications</strong>".into(), total));
        }
    }
    let mut body = format!("<p>{}</p>\n", snapshot_badge(o.at, now));
    body.push_str(&stack_table(&cols, &rows));
    body.push_str(
        "<p><small>Counted every five minutes. \"To send\" is tasks waiting to be sent, \"to validate\" and \"to assimilate\" are workunits waiting for those steps. \"Done\" is tasks returned with a success \
         outcome; \"failed\" is tasks returned with an error, tasks that failed validation and \
         tasks that passed their deadline. A day is the last eight three-hour periods and a \
         week the last 56. Timeouts are counted from when this feature was installed. \
         The queue is counted up to 100,000 tasks in all; past that a figure marked + is at \
         least that many.</small></p>\n",
    );
    body.push_str(&coverage_note(o));
    card("Applications", &body)
}

fn history_cards(history: &[Option<Period>], now: i64) -> String {
    let (first, _) = week_bounds(now);
    let times: Vec<i64> = (0..history.len() as i64)
        .map(|i| first + i * BUCKET_SECS)
        .collect();
    let col = |f: &dyn Fn(&Period) -> Option<f64>| -> Vec<Option<f64>> {
        history.iter().map(|p| p.as_ref().and_then(f)).collect()
    };
    let queued = col(&|p| (p.queued >= 0).then_some(p.queued as f64));
    let queued_capped: Vec<bool> = history
        .iter()
        .map(|p| p.as_ref().is_some_and(|p| p.queued_capped != 0))
        .collect();
    let completed = col(&|p| Some(p.returned_ok as f64));
    let users = col(&|p| Some(p.users as f64));
    let hosts = col(&|p| Some(p.hosts as f64));
    let mut cards: Vec<String> = Vec::new();

    let series = [Series {
        label: "Tasks waiting to send",
        values: &queued,
        at_least: &queued_capped,
    }];
    let c = Chart {
        title: "Tasks waiting to send",
        description: "Tasks ready to send at the last count in each three-hour period, over the last seven days",
        kind: Kind::Area,
        axis: Axis::Midnights,
        times: &times,
        series: &series,
        last_partial: true,
    };
    cards.push(card(
        "Queue, last 7 days",
        &format!(
            "<p>Tasks waiting to send, at the last count in each three-hour period.</p>\n{}{}{}",
            c.svg("chart-queue"),
            c.legend(),
            c.data_details(
                "Tasks ready to send. A period with no count is unknown. A value marked + is at least that many: the count stopped at its cap.",
            )
        ),
    ));

    let series = [Series {
        label: "Tasks completed",
        values: &completed,
        at_least: &[],
    }];
    let c = Chart {
        title: "Tasks completed",
        description: "Tasks returned with a success outcome in each three-hour period, over the last seven days",
        kind: Kind::Bars,
        axis: Axis::Midnights,
        times: &times,
        series: &series,
        last_partial: true,
    };
    cards.push(card(
        "Completed, last 7 days",
        &format!(
            "<p>Tasks returned with a success outcome in each three-hour period.</p>\n{}{}{}",
            c.svg("chart-done"),
            c.legend(),
            c.data_details(
                "Periods that have not been counted are unknown. The last period is still filling."
            )
        ),
    ));

    let series = [
        Series {
            label: "Volunteers",
            values: &users,
            at_least: &[],
        },
        Series {
            label: "Computers",
            values: &hosts,
            at_least: &[],
        },
    ];
    let c = Chart {
        title: "Active volunteers and computers",
        description: "Accounts and computers that returned a task in each three-hour period, over the last seven days",
        kind: Kind::Lines,
        axis: Axis::Midnights,
        times: &times,
        series: &series,
        last_partial: true,
    };
    cards.push(card(
        "Active, last 7 days",
        &format!(
            "<p>Volunteers and computers that returned a task in each three-hour period.</p>\n{}{}{}",
            c.svg("chart-active"),
            c.legend(),
            c.data_details("Periods that have not been counted are unknown.")
        ),
    ));
    format!("<div class=\"charts\">\n{}</div>\n", cards.concat())
}

/// `work.php`: tasks waiting, in progress and done per application, a week of
/// history, and the project's own panels. One round trip of two statements.
pub async fn work(ctx: &SiteCtx<'_>, _q: &Params, feed: &FeedState) -> CoreResult<SitePage> {
    let project = ctx.project;
    let (overview, history) = load_work(ctx.db, ctx.now).await?;
    let mut body = String::new();
    match &overview {
        Some(o) => {
            if let Some(t) = o.apps.iter().find(|f| f.id == 0) {
                let day = o.day_complete();
                body.push_str(&stats(&[
                    stat("Waiting to send", &unsent_text(o, t), ""),
                    stat("In progress", &count(t.in_progress), ""),
                    stat("Completed, 24 hours", &at_least(t.ok_24h, day), ""),
                    stat("Failed, 24 hours", &at_least(t.err_24h, day), ""),
                ]));
            }
            body.push_str(&applications_card(project, o, ctx.now));
        }
        None => body.push_str(&notice(
            "The counts are not available yet. The server makes them every five minutes.",
        )),
    }
    body.push_str(&history_cards(&history, ctx.now));
    match feed {
        FeedState::NotConfigured => {}
        FeedState::Unavailable => body.push_str(&card_accent(
            "Project panels",
            &notice("The project's own figures are not available right now."),
        )),
        FeedState::Ready(f) => body.push_str(&render_feed(f, ctx.now)),
    }
    Ok(SitePage::html(200, shell(project, "Jobs", &body)))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Person {
    id: i64,
    name: String,
    show_hosts: i64,
    blocked: i64,
}

/// What one volunteer holds.
#[derive(Default)]
struct Volunteer {
    total: i64,
    per_app: BTreeMap<i64, i64>,
    /// computer -> (application -> tasks)
    computers: BTreeMap<i64, BTreeMap<i64, i64>>,
    last_contact: i64,
}

fn volunteers(snap: &Inflight) -> BTreeMap<i64, Volunteer> {
    let mut out: BTreeMap<i64, Volunteer> = BTreeMap::new();
    for r in &snap.rows {
        let v = out.entry(r.user).or_default();
        v.total += r.n;
        *v.per_app.entry(r.app).or_default() += r.n;
        *v.computers
            .entry(r.host)
            .or_default()
            .entry(r.app)
            .or_default() += r.n;
        v.last_contact = v.last_contact.max(r.rpc);
    }
    out
}

async fn people(ctx: &SiteCtx<'_>, ids: &BTreeSet<i64>) -> CoreResult<BTreeMap<i64, Person>> {
    // A hard bound: whatever the caller asks for, at most this many accounts
    // are looked up, in at most `MAX_ACCOUNT_STATEMENTS` statements. An account
    // not looked up is not shown (see `activity`).
    let ids: Vec<i64> = ids
        .iter()
        .copied()
        .filter(|i| *i > 0)
        .take(MAX_ACCOUNT_STATEMENTS * LOOKUP_CHUNK)
        .collect();
    let stmts: Vec<Stmt> = ids
        .chunks(LOOKUP_CHUNK)
        .map(|chunk| {
            let mut s = Stmt::new(format!(
                "SELECT id, name, show_hosts, blocked FROM user WHERE id IN ({})",
                vec!["?"; chunk.len()].join(",")
            ));
            for id in chunk {
                s = s.bind(*id);
            }
            s
        })
        .collect();
    let mut out = BTreeMap::new();
    if stmts.is_empty() {
        return Ok(out);
    }
    for rows in ctx.db.batch(stmts).await? {
        for p in rows.decode::<Person>()? {
            out.insert(p.id, p);
        }
    }
    Ok(out)
}

fn person_name(p: Option<&Person>) -> String {
    match p {
        Some(p) if !p.name.trim().is_empty() => html_escape(&p.name),
        _ => "Anonymous".into(),
    }
}

fn person_link(id: i64, p: Option<&Person>) -> String {
    if p.is_some() {
        format!(
            "<a href=\"show_user.php?userid={id}\">{}</a>",
            person_name(p)
        )
    } else {
        person_name(p)
    }
}

fn app_counts(project: &Project, apps: &BTreeMap<i64, i64>) -> String {
    let mut by_count: Vec<(&i64, &i64)> = apps.iter().collect();
    by_count.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    by_count
        .iter()
        .map(|(app, n)| {
            badge(
                &format!("{}: {}", app_name(project, **app), n),
                Tone::Neutral,
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `activity.php`: the volunteers who hold tasks, and for each application
/// who is working on it. One statement for the snapshot and one or two for
/// the accounts.
pub async fn activity(ctx: &SiteCtx<'_>, _q: &Params) -> CoreResult<SitePage> {
    let project = ctx.project;
    let Some(snap) = load_inflight(ctx.db).await? else {
        return Ok(SitePage::html(
            200,
            shell(
                project,
                "Activity",
                &notice(
                    "The counts are not available yet. The server makes them every five minutes.",
                ),
            ),
        ));
    };
    let by_user = volunteers(&snap);
    let mut order: Vec<i64> = by_user.keys().copied().collect();
    order.sort_by(|a, b| by_user[b].total.cmp(&by_user[a].total).then(a.cmp(b)));
    let shown: Vec<i64> = order.iter().copied().take(MAX_VOLUNTEERS).collect();

    // Per application, the largest holders, for the applications with the
    // most tasks. The holders are chosen before anything is known about their
    // accounts, so what is looked up is fixed in advance: at most
    // `MAX_VOLUNTEERS + MAX_APP_CARDS * MAX_PER_APP` accounts, in at most
    // `MAX_ACCOUNT_STATEMENTS` statements, whatever the number of applications
    // or volunteers.
    let mut app_totals: BTreeMap<i64, i64> = BTreeMap::new();
    let mut per_app: BTreeMap<i64, Vec<(i64, i64)>> = BTreeMap::new();
    for (user, v) in &by_user {
        for (app, n) in &v.per_app {
            *app_totals.entry(*app).or_default() += n;
            per_app.entry(*app).or_default().push((*user, *n));
        }
    }
    for list in per_app.values_mut() {
        list.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        list.truncate(MAX_HOLDERS_KEPT);
    }
    let mut apps: Vec<i64> = app_totals.keys().copied().collect();
    apps.sort_by(|a, b| app_totals[b].cmp(&app_totals[a]).then(a.cmp(b)));
    let apps_all = apps.len();
    apps.truncate(MAX_APP_CARDS);
    let mut wanted: BTreeSet<i64> = shown.iter().copied().collect();
    for app in &apps {
        wanted.extend(per_app[app].iter().take(MAX_PER_APP).map(|(u, _)| *u));
    }
    let names = people(ctx, &wanted).await?;
    // Only an account that was looked up and is not blocked is shown: a
    // holder whose account was not fetched, or is blocked, is left out, never
    // shown as if it were nobody.
    let visible = |u: &i64| names.get(u).is_some_and(|p| p.blocked == 0);

    let in_progress: i64 = by_user.values().map(|v| v.total).sum();
    let mut body = String::new();
    // The volunteers and computers are counted over every task in progress; the
    // tasks are summed from the rows kept, so past the cap they are at least.
    body.push_str(&stats(&[
        stat("Volunteers working", &count(snap.working_users), ""),
        stat("Computers working", &count(snap.working_hosts), ""),
        stat(
            "Tasks in progress",
            &at_least(in_progress, !snap.truncated),
            "",
        ),
    ]));
    let age = snapshot_badge(snap.at, ctx.now);
    if by_user.is_empty() {
        // The counts say how old they are here too: an empty list that stopped
        // being refreshed is not "right now".
        body.push_str(&format!("<p>{age}</p>\n"));
        body.push_str(&empty(
            "No task was in progress when the server last counted.",
        ));
        return Ok(SitePage::html(200, shell(project, "Activity", &body)));
    }

    let cols = [
        StackCol::who("Volunteer"),
        StackCol::num("Tasks in progress"),
        StackCol::text("Applications"),
        StackCol::num("Computers"),
        StackCol::text("Last contact"),
    ];
    let rows: Vec<Vec<String>> = shown
        .iter()
        .filter(|u| visible(u))
        .map(|u| {
            let v = &by_user[u];
            let p = names.get(u);
            let hosts_cell = if p.is_some_and(|p| p.show_hosts != 0 || project.config.host_owner_public) {
                let mut list = String::new();
                for (host, apps) in v.computers.iter().take(MAX_COMPUTERS) {
                    list.push_str(&format!(
                        "<li><a href=\"show_host_detail.php?hostid={host}\">Computer #{host}</a>: {}</li>",
                        app_counts(project, apps)
                    ));
                }
                if v.computers.len() > MAX_COMPUTERS {
                    list.push_str(&format!(
                        "<li>and {} more</li>",
                        v.computers.len() - MAX_COMPUTERS
                    ));
                }
                format!(
                    "<details><summary>{}</summary><ul>{list}</ul></details>",
                    v.computers.len()
                )
            } else {
                v.computers.len().to_string()
            };
            vec![
                person_link(*u, p),
                count(v.total),
                app_counts(project, &v.per_app),
                hosts_cell,
                if v.last_contact > 0 {
                    age_text(ctx.now - v.last_contact)
                } else {
                    "unknown".into()
                },
            ]
        })
        .collect();
    let mut vol = format!(
        "<p>{age} <small>The server makes these counts every five minutes. {}</small></p>\n",
        if project.config.host_owner_public {
            "Computers are listed under their volunteers."
        } else {
            "A computer is listed under its volunteer only when the account allows its computers to be shown."
        }
    );
    vol.push_str(&stack_table(&cols, &rows));
    if snap.truncated {
        vol.push_str(
            "<p><small>There are more computers holding tasks than the page keeps; those with \
             the most are listed, so the lists below are not complete.</small></p>\n",
        );
    }
    if order.len() > shown.len() {
        vol.push_str(&format!(
            "<p><small>The {} volunteers holding the most tasks are shown, of {}.</small></p>\n",
            shown.len(),
            order.len()
        ));
    }
    body.push_str(&card_accent("Who is working", &vol));

    // By application.
    let cards: Vec<String> = apps
        .iter()
        .map(|app| {
            let list = &per_app[app];
            let holders = app_holders(&by_user, *app);
            let rows: Vec<Vec<String>> = list
                .iter()
                .take(MAX_PER_APP)
                .filter(|(u, _)| visible(u))
                .map(|(u, n)| vec![person_link(*u, names.get(u)), count(*n)])
                .collect();
            let mut b = format!(
                "<p>{} in progress, held by {}.</p>\n",
                plural(app_totals[app], "task"),
                plural(holders as i64, "volunteer")
            );
            b.push_str(&stack_table(
                &[
                    StackCol::who("Volunteer"),
                    StackCol::num("Tasks in progress"),
                ],
                &rows,
            ));
            card(&app_name(project, *app), &b)
        })
        .collect();
    body.push_str(&crate::site_ui::grid(&cards));
    if apps_all > apps.len() {
        body.push_str(&format!(
            "<p><small>The {} applications with the most tasks in progress are listed, of {}.</small></p>\n",
            apps.len(),
            apps_all
        ));
    }
    Ok(SitePage::html(200, shell(project, "Activity", &body)))
}

/// How many volunteers hold tasks of `app` (the list kept per application is cut
/// short, so this counts from the volunteers themselves).
fn app_holders(by_user: &BTreeMap<i64, Volunteer>, app: i64) -> usize {
    by_user
        .values()
        .filter(|v| v.per_app.contains_key(&app))
        .count()
}
