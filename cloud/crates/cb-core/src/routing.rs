//! Routing rules: which app versions a workunit may be sent with.
//!
//! The equivalent of the hook a stock project edits in `sched/sched_customize.cpp`,
//! `wu_is_infeasible_custom`, which `wu_is_infeasible_fast` in `sched/sched_check.cpp`
//! calls for each candidate job with the workunit, the app and the app version chosen
//! for the host. No project code is compiled in here, so the rules are data: a JSON
//! document in the `config` table (setting `routing_rules`), loaded with the other
//! settings and cached with them. A project that calibrates hosts with known-answer
//! jobs, for example, can bind each job to one build.
//!
//! A rule selects workunits of one app, by an optional name pattern and an optional
//! batch range, and lists the app versions those workunits may be sent with. The
//! first rule that selects a workunit decides; a workunit no rule selects is not
//! restricted. A version is allowed when one entry of the rule's `allow` list
//! matches its platform, plan class and version number. An empty `allow` list
//! allows nothing, so a last rule that selects everything of an app with no entries
//! refuses what no earlier rule admitted.
//!
//! Differences from the stock hook are listed as D38 in `docs/compatibility.md`.

use serde::{Deserialize, Serialize};

use crate::{CoreError, CoreResult};

/// The most rules, and the most entries in one rule's `allow` list. The rule
/// limit keeps the text a rule adds to a query small (see `sql_condition`).
pub const MAX_RULES: usize = 64;
pub const MAX_ALLOW: usize = 256;
/// The longest name pattern: D1 refuses a `GLOB` or `LIKE` pattern over 50 bytes.
pub const MAX_PATTERN: usize = 50;

/// One entry of a rule's `allow` list. Every field that is set must match.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Allow {
    /// The version's platform name; any when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// The version's plan class; any when absent, and the empty string
    /// means only the version with no plan class.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_class: Option<String>,
    /// Lowest `version_num`; 0 for no bound.
    pub min_version: i32,
    /// Highest `version_num`; 0 for no bound.
    pub max_version: i32,
}

/// One rule.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingRule {
    /// The app's name.
    pub app: String,
    /// A pattern for the workunit name: `*` matches any run of characters and `?`
    /// any one. Absent: every workunit of the app.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Lowest `workunit.batch`; 0 for no bound.
    pub min_batch: i64,
    /// Highest `workunit.batch`; 0 for no bound.
    pub max_batch: i64,
    /// Also apply the rule to assigned jobs. The stock hook is not reached for
    /// them (`send_assigned_job` in `sched/sched_assign.cpp` never calls
    /// `wu_is_infeasible_fast`), so a rule leaves them alone unless it says so.
    pub assigned: bool,
    /// The versions the selected workunits may be sent with.
    pub allow: Vec<Allow>,
}

/// An app version as a rule sees it.
#[derive(Debug, Clone, Copy)]
pub struct VersionRef<'a> {
    pub platform: &'a str,
    pub plan_class: &'a str,
    pub version_num: i32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingRules {
    pub rules: Vec<RoutingRule>,
}

/// No rules, for a context that borrows some.
pub static NONE: RoutingRules = RoutingRules { rules: Vec::new() };

/// A pattern with `*` and `?`, matched against the whole name.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ni));
            pi += 1;
        } else if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

fn word(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 254
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

fn pattern_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_PATTERN
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-*?".contains(c))
}

fn invalid(i: usize, what: &str) -> CoreError {
    CoreError::Invalid(format!("routing_rules: rule {}: {what}", i + 1))
}

impl Allow {
    fn permits(&self, v: &VersionRef<'_>) -> bool {
        self.platform.as_deref().is_none_or(|p| p == v.platform)
            && self.plan_class.as_deref().is_none_or(|p| p == v.plan_class)
            && v.version_num >= self.min_version
            && (self.max_version == 0 || v.version_num <= self.max_version)
    }
}

impl RoutingRule {
    fn selects(&self, app: &str, name: &str, batch: i64, assigned: bool) -> bool {
        self.app == app
            && (!assigned || self.assigned)
            && self.name.as_deref().is_none_or(|p| glob_match(p, name))
            && (self.min_batch == 0 || batch >= self.min_batch)
            && (self.max_batch == 0 || batch <= self.max_batch)
    }

    fn allows(&self, v: Option<&VersionRef<'_>>) -> bool {
        v.is_some_and(|v| self.allow.iter().any(|a| a.permits(v)))
    }

