//! Project settings: the rows of the `config` table and the catalog, as one
//! value a Worker loads and keeps for a few seconds.

use cb_crypto::PublicKey;
use cb_db::{stmt, Db, Stmt};
use cb_proto::xml::{xml_unescape, XmlParser};
use serde::Deserialize;

use crate::plan_class::PlanClassSpecs;
use crate::routing::RoutingRules;
use crate::scheduler::Catalog;
use crate::{Config, CoreError, CoreResult};

/// The `<config>` element of upstream `config.xml`.
pub const SETTING_CONFIG: &str = "config";
/// The upstream `plan_class_spec.xml` document.
pub const SETTING_PLAN_CLASSES: &str = "plan_class_spec";
/// The routing rules (`crate::routing`), a JSON document.
pub const SETTING_ROUTING_RULES: &str = "routing_rules";
/// The code-signing public key, in the BOINC key file format.
pub const SETTING_CODE_SIGN_KEY: &str = "code_sign_public_key";
/// The upload public key, in the BOINC key file format.
pub const SETTING_UPLOAD_KEY: &str = "upload_public_key";

pub const SETTINGS: [&str; 5] = [
    SETTING_CONFIG,
    SETTING_PLAN_CLASSES,
    SETTING_ROUTING_RULES,
    SETTING_CODE_SIGN_KEY,
    SETTING_UPLOAD_KEY,
];

