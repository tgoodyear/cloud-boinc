//! Project settings: the rows of the `config` table and the catalog, as one
//! value a Worker loads and keeps for a few seconds.

use cb_crypto::PublicKey;
use cb_db::{stmt, Db, Stmt};
use serde::Deserialize;

use crate::plan_class::PlanClassSpecs;
use crate::scheduler::Catalog;
use crate::{Config, CoreError, CoreResult};

/// The `<config>` element of upstream `config.xml`.
pub const SETTING_CONFIG: &str = "config";
/// The upstream `plan_class_spec.xml` document.
pub const SETTING_PLAN_CLASSES: &str = "plan_class_spec";
/// The code-signing public key, in the BOINC key file format.
pub const SETTING_CODE_SIGN_KEY: &str = "code_sign_public_key";
/// The upload public key, in the BOINC key file format.
pub const SETTING_UPLOAD_KEY: &str = "upload_public_key";

pub const SETTINGS: [&str; 4] = [
    SETTING_CONFIG,
    SETTING_PLAN_CLASSES,
    SETTING_CODE_SIGN_KEY,
    SETTING_UPLOAD_KEY,
];

#[derive(Debug, Clone, Default)]
pub struct Project {
    pub config: Config,
    pub plan_classes: PlanClassSpecs,
    pub catalog: Catalog,
    /// Text of the code-signing public key; empty when not set.
    pub code_sign_key: String,
    pub upload_public_key: Option<PublicKey>,
}

#[derive(Deserialize)]
struct SettingRow {
    name: String,
    value: String,
}

impl Project {
    /// Two round trips: the settings, then the catalog.
    pub async fn load(db: &dyn Db) -> CoreResult<Project> {
        let rows = db.run(Stmt::new("SELECT name, value FROM config")).await?;
        let mut p = Project::default();
        for row in rows.decode::<SettingRow>()? {
            match row.name.as_str() {
                SETTING_CONFIG => p.config = Config::parse(&row.value)?,
                SETTING_PLAN_CLASSES => p.plan_classes = PlanClassSpecs::parse(&row.value)?,
                SETTING_CODE_SIGN_KEY => p.code_sign_key = row.value,
                SETTING_UPLOAD_KEY => p.upload_public_key = Some(PublicKey::parse(&row.value)?),
                _ => {}
            }
        }
        p.catalog = Catalog::load(db).await?;
        Ok(p)
    }

    /// Where the master page tells clients to send scheduler requests.
    pub fn scheduler_url(&self) -> String {
        scheduler_url(&self.config)
    }
}

pub fn scheduler_url(config: &Config) -> String {
    config.scheduler_url()
}

/// Checks that a setting parses and is acceptable, so that a bad document is
/// refused when it is written rather than failing every request.
fn validate_setting(name: &str, value: &str) -> CoreResult<()> {
    match name {
        SETTING_CONFIG => {
            let c = Config::parse(value)?;
            if !c.master_url.starts_with("https://") && !c.master_url.starts_with("http://") {
                return Err(CoreError::Invalid(
                    "config: <master_url> must be an http or https URL".into(),
                ));
            }
            if !c.email_from.is_empty() && !crate::web::is_strict_email(&c.email_from) {
                return Err(CoreError::Invalid(
                    "config: <email_from> must be one email address".into(),
                ));
            }
        }
        SETTING_PLAN_CLASSES => {
            PlanClassSpecs::parse(value)?;
        }
        SETTING_CODE_SIGN_KEY | SETTING_UPLOAD_KEY => {
            PublicKey::parse(value)?;
        }
        _ => {
            return Err(CoreError::NotFound(format!("setting {name}")));
        }
    }
    Ok(())
}

/// Stores a setting after checking that it parses, so that a bad document
/// is refused here rather than failing every request.
pub async fn put_setting(db: &dyn Db, name: &str, value: &str, now: i64) -> CoreResult<()> {
    validate_setting(name, value)?;
    db.run(stmt!(
        "INSERT INTO config (name, value, mod_time) VALUES (?, ?, ?) \
         ON CONFLICT (name) DO UPDATE SET value = excluded.value, mod_time = excluded.mod_time",
        name,
        value,
        now
    ))
    .await?;
    Ok(())
}

/// The span of a top-level element of `<config>`, as byte offsets into the
/// document: from its `<` to the end of its closing tag.
struct TagSpan {
    start: usize,
    end: usize,
    /// The text between the tags; empty for `<tag/>`.
    inner: String,
}

/// The top-level elements of `<config>` named `tag`, and where `</config>`
/// starts. The elements of `config.xml` hold text, so a closing tag is the
/// next `</tag>`.
fn config_elements(xml: &str, tag: &str) -> CoreResult<(Vec<TagSpan>, usize)> {
    let bad = || CoreError::Xml("config: no <config> element, or it is not closed".into());
    let open = xml.find("<config>").ok_or_else(bad)? + "<config>".len();
    let close = xml[open..].find("</config>").ok_or_else(bad)? + open;
    let mut spans = Vec::new();
    let mut at = open;
    while let Some(lt) = xml[at..close].find('<').map(|i| i + at) {
        let rest = &xml[lt..close];
        if let Some(body) = rest.strip_prefix("<!--") {
            let n = body.find("-->").ok_or_else(bad)? + 3;
            at = lt + 4 + n;
            continue;
        }
        let gt = rest.find('>').ok_or_else(bad)? + lt;
        let head = &xml[lt + 1..gt];
        let self_closing = head.ends_with('/');
        let name = head
            .trim_end_matches('/')
            .split(|c: char| c.is_whitespace())
            .next()
            .unwrap_or("");
        if name.starts_with('/') || name.is_empty() {
            at = gt + 1;
            continue;
        }
        let (end, inner) = if self_closing {
            (gt + 1, String::new())
        } else {
            let closing = format!("</{name}>");
            match xml[gt + 1..close].find(&closing) {
                Some(i) => (
                    gt + 1 + i + closing.len(),
                    xml[gt + 1..gt + 1 + i].to_string(),
                ),
                // An element that is never closed runs to the end.
                None => (close, xml[gt + 1..close].to_string()),
            }
        };
        if name == tag {
            spans.push(TagSpan {
                start: lt,
                end,
                inner,
            });
        }
        at = end;
    }
    Ok((spans, close))
}

