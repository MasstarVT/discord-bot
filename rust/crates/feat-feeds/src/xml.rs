//! A port of the part of `fast-xml-parser` 5.x (`XMLParser.parse`, `OrderedObjParser.parseXml`, `node2json.compress`
//! and `@nodable/entities`' `EntityDecoder`) that `feedParsers.js` uses, with exactly its `XML_PARSER_OPTIONS`:
//! attributes kept with the `@_` prefix, namespace prefixes removed, text under `#text`, no value parsing, values
//! trimmed, entities (XML + the common HTML and currency names, numeric references) decoded, the XML declaration and
//! processing instructions ignored, boolean attributes as `true`, at most 40 nested tags.
//!
//! Why a port and not an XML crate: the JS parser is a tolerant, non-validating character scanner. Unclosed tags at
//! the end of input, stray closing tags, a raw `&` in text, `<!foo>` read as an element and its attribute regex all
//! shape what a feed parses to; a conforming XML parser rejects or reshapes those documents. The result is the same
//! JSON-like tree the JS builds (`serde_json::Value` with insertion order), so the readers of `feed_parsers` are
//! ported line for line, and the parity tests compare this tree with the JS one.

use bot_util::jsre::JsRegex;
use serde_json::{Map, Value};
use std::sync::LazyLock;

/// `maxNestedTags` of `XML_PARSER_OPTIONS`.
pub const MAX_NESTED_TAGS: usize = 40;

/// A parse failure (the JS parser throws an `Error`; `parseXmlFeed` turns any of them into `INVALID_XML`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlError(pub String);

impl std::fmt::Display for XmlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for XmlError {}

fn fail<T>(msg: &str) -> Result<T, XmlError> {
    Err(XmlError(msg.to_string()))
}

// ── Entities ───────────────────────────────────────────────────────────────────

/// The decoder's named entities: `{ ...XML, ...COMMON_HTML, ...CURRENCY }` of `@nodable/entities` 3.1.
const NAMED_ENTITIES: [(&str, &str); 40] = [
    ("amp", "&"),
    ("apos", "'"),
    ("gt", ">"),
    ("lt", "<"),
    ("quot", "\""),
    ("nbsp", "\u{a0}"),
    ("copy", "©"),
    ("reg", "®"),
    ("trade", "™"),
    ("mdash", "—"),
    ("ndash", "–"),
    ("hellip", "…"),
    ("laquo", "«"),
    ("raquo", "»"),
    ("lsquo", "‘"),
    ("rsquo", "’"),
    ("ldquo", "“"),
    ("rdquo", "”"),
    ("bull", "•"),
    ("para", "¶"),
    ("sect", "§"),
    ("deg", "°"),
    ("frac12", "½"),
    ("frac14", "¼"),
    ("frac34", "¾"),
    ("cent", "¢"),
    ("pound", "£"),
    ("curren", "¤"),
    ("yen", "¥"),
    ("euro", "€"),
    ("dollar", "$"),
    ("fnof", "ƒ"),
    ("inr", "₹"),
    ("af", "؋"),
    ("birr", "ብር"),
    ("peso", "₱"),
    ("rub", "₽"),
    ("won", "₩"),
    ("yuan", "¥"),
    ("cedil", "¸"),
];

/// JS `parseInt(s, radix)` for radix 10 or 16: leading whitespace, a sign, an optional `0x` for radix 16, then the
/// longest run of digits. `None` is NaN.
fn js_parse_int(s: &[u16], radix: u32) -> Option<f64> {
    let mut i = 0;
    while i < s.len() && char::from_u32(u32::from(s[i])).is_some_and(bot_util::js::is_js_space) {
        i += 1;
    }
    let mut neg = false;
    if i < s.len() && (s[i] == u16::from(b'+') || s[i] == u16::from(b'-')) {
        neg = s[i] == u16::from(b'-');
        i += 1;
    }
    if radix == 16 && i + 1 < s.len() && s[i] == u16::from(b'0') && (s[i + 1] == u16::from(b'x') || s[i + 1] == u16::from(b'X')) {
        i += 2;
    }
    let mut v = 0f64;
    let mut any = false;
    while i < s.len() {
        let Some(d) = char::from_u32(u32::from(s[i])).and_then(|c| c.to_digit(radix)) else { break };
        v = v * f64::from(radix) + f64::from(d);
        any = true;
        i += 1;
    }
    if !any {
        return None;
    }
    Some(if neg { -v } else { v })
}

