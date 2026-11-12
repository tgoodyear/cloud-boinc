//! The comparison of two replies, and the lists that decide what counts.
//!
//! A reply is read into leaves (`xmltree::flatten`). Every difference
//! between the stock leaves and this server's leaves is then one of:
//!
//! * equal after reading numbers as numbers (the stock server prints doubles
//!   with `%f`);
//! * covered by a rule in [`TIMES`], [`RANDOM`] or [`TRANSFORMS`]: a value
//!   that depends on the clock or on a random number, or a form the two
//!   servers write differently on purpose;
//! * covered by an entry in [`INTENDED`]: a difference between the servers
//!   that is recorded in `cloud/docs/compatibility.md` with its reason;
//! * a finding: anything else, which fails the test.
//!
//! Each rule and entry must be used by at least one step, or the test fails
//! too, so that none of them outlives the difference it describes.

use std::collections::{BTreeMap, BTreeSet};

use regex::Regex;

use crate::xmltree::{flatten, parse, shape, Leaf};

/// Matches `step` against patterns separated by `|`: the step's name
/// (`scenario/step`) from its start, `*` standing for any rest.
fn step_matches(patterns: &str, step: &str) -> bool {
    patterns
        .split('|')
        .any(|pattern| match pattern.strip_suffix('*') {
            Some(prefix) => step.starts_with(prefix),
            None => pattern == step,
        })
}

fn shape_matches(pattern: &str, shape: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => shape.starts_with(prefix),
        None => pattern == shape,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// The stock reply has the element, this one does not.
    OnlyStock,
    /// This reply has the element, the stock one does not.
    OnlyOurs,
    /// Both have it, with different values.
    Differs,
}

/// One difference this server has on purpose.
pub struct Intended {
    /// The number in the compatibility document.
    pub id: &'static str,
    /// The steps it appears in (see [`step_matches`]).
    pub step: &'static str,
    /// The shape of the leaf (see `xmltree::shape`); a trailing `*` matches
    /// any rest.
    pub shape: &'static str,
    pub kind: Kind,
}

/// Fields that hold a time. The stock value and this server's, which is
/// computed from the time the stock request was sent, may differ by this
/// many seconds: the stock server reads its own clock while handling the
/// request.
pub const TIME_TOLERANCE: f64 = 2.0;
pub const TIMES: &[&str] = &[
    "scheduler_reply/host_create_time",
    "scheduler_reply/result/report_deadline",
    "user/host/create_time",
    "user/host/expavg_time",
];

/// Fields whose value is random: both must have this form, and need not be
/// equal.
pub const RANDOM: &[(&str, &str)] = &[
    ("account_out/authenticator", "[0-9a-f]{32}"),
    // `<weak_auth>`: the user's ID and a hash that includes the stored
    // password hash, which differs between the servers.
    ("am_get_info_reply/weak_auth", "[0-9]+_[0-9a-f]{32}"),
    // The upload certificate: checked by `World::certificates_verify`.
    ("scheduler_reply/file_info/xml_signature", "[0-9a-f.\\s]+"),
];

/// Values that are the same up to one form of writing, which the servers
/// write differently on purpose: `(id, shape, pattern, replacement)` applied
/// to both values before they are compared.
pub const TRANSFORMS: &[(&str, &str, &str, &str)] = &[
    (
        // The seconds a host must wait: stock counts from the fraction of a
        // second its last request came in, this server from whole seconds.
        "wait-seconds",
        "scheduler_reply/message",
        "wait [0-9]+ seconds",
        "wait N seconds",
    ),
    (
        // The stock scheduler marks some messages for translation, as
        // `_("text")`, which the client removes before it shows them
        // (`strip_translation` in `lib/str_util.cpp`). This server sends the
        // plain text. See D11.
        "translation-markers",
        "scheduler_reply/message",
        "_\\(\"|\"\\)",
        "",
    ),
];

