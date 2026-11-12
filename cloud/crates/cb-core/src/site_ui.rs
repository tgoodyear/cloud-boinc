//! The look of the site pages: the navigation list, the header and a few
//! builders for the blocks the pages are made of. The stylesheet is
//! `site.css`, inlined by `web::page`; the class names used here are defined
//! there and listed in `cloud/docs/architecture.md` under "Site pages".
//!
//! Text arguments (`label`, `title`, `note`, `message`) are plain text and
//! escaped here. Arguments named `html`, `value`, `body` or `cells` are HTML
//! the caller has already escaped, so that a cell can hold a link.

use crate::project::Project;
use crate::web::html_escape;

/// One entry of the site navigation.
pub struct NavItem {
    pub label: &'static str,
    pub path: &'static str,
    /// An entry of the account area: it is listed on the account page and
    /// not in the header, which is the same for every visitor.
    pub account: bool,
}

/// Every page the navigation links to, in order. The header shows the
/// entries that are not `account` ones, then a link to the account page;
/// the home page lists the same entries as links. Adding a page to the menu
/// is adding a line here.
pub const NAV: &[NavItem] = &[
    NavItem {
        label: "Jobs",
        path: "work.php",
        account: false,
    },
    NavItem {
        label: "Activity",
        path: "activity.php",
        account: false,
    },
    NavItem {
        label: "Top users",
        path: "top_users.php",
        account: false,
    },
    NavItem {
        label: "Top computers",
        path: "top_hosts.php",
        account: false,
    },
    NavItem {
        label: "Server status",
        path: "server_status.php",
        account: false,
    },
];

/// The header of every page: the project name as the home link and the
/// navigation. It scrolls sideways on a narrow screen instead of wrapping.
pub fn site_header(project: &Project) -> String {
    let mut nav = String::new();
    for item in NAV.iter().filter(|i| !i.account) {
        nav.push_str(&format!(
            "<a href=\"{}\">{}</a>",
            item.path,
            html_escape(item.label)
        ));
    }
    nav.push_str("<a href=\"home.php\">Your account</a>");
    format!(
        "<header class=\"top\"><div class=\"bar\"><a class=\"brand\" href=\"./\">{}</a>\n\
         <nav aria-label=\"Site\">{nav}</nav></div></header>\n",
        html_escape(&project.config.long_name)
    )
}

/// A titled box. `body` is HTML.
pub fn card(title: &str, body: &str) -> String {
    format!(
        "<section class=\"card\">\n<h2>{}</h2>\n{body}</section>\n",
        html_escape(title)
    )
}

/// A box with an accent edge, for the one thing a page is mostly about.
pub fn card_accent(title: &str, body: &str) -> String {
    card(title, body).replacen("class=\"card\"", "class=\"card accent\"", 1)
}

/// Boxes side by side where there is room, stacked on a phone. Each item is
/// a [`card`].
pub fn grid(cards: &[String]) -> String {
    format!("<div class=\"grid\">\n{}</div>\n", cards.concat())
}

/// A headline number. `value` is HTML (usually a formatted number); `note`
/// is small text under it, or empty.
pub fn stat(label: &str, value: &str, note: &str) -> String {
    let note = if note.is_empty() {
        String::new()
    } else {
        format!("<small>{}</small>", html_escape(note))
    };
    format!(
        "<div class=\"stat\"><dt>{}</dt><dd>{value}{note}</dd></div>\n",
        html_escape(label)
    )
}

/// A row of [`stat`] blocks.
pub fn stats(items: &[String]) -> String {
    format!("<dl class=\"stats\">\n{}</dl>\n", items.concat())
}

/// A definition list of label and value pairs. Values are HTML.
pub fn kv(rows: &[(&str, String)]) -> String {
    let mut out = String::from("<dl class=\"kv\">\n");
    for (label, value) in rows {
        out.push_str(&format!(
            "<dt>{}</dt><dd>{value}</dd>\n",
            html_escape(label)
        ));
    }
    out.push_str("</dl>\n");
    out
}

/// A column of a [`data_table`].
pub struct Col {
    pub label: &'static str,
    pub kind: ColKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ColKind {
    Text,
    /// Right-aligned, digits of equal width.
    Num,
    /// A rank: narrow, right-aligned, muted.
    Rank,
}

impl Col {
    pub const fn text(label: &'static str) -> Col {
        Col {
            label,
            kind: ColKind::Text,
        }
    }
    pub const fn num(label: &'static str) -> Col {
        Col {
            label,
            kind: ColKind::Num,
        }
    }
    pub const fn rank(label: &'static str) -> Col {
        Col {
            label,
            kind: ColKind::Rank,
        }
    }
    fn class(&self) -> &'static str {
        match self.kind {
            ColKind::Text => "",
            ColKind::Num => " class=\"num\"",
            ColKind::Rank => " class=\"rank\"",
        }
    }
}

