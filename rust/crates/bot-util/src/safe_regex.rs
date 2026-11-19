//! Port of `src/utils/safeRegex.js`: user-supplied regular expressions (JavaScript syntax, as stored in the
//! database). Validation refuses the shapes that backtrack catastrophically, exactly as the JS does (V8's syntax rules
//! and error text, then the same shape scan). Matching uses regress (ECMAScript semantics) on a worker thread with a
//! hard deadline; a regex that times out is quarantined for 10 minutes and counts as no match.
//!
//! Difference from the JS (reported): V8 kills a timed-out match, while a Rust thread cannot be killed, so the worker
//! running a timed-out match is abandoned and finishes in the background (see [`runaway_matches`]).

use crate::js;
use crate::logger;
use crate::text::clamp_text;
use crate::v8_regexp;
use regress::Regex;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Longest pattern accepted, in UTF-16 units.
pub const MAX_PATTERN_LENGTH: usize = 200;
/// Input is cut to this many UTF-16 units before matching, by default.
pub const DEFAULT_INPUT_CAP: usize = 2000;

const ALLOWED_FLAGS: &str = "imsu";
const COMPILE_CACHE_MAX: usize = 500;
const WORD_CACHE_MAX: usize = 1000;
const QUARANTINE_MAX: usize = 1000;
const QUARANTINE_TTL: Duration = Duration::from_secs(10 * 60);

// ── Compiled regexes ───────────────────────────────────────────────────────────

/// A compiled JS regex: what a `RegExp` object is to the JS side.
#[derive(Debug)]
pub struct SafeRegex {
    /// `re.source` (V8's escaping: '/' and line terminators escaped, '(?:)' for an empty pattern).
    source: String,
    /// `re.flags` in canonical order.
    flags: String,
    re: Regex,
    unicode: bool,
    /// The `y` flag: a test only matches at index 0.
    sticky: bool,
    /// Non-unicode case-insensitive: the few characters regress folds differently from JS are remapped.
    legacy_fold: bool,
}

impl SafeRegex {
    /// Compiles a pattern without the safety validation (the JS accepts any `RegExp` in `safeRegexTest`). Flags are
    /// JS letters; `g`, `y` and `d` do not change a test and are kept only in `flags()`.
    pub fn new(pattern: &str, flags: &str) -> Result<Self, String> {
        let fl: String = flags.chars().filter(|c| !matches!(c, 'g' | 'y' | 'd')).collect();
        let unicode = fl.contains('u') || fl.contains('v');
        let legacy_fold = !unicode && fl.contains('i');
        let compiled = if legacy_fold { crate::jsre::legacy_fold_pattern(pattern) } else { pattern.to_string() };
        let re = crate::jsre::compile(&compiled, &fl, unicode)?;
        Ok(Self { source: v8_source(pattern), flags: canonical_flags(flags), re, unicode, sticky: flags.contains('y'), legacy_fold })
    }

    /// `re.source`.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// `re.flags`.
    pub fn flags(&self) -> &str {
        &self.flags
    }

    /// The quarantine key, `${source}/${flags}`.
    fn key(&self) -> String {
        format!("{}/{}", self.source, self.flags)
    }

    /// `re.test(input)` over UTF-16 code units, with no deadline.
    pub fn test_units(&self, input: &[u16]) -> bool {
        let mapped;
        let input = if self.legacy_fold && crate::jsre::needs_legacy_fold(input) {
            mapped = crate::jsre::legacy_fold_units(input);
            &mapped[..]
        } else {
            input
        };
        let m = if self.unicode { self.re.find_from_utf16(input, 0).next() } else { self.re.find_from_ucs2(input, 0).next() };
        match m {
            Some(m) => !self.sticky || m.range.start == 0,
            None => false,
        }
    }

    /// `re.test(input)` with no deadline.
    pub fn test_str(&self, input: &str) -> bool {
        self.test_units(&input.encode_utf16().collect::<Vec<_>>())
    }
}

