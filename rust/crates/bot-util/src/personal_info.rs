//! Port of `src/utils/personalInfo.js`: finds phone numbers, street addresses, email addresses, US social security
//! numbers and payment card numbers in message text (the automod Personal Info rule), and blanks them out of logs.
//!
//! The patterns are the JS ones verbatim, compiled with regress (ECMAScript semantics, `u` flag), so they match what
//! V8 matches. IP addresses are deliberately not matched: game-server addresses are shared all the time.

use std::collections::HashSet;
use std::sync::LazyLock;

use regress::Regex;
use unicode_normalization::UnicodeNormalization;

/// Kinds in detection order; also the order of the detail text.
pub const PERSONAL_INFO_KINDS: [&str; 5] = ["phone", "address", "email", "ssn", "card"];

/// The deletion-tracker reason automod uses for a Personal Info delete; the message-delete log redacts on it.
pub const PERSONAL_INFO_DELETE_REASON: &str = "automod: personal info";

/// Text scanned at most, in UTF-16 units (a Nitro message is 4000 characters; forwards add their snapshots).
pub const PERSONAL_INFO_INPUT_CAP: usize = 8_000;

/// `PERSONAL_INFO_LABELS[kind]`.
pub fn personal_info_label(kind: &str) -> &str {
    match kind {
        "phone" => "phone number",
        "address" => "street address",
        "email" => "email address",
        "ssn" => "social security number",
        "card" => "card number",
        other => other,
    }
}

fn re(pattern: &str, flags: &str) -> Regex {
    Regex::with_flags(pattern, flags).unwrap_or_else(|e| {
        crate::log_error!(format!("[personal_info] internal regex /{pattern}/{flags} does not compile: {e}"));
        Regex::with_flags("[^\\s\\S]", "").expect("an empty class compiles")
    })
}

// Before a number: not part of a longer number, word, price, id, decimal or version ("phone: 555…" is fine).
const NB: &str = r"(?<![\p{L}\p{N}_+#$€£¥]|\p{N}[.,/:-])";
// After a number: not followed by more digits, a decimal part or a dash/dot run of digits.
const NA: &str = r"(?![\p{L}\p{N}_]|[.,:/-]\p{N})";

static SKIP_RE: LazyLock<Regex> =
    LazyLock::new(|| re(r"<a?:\w{2,32}:\d{17,20}>|<(?:@[!&]?|#)\d{17,20}>|<t:-?\d{1,13}(?::[tTdDfFR])?>|https?:\/\/\S+", "iu"));
static HAS_CANDIDATE_RE: LazyLock<Regex> = LazyLock::new(|| re(r"[\d@]", ""));

static PATTERNS: LazyLock<Vec<(&'static str, Vec<Regex>)>> = LazyLock::new(|| {
    vec![
        (
            "phone",
            vec![
                re(&format!(r"{NB}\+\d{{1,3}}(?:[ .-]?\(\d{{1,5}}\))?[ .-]?\d{{1,14}}(?:[ .-]\d{{1,14}}){{0,5}}{NA}"), "u"),
                re(&format!(r"{NB}(?:1[ .-]?)?(?:\([2-9]\d{{2}}\)[ .-]?|[2-9]\d{{2}}[ .-])\d{{3}}[ .-]\d{{4}}{NA}"), "u"),
                re(&format!(r"{NB}[2-9]\d{{2}}[2-9]\d{{6}}{NA}"), "u"),
                re(&format!(r"{NB}0\d{{1,4}}(?:[ .-]?\d{{2,4}}){{1,4}}{NA}"), "u"),
            ],
        ),
        (
            "address",
            vec![re(
                &format!(
                    r"{NB}\d{{1,6}}[a-z]?(?:[ \t]+(?:[a-z][a-z.'-]*|\d{{1,3}}(?:st|nd|rd|th))){{1,4}}?[ \t]+{}",
                    r"(?:street|st|avenue|ave|av|boulevard|blvd|road|rd|lane|ln|drive|dr|court|ct|place|pl|terrace|ter|way|parkway|pkwy|highway|hwy|circle|cir|crescent|cres)\.?(?![\p{L}\p{N}_])"
                ),
                "iu",
            )],
        ),
        (
            "email",
            vec![re(
                r"(?<![\p{L}\p{N}._%+-])[a-z0-9][a-z0-9._%+-]{0,63}@(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.){1,8}[a-z]{2,24}(?![\p{L}\p{N}_-])",
                "iu",
            )],
        ),
        ("ssn", vec![re(&format!(r"{NB}(?!000|666|9)\d{{3}}-(?!00)\d{{2}}-(?!0000)\d{{4}}{NA}"), "u")]),
        ("card", vec![re(&format!(r"{NB}[2-6]\d{{3}}(?:[ -]?\d){{9,15}}{NA}"), "u")]),
    ]
});

// Street words that are also everyday English need a Title-case street name ("12 Oak Drive", not "2 hard drive").
const PLAIN_WORD_SUFFIXES: [&str; 10] = ["road", "lane", "drive", "dr", "court", "place", "pl", "way", "circle", "terrace"];

// Words that show the number is a quantity, not a house number ("5 miles down the road", "3 cats in the way").
static NOT_A_STREET: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "a", "an", "the", "in", "on", "at", "to", "of", "for", "from", "by", "with", "into", "onto", "off", "out", "over", "under", "down", "up",
        "across", "along", "around", "through", "past", "near", "my", "your", "his", "her", "their", "our", "its", "this", "that", "these",
        "those", "and", "or", "but", "so", "is", "are", "was", "were", "be", "i", "you", "he", "she", "we", "they", "it", "more", "less",
        "times", "minutes", "mins", "min", "hours", "hrs", "days", "weeks", "months", "years", "miles", "mi", "km", "blocks", "people", "guys",
        "than", "then", "if", "not",
    ]
    .into_iter()
    .collect()
});

