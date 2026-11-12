//! Teams: what a team is made of, what may be stored in one, and the
//! statements that read and change them. The pages are in `site_teams.rs`.
//!
//! Follows upstream `html/inc/team.inc`, `html/inc/team_types.inc` and the
//! `team*.php` pages, and the table `team` of `db/schema.sql`
//! (`migrations/0009_teams.sql` here). What differs is listed in
//! `cloud/docs/compatibility.md`: descriptions are plain text, a name is not an
//! HTML name, and `team.nusers` is kept exactly by triggers.
//!
//! Everything a person typed (name, address, description) is stored as typed
//! after the checks below and escaped where it is written out.

use std::collections::HashMap;

use cb_db::{stmt, Db, Stmt};
use cb_proto::xml::xml_escape_text;

use crate::avg::CREDIT_HALF_LIFE;
use crate::countries::is_valid_country;
use crate::model::Team;
use crate::web::{html_escape, next_id_sql};
use crate::CoreResult;

/// `$team_types` in `html/inc/team_types.inc`. Index 0 is "none".
pub const TEAM_TYPES: [&str; 13] = [
    "",
    "Other",
    "Company",
    "Primary school",
    "Secondary school",
    "Junior college",
    "University or department",
    "Government agency",
    "Non-profit organization",
    "National",
    "Local/regional",
    "Computer type",
    "Social/political/religious",
];

/// `team_type_name`: "None" for 0 and for a number outside the list.
pub fn team_type_name(n: i64) -> &'static str {
    if n > 0 && (n as usize) < TEAM_TYPES.len() {
        TEAM_TYPES[n as usize]
    } else {
        "None"
    }
}

/// Longest name: the column of `db/schema.sql` (`varchar(191)`).
pub const MAX_NAME: usize = 191;
/// Longest web address: the column of `db/schema.sql` (`varchar(254)`).
pub const MAX_URL: usize = 254;
/// Longest description, in characters. Upstream's column is a 64 KB blob.
pub const MAX_DESCRIPTION: usize = 4000;
/// Rows of one page of `top_teams.php` and the member lists
/// (`$teams_per_page` default and `$n` in `html/inc/team.inc`).
pub const PAGE_SIZE: usize = 20;
/// Deepest offset of a member list: the first 1000 members, as
/// `html/user/team_members.php`.
pub const MAX_MEMBER_OFFSET: usize = 1000;
/// Deepest offset of `top_teams.php`. Upstream allows 10000 (`ITEM_LIMIT`).
pub const MAX_TOP_OFFSET: usize = 2000;
/// Teams the search and `team_lookup.php` return at most.
pub const MAX_SEARCH_RESULTS: usize = 100;
/// Columns a list of teams reads.
pub const LIST_COLUMNS: &str = "id, create_time, userid, name, url, type, description, nusers, \
     country, total_credit, expavg_credit, expavg_time, joinable, recent_key";

/// Why a team's fields were refused, for the page or reply that says so.
pub type Refusal = &'static str;

/// The fields of a team a person gives when creating or editing one, checked.
#[derive(Debug, Clone, PartialEq)]
pub struct TeamInput {
    pub name: String,
    pub url: String,
    pub description: String,
    pub team_type: i64,
    pub country: String,
    pub joinable: bool,
}

/// A team name: not blank, at most [`MAX_NAME`] characters, one line, no `<`
/// or `>` (upstream strips tags with `sanitize_tags`; this refuses them rather
/// than store text that was not typed).
pub fn clean_name(raw: &str) -> Result<String, Refusal> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("You must choose a non-blank team name.");
    }
    if name.chars().count() > MAX_NAME {
        return Err("The team name is too long.");
    }
    if name.chars().any(|c| c.is_control() || c == '<' || c == '>') {
        return Err("A team name may not have special characters, or < or >.");
    }
    Ok(name.to_string())
}

