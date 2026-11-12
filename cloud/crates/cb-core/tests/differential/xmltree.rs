//! A forgiving XML reader for BOINC replies, and the flattening the
//! comparison works on.
//!
//! BOINC writes its XML by hand: a reply may carry text next to elements,
//! `<message priority="low">` attributes, an `<?xml?>` header with a
//! character set the text does not follow, and, in the scheduler reply,
//! whole documents (preferences) pasted in. A strict parser would refuse
//! some of what the stock server sends, and the clients do not. This reader
//! accepts any nesting that balances and keeps what a client's `parse_*`
//! calls would see: element names, attributes and trimmed text.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Default)]
pub struct Node {
    pub name: String,
    pub text: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
}

pub fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// Parses `s`. Several top-level elements are returned under a node named
/// `#document`.
pub fn parse(s: &str) -> Result<Node, String> {
    let mut stack: Vec<Node> = vec![Node {
        name: "#document".into(),
        ..Default::default()
    }];
    let b = s.as_bytes();
    let mut i = 0;
    let mut text_start = 0;
    let flush = |stack: &mut Vec<Node>, from: usize, to: usize| {
        let t = unescape(s[from..to].trim());
        if !t.is_empty() {
            let top = stack.last_mut().unwrap();
            if !top.text.is_empty() {
                top.text.push(' ');
            }
            top.text.push_str(&t);
        }
    };
    while i < b.len() {
        // A `<` that cannot begin a tag is text: the stock upload handler
        // writes "0 bytes < offset 10 bytes" in a message without escaping it.
        let tag_follows = b
            .get(i + 1)
            .is_some_and(|c| c.is_ascii_alphabetic() || matches!(c, b'/' | b'!' | b'?'));
        if b[i] != b'<' || !tag_follows {
            i += 1;
            continue;
        }
        flush(&mut stack, text_start, i);
        if s[i..].starts_with("<?") {
            let end = s[i..].find("?>").ok_or("unterminated <?")?;
            i += end + 2;
        } else if s[i..].starts_with("<!--") {
            let end = s[i..].find("-->").ok_or("unterminated comment")?;
            i += end + 3;
        } else if s[i..].starts_with("<![CDATA[") {
            let end = s[i..].find("]]>").ok_or("unterminated CDATA")?;
            let t = &s[i + 9..i + end];
            let top = stack.last_mut().unwrap();
            top.text.push_str(t.trim());
            i += end + 3;
        } else if s[i..].starts_with("<!") {
            let end = s[i..].find('>').ok_or("unterminated <!")?;
            i += end + 1;
        } else if s[i..].starts_with("</") {
            let end = s[i..].find('>').ok_or("unterminated end tag")?;
            let name = s[i + 2..i + end].trim();
            let node = stack.pop().unwrap();
            if node.name != name || stack.is_empty() {
                return Err(format!("</{name}> closes <{}>", node.name));
            }
            stack.last_mut().unwrap().children.push(node);
            i += end + 1;
        } else {
            let end = s[i..].find('>').ok_or("unterminated tag")?;
            let mut inner = &s[i + 1..i + end];
            let empty = inner.ends_with('/');
            if empty {
                inner = &inner[..inner.len() - 1];
            }
            let (name, rest) = match inner.find(char::is_whitespace) {
                Some(p) => (&inner[..p], inner[p..].trim()),
                None => (inner, ""),
            };
            let mut node = Node {
                name: name.to_string(),
                ..Default::default()
            };
            let mut rest = rest;
            while let Some(eq) = rest.find('=') {
                let key = rest[..eq].trim().to_string();
                let after = rest[eq + 1..].trim_start();
                let Some(q) = after.chars().next().filter(|c| *c == '"' || *c == '\'') else {
                    break;
                };
                let Some(close) = after[1..].find(q) else {
                    break;
                };
                node.attrs.push((key, unescape(&after[1..1 + close])));
                rest = after[close + 2..].trim_start();
            }
            if empty {
                stack.last_mut().unwrap().children.push(node);
            } else {
                stack.push(node);
            }
            i += end + 1;
        }
        text_start = i;
    }
    flush(&mut stack, text_start, b.len());
    if stack.len() != 1 {
        return Err(format!("<{}> is not closed", stack.last().unwrap().name));
    }
    let doc = stack.pop().unwrap();
    if doc.children.len() == 1 && doc.text.is_empty() {
        Ok(doc.children.into_iter().next().unwrap())
    } else {
        Ok(doc)
    }
}

/// Elements that are lists of records: for each tag, the child elements that
/// name a record. The flattened path of such an element carries the key
/// instead of a position, so two replies that list the same records in
/// another order still line up.
pub const KEYED: &[(&str, &[&str])] = &[
    ("file_info", &["name"]),
    ("workunit", &["name"]),
    ("result", &["name"]),
    ("app", &["name"]),
    ("app_version", &["app_name", "version_num", "plan_class"]),
    ("host", &["id"]),
    ("platform", &["platform_name"]),
];

/// One leaf of a document: where it is and what it says.
#[derive(Debug, Clone, PartialEq)]
pub struct Leaf {
    /// `a/b[key]/c[1]/d`; see [`shape`] for the form rules match on.
    pub path: String,
    pub value: String,
}

fn key_of(n: &Node, keys: &[&str]) -> String {
    keys.iter()
        .map(|k| {
            n.children
                .iter()
                .find(|c| c.name == *k)
                .map(|c| c.text.clone())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Turns a document into leaves: one per element with text and one per
/// attribute (`element@attr`). Repeated children are numbered in their
/// order, except the keyed records above. An element with neither text nor
/// children is a flag and gets the leaf value `<present>`.
pub fn flatten(root: &Node) -> Vec<Leaf> {
    let mut out = Vec::new();
    walk(root, root.name.clone(), &mut out);
    out
}

fn walk(n: &Node, path: String, out: &mut Vec<Leaf>) {
    for (k, v) in &n.attrs {
        out.push(Leaf {
            path: format!("{path}@{k}"),
            value: v.clone(),
        });
    }
    if n.children.is_empty() {
        out.push(Leaf {
            path,
            value: if n.text.is_empty() {
                "<present>".into()
            } else {
                n.text.clone()
            },
        });
        return;
    }
    if !n.text.is_empty() {
        out.push(Leaf {
            path: format!("{path}#text"),
            value: n.text.clone(),
        });
    }
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for c in &n.children {
        *counts.entry(c.name.as_str()).or_default() += 1;
    }
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for c in &n.children {
        let idx = seen.entry(c.name.as_str()).or_default();
        let step = match KEYED.iter().find(|(t, _)| *t == c.name) {
            Some((_, keys)) => format!("{}[{}]", c.name, key_of(c, keys)),
            None if counts[c.name.as_str()] > 1 => format!("{}[{}]", c.name, idx),
            None => c.name.clone(),
        };
        *idx += 1;
        walk(c, format!("{path}/{step}"), out);
    }
}

/// A path with every `[...]` removed: what the rules of the comparison match.
pub fn shape(path: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for ch in path.chars() {
        match ch {
            '[' => depth += 1,
            ']' => depth -= 1,
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}