/// `xml` with the top-level tag of `<config>` set to `value`, or removed when
/// `value` is `None`, and the value the tag had (the first one, if the tag is
/// repeated). A tag that is there more than once keeps the first and loses
/// the rest. Nothing else in the document changes.
pub fn edit_config_tag(
    xml: &str,
    tag: &str,
    value: Option<&str>,
) -> CoreResult<(String, Option<String>)> {
    if tag.is_empty()
        || tag.len() > 64
        || !tag.starts_with(|c: char| c.is_ascii_lowercase())
        || !tag
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(CoreError::Invalid(
            "config tag: lower-case letters, digits and '_', starting with a letter".into(),
        ));
    }
    if tag == "config" {
        return Err(CoreError::Invalid("config tag: not an option".into()));
    }
    if let Some(v) = value {
        if v.len() > 4096 || v.contains('\0') {
            return Err(CoreError::Invalid(
                "config tag: the value is too long or has a NUL".into(),
            ));
        }
    }
    let (spans, close) = config_elements(xml, tag)?;
    let previous = spans.first().map(|s| {
        if s.inner.is_empty() {
            String::new()
        } else {
            cb_proto::xml::xml_unescape(&s.inner)
        }
    });
    let element = value.map(|v| format!("<{tag}>{}</{tag}>", cb_proto::xml::xml_escape(v)));
    // Leave the line a removed element was on without a blank one.
    let widen = |s: &TagSpan| {
        let line = xml[..s.start].rfind('\n').map_or(0, |i| i + 1);
        let blank_before = xml[line..s.start].trim().is_empty();
        let tail = xml[s.end..].find('\n').map(|i| s.end + i + 1);
        match (blank_before, tail) {
            (true, Some(t)) if xml[s.end..t].trim().is_empty() => (line, t),
            _ => (s.start, s.end),
        }
    };
    let mut out = String::with_capacity(xml.len() + 64);
    let mut cursor = 0;
    match (&element, spans.first()) {
        (Some(e), Some(first)) => {
            out.push_str(&xml[..first.start]);
            out.push_str(e);
            cursor = first.end;
            for dup in &spans[1..] {
                let (from, to) = widen(dup);
                out.push_str(&xml[cursor..from]);
                cursor = to;
            }
        }
        (Some(e), None) => {
            let line = xml[..close].rfind('\n').map_or(0, |i| i + 1);
            if xml[line..close].trim().is_empty() && line > 0 {
                // `</config>` on a line of its own: a line before it, indented
                // like the element above it, or by four spaces.
                let indent = xml[..line]
                    .trim_end()
                    .rfind('\n')
                    .map(|i| &xml[i + 1..])
                    .and_then(|l| l.find('<').map(|n| &l[..n]))
                    .filter(|i| i.trim().is_empty() && !i.is_empty())
                    .unwrap_or("    ");
                out.push_str(&xml[..line]);
                out.push_str(&format!("{indent}{e}\n"));
                cursor = line;
            } else {
                out.push_str(&xml[..close]);
                out.push_str(e);
                cursor = close;
            }
        }
        (None, _) => {
            for s in &spans {
                let (from, to) = widen(s);
                out.push_str(&xml[cursor..from]);
                cursor = to;
            }
        }
    }
    out.push_str(&xml[cursor..]);
    Ok((out, previous))
}

/// What `change_config_tag` did.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TagChange {
    /// The value the tag had; `None` if it was not there.
    pub previous: Option<String>,
    /// Whether the document changed.
    pub changed: bool,
}

/// Sets or removes one tag of the `config` setting. The document is read,
/// edited and written back only if it is still the one that was read, so two
/// operators changing different tags both land, and a change that raced with
/// another is read again. The result must pass the checks of `put_setting`.
pub async fn change_config_tag(
    db: &dyn Db,
    tag: &str,
    value: Option<&str>,
    now: i64,
) -> CoreResult<TagChange> {
    for _ in 0..5 {
        let Some(old) = get_setting(db, SETTING_CONFIG).await? else {
            return Err(CoreError::NotFound("setting config".into()));
        };
        let (new, previous) = edit_config_tag(&old, tag, value)?;
        if new == old {
            return Ok(TagChange {
                previous,
                changed: false,
            });
        }
        validate_setting(SETTING_CONFIG, &new)?;
        let rows = db
            .run(stmt!(
                "UPDATE config SET value = ?, mod_time = ? WHERE name = ? AND value = ? \
                 RETURNING name",
                new.as_str(),
                now,
                SETTING_CONFIG,
                old.as_str()
            ))
            .await?;
        if !rows.rows.is_empty() {
            return Ok(TagChange {
                previous,
                changed: true,
            });
        }
    }
    Err(CoreError::Conflict(
        "the config kept changing under the edit; try again".into(),
    ))
}

pub async fn get_setting(db: &dyn Db, name: &str) -> CoreResult<Option<String>> {
    if !SETTINGS.contains(&name) {
        return Err(CoreError::NotFound(format!("setting {name}")));
    }
    let rows = db
        .run(stmt!("SELECT name, value FROM config WHERE name = ?", name))
        .await?;
    Ok(rows.first::<SettingRow>()?.map(|r| r.value))
}
