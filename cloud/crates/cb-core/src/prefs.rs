//! Computing (global) and project preferences: the XML kept in
//! `user.global_prefs` and `user.project_prefs`, the fields the site edits,
//! and the HTML form for them. Formats and defaults follow upstream
//! `html/inc/prefs.inc`, `prefs_project.inc` and `prefs_util.inc`.
//!
//! The XML is what the scheduler sends to clients: global preferences go out
//! when their `<mod_time>` is newer than the client's copy, so every save
//! sets it to the current time; project preferences go out in every reply.
//! Both may carry `<venue name="...">` sections, which a client with that
//! venue uses in place of the main set (`extract_venue`).
//!
//! Only fields that the current client or this scheduler reads are edited:
//!
//! - Computing: every field of upstream's form, all read by
//!   `GLOBAL_PREFS::parse_override` in `lib/prefs.cpp`.
//! - Project: `resource_share` and the `no_cpu`, `no_cuda`, `no_ati` and
//!   `no_intel_gpu` flags (read by the client from the account file), plus
//!   `no_apple_gpu`, `allow_beta_work`, the application selection
//!   (`<app_id>`) and `max_jobs` (read by the scheduler, `ProjectPrefs` in
//!   `scheduler.rs`). A GPU flag is offered only when the project has a
//!   version for that GPU type, and `allow_beta_work` only when an
//!   application or an application version is marked beta, as upstream. Other contents of
//!   `<project_specific>` are kept as they are.

use std::collections::BTreeMap;

use cb_proto::consts::*;
use cb_proto::xml::{extract_venue, line};

use crate::project::Project;
use crate::web::html_escape;
use crate::Config;

/// The name [`PrefsForm::parse_form`] gives the application selection in the
/// list of refused fields.
pub const APPS_FIELD: &str = "apps";

/// Style of a label that screen readers read and the page does not show.
pub const HIDDEN: &str = "position:absolute;left:-10000px;width:1px;height:1px;overflow:hidden";

/// Venue names, in the order upstream writes them.
pub const VENUES: [&str; 3] = ["home", "work", "school"];

/// Longest preferences document stored, as the scheduler's `MAX_BLOB`.
pub const MAX_PREFS_BYTES: usize = 64 * 1024;

/// How far past the server's clock a `<mod_time>` may be in a document an
/// account manager sends (`am_set_info`): a day, for clocks that disagree.
/// Upstream stores any value, but every reader of it (`GLOBAL_PREFS::parse_override`
/// in `lib/prefs.cpp`, `handle_request` in `sched/handle_request.cpp`) clamps it
/// to the current time, so a far-future one only makes the project send the
/// preferences with every request.
pub const MOD_TIME_FUTURE_SLACK_SECS: i64 = 86_400;

/// Why the `<mod_time>` of a global preferences document is refused: it is
/// not a finite number, or it is more than [`MOD_TIME_FUTURE_SLACK_SECS`]
/// after `now`. Zero, negative and missing values are accepted (they are
/// older than every other copy, so the next save or the client's own copy
/// wins), as upstream does.
pub fn mod_time_error(xml: &str, now: i64) -> Option<&'static str> {
    let limit = now.saturating_add(MOD_TIME_FUTURE_SLACK_SECS) as f64;
    // What the scheduler reads: the number at the start of the text after the
    // first `<mod_time>`, whatever follows it (`1e30junk` is 1e30). A time it
    // takes must be within the limit, so that junk after a number does not
    // slip a far-future time past a stricter reading.
    if line::parse_double(xml, "<mod_time>").is_some_and(|t| t > limit) {
        return Some("mod_time is in the future");
    }
    // And the element read as a whole number: a value that is a number but not
    // a finite one is refused.
    let text = element(xml, "mod_time")?;
    let t = text.trim().parse::<f64>().ok()?;
    if !t.is_finite() {
        Some("mod_time is not a finite number")
    } else if t > limit {
        Some("mod_time is in the future")
    } else {
        None
    }
}

/// Which of the two documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subset {
    Global,
    Project,
}

impl Subset {
    pub fn parse(s: &str) -> Option<Subset> {
        match s {
            "global" => Some(Subset::Global),
            "project" => Some(Subset::Project),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Subset::Global => "global",
            Subset::Project => "project",
        }
    }

    /// `subset_name`.
    pub fn title(self) -> &'static str {
        match self {
            Subset::Global => "Computing",
            Subset::Project => "Project",
        }
    }
}

/// `NUM_SPEC`: a number shown as `stored / scale`, accepted between `min`
/// and `max` in the units shown.
#[derive(Debug, Clone, Copy)]
struct NumSpec {
    unit: &'static str,
    min: f64,
    max: f64,
    default: f64,
    scale: f64,
    /// Only whole numbers are accepted: the reader of the stored value takes
    /// an integer (`max_jobs` in the scheduler, `daily_xfer_period_days` in
    /// the client), so a fraction would be shown but read as something else.
    whole: bool,
}

impl NumSpec {
    const fn whole(self) -> NumSpec {
        NumSpec {
            whole: true,
            ..self
        }
    }
}

