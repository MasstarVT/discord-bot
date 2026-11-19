//! `new Intl.Collator('en', { sensitivity: 'base', numeric: true }).compare`, the order of guild names (server.js) and
//! custom emoji names (discordApi.js).
//!
//! An approximation of ICU's root collation at the primary level: case and accents are ignored (NFD, combining marks
//! dropped, lower case), runs of ASCII digits compare by their numeric value, and characters fall into ICU's group
//! order: spaces, punctuation and symbols (ASCII in ICU's order, other symbols such as emoji before the currency sign),
//! digits, Latin letters (æ, ß, œ and the like expanded), then other scripts by code point. Two names that differ only in case or accents compare equal, as
//! with `sensitivity: 'base'`.

use std::cmp::Ordering;

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// ICU's root order of the ASCII spaces, punctuation and symbols (all before the digits).
const ASCII_ORDER: &str = "\t\n\x0b\x0c\r _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Element {
    /// A space, punctuation mark or symbol: (rank, code point).
    Symbol(u32, u32),
    /// A run of digits without its leading zeros.
    Number(String),
    /// A Latin letter (lower case, no accent).
    Latin(char),
    /// A letter of another script.
    Other(u32),
}

fn group(e: &Element) -> u8 {
    match e {
        Element::Symbol(..) => 0,
        Element::Number(_) => 1,
        Element::Latin(_) => 2,
        Element::Other(_) => 3,
    }
}

/// Letters ICU expands at the primary level (æ is "ae", ß is "ss", ...) or treats as a variant of a base letter.
fn expand(c: char) -> Option<&'static str> {
    Some(match c {
        'æ' | 'Æ' => "ae",
        'œ' | 'Œ' => "oe",
        'ß' | 'ẞ' => "ss",
        'ø' | 'Ø' => "o",
        'đ' | 'Đ' | 'ð' | 'Ð' => "d",
        'ł' | 'Ł' => "l",
        'þ' | 'Þ' => "th",
        'ı' => "i",
        _ => return None,
    })
}

fn elements(s: &str) -> Vec<Element> {
    let mut chars: Vec<char> = Vec::new();
    for c in s.nfd().filter(|&c| !is_combining_mark(c)) {
        match expand(c) {
            Some(e) => chars.extend(e.chars()),
            None => chars.push(c),
        }
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            let digits: String = chars[start..i].iter().collect();
            let trimmed = digits.trim_start_matches('0');
            out.push(Element::Number(if trimmed.is_empty() { "0".to_string() } else { trimmed.to_string() }));
            continue;
        }
        i += 1;
        if let Some(rank) = ASCII_ORDER.find(c) {
            out.push(Element::Symbol(rank as u32, c as u32));
        } else if c.is_ascii_alphabetic() {
            out.push(Element::Latin(c.to_ascii_lowercase()));
        } else if c.is_alphabetic() {
            let lower = c.to_lowercase().next().unwrap_or(c);
            if lower.is_ascii_alphabetic() {
                out.push(Element::Latin(lower));
            } else {
                out.push(Element::Other(lower as u32));
            }
        } else if c.is_whitespace() {
            out.push(Element::Symbol(0, c as u32));
        } else if c == '\u{200d}' || c == '\u{fe0f}' {
            // Ignorable at the primary level.
        } else {
            // Other symbols (emoji, ...) come after the ASCII ones and before the currency sign.
            out.push(Element::Symbol(ASCII_ORDER.len() as u32 - 2, c as u32));
        }
    }
    out
}

fn compare_element(a: &Element, b: &Element) -> Ordering {
    match (a, b) {
        (Element::Symbol(ra, ca), Element::Symbol(rb, cb)) => ra.cmp(rb).then(ca.cmp(cb)),
        (Element::Number(x), Element::Number(y)) => x.len().cmp(&y.len()).then_with(|| x.cmp(y)),
        (Element::Latin(x), Element::Latin(y)) => x.cmp(y),
        (Element::Other(x), Element::Other(y)) => x.cmp(y),
        _ => group(a).cmp(&group(b)),
    }
}

/// `nameCollator.compare(a, b)`.
pub fn name_compare(a: &str, b: &str) -> Ordering {
    let (x, y) = (elements(a), elements(b));
    for (p, q) in x.iter().zip(y.iter()) {
        let o = compare_element(p, q);
        if o != Ordering::Equal {
            return o;
        }
    }
    x.len().cmp(&y.len())
}
