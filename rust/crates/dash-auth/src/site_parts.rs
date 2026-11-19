//! Port of `src/dashboard/siteParts.js`: shared pieces of the public pages (landing, /docs, /changelog): escaping,
//! the footer contact and support links, each catalog category's flavour, and how a setting's default is shown.

use std::sync::LazyLock;

use bot_util::js::{number_to_string, trim, utf16_len};
use bot_util::jsre::JsRegex;
use serde_json::Value;

use crate::config::{Contact, ContactKind};

/// The public GitHub repository.
pub const REPO_URL: &str = "https://github.com/MasstarVT/discord-bot";
/// Its issue tracker.
pub const ISSUES_URL: &str = "https://github.com/MasstarVT/discord-bot/issues";

/// Escapes text for HTML content and double-quoted attributes.
pub fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// A catalog category's flavour: the class suffix the pages colour it by, and its name on the box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flavour {
    pub cls: &'static str,
    pub name: &'static str,
}

/// `FLAVOURS`, in declaration order.
pub const FLAVOURS: [(&str, Flavour); 4] = [
    ("safety", Flavour { cls: "cherry", name: "Cherry" }),
    ("community", Flavour { cls: "lemon", name: "Lemon" }),
    ("engagement", Flavour { cls: "lime", name: "Lime" }),
    ("tools", Flavour { cls: "grape", name: "Grape" }),
];

/// The flavour of a category id (an unknown one gets the first flavour rather than no colour).
pub fn flavour_of(category_id: &str) -> Flavour {
    FLAVOURS.iter().find(|(id, _)| *id == category_id).map_or(FLAVOURS[0].1, |(_, f)| *f)
}

/// Who may run a command, as the pages say it (`WHO_LABEL`); `None` for an unknown key.
pub fn who_label(who: &str) -> Option<&'static str> {
    match who {
        "everyone" => Some("Everyone"),
        "moderators" => Some("Moderators"),
        "admins" => Some("Admins"),
        "owner" => Some("Bot owner"),
        _ => None,
    }
}

static MEE6_NOTE_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"\s*\(MEE6[^)]*\)\.?$", ""));
static SENTENCE_SPLIT_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"(?<=\.)\s+", ""));
static MEE6_WORD_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"\bMEE6\b", ""));

/// A command's description without the "(MEE6 ...)" parity note the command files carry.
pub fn public_description(text: &str) -> String {
    trim(&MEE6_NOTE_RE.replace_first_with(text, |_, _| String::new())).to_string()
}

/// JS `s.split(re)` for a regex without capture groups that never matches empty.
fn js_split(re: &JsRegex, s: &str) -> Vec<String> {
    let units: Vec<u16> = s.encode_utf16().collect();
    let mut out = Vec::new();
    let mut last = 0;
    for m in re.find_all(&units) {
        out.push(String::from_utf16_lossy(&units[last..m.start]));
        last = m.end;
    }
    out.push(String::from_utf16_lossy(&units[last..]));
    out
}

/// A settings hint without the sentences comparing it to MEE6 ("MEE6 uses 15.").
pub fn public_note(text: &str) -> String {
    let kept: Vec<String> = js_split(&SENTENCE_SPLIT_RE, text).into_iter().filter(|s| !MEE6_WORD_RE.test(s)).collect();
    trim(&kept.join(" ")).to_string()
}

static EMAIL_LINK_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r#"^[^\s@<>"'?#]+@[^\s@<>"'?#]+$"#, ""));
static HTTP_URL_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"^https?:\/\/", "i"));
static HTTPS_URL_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"^https:\/\/", "i"));

/// The footer "Contact" link; only a plausible email address or an http(s) URL becomes one.
pub fn render_contact(contact: Option<&Contact>) -> String {
    let Some(contact) = contact else { return String::new() };
    let value = trim(&contact.value);
    if contact.kind == ContactKind::Email && EMAIL_LINK_RE.test(value) {
        return format!("<a href=\"mailto:{}\">Contact</a>", escape_html(value));
    }
    if contact.kind == ContactKind::Url && HTTP_URL_RE.test(value) {
        return format!("<a href=\"{}\">Contact</a>", escape_html(value));
    }
    String::new()
}

