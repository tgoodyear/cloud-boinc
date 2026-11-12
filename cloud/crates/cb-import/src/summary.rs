//! What the tool prints. Counts, table names, column names and row IDs only.

use std::collections::BTreeMap;
use std::fmt::Write;

#[derive(Debug, Clone, Default)]
pub struct TableStat {
    pub name: String,
    pub read: u64,
    pub written: u64,
    pub secs: f64,
    /// Target columns the source has no column for (they take defaults or are computed).
    pub made_up: Vec<String>,
    /// Source columns the target has no column for.
    pub dropped: Vec<String>,
}

impl TableStat {
    pub fn rate(&self) -> f64 {
        if self.secs > 0.0 {
            self.written as f64 / self.secs
        } else {
            0.0
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub tables: Vec<TableStat>,
    pub counters: BTreeMap<String, u64>,
    pub notes: Vec<String>,
    pub warnings: Vec<String>,
    pub total_secs: f64,
}

impl Summary {
    pub fn count(&mut self, key: &str, n: u64) {
        *self.counters.entry(key.to_string()).or_insert(0) += n;
    }
    pub fn note(&mut self, s: impl Into<String>) {
        self.notes.push(s.into());
    }
    pub fn warn(&mut self, s: impl Into<String>) {
        self.warnings.push(s.into());
    }

    pub fn render(&self) -> String {
        let mut o = String::new();
        let _ = writeln!(
            o,
            "{:<18} {:>10} {:>10} {:>9} {:>11}",
            "table", "read", "written", "seconds", "rows/second"
        );
        for t in &self.tables {
            let _ = writeln!(
                o,
                "{:<18} {:>10} {:>10} {:>9.2} {:>11.0}",
                t.name,
                t.read,
                t.written,
                t.secs,
                t.rate()
            );
        }
        let _ = writeln!(o, "total seconds: {:.2}", self.total_secs);
        for t in &self.tables {
            if !t.dropped.is_empty() {
                let _ = writeln!(
                    o,
                    "{}: columns not carried (no target column): {}",
                    t.name,
                    t.dropped.join(", ")
                );
            }
            if !t.made_up.is_empty() {
                let _ = writeln!(
                    o,
                    "{}: target columns without a source column (default or computed): {}",
                    t.name,
                    t.made_up.join(", ")
                );
            }
        }
        if !self.counters.is_empty() {
            let _ = writeln!(o, "counts:");
            for (k, v) in &self.counters {
                let _ = writeln!(o, "  {k}: {v}");
            }
        }
        for n in &self.notes {
            let _ = writeln!(o, "note: {n}");
        }
        for w in &self.warnings {
            let _ = writeln!(o, "WARNING: {w}");
        }
        o
    }
}