/// V8's `RegExp.prototype.source` escaping of a pattern.
fn v8_source(pattern: &str) -> String {
    if pattern.is_empty() {
        return "(?:)".into();
    }
    let mut out = String::with_capacity(pattern.len());
    let mut in_class = false;
    let mut prev_backslash = false;
    for c in pattern.chars() {
        if prev_backslash {
            match c {
                '\n' => out.push('n'),
                '\r' => out.push('r'),
                '\u{2028}' => out.push_str("u2028"),
                '\u{2029}' => out.push_str("u2029"),
                _ => out.push(c),
            }
            prev_backslash = false;
            continue;
        }
        match c {
            '\\' => {
                out.push('\\');
                prev_backslash = true;
            }
            '/' if !in_class => out.push_str("\\/"),
            '[' => {
                in_class = true;
                out.push(c);
            }
            ']' => {
                in_class = false;
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            _ => out.push(c),
        }
    }
    out
}

/// `re.flags` order: d g i m s u v y.
fn canonical_flags(flags: &str) -> String {
    "dgimsuvy".chars().filter(|c| flags.contains(*c)).collect()
}

fn dedupe_flags(flags: &str) -> String {
    let mut out = String::new();
    for c in flags.chars() {
        if !out.contains(c) {
            out.push(c);
        }
    }
    out
}

// ── Validation ─────────────────────────────────────────────────────────────────

#[derive(PartialEq, Eq)]
enum Shape {
    Backref,
    Nested,
}

#[derive(Clone, Copy, Default)]
struct Frame {
    q: bool,
    alt: bool,
}

static BRACE_Q: LazyLock<crate::jsre::JsRegex> = LazyLock::new(|| crate::jsre::JsRegex::new(r"^\{(\d+)(,(\d*))?\}", ""));

// Walks the pattern once, tracking the group stack (UTF-16 units, as the JS indexes the string).
fn scan_shape(p: &[u16]) -> Option<Shape> {
    let ch = |i: usize| -> u16 { p.get(i).copied().unwrap_or(0) };
    let is = |i: usize, c: u8| ch(i) == c as u16 && i < p.len();
    let index_of = |c: u8, from: usize| -> Option<usize> { (from..p.len()).find(|&j| p[j] == c as u16) };
    let mut stack: Vec<Frame> = Vec::new();
    let mut last_closed: Option<Frame> = None;
    let mut i = 0;
    while i < p.len() {
        let c = p[i];
        if c == b'\\' as u16 {
            let n = ch(i + 1);
            if (i + 1 < p.len() && (b'1' as u16..=b'9' as u16).contains(&n)) || (is(i + 1, b'k') && is(i + 2, b'<')) {
                return Some(Shape::Backref);
            }
            if (is(i + 1, b'p') || is(i + 1, b'P') || is(i + 1, b'u')) && is(i + 2, b'{') {
                i = match index_of(b'}', i + 3) {
                    None => p.len(),
                    Some(close) => close + 1,
                };
            } else {
                i += 2;
            }
            last_closed = None;
            continue;
        }
        if c == b'[' as u16 {
            let mut j = i + 1;
            if is(j, b'^') {
                j += 1;
            }
            while j < p.len() && p[j] != b']' as u16 {
                if p[j] == b'\\' as u16 {
                    j += 1;
                }
                j += 1;
            }
            i = j + 1;
            last_closed = None;
            continue;
        }
        if c == b'(' as u16 {
            stack.push(Frame::default());
            let mut prefix = 1;
            if is(i + 1, b'?') {
                let a = ch(i + 2);
                if i + 2 < p.len() && (a == b':' as u16 || a == b'=' as u16 || a == b'!' as u16) {
                    prefix = 3;
                } else if is(i + 2, b'<') && (is(i + 3, b'=') || is(i + 3, b'!')) {
                    prefix = 4;
                } else if is(i + 2, b'<') {
                    prefix = match index_of(b'>', i + 3) {
                        None => 3,
                        Some(close) => close - i + 1,
                    };
                }
            }
            i += prefix;
            last_closed = None;
            continue;
        }
        if c == b')' as u16 {
            let f = stack.pop();
            i += 1;
            if let (Some(f), Some(top)) = (f, stack.last_mut()) {
                top.q |= f.q;
                top.alt |= f.alt;
            }
            last_closed = f;
            continue;
        }
        if c == b'|' as u16 {
            if let Some(top) = stack.last_mut() {
                top.alt = true;
            }
            i += 1;
            last_closed = None;
            continue;
        }
        let mut q_len = 0;
        let mut large = false;
        if c == b'*' as u16 || c == b'+' as u16 {
            q_len = 1;
            large = true;
        } else if c == b'?' as u16 {
            q_len = 1;
        } else if c == b'{' as u16 {
            let window = &p[i..(i + 30).min(p.len())];
            if let Some(m) = BRACE_Q.find_at(window, 0) {
                q_len = m.end;
                let n1: f64 = m.group(window, 1).and_then(|x| x.parse().ok()).unwrap_or(f64::INFINITY);
                large = match m.groups.get(1).cloned().flatten() {
                    None => n1 > 1.0,
                    Some(_) => match m.group(window, 3).as_deref() {
                        Some("") => true,
                        Some(x) => x.parse::<f64>().unwrap_or(f64::INFINITY) > 1.0,
                        None => true,
                    },
                };
            }
        }
        if q_len > 0 {
            if large && last_closed.is_some_and(|f| f.q || f.alt) {
                return Some(Shape::Nested);
            }
            if large
                && let Some(top) = stack.last_mut() {
                    top.q = true;
                }
            i += q_len;
            if is(i, b'?') {
                i += 1;
            }
            last_closed = None;
            continue;
        }
        i += 1;
        last_closed = None;
    }
    None
}

/// Checks a user-supplied pattern: length, flags (only i, m, s, u), syntax, backreferences and nested repetition.
/// `Err` carries the reason shown to the admin, verbatim from the JS.
pub fn validate_regex(pattern: &str, flags: &str) -> Result<(), String> {
    if pattern.is_empty() {
        return Err("Pattern is empty.".into());
    }
    let n = js::utf16_len(pattern);
    if n > MAX_PATTERN_LENGTH {
        return Err(format!("Pattern is {n} characters (max {MAX_PATTERN_LENGTH})."));
    }
    let fl = dedupe_flags(flags);
    for c in fl.chars() {
        if !ALLOWED_FLAGS.contains(c) {
            return Err(format!("Flag \"{c}\" is not allowed (use i, m, s or u)."));
        }
    }
    let units: Vec<u16> = pattern.encode_utf16().collect();
    if let Some(msg) = v8_regexp::check(&units, fl.contains('u')) {
        let detail = format!("/{pattern}/{}: {msg}", canonical_flags(&fl));
        return Err(format!("Invalid regular expression: {}", clamp_text(&detail, 150)));
    }
    match scan_shape(&units) {
        Some(Shape::Backref) => Err("Backreferences (\\1, \\k<name>) are not allowed.".into()),
        Some(Shape::Nested) => Err(
            "Nested repetition is not allowed (e.g. (a+)+ or (a|aa)*). Rewrite the pattern without repeating a group that already repeats or alternates."
                .into(),
        ),
        None => Ok(()),
    }
}

/// The reason text for a pattern that is not a string (`validateRegex(5)`).
pub const NOT_TEXT_REASON: &str = "Pattern must be text.";

struct Lru<V> {
    map: HashMap<String, V>,
    order: VecDeque<String>,
    max: usize,
}

impl<V: Clone> Lru<V> {
    fn new(max: usize) -> Self {
        Self { map: HashMap::new(), order: VecDeque::new(), max }
    }
    fn get(&self, k: &str) -> Option<V> {
        self.map.get(k).cloned()
    }
    // Insertion-ordered: the oldest entry goes first, as a JS Map's first key.
    fn insert(&mut self, k: String, v: V) {
        if self.map.len() >= self.max
            && let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        self.order.push_back(k.clone());
        self.map.insert(k, v);
    }
}

static COMPILE_CACHE: LazyLock<Mutex<Lru<Arc<SafeRegex>>>> = LazyLock::new(|| Mutex::new(Lru::new(COMPILE_CACHE_MAX)));

/// Validates and compiles a user pattern (default flags 'i'); the result is cached and shared. None when invalid.
pub fn compile_safe_regex(pattern: &str, flags: &str) -> Option<Arc<SafeRegex>> {
    validate_regex(pattern, flags).ok()?;
    let fl = dedupe_flags(flags);
    let key = format!("{pattern}\u{0}{fl}");
    let mut cache = COMPILE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(hit) = cache.get(&key) {
        return Some(hit);
    }
    let re = Arc::new(SafeRegex::new(pattern, &fl).ok()?);
    cache.insert(key, re.clone());
    Some(re)
}

// ── Quarantine ─────────────────────────────────────────────────────────────────

/// Regexes that timed out, skipped until their entry expires (insertion order = age order, at most 1000).
#[derive(Default)]
pub struct Quarantine {
    entries: Mutex<(HashMap<String, Instant>, VecDeque<String>)>,
}

impl Quarantine {
    /// An empty quarantine (tests use their own; the bot uses [`global_quarantine`]).
    pub fn new() -> Self {
        Self::default()
    }

    fn add(&self, re: &SafeRegex, ms: u64) {
        let key = re.key();
        let mut g = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let (map, order) = &mut *g;
        if map.remove(&key).is_some() {
            order.retain(|k| k != &key);
        }
        map.insert(key.clone(), Instant::now() + QUARANTINE_TTL);
        order.push_back(key);
        while map.len() > QUARANTINE_MAX {
            if let Some(old) = order.pop_front() {
                map.remove(&old);
            }
        }
        drop(g);
        logger::warn(&[
            format!("Regex /{}/{} exceeded {ms} ms and is quarantined for 10 minutes", clamp_text(re.source(), 100), re.flags()).into(),
        ]);
    }

    fn release(&self, re: &SafeRegex) {
        let key = re.key();
        let mut g = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let (map, order) = &mut *g;
        if map.remove(&key).is_some() {
            order.retain(|k| k != &key);
        }
    }

    /// True while a regex that timed out is being skipped.
    pub fn is_quarantined(&self, re: &SafeRegex) -> bool {
        let key = re.key();
        let mut g = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let (map, order) = &mut *g;
        match map.get(&key) {
            None => false,
            Some(&exp) if exp <= Instant::now() => {
                map.remove(&key);
                order.retain(|k| k != &key);
                false
            }
            Some(_) => true,
        }
    }
}

static GLOBAL_QUARANTINE: LazyLock<Quarantine> = LazyLock::new(Quarantine::new);

/// The process-wide quarantine the bot uses.
pub fn global_quarantine() -> &'static Quarantine {
    &GLOBAL_QUARANTINE
}

