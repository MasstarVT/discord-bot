//! Port of `src/dashboard/changelog.js`: the public changelog (/changelog) and the landing page's latest entries,
//! from CHANGELOG.md. Only the shape that file uses is read: "## YYYY-MM-DD · Title" headings, "- " items, and the
//! paragraph under the "# " title. Pure: the caller reads the file.

use std::sync::LazyLock;

use bot_util::js::trim;
use bot_util::jsre::JsRegex;
use serde::{Deserialize, Serialize};

use crate::site_parts::{escape_html, inline_markdown};

static ENTRY_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"^##\s+(\d{4}-\d{2}-\d{2})\s+·\s+(.+)$", ""));
static LINE_SPLIT_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"\r?\n", ""));

/// One dated entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangelogEntry {
    pub date: String,
    pub title: String,
    pub items: Vec<String>,
}

/// The parsed changelog: the intro paragraph and the entries, newest first as the file lists them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changelog {
    pub intro: String,
    pub entries: Vec<ChangelogEntry>,
}

/// Parses the contents of CHANGELOG.md.
pub fn parse_changelog(markdown: &str) -> Changelog {
    let units: Vec<u16> = markdown.encode_utf16().collect();
    let mut lines = Vec::new();
    let mut last = 0;
    for m in LINE_SPLIT_RE.find_all(&units) {
        lines.push(String::from_utf16_lossy(&units[last..m.start]));
        last = m.end;
    }
    lines.push(String::from_utf16_lossy(&units[last..]));

    let mut intro: Vec<String> = Vec::new();
    let mut entries: Vec<ChangelogEntry> = Vec::new();
    for raw in &lines {
        let line = trim(raw);
        if let Some((t, m)) = ENTRY_RE.exec(line) {
            entries.push(ChangelogEntry {
                date: m.group(&t, 1).unwrap_or_default(),
                title: trim(&m.group(&t, 2).unwrap_or_default()).to_string(),
                items: Vec::new(),
            });
        } else if let (Some(item), Some(entry)) = (line.strip_prefix("- "), entries.last_mut()) {
            entry.items.push(trim(item).to_string());
        } else if !line.is_empty() && !line.starts_with('#') && entries.is_empty() {
            intro.push(line.to_string());
        }
    }
    Changelog { intro: intro.join(" "), entries }
}

/// The anchor of an entry on /changelog.
pub fn entry_id(date: &str) -> String {
    format!("lot-{date}")
}

/// The landing page's "packed on" list: the newest `count` entries (3 in the JS default), linked to /changelog.
pub fn render_latest_changes(entries: &[ChangelogEntry], count: usize) -> String {
    entries
        .iter()
        .take(count)
        .map(|e| {
            [
                "<li class=\"lot\">".to_string(),
                format!("<time class=\"lot-date\" datetime=\"{0}\">{0}</time>", escape_html(&e.date)),
                format!(
                    "<a class=\"lot-title\" href=\"/changelog#{}\">{}</a>",
                    escape_html(&entry_id(&e.date)),
                    escape_html(&e.title)
                ),
                "</li>".to_string(),
            ]
            .concat()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The default `count` of [`render_latest_changes`].
pub const LATEST_COUNT: usize = 3;

/// The full changelog: one block per dated entry.
pub fn render_changelog_entries(entries: &[ChangelogEntry]) -> String {
    if entries.is_empty() {
        return "<p class=\"leaf-empty\">No entries yet.</p>".to_string();
    }
    entries
        .iter()
        .map(|e| {
            let items: String = e.items.iter().map(|item| format!("<li>{}</li>", inline_markdown(item))).collect();
            [
                format!("<article class=\"lot-entry\" id=\"{}\">", escape_html(&entry_id(&e.date))),
                format!("<h2 class=\"lot-name\">{}</h2>", escape_html(&e.title)),
                format!("<p class=\"lot-stamp\">LOT <time datetime=\"{0}\">{0}</time></p>", escape_html(&e.date)),
                format!("<ul class=\"lot-items\">{items}</ul>"),
                "</article>".to_string(),
            ]
            .concat()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
