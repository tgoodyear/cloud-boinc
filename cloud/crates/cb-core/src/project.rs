//! Project settings: the rows of the `config` table and the catalog, as one
//! value a Worker loads and keeps for a few seconds.

use cb_crypto::PublicKey;
use cb_proto::xml::{xml_unescape, XmlParser};
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

/// A top-level element of the active `<config>`, with where it sits in the
/// document.
#[derive(Debug, Clone, PartialEq)]
struct ConfigElement {
    name: String,
    /// From the `<` of its opening tag to the end of its closing tag.
    start: usize,
    end: usize,
    /// Its text, as the loader reads it (unescaped, outside CDATA); `None`
    /// when the element holds tags or more than one piece of text, which the
    /// loader does not read as a value.
    value: Option<String>,
}

/// The active `<config>` of a document, as `Config::parse` finds it.
struct ConfigDoc {
    elements: Vec<ConfigElement>,
    /// Where `</config>` starts.
    close: usize,
}

/// Reads the config with the tokenizer `Config::parse` reads it with
/// (`XmlParser`: comments are skipped, CDATA is text, `<config >` and a
/// `<config>` with attributes are the element, the first `<config>` tag
/// anywhere starts it), and keeps the source offsets of what it reads.
fn scan_config(xml: &str) -> CoreResult<ConfigDoc> {
    let bad = || CoreError::Xml("config: no <config> element, or it is not closed".into());
    if xml.contains('\0') {
        return Err(CoreError::Invalid("config: the document has a NUL".into()));
    }
    let mut xp = XmlParser::new(xml);
    loop {
        if !xp.get_tag() {
            return Err(bad());
        }
        if xp.match_tag("config") {
            break;
        }
    }
    let mut elements = Vec::new();
    loop {
        if !xp.get_tag() {
            return Err(bad());
        }
        if xp.match_tag("/config") {
            return Ok(ConfigDoc {
                elements,
                close: xp.token_start(),
            });
        }
        // Text between elements, and a closing tag with nothing open, are
        // skipped, as the loader skips them.
        if !xp.is_tag() || xp.tag().starts_with('/') {
            continue;
        }
        let start = xp.token_start();
        let open = xp.tag().to_string();
        if let Some(name) = open.strip_suffix('/') {
            elements.push(ConfigElement {
                name: name.to_string(),
                start,
                end: xp.pos(),
                value: Some(String::new()),
            });
            continue;
        }
        // Read to the closing tag, counting any elements inside.
        let mut depth = 1;
        let mut text: Vec<String> = Vec::new();
        let mut nested = false;
        while depth > 0 {
            if !xp.get_tag() {
                return Err(bad());
            }
            if xp.is_tag() {
                let t = xp.tag();
                if t.starts_with('/') {
                    depth -= 1;
                } else if !t.ends_with('/') {
                    depth += 1;
                    nested = true;
                } else {
                    nested = true;
                }
            } else if xp.is_cdata() {
                text.push(xp.tag().to_string());
            } else {
                text.push(xml_unescape(xp.tag()));
            }
        }
        let value = match (nested, text.len()) {
            (false, 0) => Some(String::new()),
            (false, 1) => text.pop(),
            _ => None,
        };
        elements.push(ConfigElement {
            name: open,
            start,
            end: xp.pos(),
            value,
        });
    }
}

/// `xml` with the top-level tag of `<config>` set to `value`, or removed when
/// `value` is `None`, and the value the tag had. The loader takes the last
/// of a tag that is stated more than once (each reading replaces the one
/// before), so setting changes that one and leaves the others, and removing
/// takes all of them out. Nothing else in the document changes. The edit is
/// read back with the loader's own tokenizer, and refused unless the tag has
/// the value asked for (or is gone) and every other element is as it was.
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
    let doc = scan_config(xml)?;
    let same: Vec<&ConfigElement> = doc.elements.iter().filter(|e| e.name == tag).collect();
    let previous = same.last().and_then(|e| e.value.clone());
    let element = value.map(|v| format!("<{tag}>{}</{tag}>", cb_proto::xml::xml_escape(v)));
    // Leave the line a removed element was on without a blank one.
    let widen = |s: &ConfigElement| {
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
    match (&element, same.last()) {
        (Some(e), Some(last)) => {
            out.push_str(&xml[..last.start]);
            out.push_str(e);
            cursor = last.end;
        }
        (Some(e), None) => {
            let close = doc.close;
            let line = xml[..close].rfind('\n').map_or(0, |i| i + 1);
            if xml[line..close].trim().is_empty() && line > 0 {
                // `</config>` on a line of its own: a line before it, indented
                // like the element above it, or by four spaces.
                let indent = doc
                    .elements
                    .last()
                    .map(|l| {
                        let at = xml[..l.start].rfind('\n').map_or(0, |i| i + 1);
                        &xml[at..l.start]
                    })
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
            for s in &same {
                let (from, to) = widen(s);
                out.push_str(&xml[cursor..from]);
                cursor = to;
            }
        }
    }
    out.push_str(&xml[cursor..]);

    // Read it back.
    let after = scan_config(&out)?;
    let others = |d: &ConfigDoc| -> Vec<(String, Option<String>)> {
        d.elements
            .iter()
            .filter(|e| e.name != tag)
            .map(|e| (e.name.clone(), e.value.clone()))
            .collect()
    };
    let mine: Vec<&ConfigElement> = after.elements.iter().filter(|e| e.name == tag).collect();
    let ok = match value {
        Some(v) => mine.last().is_some_and(|e| e.value.as_deref() == Some(v)),
        None => mine.is_empty(),
    };
    if !ok {
        return Err(CoreError::Invalid(
            "config tag: the value cannot be stored as it is (leading or trailing space, or \
             characters XML cannot hold)"
                .into(),
        ));
    }
    if others(&doc) != others(&after) {
        return Err(CoreError::Invalid(
            "config tag: the edit would change another option".into(),
        ));
    }
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
