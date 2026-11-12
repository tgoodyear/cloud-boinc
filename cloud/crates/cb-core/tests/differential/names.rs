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
    actual_to_canon: BTreeMap<String, String>,
    canon_to_actual: BTreeMap<String, String>,
    workunit: Regex,
    token: Regex,
    random: Regex,
}

impl Names {
    pub fn new() -> Names {
        Names {
            actual_to_canon: BTreeMap::new(),
            canon_to_actual: BTreeMap::new(),
            workunit: Regex::new(r"(?s)<workunit>.*?<name>diff_(\d{4})</name>").unwrap(),
            token: Regex::new(r"diff_(\d{4})").unwrap(),
            random: Regex::new(r"_r\d+_").unwrap(),
        }
    }

    /// Takes the jobs of a scheduler reply the host has not had yet.
    pub fn learn(&mut self, reply: &str) {
        let mut seen: Vec<String> = Vec::new();
        for c in self.workunit.captures_iter(reply) {
            let n = c[1].to_string();
            if !self.actual_to_canon.contains_key(&n) && !seen.contains(&n) {
                seen.push(n);
            }
        }
        seen.sort();
        for n in seen {
            let c = format!("{:04}", self.actual_to_canon.len() + 1);
            self.actual_to_canon.insert(n.clone(), c.clone());
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

    /// The reply with job names in the canonical order, and the random part
    /// of output file names (`diff_0001_0_r123_0`) removed.
    pub fn to_canon(&self, text: &str) -> String {
        let t = self.token.replace_all(text, |c: &Captures| {
            format!(
                "diff_{}",
                self.actual_to_canon.get(&c[1]).unwrap_or(&c[1].to_string())
            )
        });
        self.random.replace_all(&t, "_r0_").into_owned()
    }
}
