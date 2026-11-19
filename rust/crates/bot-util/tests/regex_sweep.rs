//! Replays rust/fixtures/golden/bot-util/regex-sweep.json (bot-util-regex-sweep.mjs): single-character case folding
//! over the whole BMP, class escapes and the dot over every code unit, and thousands of seeded random patterns
//! (V8's verdict and error text, then matches on sample inputs).

mod common;

use bot_util::safe_regex::{self, SafeRegex};
use serde_json::Value;

fn sweep() -> Value {
    let text = std::fs::read_to_string(common::golden_dir().join("regex-sweep.json")).expect("regex-sweep.json");
    serde_json::from_str(&text).unwrap()
}

#[test]
fn single_character_case_folding_matches_v8() {
    let v = sweep();
    let mut bad = Vec::new();
    for pair in v["fold"].as_array().unwrap() {
        let c = pair[0].as_u64().unwrap() as u16;
        let r = pair[1].as_u64().unwrap() as u16;
        let bits = pair[2].as_u64().unwrap();
        let lit = format!("\\u{c:04x}");
        let res = [
            SafeRegex::new(&format!("^{lit}$"), "i").unwrap(),
            SafeRegex::new(&format!("^[{lit}]$"), "i").unwrap(),
            SafeRegex::new(&format!("^{lit}$"), "iu").unwrap(),
            SafeRegex::new(&format!("^[{lit}]$"), "iu").unwrap(),
        ];
        for (i, re) in res.iter().enumerate() {
            let want = bits & (1 << i) != 0;
            if re.test_units(&[r]) != want {
                bad.push(format!("U+{c:04X} vs U+{r:04X} ({}) want {want}", ["/^c$/i", "/^[c]$/i", "/^c$/iu", "/^[c]$/iu"][i]));
            }
        }
    }
    assert!(bad.is_empty(), "{} case-folding differences:\n{}", bad.len(), bad.iter().take(60).cloned().collect::<Vec<_>>().join("\n"));
}

#[test]
fn class_escapes_and_dot_match_v8_on_every_code_unit() {
    let v = sweep();
    let mut bad = Vec::new();
    for (key, ranges) in v["classes"].as_object().unwrap() {
        let (pattern, flags) = key.rsplit_once('/').unwrap();
        let re = SafeRegex::new(pattern, flags).unwrap();
        let mut want = vec![false; 0x10000];
        for r in ranges.as_array().unwrap() {
            for c in r[0].as_u64().unwrap()..=r[1].as_u64().unwrap() {
                want[c as usize] = true;
            }
        }
        let mut diffs = 0;
        let mut first = None;
        for c in 0..=0xffffu32 {
            if re.test_units(&[c as u16]) != want[c as usize] {
                diffs += 1;
                first.get_or_insert(c);
            }
        }
        if diffs > 0 {
            bad.push(format!("/{pattern}/{flags}: {diffs} code units differ, first U+{:04X}", first.unwrap_or(0)));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn random_patterns_validate_and_match_like_v8() {
    let v = sweep();
    let mut verdict = Vec::new();
    let mut matching = Vec::new();
    let mut compile = Vec::new();
    let mut total_tests = 0;
    let list = v["random"].as_array().unwrap();
    for r in list {
        let pattern = r[0].as_str().unwrap();
        let flags = r[1].as_str().unwrap();
        let want = r[2].as_str().map(str::to_string);
        let got = safe_regex::validate_regex(pattern, flags).err();
        if got != want {
            verdict.push(format!("/{pattern}/{flags}\n   V8:   {want:?}\n   Rust: {got:?}"));
            continue;
        }
        if want.is_some() {
            continue;
        }
        let re = match SafeRegex::new(pattern, flags) {
            Ok(re) => re,
            Err(e) => {
                compile.push(format!("/{pattern}/{flags}: regress refused it: {e}"));
                continue;
            }
        };
        for t in r[3].as_array().unwrap() {
            total_tests += 1;
            let input = t[0].as_str().unwrap();
            if re.test_str(input) != t[1].as_bool().unwrap() {
                matching.push(format!("/{pattern}/{flags} on {input:?}: V8 {}", t[1]));
            }
        }
    }
    eprintln!("{} patterns, {total_tests} matches checked", list.len());
    let all: Vec<String> = verdict.iter().chain(&compile).chain(&matching).cloned().collect();
    assert!(
        all.is_empty(),
        "{} verdicts, {} compiles, {} matches differ:\n{}",
        verdict.len(),
        compile.len(),
        matching.len(),
        all.iter().take(60).cloned().collect::<Vec<_>>().join("\n")
    );
}