/// The entity decoder of one document (`EntityDecoder.decode`): a single scan for `&…;` (the `;` within 32 units),
/// named references from [`NAMED_ENTITIES`], numeric ones by code point. NUL, surrogates and (in XML 1.0) the C0
/// controls other than tab, LF and CR are removed; an out-of-range or unknown reference stays as written.
#[derive(Debug, Clone, Copy)]
struct Entities {
    xml11: bool,
}

impl Entities {
    fn decode(&self, s: &str) -> String {
        if !s.contains('&') {
            return s.to_string();
        }
        let u: Vec<u16> = s.encode_utf16().collect();
        let len = u.len();
        let mut out: Vec<u16> = Vec::with_capacity(len);
        let mut last = 0;
        let mut i = 0;
        while i < len {
            if u[i] != u16::from(b'&') {
                i += 1;
                continue;
            }
            let mut j = i + 1;
            while j < len && u[j] != u16::from(b';') && (j - i) <= 32 {
                j += 1;
            }
            if j >= len || u[j] != u16::from(b';') {
                i += 1;
                continue;
            }
            let token = &u[i + 1..j];
            if token.is_empty() {
                i += 1;
                continue;
            }
            let replacement: Option<Vec<u16>> = if token[0] == u16::from(b'#') {
                self.ncr(token)
            } else {
                let name = String::from_utf16_lossy(token);
                NAMED_ENTITIES.iter().find(|(n, _)| *n == name).map(|(_, v)| v.encode_utf16().collect())
            };
            let Some(rep) = replacement else {
                i += 1;
                continue;
            };
            out.extend_from_slice(&u[last..i]);
            out.extend_from_slice(&rep);
            last = j + 1;
            i = last;
        }
        out.extend_from_slice(&u[last..]);
        String::from_utf16_lossy(&out)
    }

    fn ncr(&self, token: &[u16]) -> Option<Vec<u16>> {
        let hex = token.len() > 1 && (token[1] == u16::from(b'x') || token[1] == u16::from(b'X'));
        let cp = if hex { js_parse_int(&token[2..], 16) } else { js_parse_int(&token[1..], 10) }?;
        // -0 is not below 0 in JS.
        if cp < 0.0 || cp > 1_114_111.0 {
            return None;
        }
        let cp = cp as u32;
        let removed = cp == 0 || (0xD800..=0xDFFF).contains(&cp) || (!self.xml11 && (1..=0x1F).contains(&cp) && ![9, 10, 13].contains(&cp));
        if removed {
            return Some(Vec::new());
        }
        char::from_u32(cp).map(|c| c.encode_utf16(&mut [0u16; 2]).to_vec())
    }
}

// ── The ordered tree ───────────────────────────────────────────────────────────

#[derive(Debug)]
enum Child {
    Text(String),
    Elem(Node),
}

#[derive(Debug)]
struct Node {
    name: String,
    attrs: Option<Map<String, Value>>,
    children: Vec<Child>,
}

impl Node {
    fn new(name: String) -> Self {
        Self { name, attrs: None, children: Vec::new() }
    }
}

const CRITICAL: [&str; 3] = ["__proto__", "constructor", "prototype"];
const DANGEROUS: [&str; 7] = ["hasOwnProperty", "toString", "valueOf", "__defineGetter__", "__defineSetter__", "__lookupGetter__", "__lookupSetter__"];

/// `sanitizeName`: a prototype key throws; a few other `Object.prototype` names get a `__` prefix.
fn sanitize_name(name: String) -> Result<String, XmlError> {
    if CRITICAL.contains(&name.as_str()) {
        return Err(XmlError(format!("[SECURITY] Invalid name: \"{name}\" is a reserved JavaScript keyword that could cause prototype pollution")));
    }
    if DANGEROUS.contains(&name.as_str()) {
        return Ok(format!("__{name}"));
    }
    Ok(name)
}

/// `resolveNameSpace` with `removeNSPrefix` (attribute names): `xmlns…` disappears, `a:b` becomes `b`.
fn resolve_namespace(name: &str) -> String {
    let parts: Vec<&str> = name.split(':').collect();
    if parts[0] == "xmlns" {
        return String::new();
    }
    if parts.len() == 2 {
        return parts[1].to_string();
    }
    name.to_string()
}

static ATTRS_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r#"([^\s=]+)\s*(=\s*(['"])([\s\S]*?)\3)?"#, "gm"));

