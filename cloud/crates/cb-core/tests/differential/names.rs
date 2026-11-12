//! Names of the jobs a host was given; the Rust twin of
//! `cloud/tests/differential/names.py`.
//!
//! Both servers pick among the available jobs in an order of their own (the
//! stock scheduler from a random place in the feeder's shared memory), so the
//! corpus refers to jobs by the order in which the host was first sent them.
//! Replies are compared with the real names replaced by those, and requests
//! get the real names written back in.

use std::collections::BTreeMap;

use regex::{Captures, Regex};

pub struct Names {
    actual_canonicalize: BTreeMap<String, String>,
    canon_to_actual: BTreeMap<String, String>,
    workunit: Regex,
    token: Regex,
    file: Regex,
    /// Each complete output file name seen, and the placeholder it got.
    files: BTreeMap<String, String>,
    /// How many different names each job's output file has had.
    variants: BTreeMap<String, usize>,
}

impl Names {
    pub fn new() -> Names {
        Names {
            actual_canonicalize: BTreeMap::new(),
            canon_to_actual: BTreeMap::new(),
            workunit: Regex::new(r"(?s)<workunit>.*?<name>diff_(\d{4})</name>").unwrap(),
            token: Regex::new(r"diff_(\d{4})").unwrap(),
            file: Regex::new(r"diff_(\d{4})_(\d+)_r\d+_(\d+)").unwrap(),
            files: BTreeMap::new(),
            variants: BTreeMap::new(),
        }
    }

    /// Takes the jobs of a scheduler reply the host has not had yet.
    pub fn learn(&mut self, reply: &str) {
        let mut seen: Vec<String> = Vec::new();
        for c in self.workunit.captures_iter(reply) {
            let n = c[1].to_string();
            if !self.actual_canonicalize.contains_key(&n) && !seen.contains(&n) {
                seen.push(n);
            }
        }
        seen.sort();
        for n in seen {
            let c = format!("{:04}", self.actual_canonicalize.len() + 1);
            self.actual_canonicalize.insert(n.clone(), c.clone());
            self.canon_to_actual.insert(c, n);
        }
    }

    pub fn to_actual(&self, text: &str) -> String {
        self.token
            .replace_all(text, |c: &Captures| {
                format!(
                    "diff_{}",
                    self.canon_to_actual.get(&c[1]).unwrap_or(&c[1].to_string())
                )
            })
            .into_owned()
    }

    /// The reply with job names in the canonical order, and each complete
    /// output file name (`diff_0001_0_r123_0`) replaced by a placeholder:
    /// the job's canonical name with `r1` for the first different name that
    /// output file has had, `r2` for the next, and so on. The same name gets
    /// the same placeholder wherever it appears, in this reply and in later
    /// ones (a resend), and different names get different ones, so a
    /// reference that does not match the file it names stays visible.
    pub fn canonicalize(&mut self, text: &str) -> String {
        let t = self
            .file
            .replace_all(text, |c: &Captures| {
                let original = c[0].to_string();
                if let Some(p) = self.files.get(&original) {
                    return p.clone();
                }
                let job = self
                    .actual_canonicalize
                    .get(&c[1])
                    .cloned()
                    .unwrap_or_else(|| c[1].to_string());
                let base = format!("{job}_{}_{}", &c[2], &c[3]);
                let n = self.variants.entry(base.clone()).or_insert(0);
                *n += 1;
                // `~` keeps the next pass from mapping the job again.
                let placeholder = format!("diff~{job}_{}_r{n}_{}", &c[2], &c[3]);
                self.files.insert(original, placeholder.clone());
                placeholder
            })
            .into_owned();
        self.token
            .replace_all(&t, |c: &Captures| {
                format!(
                    "diff_{}",
                    self.actual_canonicalize
                        .get(&c[1])
                        .unwrap_or(&c[1].to_string())
                )
            })
            .replace("diff~", "diff_")
    }
}
