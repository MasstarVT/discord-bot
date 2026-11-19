//! Port of `src/services/feedParsers.js`: pure parsing and input normalisation for the social feeds. The XML
//! normaliser (RSS 2.0, Atom, RSS 1.0 / RDF) reads the tree of [`crate::xml`] (the fast-xml-parser port) with the JS
//! readers (`own`, `arr`, `attr`, `text`), so every feed normalises to the same items as in the JS. Also the Bluesky
//! and iTunes JSON normalisers, charset decoding, HTML detection and autodiscovery, and the user-input parsers. No
//! database, cache, Discord or service code.
//!
//! Hostile input: feeds are third-party. Every scan is linear, the DOCTYPE is removed before parsing so an entity
//! declaration can never expand, and the readers walk own keys only.

use bot_util::jsre::{JsRegex, LazyRe, u16s};
use bot_util::lazy_re;
use bot_util::text::clamp_text;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha1::Digest;
use url::Url;

use crate::js_date;

/// `ITEM_ID_MAX`.
pub const ITEM_ID_MAX: usize = 150;
const MAX_ENTRIES: usize = 200;
const URL_MAX: usize = 2048;
const HTML_SCAN_MAX: usize = 200_000;
const STRIP_INPUT_MAX: usize = 200_000;

/// `FeedParseError`: `code` is `NOT_A_FEED` or `INVALID_XML`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedParseError {
    pub code: String,
    pub message: String,
}

impl FeedParseError {
    pub fn new(code: &str, message: &str) -> Self {
        Self { code: code.into(), message: message.into() }
    }
    fn invalid_xml() -> Self {
        Self::new("INVALID_XML", "Feed XML could not be parsed")
    }
}

impl std::fmt::Display for FeedParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for FeedParseError {}

/// One normalised item. Serialised as the JS object (the shared fetch cache stores it, `publishedAt` as an ISO string).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedItem {
    pub id: String,
    pub title: String,
    pub url: Option<String>,
    pub author: String,
    #[serde(with = "bot_db::serde_js::date_opt")]
    pub published_at: Option<DateTime<Utc>>,
    pub image: Option<String>,
    pub summary: String,
    pub is_repost: bool,
    pub actor_name: String,
    pub actor_handle: String,
}

impl FeedItem {
    /// An item with every optional field empty (tests and the dashboard build items this way).
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            url: None,
            author: String::new(),
            published_at: None,
            image: None,
            summary: String::new(),
            is_repost: false,
            actor_name: String::new(),
            actor_handle: String::new(),
        }
    }
}

/// What a parser returns: `{ kind, title, link, image, items }`.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedFeed {
    /// `rss`, `atom`, `rdf` or `bluesky`.
    pub kind: String,
    pub title: Option<String>,
    pub link: Option<String>,
    pub image: Option<String>,
    pub items: Vec<FeedItem>,
}

// ── JS value helpers ───────────────────────────────────────────────────────────

/// `arr(x)`.
fn arr(x: Option<&Value>) -> Vec<&Value> {
    match x {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a.iter().collect(),
        Some(v) => vec![v],
    }
}

fn is_obj(x: Option<&Value>) -> bool {
    matches!(x, Some(Value::Object(_)) | Some(Value::Array(_)))
}

/// `own(o, k)`: an object's own key (arrays have none of the keys read here).
fn own<'a>(o: Option<&'a Value>, k: &str) -> Option<&'a Value> {
    match o {
        Some(Value::Object(m)) => m.get(k),
        _ => None,
    }
}

/// `attr(o, name)`: a trimmed string attribute, else ''.
fn attr(o: Option<&Value>, name: &str) -> String {
    match own(o, &format!("@_{name}")) {
        Some(Value::String(s)) => bot_util::js::trim(s).to_string(),
        _ => String::new(),
    }
}

/// `text(x)`: a string or number, or the `#text` of an object; trimmed.
fn text(x: Option<&Value>) -> String {
    match x {
        Some(Value::String(s)) => bot_util::js::trim(s).to_string(),
        Some(Value::Number(n)) => bot_util::js::trim(&bot_util::js::number_to_string(n.as_f64().unwrap_or(0.0))).to_string(),
        _ => match own(x, "#text") {
            Some(Value::String(s)) => bot_util::js::trim(s).to_string(),
            Some(Value::Number(n)) => bot_util::js::number_to_string(n.as_f64().unwrap_or(0.0)),
            _ => String::new(),
        },
    }
}