/// The web address of a team: empty, or an `http://` or `https://` address, or
/// one without a scheme (shown as `https://`). Upstream strips tags and shows
/// whatever is left as a link; here the scheme must be http or https, the host
/// letters, digits, dots and hyphens, an optional port, and the rest printable
/// ASCII without quotes or angle brackets, so no `javascript:`, `data:` or
/// protocol-relative address gets through.
pub fn clean_url(raw: &str) -> Result<String, Refusal> {
    let url = raw.trim();
    if url.is_empty() {
        return Ok(String::new());
    }
    const BAD: Refusal =
        "The team's web address is not valid. Use http:// or https:// and a host name.";
    if url.chars().count() > MAX_URL {
        return Err("The team's web address is too long.");
    }
    if !url
        .bytes()
        .all(|b| (0x21..=0x7e).contains(&b) && !b"\"'<>\\`^{}|".contains(&b))
    {
        return Err(BAD);
    }
    let lower = url.to_ascii_lowercase();
    let rest = if lower.starts_with("https://") {
        &url[8..]
    } else if lower.starts_with("http://") {
        &url[7..]
    } else {
        url
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (authority, None),
    };
    if let Some(p) = port {
        if p.is_empty() || p.len() > 5 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return Err(BAD);
        }
    }
    if host.is_empty()
        || host.starts_with(['.', '-'])
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return Err(BAD);
    }
    Ok(url.to_string())
}

/// The `href` for a stored address: as stored when it has a scheme, with
/// `https://` in front when it has none (upstream's `display_team_page`).
pub fn team_href(url: &str) -> String {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") {
        url.to_string()
    } else {
        format!("https://{url}")
    }
}

/// The description: plain text of at most [`MAX_DESCRIPTION`] characters, line
/// ends as `\n`. Tabs and line breaks are kept; other control characters are
/// refused.
pub fn clean_description(raw: &str) -> Result<String, Refusal> {
    let text = raw.replace("\r\n", "\n").replace('\r', "\n");
    let text = text.trim();
    if text.chars().count() > MAX_DESCRIPTION {
        return Err("The description is too long.");
    }
    if text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err("The description has characters that are not text.");
    }
    Ok(text.to_string())
}

/// All the fields of a team form, or the first refusal. An empty country is
/// "International" (`team_create_action.php`); a country that is not on the list
/// is refused, as `team_edit_action.php` does.
pub fn check_team_input(get: &dyn Fn(&str) -> String) -> Result<TeamInput, Refusal> {
    let name = clean_name(&get("name"))?;
    let url = clean_url(&get("url"))?;
    let description = clean_description(&get("description"))?;
    let type_text = get("type");
    let team_type = if type_text.trim().is_empty() {
        0
    } else {
        match type_text.trim().parse::<i64>() {
            Ok(n) if (0..TEAM_TYPES.len() as i64).contains(&n) => n,
            _ => return Err("Bad team type."),
        }
    };
    let mut country = get("country").trim().to_string();
    if country.is_empty() {
        country = "International".into();
    }
    if !is_valid_country(&country) {
        return Err("Bad country.");
    }
    Ok(TeamInput {
        name,
        url,
        description,
        team_type,
        country,
        joinable: !get("joinable").is_empty(),
    })
}

/// A description as paragraphs: blank lines separate them, single line ends
/// are kept. Escaped.
pub fn description_html(text: &str) -> String {
    let mut out = String::new();
    for para in text.split("\n\n").map(str::trim).filter(|p| !p.is_empty()) {
        out.push_str("<p>");
        out.push_str(&html_escape(para).replace('\n', "<br>\n"));
        out.push_str("</p>\n");
    }
    out
}