/// True while a regex that timed out is being skipped (process-wide quarantine).
pub fn is_quarantined(re: &SafeRegex) -> bool {
    GLOBAL_QUARANTINE.is_quarantined(re)
}

// ── Execution with a deadline ──────────────────────────────────────────────────

type Job = Box<dyn FnOnce() -> i64 + Send>;

struct Worker {
    tx: mpsc::Sender<(Job, mpsc::Sender<i64>)>,
}

static IDLE: LazyLock<Mutex<Vec<Worker>>> = LazyLock::new(|| Mutex::new(Vec::new()));
static RUNAWAY: AtomicUsize = AtomicUsize::new(0);

/// Matches that passed their deadline and are still running on an abandoned worker thread.
pub fn runaway_matches() -> usize {
    RUNAWAY.load(Ordering::Relaxed)
}

fn spawn_worker() -> Option<Worker> {
    let (tx, rx) = mpsc::channel::<(Job, mpsc::Sender<i64>)>();
    std::thread::Builder::new()
        .name("safe-regex".into())
        .spawn(move || {
            while let Ok((job, reply)) = rx.recv() {
                let v = job();
                if reply.send(v).is_err() {
                    // The caller gave up on this match: it was counted as a runaway.
                    RUNAWAY.fetch_sub(1, Ordering::Relaxed);
                }
            }
        })
        .ok()?;
    Some(Worker { tx })
}

