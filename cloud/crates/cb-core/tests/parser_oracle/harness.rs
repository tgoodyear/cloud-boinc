//! Running the oracle and comparing its dump with what the server meant.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A built oracle.
pub struct Oracle {
    bin: PathBuf,
}

/// Where the replies of a test are written, and the oracle to run on them,
/// if it is built.
pub struct Harness {
    dir: PathBuf,
    pub oracle: Option<Oracle>,
}

impl Harness {
    pub fn new(group: &str) -> Harness {
        let dir = match std::env::var_os("CB_PARSER_ORACLE_REPLIES") {
            Some(d) => PathBuf::from(d),
            None => Path::new(env!("CARGO_TARGET_TMPDIR")).join("parser-oracle-replies"),
        }
        .join(group);
        std::fs::create_dir_all(&dir).unwrap();
        Harness {
            dir,
            oracle: locate(),
        }
    }

    /// Writes a reply and returns its path.
    pub fn save(&self, name: &str, body: &str) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    /// True when the oracle is available; says why not otherwise.
    pub fn ready(&self) -> bool {
        if self.oracle.is_none() {
            eprintln!(
                "parser oracle not built, skipping the comparison: run \
                 `make -C cloud/tests/parser-oracle` or set CB_PARSER_ORACLE"
            );
        }
        self.oracle.is_some()
    }

    /// Runs the oracle on a saved reply.
    pub fn run(&self, kind: &str, file: &Path, args: &[&str]) -> Dump {
        self.oracle.as_ref().unwrap().run(kind, file, args)
    }
}

fn locate() -> Option<Oracle> {
    if let Some(p) = std::env::var_os("CB_PARSER_ORACLE") {
        let bin = PathBuf::from(p);
        assert!(bin.exists(), "CB_PARSER_ORACLE={bin:?} does not exist");
        return Some(Oracle { bin });
    }
    let bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/parser-oracle/build/oracle");
    if bin.exists() {
        return Some(Oracle { bin });
    }
    assert!(
        std::env::var_os("CB_PARSER_ORACLE_REQUIRED").is_none(),
        "the parser oracle is not built ({bin:?}) and CB_PARSER_ORACLE_REQUIRED is set"
    );
    None
}

impl Oracle {
    fn run(&self, kind: &str, file: &Path, args: &[&str]) -> Dump {
        let out = Command::new(&self.bin)
            .args(["--kind", kind, "--file"])
            .arg(file)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("cannot run {:?}: {e}", self.bin));
        assert!(
            out.status.success(),
            "oracle {kind} on {file:?} failed: {:?}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        let mut map = BTreeMap::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let (k, v) = line
                .split_once('=')
                .unwrap_or_else(|| panic!("bad oracle line {line:?}"));
            map.insert(k.to_string(), unescape(v));
        }
        Dump { map }
    }
}

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// What the oracle printed: one value per key.
pub struct Dump {
    map: BTreeMap<String, String>,
}

impl Dump {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.map.get(key).map(String::as_str)
    }

    /// The index `i` for which `{list}[i].{field}` is `value`.
    pub fn index_of(&self, list: &str, field: &str, value: &str) -> Option<usize> {
        let n: usize = self.get(&format!("{list}.count"))?.parse().ok()?;
        (0..n).find(|i| self.get(&format!("{list}[{i}].{field}")) == Some(value))
    }

    pub fn count(&self, list: &str) -> usize {
        self.get(&format!("{list}.count"))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    /// Messages the client logged, as `(priority, text)`.
    pub fn client_messages(&self) -> Vec<(String, String)> {
        (0..self.count("client_msg"))
            .map(|i| {
                (
                    self.get(&format!("client_msg[{i}].priority"))
                        .unwrap_or_default()
                        .to_string(),
                    self.get(&format!("client_msg[{i}].text"))
                        .unwrap_or_default()
                        .to_string(),
                )
            })
            .collect()
    }
}

/// Collects mismatches for one reply and reports them together.
pub struct Expect<'a> {
    case: String,
    dump: &'a Dump,
    failures: Vec<String>,
}

impl<'a> Expect<'a> {
    pub fn new(case: &str, dump: &'a Dump) -> Expect<'a> {
        Expect {
            case: case.to_string(),
            dump,
            failures: Vec::new(),
        }
    }

    pub fn dump(&self) -> &'a Dump {
        self.dump
    }

    pub fn fail(&mut self, msg: String) {
        self.failures.push(msg);
    }

    fn got(&mut self, key: &str) -> Option<&'a str> {
        let v = self.dump.get(key);
        if v.is_none() {
            self.failures
                .push(format!("{key}: the oracle printed nothing"));
        }
        v
    }

    /// A string the client read.
    pub fn s(&mut self, key: &str, want: impl AsRef<str>) {
        let want = want.as_ref();
        if let Some(got) = self.got(key) {
            if got != want {
                self.failures
                    .push(format!("{key}: client read {got:?}, server meant {want:?}"));
            }
        }
    }

    /// A number the client read. The protocol carries `%f` text, so values
    /// agree to six decimals.
    pub fn n(&mut self, key: &str, want: f64) {
        if let Some(got) = self.got(key) {
            match got.parse::<f64>() {
                Ok(g) if (g - want).abs() <= 1e-6 + want.abs() * 1e-12 => {}
                _ => self
                    .failures
                    .push(format!("{key}: client read {got}, server meant {want}")),
            }
        }
    }

    /// A string the client read, ignoring whitespace around it.
    pub fn s_trim(&mut self, key: &str, want: impl AsRef<str>) {
        let want = want.as_ref().trim();
        if let Some(got) = self.got(key) {
            if got.trim() != want {
                self.failures
                    .push(format!("{key}: client read {got:?}, server meant {want:?}"));
            }
        }
    }

    pub fn b(&mut self, key: &str, want: bool) {
        self.s(key, if want { "1" } else { "0" });
    }

    /// Both a name and a value check for a return code.
    pub fn rc(&mut self, key: &str, want: i32) {
        self.n(key, f64::from(want));
    }

    /// The client logged nothing it counts as an error and met no element it
    /// does not know, except what `allowed` lists. Each entry of `allowed`
    /// is a substring of a message the stock server also sends and the stock
    /// client also ignores or reports; the caller says why.
    pub fn quiet(&mut self, allowed: &[&str]) {
        for (prio, text) in self.dump.client_messages() {
            let bad = prio == "error" || text.contains("[unparsed_xml]");
            if bad && !allowed.iter().any(|a| text.contains(a)) {
                self.failures.push(format!("client logged {prio}: {text}"));
            }
        }
    }

    /// The client logged a message containing `text`.
    pub fn logged(&mut self, text: &str) {
        if !self
            .dump
            .client_messages()
            .iter()
            .any(|(_, t)| t.contains(text))
        {
            self.failures
                .push(format!("the client did not log {text:?}"));
        }
    }

    pub fn finish(self) {
        if !self.failures.is_empty() {
            panic!(
                "{}: the client's parsers disagree with the server:\n  {}",
                self.case,
                self.failures.join("\n  ")
            );
        }
    }
}