/// A table with a header row, in the horizontal scroll box `web::page`
/// puts around every table. Cells are HTML; a row with fewer cells than
/// columns is padded.
pub fn data_table(cols: &[Col], rows: &[Vec<String>]) -> String {
    let mut out = String::from("<table>\n<tr>");
    for c in cols {
        out.push_str(&format!("<th{}>{}</th>", c.class(), html_escape(c.label)));
    }
    out.push_str("</tr>\n");
    for row in rows {
        out.push_str("<tr>");
        for (i, c) in cols.iter().enumerate() {
            let cell = row.get(i).map_or("", String::as_str);
            out.push_str(&format!("<td{}>{cell}</td>", c.class()));
        }
        out.push_str("</tr>\n");
    }
    out.push_str("</table>\n");
    out
}

/// What a [`badge`] is saying.
#[derive(Clone, Copy)]
pub enum Tone {
    Neutral,
    Good,
    Warning,
    Bad,
}

/// A short label: an application name or a state.
pub fn badge(text: &str, tone: Tone) -> String {
    let class = match tone {
        Tone::Neutral => "badge",
        Tone::Good => "badge ok",
        Tone::Warning => "badge warn",
        Tone::Bad => "badge bad",
    };
    format!("<span class=\"{class}\">{}</span>", html_escape(text))
}

/// What a list shows when it has nothing to list.
pub fn empty(message: &str) -> String {
    format!("<p class=\"empty\">{}</p>\n", html_escape(message))
}

/// A message that something went through or needs attention.
pub fn notice(message: &str) -> String {
    format!("<p class=\"notice\">{}</p>\n", html_escape(message))
}

/// A message that something went wrong.
pub fn error_note(message: &str) -> String {
    format!("<p class=\"error\">{}</p>\n", html_escape(message))
}

/// A link that looks like a button; `secondary` is the outlined variant.
pub fn button_link(label: &str, href: &str, secondary: bool) -> String {
    format!(
        "<a class=\"button{}\" href=\"{}\">{}</a>",
        if secondary { " secondary" } else { "" },
        html_escape(href),
        html_escape(label)
    )
}

/// Buttons in a row that wraps. Each item is a [`button_link`] or an input.
pub fn actions(items: &[String]) -> String {
    format!("<div class=\"actions\">{}</div>\n", items.concat())
}

/// A list of full-width links with touch-sized rows: `(label, href)` pairs.
pub fn link_list(items: &[(&str, String)]) -> String {
    let mut out = String::from("<ul class=\"links\">\n");
    for (label, href) in items {
        out.push_str(&format!(
            "<li><a href=\"{}\">{}</a></li>\n",
            html_escape(href),
            html_escape(label)
        ));
    }
    out.push_str("</ul>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_escaped_and_html_is_kept() {
        let s = stat("A <b>", "<a href=\"x\">1</a>", "n & m");
        assert!(s.contains("<dt>A &lt;b&gt;</dt>"));
        assert!(s.contains("<dd><a href=\"x\">1</a><small>n &amp; m</small></dd>"));
        assert!(!stat("A", "1", "").contains("<small>"));
        assert!(badge("a<b", Tone::Good).contains("class=\"badge ok\">a&lt;b<"));
        assert!(empty("none <yet>").contains("none &lt;yet&gt;"));
        assert!(button_link("Go", "a?x=1&y=2", true).contains("href=\"a?x=1&amp;y=2\""));
    }

    #[test]
    fn a_table_has_a_header_and_pads_short_rows() {
        let t = data_table(
            &[Col::rank("#"), Col::text("User"), Col::num("Credit <1>")],
            &[vec!["1".into(), "<a>Ada</a>".into()]],
        );
        assert!(t.contains(
            "<th class=\"rank\">#</th><th>User</th><th class=\"num\">Credit &lt;1&gt;</th>"
        ));
        assert!(t.contains("<td class=\"rank\">1</td><td><a>Ada</a></td><td class=\"num\"></td>"));
    }

    #[test]
    fn navigation_entries_are_relative_paths() {
        for item in NAV {
            assert!(!item.label.is_empty());
            assert!(item.path.ends_with(".php") && !item.path.starts_with('/'));
        }
    }
}