/// A team as `show_team_xml` in `html/inc/xml.inc` writes it. The text is
/// escaped (upstream escapes the name and the address); the elements and their
/// order are upstream's.
pub fn team_xml(t: &Team) -> String {
    format!(
        "<team>\n    <id>{}</id>\n    <create_time>{}</create_time>\n    <userid>{}</userid>\n    \
         <name>{}</name>\n    <url>{}</url>\n    <type>{}</type>\n    <country>{}</country>\n    \
         <total_credit>{}</total_credit>\n    <expavg_credit>{}</expavg_credit>\n    \
         <expavg_time>{}</expavg_time>\n</team>\n",
        t.id,
        t.create_time,
        t.userid,
        xml_escape_text(&t.name),
        xml_escape_text(&t.url),
        t.team_type,
        xml_escape_text(&t.country),
        t.total_credit,
        t.expavg_credit,
        t.expavg_time
    )
}

/// The pattern a `LIKE ... ESCAPE '\'` needs to match `s` as a substring.
pub fn like_contains(s: &str) -> String {
    let mut out = String::from("%");
    for c in s.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// What `team_search.php` asks for.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Search {
    pub keywords: String,
    pub country: String,
    pub team_type: i64,
    /// Only teams whose recent average is above 0.1 now.
    pub active: bool,
}

/// `search` in `html/user/team_search.php`. Each criterion is a list of at
/// most 20 teams, best recent average first, and a team's score is the sum of
/// the weights of the lists it is in: 20 for the name, 5 for a name that begins
/// with the words, 3 for the words in the name or description, 2 for the type,
/// 1 for the country. Upstream's word match is a MySQL full-text `MATCH`; here
/// it is a substring match on the whole phrase. One round trip, at most five
/// statements.
pub async fn search_teams(db: &dyn Db, s: &Search, now: i64) -> CoreResult<Vec<Team>> {
    // Active: the recent average now is above 0.1, which is
    // `ln(0.1) + now * ln 2 / half life` in the key's terms.
    let active_clause = if s.active { " AND recent_key > ?" } else { "" };
    let threshold = 0.1f64.ln() + now as f64 * std::f64::consts::LN_2 / CREDIT_HALF_LIFE;
    let finish = |mut st: Stmt| {
        if s.active {
            st = st.bind(threshold);
        }
        st
    };
    let select = |cond: &str| {
        format!(
            "SELECT {LIST_COLUMNS} FROM team WHERE {cond}{active_clause} \
             ORDER BY recent_key DESC, id LIMIT 20"
        )
    };
    let mut weights: Vec<i64> = Vec::new();
    let mut stmts: Vec<Stmt> = Vec::new();
    let keywords = s.keywords.trim().to_lowercase();
    if !keywords.is_empty() {
        weights.push(20);
        stmts.push(finish(stmt!(select("name_lc = ?"), keywords.as_str())));
        weights.push(5);
        stmts.push(finish(stmt!(
            select("name_lc >= ? AND name_lc < ?"),
            keywords.as_str(),
            format!("{keywords}\u{10ffff}")
        )));
        weights.push(3);
        let pat = like_contains(&keywords);
        stmts.push(finish(stmt!(
            select("(name_lc LIKE ? ESCAPE '\\' OR lower(description) LIKE ? ESCAPE '\\')"),
            pat.as_str(),
            pat.as_str()
        )));
    }
    if !s.country.is_empty() && s.country != "None" {
        weights.push(1);
        stmts.push(finish(stmt!(select("country = ?"), s.country.as_str())));
    }
    if s.team_type > 0 {
        weights.push(2);
        stmts.push(finish(stmt!(select("type = ?"), s.team_type)));
    }
    if stmts.is_empty() {
        weights.push(0);
        stmts.push(finish(stmt!(select("id > 0"))));
    }
    let results = db.batch(stmts).await?;
    let mut found: HashMap<i64, (Team, i64)> = HashMap::new();
    for (rows, weight) in results.into_iter().zip(weights) {
        for t in rows.decode::<Team>()? {
            found
                .entry(t.id)
                .and_modify(|e| e.1 += weight)
                .or_insert((t, weight));
        }
    }
    let mut list: Vec<(Team, i64)> = found.into_values().collect();
    list.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then(b.0.recent_key.total_cmp(&a.0.recent_key))
            .then(a.0.id.cmp(&b.0.id))
    });
    list.truncate(MAX_SEARCH_RESULTS);
    Ok(list.into_iter().map(|(t, _)| t).collect())
}