fn digits_of(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

fn luhn(digits: &str) -> bool {
    let mut sum = 0u32;
    let mut dbl = false;
    for c in digits.bytes().rev() {
        let mut d = u32::from(c - b'0');
        if dbl {
            d *= 2;
            if d > 9 {
                d -= 9;
            }
        }
        sum += d;
        dbl = !dbl;
    }
    sum % 10 == 0
}

fn valid(kind: &str, m: &str) -> bool {
    match kind {
        "phone" => {
            let n = digits_of(m).len();
            if m.starts_with('+') {
                (8..=15).contains(&n)
            } else if m.starts_with('0') {
                (10..=11).contains(&n)
            } else {
                n == 10 || n == 11
            }
        }
        "address" => {
            let words: Vec<&str> = m.trim().split([' ', '\t']).filter(|w| !w.is_empty()).collect();
            if words.len() < 3 {
                return false;
            }
            let suffix = words[words.len() - 1].strip_suffix('.').unwrap_or(words[words.len() - 1]);
            let name = &words[1..words.len() - 1];
            if name.iter().any(|w| NOT_A_STREET.contains(w.to_lowercase().trim_end_matches(['.', '\'', '-']))) {
                return false;
            }
            if PLAIN_WORD_SUFFIXES.contains(&suffix.to_lowercase().as_str()) {
                return name.iter().any(|w| {
                    let mut cs = w.chars();
                    match (cs.next(), cs.next()) {
                        (Some(a), _) if a.is_ascii_digit() => true,
                        (Some(a), Some(b)) => a.is_uppercase() && b.is_lowercase(),
                        _ => false,
                    }
                });
            }
            true
        }
        "card" => {
            let d = digits_of(m);
            if d.len() < 13 || d.len() > 19 {
                return false;
            }
            // Bare digits: only the common 15-16 digit lengths (17-19 bare digits are usually Discord ids).
            if d.len() == m.len() && d.len() != 15 && d.len() != 16 {
                return false;
            }
            luhn(&d)
        }
        _ => true,
    }
}

fn is_invisible(c: char) -> bool {
    matches!(c as u32, 0xAD | 0x34F | 0x61C | 0x180E | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x206F | 0xFEFF)
}

/// NFKC (fullwidth digits become ASCII), invisible characters removed, capped at [`PERSONAL_INFO_INPUT_CAP`] UTF-16
/// units.
pub fn personal_info_text(text: &str) -> String {
    let mut out = String::new();
    let mut units = 0;
    for c in text.nfkc().filter(|c| !is_invisible(*c)) {
        units += c.len_utf16();
        if units > PERSONAL_INFO_INPUT_CAP {
            break;
        }
        out.push(c);
    }
    out
}

struct Hit {
    kind: &'static str,
    start: usize,
    end: usize,
}

fn scan(t: &str) -> Vec<Hit> {
    let skips: Vec<(usize, usize)> = SKIP_RE.find_iter(t).map(|m| (m.start(), m.end())).collect();
    let in_skip = |s: usize, e: usize| skips.iter().any(|&(a, b)| s < b && e > a);
    let mut hits = Vec::new();
    for (kind, list) in PATTERNS.iter() {
        for re in list {
            for m in re.find_iter(t) {
                let (start, end) = (m.start(), m.end());
                if in_skip(start, end) || !valid(kind, &t[start..end]) {
                    continue;
                }
                hits.push(Hit { kind, start, end });
            }
        }
    }
    // Earliest first; at the same start the longer hit wins; anything overlapping a kept hit is dropped.
    hits.sort_by(|a, b| a.start.cmp(&b.start).then((b.end - b.start).cmp(&(a.end - a.start))));
    let mut out: Vec<Hit> = Vec::new();
    for h in hits {
        if out.last().is_none_or(|l| h.start >= l.end) {
            out.push(h);
        }
    }
    out
}

/// The kinds of personal information in `text`, in [`PERSONAL_INFO_KINDS`] order (empty when none).
pub fn find_personal_info(text: &str) -> Vec<&'static str> {
    let t = personal_info_text(text);
    if HAS_CANDIDATE_RE.find(&t).is_none() {
        return Vec::new();
    }
    let found: HashSet<&str> = scan(&t).iter().map(|h| h.kind).collect();
    PERSONAL_INFO_KINDS.iter().copied().filter(|k| found.contains(k)).collect()
}

/// "phone number, email address" for the automod detail line (never the values themselves).
pub fn describe_personal_info(kinds: &[&str]) -> String {
    kinds.iter().map(|k| personal_info_label(k)).collect::<Vec<_>>().join(", ")
}

/// The text with every hit replaced by "[phone number]", "[email address]" and so on, for logs. Text without a hit
/// comes back unchanged (not normalised).
pub fn redact_personal_info(text: &str) -> String {
    let t = personal_info_text(text);
    if HAS_CANDIDATE_RE.find(&t).is_none() {
        return text.to_string();
    }
    let hits = scan(&t);
    if hits.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(t.len());
    let mut last = 0;
    for h in &hits {
        out.push_str(&t[last..h.start]);
        out.push('[');
        out.push_str(personal_info_label(h.kind));
        out.push(']');
        last = h.end;
    }
    out.push_str(&t[last..]);
    out
}