const fn num(unit: &'static str, min: f64, max: f64, default: f64) -> NumSpec {
    NumSpec {
        unit,
        min,
        max,
        default,
        scale: 1.0,
        whole: false,
    }
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    /// `PREF_BOOL`. With `invert` the form shows the opposite sense, as
    /// "Suspend when computer is on battery" for `run_on_batteries`.
    Bool { default: bool, invert: bool },
    /// `PREF_NUM`, and `PREF_OPT_NUM`, where 0 means no limit.
    Num { spec: NumSpec, optional: bool },
    /// One end of `PREF_HOUR_RANGE`; written only when the two ends differ.
    Hour { other: &'static str, start: bool },
}

#[derive(Debug, Clone, Copy)]
struct Def {
    tag: &'static str,
    label: &'static str,
    group: &'static str,
    kind: Kind,
    /// Written inside `<project_specific>`, as upstream's project-specific
    /// form does.
    in_project_specific: bool,
}

const fn b(
    group: &'static str,
    tag: &'static str,
    label: &'static str,
    default: bool,
    invert: bool,
) -> Def {
    Def {
        tag,
        label,
        group,
        kind: Kind::Bool { default, invert },
        in_project_specific: false,
    }
}

const fn n(group: &'static str, tag: &'static str, label: &'static str, spec: NumSpec) -> Def {
    Def {
        tag,
        label,
        group,
        kind: Kind::Num {
            spec,
            optional: false,
        },
        in_project_specific: false,
    }
}

const fn opt(group: &'static str, tag: &'static str, label: &'static str, spec: NumSpec) -> Def {
    Def {
        tag,
        label,
        group,
        kind: Kind::Num {
            spec,
            optional: true,
        },
        in_project_specific: false,
    }
}

const fn hours(
    group: &'static str,
    start: &'static str,
    end: &'static str,
    label: &'static str,
) -> [Def; 2] {
    [
        Def {
            tag: start,
            label,
            group,
            kind: Kind::Hour {
                other: end,
                start: true,
            },
            in_project_specific: false,
        },
        Def {
            tag: end,
            label,
            group,
            kind: Kind::Hour {
                other: start,
                start: false,
            },
            in_project_specific: false,
        },
    ]
}

const IN_USE: &str = "When computer is in use";
const NOT_IN_USE: &str = "When computer is not in use";
const GENERAL: &str = "General";
const DISK: &str = "Disk";
const NET: &str = "Network";
const PROJECT: &str = "Project";

/// The computing preferences, in upstream's order. The disk defaults come
/// from the project settings, as `get_disk_space_config`.
fn global_defs(config: &Config) -> Vec<Def> {
    let mut d = vec![
        n(
            IN_USE,
            "idle_time_to_run",
            "'In use' means mouse/keyboard input in last",
            num("minutes", 1.0, 9999.0, 3.0),
        ),
        b(
            IN_USE,
            "run_if_user_active",
            "Suspend all computing",
            true,
            true,
        ),
        b(
            IN_USE,
            "run_gpu_if_user_active",
            "Suspend GPU computing",
            false,
            true,
        ),
        n(
            IN_USE,
            "max_ncpus_pct",
            "Use at most (% of the CPUs)",
            num("% of the CPUs", 1.0, 100.0, 100.0),
        ),
        n(
            IN_USE,
            "cpu_usage_limit",
            "Use at most (% of CPU time)",
            num("% of CPU time", 1.0, 100.0, 100.0),
        ),
        opt(
            IN_USE,
            "suspend_cpu_usage",
            "Suspend when non-BOINC CPU usage is above",
            num("%", 0.0, 100.0, 25.0),
        ),
        n(
            IN_USE,
            "ram_max_used_busy_pct",
            "Use at most (% of memory)",
            num("% of memory", 1.0, 100.0, 50.0),
        ),
        n(
            NOT_IN_USE,
            "niu_max_ncpus_pct",
            "Use at most (% of the CPUs)",
            num("% of the CPUs", 1.0, 100.0, 0.0),
        ),
        n(
            NOT_IN_USE,
            "niu_cpu_usage_limit",
            "Use at most (% of CPU time)",
            num("% of CPU time", 1.0, 100.0, 0.0),
        ),
        opt(
            NOT_IN_USE,
            "niu_suspend_cpu_usage",
            "Suspend when non-BOINC CPU usage is above",
            num("%", 0.0, 100.0, 0.0),
        ),
        n(
            NOT_IN_USE,
            "ram_max_used_idle_pct",
            "Use at most (% of memory)",
            num("% of memory", 1.0, 100.0, 90.0),
        ),
        opt(
            NOT_IN_USE,
            "suspend_if_no_recent_input",
            "Suspend when no mouse/keyboard input in last",
            num("minutes", 0.0, 9999.0, 0.0),
        ),
        b(
            GENERAL,
            "run_on_batteries",
            "Suspend when computer is on battery",
            false,
            true,
        ),
        n(
            GENERAL,
            "cpu_scheduling_period_minutes",
            "Switch between tasks every",
            num("minutes", 1.0, 9999.0, 60.0),
        ),
        n(
            GENERAL,
            "disk_interval",
            "Request tasks to checkpoint at most every",
            num("seconds", 0.0, 9_999_999.0, 60.0),
        ),
        b(
            GENERAL,
            "leave_apps_in_memory",
            "Leave non-GPU tasks in memory while suspended",
            false,
            false,
        ),
        n(
            GENERAL,
            "work_buf_min_days",
            "Store at least",
            num("days of work", 0.0, 10.0, 0.1),
        ),
        n(
            GENERAL,
            "work_buf_additional_days",
            "Store up to an additional",
            num("days of work", 0.0, 10.0, 0.5),
        ),
    ];
    d.extend(hours(
        GENERAL,
        "start_hour",
        "end_hour",
        "Compute only between",
    ));
    d.extend([
        opt(
            DISK,
            "disk_max_used_gb",
            "Use no more than",
            num("GB", 0.0, 9_999_999.0, 0.0),
        ),
        opt(
            DISK,
            "disk_min_free_gb",
            "Leave at least",
            num("GB free", 0.0, 9_999_999.0, config.default_disk_min_free_gb),
        ),
        opt(
            DISK,
            "disk_max_used_pct",
            "Use no more than",
            num("% of total", 0.0, 100.0, 90.0),
        ),
        n(
            DISK,
            "vm_max_used_pct",
            "Page/swap file: use at most",
            num("%", 1.0, 100.0, 75.0),
        ),
        opt(
            NET,
            "max_bytes_sec_down",
            "Limit download rate to",
            NumSpec {
                scale: 1024.0,
                ..num("KB/second", 0.0, 9_999_999.0, 0.0)
            },
        ),
        opt(
            NET,
            "max_bytes_sec_up",
            "Limit upload rate to",
            NumSpec {
                scale: 1024.0,
                ..num("KB/second", 0.0, 9_999_999.0, 0.0)
            },
        ),
        opt(
            NET,
            "daily_xfer_limit_mb",
            "Limit usage to",
            num("MB", 0.0, 9_999_999.0, 0.0),
        ),
        opt(
            NET,
            "daily_xfer_period_days",
            "... every",
            num("days", 0.0, 9_999_999.0, 0.0).whole(),
        ),
    ]);
    d.extend(hours(
        NET,
        "net_start_hour",
        "net_end_hour",
        "Transfer files only between",
    ));
    d.extend([
        b(
            NET,
            "dont_verify_images",
            "Skip data verification for image files",
            false,
            false,
        ),
        b(
            NET,
            "confirm_before_connecting",
            "Confirm before connecting to Internet",
            false,
            false,
        ),
        b(
            NET,
            "hangup_if_dialed",
            "Disconnect when done",
            false,
            false,
        ),
    ]);
    d
}

/// The processor types the project has an app version for.
fn project_resources(project: &Project) -> [bool; 5] {
    let mut out = [false; 5];
    for av in &project.catalog.app_versions {
        let t = if av.plan_class.is_empty() {
            PROC_TYPE_CPU
        } else {
            project
                .plan_classes
                .find(&av.plan_class)
                .map(|s| s.proc_type())
                .unwrap_or(PROC_TYPE_CPU)
        };
        if let Some(slot) = out.get_mut(t as usize) {
            *slot = true;
        }
    }
    out
}

/// The project preferences this project offers (`$project_pref_descs`).
fn project_defs(project: &Project) -> Vec<Def> {
    let rsc = project_resources(project);
    let mut d = vec![n(
        PROJECT,
        "resource_share",
        "Resource share",
        num("", 0.0, 9_999_999.0, 100.0),
    )];
    let flags = [
        (PROC_TYPE_CPU, "no_cpu", "Use CPU"),
        (PROC_TYPE_AMD_GPU, "no_ati", "Use AMD GPU"),
        (PROC_TYPE_NVIDIA_GPU, "no_cuda", "Use NVIDIA GPU"),
        (PROC_TYPE_INTEL_GPU, "no_intel_gpu", "Use Intel GPU"),
        (PROC_TYPE_APPLE_GPU, "no_apple_gpu", "Use Apple GPU"),
    ];
    for (t, tag, label) in flags {
        if rsc[t as usize] {
            d.push(b(PROJECT, tag, label, false, true));
        }
    }
    // `project_has_beta`: the scheduler asks for `allow_beta_work` when the
    // application or the version is beta.
    if project.catalog.apps.iter().any(|a| a.beta != 0)
        || project.catalog.app_versions.iter().any(|av| av.beta != 0)
    {
        d.push(b(
            PROJECT,
            "allow_beta_work",
            "Run test applications?",
            false,
            false,
        ));
    }
    d.push(Def {
        in_project_specific: true,
        ..n(
            PROJECT,
            "max_jobs",
            "Most tasks in progress (0 for no limit)",
            num("tasks", 0.0, 1000.0, 0.0).whole(),
        )
    });
    d
}

/// One set of values: the main set or a venue's.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PrefSet {
    vals: BTreeMap<&'static str, f64>,
    /// Selected application IDs; `None` is every application.
    pub apps: Option<Vec<i64>>,
    /// The rest of `<project_specific>`, kept as it was.
    pub project_specific: String,
}

impl PrefSet {
    pub fn get(&self, tag: &str) -> Option<f64> {
        self.vals.get(tag).copied()
    }
}

/// A whole document: the main set and the venue sets present.
#[derive(Debug, Clone, PartialEq)]
pub struct Prefs {
    pub subset: Subset,
    pub main: PrefSet,
    /// In [`VENUES`] order.
    pub venues: [Option<PrefSet>; 3],
    /// `<mod_time>` of global preferences; 0 when absent.
    pub mod_time: f64,
}

