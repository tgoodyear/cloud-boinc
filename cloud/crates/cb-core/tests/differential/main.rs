//! Differential test: this server against recorded replies of the stock
//! server. See `cloud/docs/compatibility.md` and `cloud/tests/differential/`.

mod compare;
mod names;
mod world;
mod xmltree;

use std::path::Path;

use compare::{compare, scheduler_links, Used};
use names::Names;
use serde_json::Value;
use world::{differential_dir, Answer, World};

/// The upload certificates in a scheduler reply, checked against the upload
/// public key: `(found, valid)`. Both servers sign `<name>` and
/// `<max_nbytes>` with the same private key, but the names of output files
/// carry a random number, so the signatures themselves are not equal.
fn certificates(reply: &str, key: &cb_crypto::PublicKey) -> (usize, usize) {
    fn walk(n: &xmltree::Node, key: &cb_crypto::PublicKey, out: &mut (usize, usize)) {
        if n.name == "file_info" {
            let child = |t: &str| {
                n.children
                    .iter()
                    .find(|c| c.name == t)
                    .map(|c| c.text.as_str())
            };
            if let (Some(name), Some(sig)) = (child("name"), child("xml_signature")) {
                let max = child("max_nbytes").and_then(|v| v.parse::<f64>().ok());
                out.0 += 1;
                // The signature block was trimmed when read; it ends in a newline.
                let ok = max.is_some_and(|m| {
                    cb_crypto::verify_upload_certificate(name, m, &format!("{sig}\n"), key)
                        || cb_crypto::verify_upload_certificate(name, m, sig, key)
                });
                if ok {
                    out.1 += 1;
                }
            }
        }
        for c in &n.children {
            walk(c, key, out);
        }
    }
    let mut out = (0, 0);
    if let Ok(root) = xmltree::parse(reply) {
        walk(&root, key, &mut out);
    }
    out
}

/// The files a scheduler reply names in `<file_ref>` that it has no
/// `<file_info>` for. The client links a result to its files by these names
/// (`CLIENT_STATE::handle_scheduler_reply`), so a reference to a name that is
/// not described is a reply it cannot use.
fn unresolved_file_refs(reply: &str) -> Vec<String> {
    fn walk(n: &xmltree::Node, refs: &mut Vec<String>, infos: &mut Vec<String>) {
        if n.name == "file_ref" {
            if let Some(f) = n.children.iter().find(|c| c.name == "file_name") {
                refs.push(f.text.clone());
            }
        }
        if n.name == "file_info" {
            if let Some(f) = n.children.iter().find(|c| c.name == "name") {
                infos.push(f.text.clone());
            }
        }
        for c in &n.children {
            walk(c, refs, infos);
        }
    }
    let Ok(root) = xmltree::parse(reply) else {
        return Vec::new();
    };
    let (mut refs, mut infos) = (Vec::new(), Vec::new());
    walk(&root, &mut refs, &mut infos);
    refs.retain(|r| !infos.contains(r));
    refs
}

/// One scheduler step: the references in each reply are checked on the real
/// names, then the replies are compared with job and file names replaced by
/// placeholders (`names.rs`). Returns what is wrong.
fn compare_sched(
    step: &str,
    stock: &str,
    ours: &str,
    stock_names: &mut Names,
    our_names: &mut Names,
    used: &mut Used,
) -> Vec<String> {
    let mut findings = Vec::new();
    for (who, text) in [("stock", stock), ("ours", ours)] {
        for r in unresolved_file_refs(text) {
            findings.push(format!(
                "{step}: the {who} reply has a file_ref to {r}, which no file_info describes"
            ));
        }
    }
    stock_names.learn(stock);
    our_names.learn(ours);
    let (stock, ours) = (
        stock_names.canonicalize(stock),
        our_names.canonicalize(ours),
    );
    match compare(step, &stock, &ours, used) {
        Ok(diffs) => findings.extend(diffs.into_iter().map(|d| format!("{step}: {d}"))),
        Err(e) => findings.push(format!("{step}: cannot compare: {e}")),
    }
    findings
}

fn answer(world: &World, step: &Value, dir: &Path, time: i64, names: &Names) -> Answer {
    let request =
        |step: &Value| std::fs::read(dir.join(step["request"].as_str().unwrap())).unwrap();
    match step["kind"].as_str().unwrap() {
        "sched" => {
            // The corpus names jobs by the order they were sent; use real names.
            let text = String::from_utf8(request(step)).unwrap();
            world.scheduler(names.to_actual(&text).as_bytes(), time)
        }
        "upload" => world.upload(&request(step)),
        "web" => world.web(
            step["method"].as_str().unwrap_or("GET"),
            step["url"].as_str().unwrap(),
            time,
        ),
        k => panic!("unknown step kind {k}"),
    }
}