/// Runs `job` on a worker thread; None when it does not finish within `timeout`.
fn run_with_deadline(job: Job, timeout: Duration) -> Option<i64> {
    let worker = IDLE.lock().unwrap_or_else(|e| e.into_inner()).pop().or_else(spawn_worker)?;
    let (reply_tx, reply_rx) = mpsc::channel();
    if worker.tx.send((job, reply_tx)).is_err() {
        return None;
    }
    match reply_rx.recv_timeout(timeout) {
        Ok(v) => {
            IDLE.lock().unwrap_or_else(|e| e.into_inner()).push(worker);
            Some(v)
        }
        Err(_) => {
            // Abandon the worker: dropping its sender ends the thread once the match returns.
            RUNAWAY.fetch_add(1, Ordering::Relaxed);
            drop(reply_rx);
            None
        }
    }
}

/// Input cut to `cap` UTF-16 units (a cut may split a surrogate pair, as `slice` does in JS).
fn capped(input: &str, cap: usize) -> Vec<u16> {
    let mut v: Vec<u16> = input.encode_utf16().collect();
    v.truncate(cap);
    v
}

/// Options of the safe matchers.
#[derive(Debug, Clone, Copy)]
pub struct MatchOptions {
    pub timeout_ms: u64,
    pub input_cap: usize,
}

/// Tests a regex against `input` with a hard timeout (default 25 ms, input cut to 2000). A regex that times out is
/// quarantined for 10 minutes and counts as no match; a quarantined regex is skipped (false).
pub fn safe_regex_test(re: &Arc<SafeRegex>, input: &str, opts: Option<MatchOptions>) -> bool {
    safe_regex_test_in(&GLOBAL_QUARANTINE, re, input, opts)
}