/// `buildAttributesMap`: `@_name` → trimmed, entity-decoded value; a name without a value is `true`. `None` when
/// nothing is left.
fn build_attributes(attr_str: &str, ent: &Entities) -> Result<Option<Map<String, Value>>, XmlError> {
    let units: Vec<u16> = attr_str.encode_utf16().collect();
    let mut attrs = Map::new();
    for m in ATTRS_RE.find_all(&units) {
        let raw_name = m.group(&units, 1).unwrap_or_default();
        let name = resolve_namespace(&raw_name);
        if name.is_empty() {
            continue;
        }
        let key = sanitize_name(format!("@_{name}"))?;
        match m.group(&units, 4) {
            Some(v) => {
                let v = ent.decode(bot_util::js::trim(&v));
                attrs.insert(key, Value::String(v));
            }
            None => {
                attrs.insert(key, Value::Bool(true));
            }
        }
    }
    Ok(if attrs.is_empty() { None } else { Some(attrs) })
}

/// `tagExpWithClosingIndex`: the text up to the closing `>` (or `?>`) outside quotes, tabs outside quotes as spaces.
fn tag_exp_with_closing_index(s: &[char], start: usize, closing: &str) -> Option<(String, usize)> {
    let close: Vec<char> = closing.chars().collect();
    let mut boundary: Option<char> = None;
    let mut out = String::new();
    let mut i = start;
    while i < s.len() {
        let c = s[i];
        if let Some(b) = boundary {
            if c == b {
                boundary = None;
            }
            out.push(c);
        } else if c == '"' || c == '\'' {
            boundary = Some(c);
            out.push(c);
        } else if c == close[0] && (close.len() == 1 || s.get(i + 1) == Some(&close[1])) {
            return Some((out, i));
        } else if c == '\t' {
            out.push(' ');
        } else {
            out.push(c);
        }
        i += 1;
    }
    None
}

struct TagExp {
    tag_name: String,
    tag_exp: String,
    close_index: usize,
    attr_exp_present: bool,
}

/// `readTagExp(xmlData, i, removeNSPrefix, closingChar)`.
fn read_tag_exp(s: &[char], i: usize, remove_ns: bool, closing: &str) -> Option<TagExp> {
    let (data, close_index) = tag_exp_with_closing_index(s, i + 1, closing)?;
    let mut tag_name = data.clone();
    let mut tag_exp = data.clone();
    let mut attr_exp_present = true;
    if let Some((sep, c)) = data.char_indices().find(|(_, c)| bot_util::js::is_js_space(*c)) {
        tag_name = data[..sep].to_string();
        tag_exp = bot_util::js::trim_start(&data[sep + c.len_utf8()..]).to_string();
    }
    if remove_ns && let Some(colon) = tag_name.find(':') {
        tag_name = tag_name[colon + 1..].to_string();
        attr_exp_present = tag_name != data[colon + 1..];
    }
    Some(TagExp { tag_name, tag_exp, close_index, attr_exp_present })
}

fn find_from(s: &[char], needle: &str, from: usize) -> Option<usize> {
    let n: Vec<char> = needle.chars().collect();
    if from > s.len() {
        return None;
    }
    (from..=s.len().saturating_sub(n.len())).find(|&k| s[k..].starts_with(&n))
}

/// `findClosingIndex`: the index of the last character of `needle` at or after `from`.
fn find_closing_index(s: &[char], needle: &str, from: usize, msg: &str) -> Result<usize, XmlError> {
    match find_from(s, needle, from) {
        Some(k) => Ok(k + needle.chars().count() - 1),
        None => fail(msg),
    }
}

fn has_seq(s: &[char], seq: &str, i: usize) -> bool {
    let n: Vec<char> = seq.chars().collect();
    s.get(i..i + n.len()).is_some_and(|w| w == n.as_slice())
}

/// `DocTypeReader.readDocType` without entity support (`feedParsers` rejects any `<!ENTITY`, so a DOCTYPE that reaches
/// the parser has none): the index of its closing `>`.
fn skip_doctype(s: &[char], start: usize) -> Result<usize, XmlError> {
    if !has_seq(s, "<!DOCTYPE", start) {
        return fail("Invalid Tag instead of DOCTYPE");
    }
    let mut i = start + 9;
    let mut angle = 1i32;
    let mut has_body = false;
    let mut comment = false;
    let mut quote: Option<char> = None;
    while i < s.len() {
        let c = s[i];
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if !has_body && !comment && (c == '"' || c == '\'') {
            quote = Some(c);
            i += 1;
            continue;
        }
        if c == '<' && !comment {
            if has_body && has_seq(s, "!ENTITY", i + 1) {
                return fail("Invalid DOCTYPE");
            } else if has_body && (has_seq(s, "!ELEMENT", i + 1) || has_seq(s, "!ATTLIST", i + 1) || has_seq(s, "!NOTATION", i + 1)) {
                // Not supported: skipped up to its `>`.
                match find_from(s, ">", i) {
                    Some(k) => i = k,
                    None => return fail("Unclosed DOCTYPE"),
                }
                i += 1;
                continue;
            } else if has_seq(s, "!--", i + 1) {
                comment = true;
            } else {
                return fail("Invalid DOCTYPE");
            }
            angle += 1;
        } else if c == '>' {
            if comment {
                if i >= 2 && s[i - 1] == '-' && s[i - 2] == '-' {
                    comment = false;
                    angle -= 1;
                }
            } else {
                angle -= 1;
            }
            if angle == 0 {
                return Ok(i);
            }
        } else if c == '[' {
            has_body = true;
        }
        i += 1;
    }
    fail("Unclosed DOCTYPE")
}

