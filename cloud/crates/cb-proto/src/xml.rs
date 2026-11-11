//! Tolerant XML tokenizer, escaping and a small writer.
//!
//! BOINC XML is not always well-formed: elements are written with `printf`,
//! raw blobs are embedded without escaping, and both ends read it with a
//! tokenizer that only knows tags and text. [`XmlParser`] follows
//! `XML_PARSER` in `lib/parse.h` and `lib/parse.cpp`; the functions in
//! [`line`] follow the older string-search parsers in the same file, which
//! the upload handler and the preferences parser still use.
//!
//! Differences from upstream, all on the tolerant side:
//!
//! - Upstream compares text tokens against tag names, so the text `hostid`
//!   would be taken for the tag `<hostid>`. Here the `parse_*` functions and
//!   [`XmlParser::match_tag`] only match real tags.
//! - Upstream copies tokens into fixed buffers and fails when they overflow.
//!   There are no length limits here; callers that store values in bounded
//!   columns must truncate.
//! - Hexadecimal floating point literals are not accepted by
//!   [`XmlParser::parse_double`].

use crate::ProtoError;

/// C `isspace` for the "C" locale, restricted to ASCII.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Strip ASCII whitespace from both ends (`strip_whitespace` in
/// `lib/str_util.cpp`).
pub fn strip_whitespace(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_ascii() && is_space(c as u8))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    let first = needle[0];
    let last = haystack.len() - needle.len();
    let mut i = 0;
    while i <= last {
        i += haystack[i..=last].iter().position(|&c| c == first)?;
        if &haystack[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Result of C `strtol(s, &end, 0)`: value, bytes consumed, overflow flag.
///
/// Base 0 means a `0x` prefix selects hexadecimal and a leading `0` selects
/// octal, as in C. When no digits are found the consumed length is 0.
pub(crate) fn c_strtol(s: &str) -> (i64, usize, bool) {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && is_space(b[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        neg = b[i] == b'-';
        i += 1;
    }
    let mut base: i128 = 10;
    if i + 2 < b.len()
        && b[i] == b'0'
        && (b[i + 1] == b'x' || b[i + 1] == b'X')
        && b[i + 2].is_ascii_hexdigit()
    {
        base = 16;
        i += 2;
    } else if i < b.len() && b[i] == b'0' {
        base = 8;
    }
    let start = i;
    let mut val: i128 = 0;
    let mut overflow = false;
    while i < b.len() {
        let d = match b[i] {
            c @ b'0'..=b'9' => (c - b'0') as i128,
            c @ b'a'..=b'f' => (c - b'a' + 10) as i128,
            c @ b'A'..=b'F' => (c - b'A' + 10) as i128,
            _ => break,
        };
        if d >= base {
            break;
        }
        if !overflow {
            val = val * base + d;
            if val > i64::MAX as i128 + 1 {
                overflow = true;
            }
        }
        i += 1;
    }
    if i == start {
        return (0, 0, false);
    }
    let signed = if neg { -val } else { val };
    if overflow || signed > i64::MAX as i128 || signed < i64::MIN as i128 {
        return (if neg { i64::MIN } else { i64::MAX }, i, true);
    }
    (signed as i64, i, false)
}

/// Result of C `strtod(s, &end)`: value, bytes consumed, range-error flag.
///
/// Accepts decimal notation with an optional exponent, and `inf`,
/// `infinity` and `nan` in any case. When nothing parses the consumed
/// length is 0.
pub(crate) fn c_strtod(s: &str) -> (f64, usize, bool) {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && is_space(b[i]) {
        i += 1;
    }
    let num_start = i;
    let mut neg = false;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        neg = b[i] == b'-';
        i += 1;
    }
    let rest = &b[i..];
    let starts_with =
        |word: &[u8]| rest.len() >= word.len() && rest[..word.len()].eq_ignore_ascii_case(word);
    if starts_with(b"infinity") {
        let v = if neg {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
        return (v, i + 8, false);
    }
    if starts_with(b"inf") {
        let v = if neg {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
        return (v, i + 3, false);
    }
    if starts_with(b"nan") {
        return (f64::NAN, i + 3, false);
    }
    let mut digits = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    if i < b.len() && b[i] == b'.' {
        let mut j = i + 1;
        let mut frac = 0;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
            frac += 1;
        }
        if digits + frac > 0 {
            i = j;
            digits += frac;
        }
    }
    if digits == 0 {
        return (0.0, 0, false);
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        let exp_start = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > exp_start {
            i = j;
        }
    }
    let text = &s[num_start..i];
    let text = text.strip_prefix('+').unwrap_or(text);
    match text.parse::<f64>() {
        Ok(v) => (v, i, v.is_infinite()),
        Err(_) => (0.0, 0, false),
    }
}

/// C `atoi`: leading whitespace, optional sign, decimal digits; 0 on failure.
pub(crate) fn c_atoi(s: &str) -> i32 {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && is_space(b[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        neg = b[i] == b'-';
        i += 1;
    }
    let mut val: i64 = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        val = (val * 10 + (b[i] - b'0') as i64).min(i64::from(u32::MAX));
        i += 1;
    }
    (if neg { -val } else { val }) as i32
}

/// Escape text for inclusion in BOINC XML (`xml_escape` in `lib/parse.cpp`).
///
/// `<` and `&` are replaced by entities, `]]>` becomes `]]&gt;`, bytes above
/// 127 and tab, newline and carriage return become `&#N;` (one reference per
/// byte, so multi-byte UTF-8 sequences round-trip through
/// [`xml_unescape`]), and other control characters are dropped. `>` and
/// quotes are left alone.
pub fn xml_escape(input: &str) -> String {
    let b = input.as_bytes();
    let mut out = String::with_capacity(b.len() + 16);
    let mut i = 0;
    while i < b.len() {
        let x = b[i];
        match x {
            b'<' => out.push_str("&lt;"),
            b'&' => out.push_str("&amp;"),
            128..=255 => {
                out.push_str("&#");
                out.push_str(&x.to_string());
                out.push(';');
            }
            9 | 10 | 13 => {
                out.push_str("&#");
                out.push_str(&x.to_string());
                out.push(';');
            }
            0..=31 => {}
            b']' if b[i..].starts_with(b"]]>") => {
                out.push_str("]]&gt;");
                i += 2;
            }
            _ => out.push(x as char),
        }
        i += 1;
    }
    out
}

/// Escape text for XML that a standard parser reads, keeping non-ASCII
/// characters as they are.
///
/// Use this for replies the output of which is UTF-8 and declared so
/// (`XML_HEADER_UTF8`), such as `show_user.php?format=xml`. [`xml_escape`]
/// is for everything the BOINC client parses with its own byte-oriented
/// parser: it turns each byte of a multi-byte character into its own
/// reference, which a standard parser reads as several Latin-1 characters.
///
/// `<` and `&` become entities and tab, newline and carriage return become
/// `&#N;`. Other control characters and U+FFFE and U+FFFF, which XML 1.0
/// forbids, are dropped. A `>` that would follow `]]` in the output becomes
/// `&gt;`. Every other character is kept. For ASCII input without dropped
/// characters the output equals [`xml_escape`]'s.
pub fn xml_escape_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    for c in input.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '&' => out.push_str("&amp;"),
            '\t' | '\n' | '\r' => out.push_str(&format!("&#{};", c as u32)),
            '\0'..='\x1f' | '\u{fffe}' | '\u{ffff}' => {}
            // Checked against the output, so that a dropped character
            // between the brackets and the `>` does not leave `]]>`.
            '>' if out.ends_with("]]") => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Undo [`xml_escape`] (`xml_unescape` in `lib/parse.cpp`).
///
/// Handles `&lt;`, `&gt;`, `&quot;`, `&apos;`, `&amp;`, `&#xD;`, `&#xA;`
/// and decimal `&#N;` with `N < 256`, which yields one byte. Anything else
/// is copied through. The result is decoded as UTF-8, with invalid
/// sequences replaced.
pub fn xml_unescape(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let b = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'&' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        let rest = &b[i..];
        if rest.starts_with(b"&lt;") {
            out.push(b'<');
            i += 4;
        } else if rest.starts_with(b"&gt;") {
            out.push(b'>');
            i += 4;
        } else if rest.starts_with(b"&quot;") {
            out.push(b'"');
            i += 6;
        } else if rest.starts_with(b"&apos;") {
            out.push(b'\'');
            i += 6;
        } else if rest.starts_with(b"&amp;") {
            out.push(b'&');
            i += 5;
        } else if rest.starts_with(b"&#xD;") || rest.starts_with(b"&#xd;") {
            out.push(b'\r');
            i += 5;
        } else if rest.starts_with(b"&#xA;") || rest.starts_with(b"&#xa;") {
            out.push(b'\n');
            i += 5;
        } else if rest.starts_with(b"&#") {
            // Upstream emits "&#" and carries on when the reference is
            // malformed or out of range.
            i += 2;
            let digits = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
            let terminated = digits > 0 && b.get(i + digits) == Some(&b';');
            let value = b[i..i + digits]
                .iter()
                .fold(0u32, |acc, c| (acc * 10 + u32::from(c - b'0')).min(100_000));
            if terminated && value < 256 {
                out.push(value as u8);
                i += digits + 1;
            } else {
                out.extend_from_slice(b"&#");
            }
        } else {
            out.push(b'&');
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

enum Scan {
    Tag,
    Data,
    Cdata,
}

enum RawTag {
    Tag(String, String),
    Cdata(String),
    Comment,
}

/// Token-level parser equivalent to `XML_PARSER` in `lib/parse.h`.
///
/// The usual loop is the upstream one:
///
/// ```
/// use cb_proto::xml::XmlParser;
/// let mut xp = XmlParser::new("<a>\n <n>3</n>\n <junk><x/></junk>\n</a>\n");
/// assert!(xp.parse_start("a"));
/// let mut n = 0;
/// while xp.get_tag() {
///     if xp.match_tag("/a") {
///         break;
///     }
///     if let Some(v) = xp.parse_int("n") {
///         n = v;
///         continue;
///     }
///     xp.skip_unexpected();
/// }
/// assert_eq!(n, 3);
/// ```
///
/// Note that [`XmlParser::get_tag`] returns `true` when it read a token;
/// upstream's `get_tag` returns `true` at end of input.
#[derive(Debug, Clone)]
pub struct XmlParser<'a> {
    src: &'a [u8],
    pos: usize,
    tag: String,
    attrs: String,
    is_tag: bool,
}

impl<'a> XmlParser<'a> {
    /// Create a parser over `src`. A NUL byte ends the input, as it does
    /// upstream.
    pub fn new(src: &'a str) -> Self {
        let bytes = src.as_bytes();
        let end = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());
        XmlParser {
            src: &bytes[..end],
            pos: 0,
            tag: String::new(),
            attrs: String::new(),
            is_tag: false,
        }
    }

    /// Byte offset of the next unread byte.
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// The token read by the last [`get_tag`](Self::get_tag): a tag name
    /// without brackets (`foo`, `/foo`, `foo/`), or text.
    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// Whether the last token was a tag (as opposed to text or CDATA).
    pub fn is_tag(&self) -> bool {
        self.is_tag
    }

    /// Attributes of the last tag, with the whitespace after the tag name
    /// removed: `priority="low"` for `<message priority="low">`.
    pub fn attrs(&self) -> &str {
        &self.attrs
    }

    fn getc(&mut self) -> Option<u8> {
        let c = *self.src.get(self.pos)?;
        self.pos += 1;
        Some(c)
    }

    fn scan_nonws(&mut self) -> Option<u8> {
        loop {
            let c = self.getc()?;
            if !is_space(c) {
                return Some(c);
            }
        }
    }

    /// `XML_PARSER::scan_tag`: called after `<` was read.
    ///
    /// Whitespace is never copied. Before the first whitespace characters go
    /// to the tag name, after it to the attributes, except `/`, which always
    /// goes to the tag name. So `<foo />` yields `foo/`, and so does
    /// `<foo href="a/b">`: an attribute value containing a slash makes the
    /// tag look self-closing.
    fn scan_tag(&mut self) -> Option<RawTag> {
        let mut name: Vec<u8> = Vec::new();
        let mut attrs: Vec<u8> = Vec::new();
        let mut found_space = false;
        let mut i = 0usize;
        loop {
            let c = self.getc()?;
            if c == b'>' {
                return Some(RawTag::Tag(lossy(&name), lossy(&attrs)));
            }
            if is_space(c) {
                if found_space {
                    attrs.push(c);
                }
                found_space = true;
            } else if c == b'/' || !found_space {
                name.push(c);
            } else {
                attrs.push(c);
            }
            if i == 2 && name.starts_with(b"!--") {
                let rel = find(&self.src[self.pos..], b"-->");
                return match rel {
                    Some(rel) => {
                        self.pos += rel + 3;
                        Some(RawTag::Comment)
                    }
                    None => {
                        self.pos = self.src.len();
                        None
                    }
                };
            }
            if i == 7 && name.starts_with(b"![CDATA[") {
                let rel = find(&self.src[self.pos..], b"]]>");
                return match rel {
                    Some(rel) => {
                        let text = lossy(&self.src[self.pos..self.pos + rel]);
                        self.pos += rel + 3;
                        Some(RawTag::Cdata(text))
                    }
                    None => {
                        self.pos = self.src.len();
                        None
                    }
                };
            }
            i += 1;
        }
    }

    /// `XML_PARSER::get_aux`: next tag or text, stripped, comments skipped.
    /// Text that is not followed by a tag counts as end of input.
    fn get_aux(&mut self) -> Option<(Scan, String, String)> {
        loop {
            let c = self.scan_nonws()?;
            if c == b'<' {
                match self.scan_tag()? {
                    RawTag::Comment => continue,
                    RawTag::Tag(name, attrs) => return Some((Scan::Tag, name, attrs)),
                    RawTag::Cdata(text) => {
                        let text = strip_whitespace(&text).to_string();
                        return Some((Scan::Cdata, text, String::new()));
                    }
                }
            }
            let start = self.pos - 1;
            match self.src[self.pos..].iter().position(|&c| c == b'<') {
                None => {
                    self.pos = self.src.len();
                    return None;
                }
                Some(rel) => {
                    self.pos += rel;
                    let text = lossy(&self.src[start..self.pos]);
                    let text = strip_whitespace(&text).to_string();
                    return Some((Scan::Data, text, String::new()));
                }
            }
        }
    }

    /// Read the next token (tag or text). Returns `false` at end of input.
    pub fn get_tag(&mut self) -> bool {
        match self.get_aux() {
            None => false,
            Some((kind, text, attrs)) => {
                self.is_tag = matches!(kind, Scan::Tag);
                self.tag = text;
                self.attrs = attrs;
                true
            }
        }
    }

    /// Whether the current token is the tag `name`.
    pub fn match_tag(&self, name: &str) -> bool {
        self.is_tag && self.tag == name
    }

    /// Whether the current token is a self-closing tag (`<foo/>`).
    pub fn is_self_closing(&self) -> bool {
        self.is_tag && self.tag.ends_with('/')
    }

    /// Read the document's start tag, skipping an optional `<?xml ?>`
    /// declaration (`XML_PARSER::parse_start`).
    pub fn parse_start(&mut self, start_tag: &str) -> bool {
        if !self.get_tag() || !self.is_tag {
            return false;
        }
        if self.tag.contains("?xml") && (!self.get_tag() || !self.is_tag) {
            return false;
        }
        self.tag == start_tag
    }

    fn end_tag_follows(&mut self, end_tag: &str) -> bool {
        matches!(self.get_aux(), Some((Scan::Tag, t, _)) if t == end_tag)
    }

    /// If the current tag is `name`, read its text and closing tag
    /// (`XML_PARSER::parse_str` and `parse_string`).
    ///
    /// `<name/>` and `<name></name>` give an empty string. The text is
    /// unescaped unless it came from a CDATA section. Returns `None` when
    /// the tag does not match, or when the element contains nested tags or
    /// is not closed; in the second case tokens have been consumed and the
    /// caller normally falls through to [`skip_unexpected`](Self::skip_unexpected).
    pub fn parse_str(&mut self, name: &str) -> Option<String> {
        if !self.is_tag {
            return None;
        }
        if self.tag.strip_suffix('/') == Some(name) {
            return Some(String::new());
        }
        if self.tag != name {
            return None;
        }
        let end_tag = format!("/{name}");
        let (kind, text, _) = self.get_aux()?;
        match kind {
            Scan::Tag => (text == end_tag).then(String::new),
            Scan::Data => self.end_tag_follows(&end_tag).then(|| xml_unescape(&text)),
            Scan::Cdata => self.end_tag_follows(&end_tag).then_some(text),
        }
    }

    /// Text of a numeric element; `Some(None)` stands for `<name></name>`.
    fn numeric_text(&mut self, name: &str) -> Option<(Option<String>, String)> {
        if !self.match_tag(name) {
            return None;
        }
        let end_tag = format!("/{name}");
        let (kind, text, _) = self.get_aux()?;
        match kind {
            Scan::Tag => (text == end_tag).then_some((None, end_tag)),
            _ => Some((Some(text), end_tag)),
        }
    }

    /// If the current tag is `name`, read a C `long` (`XML_PARSER::parse_long`).
    ///
    /// The number is read as `strtol` with base 0 reads it: decimal, `0x`
    /// hexadecimal, or octal with a leading zero. The whole text must be
    /// numeric. `<name></name>` gives 0.
    pub fn parse_long(&mut self, name: &str) -> Option<i64> {
        let (text, end_tag) = self.numeric_text(name)?;
        let Some(text) = text else {
            return Some(0);
        };
        let (val, used, overflow) = c_strtol(&text);
        if overflow || used != text.len() {
            return None;
        }
        self.end_tag_follows(&end_tag).then_some(val)
    }

    /// If the current tag is `name`, read a C `int` (`XML_PARSER::parse_int`).
    /// Values outside the `int` range are truncated as a C cast does.
    pub fn parse_int(&mut self, name: &str) -> Option<i32> {
        self.parse_long(name).map(|v| v as i32)
    }

    /// If the current tag is `name`, read an unsigned 64-bit integer
    /// (`XML_PARSER::parse_ulonglong`).
    pub fn parse_u64(&mut self, name: &str) -> Option<u64> {
        let (text, end_tag) = self.numeric_text(name)?;
        let Some(text) = text else {
            return Some(0);
        };
        let val = if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
            u64::from_str_radix(hex, 16).ok()?
        } else {
            text.parse::<u64>().ok()?
        };
        self.end_tag_follows(&end_tag).then_some(val)
    }

    /// If the current tag is `name`, read a double
    /// (`XML_PARSER::parse_double`). `<name></name>` gives 0. The value may
    /// be `inf` or `nan`; callers check for finiteness where upstream does.
    pub fn parse_double(&mut self, name: &str) -> Option<f64> {
        let (text, end_tag) = self.numeric_text(name)?;
        let Some(text) = text else {
            return Some(0.0);
        };
        let (val, used, range_error) = c_strtod(&text);
        if range_error || used != text.len() {
            return None;
        }
        self.end_tag_follows(&end_tag).then_some(val)
    }

    /// If the current tag is `name`, read a boolean
    /// (`XML_PARSER::parse_bool`): `<name/>` is true, `<name>N</name>` is
    /// true for non-zero `N`. `<name></name>` does not parse.
    pub fn parse_bool(&mut self, name: &str) -> Option<bool> {
        if !self.is_tag {
            return None;
        }
        if self.tag.strip_suffix('/') == Some(name) {
            return Some(true);
        }
        if self.tag != name {
            return None;
        }
        let (kind, text, _) = self.get_aux()?;
        if matches!(kind, Scan::Tag) {
            return None;
        }
        let (val, used, _) = c_strtol(&text);
        if used != text.len() {
            return None;
        }
        let end_tag = format!("/{name}");
        self.end_tag_follows(&end_tag).then_some(val != 0)
    }

    /// Skip the element whose start tag is the current token
    /// (`XML_PARSER::skip_unexpected`).
    ///
    /// Does nothing for end tags and self-closing tags. Otherwise reads
    /// until the matching end tag, skipping nested elements the same way;
    /// an element that is never closed swallows the rest of the input.
    /// Text tokens are ignored (upstream would treat the text as a tag name
    /// and scan for its end tag).
    pub fn skip_unexpected(&mut self) {
        if !self.is_tag || self.tag.contains('/') {
            return;
        }
        let mut stack = vec![format!("/{}", self.tag)];
        while let Some(end_tag) = stack.last() {
            let Some(c) = self.scan_nonws() else {
                return;
            };
            if c != b'<' {
                continue;
            }
            match self.scan_tag() {
                None => return,
                Some(RawTag::Tag(name, _)) => {
                    if name == *end_tag {
                        stack.pop();
                    } else if !name.contains('/') {
                        stack.push(format!("/{name}"));
                    }
                }
                Some(_) => {}
            }
        }
    }

    /// Copy the raw text up to `end_tag` (given with brackets, as in
    /// `"</stderr_out>"`) and move past the end tag. Nothing is stripped or
    /// unescaped (`copy_element_contents` in `lib/parse.cpp`).
    pub fn copy_element_contents(&mut self, end_tag: &str) -> Result<String, ProtoError> {
        match find(&self.src[self.pos..], end_tag.as_bytes()) {
            None => {
                self.pos = self.src.len();
                Err(ProtoError::NoEndTag(end_tag.to_string()))
            }
            Some(rel) => {
                let text = lossy(&self.src[self.pos..self.pos + rel]);
                self.pos += rel + end_tag.len();
                Ok(text)
            }
        }
    }

    /// Like [`copy_element_contents`](Self::copy_element_contents), with
    /// whitespace stripped from both ends (`XML_PARSER::element_contents`).
    pub fn element_contents(&mut self, end_tag: &str) -> Result<String, ProtoError> {
        let raw = self.copy_element_contents(end_tag)?;
        Ok(strip_whitespace(&raw).to_string())
    }

    /// Copy the element whose start tag is the current token, including
    /// start and end tags (`XML_PARSER::copy_element`). Attributes of the
    /// start tag are dropped, as upstream drops them.
    pub fn copy_element(&mut self) -> Result<String, ProtoError> {
        if !self.is_tag {
            return Err(ProtoError::Invalid("copy_element on text".to_string()));
        }
        if self.tag.ends_with('/') {
            return Ok(format!("<{}>", self.tag));
        }
        if self.tag.contains('/') {
            return Err(ProtoError::Invalid(format!(
                "copy_element on <{}>",
                self.tag
            )));
        }
        let end_tag = format!("</{}>", self.tag);
        let body = self.element_contents(&end_tag)?;
        Ok(format!("<{}>{}{}", self.tag, body, end_tag))
    }

    /// Read one line including its newline, like `fgets`. Used by the
    /// line-oriented parsers that upstream runs on the same stream.
    pub fn next_line(&mut self) -> Option<&'a str> {
        if self.pos >= self.src.len() {
            return None;
        }
        let rest = &self.src[self.pos..];
        let len = rest
            .iter()
            .position(|&c| c == b'\n')
            .map_or(rest.len(), |i| i + 1);
        self.pos += len;
        // The source was a `&str` and the cut points are ASCII bytes or
        // token boundaries; fall back to an empty line if a caller moved
        // the position into the middle of a character.
        Some(std::str::from_utf8(&rest[..len]).unwrap_or(""))
    }
}

/// The string-search parsers of `lib/parse.cpp` (`match_tag`, `parse_int`,
/// `parse_double`, `parse_str`, `parse_bool` taking a buffer and a tag).
///
/// They look for the tag text anywhere in the buffer and read what follows,
/// without checking for an end tag. Tags are passed with brackets
/// (`"<nbytes>"`), except for [`parse_bool`](line::parse_bool).
pub mod line {
    use super::{c_strtod, c_strtol, strip_whitespace, xml_unescape};

    /// Whether `buf` contains `tag`.
    pub fn match_tag(buf: &str, tag: &str) -> bool {
        buf.contains(tag)
    }

    /// Integer following `tag`. Text that is not a number reads as 0, as
    /// `strtol` reads it.
    pub fn parse_int(buf: &str, tag: &str) -> Option<i32> {
        let at = buf.find(tag)?;
        let (val, _, overflow) = c_strtol(&buf[at + tag.len()..]);
        if overflow {
            return None;
        }
        Some(val as i32)
    }

    /// Double following `tag`; `None` when the tag is absent or the value
    /// is not finite.
    pub fn parse_double(buf: &str, tag: &str) -> Option<f64> {
        let at = buf.find(tag)?;
        let (val, _, range_error) = c_strtod(&buf[at + tag.len()..]);
        if range_error || !val.is_finite() {
            return None;
        }
        Some(val)
    }

    /// Text between the `>` that closes `tag` and the next `<`, stripped
    /// and unescaped. Pass `"<tag"` when the tag may carry attributes.
    pub fn parse_str(buf: &str, tag: &str) -> Option<String> {
        let at = buf.find(tag)?;
        let rest = &buf[at..];
        let start = rest.find('>')? + 1;
        let rest = &rest[start..];
        let end = rest.find('<')?;
        Some(xml_unescape(strip_whitespace(&rest[..end])))
    }

    /// Boolean named `tag` (without brackets): `<tag/>` and `<tag />` are
    /// true, `<tag>N` is true for non-zero `N`.
    pub fn parse_bool(buf: &str, tag: &str) -> Option<bool> {
        if !buf.contains(tag) {
            return None;
        }
        if buf.contains(&format!("<{tag}/>")) || buf.contains(&format!("<{tag} />")) {
            return Some(true);
        }
        parse_int(buf, &format!("<{tag}>")).map(|x| x != 0)
    }
}

/// Select the preferences that apply to a venue (`extract_venue` in
/// `lib/parse.cpp`).
///
/// If `input` contains `<venue name="venue_name">`, the contents of that
/// element are returned. Otherwise all `<venue>` elements are removed and
/// the rest is returned.
pub fn extract_venue(input: &str, venue_name: &str) -> String {
    let open = format!("<venue name=\"{venue_name}\">");
    if let Some(at) = input.find(&open) {
        let body = &input[at + open.len()..];
        return match body.find("</venue") {
            Some(end) => body[..end].to_string(),
            None => body.to_string(),
        };
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    loop {
        match rest.find("<venue") {
            None => {
                out.push_str(rest);
                break;
            }
            Some(at) => {
                out.push_str(&rest[..at]);
                match rest[at..].find("</venue>") {
                    None => break,
                    Some(end) => rest = &rest[at + end + "</venue>".len()..],
                }
            }
        }
    }
    out
}

/// A minimal writer for the `printf`-style XML that BOINC emits.
///
/// Every element goes on its own line, which is what the line-oriented
/// parsers on the other side expect.
#[derive(Debug, Clone, Default)]
pub struct XmlWriter {
    buf: String,
}

impl XmlWriter {
    /// An empty writer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append text verbatim.
    pub fn raw(&mut self, text: &str) -> &mut Self {
        self.buf.push_str(text);
        self
    }

    /// Append text verbatim, adding a newline unless it already ends with one.
    pub fn raw_line(&mut self, text: &str) -> &mut Self {
        self.buf.push_str(text);
        if !text.ends_with('\n') {
            self.buf.push('\n');
        }
        self
    }

    fn indent(&mut self, indent: usize) {
        for _ in 0..indent {
            self.buf.push(' ');
        }
    }

    /// `<tag>` on its own line.
    pub fn open(&mut self, indent: usize, tag: &str) -> &mut Self {
        self.indent(indent);
        self.buf.push('<');
        self.buf.push_str(tag);
        self.buf.push_str(">\n");
        self
    }

    /// `</tag>` on its own line.
    pub fn close(&mut self, indent: usize, tag: &str) -> &mut Self {
        self.indent(indent);
        self.buf.push_str("</");
        self.buf.push_str(tag);
        self.buf.push_str(">\n");
        self
    }

    /// `<tag>value</tag>` with the value written verbatim.
    pub fn elem_raw(&mut self, indent: usize, tag: &str, value: &str) -> &mut Self {
        self.indent(indent);
        self.buf.push('<');
        self.buf.push_str(tag);
        self.buf.push('>');
        self.buf.push_str(value);
        self.buf.push_str("</");
        self.buf.push_str(tag);
        self.buf.push_str(">\n");
        self
    }

    /// `<tag>value</tag>` with the value escaped by [`xml_escape`].
    pub fn elem_str(&mut self, indent: usize, tag: &str, value: &str) -> &mut Self {
        let escaped = xml_escape(value);
        self.elem_raw(indent, tag, &escaped)
    }

    /// `<tag>N</tag>`.
    pub fn elem_int(&mut self, indent: usize, tag: &str, value: i64) -> &mut Self {
        self.elem_raw(indent, tag, &value.to_string())
    }

    /// `<tag>X</tag>` with `%f` formatting.
    pub fn elem_double(&mut self, indent: usize, tag: &str, value: f64) -> &mut Self {
        self.elem_raw(indent, tag, &crate::fmt_f(value))
    }

    /// `<tag/>` on its own line.
    pub fn flag(&mut self, indent: usize, tag: &str) -> &mut Self {
        self.indent(indent);
        self.buf.push('<');
        self.buf.push_str(tag);
        self.buf.push_str("/>\n");
        self
    }

    /// The text written so far.
    pub fn as_str(&self) -> &str {
        &self.buf
    }

    /// Consume the writer and return the text.
    pub fn finish(self) -> String {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(src: &str) -> Vec<(bool, String)> {
        let mut xp = XmlParser::new(src);
        let mut out = Vec::new();
        while xp.get_tag() {
            out.push((xp.is_tag(), xp.tag().to_string()));
        }
        out
    }

    #[test]
    fn tokenizes_tags_and_text() {
        let t = tokens("<a>\n  <b>  hello world \n</b>\n</a>\n");
        assert_eq!(
            t,
            vec![
                (true, "a".to_string()),
                (true, "b".to_string()),
                (false, "hello world".to_string()),
                (true, "/b".to_string()),
                (true, "/a".to_string()),
            ]
        );
    }

    #[test]
    fn tag_attributes_and_self_closing() {
        let mut xp = XmlParser::new("<message priority=\"low\">hi</message><foo /><bar/>");
        assert!(xp.get_tag());
        assert_eq!(xp.tag(), "message");
        assert_eq!(xp.attrs(), "priority=\"low\"");
        assert_eq!(xp.parse_str("message").as_deref(), Some("hi"));
        assert!(xp.get_tag());
        assert_eq!(xp.tag(), "foo/");
        assert!(xp.is_self_closing());
        assert!(xp.get_tag());
        assert_eq!(xp.tag(), "bar/");
        assert!(!xp.get_tag());
    }

    #[test]
    fn slash_in_attribute_lands_in_tag_name() {
        // Upstream quirk, kept on purpose.
        let mut xp = XmlParser::new("<a href=\"x/y\">");
        assert!(xp.get_tag());
        assert_eq!(xp.tag(), "a/");
        assert_eq!(xp.attrs(), "href=\"xy\"");
    }

    #[test]
    fn comments_are_skipped() {
        let t = tokens("<a><!-- <ignored> --><b/></a>");
        assert_eq!(t.len(), 3);
        assert_eq!(t[1].1, "b/");
    }

    #[test]
    fn trailing_text_without_tag_is_eof() {
        let mut xp = XmlParser::new("<a>text");
        assert!(xp.get_tag());
        assert!(!xp.get_tag());
    }

    #[test]
    fn nul_ends_input() {
        let t = tokens("<a>\0<b>");
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn parse_start_skips_declaration() {
        let mut xp = XmlParser::new("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<doc>\n</doc>");
        assert!(xp.parse_start("doc"));
        let mut xp = XmlParser::new("<other>");
        assert!(!xp.parse_start("doc"));
        let mut xp = XmlParser::new("text<doc>");
        assert!(!xp.parse_start("doc"));
    }

    #[test]
    fn parse_str_forms() {
        let mut xp = XmlParser::new(
            "<a>x &lt; y &amp; z</a><b/><c></c><d><![CDATA[ raw &lt; <tag> ]]></d><e>1<f/></e>",
        );
        assert!(xp.get_tag());
        assert_eq!(xp.parse_str("zzz"), None);
        assert_eq!(xp.parse_str("a").as_deref(), Some("x < y & z"));
        assert!(xp.get_tag());
        assert_eq!(xp.parse_str("b").as_deref(), Some(""));
        assert!(xp.get_tag());
        assert_eq!(xp.parse_str("c").as_deref(), Some(""));
        assert!(xp.get_tag());
        assert_eq!(xp.parse_str("d").as_deref(), Some("raw &lt; <tag>"));
        assert!(xp.get_tag());
        assert_eq!(xp.parse_str("e"), None);
    }

    #[test]
    fn parse_str_does_not_match_text_tokens() {
        let mut xp = XmlParser::new("<a>b</a><b>1</b>");
        assert!(xp.get_tag());
        assert!(xp.get_tag());
        assert_eq!(xp.tag(), "b");
        assert!(!xp.is_tag());
        assert!(!xp.match_tag("b"));
        assert_eq!(xp.parse_str("b"), None);
        assert_eq!(xp.parse_int("b"), None);
    }

    #[test]
    fn parse_int_forms() {
        let mut xp = XmlParser::new(
            "<a>42</a><b></b><c>-7</c><d>0x1f</d><e>010</e><f>12abc</f><g>1.5</g><h>99999999999999999999</h><i>4294967297</i>",
        );
        let mut next = |name: &str| {
            assert!(xp.get_tag());
            let v = xp.parse_int(name);
            if v.is_none() {
                xp.skip_unexpected();
            }
            v
        };
        assert_eq!(next("a"), Some(42));
        assert_eq!(next("b"), Some(0));
        assert_eq!(next("c"), Some(-7));
        assert_eq!(next("d"), Some(31));
        assert_eq!(next("e"), Some(8));
        assert_eq!(next("f"), None);
        assert_eq!(next("g"), None);
        assert_eq!(next("h"), None);
        assert_eq!(next("i"), Some(1));
    }

    #[test]
    fn parse_int_rejects_self_closing() {
        let mut xp = XmlParser::new("<a/>");
        assert!(xp.get_tag());
        assert_eq!(xp.parse_int("a"), None);
    }

    #[test]
    fn parse_long_and_u64() {
        let mut xp = XmlParser::new("<a>8589934592</a><b>18446744073709551615</b>");
        assert!(xp.get_tag());
        assert_eq!(xp.parse_long("a"), Some(8_589_934_592));
        assert!(xp.get_tag());
        assert_eq!(xp.parse_u64("b"), Some(u64::MAX));
    }

    #[test]
    fn parse_double_forms() {
        let mut xp = XmlParser::new(
            "<a>1.500000</a><b></b><c>1e3</c><d>-2.5E-1</d><e>inf</e><f>nan</f><g>1.5x</g><h>1e999</h><i>.5</i><j>3.</j>",
        );
        let mut next = |name: &str| {
            assert!(xp.get_tag());
            let v = xp.parse_double(name);
            if v.is_none() {
                xp.skip_unexpected();
            }
            v
        };
        assert_eq!(next("a"), Some(1.5));
        assert_eq!(next("b"), Some(0.0));
        assert_eq!(next("c"), Some(1000.0));
        assert_eq!(next("d"), Some(-0.25));
        assert_eq!(next("e"), Some(f64::INFINITY));
        assert!(next("f").unwrap().is_nan());
        assert_eq!(next("g"), None);
        assert_eq!(next("h"), None);
        assert_eq!(next("i"), Some(0.5));
        assert_eq!(next("j"), Some(3.0));
    }

    #[test]
    fn parse_bool_forms() {
        let mut xp = XmlParser::new("<a/><b>1</b><c>0</c><e>yes</e><f /><d></d>");
        let mut next = |name: &str| {
            assert!(xp.get_tag());
            let v = xp.parse_bool(name);
            if v.is_none() {
                xp.skip_unexpected();
            }
            v
        };
        assert_eq!(next("a"), Some(true));
        assert_eq!(next("b"), Some(true));
        assert_eq!(next("c"), Some(false));
        assert_eq!(next("e"), None);
        assert_eq!(next("f"), Some(true));
        // Upstream reads the end tag while looking for the value.
        assert_eq!(next("d"), None);
    }

    #[test]
    fn skip_unexpected_handles_nesting() {
        let src = "<doc><junk><a><junk>1</junk></a><b/><!-- c --><![CDATA[ </junk> ]]></junk><keep>1</keep></doc>";
        let mut xp = XmlParser::new(src);
        assert!(xp.parse_start("doc"));
        assert!(xp.get_tag());
        assert_eq!(xp.tag(), "junk");
        xp.skip_unexpected();
        assert!(xp.get_tag());
        assert_eq!(xp.parse_int("keep"), Some(1));
        assert!(xp.get_tag());
        assert!(xp.match_tag("/doc"));
    }

    #[test]
    fn skip_unexpected_ignores_end_and_empty_tags() {
        let mut xp = XmlParser::new("<a/></b><c>1</c>");
        assert!(xp.get_tag());
        xp.skip_unexpected();
        assert!(xp.get_tag());
        assert_eq!(xp.tag(), "/b");
        xp.skip_unexpected();
        assert!(xp.get_tag());
        assert_eq!(xp.parse_int("c"), Some(1));
    }

    #[test]
    fn skip_unexpected_unclosed_swallows_rest() {
        let mut xp = XmlParser::new("<a><b>1</b>");
        assert!(xp.get_tag());
        xp.skip_unexpected();
        assert!(!xp.get_tag());
    }

    #[test]
    fn skip_unexpected_deep_nesting_does_not_overflow() {
        let mut src = String::new();
        for _ in 0..200_000 {
            src.push_str("<a>");
        }
        let mut xp = XmlParser::new(&src);
        assert!(xp.get_tag());
        xp.skip_unexpected();
        assert!(!xp.get_tag());
    }

    #[test]
    fn copy_element_forms() {
        let mut xp = XmlParser::new(
            "<file_info>\n    <name>f</name>\n    <nbytes>1.0</nbytes>\n</file_info>\n<x/><y>",
        );
        assert!(xp.get_tag());
        assert_eq!(
            xp.copy_element().unwrap(),
            "<file_info><name>f</name>\n    <nbytes>1.0</nbytes></file_info>"
        );
        assert!(xp.get_tag());
        assert_eq!(xp.copy_element().unwrap(), "<x/>");
        assert!(xp.get_tag());
        assert!(matches!(xp.copy_element(), Err(ProtoError::NoEndTag(_))));
    }

    #[test]
    fn raw_contents() {
        let mut xp = XmlParser::new("<s>\n line 1\n<![CDATA[\n<x>\n]]>\n</s><t>1</t>");
        assert!(xp.get_tag());
        assert_eq!(
            xp.copy_element_contents("</s>").unwrap(),
            "\n line 1\n<![CDATA[\n<x>\n]]>\n"
        );
        assert!(xp.get_tag());
        assert_eq!(xp.parse_int("t"), Some(1));

        let mut xp = XmlParser::new("<s>  a <b/> c  </s>");
        assert!(xp.get_tag());
        assert_eq!(xp.element_contents("</s>").unwrap(), "a <b/> c");
    }

    #[test]
    fn next_line_reads_like_fgets() {
        let mut xp = XmlParser::new("<m>\nline one\nline two");
        assert!(xp.get_tag());
        assert_eq!(xp.next_line(), Some("\n"));
        assert_eq!(xp.next_line(), Some("line one\n"));
        assert_eq!(xp.next_line(), Some("line two"));
        assert_eq!(xp.next_line(), None);
    }

    #[test]
    fn escape_matches_upstream() {
        assert_eq!(xml_escape("a<b>&c"), "a&lt;b>&amp;c");
        assert_eq!(xml_escape("x]]>y]z"), "x]]&gt;y]z");
        assert_eq!(xml_escape("a\tb\nc\rd\x01e"), "a&#9;b&#10;c&#13;de");
        assert_eq!(xml_escape("\u{e9}"), "&#195;&#169;");
        assert_eq!(xml_escape("\"quoted\" 'x'"), "\"quoted\" 'x'");
    }

    #[test]
    fn escape_text_keeps_unicode_and_matches_escape_for_ascii() {
        assert_eq!(
            xml_escape_text("Jos\u{e9} \u{65e5}\u{672c}"),
            "Jos\u{e9} \u{65e5}\u{672c}"
        );
        assert_eq!(xml_escape_text("\u{1f600}"), "\u{1f600}");
        assert_eq!(xml_escape_text("a<b>&c"), "a&lt;b>&amp;c");
        assert_eq!(xml_escape_text("x]]>y]z"), "x]]&gt;y]z");
        assert_eq!(xml_escape_text("]]]>\u{e9}"), "]]]&gt;\u{e9}");
        assert_eq!(xml_escape_text("x]]\u{fffe}>y"), "x]]&gt;y");
        assert_eq!(xml_escape_text("x]\x01]\x02>y"), "x]]&gt;y");
        assert_eq!(xml_escape_text("x]]\n>y"), "x]]&#10;>y");
        assert_eq!(
            xml_escape_text("a\tb\nc\rd\x01e\u{fffe}"),
            "a&#9;b&#10;c&#13;de"
        );
        for s in ["", "plain", "a<b>&c]]>d", "t\t\r\n\x02", "\"q\" 'x'"] {
            assert_eq!(xml_escape_text(s), xml_escape(s));
        }
    }

    #[test]
    fn unescape_matches_upstream() {
        assert_eq!(
            xml_unescape("&lt;a&gt; &amp; &quot;b&quot; &apos;c&apos;"),
            "<a> & \"b\" 'c'"
        );
        assert_eq!(xml_unescape("a&#xD;&#xa;b"), "a\r\nb");
        assert_eq!(xml_unescape("&#65;&#10;"), "A\n");
        assert_eq!(xml_unescape("&#195;&#169;"), "\u{e9}");
        assert_eq!(xml_unescape("&#300;"), "&#300;");
        assert_eq!(xml_unescape("&#;"), "&#;");
        assert_eq!(xml_unescape("&#12"), "&#12");
        assert_eq!(xml_unescape("&#x41;"), "&#x41;");
        assert_eq!(xml_unescape("&unknown; & alone"), "&unknown; & alone");
        assert_eq!(xml_unescape("&amp;lt;"), "&lt;");
    }

    #[test]
    fn escape_round_trip() {
        for s in [
            "plain",
            "a<b>&c \"q\"",
            "tab\tnl\n",
            "caf\u{e9} \u{4e2d}\u{6587}",
            "]]>",
        ] {
            assert_eq!(xml_unescape(&xml_escape(s)), s);
        }
    }

    #[test]
    fn line_parsers() {
        let buf = "    <nbytes>1234.000000</nbytes>\n<offset>0</offset><name> a&amp;b </name>";
        assert!(line::match_tag(buf, "<nbytes>"));
        assert_eq!(line::parse_double(buf, "<nbytes>"), Some(1234.0));
        assert_eq!(line::parse_int(buf, "<offset>"), Some(0));
        assert_eq!(line::parse_int(buf, "<missing>"), None);
        assert_eq!(line::parse_int("<n>abc</n>", "<n>"), Some(0));
        assert_eq!(line::parse_int("<n> 12abc</n>", "<n>"), Some(12));
        assert_eq!(line::parse_double("<x>inf</x>", "<x>"), None);
        assert_eq!(line::parse_str(buf, "<name>").as_deref(), Some("a&b"));
        assert_eq!(line::parse_str("<name>unterminated", "<name>"), None);
        assert_eq!(line::parse_bool("<flag/>", "flag"), Some(true));
        assert_eq!(line::parse_bool("<flag />", "flag"), Some(true));
        assert_eq!(line::parse_bool("<flag>0</flag>", "flag"), Some(false));
        assert_eq!(line::parse_bool("<flag>1</flag>", "flag"), Some(true));
        assert_eq!(line::parse_bool("<other/>", "flag"), None);
    }

    #[test]
    fn venue_extraction() {
        let prefs = "<a>1</a>\n<venue name=\"home\">\n<a>2</a>\n</venue>\n<venue name=\"work\">\n<a>3</a>\n</venue>\n<b>4</b>\n";
        assert_eq!(extract_venue(prefs, "home"), "\n<a>2</a>\n");
        assert_eq!(extract_venue(prefs, "work"), "\n<a>3</a>\n");
        assert_eq!(extract_venue(prefs, ""), "<a>1</a>\n\n\n<b>4</b>\n");
        assert_eq!(extract_venue(prefs, "school"), "<a>1</a>\n\n\n<b>4</b>\n");
        assert_eq!(extract_venue("<a/><venue name=\"x\"><b/>", "y"), "<a/>");
    }

    #[test]
    fn c_number_helpers() {
        assert_eq!(c_strtol("  -12rest"), (-12, 5, false));
        assert_eq!(c_strtol("0x"), (0, 1, false));
        assert_eq!(c_strtol("abc"), (0, 0, false));
        assert!(c_strtol("9223372036854775808").2);
        assert_eq!(c_strtol("-9223372036854775808"), (i64::MIN, 20, false));
        assert_eq!(c_strtod("1.5e"), (1.5, 3, false));
        assert_eq!(c_strtod("+2"), (2.0, 2, false));
        assert_eq!(c_strtod("."), (0.0, 0, false));
        assert_eq!(c_strtod("-Infinity"), (f64::NEG_INFINITY, 9, false));
        assert_eq!(c_atoi(" 42 7"), 42);
        assert_eq!(c_atoi("x"), 0);
        assert_eq!(c_atoi("-3"), -3);
    }

    #[test]
    fn writer_output() {
        let mut w = XmlWriter::new();
        w.open(0, "doc")
            .elem_str(4, "name", "a<b")
            .elem_int(4, "n", -3)
            .elem_double(4, "x", 0.5)
            .flag(4, "flag")
            .raw_line("<raw/>")
            .close(0, "doc");
        assert_eq!(
            w.finish(),
            "<doc>\n    <name>a&lt;b</name>\n    <n>-3</n>\n    <x>0.500000</x>\n    <flag/>\n<raw/>\n</doc>\n"
        );
    }
}
