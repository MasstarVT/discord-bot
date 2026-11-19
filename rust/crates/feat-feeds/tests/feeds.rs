//! Port of `test/feeds.test.js`, `test/review-feeds.test.js` and `test/sec-feeds.test.js` (the feed parts), with the
//! parser parity checks against `rust/fixtures/golden/feat-feeds/parsers.json` (made by
//! `rust/fixtures/gen/feat-feeds-parsers.mjs` from the JS parsers) and the service/command checks.

mod common;

use bot_core::prelude::*;
use bot_core::testing::{DbTestApp, FakeInteraction, TestApp, database_url, offline_discord_as, run_node};
use bot_db::{Feed, FeedType};
use bot_util::safe_fetch::SafeFetchCode;
use chrono::{DateTime, Utc};
use common::*;
use feat_feeds::feed_parsers::{self as p, FeedItem};
use feat_feeds::feed_service as svc;
use serde_json::{Value, json};
use serenity::all::Permissions;

fn golden(name: &str) -> Value {
    let path = bot_core::testing::golden_dir().join("feat-feeds").join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    // A fixture that does not parse must fail loudly: a Null would turn every loop over it into a no-op.
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn feed_json(r: Result<p::ParsedFeed, p::FeedParseError>) -> Value {
    match r {
        Ok(f) => json!({ "ok": true, "kind": f.kind, "title": f.title, "link": f.link, "image": f.image, "items": f.items }),
        Err(e) => json!({ "ok": false, "code": e.code }),
    }
}

// ── Parser parity with the JS ──────────────────────────────────────────────────

#[test]
fn xml_trees_match_fast_xml_parser() {
    let g = golden("parsers.json");
    let docs = g["docs"].as_object().cloned().unwrap_or_default();
    assert!(docs.len() > 40);
    for (name, d) in docs {
        let xml = d["xml"].as_str().unwrap_or("");
        let ours = match feat_feeds::xml::parse(&p::strip_doctype(xml)) {
            Ok(t) => json!({ "ok": true, "tree": t }),
            Err(_) => json!({ "ok": false }),
        };
        assert_eq!(ours, d["tree"], "tree of {name}");
    }
}

/// Generated feed documents and mutations of the golden ones (rust/fixtures/gen/feat-feeds-xmlfuzz.mjs): the
/// fast-xml-parser tree and the parseXmlFeed output. Every mismatch is listed.
#[test]
fn xml_fuzz_matches_the_js() {
    let g = golden("xml-fuzz.json");
    assert!(g.as_array().is_some_and(|a| a.len() > 600), "the fixture has its documents");
    let mut bad = Vec::new();
    for (i, d) in g.as_array().into_iter().flatten().enumerate() {
        let xml = d["xml"].as_str().unwrap_or("");
        let tree = match feat_feeds::xml::parse(&p::strip_doctype(xml)) {
            Ok(t) => json!({ "ok": true, "tree": t }),
            Err(_) => json!({ "ok": false }),
        };
        if tree != d["tree"] {
            bad.push(format!("#{i} tree of {xml:?}
  js   {}
  rust {tree}", d["tree"]));
        }
        let feed = feed_json(p::parse_xml_feed(xml));
        if feed != d["feed"] {
            bad.push(format!("#{i} feed of {xml:?}
  js   {}
  rust {feed}", d["feed"]));
        }
    }
    assert!(bad.is_empty(), "{} mismatches:
{}", bad.len(), bad.join("
"));
}

#[test]
fn feeds_normalise_like_the_js() {
    let g = golden("parsers.json");
    for (name, d) in g["docs"].as_object().cloned().unwrap_or_default() {
        let ours = feed_json(p::parse_xml_feed(d["xml"].as_str().unwrap_or("")));
        assert_eq!(ours, d["feed"], "feed of {name}");
    }
}

/// Hostile input never panics: in the JS any parser failure becomes INVALID_XML (a counted failure), while a Rust
/// panic would kill the poll. Every prefix of every golden document and date, with a few mutations, is parsed.
#[test]
fn parsers_never_panic_on_truncated_or_mutated_input() {
    let g = golden("parsers.json");
    let mut inputs: Vec<String> = Vec::new();
    for (_, d) in g["docs"].as_object().cloned().unwrap_or_default() {
        inputs.push(d["xml"].as_str().unwrap_or("").to_string());
    }
    let dates: Vec<String> = g["dates"].as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
    let mutate = |s: &str, k: usize| -> Vec<String> {
        let chars: Vec<char> = s.chars().collect();
        let mut out = Vec::new();
        for (i, _) in chars.iter().enumerate().step_by(k.max(1)) {
            for rep in ['<', '>', '&', ';', '"', '\'', '/', '!', '?', ']', '-', '😀', '\u{0}', 'é'] {
                let mut c = chars.clone();
                c[i] = rep;
                out.push(c.into_iter().collect());
            }
            let mut c = chars.clone();
            c.remove(i);
            out.push(c.into_iter().collect());
        }
        out
    };
    let run = |label: &str, s: &str| {
        let r = std::panic::catch_unwind(|| {
            let _ = p::parse_xml_feed(s);
            let _ = p::looks_like_html(s, "");
            let _ = p::discover_feed_links(s, "https://example.com/a/b");
            let _ = p::strip_html(s);
            let _ = p::one_line(s);
            let _ = p::strip_doctype(s);
            let _ = feat_feeds::js_date::date_parse(s);
            let _ = p::normalize_feed_url(s);
            let _ = p::parse_youtube_channel_input(s);
            let _ = p::parse_subreddit_input(s);
            let _ = p::parse_bluesky_actor_input(s);
            let _ = p::decode_xml_buffer(s.as_bytes(), "text/xml; charset=iso-8859-1");
            let _ = p::decode_xml_buffer(s.as_bytes(), "text/xml; charset=x-unknown-☃");
        });
        assert!(r.is_ok(), "{label} panicked on {s:?}");
    };
    for doc in &inputs {
        let idx: Vec<usize> = doc.char_indices().map(|(i, _)| i).chain(std::iter::once(doc.len())).collect();
        for &i in idx.iter().step_by(3) {
            run("prefix", &doc[..i]);
            run("suffix", &doc[i..]);
        }
        let step = (doc.chars().count() / 40).max(1);
        for m in mutate(doc, step) {
            run("mutation", &m);
        }
    }
    for d in &dates {
        let idx: Vec<usize> = d.char_indices().map(|(i, _)| i).chain(std::iter::once(d.len())).collect();
        for &i in &idx {
            run("date prefix", &d[..i]);
        }
        for m in mutate(d, 1) {
            run("date mutation", &m);
        }
    }
    for odd in ["", "\u{feff}", "<", "<!", "<!--", "<![CDATA[", "<?xml", "<a", "<a b=", "<a b=\"", "&#x;", "&#xFFFFFFFFFF;", "&#99999999999;", "\u{d7ff}\u{e000}", "<rss><channel><item><title>&amp", "ÿþ<\u{0}r\u{0}"] {
        run("odd", odd);
    }
    let bytes: Vec<u8> = (0u16..512).map(|b| (b.wrapping_mul(97) % 256) as u8).collect();
    let r = std::panic::catch_unwind(|| {
        for cs in ["", "text/xml; charset=utf-16", "text/xml; charset=utf-16le", "text/xml; charset=windows-1252", "text/xml; charset=\"\"", "application/xml;charset=shift_jis"] {
            let _ = p::decode_xml_buffer(&bytes, cs);
            let _ = p::decode_xml_buffer(&bytes[..1], cs);
            let _ = p::decode_xml_buffer(&[0xFF, 0xFE, 0x3C], cs);
        }
    });
    assert!(r.is_ok(), "decode_xml_buffer panicked on raw bytes");
}

#[test]
fn dates_parse_like_v8() {
    let g = golden("parsers.json");
    for (s, expected) in g["dates"].as_object().cloned().unwrap_or_default() {
        let t = feat_feeds::js_date::date_parse(&s);
        let ours = if t.is_finite() { json!(t as i64) } else { Value::Null };
        assert_eq!(ours, expected, "Date.parse({s:?})");
    }
}

/// Generated date strings (rust/fixtures/gen/feat-feeds-dates.mjs, read by V8 in UTC). Skipped when TZ names
/// another zone, since a string without a zone is read in the local one.
#[test]
fn generated_dates_parse_like_v8() {
    if std::env::var("TZ").is_ok_and(|z| !z.is_empty() && z != "UTC") {
        eprintln!("skipped: TZ is set");
        return;
    }
    let g = golden("dates-fuzz.json");
    assert!(g.as_object().is_some_and(|o| o.len() > 400), "the fixture has its dates");
    let mut bad = Vec::new();
    for (s, expected) in g.as_object().cloned().unwrap_or_default() {
        let t = feat_feeds::js_date::date_parse(&s);
        let ours = if t.is_finite() { json!(t as i64) } else { Value::Null };
        if ours != expected {
            bad.push(format!("Date.parse({s:?}): js {expected} rust {ours}"));
        }
    }
    assert!(bad.is_empty(), "{} mismatches:
{}", bad.len(), bad.join("
"));
}

#[test]
fn json_and_text_helpers_match_the_js() {
    let g = golden("parsers.json");
    let m = &g["misc"];
    assert_eq!(feed_json(p::parse_bluesky_author_feed(&m["blueskyInput"])), m["bluesky"]);
    let bad = [p::parse_bluesky_author_feed(&json!({})), p::parse_bluesky_author_feed(&Value::Null), p::parse_bluesky_author_feed(&json!({ "feed": "x" }))];
    assert_eq!(Value::Array(bad.into_iter().map(feed_json).collect()), m["blueskyBad"]);
    assert_eq!(serde_json::to_value(p::parse_itunes_search(&m["itunesInput"])).unwrap_or(Value::Null), m["itunes"]);
    for c in m["stripHtml"].as_array().into_iter().flatten() {
        assert_eq!(p::strip_html(c[0].as_str().unwrap_or("")), c[1].as_str().unwrap_or(""), "stripHtml {}", c[0]);
    }
    for c in m["oneLine"].as_array().into_iter().flatten() {
        assert_eq!(p::one_line(c[0].as_str().unwrap_or("")), c[1].as_str().unwrap_or(""), "oneLine {}", c[0]);
    }
    for c in m["looksLikeHtml"].as_array().into_iter().flatten() {
        assert_eq!(json!(p::looks_like_html(c[0].as_str().unwrap_or(""), c[1].as_str().unwrap_or(""))), c[2], "looksLikeHtml {} / {}", c[0], c[1]);
    }
    for c in m["discover"].as_array().into_iter().flatten() {
        assert_eq!(json!(p::discover_feed_links(c[0].as_str().unwrap_or(""), c[1].as_str().unwrap_or(""))), c[2], "discover {}", c[0]);
    }
    for c in m["stripDoctype"].as_array().into_iter().flatten() {
        assert_eq!(p::strip_doctype(c[0].as_str().unwrap_or("")), c[1].as_str().unwrap_or(""), "stripDoctype {}", c[0]);
    }
    for c in m["decode"].as_array().into_iter().flatten() {
        let bytes = base64_decode(c[0].as_str().unwrap_or(""));
        assert_eq!(p::decode_xml_buffer(&bytes, c[1].as_str().unwrap_or("")), c[2].as_str().unwrap_or(""), "decode {}", c[1]);
    }
    for c in m["itemKey"].as_array().into_iter().flatten() {
        assert_eq!(p::item_key(c[0].as_str().unwrap_or("")), c[1].as_str().unwrap_or(""));
    }
    for c in m["normalizeFeedUrl"].as_array().into_iter().flatten() {
        let ours = match p::normalize_feed_url(c[0].as_str().unwrap_or("")) {
            Ok(url) => json!({ "ok": true, "url": url }),
            Err(reason) => json!({ "ok": false, "reason": reason }),
        };
        assert_eq!(ours, c[1], "normalizeFeedUrl {}", c[0]);
    }
    for c in m["youtube"].as_array().into_iter().flatten() {
        let ours = match p::parse_youtube_channel_input(c[0].as_str().unwrap_or("")) {
            Ok(id) => json!({ "ok": true, "channelId": id }),
            Err(r) => json!({ "ok": false, "handle": r.handle, "reason": r.reason }),
        };
        assert_eq!(ours, c[1], "youtube {}", c[0]);
    }
    for c in m["subreddit"].as_array().into_iter().flatten() {
        let ours = match p::parse_subreddit_input(c[0].as_str().unwrap_or("")) {
            Ok(name) => json!({ "ok": true, "name": name }),
            Err(reason) => json!({ "ok": false, "reason": reason }),
        };
        assert_eq!(ours, c[1], "subreddit {}", c[0]);
    }
    for c in m["bluesky_actor"].as_array().into_iter().flatten() {
        let ours = match p::parse_bluesky_actor_input(c[0].as_str().unwrap_or("")) {
            Ok(actor) => json!({ "ok": true, "actor": actor }),
            Err(reason) => json!({ "ok": false, "reason": reason }),
        };
        assert_eq!(ours, c[1], "bluesky actor {}", c[0]);
    }
}

fn base64_decode(s: &str) -> Vec<u8> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        let Some(v) = T.iter().position(|t| *t == c) else { continue };
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    out
}

// ── Declarations ───────────────────────────────────────────────────────────────

fn registry() -> bot_core::Registry {
    let mut reg = bot_core::Registry::new();
    feat_feeds::register(&mut reg);
    reg
}

#[test]
fn commands_and_plugins_match_the_js() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &["feed", "reminder"]);
    bot_core::testing::assert_plugin_matches_golden(&reg, "feeds");
    bot_core::testing::assert_plugin_matches_golden(&reg, "reminders");
}

#[path = "feeds/service.rs"]
mod service;

#[path = "feeds/network.rs"]
mod network;

#[path = "feeds/db.rs"]
mod db;