/// `String(v)` for a JSON value.
pub(crate) fn js_string(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => bot_util::js::number_to_string(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(|x| if x.is_null() { String::new() } else { js_string(x) }).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// JS truthiness of a JSON value.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

static HTTP_START: LazyRe = lazy_re!(r"^https?:\/\/", "i");

/// `new URL(s).href`, or `None` when the URL does not parse.
pub(crate) fn url_href(s: &str) -> Option<String> {
    Url::parse(s).ok().map(|u| u.to_string())
}

/// `httpOrNull(s)`: http(s) only, normalised through URL, at most 2048 characters.
pub fn http_or_null(s: &str) -> Option<String> {
    let v = bot_util::js::trim(s);
    if !HTTP_START.test(v) || bot_util::js::utf16_len(v) > URL_MAX {
        return None;
    }
    url_href(v).filter(|h| bot_util::js::utf16_len(h) <= URL_MAX)
}

fn http_of(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => http_or_null(s),
        _ => None,
    }
}

/// `dateOrNull(s)`: `Date.parse(String(s))` for a string or number.
fn date_or_null(v: Option<&Value>) -> Option<DateTime<Utc>> {
    match v {
        Some(Value::String(s)) => js_date::parse_date(s),
        Some(Value::Number(_)) => js_date::parse_date(&js_string(v.unwrap_or(&Value::Null))),
        _ => None,
    }
}

fn date_of_text(s: &str) -> Option<DateTime<Utc>> {
    js_date::parse_date(s)
}

/// `pickLink(linkNode)`: RSS `<link>text</link>` first, then an Atom alternate link, then any http link.
fn pick_link(link: Option<&Value>) -> Option<String> {
    let nodes = arr(link);
    for n in &nodes {
        let textual = matches!(n, Value::String(_) | Value::Number(_)) || (is_obj(Some(n)) && own(Some(n), "#text").is_some());
        if textual && let Some(u) = http_or_null(&text(Some(n))) {
            return Some(u);
        }
    }
    for n in &nodes {
        if !is_obj(Some(n)) {
            continue;
        }
        let rel = bot_util::js::lower(&attr(Some(n), "rel"));
        if (rel.is_empty() || rel == "alternate")
            && let Some(u) = http_or_null(&attr(Some(n), "href"))
        {
            return Some(u);
        }
    }
    nodes.iter().find_map(|n| http_or_null(&attr(Some(n), "href")))
}

/// `itemKey(id)`: ids over [`ITEM_ID_MAX`] characters become `sha1:<hex>`.
pub fn item_key(id: &str) -> String {
    if bot_util::js::utf16_len(id) <= ITEM_ID_MAX {
        return id.to_string();
    }
    format!("sha1:{}", hex::encode(sha1::Sha1::digest(id.as_bytes())))
}

// ── HTML to text ───────────────────────────────────────────────────────────────

static SCRIPT_OPEN: LazyRe = lazy_re!(r"<script(?=[\s>/])", "gi");
static SCRIPT_CLOSE: LazyRe = lazy_re!(r"</script[^<>]*>", "gi");
static STYLE_OPEN: LazyRe = lazy_re!(r"<style(?=[\s>/])", "gi");
static STYLE_CLOSE: LazyRe = lazy_re!(r"</style[^<>]*>", "gi");
static BR: LazyRe = lazy_re!(r"<br\s*\/?>", "gi");
static BLOCK_END: LazyRe = lazy_re!(r"<\/(?:p|div|li)>", "gi");
static TAG: LazyRe = lazy_re!(r"<[^<>]+>", "g");
static ENTITY: LazyRe = lazy_re!(r"&(?:#(\d{1,7})|#[xX]([0-9a-fA-F]{1,6})|(amp|lt|gt|quot|apos|nbsp));", "g");
static SPACES: LazyRe = lazy_re!(r"[ \t]+", "g");
static SPACED_NL: LazyRe = lazy_re!(r"[ \t]*\n[ \t]*", "g");
static MANY_NL: LazyRe = lazy_re!(r"\n{3,}", "g");

/// `removeBlocks(s, tag)`: removes `<tag …>…</tag>` blocks in one linear pass; an unclosed block drops the rest.
fn remove_blocks(s: &str, open: &JsRegex, close: &JsRegex) -> String {
    let t = u16s(s);
    let mut out: Vec<u16> = Vec::with_capacity(t.len());
    let mut pos = 0;
    loop {
        let Some(o) = open.find_at(&t, pos) else {
            out.extend_from_slice(&t[pos..]);
            break;
        };
        out.extend_from_slice(&t[pos..o.start]);
        let Some(c) = close.find_at(&t, o.end) else { break };
        pos = c.end;
    }
    String::from_utf16_lossy(&out)
}

/// `decodeEntities(s)`: the common named and numeric entities in one pass (so `&amp;#65;` stays `&#65;`).
fn decode_entities(s: &str) -> String {
    ENTITY.replace_all_with(s, |m, t| {
        if let Some(name) = m.group(t, 3) {
            return match name.as_str() {
                "amp" => "&",
                "lt" => "<",
                "gt" => ">",
                "quot" => "\"",
                "apos" => "'",
                _ => " ",
            }
            .to_string();
        }
        let cp = match (m.group(t, 1), m.group(t, 2)) {
            (Some(d), _) => d.parse::<u32>().ok(),
            (None, Some(h)) => u32::from_str_radix(&h, 16).ok(),
            _ => None,
        };
        match cp {
            Some(cp) if cp > 0 && cp <= 0x10ffff && !(0xd800..=0xdfff).contains(&cp) => char::from_u32(cp).map(String::from).unwrap_or_default(),
            _ => String::new(),
        }
    })
}

/// `stripHtml(input)`: scripts and styles go, block ends become newlines, tags go, entities are decoded once,
/// whitespace is collapsed. Never fails; the input is cut to 200 000 characters first.
pub fn strip_html(input: &str) -> String {
    let s = bot_util::js::utf16_prefix(input, STRIP_INPUT_MAX);
    let s = remove_blocks(&remove_blocks(s, &SCRIPT_OPEN, &SCRIPT_CLOSE), &STYLE_OPEN, &STYLE_CLOSE);
    let s = BLOCK_END.replace_all(&BR.replace_all(&s, "\n"), "\n");
    let s = TAG.replace_all(&s, "");
    let s = decode_entities(&s);
    let s = MANY_NL.replace_all(&SPACED_NL.replace_all(&SPACES.replace_all(&s, " "), "\n"), "\n\n");
    bot_util::js::trim(&s).to_string()
}

static CONTROL_OR_SPACE: LazyRe = lazy_re!(r"[\p{Cc}\s]+", "gu");

/// `oneLine(input)`: control characters and whitespace runs become single spaces.
pub fn one_line(input: &str) -> String {
    bot_util::js::trim(&CONTROL_OR_SPACE.replace_all(input, " ")).to_string()
}

// ── Decoding, HTML detection and autodiscovery ─────────────────────────────────

static CHARSET: LazyRe = lazy_re!(r#"charset=["']?([\w.:-]+)"#, "i");
static XML_ENCODING: LazyRe = lazy_re!(r#"^<\?xml[^>]*encoding=["']([\w.:-]+)["']"#, "i");

/// `decodeXmlBuffer(buffer, contentType)`: the charset of the Content-Type, else of the XML declaration, else UTF-8
/// (`TextDecoder`, which never switches encoding on a BOM); a leading BOM is removed.
pub fn decode_xml_buffer(buffer: &[u8], content_type: &str) -> String {
    let mut label = CHARSET.exec(content_type).and_then(|(t, m)| m.group(&t, 1));
    if label.is_none() {
        let head: String = buffer.iter().take(200).map(|b| char::from(*b)).collect();
        label = XML_ENCODING.exec(&head).and_then(|(t, m)| m.group(&t, 1));
    }
    let enc = label
        .and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes()))
        .filter(|e| *e != encoding_rs::REPLACEMENT)
        .unwrap_or(encoding_rs::UTF_8);
    let (out, _) = enc.decode_with_bom_removal(buffer);
    let out = out.into_owned();
    match out.strip_prefix('\u{feff}') {
        Some(rest) => rest.to_string(),
        None => out,
    }
}

/// `skipLeadingComments(s, from)`: the index (UTF-16) after any leading `<!-- … -->` comments and the whitespace
/// behind them.
fn skip_leading_comments(s: &[u16], from: usize) -> usize {
    let open = u16s("<!--");
    let close = u16s("-->");
    let mut i = from;
    while s.get(i..).is_some_and(|r| r.starts_with(&open)) {
        let Some(end) = (i + 4..s.len().saturating_sub(2)).find(|&k| s[k..].starts_with(&close)) else { break };
        i = end + 3;
        while i < s.len() && char::from_u32(u32::from(s[i])).is_some_and(bot_util::js::is_js_space) {
            i += 1;
        }
    }
    i
}

static HTML_TYPE: LazyRe = lazy_re!(r"text\/html|application\/xhtml", "i");
static XML_DECL: LazyRe = lazy_re!(r"^<\?xml[^>]*\?>\s*", "i");
static FEED_ROOT: LazyRe = lazy_re!(r"^<(rss|feed|rdf:RDF)[\s>]", "i");
static HTML_DOCTYPE: LazyRe = lazy_re!(r"^<!doctype html", "i");
static HTML_ROOT: LazyRe = lazy_re!(r"^<html[\s>]", "i");

/// `looksLikeHtml(textBody, contentType)`: true for an HTML page (content type, or a leading doctype / html tag).
pub fn looks_like_html(body: &str, content_type: &str) -> bool {
    let head_raw = bot_util::js::utf16_prefix(body, 1000);
    let head = u16s(bot_util::js::trim(head_raw));
    if HTML_TYPE.test(content_type) {
        let decl = XML_DECL.find_at(&head, 0).filter(|m| m.start == 0).map(|m| m.end).unwrap_or(0);
        let rest = &head[skip_leading_comments(&head, decl)..];
        return !FEED_ROOT.test_u16(rest);
    }
    let rest = &head[skip_leading_comments(&head, 0)..];
    HTML_DOCTYPE.test_u16(rest) || HTML_ROOT.test_u16(rest)
}

static LINK_TAG: LazyRe = lazy_re!(r"<link\b[^<>]*>", "gi");
static ATTR_RE: LazyRe = lazy_re!(r#"([^\s"'<>/=]{1,100})\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'<>]+))"#, "g");
static ALTERNATE: LazyRe = lazy_re!(r"(^|\s)alternate(\s|$)", "i");
const LINK_TAG_MAX: usize = 4096;

/// `discoverFeedLinks(html, baseUrl)`: the RSS and Atom links an HTML page advertises, at most 5, in order.
pub fn discover_feed_links(html: &str, base_url: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let head = u16s(bot_util::js::utf16_prefix(html, HTML_SCAN_MAX));
    // `new URL(href, base)` throws for every href when the base itself does not parse.
    let Ok(base) = Url::parse(base_url) else { return found };
    for tag in LINK_TAG.find_all(&head) {
        if tag.end - tag.start > LINK_TAG_MAX {
            continue;
        }
        let tag_text = &head[tag.start..tag.end];
        let mut attrs: Vec<(String, String)> = Vec::new();
        for m in ATTR_RE.find_all(tag_text) {
            let name = bot_util::js::lower(&m.group(tag_text, 1).unwrap_or_default());
            if attrs.iter().any(|(n, _)| *n == name) {
                continue;
            }
            let v = m.group(tag_text, 2).or_else(|| m.group(tag_text, 3)).or_else(|| m.group(tag_text, 4)).unwrap_or_default();
            attrs.push((name, v));
        }
        let get = |k: &str| attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        if !ALTERNATE.test(&get("rel").unwrap_or_default()) {
            continue;
        }
        let kind = bot_util::js::lower(bot_util::js::trim(&get("type").unwrap_or_default()));
        if kind != "application/rss+xml" && kind != "application/atom+xml" {
            continue;
        }
        let Some(href) = get("href").filter(|h| !h.is_empty()) else { continue };
        let target = decode_entities(bot_util::js::trim(&href));
        if let Ok(u) = base.join(&target)
            && (u.scheme() == "http" || u.scheme() == "https")
        {
            let href = u.to_string();
            if !found.contains(&href) {
                found.push(href);
            }
        }
        if found.len() >= 5 {
            break;
        }
    }
    found
}

static DOCTYPE: LazyRe = lazy_re!(r"<!DOCTYPE", "i");

/// `stripDoctype(xml)`: removes every DOCTYPE (with its internal subset), in at most 4 rounds. An unterminated
/// DOCTYPE drops the rest of the document.
pub fn strip_doctype(xml: &str) -> String {
    let mut s = u16s(xml);
    for _ in 0..4 {
        let Some(m) = DOCTYPE.find_at(&s, 0) else { break };
        let start = m.start;
        let mut depth = 0i64;
        let mut quote: Option<u16> = None;
        let mut end: Option<usize> = None;
        let mut i = start + 9;
        while i < s.len() {
            let c = s[i];
            if let Some(q) = quote {
                if c == q {
                    quote = None;
                }
                i += 1;
                continue;
            }
            if c == u16::from(b'"') || c == u16::from(b'\'') {
                if depth > 0 {
                    quote = Some(c);
                }
            } else if c == u16::from(b'[') {
                depth += 1;
            } else if c == u16::from(b']') {
                depth = (depth - 1).max(0);
            } else if c == u16::from(b'>') && depth == 0 {
                end = Some(i);
                break;
            }
            i += 1;
        }
        s = match end {
            None => s[..start].to_vec(),
            Some(e) => [&s[..start], &s[e + 1..]].concat(),
        };
    }
    String::from_utf16_lossy(&s)
}

// ── XML normaliser ─────────────────────────────────────────────────────────────

fn first_http(candidates: &[String]) -> Option<String> {
    candidates.iter().find_map(|c| http_or_null(c))
}

static LEADING_SLASH: LazyRe = lazy_re!(r"^\/", "");

fn entry_author(e: Option<&Value>) -> String {
    let creator = own(e, "creator");
    let mut a = text(arr(creator).first().copied());
    if a.is_empty() {
        for node in arr(own(e, "author")) {
            a = text(own(Some(node), "name"));
            if a.is_empty() {
                a = text(Some(node));
            }
            if !a.is_empty() {
                break;
            }
        }
    }
    clamp_text(&strip_html(&LEADING_SLASH.replace_all(&a, "")), 256)
}

fn entry_image(e: Option<&Value>) -> Option<String> {
    let enclosures = arr(own(e, "enclosure"));
    let group = arr(own(e, "group")).first().copied();
    let mut candidates = vec![attr(arr(own(e, "thumbnail")).first().copied(), "url"), attr(arr(own(group, "thumbnail")).first().copied(), "url")];
    for c in arr(own(e, "content")) {
        if !is_obj(Some(c)) {
            continue;
        }
        if attr(Some(c), "medium") == "image" || bot_util::js::lower(&attr(Some(c), "type")).starts_with("image/") {
            candidates.push(attr(Some(c), "url"));
            break;
        }
    }
    if let Some(en) = enclosures.iter().find(|en| bot_util::js::lower(&attr(Some(en), "type")).starts_with("image/")) {
        candidates.push(attr(Some(en), "url"));
    }
    candidates.push(attr(arr(own(e, "image")).first().copied(), "href"));
    first_http(&candidates)
}

static SUBMITTED_BY: LazyRe = lazy_re!(r"\s*submitted by\s+\/?u\/\S+[\s\S]*$", "i");

fn entry_summary(e: Option<&Value>) -> String {
    let group = arr(own(e, "group")).first().copied();
    let sources = [
        text(own(e, "encoded")),
        text(own(e, "description")),
        text(own(e, "summary")),
        text(arr(own(e, "content")).first().copied()),
        text(own(group, "description")),
    ];
    for raw in sources {
        if raw.is_empty() {
            continue;
        }
        let stripped = strip_html(&raw);
        let cut = bot_util::js::utf16_prefix(&stripped, 5000);
        let plain = SUBMITTED_BY.replace_all_with(cut, |_, _| String::new());
        let plain = bot_util::js::trim(&plain);
        if !plain.is_empty() {
            return clamp_text(plain, 500);
        }
    }
    String::new()
}

fn normalize_entry(e: &Value) -> Option<FeedItem> {
    if !is_obj(Some(e)) {
        return None;
    }
    let e = Some(e);
    let title_raw = clamp_text(&strip_html(&text(arr(own(e, "title")).first().copied())), 256);
    let title = if title_raw.is_empty() { "(untitled)".to_string() } else { title_raw };
    let guid = arr(own(e, "guid")).first().copied();
    let enclosure = arr(own(e, "enclosure")).first().copied();
    let link = pick_link(own(e, "link"));
    let mut published = text(own(e, "pubDate"));
    if published.is_empty() {
        published = text(own(e, "published"));
    }
    if published.is_empty() {
        published = text(own(e, "updated"));
    }
    let mut raw_id = text(guid);
    if raw_id.is_empty() {
        raw_id = text(arr(own(e, "id")).first().copied());
    }
    if raw_id.is_empty() {
        raw_id = attr(e, "about");
    }
    if raw_id.is_empty() {
        raw_id = link.clone().unwrap_or_default();
    }
    if raw_id.is_empty() {
        raw_id = attr(enclosure, "url");
    }
    if raw_id.is_empty() && (title != "(untitled)" || !published.is_empty()) {
        raw_id = format!("{title}|{published}");
    }
    if raw_id.is_empty() {
        return None;
    }
    let guid_url = if bot_util::js::lower(&attr(guid, "isPermaLink")) != "false" { http_or_null(&text(guid)) } else { None };
    let mut published_at = None;
    for c in [own(e, "pubDate"), own(e, "date"), own(e, "published"), own(e, "updated")] {
        published_at = date_of_text(&text(arr(c).first().copied()));
        if published_at.is_some() {
            break;
        }
    }
    Some(FeedItem {
        id: item_key(&raw_id),
        title,
        url: link.or(guid_url).or_else(|| http_or_null(&attr(enclosure, "url"))),
        author: entry_author(e),
        published_at,
        image: entry_image(e),
        summary: entry_summary(e),
        is_repost: false,
        actor_name: String::new(),
        actor_handle: String::new(),
    })
}

static ENTITY_DECL: LazyRe = lazy_re!(r"<!ENTITY", "i");

/// `parseXmlFeed(xml)`: an RSS 2.0, Atom or RSS 1.0 (RDF) document; `INVALID_XML` or `NOT_A_FEED` otherwise.
pub fn parse_xml_feed(xml: &str) -> Result<ParsedFeed, FeedParseError> {
    let source = strip_doctype(xml);
    // Defence in depth: an entity declaration outside a DOCTYPE is never legitimate.
    if ENTITY_DECL.test(&source) {
        return Err(FeedParseError::invalid_xml());
    }
    let doc = crate::xml::parse(&source).map_err(|_| FeedParseError::invalid_xml())?;
    let d = Some(&doc);
    let (kind, channel, raw_items) = if let Some(rss) = own(d, "rss") {
        let ch = arr(own(Some(rss), "channel")).first().copied();
        ("rss", ch, arr(own(ch, "item")))
    } else if let Some(feed) = own(d, "feed") {
        ("atom", Some(feed), arr(own(Some(feed), "entry")))
    } else if let Some(rdf) = own(d, "RDF") {
        ("rdf", arr(own(Some(rdf), "channel")).first().copied(), arr(own(Some(rdf), "item")))
    } else {
        return Err(FeedParseError::new("NOT_A_FEED", "Not an RSS or Atom feed"));
    };
    if !is_obj(channel) {
        return Err(FeedParseError::new("NOT_A_FEED", "Not an RSS or Atom feed"));
    }
    let mut images = Vec::new();
    for im in arr(own(channel, "image")) {
        images.push(attr(Some(im), "href"));
        images.push(text(own(Some(im), "url")));
    }
    images.push(text(own(channel, "logo")));
    images.push(text(own(channel, "icon")));
    let title = clamp_text(&strip_html(&text(arr(own(channel, "title")).first().copied())), 256);
    Ok(ParsedFeed {
        kind: kind.into(),
        title: if title.is_empty() { None } else { Some(title) },
        link: pick_link(own(channel, "link")),
        image: first_http(&images),
        items: raw_items.into_iter().take(MAX_ENTRIES).filter_map(normalize_entry).collect(),
    })
}

// ── Bluesky and iTunes normalisers ─────────────────────────────────────────────

fn good_handle(h: Option<&Value>) -> Option<String> {
    match h {
        Some(Value::String(s)) if !s.is_empty() && s != "handle.invalid" => Some(s.clone()),
        _ => None,
    }
}

fn str_at<'a>(v: Option<&'a Value>, path: &[&str]) -> Option<&'a Value> {
    let mut cur = v;
    for k in path {
        cur = match cur {
            Some(Value::Object(m)) => m.get(*k),
            Some(Value::Array(a)) => k.parse::<usize>().ok().and_then(|i| a.get(i)),
            _ => None,
        };
    }
    cur
}