#[test]
fn replies_match_the_stock_server() {
    let corpus = differential_dir().join("corpus");
    let mut scenarios: Vec<_> = std::fs::read_dir(&corpus)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.join("steps.json").exists())
        .collect();
    scenarios.sort();
    assert!(!scenarios.is_empty(), "no scenarios in {corpus:?}");
    let dump = std::env::var_os("CB_DIFFERENTIAL_DUMP").map(std::path::PathBuf::from);
    let mut used = Used::default();
    let mut findings = Vec::new();
    let (mut stock_certs, mut our_certs) = (0usize, 0usize);
    for dir in &scenarios {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let steps: Vec<Value> =
            serde_json::from_str(&std::fs::read_to_string(dir.join("steps.json")).unwrap())
                .unwrap();
        let golden: Vec<Value> =
            serde_json::from_str(&std::fs::read_to_string(dir.join("golden.json")).unwrap())
                .unwrap();
        let world = World::new();
        let (mut stock_names, mut our_names) = (Names::new(), Names::new());
        for (step, g) in steps.iter().zip(&golden) {
            let step_name = step["name"].as_str().unwrap();
            let time = g["time"].as_i64().unwrap();
            let ours = answer(&world, step, dir, time, &our_names);
            if i64::from(ours.status) != g["status"].as_i64().unwrap() {
                findings.push(format!(
                    "{name}/{step_name}: status {} here, {} from the stock server",
                    ours.status, g["status"]
                ));
            }
            let stock = std::fs::read(dir.join(format!("{step_name}.reply"))).unwrap();
            if let Some(d) = &dump {
                let out = d.join(&name);
                std::fs::create_dir_all(&out).unwrap();
                std::fs::write(out.join(format!("{step_name}.ours")), &ours.body).unwrap();
            }
            let stock = String::from_utf8_lossy(&stock).into_owned();
            let ours = String::from_utf8_lossy(&ours.body).into_owned();
            if step["kind"].as_str() == Some("sched") {
                for (who, text, total) in [
                    ("stock", &stock, &mut stock_certs),
                    ("ours", &ours, &mut our_certs),
                ] {
                    let (found, valid) = certificates(text, world.upload_key());
                    if found != valid {
                        findings.push(format!(
                            "{name}/{step_name}: {} of {found} upload certificates in the {who} \
                             reply do not verify against the upload key",
                            found - valid
                        ));
                    }
                    *total += found;
                }
                findings.extend(compare_sched(
                    &format!("{name}/{step_name}"),
                    &stock,
                    &ours,
                    &mut stock_names,
                    &mut our_names,
                    &mut used,
                ));
                continue;
            }
            if step["compare"].as_str() == Some("master") {
                let (a, b) = (scheduler_links(&stock), scheduler_links(&ours));
                assert!(
                    !a.is_empty() && !b.is_empty(),
                    "{name}/{step_name}: no scheduler link"
                );
                if a != b {
                    findings.push(format!(
                        "{name}/{step_name}: scheduler links differ: stock {a:?}, ours {b:?}"
                    ));
                }
                continue;
            }
            match compare(&format!("{name}/{step_name}"), &stock, &ours, &mut used) {
                Ok(diffs) => {
                    for d in diffs {
                        findings.push(format!("{name}/{step_name}: {d}"));
                    }
                }
                Err(e) => findings.push(format!("{name}/{step_name}: cannot compare: {e}")),
            }
        }
    }
    // Every rule and entry must have been needed.
    let unused: Vec<String> = compare::INTENDED
        .iter()
        .enumerate()
        .filter(|(i, _)| !used.intended_index.contains(i))
        .map(|(_, e)| {
            format!(
                "intended difference {} ({} {:?} {})",
                e.id, e.step, e.kind, e.shape
            )
        })
        .chain(
            compare::TIMES
                .iter()
                .filter(|t| !used.times.contains(*t))
                .map(|t| format!("time field {t}")),
        )
        .chain(
            compare::RANDOM
                .iter()
                .filter(|(s, _)| !used.random.contains(s))
                .map(|(s, _)| format!("random field {s}")),
        )
        .chain(
            compare::TRANSFORMS
                .iter()
                .filter(|(id, ..)| !used.transforms.contains(id))
                .map(|(id, ..)| format!("transform {id}")),
        )
        .collect();
    if findings.is_empty() {
        assert!(unused.is_empty(), "rules no step needed: {unused:?}");
    }
    assert!(
        stock_certs > 0 && our_certs > 0,
        "upload certificates checked: stock {stock_certs}, ours {our_certs}"
    );
    if !findings.is_empty() {
        if std::env::var_os("CB_DIFFERENTIAL_SUMMARY").is_some() {
            // One line per kind of difference, for working out what to list.
            let mut groups: std::collections::BTreeMap<String, (usize, String)> =
                Default::default();
            for f in &findings {
                let key: String = f
                    .lines()
                    .next()
                    .unwrap()
                    .split_once(": ")
                    .map(|(_, rest)| rest.to_string())
                    .unwrap_or_default();
                let shape = key
                    .split(" = ")
                    .next()
                    .map(xmltree::shape)
                    .unwrap_or_default();
                let e = groups.entry(shape).or_insert((0, f.clone()));
                e.0 += 1;
            }
            for (shape, (n, example)) in groups {
                println!(
                    "{n:4} {shape}\n       e.g. {}",
                    example.replace('\n', "\n       ")
                );
            }
        }
        panic!("{} findings:\n{}", findings.len(), findings.join("\n"));
    }
}

