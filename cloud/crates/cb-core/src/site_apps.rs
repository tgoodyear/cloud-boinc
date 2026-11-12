//! The applications page, `apps.php`, and its XML form, `apps.php?xml=1`.
//! Upstream's page is `html/user/apps.php`; the list of current versions is
//! `latest_avs_app_platform` in `html/inc/boinc_db.inc`.
//!
//! Only applications and platforms that are not deprecated are listed, and
//! for each application the versions that are not deprecated and not
//! replaced: a version is replaced by another one of the same application,
//! platform, plan class and beta flag with a higher version number. The page
//! runs no statement of its own. The web Worker has already loaded the
//! catalog (platforms, applications and versions, see
//! `crate::scheduler::Catalog`, kept for 30 seconds in each isolate, `PROJECT_TTL_MS` in
//! `cb-worker`) with
//! the settings, and the catalog holds exactly the rows this page lists, so
//! the cost is the same for one application and for a hundred.
//!
//! Differences from upstream are listed as D16 in `cloud/docs/compatibility.md`.

use std::collections::{BTreeMap, HashMap};

use cb_proto::web_rpc::XML_HEADER_UTF8;
use cb_proto::xml::xml_escape_text;

use crate::model::{App, AppVersion, Platform};
use crate::scheduler::Catalog;
use crate::site::{layout, time_str, Params, SiteCtx, SitePage};
use crate::site_boards::{stack_table, StackCol};
use crate::site_ui;
use crate::web::html_escape;

/// The current versions of every application and platform, in the order of
/// their IDs, by `(application ID, platform ID)`: `latest_avs_app_platform`
/// for all pairs at once. A version is current when it is not deprecated and
/// no other version of the same application, platform, plan class and beta
/// flag has a higher number; versions that tie for the highest number are all
/// kept, as upstream keeps them (it compares with `>`). Two passes over the
/// versions: the highest number of each (application, platform, plan class,
/// beta flag), then the versions that have it. A pair with no versions has no
/// entry, so the cost depends on the versions and not on how many applications
/// and platforms there are.
type CurrentIndex<'a> = BTreeMap<(i64, i64), Vec<&'a AppVersion>>;

fn current_index(catalog: &Catalog) -> CurrentIndex<'_> {
    let mut versions: Vec<&AppVersion> = catalog
        .app_versions
        .iter()
        .filter(|v| v.deprecated == 0)
        .collect();
    versions.sort_by_key(|v| v.id);
    let mut highest: HashMap<(i64, i64, &str, i32), i32> = HashMap::new();
    for v in &versions {
        let top = highest
            .entry((v.appid, v.platformid, v.plan_class.as_str(), v.beta))
            .or_insert(v.version_num);
        *top = (*top).max(v.version_num);
    }
    let mut index = CurrentIndex::new();
    for v in versions {
        if highest[&(v.appid, v.platformid, v.plan_class.as_str(), v.beta)] == v.version_num {
            index.entry((v.appid, v.platformid)).or_default().push(v);
        }
    }
    index
}

/// The current versions of `app` on `platform`, from [`current_index`].
fn current_versions<'a>(
    index: &'a CurrentIndex<'_>,
    app: &App,
    platform: &Platform,
) -> &'a [&'a AppVersion] {
    index.get(&(app.id, platform.id)).map_or(&[], Vec::as_slice)
}

/// A version number as the page writes it: 102 is `1.02` (`%0.2f` of the
/// number over 100 in upstream).
pub(crate) fn version_text(version_num: i32) -> String {
    format!("{:.2}", f64::from(version_num) / 100.0)
}

/// `apps.php`: the page, or with a non-empty `xml` other than `0` the XML list.
pub fn apps(ctx: &SiteCtx<'_>, q: &Params) -> SitePage {
    let catalog = &ctx.project.catalog;
    if q.get("xml").is_some_and(|v| !v.is_empty() && v != "0") {
        return SitePage::xml(apps_xml(catalog, &current_index(catalog)));
    }
    let name = &ctx.project.config.long_name;
    let mut body = format!(
        "<p>{0} currently has the following applications. When you participate in {0}, tasks for \
         one or more of these applications will be assigned to your computer. The current \
         version of the application will be downloaded to your computer. This happens \
         automatically; you don't have to do anything.</p>\n",
        html_escape(name)
    );
    if catalog.apps.is_empty() {
        body.push_str(&site_ui::empty("No applications are available yet."));
    }
    let index = current_index(catalog);
    let cols = [
        StackCol::who("Platform"),
        StackCol::text("Version"),
        StackCol::text("Plan class"),
        StackCol::text("Created"),
    ];
    for app in &catalog.apps {
        let mut rows = Vec::new();
        for platform in &catalog.platforms {
            for av in current_versions(&index, app, platform) {
                let beta = if av.beta != 0 { " (beta test)" } else { "" };
                rows.push(vec![
                    html_escape(&platform.user_friendly_name),
                    format!("{}{beta}", version_text(av.version_num)),
                    if av.plan_class.is_empty() {
                        "---".to_string()
                    } else {
                        html_escape(&av.plan_class)
                    },
                    time_str(av.create_time),
                ]);
            }
        }
        let title = format!(
            "{}{}",
            app.user_friendly_name,
            if app.beta != 0 { " (beta test)" } else { "" }
        );
        let mut card = format!(
            "<p class=\"muted\">Added {}</p>\n",
            html_escape(&time_str(app.create_time))
        );
        if rows.is_empty() {
            card.push_str(&site_ui::empty("No versions are available yet."));
        } else {
            card.push_str(&stack_table(&cols, &rows));
        }
        body.push_str(&site_ui::card(&title, &card));
    }
    SitePage::html(200, layout(ctx.project, "Applications", &body))
}

/// `apps.php?xml=1`: upstream's `show_apps_xml`, element for element.
fn apps_xml(catalog: &Catalog, index: &CurrentIndex<'_>) -> String {
    let mut o = format!("{XML_HEADER_UTF8}<app_versions>\n");
    for app in &catalog.apps {
        o.push_str(&format!(
            "<application>\n    <user_friendly_name>{}</user_friendly_name>\n    <name>{}</name>\n    <id>{}</id>\n",
            xml_escape_text(&app.user_friendly_name),
            xml_escape_text(&app.name),
            app.id
        ));
        if app.beta != 0 {
            o.push_str("        <beta/>\n");
        }
        for platform in &catalog.platforms {
            for av in current_versions(index, app, platform) {
                o.push_str(&format!(
                    "    <version>\n        <platform_short>{}</platform_short>\n        \
                     <platform_long>{}</platform_long>\n        <version_num>{}</version_num>\n        \
                     <plan_class>{}</plan_class>\n        <date>{}</date>\n        \
                     <date_unix>{}</date_unix>\n",
                    xml_escape_text(&platform.name),
                    xml_escape_text(&platform.user_friendly_name),
                    av.version_num,
                    xml_escape_text(&av.plan_class),
                    time_str(av.create_time),
                    av.create_time
                ));
                if av.beta != 0 {
                    o.push_str("        <beta/>\n");
                }
                o.push_str("    </version>\n");
            }
        }
        o.push_str("</application>\n");
    }
    o.push_str("</app_versions>\n");
    o
}
