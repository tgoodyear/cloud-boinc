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

use cb_proto::web_rpc::XML_HEADER_UTF8;
use cb_proto::xml::xml_escape_text;

use crate::model::{App, AppVersion, Platform};
use crate::scheduler::Catalog;
use crate::site::{layout, time_str, Params, SiteCtx, SitePage};
use crate::site_boards::{stack_table, StackCol};
use crate::site_ui;
use crate::web::html_escape;

/// The versions of `app` on `platform` that a visitor should know about, in
/// the order of their IDs: `latest_avs_app_platform`.
fn current_versions<'a>(
    catalog: &'a Catalog,
    app: &App,
    platform: &Platform,
) -> Vec<&'a AppVersion> {
    let all: Vec<&AppVersion> = catalog
        .app_versions
        .iter()
        .filter(|v| v.appid == app.id && v.platformid == platform.id && v.deprecated == 0)
        .collect();
    all.iter()
        .copied()
        .filter(|v| {
            !all.iter().any(|o| {
                o.id != v.id
                    && o.plan_class == v.plan_class
                    && o.beta == v.beta
                    && o.version_num > v.version_num
            })
        })
        .collect()
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
        return SitePage::xml(apps_xml(catalog));
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
    let cols = [
        StackCol::who("Platform"),
        StackCol::text("Version"),
        StackCol::text("Plan class"),
        StackCol::text("Created"),
    ];
    for app in &catalog.apps {
        let mut rows = Vec::new();
        for platform in &catalog.platforms {
            for av in current_versions(catalog, app, platform) {
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
fn apps_xml(catalog: &Catalog) -> String {
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
            for av in current_versions(catalog, app, platform) {
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
