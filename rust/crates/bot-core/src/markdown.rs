//! `escapeMarkdown(text, options)` of `@discordjs/formatters` 0.6 (discord.js re-exports it). With the default
//! options code blocks, inline code, bold, italic, underline, strikethrough, spoilers and backslashes are escaped;
//! headings, lists and masked links are not. The regexes are the library's, run with JS semantics.
//!
//! The italic and underline regexes start with lookbehinds that scan backwards over the text; run naively at every
//! position they are quadratic. Each pattern here first asserts the marker character with a lookahead (`(?=\*)`,
//! `(?=_)`, `(?=__)`), which every match of the library's pattern has at its start anyway, so the lookbehinds only run
//! where a marker is, and a step is skipped entirely when the text has no marker. The matches are identical.

use bot_util::jsre::{JsMatch, LazyRe, u16s};
use bot_util::lazy_re;

static INLINE_CODE: LazyRe = lazy_re!(r"(?<=^|[^`])``?(?=[^`]|$)", "g");
static INLINE_CODE_SPLIT: LazyRe = lazy_re!(r"(?<=^|[^`])`(?=[^`]|$)", "g");
static ITALIC_STAR: LazyRe = lazy_re!(r"(?=\*)(?<=^|[^*])(?<!(?<!<)https?:\/\/\S*|<[^\s:]+:\/[^\s>]*)\*([^*]|\*\*|$)", "g");
static ITALIC_UNDERSCORE: LazyRe = lazy_re!(r"(?=_)(?<=^|[^_])(?<!<a?:.+|(?<!<)https?:\/\/\S*|<[^\s:]:\/[^\s>]*)_(?!:\d+>)([^_]|__|$)", "g");
static BOLD: LazyRe = lazy_re!(r"\*\*(\*)?", "g");
static UNDERLINE: LazyRe = lazy_re!(r"(?=__)(?<!<a?:.+|https?:\/\/\S+)__(_)?(?!:\d+>)", "g");
static HEADING: LazyRe = lazy_re!(r"^( {0,2})([*-] )?( *)(#{1,3} )", "gm");
static BULLETED_LIST: LazyRe = lazy_re!(r"^( *)([*-])( +)", "gm");
static NUMBERED_LIST: LazyRe = lazy_re!(r"^( *\d+)\.", "gm");
static MASKED_LINK: LazyRe = lazy_re!(r"\[.+]\(.+\)", "gm");

/// `EscapeMarkdownOptions`; [`Default`] is the library's defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EscapeOptions {
    pub code_block: bool,
    pub inline_code: bool,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub spoiler: bool,
    /// Escape inside code blocks too (false: text between ``` pairs is left as is).
    pub code_block_content: bool,
    /// Escape inside inline code too (false: text between ` pairs is left as is).
    pub inline_code_content: bool,
    /// Escape backslashes.
    pub escape: bool,
    pub heading: bool,
    pub bulleted_list: bool,
    pub numbered_list: bool,
    pub masked_link: bool,
}

impl Default for EscapeOptions {
    fn default() -> Self {
        Self {
            code_block: true,
            inline_code: true,
            bold: true,
            italic: true,
            underline: true,
            strikethrough: true,
            spoiler: true,
            code_block_content: true,
            inline_code_content: true,
            escape: true,
            heading: false,
            bulleted_list: false,
            numbered_list: false,
            masked_link: false,
        }
    }
}

/// `escapeMarkdown(text)` with the default options.
pub fn escape_markdown(text: &str) -> String {
    escape_markdown_with(text, EscapeOptions::default())
}

/// JS `str.split(re)` for a regex without captures.
fn split_by(re: &LazyRe, text: &str) -> Vec<String> {
    let t = u16s(text);
    let mut out = Vec::new();
    let mut last = 0;
    for m in re.find_all(&t) {
        out.push(String::from_utf16_lossy(&t[last..m.start]));
        last = m.end;
    }
    out.push(String::from_utf16_lossy(&t[last..]));
    out
}

