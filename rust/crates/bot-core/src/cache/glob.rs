//! Redis glob patterns.
//!
//! * [`glob_to_regex`] is `globToRegExp` from redis.js: `*` and `?` are wildcards, every other character is literal.
//!   The JS uses it as the keep-filter of `invalidateAny`.
//! * [`redis_glob_match`] is Redis's own `stringmatchlen` (`*`, `?`, `[...]` with `^` and ranges, `\` escapes), used
//!   by the in-memory backend's `SCAN MATCH`.

use regex::Regex;

/// `globToRegExp(glob)`: an anchored regex where `*` is `.*`, `?` is `.` (dot matches newlines, flag `s`) and every
/// other character is literal.
pub fn glob_to_regex(glob: &str) -> Regex {
    let mut body = String::with_capacity(glob.len() * 2 + 8);
    body.push_str("(?s)^");
    for ch in glob.chars() {
        match ch {
            '*' => body.push_str(".*"),
            '?' => body.push('.'),
            c => body.push_str(&regex::escape(&c.to_string())),
        }
    }
    body.push('$');
    // Every character is escaped, so this cannot fail; fall back to a regex that matches nothing.
    Regex::new(&body).unwrap_or_else(|_| Regex::new("[^\\s\\S]").expect("static regex"))
}

/// Redis's glob matcher (`stringmatchlen`, case-sensitive), over characters.
pub fn redis_glob_match(pattern: &str, string: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = string.chars().collect();
    matches(&p, &s)
}

fn matches(p: &[char], s: &[char]) -> bool {
    let (mut pi, mut si) = (0usize, 0usize);
    while pi < p.len() {
        match p[pi] {
            '*' => {
                while pi + 1 < p.len() && p[pi + 1] == '*' {
                    pi += 1;
                }
                if pi + 1 == p.len() {
                    return true;
                }
                for start in si..=s.len() {
                    if matches(&p[pi + 1..], &s[start..]) {
                        return true;
                    }
                }
                return false;
            }
            '?' => {
                if si >= s.len() {
                    return false;
                }
                si += 1;
            }
            '[' => {
                if si >= s.len() {
                    return false;
                }
                pi += 1;
                let not = pi < p.len() && p[pi] == '^';
                if not {
                    pi += 1;
                }
                let mut matched = false;
                loop {
                    if pi >= p.len() {
                        // Unterminated class: Redis treats the end of the pattern as the closing bracket.
                        pi -= 1;
                        break;
                    }
                    if p[pi] == '\\' && pi + 2 < p.len() {
                        pi += 1;
                        if p[pi] == s[si] {
                            matched = true;
                        }
                    } else if p[pi] == ']' {
                        break;
                    } else if pi + 2 < p.len() && p[pi + 1] == '-' {
                        let (mut start, mut end) = (p[pi], p[pi + 2]);
                        if start > end {
                            std::mem::swap(&mut start, &mut end);
                        }
                        pi += 2;
                        if s[si] >= start && s[si] <= end {
                            matched = true;
                        }
                    } else if p[pi] == s[si] {
                        matched = true;
                    }
                    pi += 1;
                }
                if not {
                    matched = !matched;
                }
                if !matched {
                    return false;
                }
                si += 1;
            }
            '\\' if pi + 1 < p.len() => {
                pi += 1;
                if si >= s.len() || p[pi] != s[si] {
                    return false;
                }
                si += 1;
            }
            c => {
                if si >= s.len() || c != s[si] {
                    return false;
                }
                si += 1;
            }
        }
        pi += 1;
    }
    si == s.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_to_regex_like_the_js() {
        assert!(glob_to_regex("guild:1.2:*").is_match("guild:1.2:settings"));
        assert!(!glob_to_regex("guild:1.2:*").is_match("guild:102:settings"), "a dot is literal");
        assert!(glob_to_regex("a?c").is_match("abc"));
        assert!(glob_to_regex("a*").is_match("a\nb"), "flag s");
        assert!(!glob_to_regex("a").is_match("ab"), "anchored");
        assert!(glob_to_regex("x[1]").is_match("x[1]"), "brackets are literal in globToRegExp");
    }

    #[test]
    fn redis_glob() {
        assert!(redis_glob_match("*:9:*", "guild:9:settings"));
        assert!(!redis_glob_match("*:9:*", "guild:99:settings"));
        assert!(redis_glob_match("h?llo", "hello"));
        assert!(redis_glob_match("h[ae]llo", "hallo"));
        assert!(!redis_glob_match("h[^e]llo", "hello"));
        assert!(redis_glob_match("h[a-b]llo", "hbllo"));
        assert!(redis_glob_match("h\\*llo", "h*llo"));
        assert!(!redis_glob_match("h\\*llo", "hello"));
        assert!(redis_glob_match("*", ""));
        assert!(redis_glob_match("guild:*:settings", "guild:1:settings"));
    }
}