fn http_at(v: Option<&Value>, path: &[&str]) -> Option<String> {
    http_of(str_at(v, path))
}

fn bsky_image(embed: Option<&Value>) -> Option<String> {
    if !is_obj(embed) {
        return None;
    }
    match str_at(embed, &["$type"]).and_then(Value::as_str) {
        Some("app.bsky.embed.images#view") => http_at(embed, &["images", "0", "fullsize"]).or_else(|| http_at(embed, &["images", "0", "thumb"])),
        Some("app.bsky.embed.external#view") => http_at(embed, &["external", "thumb"]),
        Some("app.bsky.embed.recordWithMedia#view") => {
            http_at(embed, &["media", "images", "0", "fullsize"]).or_else(|| http_at(embed, &["media", "images", "0", "thumb"]))
        }
        Some("app.bsky.embed.video#view") => http_at(embed, &["thumbnail"]),
        _ => None,
    }
}

/// `parseBlueskyAuthorFeed(json)`: an `app.bsky.feed.getAuthorFeed` response; pins are dropped, reposts flagged.
pub fn parse_bluesky_author_feed(json: &Value) -> Result<ParsedFeed, FeedParseError> {
    let Some(Value::Array(feed)) = json.get("feed") else {
        return Err(FeedParseError::new("NOT_A_FEED", "Unexpected Bluesky response"));
    };
    let mut items = Vec::new();
    for entry in feed.iter().take(100) {
        let entry = Some(entry);
        let post = str_at(entry, &["post"]);
        let Some(uri) = str_at(post, &["uri"]).and_then(Value::as_str) else { continue };
        let reason = str_at(entry, &["reason"]);
        let reason_type = str_at(reason, &["$type"]).and_then(Value::as_str).unwrap_or("");
        if reason_type == "app.bsky.feed.defs#reasonPin" {
            continue;
        }
        let is_repost = reason_type == "app.bsky.feed.defs#reasonRepost";
        let empty = Value::Object(Default::default());
        let author = str_at(post, &["author"]).filter(|a| is_obj(Some(a))).unwrap_or(&empty);
        let actor: &Value = if is_repost {
            str_at(reason, &["by"]).filter(|a| is_obj(Some(a))).unwrap_or(&empty)
        } else {
            author
        };
        let rkey = uri.rsplit('/').next().unwrap_or("");
        let body = str_at(post, &["record", "text"]).and_then(Value::as_str).unwrap_or("");
        let image = bsky_image(str_at(post, &["embed"]));
        let author_handle = good_handle(author.get("handle"));
        let profile = author_handle.clone().or_else(|| author.get("did").and_then(Value::as_str).map(String::from)).filter(|p| !p.is_empty());
        let when = if is_repost { date_or_null(str_at(reason, &["indexedAt"])) } else { date_or_null(str_at(post, &["indexedAt"])) }
            .or_else(|| date_or_null(str_at(post, &["record", "createdAt"])));
        let indexed = match str_at(reason, &["indexedAt"]) {
            None | Some(Value::Null) => String::new(),
            Some(v) => js_string(v),
        };
        let first_line = body.split('\n').find(|l| !bot_util::js::trim(l).is_empty()).map(|l| bot_util::js::trim(l).to_string());
        let title = first_line.unwrap_or_else(|| if image.is_some() { "(image)".into() } else { "(post)".into() });
        let author_name = if truthy(author.get("displayName")) {
            js_string(author.get("displayName").unwrap_or(&Value::Null))
        } else if let Some(h) = &author_handle {
            format!("@{h}")
        } else {
            String::new()
        };
        let actor_name = if truthy(actor.get("displayName")) {
            js_string(actor.get("displayName").unwrap_or(&Value::Null))
        } else {
            good_handle(actor.get("handle")).unwrap_or_default()
        };
        let actor_handle = good_handle(actor.get("handle")).or_else(|| actor.get("did").and_then(Value::as_str).map(String::from)).unwrap_or_default();
        items.push(FeedItem {
            id: item_key(&if is_repost { format!("repost:{uri}:{indexed}") } else { uri.to_string() }),
            title: clamp_text(&title, 256),
            url: profile.and_then(|p| http_or_null(&format!("https://bsky.app/profile/{p}/post/{rkey}"))),
            author: clamp_text(&author_name, 256),
            published_at: when,
            image,
            summary: clamp_text(body, 500),
            is_repost,
            actor_name: clamp_text(&actor_name, 256),
            actor_handle,
        });
    }
    Ok(ParsedFeed { kind: "bluesky".into(), title: None, link: None, image: None, items })
}