/// `escapeMarkdown(text, options)`.
pub fn escape_markdown_with(text: &str, o: EscapeOptions) -> String {
    let d = EscapeOptions::default();
    if !o.code_block_content {
        let parts = split_by_literal(text, "```");
        let n = parts.len();
        let inner = EscapeOptions { code_block: d.code_block, code_block_content: d.code_block_content, ..o };
        let mapped: Vec<String> =
            parts.iter().enumerate().map(|(i, s)| if i % 2 == 1 && i != n - 1 { s.clone() } else { escape_markdown_with(s, inner) }).collect();
        return mapped.join(if o.code_block { "\\`\\`\\`" } else { "```" });
    }
    if !o.inline_code_content {
        let parts = split_by(&INLINE_CODE_SPLIT, text);
        let n = parts.len();
        let inner = EscapeOptions { inline_code: d.inline_code, code_block_content: d.code_block_content, inline_code_content: d.inline_code_content, ..o };
        let mapped: Vec<String> =
            parts.iter().enumerate().map(|(i, s)| if i % 2 == 1 && i != n - 1 { s.clone() } else { escape_markdown_with(s, inner) }).collect();
        return mapped.join(if o.inline_code { "\\`" } else { "`" });
    }
    let mut res = text.to_string();
    if o.escape {
        res = res.replace('\\', "\\\\");
    }
    if o.inline_code && res.contains('`') {
        res = INLINE_CODE.replace_all_with(&res, |m, _| if m.end - m.start == 2 { "\\`\\`".into() } else { "\\`".into() });
    }
    if o.code_block {
        res = res.replace("```", "\\`\\`\\`");
    }
    if o.italic {
        res = escape_italic(&res);
    }
    if o.bold && res.contains("**") {
        let mut idx = 0u32;
        res = BOLD.replace_all_with(&res, |m, t| match m.group(t, 1) {
            Some(g) => {
                idx += 1;
                if idx % 2 == 1 { format!("{g}\\*\\*") } else { format!("\\*\\*{g}") }
            }
            None => "\\*\\*".into(),
        });
    }
    if o.underline && res.contains("__") {
        let mut idx = 0u32;
        res = UNDERLINE.replace_all_with(&res, |m, t| match m.group(t, 1) {
            Some(g) => {
                idx += 1;
                if idx % 2 == 1 { format!("{g}\\_\\_") } else { format!("\\_\\_{g}") }
            }
            None => "\\_\\_".into(),
        });
    }
    if o.strikethrough {
        res = res.replace("~~", "\\~\\~");
    }
    if o.spoiler {
        res = res.replace("||", "\\|\\|");
    }
    if o.heading && res.contains('#') {
        res = HEADING.replace_all_with(&res, |m, t| {
            let g = |i| m.group(t, i).unwrap_or_default();
            format!("{}{}{}\\{}", g(1), g(2), g(3), g(4))
        });
    }
    if o.bulleted_list {
        res = BULLETED_LIST.replace_all_with(&res, |m, t| {
            let g = |i| m.group(t, i).unwrap_or_default();
            format!("{}\\{}{}", g(1), g(2), g(3))
        });
    }
    if o.numbered_list {
        res = NUMBERED_LIST.replace_all_with(&res, |m, t| format!("{}\\.", m.group(t, 1).unwrap_or_default()));
    }
    if o.masked_link && res.contains('[') {
        res = MASKED_LINK.replace_all_with(&res, |m: &JsMatch, t| format!("\\{}", m.text(t)));
    }
    res
}

fn split_by_literal(text: &str, sep: &str) -> Vec<String> {
    text.split(sep).map(str::to_string).collect()
}

/// `escapeItalic`: `*` then `_`, each counting `**` / `__` pairs separately.
fn escape_italic(text: &str) -> String {
    let mut res = text.to_string();
    if res.contains('*') {
        let mut idx = 0u32;
        res = ITALIC_STAR.replace_all_with(&res, |m, t| {
            let g = m.group(t, 1).unwrap_or_default();
            if g == "**" {
                idx += 1;
                if idx % 2 == 1 { format!("\\*{g}") } else { format!("{g}\\*") }
            } else {
                format!("\\*{g}")
            }
        });
    }
    if res.contains('_') {
        let mut idx = 0u32;
        res = ITALIC_UNDERSCORE.replace_all_with(&res, |m, t| {
            let g = m.group(t, 1).unwrap_or_default();
            if g == "__" {
                idx += 1;
                if idx % 2 == 1 { format!("\\_{g}") } else { format!("{g}\\_") }
            } else {
                format!("\\_{g}")
            }
        });
    }
    res
}