struct Parser {
    stack: Vec<Node>,
    ent: Entities,
}

impl Parser {
    fn current(&mut self) -> &mut Node {
        let n = self.stack.len() - 1;
        &mut self.stack[n]
    }

    /// `saveTextToParentTag`: trims, decodes and stores non-empty text in the current node.
    fn save_text(&mut self, text: &mut String) {
        if text.is_empty() {
            return;
        }
        let trimmed = bot_util::js::trim(text);
        if !trimmed.is_empty() {
            let v = self.ent.decode(trimmed);
            if !v.is_empty() {
                self.current().children.push(Child::Text(v));
            }
        }
        text.clear();
    }

    fn close_current(&mut self) {
        if self.stack.len() > 1 {
            let node = self.stack.pop().unwrap_or_else(|| Node::new(String::new()));
            self.current().children.push(Child::Elem(node));
        }
    }
}

/// `new XMLParser(XML_PARSER_OPTIONS).parse(xml)`: the JS object tree, or the error the JS throws.
pub fn parse(xml: &str) -> Result<Value, XmlError> {
    let normalized = xml.replace("\r\n", "\n").replace('\r', "\n");
    let s: Vec<char> = normalized.chars().collect();
    let mut p = Parser { stack: vec![Node::new("!xml".into())], ent: Entities { xml11: false } };
    let mut text = String::new();
    let mut doctype_found = false;
    let len = s.len();
    let mut i = 0;
    while i < len {
        let ch = s[i];
        if ch != '<' {
            text.push(ch);
            i += 1;
            continue;
        }
        let c1 = s.get(i + 1).copied();
        if c1 == Some('/') {
            let close = find_closing_index(&s, ">", i, "Closing Tag is not closed.")?;
            let raw: String = s[i + 2..close].iter().collect();
            let mut name = bot_util::js::trim(&raw).to_string();
            if let Some(colon) = name.find(':') {
                name = name[colon + 1..].to_string();
            }
            sanitize_name(name)?;
            p.save_text(&mut text);
            p.close_current();
            text.clear();
            i = close + 1;
        } else if c1 == Some('?') {
            let Some(tag) = read_tag_exp(&s, i, false, "?>") else { return fail("Pi Tag is not closed.") };
            p.save_text(&mut text);
            if let Some(attrs) = build_attributes(&tag.tag_exp, &p.ent)? {
                let ver = attrs.get("@_version").and_then(Value::as_str).map(|v| bot_util::js::trim(v).parse::<f64>().unwrap_or(f64::NAN));
                p.ent.xml11 = ver == Some(1.1);
            }
            i = tag.close_index + 2;
        } else if c1 == Some('!') && s.get(i + 2) == Some(&'-') && s.get(i + 3) == Some(&'-') {
            let end = find_closing_index(&s, "-->", i + 4, "Comment is not closed.")?;
            i = end + 1;
        } else if c1 == Some('!') && s.get(i + 2) == Some(&'D') {
            if doctype_found {
                return fail("Multiple DOCTYPE declarations found.");
            }
            doctype_found = true;
            i = skip_doctype(&s, i)? + 1;
        } else if c1 == Some('!') && s.get(i + 2) == Some(&'[') {
            let close = find_closing_index(&s, "]]>", i, "CDATA is not closed.")? - 2;
            // `substring(i + 9, close)` swaps its bounds when the first is larger.
            let (a, b) = if i + 9 <= close { (i + 9, close) } else { (close, (i + 9).min(len)) };
            let body: String = s[a..b].iter().collect();
            p.save_text(&mut text);
            p.current().children.push(Child::Text(body));
            i = close + 3;
        } else {
            let Some(tag) = read_tag_exp(&s, i, true, ">") else {
                return Err(XmlError(format!("readTagExp returned undefined at position {i}.")));
            };
            let mut tag_name = sanitize_name(tag.tag_name)?;
            let mut tag_exp = tag.tag_exp;
            let mut attr_exp_present = tag.attr_exp_present;
            if tag_name == "#text" {
                return Err(XmlError(format!("Invalid tag name: {tag_name}")));
            }
            if !text.is_empty() && p.stack.len() > 1 {
                p.save_text(&mut text);
            }
            let mut self_closing = false;
            if tag_exp.ends_with('/') {
                self_closing = true;
                if tag_name.ends_with('/') {
                    tag_name.pop();
                    tag_exp = tag_name.clone();
                } else {
                    tag_exp.pop();
                }
                attr_exp_present = tag_name != tag_exp;
            }
            let attrs = if tag_name != tag_exp && attr_exp_present { build_attributes(&tag_exp, &p.ent)? } else { None };
            if self_closing {
                // The self-closing branch runs `transformTagName` (and so `sanitizeName`) again.
                tag_name = sanitize_name(tag_name)?;
            }
            let mut node = Node::new(tag_name);
            node.attrs = attrs;
            if self_closing {
                p.current().children.push(Child::Elem(node));
            } else {
                if p.stack.len() - 1 > MAX_NESTED_TAGS {
                    return fail("Maximum nested tags exceeded");
                }
                p.stack.push(node);
            }
            text.clear();
            i = tag.close_index + 1;
        }
    }
    // Unclosed elements stay in the tree, as in the JS (each was added to its parent when it opened).
    while p.stack.len() > 1 {
        p.close_current();
    }
    let root = p.stack.pop().unwrap_or_else(|| Node::new(String::new()));
    Ok(Value::Object(compress(root.children)))
}