/// The "Support server" link (`DASHBOARD_SUPPORT_URL`), or nothing. `label` defaults to `Support server` in the JS.
pub fn render_support(support_url: Option<&str>, label: &str) -> String {
    let value = support_url.map(trim).unwrap_or("");
    if HTTPS_URL_RE.test(value) {
        format!("<a href=\"{}\">{}</a>", escape_html(value), escape_html(label))
    } else {
        String::new()
    }
}

/// The default label of [`render_support`].
pub const SUPPORT_LABEL: &str = "Support server";

static INLINE_MD_RE: LazyLock<JsRegex> =
    LazyLock::new(|| JsRegex::new(r"`([^`]+)`|\*\*([^*]+)\*\*|\[([^\]]+)\]\((https:\/\/[^)\s]+)\)", "g"));

/// Inline markdown for changelog lines: `code`, **bold** and [text](https://...) links; everything else is escaped.
pub fn inline_markdown(text: &str) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    let slice = |a: usize, b: usize| String::from_utf16_lossy(&units[a..b]);
    let mut out = String::new();
    let mut last = 0;
    for m in INLINE_MD_RE.find_all(&units) {
        out.push_str(&escape_html(&slice(last, m.start)));
        if let Some(code) = m.group(&units, 1) {
            out.push_str(&format!("<code>{}</code>", escape_html(&code)));
        } else if let Some(bold) = m.group(&units, 2) {
            out.push_str(&format!("<strong>{}</strong>", escape_html(&bold)));
        } else {
            let label = m.group(&units, 3).unwrap_or_default();
            let href = m.group(&units, 4).unwrap_or_default();
            out.push_str(&format!("<a href=\"{}\">{}</a>", escape_html(&href), escape_html(&label)));
        }
        last = m.end;
    }
    out.push_str(&escape_html(&slice(last, units.len())));
    out
}

/// Field types whose default reads as one short value.
const SCALAR_TYPES: [&str; 5] = ["int", "float", "boolean", "enum", "text"];

/// JS truthiness of a JSON value.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// JS `String(v)` / `${v}` for a JSON value.
pub(crate) fn js_string(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) if i.unsigned_abs() < (1u64 << 53) => i.to_string(),
            (_, Some(u)) if u < (1u64 << 53) => u.to_string(),
            _ => number_to_string(n.as_f64().unwrap_or(f64::NAN)),
        },
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(|x| if x.is_null() { String::new() } else { js_string(x) }).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// How a settings field's default is shown, or `None` when it has no short value (lists, channels, roles, objects).
/// `field` is a field descriptor as the catalog serialises it (`type`, `default`, `unit`, `label`, `options`).
pub fn format_default(field: &Value) -> Option<String> {
    let ty = field.get("type").and_then(Value::as_str).unwrap_or("");
    if !SCALAR_TYPES.contains(&ty) {
        return None;
    }
    let d = field.get("default");
    if ty == "boolean" {
        return Some(if d.is_some_and(truthy) { "On" } else { "Off" }.to_string());
    }
    if ty == "enum" {
        let d = d?;
        let options = field.get("options")?.as_array()?;
        let found = options.iter().find(|o| o.get("value") == Some(d))?;
        return found.get("label").filter(|l| !l.is_null()).map(js_string);
    }
    let d = match d {
        None | Some(Value::Null) => return None,
        Some(Value::String(s)) if s.is_empty() => return None,
        Some(d) => d,
    };
    let text = js_string(d);
    if ty == "text" {
        return if utf16_len(&text) <= 80 { Some(text) } else { None };
    }
    // a unit the label already names ("Cooldown (seconds)") is not repeated after the value
    let unit = field.get("unit").filter(|u| truthy(u)).map(js_string);
    let label = field.get("label").filter(|l| !l.is_null()).map(js_string).unwrap_or_default().to_lowercase();
    match unit {
        Some(unit) if !label.contains(&unit) => Some(format!("{text} {unit}")),
        _ => Some(text),
    }
}

/// Every settings field of a catalog module (`mod.groups[].fields[]`) that has a short default, in display order.
pub fn scalar_settings(module: &Value) -> Vec<(&Value, String)> {
    let groups = module.get("groups").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    groups
        .iter()
        .flat_map(|g| g.get("fields").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]))
        .filter_map(|f| format_default(f).map(|v| (f, v)))
        .collect()
}