pub const INTENDED: &[Intended] = &[
    // D1: no project web pages, so no `<gui_urls>` and `<rss_feeds>`.
    Intended {
        id: "gui-urls",
        step: "*",
        shape: "scheduler_reply/gui_urls/*",
        kind: Kind::OnlyStock,
    },
    Intended {
        id: "rss-feeds",
        step: "*",
        shape: "scheduler_reply/rss_feeds/*",
        kind: Kind::OnlyStock,
    },
    // D2: a wrong password. The stock script dies with a PHP error.
    Intended {
        id: "lookup-bad-password",
        step: "attach/03-*",
        shape: "#document*",
        kind: Kind::OnlyStock,
    },
    Intended {
        id: "lookup-bad-password",
        step: "attach/03-*",
        shape: "error/*",
        kind: Kind::OnlyOurs,
    },
    // D3: `am_set_info.php`.
    Intended {
        id: "am-set-info",
        step: "web/10-*",
        shape: "error/*",
        kind: Kind::OnlyStock,
    },
    Intended {
        id: "am-set-info",
        step: "web/10-*",
        shape: "am_set_info_reply/*",
        kind: Kind::OnlyOurs,
    },
    // D4: `server_status.php?xml=1`.
    Intended {
        id: "server-status",
        step: "web/03-*",
        shape: "server_status/*",
        kind: Kind::OnlyStock,
    },
    Intended {
        id: "server-status",
        step: "web/03-*",
        shape: "server_status/*",
        kind: Kind::OnlyOurs,
    },
    Intended {
        id: "server-status",
        step: "web/03-*",
        shape: "server_status/*",
        kind: Kind::Differs,
    },
    // D5: uploads of files smaller than a piece are not resumable.
    Intended {
        id: "upload-pieces",
        step: "uploads/10-*|uploads/11-*|uploads/12-*",
        shape: "data_server_reply/*",
        kind: Kind::Differs,
    },
    Intended {
        id: "upload-pieces",
        step: "uploads/10-*|uploads/11-*|uploads/12-*",
        shape: "data_server_reply/*",
        kind: Kind::OnlyOurs,
    },
    // D6: a file that exists is never replaced.
    Intended {
        id: "upload-exists",
        step: "uploads/06-*",
        shape: "data_server_reply/*",
        kind: Kind::Differs,
    },
    Intended {
        id: "upload-exists",
        step: "uploads/06-*",
        shape: "data_server_reply/*",
        kind: Kind::OnlyOurs,
    },
];

#[derive(Debug, Clone, PartialEq)]
pub enum Diff {
    OnlyStock {
        path: String,
        value: String,
    },
    OnlyOurs {
        path: String,
        value: String,
    },
    Differs {
        path: String,
        stock: String,
        ours: String,
    },
}

impl Diff {
    pub fn path(&self) -> &str {
        match self {
            Diff::OnlyStock { path, .. }
            | Diff::OnlyOurs { path, .. }
            | Diff::Differs { path, .. } => path,
        }
    }
    fn kind(&self) -> Kind {
        match self {
            Diff::OnlyStock { .. } => Kind::OnlyStock,
            Diff::OnlyOurs { .. } => Kind::OnlyOurs,
            Diff::Differs { .. } => Kind::Differs,
        }
    }
}

impl std::fmt::Display for Diff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Diff::OnlyStock { path, value } => write!(f, "only in stock: {path} = {value:?}"),
            Diff::OnlyOurs { path, value } => write!(f, "only in ours:  {path} = {value:?}"),
            Diff::Differs { path, stock, ours } => {
                write!(
                    f,
                    "differs: {path}\n      stock: {stock:?}\n      ours:  {ours:?}"
                )
            }
        }
    }
}

/// Which rules and entries a run used.
#[derive(Default)]
pub struct Used {
    pub times: BTreeSet<&'static str>,
    pub random: BTreeSet<&'static str>,
    pub transforms: BTreeSet<&'static str>,
    pub intended: BTreeMap<&'static str, usize>,
    /// Entries of [`INTENDED`] by index, to tell which were never used.
    pub intended_index: BTreeSet<usize>,
}

/// Numbers compare as numbers.
fn same_value(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (a.parse::<f64>(), b.parse::<f64>()) {
        (Ok(x), Ok(y)) => x == y || (x - y).abs() <= 1e-9 * x.abs().max(y.abs()),
        _ => false,
    }
}

fn leaves(body: &str) -> Result<Vec<Leaf>, String> {
    Ok(flatten(&parse(body)?))
}

