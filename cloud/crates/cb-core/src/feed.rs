//! Project panels: figures a project supplies as a JSON document, shown as
//! cards on `work.php`. The schema is in `cloud/docs/dashboard-feed.md`.
//!
//! The document comes from outside, so it is validated field by field and
//! every length and count is bounded; an unknown field, a wrong type or a
//! value out of range refuses the whole document (the page then says the
//! panels are not available). Nothing from the document reaches the page
//! unescaped, links must be `https`, and a figure the document does not give a
//! value for is shown as "unknown", never as 0.
//!
//! Fetching is the Worker's job (`cloud/workers/web/src/lib.rs`): it passes the
//! text here with the size cap [`MAX_FEED_BYTES`] already applied.

use serde::Deserialize;

use crate::charts::{Axis, Chart, Kind, Series};
use crate::site::number_format;
use crate::site_ui::{badge, card, notice, stat, stats, Tone};
use crate::web::html_escape;

/// Most bytes of the document the Worker reads.
pub const MAX_FEED_BYTES: usize = 64 * 1024;
/// The only schema version this build reads.
pub const FEED_SCHEMA: u32 = 1;
pub const MAX_SECTIONS: usize = 8;
pub const MAX_FIGURES: usize = 12;
pub const MAX_SERIES: usize = 3;
pub const MAX_POINTS: usize = 200;
const MAX_TITLE: usize = 80;
const MAX_LABEL: usize = 60;
const MAX_VALUE_TEXT: usize = 40;
const MAX_UNIT: usize = 24;
const MAX_NOTE: usize = 160;
const MAX_URL: usize = 300;
/// A section whose `updated` is older than this is stale unless it says
/// otherwise with `stale_after_secs`.
const DEFAULT_STALE_SECS: i64 = 3600;
const MAX_STALE_SECS: i64 = 365 * 86_400;
/// Series spacing bounds, seconds.
const MIN_INTERVAL: i64 = 60;
const MAX_INTERVAL: i64 = 31 * 86_400;
/// How far ahead of the clock an `updated` may be before it is refused.
const FUTURE_SLACK: i64 = 86_400;
const MAX_ABS_VALUE: f64 = 1e18;
/// Nothing the feed dates is older than September 2001.
const MIN_TIME: i64 = 1_000_000_000;

#[derive(Debug, Clone, PartialEq)]
pub struct FeedError(pub String);

