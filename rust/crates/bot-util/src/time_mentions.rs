//! Port of `src/utils/timeMentions.js`: finds clock times typed in chat ("fri 5pm", "tomorrow at 18:30", "7pm EST")
//! and reads each with `parse_when`. Strict on purpose (it answers ordinary chat); the JS regexes are used verbatim.

use crate::jsre::{u16s, JsRegex, LazyRe};
use crate::lazy_re;
use crate::time::{parse_when_ms, zone_from_abbreviation, zone_from_offset, ZONE_ABBREVIATIONS};
use std::sync::LazyLock;

const WEEKDAY: &str = "sun(?:day)?|mon(?:day)?|tue(?:s(?:day)?)?|wed(?:s|nesday)?|thu(?:r(?:s(?:day)?)?)?|fri(?:day)?|sat(?:urday)?";
const MONTH: &str = "jan(?:uary)?|feb(?:ruary)?|mar(?:ch)?|apr(?:il)?|may|june?|july?|aug(?:ust)?|sep(?:t(?:ember)?)?|oct(?:ober)?|nov(?:ember)?|dec(?:ember)?";
const ORD: &str = "(?:st|nd|rd|th)?";
const TIME: &str = "(?<h12>\\d{1,2})(?:[:.](?<m12>[0-5]\\d))? ?(?<ap>[ap])\\.?m\\.?(?![a-z0-9])|(?<h24>0\\d|1[3-9]|2[0-3]):(?<m24>[0-5]\\d)(?![\\w:])|(?<word>noon|midnight)(?![a-z])";

fn day_pattern() -> String {
    format!(
        "(?:(?:next|this) )?(?:{WEEKDAY})|today|tonight|tomorrow|tmrw|tmr|(?:{MONTH})\\.? \\d{{1,2}}{ORD}(?:,? \\d{{4}})?|\\d{{1,2}}{ORD} (?:of )?(?:{MONTH})\\.?(?:,? \\d{{4}})?|\\d{{4}}-\\d{{1,2}}-\\d{{1,2}}"
    )
}

fn zone_pattern() -> String {
    // Longest first, so 'cest' is tried before 'cet' (a stable sort, as Array.prototype.sort is).
    let mut abbrs: Vec<&str> = ZONE_ABBREVIATIONS.iter().map(|(k, _)| *k).collect();
    abbrs.extend(["utc", "gmt"]);
    abbrs.sort_by(|a, b| b.len().cmp(&a.len()));
    format!(
        "(?<offset>(?:utc|gmt) ?[+\\-−] ?\\d{{1,2}}(?::?00)?)(?![\\d:])|(?<abbr>{})(?![\\w+\\-−])|(?<badzone>(?:utc|gmt)\\S*)",
        abbrs.join("|")
    )
}

static MENTION_RE: LazyLock<JsRegex> = LazyLock::new(|| {
    let day = day_pattern();
    let zone = zone_pattern();
    JsRegex::new(
        &format!("(?<![\\w:.'/$£€#])(?:(?<dayB>{day}),? )?(?<at>at |@ ?)?(?:{TIME})(?: (?:{zone}))?(?:,? (?:on )?(?<dayA>{day})(?![\\w-]))?"),
        "i",
    )
});
static TRAILING_ZONE_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(&format!(" (?:{})", zone_pattern()), "i"));

const WORDLIKE_ABBREVIATIONS: [&str; 5] = ["art", "eat", "wat", "west", "wet"];

static IGNORE_FENCE: LazyRe = lazy_re!(r"```[\s\S]*?```", "");
static IGNORE_CODE: LazyRe = lazy_re!(r"`[^`]*`", "");
static IGNORE_SPOILER: LazyRe = lazy_re!(r"\|\|[\s\S]*?\|\|", "");
static IGNORE_BLOCKQUOTE: LazyRe = lazy_re!(r"^>>> [\s\S]*", "m");
static IGNORE_QUOTE: LazyRe = lazy_re!(r"^> .*$", "m");
static IGNORE_LINK: LazyRe = lazy_re!(r"\bhttps?:\/\/\S+", "i");
static IGNORE_TOKEN: LazyRe = lazy_re!(r"<[^<>\s]+>", "");
static MAY_RE: LazyRe = lazy_re!(r"\d ?[ap]\.?m|\d:[0-5]\d|noon|midnight", "i");
static STAMP_RE: LazyRe = lazy_re!(r"<t:-?\d", "");
static WS_RE: LazyRe = lazy_re!(r"\s+", "");
static OF_RE: LazyRe = lazy_re!(r" of ", "");

/// Cheap pre-filter for every message: true when the text may hold a time.
pub fn may_mention_time(content: &str) -> bool {
    MAY_RE.test(content)
}

/// A time found in a message, not yet read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeSpan {
    /// What was typed.
    pub text: String,
    /// The day parse_when will read ('fri', 'today', 'oct 20'), if any.
    pub day: Option<String>,
    /// The time parse_when will read ('5pm', '18:30', 'noon').
    pub time: String,
    /// The zone the text names, if any.
    pub zone: Option<String>,
}

fn normalize_day(day: &str) -> String {
    let d = OF_RE.replace_first_with(&day.to_lowercase(), |_, _| " ".into());
    if d == "tonight" { "today".into() } else { d }
}

struct Groups {
    day_b: Option<String>,
    at: Option<String>,
    h12: Option<String>,
    m12: Option<String>,
    ap: Option<String>,
    h24: Option<String>,
    m24: Option<String>,
    word: Option<String>,
    offset: Option<String>,
    abbr: Option<String>,
    badzone: Option<String>,
    day_a: Option<String>,
}