/// [`safe_regex_test`] against a given quarantine.
pub fn safe_regex_test_in(q: &Quarantine, re: &Arc<SafeRegex>, input: &str, opts: Option<MatchOptions>) -> bool {
    let o = opts.unwrap_or(MatchOptions { timeout_ms: 25, input_cap: DEFAULT_INPUT_CAP });
    if q.is_quarantined(re) {
        return false;
    }
    let units = capped(input, o.input_cap);
    test_units_with_deadline(q, re, units, o.timeout_ms)
}

fn test_units_with_deadline(q: &Quarantine, re: &Arc<SafeRegex>, units: Vec<u16>, timeout_ms: u64) -> bool {
    let r = re.clone();
    match run_with_deadline(Box::new(move || i64::from(r.test_units(&units))), Duration::from_millis(timeout_ms)) {
        Some(v) => v == 1,
        None => {
            q.add(re, timeout_ms);
            false
        }
    }
}

/// Tests several regexes against one input and returns the index of the first (in order) that matches, or -1. The
/// batch has one deadline (default 50 ms); when it passes, every regex in it is quarantined and then retried on its
/// own (25 ms each), so the innocent ones are released. `None` entries (not a regex) are skipped.
pub fn safe_regex_first_match(regexes: &[Option<Arc<SafeRegex>>], input: &str, opts: Option<MatchOptions>) -> i64 {
    safe_regex_first_match_in(&GLOBAL_QUARANTINE, regexes, input, opts)
}

/// [`safe_regex_first_match`] against a given quarantine.
pub fn safe_regex_first_match_in(q: &Quarantine, regexes: &[Option<Arc<SafeRegex>>], input: &str, opts: Option<MatchOptions>) -> i64 {
    let o = opts.unwrap_or(MatchOptions { timeout_ms: 50, input_cap: DEFAULT_INPUT_CAP });
    let res: Vec<Option<Arc<SafeRegex>>> =
        regexes.iter().map(|r| r.as_ref().filter(|r| !q.is_quarantined(r)).cloned()).collect();
    if res.iter().all(Option::is_none) {
        return -1;
    }
    let units = capped(input, o.input_cap);
    let batch = res.clone();
    let batch_units = units.clone();
    let job: Job = Box::new(move || {
        for (i, r) in batch.iter().enumerate() {
            if let Some(r) = r
                && r.test_units(&batch_units) {
                    return i as i64;
                }
        }
        -1
    });
    if let Some(idx) = run_with_deadline(job, Duration::from_millis(o.timeout_ms)) {
        return idx;
    }
    for r in res.iter().flatten() {
        q.add(r, o.timeout_ms);
    }
    let mut first = -1;
    for (i, r) in res.iter().enumerate() {
        let Some(r) = r else { continue };
        q.release(r);
        if test_units_with_deadline(q, r, units.clone(), 25) && first == -1 {
            first = i as i64;
        }
    }
    first
}

// ── Escaping and whole-word matching ───────────────────────────────────────────

/// Escapes regex syntax characters. `-` and `/` are not escaped (`\-` is a SyntaxError under the `u` flag).
pub fn escape_regex(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if ".*+?^${}()|[]\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

static WORD_CACHE: LazyLock<Mutex<Lru<Arc<Regex>>>> = LazyLock::new(|| Mutex::new(Lru::new(WORD_CACHE_MAX)));

/// Whole-word match that is Unicode-aware, so 'cunt' does not match inside 'Scunthorpe'. Linear by construction.
pub fn contains_word(content: &str, word: &str, case_sensitive: bool) -> bool {
    if word.is_empty() {
        return false;
    }
    let flags = if case_sensitive { "u" } else { "iu" };
    let key = format!("{flags}\u{0}{word}");
    let re = {
        let mut cache = WORD_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        match cache.get(&key) {
            Some(r) => r,
            None => {
                let pattern = format!(r"(?<![\p{{L}}\p{{N}}_]){}(?![\p{{L}}\p{{N}}_])", escape_regex(word));
                let Ok(r) = Regex::with_flags(&pattern, flags) else { return false };
                let r = Arc::new(r);
                cache.insert(key, r.clone());
                r
            }
        }
    };
    let units: Vec<u16> = content.encode_utf16().collect();
    re.find_from_utf16(&units, 0).next().is_some()
}
