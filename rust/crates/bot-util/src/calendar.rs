//! Port of `src/utils/calendar.js`: "Add to calendar" links (Google Calendar, Outlook.com) and an iCalendar file
//! (RFC 5545) for one timed event. Times are always written in UTC. Pure.

use crate::js;
use chrono::{DateTime, Utc};

/// Discord refuses a link button whose URL is longer than this.
pub const LINK_URL_MAX: usize = 512;

const ELLIPSIS: &str = "…";
const GOOGLE_URL: &str = "https://calendar.google.com/calendar/render";
const OUTLOOK_URL: &str = "https://outlook.live.com/calendar/0/deeplink/compose";

/// One event.
#[derive(Debug, Clone, PartialEq)]
pub struct CalendarEntry {
    /// Stable across edits, so importing the file again updates the entry.
    pub uid: String,
    pub title: String,
    /// Plain text.
    pub description: Option<String>,
    pub location: Option<String>,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// A link back to where the event was announced.
    pub url: Option<String>,
    /// The iCalendar SEQUENCE; None or a non-finite value is 0.
    pub sequence: Option<f64>,
}

fn iso_utc(d: &DateTime<Utc>) -> String {
    let iso = js::to_iso_string(d.timestamp_millis() as f64).unwrap_or_default();
    // '2026-11-05T19:00:00.000Z' → '2026-11-05T19:00:00Z'
    match iso.len().checked_sub(5) {
        Some(i) if iso.as_bytes().get(i) == Some(&b'.') && iso.ends_with('Z') => format!("{}Z", &iso[..i]),
        _ => iso,
    }
}

fn compact_utc(d: &DateTime<Utc>) -> String {
    iso_utc(d).chars().filter(|c| *c != '-' && *c != ':').collect()
}

/// The longest start of `text` (whole characters) whose URL encoding is at most `budget` characters, ending in an
/// ellipsis when it was cut; '' when not even that fits.
pub fn fit_encoded(text: &str, budget: i64) -> String {
    if js::encoded_len(text) as i64 <= budget {
        return text.to_string();
    }
    let room = budget - js::encoded_len(ELLIPSIS) as i64;
    let mut used = 0i64;
    let mut out = String::new();
    for ch in text.chars() {
        let mut buf = [0u8; 4];
        let n = js::encoded_len(ch.encode_utf8(&mut buf)) as i64;
        if used + n > room {
            break;
        }
        used += n;
        out.push(ch);
    }
    let out = js::trim_end(&out);
    if out.is_empty() { String::new() } else { format!("{out}{ELLIPSIS}") }
}

fn non_empty(s: &Option<String>) -> &str {
    s.as_deref().unwrap_or("")
}

/// The description, then the link back: the description is cut first, the link only goes when it cannot fit.
fn fit_details(entry: &CalendarEntry, budget: i64) -> String {
    let url = non_empty(&entry.url);
    let tail = if url.is_empty() { 0 } else { js::encoded_len(&format!("\n\n{url}")) as i64 };
    if url.is_empty() || tail > budget {
        return fit_encoded(non_empty(&entry.description), budget);
    }
    let description = fit_encoded(non_empty(&entry.description), budget - tail);
    if description.is_empty() { url.to_string() } else { format!("{description}\n\n{url}") }
}

enum Val<'a> {
    Text(&'a str),
    Details,
}

