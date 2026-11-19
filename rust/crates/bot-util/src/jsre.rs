//! JavaScript regular expressions, run with regress (ECMAScript semantics) over UTF-16 code units, so a pattern
//! copied from the JS source matches exactly what V8 matches: `\s`, `\w`, `\b`, case folding, lookbehind, and
//! quantifiers that count code units in non-unicode mode.

use regress::{Flags, Regex};

/// A compiled JS regex literal. Flags use JS letters (`i`, `m`, `s`, `u`); `g` and `y` are emulated by the callers
/// (`replace_all`, `sticky_at`), so they are stripped here.
#[derive(Debug, Clone)]
pub struct JsRegex {
    re: Regex,
    unicode: bool,
    legacy_fold: bool,
}

// Non-unicode case-insensitive JS regexes compare characters by `Canonicalize`: `toUpperCase` when that gives one
// UTF-16 unit and does not turn a non-ASCII character into an ASCII one, else the character itself. So ſ is not s,
// ı is not i, Kelvin K is not k, and ᾀ (upper-cased 'ἈΙ') matches only itself. regress folds such characters with
// simple case mappings, so every character whose JS class is just itself but which has a case mapping is moved to
// its own private-use stand-in, in the pattern and in the input alike, where no folding reaches it.
static LEGACY_FOLD: std::sync::LazyLock<std::collections::HashMap<u16, u16>> = std::sync::LazyLock::new(|| {
    use std::collections::{HashMap, HashSet};
    let one = |it: &mut dyn Iterator<Item = char>| -> Option<u32> {
        let v: Vec<char> = it.collect();
        if v.len() == 1 && (v[0] as u32) <= 0xffff { Some(v[0] as u32) } else { None }
    };
    let canon = |c: u32| -> u32 {
        let Some(ch) = char::from_u32(c) else { return c };
        match one(&mut ch.to_uppercase()) {
            Some(u) if !(c >= 128 && u < 128) => u,
            _ => c,
        }
    };
    let mut class_size: HashMap<u32, usize> = HashMap::new();
    let mut targets: HashSet<u32> = HashSet::new();
    let mut cased: HashSet<u32> = HashSet::new();
    for c in 0..=0xffffu32 {
        let Some(ch) = char::from_u32(c) else { continue };
        *class_size.entry(canon(c)).or_default() += 1;
        let up: Vec<char> = ch.to_uppercase().collect();
        let low: Vec<char> = ch.to_lowercase().collect();
        if up != [ch] || low != [ch] {
            cased.insert(c);
            for x in up.iter().chain(low.iter()) {
                targets.insert(*x as u32);
            }
        }
    }
    let mut list: Vec<u32> = (0..=0xffffu32)
        .filter(|&c| char::from_u32(c).is_some() && (cased.contains(&c) || targets.contains(&c)) && canon(c) == c && class_size.get(&c) == Some(&1))
        .collect();
    list.sort_unstable();
    list.into_iter().enumerate().map(|(i, c)| (c as u16, 0xe000 + i as u16)).collect()
});

/// True when `text` holds a character that non-unicode case folding treats differently in regress and JS.
pub fn needs_legacy_fold(text: &[u16]) -> bool {
    text.iter().any(|c| LEGACY_FOLD.contains_key(c))
}

/// The input with those characters moved to their private-use stand-ins.
pub fn legacy_fold_units(text: &[u16]) -> Vec<u16> {
    text.iter().map(|c| LEGACY_FOLD.get(c).copied().unwrap_or(*c)).collect()
}

fn fold_target(c: u32) -> Option<u16> {
    u16::try_from(c).ok().and_then(|c| LEGACY_FOLD.get(&c).copied())
}

// One element of a character class: its source text and, for a single character, its code point.
fn class_atoms(chars: &[char], mut i: usize) -> (Vec<(String, Option<u32>)>, usize) {
    let mut atoms = Vec::new();
    while i < chars.len() && chars[i] != ']' {
        let c = chars[i];
        if c != '\\' {
            atoms.push((c.to_string(), Some(c as u32)));
            i += 1;
            continue;
        }
        let n = chars.get(i + 1).copied();
        let hex = |from: usize, len: usize| -> Option<u32> {
            let h: String = chars.get(from..from + len)?.iter().collect();
            if h.len() == len && h.chars().all(|x| x.is_ascii_hexdigit()) { u32::from_str_radix(&h, 16).ok() } else { None }
        };
        match n {
            Some('u') if hex(i + 2, 4).is_some() => {
                atoms.push((chars[i..i + 6].iter().collect(), hex(i + 2, 4)));
                i += 6;
            }
            Some('x') if hex(i + 2, 2).is_some() => {
                atoms.push((chars[i..i + 4].iter().collect(), hex(i + 2, 2)));
                i += 4;
            }
            Some('d' | 'D' | 's' | 'S' | 'w' | 'W') => {
                atoms.push((chars[i..i + 2].iter().collect(), None));
                i += 2;
            }
            Some('b') => {
                atoms.push(("\\b".into(), Some(8)));
                i += 2;
            }
            Some(x) if !x.is_ascii_alphanumeric() => {
                atoms.push((chars[i..i + 2].iter().collect(), Some(x as u32)));
                i += 2;
            }
            Some(_) => {
                // Other escapes (\n, \t, \cX, octal...) never name one of the remapped characters.
                atoms.push((chars[i..i + 2].iter().collect(), None));
                i += 2;
            }
            None => {
                atoms.push(("\\".into(), None));
                i += 1;
            }
        }
    }
    (atoms, i)
}