/// The editable fields of one subset, for one project.
pub struct PrefsForm {
    subset: Subset,
    defs: Vec<Def>,
    /// `(id, name)` of the applications a user can choose among; empty when
    /// there is only one.
    apps: Vec<(i64, String)>,
}

/// The not-in-use limits that take the in-use ones when the document does not
/// have them, and whether a value of 0 or less is read as 100 (the client
/// does so for the first two); for `niu_suspend_cpu_usage` 0 is "no limit".
const NIU: [(&str, &str, bool); 3] = [
    ("niu_max_ncpus_pct", "max_ncpus_pct", true),
    ("niu_cpu_usage_limit", "cpu_usage_limit", true),
    ("niu_suspend_cpu_usage", "suspend_cpu_usage", false),
];

/// `set_niu_prefs`, as the client does it (`GLOBAL_PREFS::parse` and
/// `parse_override`): a not-in-use limit the document does not have takes
/// the in-use one, and one it has stays as it is, a 0 included. Upstream's
/// PHP tests the value instead, so it cannot keep an explicit 0 for
/// `niu_suspend_cpu_usage`: a blank idle threshold would come back as the
/// in-use one and be written out as that on the next save.
fn set_niu_prefs(s: &mut PrefSet, present: &[&str]) {
    for (niu, inuse, zero_is_full) in NIU {
        if !present.contains(&niu) {
            let v = s.get(inuse).unwrap_or(0.0);
            s.vals.insert(niu, v);
        } else if zero_is_full && s.get(niu).unwrap_or(0.0) <= 0.0 {
            s.vals.insert(niu, 100.0);
        }
    }
}

/// The text of the first `<tag>...</tag>` in `buf`; `""` for `<tag/>`.
fn element<'a>(buf: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let empty = [format!("<{tag}/>"), format!("<{tag} />")];
    let at_open = buf.find(&open);
    let at_empty = empty.iter().filter_map(|e| buf.find(e.as_str())).min();
    match (at_open, at_empty) {
        (Some(o), e) if e.is_none_or(|e| o < e) => {
            let body = &buf[o + open.len()..];
            Some(match body.find(&format!("</{tag}>")) {
                Some(end) => &body[..end],
                None => body,
            })
        }
        (_, Some(_)) => Some(""),
        _ => None,
    }
}

/// `buf` without any `<tag>...</tag>` or `<tag/>` element.
fn remove_elements(buf: &str, tag: &str) -> String {
    let mut out = buf.to_string();
    for empty in [format!("<{tag}/>"), format!("<{tag} />")] {
        out = out.replace(&empty, "");
    }
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    while let Some(at) = out.find(&open) {
        let end = match out[at..].find(&close) {
            Some(e) => at + e + close.len(),
            None => out.len(),
        };
        out.replace_range(at..end, "");
    }
    out
}

/// Every `<app_id>` in `buf`.
fn app_ids(buf: &str) -> Vec<i64> {
    let mut out = Vec::new();
    let mut rest = buf;
    while let Some(at) = rest.find("<app_id>") {
        rest = &rest[at + "<app_id>".len()..];
        let end = rest.find('<').unwrap_or(rest.len());
        if let Ok(id) = rest[..end].trim().parse::<i64>() {
            if id > 0 && !out.contains(&id) {
                out.push(id);
            }
        }
    }
    out
}

/// PHP's string form of a number: no exponent, no trailing `.0`.
fn num_str(x: f64) -> String {
    let x = if x.is_finite() { x } else { 0.0 };
    let s = format!("{x}");
    if s == "-0" {
        "0".into()
    } else {
        s
    }
}

/// The `<project_specific>` text upstream's form leaves alone: everything
/// but the application selection and `max_jobs`.
fn project_specific_rest(buf: &str) -> String {
    let Some(inner) = element(buf, "project_specific") else {
        return String::new();
    };
    let mut rest = remove_elements(inner, "apps_selected");
    rest = remove_elements(&rest, "app_id");
    rest = remove_elements(&rest, "max_jobs");
    rest.lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The body of `<venue name="...">` in `xml`, if there is one.
fn venue_body<'a>(xml: &'a str, venue: &str) -> Option<&'a str> {
    let open = format!("<venue name=\"{venue}\">");
    let at = xml.find(&open)? + open.len();
    let body = &xml[at..];
    Some(match body.find("</venue") {
        Some(end) => &body[..end],
        None => body,
    })
}

impl PrefsForm {
    pub fn new(subset: Subset, project: &Project) -> PrefsForm {
        let (defs, apps) = match subset {
            Subset::Global => (global_defs(&project.config), Vec::new()),
            Subset::Project => {
                let apps: Vec<(i64, String)> = project
                    .catalog
                    .apps
                    .iter()
                    .map(|a| (a.id, a.user_friendly_name.clone()))
                    .collect();
                let apps = if apps.len() > 1 { apps } else { Vec::new() };
                (project_defs(project), apps)
            }
        };
        PrefsForm { subset, defs, apps }
    }

    /// `default_prefs_global` / `default_prefs_project`: the values before
    /// any document is read.
    fn raw_defaults(&self) -> PrefSet {
        let mut s = PrefSet::default();
        for d in &self.defs {
            let v = match d.kind {
                Kind::Bool { default, .. } => f64::from(u8::from(default)),
                Kind::Num { spec, .. } => spec.default,
                Kind::Hour { .. } => 0.0,
            };
            s.vals.insert(d.tag, v);
        }
        s
    }

    /// The values of an account with no preferences stored, as the form
    /// shows them.
    pub fn defaults(&self) -> PrefSet {
        self.parse_set("")
    }

    fn parse_set(&self, buf: &str) -> PrefSet {
        let mut s = self.raw_defaults();
        // The tags the document has, with a value that was read.
        let mut present: Vec<&str> = Vec::new();
        for d in &self.defs {
            let Some(text) = element(buf, d.tag) else {
                continue;
            };
            let text = text.trim();
            // `<tag></tag>` is 0 to the client's number parsers; `<tag/>` is
            // not read as a number.
            let is_bool = matches!(d.kind, Kind::Bool { .. });
            let text =
                if !is_bool && text.is_empty() && buf.contains(&format!("<{0}></{0}>", d.tag)) {
                    "0"
                } else {
                    text
                };
            let v = match d.kind {
                // `PREF_BOOL::xml_parse`: anything but "0" is true.
                Kind::Bool { .. } => f64::from(u8::from(text != "0")),
                Kind::Num { spec, .. } if spec.whole => {
                    // As `line::parse_int`, which the readers use: the
                    // leading integer, so "0.5" is 0.
                    match cb_proto::xml::line::parse_int(text, "") {
                        Some(v) => f64::from(v),
                        None => continue,
                    }
                }
                _ => match text.parse::<f64>() {
                    Ok(v) if v.is_finite() => v,
                    _ => continue,
                },
            };
            s.vals.insert(d.tag, v);
            present.push(d.tag);
        }
        if self.subset == Subset::Global {
            set_niu_prefs(&mut s, &present);
        } else {
            let ids = app_ids(buf);
            s.apps = (!ids.is_empty()).then_some(ids);
            s.project_specific = project_specific_rest(buf);
        }
        s
    }

    /// Reads a stored document. Empty or unreadable values take the
    /// defaults, as upstream's parser leaves them.
    pub fn parse(&self, xml: &str) -> Prefs {
        let main = self.parse_set(&extract_venue(xml, ""));
        let venues = VENUES.map(|v| venue_body(xml, v).map(|body| self.parse_set(body)));
        // The whole text as a number, or else what the scheduler reads there
        // (the number a text starts with), so that a time stored as `1e30junk`
        // is seen here as the scheduler sees it.
        let mod_time = element(xml, "mod_time")
            .and_then(|t| t.trim().parse::<f64>().ok())
            .filter(|t| t.is_finite())
            .or_else(|| line::parse_double(xml, "<mod_time>"))
            .unwrap_or(0.0);
        Prefs {
            subset: self.subset,
            main,
            venues,
            mod_time,
        }
    }