/// A work reply with one output file, named `info` in its `file_info` and
/// `file_ref` in the reference from its result.
#[cfg(test)]
fn work_reply(info: &str, file_ref: &str) -> String {
    format!(
        "<scheduler_reply>\n<file_info>\n<name>{info}</name>\n</file_info>\n\
         <workunit>\n<name>diff_0001</name>\n</workunit>\n\
         <result>\n<name>diff_0001_0</name>\n<file_ref>\n<file_name>{file_ref}</file_name>\n\
         </file_ref>\n</result>\n</scheduler_reply>\n"
    )
}

#[cfg(test)]
fn findings_for(stock: &str, ours: &str) -> Vec<String> {
    compare_sched(
        "t/01",
        stock,
        ours,
        &mut Names::new(),
        &mut Names::new(),
        &mut Used::default(),
    )
}

/// Random numbers in output file names do not matter when both replies are
/// consistent, whatever the numbers are.
#[test]
fn consistent_file_names_with_other_random_parts_compare_equal() {
    let stock = work_reply("diff_0001_0_r123_0", "diff_0001_0_r123_0");
    let ours = work_reply("diff_0001_0_r987_0", "diff_0001_0_r987_0");
    assert_eq!(findings_for(&stock, &ours), Vec::<String>::new());
}

/// A reply whose `file_ref` names another file than its `file_info` is one
/// the client cannot link, and must fail, however it looks after the random
/// part is dropped.
#[test]
fn a_file_ref_that_does_not_match_its_file_info_fails() {
    let good = work_reply("diff_0001_0_r123_0", "diff_0001_0_r123_0");
    let broken = work_reply("diff_0001_0_r123_0", "diff_0001_0_r456_0");
    let f = findings_for(&good, &broken);
    assert!(
        f.iter()
            .any(|m| m.contains("ours reply has a file_ref to diff_0001_0_r456_0")),
        "{f:?}"
    );
    assert!(
        f.iter().any(|m| m.contains("differs")),
        "the comparison must differ too: {f:?}"
    );
    // The same fault on the stock side.
    let f = findings_for(&broken, &good);
    assert!(
        f.iter().any(|m| m.contains("stock reply has a file_ref")),
        "{f:?}"
    );
    assert!(f.iter().any(|m| m.contains("differs")), "{f:?}");
}

/// The same name gets the same placeholder in a later reply (a resend), and
/// another name for the same output file gets another.
#[test]
fn placeholders_follow_the_complete_name_across_replies() {
    let mut n = Names::new();
    let first = work_reply("diff_0001_0_r123_0", "diff_0001_0_r123_0");
    n.learn(&first);
    let a = n.canonicalize(&first);
    let again = n.canonicalize(&first);
    assert_eq!(a, again);
    let other = n.canonicalize(&work_reply("diff_0001_0_r555_0", "diff_0001_0_r555_0"));
    assert_ne!(a, other);
    assert!(a.contains("diff_0001_0_r1_0") && other.contains("diff_0001_0_r2_0"));
}