/// The statement that makes a team (`make_team` in `html/inc/team.inc`) and
/// the one that puts the founder in it (`user_join_team`). They run in one
/// batch. The team is made only when the account is still the one read, in no
/// team, and not blocked; the second statement changes the account only if the
/// first made a team (`changes()`), and `INSERT OR IGNORE` leaves a taken name
/// as no row, which the caller sees as an empty `RETURNING`. The trigger of
/// `0009_teams.sql` counts the founder as a member.
pub fn create_statements(user_id: i64, authenticator: &str, t: &TeamInput, now: i64) -> Vec<Stmt> {
    let name_lc = t.name.to_lowercase();
    vec![
        Stmt {
            sql: format!(
                "INSERT OR IGNORE INTO team (id, create_time, userid, name, name_lc, url, type, \
                 description, nusers, country, expavg_time, joinable) \
                 SELECT {}, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?, ? \
                 WHERE EXISTS (SELECT 1 FROM user WHERE id = ? AND authenticator = ? \
                 AND teamid = 0 AND blocked = 0) RETURNING id",
                next_id_sql("team")
            ),
            params: vec![
                now.into(),
                user_id.into(),
                t.name.as_str().into(),
                name_lc.as_str().into(),
                t.url.as_str().into(),
                t.team_type.into(),
                t.description.as_str().into(),
                t.country.as_str().into(),
                now.into(),
                t.joinable.into(),
                user_id.into(),
                authenticator.into(),
            ],
        },
        stmt!(
            "UPDATE user SET teamid = (SELECT id FROM team WHERE name_lc = ? AND userid = ?) \
             WHERE id = ? AND teamid = 0 AND changes() > 0",
            name_lc.as_str(),
            user_id,
            user_id
        ),
    ]
}

/// The statement that joins an account to a team (`user_join_team`): only to a
/// team that takes new members, and only while the account still has the key
/// that was read. `RETURNING` is empty when it did nothing. It replaces any
/// team the account was in; the trigger moves the counts.
pub fn join_statement(user_id: i64, authenticator: &str, team_id: i64) -> Stmt {
    stmt!(
        "UPDATE user SET teamid = ? WHERE id = ? AND authenticator = ? AND blocked = 0 \
         AND EXISTS (SELECT 1 FROM team WHERE id = ? AND joinable = 1) RETURNING id",
        team_id,
        user_id,
        authenticator,
        team_id
    )
}

/// The statement that takes an account out of the team it is in
/// (`user_quit_team`). Names the team it expects, so a second request does not
/// quit a team joined since.
pub fn quit_statement(user_id: i64, authenticator: &str, team_id: i64) -> Stmt {
    stmt!(
        "UPDATE user SET teamid = 0 WHERE id = ? AND authenticator = ? AND teamid = ? RETURNING id",
        user_id,
        authenticator,
        team_id
    )
}