/// One iTunes search hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItunesHit {
    pub name: String,
    pub artist: String,
    pub feed_url: String,
    pub artwork: Option<String>,
    pub url: Option<String>,
}

/// `parseItunesSearch(json)`: the podcasts that expose an http(s) feed URL.
pub fn parse_itunes_search(json: &Value) -> Vec<ItunesHit> {
    let mut out = Vec::new();
    for r in arr(json.get("results")) {
        let Some(feed_url) = r.get("feedUrl").and_then(Value::as_str).filter(|u| HTTP_START.test(u)) else { continue };
        let name = match r.get("collectionName").filter(|v| !v.is_null()).or_else(|| r.get("trackName").filter(|v| !v.is_null())) {
            Some(v) => js_string(v),
            None => String::new(),
        };
        let artist = match r.get("artistName").filter(|v| !v.is_null()) {
            Some(v) => js_string(v),
            None => String::new(),
        };
        out.push(ItunesHit {
            name,
            artist,
            feed_url: feed_url.to_string(),
            artwork: http_of(r.get("artworkUrl600")).or_else(|| http_of(r.get("artworkUrl100"))),
            url: http_of(r.get("collectionViewUrl")),
        });
    }
    out
}

// ── User-input parsers ─────────────────────────────────────────────────────────