    /// The condition on `w` (the workunit table) that this rule selects the row,
    /// in SQL that SQLite and D1 both run. The name pattern is checked to have
    /// no quote or bracket, so it is written into the text as it is.
    fn sql_selects(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(p) = &self.name {
            parts.push(format!("w.name GLOB '{p}'"));
        }
        if self.min_batch != 0 {
            parts.push(format!("w.batch >= {}", self.min_batch));
        }
        if self.max_batch != 0 {
            parts.push(format!("w.batch <= {}", self.max_batch));
        }
        if parts.is_empty() {
            "1 = 1".to_string()
        } else {
            format!("({})", parts.join(" AND "))
        }
    }
}

impl RoutingRules {
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Reads the stored document. Empty text is no rules. A document that is
    /// not valid is refused here, so a bad one is rejected when written.
    pub fn parse(text: &str) -> CoreResult<RoutingRules> {
        if text.trim().is_empty() {
            return Ok(RoutingRules::default());
        }
        let rules: RoutingRules = serde_json::from_str(text)
            .map_err(|e| CoreError::Invalid(format!("routing_rules: {e}")))?;
        rules.validate()?;
        Ok(rules)
    }

    fn validate(&self) -> CoreResult<()> {
        if self.rules.len() > MAX_RULES {
            return Err(CoreError::Invalid(format!(
                "routing_rules: at most {MAX_RULES} rules"
            )));
        }
        for (i, r) in self.rules.iter().enumerate() {
            if !word(&r.app) {
                return Err(invalid(i, "app must be an app name"));
            }
            if r.name.as_deref().is_some_and(|p| !pattern_ok(p)) {
                return Err(invalid(
                    i,
                    "name may hold only letters, digits, . _ - and the wildcards * and ?, up to 50 bytes",
                ));
            }
            if r.min_batch < 0 || r.max_batch < 0 || (r.max_batch != 0 && r.min_batch > r.max_batch)
            {
                return Err(invalid(
                    i,
                    "min_batch and max_batch must be 0 or more, in order",
                ));
            }
            if r.allow.len() > MAX_ALLOW {
                return Err(invalid(i, "too many allow entries"));
            }
            for a in &r.allow {
                if a.platform.as_deref().is_some_and(|p| !word(p)) {
                    return Err(invalid(i, "platform must be a platform name"));
                }
                if a.plan_class
                    .as_deref()
                    .is_some_and(|p| !p.is_empty() && !word(p))
                {
                    return Err(invalid(i, "plan_class must be a plan class name or empty"));
                }
                if a.min_version < 0
                    || a.max_version < 0
                    || (a.max_version != 0 && a.min_version > a.max_version)
                {
                    return Err(invalid(
                        i,
                        "min_version and max_version must be 0 or more, in order",
                    ));
                }
            }
        }
        Ok(())
    }

    /// The first rule that selects the workunit, if any.
    fn rule_for(&self, app: &str, name: &str, batch: i64, assigned: bool) -> Option<&RoutingRule> {
        self.rules
            .iter()
            .find(|r| r.selects(app, name, batch, assigned))
    }

    /// Whether the workunit may be sent with the version; `None` is a version of
    /// the client's own (the anonymous platform), which no rule allows because
    /// the project has no record of what it holds. True when no rule selects the
    /// workunit.
    pub fn permits(
        &self,
        app: &str,
        name: &str,
        batch: i64,
        assigned: bool,
        version: Option<&VersionRef<'_>>,
    ) -> bool {
        if self.rules.is_empty() {
            return true;
        }
        self.rule_for(app, name, batch, assigned)
            .is_none_or(|r| r.allows(version))
    }