    fn write_set(&self, s: &PrefSet, out: &mut String) {
        let mut specific = String::new();
        for d in &self.defs {
            let v = s.get(d.tag).unwrap_or(0.0);
            let line = match d.kind {
                Kind::Bool { .. } => format!("<{0}>{1}</{0}>\n", d.tag, u8::from(v != 0.0)),
                Kind::Num { .. } => format!("<{0}>{1}</{0}>\n", d.tag, num_str(v)),
                Kind::Hour { other, .. } => {
                    if s.get(other).unwrap_or(0.0) == v {
                        continue;
                    }
                    format!("<{0}>{1}</{0}>\n", d.tag, num_str(v))
                }
            };
            if d.in_project_specific {
                if v != 0.0 {
                    specific.push_str(&line);
                }
            } else {
                out.push_str(&line);
            }
        }
        if self.subset == Subset::Project {
            if let Some(ids) = &s.apps {
                specific.push_str("<apps_selected>\n");
                for id in ids {
                    specific.push_str(&format!("<app_id>{id}</app_id>\n"));
                }
                specific.push_str("</apps_selected>\n");
            }
            let rest = s.project_specific.trim();
            if !rest.is_empty() {
                specific.push_str(rest);
                specific.push('\n');
            }
            if !specific.is_empty() {
                out.push_str(&format!(
                    "<project_specific>\n{specific}</project_specific>\n"
                ));
            }
        }
    }

    /// `global_prefs_make_xml` / `project_prefs_make_xml`. Global
    /// preferences get `now` as their `<mod_time>`, so that clients take
    /// them in place of their own copy, but never less than one second after
    /// the `<mod_time>` they replace (`p.mod_time`, as stored): the scheduler
    /// sends preferences only when the stored `<mod_time>` is greater than
    /// the one the client has, and two saves within a second would otherwise
    /// carry the same one. A stored `<mod_time>` more than
    /// [`MOD_TIME_FUTURE_SLACK_SECS`] after `now` is not believed (every
    /// reader clamps such a time to the current one, see the constant), so an
    /// absurd one, which an import can hold, is replaced by the save instead
    /// of being kept or overflowing.
    pub fn write(&self, p: &Prefs, now: i64) -> String {
        let believable = p.mod_time.is_finite()
            && p.mod_time <= now.saturating_add(MOD_TIME_FUTURE_SLACK_SECS) as f64;
        let stored = if believable {
            // A float to integer cast saturates, and the increment does too.
            (p.mod_time.floor() as i64).saturating_add(1)
        } else {
            now
        };
        let now = now.max(stored);
        let root = match self.subset {
            Subset::Global => "global_preferences",
            Subset::Project => "project_preferences",
        };
        let mut out = format!("<{root}>\n");
        if self.subset == Subset::Global {
            out.push_str(&format!("<mod_time>{now}</mod_time>\n"));
        }
        self.write_set(&p.main, &mut out);
        for (name, set) in VENUES.iter().zip(&p.venues) {
            if let Some(set) = set {
                out.push_str(&format!("<venue name=\"{name}\">\n"));
                self.write_set(set, &mut out);
                out.push_str("</venue>\n");
            }
        }
        out.push_str(&format!("</{root}>\n"));
        out
    }