enum TextAcc {
    Unset,
    EmptyArray,
    Str(String),
}

/// `node2json.compress`: repeated names become arrays, a lone text child becomes the element's value, an empty
/// element is `""`, attributes are merged after the children.
fn compress(children: Vec<Child>) -> Map<String, Value> {
    let mut obj = Map::new();
    let mut text = TextAcc::Unset;
    for child in children {
        let (name, val) = match child {
            Child::Text(t) => {
                text = match text {
                    TextAcc::Unset | TextAcc::EmptyArray => TextAcc::Str(t),
                    TextAcc::Str(s) => TextAcc::Str(s + &t),
                };
                continue;
            }
            Child::Elem(node) => {
                if node.name == ":@" {
                    continue;
                }
                if node.name == "#text" {
                    // Only `<#text/>` gets here: its value is an empty array, which JS string concatenation treats as ''.
                    text = match text {
                        TextAcc::Unset => TextAcc::EmptyArray,
                        TextAcc::EmptyArray => TextAcc::Str(String::new()),
                        s => s,
                    };
                    continue;
                }
                let mut val = compress(node.children);
                let v = if let Some(attrs) = node.attrs {
                    for (k, a) in attrs {
                        val.insert(k, a);
                    }
                    Value::Object(val)
                } else if val.len() == 1 && val.contains_key("#text") {
                    val.remove("#text").unwrap_or(Value::Null)
                } else if val.is_empty() {
                    Value::String(String::new())
                } else {
                    Value::Object(val)
                };
                (node.name, v)
            }
        };
        match obj.get_mut(&name) {
            Some(Value::Array(a)) => a.push(val),
            Some(existing) => {
                let first = existing.take();
                *existing = Value::Array(vec![first, val]);
            }
            None => {
                obj.insert(name, val);
            }
        }
    }
    match text {
        TextAcc::Str(s) if !s.is_empty() => {
            obj.insert("#text".into(), Value::String(s));
        }
        TextAcc::EmptyArray => {
            obj.insert("#text".into(), Value::Array(Vec::new()));
        }
        _ => {}
    }
    obj
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shapes() {
        assert_eq!(parse("<a><b>x</b><b>y</b></a>").ok(), Some(json!({ "a": { "b": ["x", "y"] } })));
        assert_eq!(parse("<a x='1' y>t</a>").ok(), Some(json!({ "a": { "#text": "t", "@_x": "1", "@_y": true } })));
        assert_eq!(parse("<a>p <![CDATA[ <q> ]]> r</a>").ok(), Some(json!({ "a": "p <q> r" })));
        assert_eq!(parse("<a>&amp;&#65;&#x42;&bogus;&#0;</a>").ok(), Some(json!({ "a": "&AB&bogus;" })));
        assert!(parse("<a").is_err());
        // Text after the last tag is dropped, as in the JS (it is saved only when the next tag starts).
        assert_eq!(parse("<a><b>open").ok(), Some(json!({ "a": { "b": "" } })));
    }
}
