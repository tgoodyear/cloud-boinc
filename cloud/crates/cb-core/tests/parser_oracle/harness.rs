//! Running the oracle and comparing its dump with what the server meant.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// What a finished oracle run left.
struct Output {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// A built oracle.
pub struct Oracle {
    bin: PathBuf,
}

/// Where the replies of a test are written, and the oracle to run on them,
/// if it is built.
///
/// Every `Harness` has a directory of its own, so tests running in parallel
/// never overwrite each other's replies, whatever the file names.
pub struct Harness {
    dir: PathBuf,
    keep: bool,
    pub oracle: Option<Oracle>,
}

/// Distinguishes the `Harness` instances of one test process.
static INSTANCES: AtomicUsize = AtomicUsize::new(0);

impl Harness {
    /// `group` names the kind of reply; it only labels the directory.
    pub fn new(group: &str) -> Harness {
        // With CB_PARSER_ORACLE_REPLIES set the replies stay for inspection;
        // otherwise they are removed once the test passes.
        let named = std::env::var_os("CB_PARSER_ORACLE_REPLIES");
        let base = match &named {
            Some(d) => PathBuf::from(d),
            None => Path::new(env!("CARGO_TARGET_TMPDIR")).join("parser-oracle-replies"),
        };
        let base = std::path::absolute(base).unwrap();
        let n = INSTANCES.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(group).join(format!("{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Harness {
            dir,
            keep: named.is_some(),
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

impl Drop for Harness {
    fn drop(&mut self) {
        // A failed test keeps its replies.
        if self.keep || std::thread::panicking() {
            return;
        }
        let _ = std::fs::remove_dir_all(&self.dir);
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

/// Reads a pipe to its end on another thread.
fn drain(mut pipe: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    rx
}

/// What `drain` read, once the oracle has exited. A pipe that stays open
/// means something the oracle started is still running.
fn finished(rx: &mpsc::Receiver<Vec<u8>>, kind: &str) -> Vec<u8> {
    rx.recv_timeout(Duration::from_secs(10))
        .unwrap_or_else(|_| panic!("oracle {kind} exited but left a process holding its output"))
}

/// How long one oracle run may take before the test fails. The oracle's own
/// watchdog (60 seconds unless `CB_PARSER_ORACLE_TIMEOUT_SECS` says
/// otherwise) fires first, with its own message; this one covers an oracle
/// that cannot even do that.
const RUN_LIMIT: Duration = Duration::from_secs(120);

impl Oracle {
    fn run(&self, kind: &str, file: &Path, args: &[&str]) -> Dump {
        let mut child = Command::new(&self.bin)
            .args(["--kind", kind, "--file"])
            .arg(file)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("cannot run {:?}: {e}", self.bin));
        // Both pipes are read while the oracle runs, so that output larger
        // than a pipe buffer cannot stop it.
        let stdout = drain(child.stdout.take().unwrap());
        let stderr = drain(child.stderr.take().unwrap());
        let deadline = Instant::now() + RUN_LIMIT;
        let status = loop {
            match child.try_wait().unwrap() {
                Some(status) => break status,
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    // The readers are left alone: a grandchild may still
                    // hold the pipes, and it ends on its own watchdog.
                    panic!(
                        "oracle {kind} on {file:?} did not finish within {RUN_LIMIT:?}; \
                         killed it"
                    );
                }
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        };
        let out = Output {
            status,
            stdout: finished(&stdout, kind),
            stderr: finished(&stderr, kind),
        };
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