fn time_text(g: &Groups) -> String {
    if let Some(w) = &g.word {
        return w.to_lowercase();
    }
    if let Some(h) = &g.h24 {
        return format!("{h}:{}", g.m24.clone().unwrap_or_default());
    }
    format!(
        "{}{}{}m",
        g.h12.clone().unwrap_or_default(),
        g.m12.as_ref().map(|m| format!(":{m}")).unwrap_or_default(),
        g.ap.clone().unwrap_or_default().to_lowercase()
    )
}

fn named_zone(g: &Groups) -> Option<String> {
    if let Some(off) = &g.offset {
        return zone_from_offset(&off.replace(' ', ""));
    }
    let abbr = g.abbr.as_ref()?;
    if WORDLIKE_ABBREVIATIONS.contains(&abbr.to_lowercase().as_str()) && *abbr != abbr.to_uppercase() {
        return None;
    }
    zone_from_abbreviation(abbr).map(str::to_string)
}

/// The times in a message, not yet read (at most `max` spans). A message that already holds a Discord timestamp, and
/// anything in code, quotes, spoilers, links or mentions, is skipped.
pub fn extract_time_spans(content: &str, max: usize) -> Vec<TimeSpan> {
    if !may_mention_time(content) || STAMP_RE.test(content) {
        return Vec::new();
    }
    let mut text = content.to_string();
    text = IGNORE_FENCE.replace_all(&text, " ");
    text = IGNORE_CODE.replace_all(&text, " ");
    text = IGNORE_SPOILER.replace_all(&text, " ");
    text = IGNORE_BLOCKQUOTE.replace_first_with(&text, |_, _| " ".into());
    text = IGNORE_QUOTE.replace_all(&text, " ");
    text = IGNORE_LINK.replace_all(&text, " ");
    text = IGNORE_TOKEN.replace_all(&text, " ");
    text = WS_RE.replace_all(&text, " ");

    let t = u16s(&text);
    let mut spans = Vec::new();
    for m in MENTION_RE.find_all(&t) {
        let mut g = Groups {
            day_b: m.named(&t, "dayB"),
            at: m.named(&t, "at"),
            h12: m.named(&t, "h12"),
            m12: m.named(&t, "m12"),
            ap: m.named(&t, "ap"),
            h24: m.named(&t, "h24"),
            m24: m.named(&t, "m24"),
            word: m.named(&t, "word"),
            offset: m.named(&t, "offset"),
            abbr: m.named(&t, "abbr"),
            badzone: m.named(&t, "badzone"),
            day_a: m.named(&t, "dayA"),
        };
        let mut typed = m.text(&t);
        if g.day_a.is_some() && g.offset.is_none() && g.abbr.is_none() && g.badzone.is_none()
            && let Some(z) = TRAILING_ZONE_RE.sticky_at(&t, m.end) {
                g.offset = z.named(&t, "offset");
                g.abbr = z.named(&t, "abbr");
                g.badzone = z.named(&t, "badzone");
                typed.push_str(&z.text(&t));
            }
        if g.badzone.is_some() {
            continue;
        }
        let zone = named_zone(&g);
        if let Some(abbr) = &g.abbr
            && zone.is_none() {
                typed = typed.replacen(&format!(" {abbr}"), "", 1);
            }
        let day = g.day_b.clone().or_else(|| g.day_a.clone());
        if g.ap.is_none() && day.is_none() && zone.is_none() && g.at.is_none() {
            continue;
        }
        spans.push(TimeSpan {
            text: crate::js::trim(&typed).to_string(),
            day: day.as_deref().map(normalize_day),
            time: time_text(&g),
            zone,
        });
        if spans.len() >= max {
            break;
        }
    }
    spans
}

/// A time read from a message.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadTime {
    pub text: String,
    /// Epoch milliseconds.
    pub date_ms: f64,
    pub zone: String,
    pub zone_from_input: bool,
}

/// Reads spans as instants. A span that names no zone is read in `zone` (skipped when that is None). A time with no
/// day is today's when it passed less than `past_grace_ms` ago, else the next one. Instants further in the past than
/// that, more than a year ahead, or repeated are dropped. At most `max` results.
pub fn read_time_spans(spans: &[TimeSpan], zone: Option<&str>, now_ms: f64, past_grace_ms: f64, max: usize) -> Vec<ReadTime> {
    let mut out: Vec<ReadTime> = Vec::new();
    let mut seen: Vec<f64> = Vec::new();
    for span in spans {
        let Some(tz) = span.zone.as_deref().or(zone) else { continue };
        let mut parsed = parse_when_ms(&format!("{} {}", span.day.as_deref().unwrap_or("today"), span.time), tz, now_ms);
        if span.day.is_none()
            && let Some(p) = &parsed
                && p.date_ms < now_ms - past_grace_ms {
                    parsed = parse_when_ms(&span.time, tz, now_ms);
                }
        let Some(p) = parsed else { continue };
        let ms = p.date_ms;
        if !ms.is_finite() || ms < now_ms - past_grace_ms || ms > now_ms + 366.0 * 86_400_000.0 || seen.contains(&ms) {
            continue;
        }
        seen.push(ms);
        out.push(ReadTime { text: span.text.clone(), date_ms: ms, zone: tz.to_string(), zone_from_input: span.zone.is_some() });
        if out.len() >= max {
            break;
        }
    }
    out
}