static ANGLE_WRAPPED: LazyRe = lazy_re!(r"^<(.*)>$", "");

fn clean_input(raw: &str) -> String {
    let t = bot_util::js::trim(raw);
    let unwrapped = ANGLE_WRAPPED.replace_first_with(t, |m, u| m.group(u, 1).unwrap_or_default());
    bot_util::js::trim(&unwrapped).to_string()
}

static FEED_SCHEME: LazyRe = lazy_re!(r"^feed:(\/\/)?", "i");
static HAS_SCHEME: LazyRe = lazy_re!(r"^[a-z][a-z0-9+.-]*:\/\/", "i");

/// `normalizeFeedUrl(raw)`: `Ok(url)` or `Err(reason)`.
pub fn normalize_feed_url(raw: &str) -> Result<String, String> {
    let mut s = clean_input(raw);
    if s.is_empty() {
        return Err("Give a feed URL.".into());
    }
    s = FEED_SCHEME.replace_first_with(&s, |_, _| String::new());
    if !HAS_SCHEME.test(&s) {
        s = format!("https://{s}");
    }
    let Ok(mut u) = Url::parse(&s) else { return Err("That doesn't look like a URL.".into()) };
    if u.scheme() != "http" && u.scheme() != "https" {
        return Err("Only http and https feed URLs work.".into());
    }
    if !u.username().is_empty() || u.password().is_some_and(|p| !p.is_empty()) {
        return Err("URLs with a username or password are not allowed.".into());
    }
    u.set_fragment(None);
    let href = u.to_string();
    if bot_util::js::utf16_len(&href) > 500 {
        return Err("That URL is too long (max 500 characters).".into());
    }
    Ok(href)
}

