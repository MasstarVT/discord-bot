//! The fixed regular expressions of the automod JS (never user input: user patterns go through
//! `bot_util::safe_regex`). They are compiled with regress, which has ECMAScript semantics (lookbehind, `\p{..}`,
//! the `i` and `u` flags), so they match what V8 matched.

use regress::{Match, Regex};

/// Compiles a fixed pattern. A pattern that does not compile (a bug, caught by the tests) never matches.
pub(crate) fn re(pattern: &str, flags: &str) -> Regex {
    match Regex::with_flags(pattern, flags) {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_error!(format!("[automod] internal regex /{pattern}/{flags} does not compile: {e}"));
            Regex::new("(?!)").unwrap_or_else(|_| unreachable_regex())
        }
    }
}

fn unreachable_regex() -> Regex {
    // `(?!)` always compiles; this keeps the code free of unwrap.
    Regex::new("[^\\s\\S]").unwrap_or_else(|_| unreachable_regex())
}

/// `re.test(s)`.
pub(crate) fn test(re: &Regex, s: &str) -> bool {
    re.find(s).is_some()
}

/// The number of (non-overlapping) matches, `(s.match(/../g) ?? []).length`.
pub(crate) fn count(re: &Regex, s: &str) -> usize {
    re.find_iter(s).count()
}

/// `s.replace(/../g, f)`.
pub(crate) fn replace_all(re: &Regex, s: &str, mut f: impl FnMut(&Match, &str) -> String) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for m in re.find_iter(s) {
        let r = m.range();
        if r.start < last {
            continue;
        }
        out.push_str(&s[last..r.start]);
        out.push_str(&f(&m, s));
        last = r.end;
    }
    out.push_str(&s[last..]);
    out
}

/// The text of capture group `i` of a match.
pub(crate) fn group<'a>(m: &Match, s: &'a str, i: usize) -> Option<&'a str> {
    m.group(i).map(|r| &s[r])
}