/// One team by ID, with its founder's name.
pub async fn team_by_id(db: &dyn Db, id: i64) -> CoreResult<Option<Team>> {
    Ok(db
        .run(stmt!(
            "SELECT t.*, u.name AS founder_name FROM team t LEFT JOIN user u ON u.id = t.userid \
             WHERE t.id = ?",
            id
        ))
        .await?
        .first()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_plain_one_line_text() {
        assert_eq!(clean_name("  Team A  ").unwrap(), "Team A");
        assert!(clean_name("R&D <3").unwrap_err().contains("< or >"));
        assert!(clean_name("   ").is_err());
        assert!(clean_name("a\nb").is_err());
        assert!(clean_name(&"x".repeat(MAX_NAME)).is_ok());
        assert!(clean_name(&"x".repeat(MAX_NAME + 1)).is_err());
        assert_eq!(clean_name("Café 日本").unwrap(), "Café 日本");
    }

    #[test]
    fn urls_are_http_or_https_or_bare() {
        for ok in [
            "",
            "example.org",
            "http://example.org",
            "HTTPS://Example.org/a/b?c=d&e=f#g",
            "example.org:8080/x",
            "localhost",
            "http://a-b.example.org/%20x",
        ] {
            assert!(clean_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "data:text/html;base64,AAAA",
            "//evil.example",
            "ftp://example.org",
            "http://",
            "https:///x",
            "http://user@example.org",
            "example.org:abc",
            "example.org:",
            "exa mple.org",
            "example.org/\"onmouseover=",
            "example.org/<script>",
            "vbscript:x",
            "http://-bad.example",
            "http://exämple.org",
        ] {
            assert!(clean_url(bad).is_err(), "{bad}");
        }
        assert!(clean_url(&format!("a.org/{}", "x".repeat(MAX_URL))).is_err());
        assert_eq!(team_href("example.org"), "https://example.org");
        assert_eq!(team_href("http://example.org"), "http://example.org");
    }

    #[test]
    fn descriptions_are_text() {
        assert_eq!(clean_description("a\r\nb\r\n\r\nc").unwrap(), "a\nb\n\nc");
        assert!(clean_description("a\u{0}b").is_err());
        assert!(clean_description(&"x".repeat(MAX_DESCRIPTION + 1)).is_err());
        assert_eq!(
            description_html("one <b>&\ntwo\n\nthree"),
            "<p>one &lt;b&gt;&amp;<br>\ntwo</p>\n<p>three</p>\n"
        );
    }

    #[test]
    fn a_form_is_checked_as_a_whole() {
        let form = |pairs: &'static [(&str, &str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
                    .unwrap_or_default()
            }
        };
        let t = check_team_input(&form(&[("name", "A"), ("type", "3")])).unwrap();
        assert_eq!(
            (t.team_type, t.country.as_str(), t.joinable),
            (3, "International", false)
        );
        assert!(check_team_input(&form(&[("name", "A"), ("type", "13")])).is_err());
        assert!(check_team_input(&form(&[("name", "A"), ("type", "x")])).is_err());
        assert!(check_team_input(&form(&[("name", "A"), ("country", "Atlantis")])).is_err());
        assert!(check_team_input(&form(&[("name", "")])).is_err());
        let t = check_team_input(&form(&[("name", "A"), ("joinable", "on")])).unwrap();
        assert!(t.joinable);
    }

    #[test]
    fn like_patterns_escape_wildcards() {
        assert_eq!(like_contains("a_b%c\\"), "%a\\_b\\%c\\\\%");
    }

    #[test]
    fn type_names_follow_upstream() {
        assert_eq!(team_type_name(0), "None");
        assert_eq!(team_type_name(1), "Other");
        assert_eq!(team_type_name(12), "Social/political/religious");
        assert_eq!(team_type_name(13), "None");
        assert_eq!(team_type_name(-1), "None");
    }

    #[test]
    fn xml_has_upstream_elements_and_escapes() {
        let t = Team {
            id: 7,
            create_time: 5,
            userid: 2,
            name: "A & <B>".into(),
            url: "x.org/?a=1&b=2".into(),
            team_type: 3,
            country: "Canada".into(),
            total_credit: 12.5,
            expavg_credit: 1.0,
            expavg_time: 99.0,
            ..Default::default()
        };
        let x = team_xml(&t);
        assert_eq!(
            x,
            "<team>\n    <id>7</id>\n    <create_time>5</create_time>\n    <userid>2</userid>\n    \
             <name>A &amp; &lt;B></name>\n    <url>x.org/?a=1&amp;b=2</url>\n    <type>3</type>\n    \
             <country>Canada</country>\n    <total_credit>12.5</total_credit>\n    \
             <expavg_credit>1</expavg_credit>\n    <expavg_time>99</expavg_time>\n</team>\n"
        );
    }
}