    /// The same decision as a condition on workunit rows `w`, to leave out of a
    /// query the rows that [`permits`](Self::permits) would refuse; empty when no
    /// rule of the app can refuse this version.
    ///
    /// One `CASE` that lists the app's rules in order, each `WHEN` selector once,
    /// so the text is linear in the number of rules, and stops after the last rule
    /// that refuses the version (the rules after it cannot change the outcome).
    /// The first selector that holds decides, as in `permits`. With the limits on
    /// rules and patterns one version adds at most a few kilobytes.
    pub fn sql_condition(
        &self,
        app: &str,
        assigned: bool,
        version: Option<&VersionRef<'_>>,
    ) -> String {
        let mine: Vec<&RoutingRule> = self
            .rules
            .iter()
            .filter(|r| r.app == app && (!assigned || r.assigned))
            .collect();
        let Some(last) = mine.iter().rposition(|r| !r.allows(version)) else {
            return String::new();
        };
        let mut sql = String::from("CASE");
        for r in &mine[..=last] {
            sql.push_str(&format!(
                " WHEN {} THEN {}",
                r.sql_selects(),
                u8::from(!r.allows(version))
            ));
        }
        sql.push_str(" ELSE 0 END = 0");
        sql
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"{"rules":[
        {"app":"calib","name":"calib_a_*","assigned":true,
         "allow":[{"platform":"x86_64-pc-linux-gnu","min_version":3,"max_version":3},
                  {"platform":"arm64-apple-darwin","plan_class":"","min_version":5}]},
        {"app":"calib","min_batch":10,"max_batch":20,"allow":[{"plan_class":"cuda"}]},
        {"app":"calib","name":"calib_*","assigned":true,"allow":[]}
    ]}"#;

    fn v<'a>(platform: &'a str, plan_class: &'a str, n: i32) -> VersionRef<'a> {
        VersionRef {
            platform,
            plan_class,
            version_num: n,
        }
    }

    #[test]
    fn globs() {
        assert!(glob_match("a*", "abc"));
        assert!(glob_match("a*c", "abc"));
        assert!(glob_match("a?c", "abc"));
        assert!(glob_match("*", ""));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(!glob_match("a*c", "abd"));
        assert!(!glob_match("abc", "abcd"));
        assert!(glob_match("*abc", "xxabc"));
    }

    #[test]
    fn the_first_selecting_rule_decides() {
        let r = RoutingRules::parse(DOC).unwrap();
        let lin = v("x86_64-pc-linux-gnu", "", 3);
        let mac = v("arm64-apple-darwin", "", 7);
        // Rule 1: only version 3 on Linux, 5 and up on macOS with no plan class.
        assert!(r.permits("calib", "calib_a_1", 0, false, Some(&lin)));
        assert!(!r.permits(
            "calib",
            "calib_a_1",
            0,
            false,
            Some(&v("x86_64-pc-linux-gnu", "", 4))
        ));
        assert!(r.permits("calib", "calib_a_1", 0, false, Some(&mac)));
        assert!(!r.permits(
            "calib",
            "calib_a_1",
            0,
            false,
            Some(&v("arm64-apple-darwin", "cuda", 7))
        ));
        assert!(!r.permits(
            "calib",
            "calib_a_1",
            0,
            false,
            Some(&v("arm64-apple-darwin", "", 4))
        ));
        // Rule 2 selects by batch; rule 3 refuses the other calib_ names, but not
        // rows rule 2 admitted first.
        assert!(r.permits("calib", "other", 15, false, Some(&v("p", "cuda", 1))));
        assert!(!r.permits("calib", "other", 15, false, Some(&v("p", "", 1))));
        assert!(!r.permits("calib", "calib_b", 5, false, Some(&lin)));
        // Another app, and a name no rule selects, are not restricted.
        assert!(r.permits("other", "calib_a_1", 0, false, Some(&lin)));
        assert!(r.permits("calib", "other", 5, false, Some(&lin)));
    }

    #[test]
    fn the_anonymous_platform_is_refused_where_a_rule_selects() {
        let r = RoutingRules::parse(DOC).unwrap();
        assert!(!r.permits("calib", "calib_a_1", 0, false, None));
        assert!(r.permits("calib", "other", 5, false, None));
        assert!(NONE.permits("calib", "calib_a_1", 0, false, None));
    }

    #[test]
    fn a_rule_without_assigned_leaves_assigned_jobs_alone() {
        let r = RoutingRules::parse(DOC).unwrap();
        // Rule 2 has no `assigned`, so an assigned job in its batch range is not
        // selected by it, and rule 3 (assigned) takes it when the name fits.
        assert!(r.permits("calib", "other", 15, true, Some(&v("p", "", 1))));
        assert!(!r.permits("calib", "calib_b", 15, true, Some(&v("p", "cuda", 1))));
    }

    #[test]
    fn documents_are_checked() {
        assert!(RoutingRules::parse("").unwrap().is_empty());
        assert!(RoutingRules::parse(r#"{"rules":[]}"#).unwrap().is_empty());
        for bad in [
            "{\"rules\":{}}",
            r#"{"rules":[{"app":""}]}"#,
            r#"{"rules":[{"app":"a b"}]}"#,
            r#"{"rules":[{"app":"a","name":"x'y"}]}"#,
            r#"{"rules":[{"app":"a","name":"x[0]"}]}"#,
            r#"{"rules":[{"app":"a","min_batch":5,"max_batch":2}]}"#,
            r#"{"rules":[{"app":"a","min_batch":-1}]}"#,
            r#"{"rules":[{"app":"a","allow":[{"min_version":4,"max_version":2}]}]}"#,
            r#"{"rules":[{"app":"a","allow":[{"platform":"a b"}]}]}"#,
            r#"{"rules":[{"app":"a","unknown":1}]}"#,
            r#"{"rules":[{"app":"a","allow":[{"version":1}]}]}"#,
        ] {
            assert!(RoutingRules::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_query_condition_leaves_out_what_a_version_would_be_refused() {
        let r = RoutingRules::parse(DOC).unwrap();
        let lin = v("x86_64-pc-linux-gnu", "", 3);
        let c = r.sql_condition("calib", false, Some(&lin));
        // Rule 1 allows the version, rules 2 and 3 do not; the selectors come in
        // rule order, once each.
        assert_eq!(
            c,
            "CASE WHEN (w.name GLOB 'calib_a_*') THEN 0 \
             WHEN (w.batch >= 10 AND w.batch <= 20) THEN 1 \
             WHEN (w.name GLOB 'calib_*') THEN 1 ELSE 0 END = 0"
        );
        // For assigned jobs only the rules that apply to them count.
        let c = r.sql_condition("calib", true, Some(&lin));
        assert!(!c.contains("w.batch"), "{c}");
        // An app with no rule has no condition, and a rule that allows the
        // version adds none.
        assert_eq!(r.sql_condition("other", false, Some(&lin)), "");
        let only =
            RoutingRules::parse(r#"{"rules":[{"app":"a","allow":[{"platform":"p"}]}]}"#).unwrap();
        assert_eq!(only.sql_condition("a", false, Some(&v("p", "", 1))), "");
        let refused = "CASE WHEN 1 = 1 THEN 1 ELSE 0 END = 0";
        assert_eq!(
            only.sql_condition("a", false, Some(&v("q", "", 1))),
            refused
        );
        assert_eq!(only.sql_condition("a", false, None), refused);
    }

    #[test]
    fn patterns_are_limited_to_what_d1_accepts() {
        let ok = format!(r#"{{"rules":[{{"app":"a","name":"{}"}}]}}"#, "x".repeat(50));
        assert!(RoutingRules::parse(&ok).is_ok());
        let long = format!(r#"{{"rules":[{{"app":"a","name":"{}"}}]}}"#, "x".repeat(51));
        assert!(RoutingRules::parse(&long).is_err());
    }

    #[test]
    fn the_worst_accepted_rule_set_stays_small() {
        // The most rules, each with the longest pattern and both batch bounds at
        // their widest, none of which allows the version.
        let rule = format!(
            r#"{{"app":"a","name":"{}","min_batch":9223372036854775806,"max_batch":9223372036854775807,"allow":[]}}"#,
            "x".repeat(50)
        );
        let rules = vec![rule; MAX_RULES].join(",");
        let r = RoutingRules::parse(&format!(r#"{{"rules":[{rules}]}}"#)).unwrap();
        let c = r.sql_condition("a", false, Some(&v("p", "", 1)));
        assert!(c.len() < 12_000, "{} bytes", c.len());
        // And one more rule is refused.
        let rules = vec![r#"{"app":"a"}"#; MAX_RULES + 1].join(",");
        assert!(RoutingRules::parse(&format!(r#"{{"rules":[{rules}]}}"#)).is_err());
    }

    #[test]
    fn a_zero_bound_is_no_bound_even_for_a_negative_batch() {
        let r = RoutingRules::parse(r#"{"rules":[{"app":"a","allow":[]}]}"#).unwrap();
        assert!(!r.permits("a", "n", -3, false, Some(&v("p", "", 1))));
        let r = RoutingRules::parse(r#"{"rules":[{"app":"a","min_batch":2,"allow":[]}]}"#).unwrap();
        assert!(r.permits("a", "n", -3, false, Some(&v("p", "", 1))));
        assert!(!r.permits("a", "n", 2, false, Some(&v("p", "", 1))));
    }
}
