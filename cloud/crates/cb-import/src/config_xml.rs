//! A stock `config.xml`: what this server reads of it, as `cboinc config-set`
//! lines. Nothing is applied. The values of tags whose names suggest a
//! credential are never read into the output.

use std::collections::{BTreeMap, BTreeSet};

use cb_core::config::Config;
use cb_proto::xml::XmlParser;

use crate::error::{ImportError, Result};

#[derive(Debug, Clone, Default)]
pub struct StockConfig {
    /// Tags of `<config>` this server reads, in file order, with their text
    /// (`1` for a tag written as `<name/>`).
    pub recognised: Vec<(String, String)>,
    /// Tags of `<config>` this server ignores (names only).
    pub ignored: Vec<String>,
    pub fanout: u32,
    pub master_url: String,
    pub download_url: String,
    /// App name to the validator kind its daemon runs: `trivial`, `bitwise`
    /// or `other`.
    pub validators: BTreeMap<String, String>,
}

fn looks_secret(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["passwd", "password", "secret", "token", "key", "auth"]
        .iter()
        .any(|w| n.contains(w))
}

/// Tags that name the stock host or its database, which this server has no use for.
fn is_host_specific(name: &str) -> bool {
    matches!(
        name,
        "host" | "db_name" | "db_user" | "db_host" | "replica_db_name" | "replica_db_host"
    ) || name.ends_with("_dir")
        || name.ends_with("_path")
        || name.ends_with("_url_path")
}

pub fn parse(xml: &str) -> Result<StockConfig> {
    let known: BTreeSet<&'static str> = Config::option_kinds().map(|(n, _)| n).collect();
    let mut out = StockConfig::default();
    let mut xp = XmlParser::new(xml);
    let mut in_config = false;
    let mut seen_config = false;
    while xp.get_tag() {
        if !xp.is_tag() {
            continue;
        }
        let tag = xp.tag().to_string();
        if tag == "config" {
            in_config = true;
            seen_config = true;
            continue;
        }
        if tag == "/config" {
            in_config = false;
            continue;
        }
        if tag == "cmd" || tag == "cmd/" {
            if let Some(cmd) = xp.parse_str("cmd") {
                daemon(&cmd, &mut out);
            }
            continue;
        }
        if !in_config {
            continue;
        }
        let (name, value) = if let Some(n) = tag.strip_suffix('/') {
            (n.to_string(), Some("1".to_string()))
        } else {
            let v = xp.parse_str(&tag);
            (tag.clone(), v)
        };
        let Some(value) = value else {
            xp.skip_unexpected();
            continue;
        };
        if looks_secret(&name) {
            out.ignored.push(name);
            continue;
        }
        let value = value.trim().to_string();
        match name.as_str() {
            "uldl_dir_fanout" => out.fanout = value.parse().unwrap_or(0),
            "master_url" => out.master_url = value.clone(),
            "download_url" => out.download_url = value.clone(),
            _ => {}
        }
        if known.contains(name.as_str()) && !is_host_specific(&name) {
            out.recognised.push((name, value));
        } else {
            out.ignored.push(name);
        }
    }
    if !seen_config {
        return Err(ImportError::usage(
            "the --config file has no <config> element",
        ));
    }
    Ok(out)
}

fn daemon(cmd: &str, out: &mut StockConfig) {
    let words: Vec<&str> = cmd.split_whitespace().collect();
    let Some(bin) = words.first() else { return };
    let bin = bin.rsplit('/').next().unwrap_or(bin);
    if !bin.contains("validator") {
        return;
    }
    let app = words
        .iter()
        .position(|w| *w == "--app" || *w == "-app")
        .and_then(|i| words.get(i + 1));
    let Some(app) = app else { return };
    let kind = match bin {
        "sample_trivial_validator" => "trivial",
        "sample_bitwise_validator" => "bitwise",
        _ => "other",
    };
    out.validators.insert(app.to_string(), kind.to_string());
}

fn shell_quote(v: &str) -> String {
    format!("'{}'", v.replace('\'', "'\\''"))
}

/// The lines to run against the new server: `cboinc config-set ...`, and
/// `#` comment lines for what needs a look.
pub fn config_set_lines(c: &StockConfig) -> Vec<String> {
    let mut lines = Vec::new();
    // A tag that appears twice in `<config>` counts once, with the last value, as
    // the stock parser reads it.
    let mut last: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for (i, (tag, _)) in c.recognised.iter().enumerate() {
        last.insert(tag.as_str(), i);
    }
    for (i, (tag, value)) in c.recognised.iter().enumerate() {
        if last[tag.as_str()] != i {
            continue;
        }
        lines.push(format!(
            "cboinc config-set --tag {tag} --value {}",
            shell_quote(value)
        ));
    }
    if let Some(short) = c
        .master_url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty() && !s.contains(':') && !s.contains('.'))
    {
        lines.push(format!(
            "# stock_short_name is not in config.xml; the master URL's last segment is {short:?}. If that is the project's short name:"
        ));
        lines.push(format!(
            "cboinc config-set --tag stock_short_name --value {}",
            shell_quote(short)
        ));
    }
    if !c.ignored.is_empty() {
        let mut names: Vec<&str> = c.ignored.iter().map(|s| s.as_str()).collect();
        names.sort();
        names.dedup();
        lines.push(format!(
            "# {} tags of <config> are not read by this server or belong to the stock host: {}",
            names.len(),
            names.join(", ")
        ));
    }
    for (app, kind) in &c.validators {
        if kind == "other" {
            lines.push(format!(
                "# the stock validator of app {app:?} is project code: set its validator on this server (cboinc app-put --validator webhook) before work is sent"
            ));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: &str = r#"<boinc>
<config>
    <db_name>proj</db_name>
    <db_passwd>hunter2-secret</db_passwd>
    <master_url>http://project.test/proj/</master_url>
    <uldl_dir_fanout>1024</uldl_dir_fanout>
    <send_result_abort/>
    <min_sendwork_interval>30</min_sendwork_interval>
    <min_sendwork_interval>31</min_sendwork_interval>
    <upload_dir>/x/upload</upload_dir>
    <shmem_key>1234</shmem_key>
    <fuh_debug_level>3</fuh_debug_level>
</config>
<daemons>
  <daemon><cmd>sample_trivial_validator -d 3 --app upper</cmd></daemon>
  <daemon><cmd>bin/my_validator --app other</cmd></daemon>
</daemons>
</boinc>"#;

    #[test]
    fn reads_recognised_tags_and_hides_secrets() {
        let c = parse(CFG).unwrap();
        let lines = config_set_lines(&c).join("\n");
        assert!(
            lines.contains("--tag send_result_abort --value '1'"),
            "{lines}"
        );
        assert!(lines.contains("--tag min_sendwork_interval --value '31'"));
        assert!(!lines.contains("'30'"), "the last of two equal tags wins");
        assert!(lines.contains("--tag master_url"));
        assert!(!lines.contains("hunter2"));
        assert!(!lines.contains("1234"), "shmem_key value is not printed");
        assert!(lines.contains("stock_short_name --value 'proj'"));
        assert_eq!(c.fanout, 1024);
        assert_eq!(c.validators["upper"], "trivial");
        assert_eq!(c.validators["other"], "other");
        assert!(
            lines.contains("db_passwd"),
            "names of ignored tags are listed"
        );
    }

    #[test]
    fn needs_a_config_element() {
        assert!(parse("<boinc></boinc>").is_err());
    }
}