/// What [`parse_youtube_channel_input`] refuses: `handle` when an @handle or a /c/ /user/ link was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YouTubeRefusal {
    pub handle: bool,
    pub reason: String,
}

fn is_yt_id(s: &str) -> bool {
    s.len() == 24 && s.starts_with("UC") && s[2..].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

const YT_HANDLE_REASON: &str = "YouTube @handles and /c/ or /user/ links can't be looked up without a YouTube API key, which this bot doesn't use. Use the channel ID instead: it starts with **UC** and is 24 characters long (on YouTube: the channel → About → Share → **Copy channel ID**).";
const YT_INVALID_REASON: &str = "That isn't a YouTube channel ID. Use the 24-character ID starting with **UC**, or a youtube.com/channel/UC… link.";
static YT_HOST: LazyRe = lazy_re!(r"youtube\.com|youtu\.be", "i");

/// `parseYouTubeChannelInput(raw)`: the channel id, or why not.
pub fn parse_youtube_channel_input(raw: &str) -> Result<String, YouTubeRefusal> {
    let s = clean_input(raw);
    let invalid = || YouTubeRefusal { handle: false, reason: YT_INVALID_REASON.into() };
    let handle = || YouTubeRefusal { handle: true, reason: YT_HANDLE_REASON.into() };
    if is_yt_id(&s) {
        return Ok(s);
    }
    if s.starts_with('@') {
        return Err(handle());
    }
    if !YT_HOST.test(&s) {
        return Err(invalid());
    }
    let full = if HAS_SCHEME.test(&s) { s.clone() } else { format!("https://{s}") };
    let Ok(u) = Url::parse(&full) else { return Err(invalid()) };
    let host = bot_util::js::lower(u.host_str().unwrap_or(""));
    if host != "youtube.com" && !host.ends_with(".youtube.com") {
        return Err(invalid());
    }
    let parts: Vec<&str> = u.path().split('/').collect();
    if parts.get(1) == Some(&"channel") && is_yt_id(parts.get(2).copied().unwrap_or("")) {
        return Ok(parts[2].to_string());
    }
    if let Some((_, q)) = u.query_pairs().find(|(k, _)| k == "channel_id")
        && is_yt_id(&q)
    {
        return Ok(q.into_owned());
    }
    let path = u.path();
    if path.starts_with("/@") || path.starts_with("/c/") || path.starts_with("/user/") {
        return Err(handle());
    }
    Err(invalid())
}

const SUB_INVALID: &str = "That isn't a valid subreddit name (2–21 letters, digits or underscores).";
const SUB_PROFILE: &str = "Only subreddits are supported, not user profiles.";
static REDDIT_HOST: LazyRe = lazy_re!(r"reddit\.com", "i");
static USER_SEG: LazyRe = lazy_re!(r"^(u|user)$", "i");
static USER_PATH: LazyRe = lazy_re!(r"^\/?(u|user)\/", "i");
static R_PREFIX: LazyRe = lazy_re!(r"^\/?r\/", "i");
static TRAILING_SLASHES: LazyRe = lazy_re!(r"\/+$", "");

fn is_sub_name(s: &str) -> bool {
    let b = s.as_bytes();
    (2..=21).contains(&b.len()) && b[0].is_ascii_alphanumeric() && b[1..].iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

/// `parseSubredditInput(raw)`: the subreddit name, or why not.
pub fn parse_subreddit_input(raw: &str) -> Result<String, String> {
    let original = clean_input(raw);
    let s;
    if REDDIT_HOST.test(&original) {
        let full = if HAS_SCHEME.test(&original) { original.clone() } else { format!("https://{original}") };
        let Ok(u) = Url::parse(&full) else { return Err(SUB_INVALID.into()) };
        let host = bot_util::js::lower(u.host_str().unwrap_or(""));
        if host != "reddit.com" && !host.ends_with(".reddit.com") {
            return Err(SUB_INVALID.into());
        }
        let segs: Vec<&str> = u.path().split('/').filter(|x| !x.is_empty()).collect();
        if USER_SEG.test(segs.first().copied().unwrap_or("")) {
            return Err(SUB_PROFILE.into());
        }
        let Some(at) = segs.iter().position(|x| bot_util::js::lower(x) == "r") else { return Err(SUB_INVALID.into()) };
        match segs.get(at + 1) {
            Some(n) if !n.is_empty() => s = n.to_string(),
            _ => return Err(SUB_INVALID.into()),
        }
    } else {
        if USER_PATH.test(&original) {
            return Err(SUB_PROFILE.into());
        }
        let a = R_PREFIX.replace_first_with(&original, |_, _| String::new());
        let b = TRAILING_SLASHES.replace_first_with(&a, |_, _| String::new());
        s = b.split('/').next().unwrap_or("").to_string();
    }
    if !is_sub_name(&s) {
        return Err(SUB_INVALID.into());
    }
    Ok(s)
}

static BSKY_DID: LazyRe = lazy_re!(r"^did:(plc|web):[A-Za-z0-9._:%-]{1,200}$", "");
static BSKY_HANDLE: LazyRe = lazy_re!(r"^(?=.{3,253}$)([a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z](?:[a-z0-9-]{0,61}[a-z0-9])?$", "");
static PROFILE_SPLIT: LazyRe = lazy_re!(r"[/?#]", "");

/// `parseBlueskyActorInput(raw)`: a handle or DID, or why not.
pub fn parse_bluesky_actor_input(raw: &str) -> Result<String, String> {
    let mut s = clean_input(raw);
    const MARK: &str = "bsky.app/profile/";
    if let Some(at) = s.find(MARK) {
        let rest = s[at + MARK.len()..].to_string();
        let u = u16s(&rest);
        let end = PROFILE_SPLIT.find_at(&u, 0).map(|m| m.start).unwrap_or(u.len());
        s = String::from_utf16_lossy(&u[..end]);
    }
    if let Some(stripped) = s.strip_prefix('@') {
        s = stripped.to_string();
    }
    if BSKY_DID.test(&s) {
        return Ok(s);
    }
    s = bot_util::js::lower(&s);
    if !s.contains('.') {
        s.push_str(".bsky.social");
    }
    if !BSKY_HANDLE.test(&s) {
        return Err("That isn't a Bluesky handle. Use something like **alice.bsky.social**, a bsky.app profile link, or a did:plc: ID.".into());
    }
    Ok(s)
}

/// The `{ ok, … }` JSON shape of a parser result, as the JS returns it (the dashboard reads these).
pub fn parsed_feed_json(f: &ParsedFeed) -> Value {
    serde_json::json!({ "kind": f.kind, "title": f.title, "link": f.link, "image": f.image, "items": f.items })
}