fn escape_unit(c: u32) -> String {
    format!("\\u{c:04X}")
}

/// The pattern with those characters (literals, `\uXXXX` escapes, and class ranges that span them) moved to their
/// stand-ins.
pub fn legacy_fold_pattern(pattern: &str) -> String {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::with_capacity(pattern.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' {
            if chars.get(i + 1) == Some(&'u') && i + 6 <= chars.len() {
                let hex: String = chars[i + 2..i + 6].iter().collect();
                if let Some(t) = u32::from_str_radix(&hex, 16).ok().and_then(fold_target) {
                    out.push_str(&escape_unit(t as u32));
                    i += 6;
                    continue;
                }
            }
            out.push(c);
            if let Some(n) = chars.get(i + 1) {
                out.push(*n);
            }
            i += 2;
            continue;
        }
        if c == '[' {
            out.push('[');
            i += 1;
            if chars.get(i) == Some(&'^') {
                out.push('^');
                i += 1;
            }
            let (atoms, end) = class_atoms(&chars, i);
            let mut k = 0;
            while k < atoms.len() {
                let is_range = k + 2 < atoms.len() && atoms[k + 1].0 == "-" && atoms[k].1.is_some() && atoms[k + 2].1.is_some();
                if is_range {
                    let (lo, hi) = (atoms[k].1.unwrap_or(0), atoms[k + 2].1.unwrap_or(0));
                    let inside: Vec<(u32, u16)> =
                        LEGACY_FOLD.iter().filter(|(f, _)| lo <= **f as u32 && (**f as u32) <= hi).map(|(f, t)| (*f as u32, *t)).collect();
                    if inside.is_empty() || lo > hi {
                        out.push_str(&format!("{}-{}", atoms[k].0, atoms[k + 2].0));
                    } else {
                        let mut start = lo;
                        let mut cuts: Vec<u32> = inside.iter().map(|(f, _)| *f).collect();
                        cuts.sort_unstable();
                        for f in cuts {
                            if start < f {
                                out.push_str(&format!("{}-{}", escape_unit(start), escape_unit(f - 1)));
                            }
                            start = f + 1;
                        }
                        if start <= hi {
                            out.push_str(&format!("{}-{}", escape_unit(start), escape_unit(hi)));
                        }
                        for (_, t) in inside {
                            out.push_str(&escape_unit(t as u32));
                        }
                    }
                    k += 3;
                    continue;
                }
                match atoms[k].1.and_then(fold_target) {
                    Some(t) => out.push_str(&escape_unit(t as u32)),
                    None => out.push_str(&atoms[k].0),
                }
                k += 1;
            }
            i = end;
            if i < chars.len() {
                out.push(']');
                i += 1;
            }
            continue;
        }
        match fold_target(c as u32) {
            Some(t) => out.push(char::from_u32(t as u32).unwrap_or(c)),
            None => out.push(c),
        }
        i += 1;
    }
    out
}

/// One match: UTF-16 ranges of the whole match and of each capture group.
#[derive(Debug, Clone)]
pub struct JsMatch {
    pub start: usize,
    pub end: usize,
    pub groups: Vec<Option<std::ops::Range<usize>>>,
    names: Vec<(String, Option<std::ops::Range<usize>>)>,
}

impl JsMatch {
    /// Capture group `i` (1-based) as text.
    pub fn group(&self, text: &[u16], i: usize) -> Option<String> {
        self.groups.get(i.checked_sub(1)?)?.as_ref().map(|r| String::from_utf16_lossy(&text[r.clone()]))
    }

    /// A named capture group as text.
    pub fn named(&self, text: &[u16], name: &str) -> Option<String> {
        let r = self.names.iter().find(|(n, _)| n == name)?.1.clone()?;
        Some(String::from_utf16_lossy(&text[r]))
    }

    /// The whole match as text.
    pub fn text(&self, text: &[u16]) -> String {
        String::from_utf16_lossy(&text[self.start..self.end])
    }
}