/// The differences between two replies that no rule or entry covers.
pub fn compare(step: &str, stock: &str, ours: &str, used: &mut Used) -> Result<Vec<Diff>, String> {
    let a = leaves(stock).map_err(|e| format!("stock reply: {e}"))?;
    let b = leaves(ours).map_err(|e| format!("our reply: {e}"))?;
    let mut ours_map: BTreeMap<&str, &str> = BTreeMap::new();
    for l in &b {
        ours_map.insert(&l.path, &l.value);
    }
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut found = Vec::new();
    for l in &a {
        seen.insert(&l.path);
        match ours_map.get(l.path.as_str()) {
            None => found.push(Diff::OnlyStock {
                path: l.path.clone(),
                value: l.value.clone(),
            }),
            Some(v) if same_value(&l.value, v) => {}
            Some(v) => found.push(Diff::Differs {
                path: l.path.clone(),
                stock: l.value.clone(),
                ours: v.to_string(),
            }),
        }
    }
    for l in &b {
        if !seen.contains(l.path.as_str()) {
            found.push(Diff::OnlyOurs {
                path: l.path.clone(),
                value: l.value.clone(),
            });
        }
    }
    // A rule is needed when its field appears, whether or not the values
    // happen to be equal this time.
    for l in &a {
        if !ours_map.contains_key(l.path.as_str()) {
            continue;
        }
        let sh = shape(&l.path);
        if let Some(t) = TIMES.iter().find(|t| **t == sh) {
            used.times.insert(*t);
        }
        for (pat, _) in RANDOM.iter().filter(|(p, _)| shape_matches(p, &sh)) {
            used.random.insert(*pat);
        }
        for (id, pat, re, _) in TRANSFORMS {
            if shape_matches(pat, &sh) && Regex::new(re).unwrap().is_match(&l.value) {
                used.transforms.insert(*id);
            }
        }
    }
    let mut out = Vec::new();
    'diffs: for d in found {
        let sh = shape(d.path());
        if let Diff::Differs { stock, ours, .. } = &d {
            for t in TIMES.iter().filter(|t| **t == sh) {
                if let (Ok(x), Ok(y)) = (stock.parse::<f64>(), ours.parse::<f64>()) {
                    if (x - y).abs() <= TIME_TOLERANCE {
                        let _ = t;
                        continue 'diffs;
                    }
                }
            }
            for (shape_pat, pattern) in RANDOM.iter().filter(|(s, _)| shape_matches(s, &sh)) {
                let re = Regex::new(&format!("^(?:{pattern})$")).unwrap();
                if re.is_match(stock) && re.is_match(ours) {
                    let _ = shape_pat;
                    continue 'diffs;
                }
            }
            for (id, shape_pat, pattern, with) in TRANSFORMS {
                if shape_matches(shape_pat, &sh) {
                    let re = Regex::new(pattern).unwrap();
                    if same_value(&re.replace_all(stock, *with), &re.replace_all(ours, *with)) {
                        let _ = id;
                        continue 'diffs;
                    }
                }
            }
        }
        for (i, e) in INTENDED.iter().enumerate() {
            if e.kind == d.kind() && step_matches(e.step, step) && shape_matches(e.shape, &sh) {
                *used.intended.entry(e.id).or_default() += 1;
                used.intended_index.insert(i);
                continue 'diffs;
            }
        }
        out.push(d);
    }
    Ok(out)
}

/// The scheduler URLs a master page offers: `<link rel="boinc_scheduler"
/// href=...>` and the commented `<scheduler>` form, which are what the
/// client reads (`SCHEDULER_OP::parse_master_file`). The rest of the page is
/// a web page, not a protocol.
pub fn scheduler_links(page: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = page;
    while let Some(i) = rest.find("rel=\"boinc_scheduler\"") {
        let tag_start = rest[..i].rfind('<').unwrap_or(0);
        let tag_end = rest[i..].find('>').map(|e| i + e).unwrap_or(rest.len());
        let tag = &rest[tag_start..tag_end];
        if let Some(h) = tag.find("href=\"") {
            let v = &tag[h + 6..];
            if let Some(e) = v.find('"') {
                out.push(v[..e].to_string());
            }
        }
        rest = &rest[tag_end..];
    }
    let mut rest = page;
    while let Some(i) = rest.find("<scheduler>") {
        let v = &rest[i + 11..];
        let Some(e) = v.find("</scheduler>") else {
            break;
        };
        out.push(v[..e].to_string());
        rest = &v[e..];
    }
    out
}