fn link_within(head: String, pairs: &[(&str, Val<'_>)], entry: &CalendarEntry, max: usize) -> String {
    let mut url = head;
    for (key, value) in pairs {
        let room = max as i64 - url.len() as i64 - key.len() as i64 - 2;
        let v = match value {
            Val::Text(t) => fit_encoded(t, room),
            Val::Details => fit_details(entry, room),
        };
        if !v.is_empty() {
            url.push_str(&format!("&{key}={}", js::encode_uri_component(&v)));
        }
    }
    url
}

/// A Google Calendar "new event" link of at most `max` characters.
pub fn google_calendar_url(entry: &CalendarEntry, max: usize) -> String {
    let head = format!("{GOOGLE_URL}?action=TEMPLATE&dates={}/{}", compact_utc(&entry.start), compact_utc(&entry.end));
    link_within(
        head,
        &[("text", Val::Text(&entry.title)), ("location", Val::Text(non_empty(&entry.location))), ("details", Val::Details)],
        entry,
        max,
    )
}

/// An Outlook.com "new event" link of at most `max` characters.
pub fn outlook_calendar_url(entry: &CalendarEntry, max: usize) -> String {
    let head = format!(
        "{OUTLOOK_URL}?path=%2Fcalendar%2Faction%2Fcompose&rru=addevent&startdt={}&enddt={}",
        iso_utc(&entry.start),
        iso_utc(&entry.end)
    );
    link_within(
        head,
        &[("subject", Val::Text(&entry.title)), ("location", Val::Text(non_empty(&entry.location))), ("body", Val::Details)],
        entry,
        max,
    )
}

/// RFC 5545 TEXT escaping: backslash first, then `;`, `,` and line breaks.
pub fn ics_escape(text: &str) -> String {
    let s = text.replace('\\', "\\\\").replace(';', "\\;").replace(',', "\\,");
    s.replace("\r\n", "\\n").replace(['\r', '\n'], "\\n")
}

/// Folds a content line at 75 octets without splitting a UTF-8 character; continuations start with a space.
pub fn ics_fold(line: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut bytes = 0;
    for ch in line.chars() {
        let n = ch.len_utf8();
        if bytes + n > if out.is_empty() { 75 } else { 74 } {
            out.push(std::mem::take(&mut cur));
            bytes = 0;
        }
        cur.push(ch);
        bytes += n;
    }
    out.push(cur);
    out.join("\r\n ")
}

/// The default PRODID of [`build_ics`].
pub const DEFAULT_PROD_ID: &str = "-//discord-bot//Events//EN";

/// One VEVENT in a VCALENDAR, CRLF line endings, lines folded. `now` is the DTSTAMP.
pub fn build_ics(entry: &CalendarEntry, now: DateTime<Utc>, prod_id: &str) -> String {
    let sequence = match entry.sequence {
        Some(s) if s.is_finite() => s.floor().max(0.0),
        _ => 0.0,
    };
    let url = non_empty(&entry.url);
    let description = [js::trim(non_empty(&entry.description)), url].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n");
    let mut lines: Vec<String> = vec![
        "BEGIN:VCALENDAR".into(),
        "VERSION:2.0".into(),
        format!("PRODID:{prod_id}"),
        "CALSCALE:GREGORIAN".into(),
        "METHOD:PUBLISH".into(),
        "BEGIN:VEVENT".into(),
        format!("UID:{}", ics_escape(&entry.uid)),
        format!("DTSTAMP:{}", compact_utc(&now)),
        format!("DTSTART:{}", compact_utc(&entry.start)),
        format!("DTEND:{}", compact_utc(&entry.end)),
        format!("SEQUENCE:{}", js::number_to_string(sequence)),
        format!("SUMMARY:{}", ics_escape(&entry.title)),
    ];
    if !description.is_empty() {
        lines.push(format!("DESCRIPTION:{}", ics_escape(&description)));
    }
    let location = non_empty(&entry.location);
    if !location.is_empty() {
        lines.push(format!("LOCATION:{}", ics_escape(location)));
    }
    if !url.is_empty() {
        lines.push(format!("URL:{}", url.replace(['\r', '\n'], "")));
    }
    lines.extend(["STATUS:CONFIRMED".to_string(), "END:VEVENT".into(), "END:VCALENDAR".into()]);
    format!("{}\r\n", lines.iter().map(|l| ics_fold(l)).collect::<Vec<_>>().join("\r\n"))
}