    /// Values of one set as table rows, under group headings.
    pub fn show(&self, s: &PrefSet) -> String {
        let mut out = String::new();
        let mut group = "";
        for d in &self.defs {
            if d.group != group && self.subset == Subset::Global {
                group = d.group;
                out.push_str(&format!(
                    "<tr><th colspan=\"2\">{}</th></tr>\n",
                    html_escape(group)
                ));
            }
            let v = s.get(d.tag).unwrap_or(0.0);
            let shown = match d.kind {
                Kind::Bool { invert, .. } => {
                    if (v != 0.0) != invert {
                        "yes".to_string()
                    } else {
                        "no".to_string()
                    }
                }
                Kind::Num { spec, optional } => {
                    let mut x = v / spec.scale;
                    if !(spec.min..=spec.max).contains(&x) && !(optional && x == 0.0) {
                        x = spec.default;
                    }
                    if x == 0.0 {
                        "---".into()
                    } else {
                        format!("{} {}", num_str(x), html_escape(spec.unit))
                    }
                }
                Kind::Hour { other, start } => {
                    if !start {
                        continue;
                    }
                    let end = s.get(other).unwrap_or(0.0);
                    if end == v {
                        "---".into()
                    } else {
                        format!("{}:00 and {}:00", num_str(v), num_str(end))
                    }
                }
            };
            out.push_str(&format!(
                "<tr><td>{}</td><td>{shown}</td></tr>\n",
                html_escape(d.label)
            ));
        }
        if self.subset == Subset::Project && !self.apps.is_empty() {
            let names: Vec<String> = self
                .apps
                .iter()
                .filter(|(id, _)| s.apps.as_ref().is_none_or(|ids| ids.contains(id)))
                .map(|(_, name)| html_escape(name))
                .collect();
            let shown = if s.apps.is_none() {
                "all".to_string()
            } else if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            };
            out.push_str(&format!("<tr><td>Applications</td><td>{shown}</td></tr>\n"));
        }
        out
    }

    /// Form fields for one set, as table rows. With `sent`, the fields show
    /// what a browser submitted, whether or not it was accepted, so a page
    /// that reports an error keeps every edit; `bad` lists the tags (and
    /// [`APPS_FIELD`]) that were refused, which get a mark.
    ///
    /// Each control has an ID, `pref_<tag>`, and a `<label>`. A control that
    /// is one of several with the same caption (the "not in use" limits
    /// repeat the "in use" ones, and three disk fields start "Use no more
    /// than") is also named by its group heading and its unit, through
    /// `aria-labelledby`. The two selects of an hour range have a label each,
    /// hidden from sight, for the start and the end.
    pub fn form(&self, s: &PrefSet, sent: Option<&crate::site::Params>, bad: &[&str]) -> String {
        let mut out = String::new();
        let mut group = "";
        let mut group_id = String::new();
        let mut groups = 0;
        for d in &self.defs {
            if d.group != group && self.subset == Subset::Global {
                group = d.group;
                groups += 1;
                group_id = format!("pref_group_{groups}");
                out.push_str(&format!(
                    "<tr><th colspan=\"2\" id=\"{group_id}\">{}</th></tr>\n",
                    html_escape(group)
                ));
            }
            let id = format!("pref_{}", d.tag);
            let label_id = format!("{id}_label");
            let v = s.get(d.tag).unwrap_or(0.0);
            let sent_text = sent.and_then(|q| q.get(d.tag));
            let refused = |tag: &str| bad.contains(&tag);
            let (mark, invalid) = if refused(d.tag) {
                (
                    format!(" <strong id=\"{id}_error\">(not accepted)</strong>"),
                    format!(" aria-invalid=\"true\" aria-describedby=\"{id}_error\""),
                )
            } else {
                (String::new(), String::new())
            };
            let name_of = |parts: &[&str]| {
                let ids: Vec<&str> = parts.iter().copied().filter(|p| !p.is_empty()).collect();
                format!(" aria-labelledby=\"{}\"", ids.join(" "))
            };
            let caption = html_escape(d.label);
            let (label, field) = match d.kind {
                Kind::Bool { invert, .. } => {
                    let on = match sent {
                        Some(q) => q.get(d.tag).is_some_and(|x| !x.is_empty()),
                        None => (v != 0.0) != invert,
                    };
                    (
                        format!("<label id=\"{label_id}\" for=\"{id}\">{caption}</label>"),
                        format!(
                            "<input type=\"checkbox\" id=\"{id}\"{} name=\"{}\" value=\"1\"{}>{mark}",
                            name_of(&[&group_id, &label_id]),
                            d.tag,
                            if on { " checked" } else { "" }
                        ),
                    )
                }
                Kind::Num { spec, optional } => {
                    let text = match sent_text {
                        Some(t) => t.clone(),
                        None if optional && v == 0.0 => String::new(),
                        None => num_str(v / spec.scale),
                    };
                    let unit_id = format!("{id}_unit");
                    let unit = if spec.unit.is_empty() {
                        String::new()
                    } else {
                        format!(" <span id=\"{unit_id}\">{}</span>", html_escape(spec.unit))
                    };
                    let unit_ref = if spec.unit.is_empty() { "" } else { &unit_id };
                    (
                        format!("<label id=\"{label_id}\" for=\"{id}\">{caption}</label>"),
                        format!(
                            "<input type=\"text\" size=\"8\" id=\"{id}\"{}{invalid} name=\"{}\" value=\"{}\">{unit}{}{mark}",
                            name_of(&[&group_id, &label_id, unit_ref]),
                            d.tag,
                            html_escape(&text),
                            if optional {
                                " (blank for no limit)"
                            } else {
                                ""
                            }
                        ),
                    )
                }
                Kind::Hour { other, start } => {
                    if !start {
                        continue;
                    }
                    let hour = |tag: &str, current: f64| {
                        sent.and_then(|q| q.get(tag))
                            .and_then(|t| t.trim().parse::<u8>().ok())
                            .filter(|h| *h < 24)
                            .map_or(current, f64::from)
                    };
                    let select = |tag: &str, cur: f64, which: &str| {
                        let mut x = format!(
                            "<label for=\"pref_{tag}\" style=\"{HIDDEN}\">{caption}: {which} hour</label>\
                             <select id=\"pref_{tag}\" name=\"{tag}\">"
                        );
                        for h in 0..24 {
                            let sel = if f64::from(h) == cur { " selected" } else { "" };
                            x.push_str(&format!("<option value=\"{h}\"{sel}>{h}:00</option>"));
                        }
                        x.push_str("</select>");
                        x
                    };
                    let mark = if refused(d.tag) || refused(other) {
                        " <strong>(not accepted)</strong>".to_string()
                    } else {
                        String::new()
                    };
                    (
                        caption.clone(),
                        format!(
                            "{} and {} <small>(the same hour for no restriction)</small>{mark}",
                            select(d.tag, hour(d.tag, v), "start"),
                            select(other, hour(other, s.get(other).unwrap_or(0.0)), "end")
                        ),
                    )
                }
            };
            out.push_str(&format!("<tr><td>{label}</td><td>{field}</td></tr>\n"));
        }
        if self.subset == Subset::Project && !self.apps.is_empty() {
            let mut boxes = String::new();
            for (id, name) in &self.apps {
                let on = match sent {
                    Some(q) => q.contains_key(&format!("app_id_{id}")),
                    None => s.apps.as_ref().is_none_or(|ids| ids.contains(id)),
                };
                boxes.push_str(&format!(
                    "<label><input type=\"checkbox\" name=\"app_id_{id}\" value=\"1\"{}> {}</label><br>",
                    if on { " checked" } else { "" },
                    html_escape(name)
                ));
            }
            let mark = if bad.contains(&APPS_FIELD) {
                " <strong>(not accepted: choose at least one application)</strong>"
            } else {
                ""
            };
            out.push_str(&format!(
                "<tr><td id=\"pref_apps_label\">Run only the selected applications</td>\
                 <td><div role=\"group\" aria-labelledby=\"pref_apps_label\">{boxes}</div>{mark}</td></tr>\n"
            ));
        }
        out
    }

    /// The fields a browser sends for [`PrefsForm::form`] of `s` left as it
    /// is.
    pub fn form_values(&self, s: &PrefSet) -> crate::site::Params {
        let mut q = crate::site::Params::new();
        for d in &self.defs {
            let v = s.get(d.tag).unwrap_or(0.0);
            match d.kind {
                Kind::Bool { invert, .. } => {
                    if (v != 0.0) != invert {
                        q.insert(d.tag.to_string(), "1".into());
                    }
                }
                Kind::Num { spec, optional } => {
                    let text = if optional && v == 0.0 {
                        String::new()
                    } else {
                        num_str(v / spec.scale)
                    };
                    q.insert(d.tag.to_string(), text);
                }
                Kind::Hour { .. } => {
                    q.insert(d.tag.to_string(), num_str(v));
                }
            }
        }
        for (id, _) in &self.apps {
            if s.apps.as_ref().is_none_or(|ids| ids.contains(id)) {
                q.insert(format!("app_id_{id}"), "1".into());
            }
        }
        q
    }

    /// `prefs_global_parse_form` / `prefs_resource_parse_form`: the
    /// submitted values over `base`. On failure, the tags that were refused.
    pub fn parse_form(
        &self,
        base: &PrefSet,
        q: &crate::site::Params,
    ) -> Result<PrefSet, Vec<&'static str>> {
        let mut s = base.clone();
        let mut bad = Vec::new();
        for d in &self.defs {
            match d.kind {
                Kind::Bool { invert, .. } => {
                    let on = q.get(d.tag).is_some_and(|v| !v.is_empty());
                    s.vals.insert(d.tag, f64::from(u8::from(on != invert)));
                }
                Kind::Num { spec, .. } => {
                    let text = q.get(d.tag).map(|t| t.trim()).unwrap_or_default();
                    let text = if text.is_empty() { "0" } else { text };
                    match text.parse::<f64>() {
                        Ok(x)
                            if (spec.min..=spec.max).contains(&x)
                                && (!spec.whole || x.fract() == 0.0) =>
                        {
                            s.vals.insert(d.tag, x * spec.scale);
                        }
                        _ => bad.push(d.tag),
                    }
                }
                Kind::Hour { .. } => match q.get(d.tag).and_then(|t| t.trim().parse::<u8>().ok()) {
                    Some(h) if h < 24 => {
                        s.vals.insert(d.tag, f64::from(h));
                    }
                    _ => bad.push(d.tag),
                },
            }
        }
        if self.subset == Subset::Project && !self.apps.is_empty() {
            let chosen: Vec<i64> = self
                .apps
                .iter()
                .map(|(id, _)| *id)
                .filter(|id| q.contains_key(&format!("app_id_{id}")))
                .collect();
            if chosen.is_empty() {
                // No `<app_id>` means every application to the scheduler (and
                // to upstream's form), so "none" cannot be stored.
                bad.push(APPS_FIELD);
            }
            // As upstream, a selection of every application is no selection.
            s.apps = (chosen.len() < self.apps.len()).then_some(chosen);
        }
        if bad.is_empty() {
            Ok(s)
        } else {
            Err(bad)
        }
    }
}

/// `bad_xml` in upstream `am_set_info.php`: the text, trimmed, starts with
/// `<root>`, ends with `</root>` and has a line break. Returns what is
/// wrong.
pub fn bad_xml(text: &str, root: &str) -> Option<&'static str> {
    let text = text.trim();
    if !text.starts_with(&format!("<{root}>")) {
        return Some("No start tag");
    }
    if !text.ends_with(&format!("</{root}>")) {
        return Some("No end tag");
    }
    if !text.contains('\n') {
        return Some("No CR");
    }
    None
}