#[derive(Debug, Clone, Default)]
pub struct Project {
    pub config: Config,
    pub plan_classes: PlanClassSpecs,
    pub routing: RoutingRules,
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
                SETTING_ROUTING_RULES => p.routing = RoutingRules::parse(&row.value)?,
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
            if !c.dashboard_feed_url.is_empty()
                && !crate::feed::valid_https_url(&c.dashboard_feed_url)
            {
                return Err(CoreError::Invalid(
                    "config: <dashboard_feed_url> must be an https URL".into(),
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
        SETTING_ROUTING_RULES => {
            RoutingRules::parse(value)?;
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

/// Removes a setting; a project without it behaves as one that never had it.
/// The `config` setting cannot be removed.
pub async fn delete_setting(db: &dyn Db, name: &str) -> CoreResult<bool> {
    if !SETTINGS.contains(&name) {
        return Err(CoreError::NotFound(format!("setting {name}")));
    }
    if name == SETTING_CONFIG {
        return Err(CoreError::Invalid(
            "the config setting cannot be removed".into(),
        ));
    }
    let rows = db
        .run(stmt!(
            "DELETE FROM config WHERE name = ? RETURNING name",
            name
        ))
        .await?;
    Ok(!rows.rows.is_empty())
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
/// `value` is `None`, and the value the tag had (its last occurrence's).
///
/// For an option the loader reads, the edit is of the setting, not of one
/// element: every occurrence of the tag, and of the tags that set the same
/// thing (`Config::option_group`), is taken out, and the tag is stated once, at
/// the place of its last occurrence. The loader takes the last of a repeated
/// tag for most options, but a flag that accumulates (`no_web_account_creation`
/// is on if any element says so) or that is combined with another spelling
/// would otherwise keep the old value. A tag the loader does not read is
/// changed in its last occurrence only (a stock `config.xml` repeats some
/// such tags, `ban_os` for one), and removal takes all of that tag.
///
/// The edit is read back with the loader's own tokenizer and refused unless
/// the tag has the value asked for (or is gone), every other element is as it
/// was, and what the loader makes of the result is what it makes of the
/// option stated alone: each setting a lone `<tag>value</tag>` changes has the
/// same value in the edited document.
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
        // An option the loader reads must be given a value it reads: the
        // loader skips `1800s` for a number and uses the default, so the
        // edit would report a change that did not happen.
        Config::check_option(tag, v)?;
    }
    let doc = scan_config(xml)?;
    let group = Config::option_group(tag);
    let names: Vec<&str> = group.clone().unwrap_or_else(|| vec![tag]);
    let in_group = |e: &&ConfigElement| names.contains(&e.name.as_str());
    let occurrences: Vec<&ConfigElement> = doc.elements.iter().filter(in_group).collect();
    let own: Vec<&&ConfigElement> = occurrences.iter().filter(|e| e.name == tag).collect();
    let previous = own.last().and_then(|e| e.value.clone());
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
    // What to cut or put: (from, to, text).
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    // The element that stays, with the new text in it; none when the tag
    // is not stated or the edit is a removal.
    let anchor: Option<&ConfigElement> = match (&element, group.is_some()) {
        (None, _) => None,
        (Some(_), true) => own.last().map(|e| **e).or(occurrences.last().copied()),
        (Some(_), false) => own.last().map(|e| **e),
    };
    // What is removed: the occurrences other than the anchor, of the whole
    // group for an option and of the tag for any other.
    let removed: Vec<&ConfigElement> = if group.is_some() {
        occurrences.clone()
    } else if element.is_none() {
        own.iter().map(|e| **e).collect()
    } else {
        Vec::new()
    };
    for e in removed {
        if anchor.is_some_and(|a| a.start == e.start) {
            continue;
        }
        let (from, to) = widen(e);
        edits.push((from, to, String::new()));
    }
    match (&element, anchor) {
        (Some(e), Some(a)) => edits.push((a.start, a.end, e.clone())),
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
                edits.push((line, line, format!("{indent}{e}\n")));
            } else {
                edits.push((close, close, e.clone()));
            }
        }
        (None, _) => {}
    }
    edits.sort_by_key(|e| e.0);
    let mut out = String::with_capacity(xml.len() + 64);
    let mut cursor = 0;
    for (from, to, text) in &edits {
        out.push_str(&xml[cursor..*from]);
        out.push_str(text);
        cursor = *to;
    }
    out.push_str(&xml[cursor..]);

    // Read it back.
    let after = scan_config(&out)?;
    let others = |d: &ConfigDoc| -> Vec<(String, Option<String>)> {
        d.elements
            .iter()
            .filter(|e| !names.contains(&e.name.as_str()))
            .map(|e| (e.name.clone(), e.value.clone()))
            .collect()
    };
    let mine: Vec<&ConfigElement> = after.elements.iter().filter(in_group).collect();
    let ok = match value {
        Some(v) if group.is_some() => {
            mine.len() == 1 && mine[0].name == tag && mine[0].value.as_deref() == Some(v)
        }
        Some(v) => mine
            .iter()
            .rfind(|e| e.name == tag)
            .is_some_and(|e| e.value.as_deref() == Some(v)),
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
    // The URLs are filled in from the master URL when the config is
    // finished, so what one of them comes to depends on the others.
    let derived = ["master_url", "download_url", "upload_url"].contains(&tag);
    if let (Some(v), true, false) = (value, group.is_some(), derived) {
        effective_value_holds(&out, tag, v)?;
    }
    Ok((out, previous))
}

/// What the loader makes of `out` for an option just set: every setting that
/// `<tag>value</tag>` alone changes holds the value it gets from that element.
/// A setting some other element of the document still overrides is a setting
/// the edit did not set; the edit is refused, naming the tags in the document
/// that read the same.
fn effective_value_holds(out: &str, tag: &str, value: &str) -> CoreResult<()> {
    let lone = format!(
        "<config><master_url>https://p.example/</master_url><{tag}>{}</{tag}></config>",
        cb_proto::xml::xml_escape(value)
    );
    let base = "<config><master_url>https://p.example/</master_url></config>";
    let (Ok(lone), Ok(base), Ok(got)) = (
        Config::parse(&lone),
        Config::parse(base),
        Config::parse(out),
    ) else {
        // A document that does not parse is refused by the checks that follow.
        return Ok(());
    };
    // The settings the element sets, and what they come to in the document.
    let wanted = lone.changed_fields(&base);
    let have = got.changed_fields(&base);
    for (field, v) in wanted {
        if !have.iter().any(|(f, x)| *f == field && *x == v) {
            return Err(CoreError::Invalid(format!(
                "config tag: <{tag}> is overridden by another element of the document \
                 ({field} reads differently)"
            )));
        }
    }
    Ok(())
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
