//! test/registry.test.js sections 7 and 12, as scans of the Rust sources of every crate and binary:
//! every literal customId prefix is registered in `CUSTOM_ID_PREFIXES`, and no SQL string uses a bare `now()` or
//! `CURRENT_TIMESTAMP` (use `SQL_NOW` / `(now() AT TIME ZONE 'UTC')`). They catch the feature crates as they land.

use std::path::{Path, PathBuf};

use regex::Regex;

fn rust_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Every `src` file of every crate and binary (bot-config itself holds the registry and is skipped).
fn sources() -> Vec<PathBuf> {
    let mut files = Vec::new();
    for group in ["crates", "bins"] {
        let Ok(entries) = std::fs::read_dir(rust_root().join(group)) else { continue };
        for e in entries.flatten() {
            if e.file_name() == "bot-config" {
                continue;
            }
            walk(&e.path().join("src"), &mut files);
        }
    }
    files
}

fn rel(p: &Path) -> String {
    p.strip_prefix(rust_root()).unwrap_or(p).display().to_string().replace('\\', "/")
}

fn line_of(text: &str, byte: usize) -> usize {
    text[..byte].matches('\n').count() + 1
}

#[test]
fn every_literal_custom_id_prefix_is_registered() {
    let patterns = [
        r#"custom_id!\(\s*"([a-z][a-z0-9-]*)"#,
        r#"build_custom_id\(\s*"([a-z][a-z0-9-]*)"#,
        r#"CreateButton::new\(\s*"([a-z][a-z0-9-]*)"#,
        r#"CreateSelectMenu::new\(\s*"([a-z][a-z0-9-]*)"#,
        r#"CreateModal::new\(\s*"([a-z][a-z0-9-]*)"#,
        r#"CreateInputText::new\([^,]*,[^,]*,\s*"([a-z][a-z0-9-]*)"#,
        r#"\.custom_id\(\s*"([a-z][a-z0-9-]*)"#,
    ];
    let res: Vec<Regex> = patterns.iter().map(|p| Regex::new(p).unwrap()).collect();
    let mut problems = Vec::new();
    let files = sources();
    assert!(files.iter().any(|f| rel(f).starts_with("crates/bot-core/")), "the scan sees the sources");
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap_or_default();
        for re in &res {
            for m in re.captures_iter(&text) {
                let prefix = m.get(1).unwrap();
                if bot_config::prefix_info(prefix.as_str()).is_none() {
                    problems.push(format!("{}:{} unknown prefix \"{}\"", rel(file), line_of(&text, prefix.start()), prefix.as_str()));
                }
            }
        }
    }
    assert!(problems.is_empty(), "unregistered customId prefixes:\n{}", problems.join("\n"));
}

/// The string literals of a Rust source (normal and raw), with their byte offsets. Comments and char literals are
/// skipped; lifetimes are told apart from char literals.
fn string_literals(text: &str) -> Vec<(usize, String)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b'r' if (b.get(i + 1) == Some(&b'"') || b.get(i + 1) == Some(&b'#')) && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')) => {
                let mut j = i + 1;
                let mut hashes = 0;
                while b.get(j) == Some(&b'#') {
                    hashes += 1;
                    j += 1;
                }
                if b.get(j) != Some(&b'"') {
                    i += 1;
                    continue;
                }
                let start = j + 1;
                let close: String = std::iter::once('"').chain(std::iter::repeat_n('#', hashes)).collect();
                let end = text[start..].find(&close).map(|e| start + e).unwrap_or(b.len());
                out.push((start, text[start..end].to_string()));
                i = end + close.len();
            }
            b'"' => {
                let start = i + 1;
                let mut j = start;
                while j < b.len() && b[j] != b'"' {
                    if b[j] == b'\\' {
                        j += 1;
                    }
                    j += 1;
                }
                out.push((start, text[start..j.min(b.len())].to_string()));
                i = j + 1;
            }
            b'\'' => {
                // A char literal ('x', '\n', '"', '\'') or a lifetime ('a).
                if b.get(i + 1) == Some(&b'\\') {
                    let mut j = i + 2;
                    while j < b.len() && b[j] != b'\'' {
                        j += 1;
                    }
                    i = j + 1;
                } else if let Some(c) = text[i + 1..].chars().next() {
                    let after = i + 1 + c.len_utf8();
                    i = if b.get(after) == Some(&b'\'') { after + 1 } else { i + 1 };
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    out
}

/// A bare `now()` (not `.now()`, not followed by ` AT TIME ZONE 'UTC'`) or `CURRENT_TIMESTAMP`.
fn has_bare_now(sql: &str) -> bool {
    let lower = sql.to_lowercase();
    if lower.contains("current_timestamp") {
        return true;
    }
    let mut from = 0;
    while let Some(pos) = lower[from..].find("now()") {
        let at = from + pos;
        let before = lower[..at].chars().next_back();
        let qualified = before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.');
        let rest = lower[at + 5..].trim_start();
        let utc = rest.starts_with("at time zone 'utc'");
        if !qualified && !utc {
            return true;
        }
        from = at + 5;
    }
    false
}

#[test]
fn no_sql_string_uses_a_bare_now() {
    let sql = Regex::new(r"(?i)\b(select|insert\s+into|update|delete\s+from|with)\b").unwrap();
    let mut problems = Vec::new();
    for file in sources() {
        let r = rel(&file);
        // The SQL_NOW definition itself, and the migration runner: _prisma_migrations has timestamptz columns that
        // Prisma's own engine fills with now().
        if r == "crates/bot-db/src/client.rs" || r == "crates/bot-db/src/migrate.rs" {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        for (offset, lit) in string_literals(&text) {
            if sql.is_match(&lit) && has_bare_now(&lit) {
                problems.push(format!("{r}:{}", line_of(&text, offset)));
            }
        }
    }
    assert!(problems.is_empty(), "use SQL_NOW / (now() AT TIME ZONE 'UTC'):\n{}", problems.join("\n"));
}

#[test]
fn the_scanners_see_what_they_look_for() {
    assert!(has_bare_now("UPDATE x SET a = now()"));
    assert!(has_bare_now("SELECT CURRENT_TIMESTAMP"));
    assert!(!has_bare_now("SELECT (now() AT TIME ZONE 'UTC')"));
    assert!(!has_bare_now("SELECT x.now()"));
    let lits = string_literals("let a = \"SELECT now()\"; // \"no\"\nlet b = r#\"UPDATE \"T\" SET x = 1\"#; let c = '\"'; fn f<'a>() {}");
    let texts: Vec<&str> = lits.iter().map(|(_, s)| s.as_str()).collect();
    assert_eq!(texts, vec!["SELECT now()", "UPDATE \"T\" SET x = 1"]);
}