impl std::fmt::Display for FeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn bad<T>(msg: impl Into<String>) -> Result<T, FeedError> {
    Err(FeedError(msg.into()))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Feed {
    pub sections: Vec<Section>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    pub title: String,
    /// When the figures were made, Unix seconds; the feed's `updated` when the
    /// section has none.
    pub updated: Option<i64>,
    pub stale_after: i64,
    pub link: Option<Link>,
    pub figures: Vec<Figure>,
    pub series: Vec<Line>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub label: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Number(f64),
    Text(String),
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Figure {
    pub label: String,
    pub value: Value,
    pub unit: String,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub title: String,
    pub unit: String,
    pub kind: Kind,
    pub start: i64,
    pub interval: i64,
    pub values: Vec<Option<f64>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFeed {
    schema: u32,
    #[serde(default)]
    updated: Option<i64>,
    sections: Vec<RawSection>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSection {
    title: String,
    #[serde(default)]
    updated: Option<i64>,
    #[serde(default)]
    stale_after_secs: Option<i64>,
    #[serde(default)]
    link: Option<RawLink>,
    #[serde(default)]
    figures: Vec<RawFigure>,
    #[serde(default)]
    series: Vec<RawSeries>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLink {
    label: String,
    url: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFigure {
    label: String,
    #[serde(default)]
    value: Option<serde_json::Value>,
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSeries {
    title: String,
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    start: i64,
    interval_secs: i64,
    values: Vec<Option<f64>>,
}

/// Plain text of at most `max` characters, with no control characters.
fn text(s: &str, what: &str, max: usize, required: bool) -> Result<String, FeedError> {
    let s = s.trim();
    if required && s.is_empty() {
        return bad(format!("{what} is empty"));
    }
    if s.chars().count() > max {
        return bad(format!("{what} is longer than {max} characters"));
    }
    if s.chars().any(char::is_control) {
        return bad(format!("{what} has a control character"));
    }
    Ok(s.to_string())
}

/// Whether `url` is an `https` address a link may point to: a host, no
/// credentials, no spaces, quotes or angle brackets.
pub fn valid_https_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    if url.len() > MAX_URL
        || url
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "\"'<>\\`".contains(c))
    {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !authority.is_empty()
        && !authority.contains('@')
        && authority
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
}

fn timestamp(t: Option<i64>, what: &str, now: i64) -> Result<Option<i64>, FeedError> {
    match t {
        None => Ok(None),
        Some(t) if t >= MIN_TIME && t <= now + FUTURE_SLACK => Ok(Some(t)),
        Some(_) => bad(format!("{what} is not a recent Unix time")),
    }
}

/// Reads and validates a document. `now` is the time the page is built.
pub fn parse_feed(body: &str, now: i64) -> Result<Feed, FeedError> {
    if body.len() > MAX_FEED_BYTES {
        return bad("the document is too large");
    }
    let raw: RawFeed = serde_json::from_str(body).map_err(|e| FeedError(e.to_string()))?;
    if raw.schema != FEED_SCHEMA {
        return bad(format!("schema {} is not supported", raw.schema));
    }
    if raw.sections.len() > MAX_SECTIONS {
        return bad(format!("more than {MAX_SECTIONS} sections"));
    }
    let feed_updated = timestamp(raw.updated, "updated", now)?;
    let mut sections = Vec::new();
    for s in raw.sections {
        let title = text(&s.title, "a section title", MAX_TITLE, true)?;
        if s.figures.len() > MAX_FIGURES {
            return bad(format!("more than {MAX_FIGURES} figures in a section"));
        }
        if s.series.len() > MAX_SERIES {
            return bad(format!("more than {MAX_SERIES} series in a section"));
        }
        let updated = timestamp(s.updated, "a section's updated", now)?.or(feed_updated);
        let stale_after = match s.stale_after_secs {
            None => DEFAULT_STALE_SECS,
            Some(n) if (60..=MAX_STALE_SECS).contains(&n) => n,
            Some(_) => return bad("stale_after_secs is out of range"),
        };
        let link = match s.link {
            None => None,
            Some(l) => {
                if !valid_https_url(&l.url) {
                    return bad("a link is not an https address");
                }
                Some(Link {
                    label: text(&l.label, "a link label", MAX_LABEL, true)?,
                    url: l.url,
                })
            }
        };
        let mut figures = Vec::new();
        for f in s.figures {
            let value = match f.value {
                None | Some(serde_json::Value::Null) => Value::Unknown,
                Some(serde_json::Value::Number(n)) => match n.as_f64() {
                    Some(v) if v.is_finite() && v.abs() <= MAX_ABS_VALUE => Value::Number(v),
                    _ => return bad("a figure value is out of range"),
                },
                Some(serde_json::Value::String(t)) => {
                    let t = text(&t, "a figure value", MAX_VALUE_TEXT, false)?;
                    if t.is_empty() {
                        Value::Unknown
                    } else {
                        Value::Text(t)
                    }
                }
                Some(_) => return bad("a figure value is not a number, a string or null"),
            };
            figures.push(Figure {
                label: text(&f.label, "a figure label", MAX_LABEL, true)?,
                value,
                unit: text(f.unit.as_deref().unwrap_or(""), "a unit", MAX_UNIT, false)?,
                note: text(f.note.as_deref().unwrap_or(""), "a note", MAX_NOTE, false)?,
            });
        }
        let mut series = Vec::new();
        for r in s.series {
            if r.values.is_empty() || r.values.len() > MAX_POINTS {
                return bad(format!("a series needs 1 to {MAX_POINTS} values"));
            }
            if !(MIN_INTERVAL..=MAX_INTERVAL).contains(&r.interval_secs) {
                return bad("interval_secs is out of range");
            }
            // The time of the last value, by checked arithmetic: `start` comes
            // from outside.
            let last = r
                .interval_secs
                .checked_mul(r.values.len() as i64 - 1)
                .and_then(|d| r.start.checked_add(d));
            if r.start < MIN_TIME || !last.is_some_and(|t| t <= now + FUTURE_SLACK) {
                return bad("a series does not end in the past");
            }
            if r.values
                .iter()
                .flatten()
                .any(|v| !v.is_finite() || v.abs() > MAX_ABS_VALUE)
            {
                return bad("a series value is out of range");
            }
            // The charts draw from zero upward; a negative value would be drawn
            // as something else than the table says.
            if r.values.iter().flatten().any(|v| *v < 0.0) {
                return bad("a series value is negative");
            }
            let kind = match r.kind.as_deref() {
                None | Some("line") => Kind::Lines,
                Some("bars") => Kind::Bars,
                Some("area") => Kind::Area,
                Some(_) => return bad("a series kind is not line, bars or area"),
            };
            series.push(Line {
                title: text(&r.title, "a series title", MAX_TITLE, true)?,
                unit: text(r.unit.as_deref().unwrap_or(""), "a unit", MAX_UNIT, false)?,
                kind,
                start: r.start,
                interval: r.interval_secs,
                values: r.values,
            });
        }
        sections.push(Section {
            title,
            updated,
            stale_after,
            link,
            figures,
            series,
        });
    }
    Ok(Feed { sections })
}

/// `5 min ago`, `3 hours ago`, `2 days ago`.
pub fn age_text(secs: i64) -> String {
    let s = secs.max(0);
    if s < 90 {
        "just now".into()
    } else if s < 5400 {
        format!("{} min ago", (s + 30) / 60)
    } else if s < 2 * 86_400 {
        format!("{} hours ago", (s + 1800) / 3600)
    } else {
        format!("{} days ago", (s + 43_200) / 86_400)
    }
}

/// A number the way a figure shows it: whole numbers with separators, others
/// to at most three decimals.
pub fn format_value(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return number_format(v, 0);
    }
    let decimals = if v.abs() >= 100.0 {
        1
    } else if v.abs() >= 0.1 {
        2
    } else {
        3
    };
    let s = number_format(v, decimals);
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

fn staleness(updated: Option<i64>, stale_after: i64, now: i64) -> String {
    match updated {
        None => badge("update time unknown", Tone::Neutral),
        Some(t) => {
            let age = (now - t).max(0);
            if age <= stale_after {
                badge(&format!("updated {}", age_text(age)), Tone::Good)
            } else {
                badge(&format!("stale, updated {}", age_text(age)), Tone::Warning)
            }
        }
    }
}

fn render_section(s: &Section, now: i64, index: usize) -> String {
    let mut body = String::new();
    let mut head = staleness(s.updated, s.stale_after, now);
    if let Some(l) = &s.link {
        head.push_str(&format!(
            " <a href=\"{}\" rel=\"noopener noreferrer\">{}</a>",
            html_escape(&l.url),
            html_escape(&l.label)
        ));
    }
    body.push_str(&format!("<p>{head}</p>\n"));
    if !s.figures.is_empty() {
        let items: Vec<String> = s
            .figures
            .iter()
            .map(|f| {
                let value = match &f.value {
                    Value::Number(v) => html_escape(&format_value(*v)),
                    Value::Text(t) => html_escape(t),
                    Value::Unknown => "<span class=\"muted\">unknown</span>".to_string(),
                };
                let note = [f.unit.as_str(), f.note.as_str()]
                    .into_iter()
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
                    .join(" \u{b7} ");
                stat(&f.label, &value, &note)
            })
            .collect();
        body.push_str(&stats(&items));
    }
    for (i, line) in s.series.iter().enumerate() {
        let times: Vec<i64> = (0..line.values.len() as i64)
            .map(|k| line.start + k * line.interval)
            .collect();
        let series = [Series {
            label: &line.title,
            values: &line.values,
        }];
        let chart = Chart {
            title: &line.title,
            description: &format!("{} over time", line.title),
            kind: line.kind,
            axis: if line.interval >= 86_400 {
                Axis::Dates
            } else {
                Axis::Midnights
            },
            times: &times,
            series: &series,
            last_partial: false,
        };
        body.push_str(&format!("<h3>{}</h3>\n", html_escape(&line.title)));
        body.push_str(&chart.svg(&format!("feed{index}-{i}")));
        body.push_str(&chart.data_details(&line.unit));
    }
    card(&s.title, &body)
}

/// The cards for a feed, or a note that there are none. `now` decides which
/// badges say "stale".
pub fn render_feed(feed: &Feed, now: i64) -> String {
    if feed.sections.is_empty() {
        return notice("The project supplied no panels.");
    }
    feed.sections
        .iter()
        .enumerate()
        .map(|(i, s)| render_section(s, now, i))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn ok(body: &str) -> Feed {
        parse_feed(body, NOW).unwrap_or_else(|e| panic!("{e}: {body}"))
    }

    fn refused(body: &str) -> String {
        parse_feed(body, NOW).unwrap_err().0
    }

    #[test]
    fn a_full_document_parses() {
        let feed = ok(&format!(
            r#"{{"schema":1,"updated":{u},"sections":[{{"title":"Corpus",
            "link":{{"label":"Full dashboard","url":"https://example.org/dash"}},
            "figures":[{{"label":"Pages","value":12345,"unit":"pages","note":"since June"}},
                       {{"label":"Accuracy","value":null}},
                       {{"label":"Mode","value":"slow"}},
                       {{"label":"Ratio","value":0.4567}}],
            "series":[{{"title":"Per day","unit":"pages","kind":"bars","start":{s},"interval_secs":86400,
                        "values":[1,2,null,4]}}]}}]}}"#,
            u = NOW - 60,
            s = NOW - 5 * 86_400
        ));
        let s = &feed.sections[0];
        assert_eq!(s.updated, Some(NOW - 60));
        assert_eq!(s.figures[0].value, Value::Number(12345.0));
        assert_eq!(s.figures[1].value, Value::Unknown);
        assert_eq!(s.figures[2].value, Value::Text("slow".into()));
        assert_eq!(
            s.series[0].values,
            vec![Some(1.0), Some(2.0), None, Some(4.0)]
        );
        assert_eq!(s.series[0].kind, Kind::Bars);
    }

    #[test]
    fn unknown_is_never_zero() {
        let feed = ok(r#"{"schema":1,"sections":[{"title":"T","figures":[
            {"label":"A","value":null},{"label":"B"},{"label":"C","value":""},{"label":"D","value":0}]}]}"#);
        let html = render_feed(&feed, NOW);
        assert_eq!(html.matches("muted\">unknown<").count(), 3);
        // The real zero is shown as 0.
        assert!(html.contains("<dd>0</dd>"));
        assert!(html.contains("update time unknown"));
    }

    #[test]
    fn strict_validation_refuses_the_document() {
        for (body, why) in [
            (r#"{"schema":2,"sections":[]}"#, "schema"),
            (r#"{"schema":1,"sections":[],"extra":1}"#, "unknown field"),
            (r#"{"schema":1}"#, "missing sections"),
            (r#"{"schema":1,"sections":[{"title":""}]}"#, "empty title"),
            (
                r#"{"schema":1,"sections":[{"title":"T","link":{"label":"x","url":"http://example.org/"}}]}"#,
                "http link",
            ),
            (
                r#"{"schema":1,"sections":[{"title":"T","link":{"label":"x","url":"javascript:alert(1)"}}]}"#,
                "script link",
            ),
            (
                r#"{"schema":1,"sections":[{"title":"T","link":{"label":"x","url":"https://user:pw@example.org/"}}]}"#,
                "credentials in link",
            ),
            (
                r#"{"schema":1,"sections":[{"title":"T","figures":[{"label":"A","value":[1]}]}]}"#,
                "array value",
            ),
            (
                r#"{"schema":1,"sections":[{"title":"T","figures":[{"label":"A","value":1e30}]}]}"#,
                "huge value",
            ),
            (
                r#"{"schema":1,"sections":[{"title":"T","figures":[{"label":"A","extra":1}]}]}"#,
                "unknown figure field",
            ),
            (
                r#"{"schema":1,"updated":5,"sections":[]}"#,
                "ancient updated",
            ),
            (
                r#"{"schema":1,"sections":[{"title":"T","series":[{"title":"S","start":9223372036854775807,"interval_secs":86400,"values":[1,2]}]}]}"#,
                "start near i64::MAX",
            ),
            (
                r#"{"schema":1,"sections":[{"title":"T","series":[{"title":"S","start":1700000000,"interval_secs":86400,"values":[-5,5]}]}]}"#,
                "negative series value",
            ),
            (
                r#"{"schema":1,"sections":[{"title":"T","series":[{"title":"S","start":1700000000,"interval_secs":5,"values":[1]}]}]}"#,
                "interval too short",
            ),
            (
                r#"{"schema":1,"sections":[{"title":"T","series":[{"title":"S","start":1700000000,"interval_secs":60,"values":[]}]}]}"#,
                "no values",
            ),
            ("not json", "garbage"),
        ] {
            assert!(parse_feed(body, NOW).is_err(), "{why} was accepted");
        }
        // Counts and lengths are bounded.
        let long = "x".repeat(MAX_LABEL + 1);
        assert!(refused(&format!(
            r#"{{"schema":1,"sections":[{{"title":"T","figures":[{{"label":"{long}"}}]}}]}}"#
        ))
        .contains("longer"));
        let many = [r#"{"title":"T"}"#; MAX_SECTIONS + 1].join(",");
        assert!(refused(&format!(r#"{{"schema":1,"sections":[{many}]}}"#)).contains("sections"));
        let big = format!(
            r#"{{"schema":1,"sections":[{{"title":"{}"}}]}}"#,
            "x".repeat(MAX_FEED_BYTES)
        );
        assert!(refused(&big).contains("too large"));
        assert!(refused(r#"{"schema":1,"sections":[{"title":"a\u0007b"}]}"#).contains("control"));
    }

    #[test]
    fn everything_is_escaped() {
        let feed = ok(
            r#"{"schema":1,"updated":1799999999,"sections":[{"title":"<img src=x onerror=alert(1)>",
            "link":{"label":"<b>go</b>","url":"https://example.org/?a=1&b=2"},
            "figures":[{"label":"<script>","value":"<i>","unit":"<u>","note":"\"quoted\" & <more>"}]}]}"#,
        );
        let html = render_feed(&feed, NOW);
        assert!(!html.contains("<img"));
        assert!(!html.contains("<script"));
        assert!(!html.contains("<b>go"));
        assert!(!html.contains("<i>"));
        assert!(html.contains("&lt;img src=x onerror=alert(1)&gt;"));
        assert!(html.contains("href=\"https://example.org/?a=1&amp;b=2\""));
        assert!(html.contains("&quot;quoted&quot; &amp; &lt;more&gt;"));
        assert!(html.contains("rel=\"noopener noreferrer\""));
    }

    #[test]
    fn staleness_badges() {
        let fresh = ok(&format!(
            r#"{{"schema":1,"updated":{},"sections":[{{"title":"T"}}]}}"#,
            NOW - 120
        ));
        assert!(render_feed(&fresh, NOW).contains("badge ok\">updated 2 min ago"));
        let old = ok(&format!(
            r#"{{"schema":1,"updated":{},"sections":[{{"title":"T"}}]}}"#,
            NOW - 3 * 86_400
        ));
        assert!(render_feed(&old, NOW).contains("badge warn\">stale, updated 3 days ago"));
        // A section can allow more time.
        let slow = ok(&format!(
            r#"{{"schema":1,"updated":{},"sections":[{{"title":"T","stale_after_secs":604800}}]}}"#,
            NOW - 3 * 86_400
        ));
        assert!(render_feed(&slow, NOW).contains("badge ok"));
    }

    #[test]
    fn a_single_sample_may_have_a_long_interval() {
        // The last (only) sample is at `start`; one interval beyond it does not count.
        let start = NOW - 86_400;
        assert!(parse_feed(
            &format!(
                r#"{{"schema":1,"sections":[{{"title":"T","series":[{{"title":"S","start":{start},"interval_secs":2678400,"values":[3]}}]}}]}}"#
            ),
            NOW
        )
        .is_ok());
    }

    #[test]
    fn series_render_as_charts_with_a_table() {
        let feed = ok(&format!(
            r#"{{"schema":1,"sections":[{{"title":"T","series":[{{"title":"Per day","unit":"pages",
            "start":{},"interval_secs":86400,"values":[1,null,3]}}]}}]}}"#,
            NOW - 4 * 86_400
        ));
        let html = render_feed(&feed, NOW);
        assert!(html.contains("<svg class=\"chart\""));
        assert!(html.contains("<details>"));
        assert!(html.contains("unknown"));
    }

    #[test]
    fn url_rules() {
        assert!(valid_https_url("https://example.org"));
        assert!(valid_https_url("https://sub.example.org:8443/a?b=c#d"));
        assert!(!valid_https_url("http://example.org"));
        assert!(!valid_https_url("https://"));
        assert!(!valid_https_url("https:///path"));
        assert!(!valid_https_url("https://a b.example"));
        assert!(!valid_https_url("https://example.org/\"onclick="));
        assert!(!valid_https_url(&format!(
            "https://e.org/{}",
            "a".repeat(400)
        )));
    }

    #[test]
    fn value_formatting() {
        assert_eq!(format_value(12345.0), "12,345");
        assert_eq!(format_value(0.0), "0");
        assert_eq!(format_value(0.4567), "0.46");
        assert_eq!(format_value(1234.56), "1,234.6");
        assert_eq!(format_value(0.00512), "0.005");
        assert_eq!(age_text(30), "just now");
        assert_eq!(age_text(3600), "60 min ago");
        assert_eq!(age_text(7200), "2 hours ago");
    }
}