/// Project preferences from an account manager keep the project-specific
/// part already stored when they carry none of their own, as upstream's
/// `am_set_info.php` does.
pub fn keep_project_specific(new: &str, old: &str) -> String {
    if new.to_ascii_lowercase().contains("<project_specific>") {
        return new.to_string();
    }
    let Some(start) = old.find("<project_specific>") else {
        return new.to_string();
    };
    let end = old[start..]
        .find("</project_specific>")
        .map(|e| start + e + "</project_specific>".len())
        .unwrap_or(old.len());
    let block = &old[start..end];
    match new.find("<project_preferences>") {
        Some(at) => {
            let at = at + "<project_preferences>".len();
            format!("{}\n{block}{}", &new[..at], &new[at..])
        }
        None => new.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{App, AppVersion};

    fn project(apps: usize, beta: bool) -> Project {
        let mut p = Project::default();
        for i in 0..apps {
            p.catalog.apps.push(App {
                id: i as i64 + 1,
                name: format!("app{i}"),
                user_friendly_name: format!("App <{i}>"),
                beta: i32::from(beta && i == 0),
                ..Default::default()
            });
            p.catalog.app_versions.push(AppVersion {
                id: i as i64 + 1,
                appid: i as i64 + 1,
                ..Default::default()
            });
        }
        p
    }

    fn q(pairs: &[(&str, &str)]) -> crate::site::Params {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn with_mod_time(form: &PrefsForm, text: &str) -> Prefs {
        form.parse(&format!(
            "<global_preferences>\n<mod_time>{text}</mod_time>\n</global_preferences>\n"
        ))
    }

    fn written_mod_time(form: &PrefsForm, p: &Prefs, now: i64) -> i64 {
        let out = form.write(p, now);
        let at = out.find("<mod_time>").unwrap() + "<mod_time>".len();
        out[at..at + out[at..].find('<').unwrap()].parse().unwrap()
    }

    #[test]
    fn a_save_stamps_a_sane_mod_time_over_whatever_was_stored() {
        let form = PrefsForm::new(Subset::Global, &project(1, false));
        let now = 1_800_000_000;
        // The normal cases: a past time gives now; the same second gives one
        // more, so two saves within a second still differ.
        assert_eq!(
            written_mod_time(&form, &with_mod_time(&form, "1700000000"), now),
            now
        );
        assert_eq!(
            written_mod_time(&form, &with_mod_time(&form, &now.to_string()), now),
            now + 1
        );
        // A time a little ahead (clocks differ) is kept ahead; one more than
        // a day ahead is not believed.
        assert_eq!(
            written_mod_time(&form, &with_mod_time(&form, &(now + 500).to_string()), now),
            now + 501
        );
        assert_eq!(
            written_mod_time(
                &form,
                &with_mod_time(&form, &(now + MOD_TIME_FUTURE_SLACK_SECS + 1).to_string()),
                now
            ),
            now
        );
        // Absurd stored values: no overflow, and the result is near now.
        for text in [
            "1e30",
            "-1e30",
            "NaN",
            "nan",
            "inf",
            "-inf",
            "9223372036854775807",
            "9223372036854775806",
            "9.3e18",
            "-9223372036854775808",
            "-9223372036854775809",
            "",
            "garbage",
            "1e30junk",
            "9223372036854775807x",
            "1e30 1e30",
        ] {
            let p = with_mod_time(&form, text);
            assert!(p.mod_time.is_finite(), "{text}: {}", p.mod_time);
            let t = written_mod_time(&form, &p, now);
            assert!(
                (now..=now + 1).contains(&t),
                "{text} gave {t} for a save at {now}"
            );
        }
        // Even a save at the top of the range cannot overflow.
        let p = with_mod_time(&form, "9223372036854775807");
        assert_eq!(written_mod_time(&form, &p, i64::MAX), i64::MAX);
        assert_eq!(
            mod_time_error("<global_preferences><mod_time>1e30</mod_time>", now),
            Some("mod_time is in the future")
        );
    }

    #[test]
    fn mod_time_of_a_document_from_an_account_manager_is_checked() {
        let now = 1_800_000_000;
        let doc = |t: &str| {
            format!("<global_preferences>\n<mod_time>{t}</mod_time>\n</global_preferences>")
        };
        // Accepted: absent, zero, negative, in the past, a little ahead.
        assert_eq!(
            mod_time_error("<global_preferences></global_preferences>", now),
            None
        );
        for ok in [
            "0",
            "-5",
            "-1e30",
            "1700000000.5",
            "1800000000",
            "1800086400",
        ] {
            assert_eq!(mod_time_error(&doc(ok), now), None, "{ok}");
        }
        // Refused: more than a day ahead, or not a finite number.
        for bad in [
            "1800086401",
            "1e30",
            "9223372036854775807",
            "inf",
            "-inf",
            "NaN",
        ] {
            assert!(mod_time_error(&doc(bad), now).is_some(), "{bad}");
        }
        // Text that is not a number is not this check's business.
        assert_eq!(mod_time_error(&doc("soon"), now), None);
        // A number with text after it: the scheduler reads the number.
        for bad in [
            "1e30junk",
            "9223372036854775807x",
            "1800086401 soon",
            "1e30 1e30",
        ] {
            assert_eq!(
                mod_time_error(&doc(bad), now),
                Some("mod_time is in the future"),
                "{bad}"
            );
        }
        for ok in ["1700000000junk", "5x", "-1e30x"] {
            assert_eq!(mod_time_error(&doc(ok), now), None, "{ok}");
        }
    }

    #[test]
    fn global_prefs_from_a_client_are_read() {
        let form = PrefsForm::new(Subset::Global, &project(1, false));
        let xml = "<global_preferences>\n   <mod_time>1700000000.000000</mod_time>\n   \
                   <run_on_batteries>0</run_on_batteries>\n   <run_if_user_active/>\n   \
                   <max_ncpus_pct>50.000000</max_ncpus_pct>\n   <start_hour>8</start_hour>\n   \
                   <end_hour>17</end_hour>\n   <max_bytes_sec_down>102400</max_bytes_sec_down>\n\
                   <venue name=\"work\">\n<max_ncpus_pct>25</max_ncpus_pct>\n</venue>\n</global_preferences>\n";
        let p = form.parse(xml);
        assert_eq!(p.mod_time, 1_700_000_000.0);
        assert_eq!(p.main.get("max_ncpus_pct"), Some(50.0));
        assert_eq!(p.main.get("run_if_user_active"), Some(1.0));
        assert_eq!(p.main.get("run_on_batteries"), Some(0.0));
        // Not-in-use limits that are unset take the in-use ones.
        assert_eq!(p.main.get("niu_max_ncpus_pct"), Some(50.0));
        assert_eq!(p.main.get("work_buf_min_days"), Some(0.1));
        assert!(p.venues[0].is_none());
        assert_eq!(
            p.venues[1].as_ref().unwrap().get("max_ncpus_pct"),
            Some(25.0)
        );
        assert!(p.venues[2].is_none());

        let out = form.write(&p, 1_800_000_000);
        assert!(out.starts_with("<global_preferences>\n<mod_time>1800000000</mod_time>\n"));
        assert!(out.contains("<max_ncpus_pct>50</max_ncpus_pct>\n"));
        assert!(out.contains("<start_hour>8</start_hour>\n<end_hour>17</end_hour>\n"));
        assert!(
            !out.contains("<net_start_hour>"),
            "equal hours are not written"
        );
        assert!(out.contains("<max_bytes_sec_down>102400</max_bytes_sec_down>"));
        assert!(out.contains("<work_buf_min_days>0.1</work_buf_min_days>"));
        assert!(out.contains("<venue name=\"work\">\n<idle_time_to_run>"));
        assert!(out.ends_with("</venue>\n</global_preferences>\n"));
        // What the scheduler and client read back is what was written.
        let again = form.parse(&out);
        assert_eq!(again.main, p.main);
        assert_eq!(again.venues, p.venues);
        assert_eq!(
            cb_proto::sched_request::GlobalPrefs::parse(&out, "work").max_ncpus_pct,
            25.0
        );
    }

    #[test]
    fn global_form_round_trip_and_ranges() {
        let form = PrefsForm::new(Subset::Global, &project(1, false));
        let base = form.defaults();
        let html = form.form(&base, None, &[]);
        assert!(
            html.contains("name=\"run_if_user_active\" value=\"1\">"),
            "inverted: unchecked"
        );
        assert!(html.contains("name=\"max_bytes_sec_down\" value=\"\">"));

        // An unchanged form gives back the same values.
        assert_eq!(
            form.parse_form(&base, &form.form_values(&base)).unwrap(),
            base
        );

        let mut fields = form.form_values(&base);
        for (k, v) in [
            ("max_ncpus_pct", "75"),
            ("max_bytes_sec_down", "100"),
            ("net_start_hour", "22"),
            ("net_end_hour", "6"),
            ("run_on_batteries", "1"),
        ] {
            fields.insert(k.into(), v.into());
        }
        fields.remove("run_if_user_active");
        let s = form.parse_form(&base, &fields).unwrap();
        assert_eq!(s.get("max_ncpus_pct"), Some(75.0));
        assert_eq!(s.get("max_bytes_sec_down"), Some(102_400.0));
        // Ticked "suspend when on battery" stores run_on_batteries = 0.
        assert_eq!(s.get("run_on_batteries"), Some(0.0));
        // Unticked "suspend all computing" stores run_if_user_active = 1.
        assert_eq!(s.get("run_if_user_active"), Some(1.0));
        let out = form.write(
            &Prefs {
                subset: Subset::Global,
                main: s,
                venues: [None, None, None],
                mod_time: 0.0,
            },
            5,
        );
        assert!(
            out.contains("<net_start_hour>22</net_start_hour>\n<net_end_hour>6</net_end_hour>\n")
        );

        for (k, v) in [
            ("cpu_usage_limit", "150"),
            ("work_buf_min_days", "x"),
            ("idle_time_to_run", ""),
        ] {
            fields.insert(k.into(), v.into());
        }
        let bad = form.parse_form(&base, &fields).unwrap_err();
        assert_eq!(
            bad,
            vec!["idle_time_to_run", "cpu_usage_limit", "work_buf_min_days"]
        );
        let html = form.form(&base, Some(&fields), &bad);
        assert!(html.contains("name=\"work_buf_min_days\" value=\"x\">"));
        assert!(html.contains("(not accepted)"));
    }

    #[test]
    fn project_prefs_keep_what_the_form_does_not_edit() {
        let p = project(3, true);
        let form = PrefsForm::new(Subset::Project, &p);
        let xml = "<project_preferences>\n<resource_share>50</resource_share>\n<no_cpu>0</no_cpu>\n\
                   <project_specific>\n<color_scheme>Tahiti</color_scheme>\n<apps_selected>\n<app_id>2</app_id>\n\
                   </apps_selected>\n<max_jobs>3</max_jobs>\n</project_specific>\n\
                   <venue name=\"home\">\n<resource_share>10</resource_share>\n</venue>\n</project_preferences>\n";
        let prefs = form.parse(xml);
        assert_eq!(prefs.main.get("resource_share"), Some(50.0));
        assert_eq!(prefs.main.get("max_jobs"), Some(3.0));
        assert_eq!(prefs.main.apps, Some(vec![2]));
        assert_eq!(
            prefs.main.project_specific,
            "<color_scheme>Tahiti</color_scheme>"
        );
        assert_eq!(
            prefs.venues[0].as_ref().unwrap().get("resource_share"),
            Some(10.0)
        );
        assert_eq!(prefs.venues[0].as_ref().unwrap().apps, None);

        let html = form.form(&prefs.main, None, &[]);
        assert!(html.contains("App &lt;0&gt;"));
        assert!(html.contains("name=\"allow_beta_work\""));
        assert!(
            !html.contains("no_cuda"),
            "no NVIDIA version, no NVIDIA flag"
        );

        let mut fields = form.form_values(&prefs.main);
        assert_eq!(fields.get("max_jobs").map(String::as_str), Some("3"));
        fields.remove("app_id_2");
        for (k, v) in [
            ("resource_share", "200"),
            ("no_cpu", "1"),
            ("app_id_1", "1"),
            ("app_id_3", "1"),
            ("max_jobs", ""),
        ] {
            fields.insert(k.into(), v.into());
        }
        let s = form.parse_form(&prefs.main, &fields).unwrap();
        let mut next = prefs.clone();
        next.main = s;
        let out = form.write(&next, 0);
        assert!(!out.contains("mod_time"));
        assert!(out.contains("<resource_share>200</resource_share>\n<no_cpu>0</no_cpu>\n"));
        assert!(out.contains(
            "<project_specific>\n<apps_selected>\n<app_id>1</app_id>\n<app_id>3</app_id>\n</apps_selected>\n\
             <color_scheme>Tahiti</color_scheme>\n</project_specific>\n"
        ));
        assert!(!out.contains("max_jobs"));
        assert!(out.contains("<venue name=\"home\">\n<resource_share>10</resource_share>"));

        // Every application ticked is no selection.
        let all = form
            .parse_form(
                &prefs.main,
                &q(&[
                    ("resource_share", "1"),
                    ("max_jobs", "0"),
                    ("app_id_1", "1"),
                    ("app_id_2", "1"),
                    ("app_id_3", "1"),
                ]),
            )
            .unwrap();
        assert_eq!(all.apps, None);
    }

    #[test]
    fn beta_work_is_offered_for_a_beta_application_or_version() {
        let offered =
            |p: &Project| PrefsForm::new(Subset::Project, p).form(&PrefSet::default(), None, &[]);
        assert!(!offered(&project(2, false)).contains("allow_beta_work"));
        assert!(offered(&project(2, true)).contains("name=\"allow_beta_work\""));
        // A version that is beta under an application that is not.
        let mut p = project(2, false);
        p.catalog.app_versions[1].beta = 1;
        assert!(offered(&p).contains("name=\"allow_beta_work\""));
    }

    #[test]
    fn whole_number_fields_refuse_fractions() {
        let project_form = PrefsForm::new(Subset::Project, &project(1, false));
        let base = project_form.defaults();
        let sent = |v: &str| q(&[("resource_share", "100"), ("max_jobs", v)]);
        for bad in ["0.5", "2.5", "1e-1", "-1", "1001", "x", "nan"] {
            assert_eq!(
                project_form.parse_form(&base, &sent(bad)),
                Err(vec!["max_jobs"]),
                "{bad}"
            );
        }
        for (good, stored) in [
            ("0", "0"),
            ("", "0"),
            ("3", "3"),
            (" 3.0 ", "3"),
            ("1e2", "100"),
        ] {
            let s = project_form.parse_form(&base, &sent(good)).unwrap();
            assert_eq!(
                s.get("max_jobs").map(num_str).as_deref(),
                Some(stored),
                "{good}"
            );
        }
        // The scheduler reads the leading integer of what is stored, and so
        // does the form: a stored fraction is not shown as one.
        let xml = "<project_preferences>\n<project_specific>\n<max_jobs>0.5</max_jobs>\n</project_specific>\n</project_preferences>\n";
        assert_eq!(project_form.parse(xml).main.get("max_jobs"), Some(0.0));
        let xml = xml.replace("0.5", "7.9");
        assert_eq!(project_form.parse(&xml).main.get("max_jobs"), Some(7.0));

        // `max_cpus` (read as an integer by the client) is not offered;
        // `daily_xfer_period_days` is, and is whole.
        let global = PrefsForm::new(Subset::Global, &project(1, false));
        assert!(global.defs.iter().all(|d| d.tag != "max_cpus"));
        let mut fields = global.form_values(&global.defaults());
        fields.insert("daily_xfer_period_days".into(), "1.5".into());
        assert_eq!(
            global.parse_form(&global.defaults(), &fields),
            Err(vec!["daily_xfer_period_days"])
        );
        fields.insert("daily_xfer_period_days".into(), "2".into());
        assert!(global.parse_form(&global.defaults(), &fields).is_ok());
    }

    #[test]
    fn no_application_chosen_is_refused() {
        let form = PrefsForm::new(Subset::Project, &project(3, false));
        let base = form.defaults();
        let none = q(&[("resource_share", "100"), ("max_jobs", "0")]);
        assert_eq!(form.parse_form(&base, &none), Err(vec![APPS_FIELD]));
        let html = form.form(&base, Some(&none), &[APPS_FIELD]);
        assert!(html.contains("choose at least one application"));
        assert!(!html.contains(" checked"), "what was sent is shown: {html}");
        let mut one = none.clone();
        one.insert("app_id_2".into(), "1".into());
        assert_eq!(form.parse_form(&base, &one).unwrap().apps, Some(vec![2]));
        // What the form writes for a selection is what the scheduler reads.
        let mut next = form.parse("");
        next.main = form.parse_form(&base, &one).unwrap();
        let xml = form.write(&next, 0);
        assert!(xml.contains("<apps_selected>\n<app_id>2</app_id>\n</apps_selected>"));
    }

    #[test]
    fn controls_have_ids_and_labels() {
        let form = PrefsForm::new(Subset::Global, &project(1, false));
        let html = form.form(&form.defaults(), None, &[]);
        assert!(html.contains(
            "<label id=\"pref_work_buf_min_days_label\" for=\"pref_work_buf_min_days\">Store at least</label>"
        ));
        assert!(html.contains(
            "id=\"pref_work_buf_min_days\" aria-labelledby=\"pref_group_3 pref_work_buf_min_days_label pref_work_buf_min_days_unit\""
        ));
        // Three disk fields start "Use no more than": the unit tells them apart.
        assert!(html.contains("id=\"pref_disk_max_used_gb_unit\">GB</span>"));
        assert!(html.contains("id=\"pref_disk_max_used_pct_unit\">% of total</span>"));
        assert!(html.contains("aria-labelledby=\"pref_group_4 pref_disk_max_used_gb_label pref_disk_max_used_gb_unit\""));
        // An hour range: one label for each end.
        assert!(html.contains("<label for=\"pref_start_hour\" style=\""));
        assert!(html.contains("Compute only between: start hour</label><select id=\"pref_start_hour\" name=\"start_hour\">"));
        assert!(html.contains(
            "Compute only between: end hour</label><select id=\"pref_end_hour\" name=\"end_hour\">"
        ));
        // A refused value is tied to its message.
        let mut sent = form.form_values(&form.defaults());
        sent.insert("cpu_usage_limit".into(), "500".into());
        let html = form.form(&form.defaults(), Some(&sent), &["cpu_usage_limit"]);
        assert!(
            html.contains("aria-invalid=\"true\" aria-describedby=\"pref_cpu_usage_limit_error\"")
        );
        assert!(html.contains("<strong id=\"pref_cpu_usage_limit_error\">(not accepted)</strong>"));

        let form = PrefsForm::new(Subset::Project, &project(2, false));
        let html = form.form(&form.defaults(), None, &[]);
        assert!(html.contains("<div role=\"group\" aria-labelledby=\"pref_apps_label\">"));
        assert!(html.contains("<td id=\"pref_apps_label\">Run only the selected applications</td>"));
    }

    #[test]
    fn an_explicit_zero_is_not_an_absent_value() {
        let form = PrefsForm::new(Subset::Global, &project(1, false));
        let doc = |extra: &str| {
            format!(
                "<global_preferences>\n<suspend_cpu_usage>25</suspend_cpu_usage>\n\
                 <max_ncpus_pct>40</max_ncpus_pct>\n{extra}</global_preferences>\n"
            )
        };
        // Absent: the in-use value. Present, 0: no limit, as the client reads it.
        let absent = form.parse(&doc(""));
        assert_eq!(absent.main.get("niu_suspend_cpu_usage"), Some(25.0));
        assert_eq!(absent.main.get("niu_max_ncpus_pct"), Some(40.0));
        let zero = form.parse(&doc("<niu_suspend_cpu_usage>0</niu_suspend_cpu_usage>\n"));
        assert_eq!(zero.main.get("niu_suspend_cpu_usage"), Some(0.0));
        // Writing and reading again keeps it.
        let again = form.parse(&form.write(&zero, 1));
        assert_eq!(again.main.get("niu_suspend_cpu_usage"), Some(0.0));
        assert_eq!(again.main, zero.main);
        // Not-in-use CPU limits of 0 or less are 100 to the client.
        let full = form.parse(&doc("<niu_max_ncpus_pct>0</niu_max_ncpus_pct>\n"));
        assert_eq!(full.main.get("niu_max_ncpus_pct"), Some(100.0));
        // A venue is read the same way.
        let venue = form.parse(&format!(
            "{}<venue name=\"work\">\n<suspend_cpu_usage>30</suspend_cpu_usage>\n\
             <niu_suspend_cpu_usage>0</niu_suspend_cpu_usage>\n</venue>\n",
            doc("")
        ));
        assert_eq!(
            venue.venues[1]
                .as_ref()
                .unwrap()
                .get("niu_suspend_cpu_usage"),
            Some(0.0)
        );
        // An empty number element is 0 to the client; a self-closed one is
        // not read.
        let empty = form.parse(&doc(
            "<work_buf_min_days></work_buf_min_days>\n<disk_interval/>\n",
        ));
        assert_eq!(empty.main.get("work_buf_min_days"), Some(0.0));
        assert_eq!(empty.main.get("disk_interval"), Some(60.0));
        let flag = form.parse(&doc("<run_on_batteries></run_on_batteries>\n"));
        assert_eq!(flag.main.get("run_on_batteries"), Some(1.0));
    }

    #[test]
    fn element_reading() {
        assert_eq!(element("<a>1</a>", "a"), Some("1"));
        assert_eq!(element("<a/><a>1</a>", "a"), Some(""));
        assert_eq!(element("<ab>1</ab>", "a"), None);
        assert_eq!(remove_elements("x<a>1</a>y<a/>z", "a"), "xyz");
        assert_eq!(
            app_ids("<app_id>3</app_id><app_id>x</app_id><app_id>3</app_id>"),
            vec![3]
        );
        assert_eq!(num_str(0.1), "0.1");
        assert_eq!(num_str(100.0), "100");
        assert_eq!(num_str(1e21), "1000000000000000000000");
    }

    #[test]
    fn account_manager_documents() {
        assert_eq!(bad_xml("<a>\n</a>", "a"), None);
        assert_eq!(bad_xml("<a></a>", "a"), Some("No CR"));
        assert_eq!(bad_xml("<b>\n</a>", "a"), Some("No start tag"));
        assert_eq!(bad_xml("<a>\n</b>", "a"), Some("No end tag"));
        let old = "<project_preferences>\n<project_specific>\n<x>1</x>\n</project_specific>\n</project_preferences>\n";
        let new =
            "<project_preferences>\n<resource_share>5</resource_share>\n</project_preferences>\n";
        let kept = keep_project_specific(new, old);
        assert_eq!(
            kept,
            "<project_preferences>\n<project_specific>\n<x>1</x>\n</project_specific>\n<resource_share>5</resource_share>\n</project_preferences>\n"
        );
        assert_eq!(
            keep_project_specific(new, "<project_preferences>\n</project_preferences>"),
            new
        );
        let own = "<project_preferences>\n<project_specific>\n<y/>\n</project_specific>\n</project_preferences>\n";
        assert_eq!(keep_project_specific(own, old), own);
    }
}