/// UTF-16 code units of a string.
pub fn u16s(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

impl JsRegex {
    /// Compiles `pattern` with JS `flags`. Panics on a syntax error: only for patterns written in this crate.
    pub fn new(pattern: &str, flags: &str) -> Self {
        Self::try_new(pattern, flags).unwrap_or_else(|e| panic!("bad built-in regex /{pattern}/{flags}: {e}"))
    }

    /// Compiles `pattern` with JS `flags`, or returns regress's error text.
    pub fn try_new(pattern: &str, flags: &str) -> Result<Self, String> {
        let fl: String = flags.chars().filter(|c| !matches!(c, 'g' | 'y' | 'd')).collect();
        let unicode = fl.contains('u') || fl.contains('v');
        let legacy_fold = !unicode && fl.contains('i');
        let compiled = if legacy_fold { legacy_fold_pattern(pattern) } else { pattern.to_string() };
        let re = compile(&compiled, &fl, unicode)?;
        Ok(Self { re, unicode, legacy_fold })
    }

    fn to_match(&self, m: regress::Match) -> JsMatch {
        let names = m.named_groups().map(|(n, r)| (n.to_string(), r)).collect();
        JsMatch { start: m.range.start, end: m.range.end, groups: m.captures, names }
    }

    /// The first match starting at or after UTF-16 offset `pos`.
    pub fn find_at(&self, text: &[u16], pos: usize) -> Option<JsMatch> {
        if pos > text.len() {
            return None;
        }
        let mapped;
        let text = if self.legacy_fold && needs_legacy_fold(text) {
            mapped = legacy_fold_units(text);
            &mapped[..]
        } else {
            text
        };
        let m = if self.unicode { self.re.find_from_utf16(text, pos).next() } else { self.re.find_from_ucs2(text, pos).next() };
        m.map(|m| self.to_match(m))
    }

    /// A match that starts exactly at `pos` (the `y` flag).
    pub fn sticky_at(&self, text: &[u16], pos: usize) -> Option<JsMatch> {
        self.find_at(text, pos).filter(|m| m.start == pos)
    }

    /// `re.test(s)`.
    pub fn test(&self, s: &str) -> bool {
        self.find_at(&u16s(s), 0).is_some()
    }

    /// `re.test` over code units.
    pub fn test_u16(&self, text: &[u16]) -> bool {
        self.find_at(text, 0).is_some()
    }

    /// `re.exec(s)` without `g`: the first match.
    pub fn exec(&self, s: &str) -> Option<(Vec<u16>, JsMatch)> {
        let t = u16s(s);
        let m = self.find_at(&t, 0)?;
        Some((t, m))
    }

    /// Every match, as `String.prototype.matchAll` with `g` finds them.
    pub fn find_all(&self, text: &[u16]) -> Vec<JsMatch> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos <= text.len() {
            let Some(m) = self.find_at(text, pos) else { break };
            pos = if m.end == m.start { self.advance(text, m.end) } else { m.end };
            out.push(m);
        }
        out
    }

    fn advance(&self, text: &[u16], i: usize) -> usize {
        if self.unicode && i + 1 < text.len() && (0xd800..0xdc00).contains(&text[i]) && (0xdc00..0xe000).contains(&text[i + 1]) {
            i + 2
        } else {
            i + 1
        }
    }

    /// `s.replace(re /g/, fn)`: every match replaced by `f(match, text)`.
    pub fn replace_all_with(&self, s: &str, mut f: impl FnMut(&JsMatch, &[u16]) -> String) -> String {
        let text = u16s(s);
        let mut out: Vec<u16> = Vec::with_capacity(text.len());
        let mut last = 0;
        for m in self.find_all(&text) {
            out.extend_from_slice(&text[last..m.start]);
            out.extend(f(&m, &text).encode_utf16());
            last = m.end;
        }
        out.extend_from_slice(&text[last..]);
        String::from_utf16_lossy(&out)
    }

    /// `s.replace(re /g/, literal)` (no `$` patterns).
    pub fn replace_all(&self, s: &str, with: &str) -> String {
        self.replace_all_with(s, |_, _| with.to_string())
    }

    /// `s.replace(re, fn)` without `g`: only the first match.
    pub fn replace_first_with(&self, s: &str, f: impl FnOnce(&JsMatch, &[u16]) -> String) -> String {
        let text = u16s(s);
        let Some(m) = self.find_at(&text, 0) else { return s.to_string() };
        let mut out: Vec<u16> = text[..m.start].to_vec();
        out.extend(f(&m, &text).encode_utf16());
        out.extend_from_slice(&text[m.end..]);
        String::from_utf16_lossy(&out)
    }
}

/// Compiles with regress. Without the `u` flag the pattern is read as UTF-16 code units, as V8 reads it, so an
/// astral character is two units and a quantifier after it repeats only the second one.
pub fn compile(pattern: &str, flags: &str, unicode: bool) -> Result<Regex, String> {
    let r = if unicode {
        Regex::with_flags(pattern, Flags::from(flags))
    } else {
        let units: Vec<u32> = pattern.encode_utf16().map(u32::from).collect();
        Regex::from_unicode(units.into_iter(), Flags::from(flags))
    };
    r.map_err(|e| e.to_string())
}

/// A lazily compiled JS regex literal: `static RE: LazyRe = lazy_re!("pattern", "flags");`.
pub type LazyRe = std::sync::LazyLock<JsRegex>;

/// Declares a lazily compiled JS regex literal.
#[macro_export]
macro_rules! lazy_re {
    ($pattern:expr, $flags:expr) => {
        std::sync::LazyLock::new(|| $crate::jsre::JsRegex::new($pattern, $flags))
    };
}
