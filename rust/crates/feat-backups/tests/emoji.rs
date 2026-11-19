//! `/emoji` tests: the parsers and texts against the JS (`rust/fixtures/gen/feat-backups-golden.mjs`), the upload guard,
//! and the command through the router with an offline guild (ported from `test/emojis.test.js`).

use std::collections::HashSet;
use std::sync::Arc;

use bot_core::command::option_type::{ROLE, STRING};
use bot_core::testing::{FakeGuild, FakeInteraction, TEST_BOT_ID, TestApp, load_golden};
use bot_util::safe_fetch::{SafeFetchCode, SafeFetchError};
use feat_backups::commands::emoji::*;
use serde_json::{Value, json};
use serenity::all::Permissions;

fn g() -> Value {
    load_golden("feat-backups/golden.json")["emoji"].clone()
}

fn emojis() -> Vec<EmojiInfo> {
    g()["emojis"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| EmojiInfo {
            id: e["id"].as_str().unwrap().into(),
            name: e["name"].as_str().unwrap().into(),
            animated: e["animated"].as_bool().unwrap(),
            managed: e["managed"].as_bool().unwrap(),
            available: e["available"].as_bool().unwrap(),
            role_count: e["roles"]["cache"]["size"].as_u64().unwrap() as usize,
        })
        .collect()
}

#[test]
fn parsers_match_the_js() {
    let g = g();
    for c in g["parsed"].as_array().unwrap() {
        assert_eq!(parse_emoji_source(c["in"].as_str().unwrap()).to_json(), c["out"], "parseEmojiSource({})", c["in"]);
    }
    for c in g["sanitize"].as_array().unwrap() {
        assert_eq!(sanitize_emoji_name(c["in"].as_str().unwrap()), c["out"].as_str().unwrap(), "sanitize({})", c["in"]);
    }
    for c in g["nameUrls"].as_array().unwrap() {
        assert_eq!(json!(name_from_url(c["in"].as_str().unwrap())), c["out"], "nameFromUrl({})", c["in"]);
    }
    for c in g["resolveNames"].as_array().unwrap() {
        let src = &c["in"]["source"];
        let source = match src["kind"].as_str() {
            Some("custom") => EmojiSource::Custom { id: "1".into(), name: src["name"].as_str().map(String::from), animated: false },
            Some("url") => EmojiSource::Url { url: String::new(), name_hint: src["nameHint"].as_str().map(String::from) },
            _ => EmojiSource::Invalid,
        };
        let out = match resolve_emoji_name(c["in"]["given"].as_str(), &source) {
            NameResult::Ok { name, adjusted, derived } => json!({ "ok": true, "name": name, "adjusted": adjusted, "derived": derived }),
            NameResult::Err(reason) => json!({ "ok": false, "reason": reason }),
        };
        assert_eq!(out, c["out"], "resolveEmojiName({})", c["in"]);
    }
    let mut taken: HashSet<String> = ["blob".to_string(), "blob_2".into(), "full".into(), "x".repeat(32)].into_iter().collect();
    taken.extend((0..98).map(|i| format!("full_{}", i + 2)));
    for c in g["freeNames"].as_array().unwrap() {
        assert_eq!(json!(pick_free_name(c["in"].as_str().unwrap(), &taken)), c["out"], "pickFreeName({})", c["in"]);
    }
    assert_eq!(json!([emoji_cdn_url("1", false, None), emoji_cdn_url("1", true, Some(96))]), g["cdn"]);
}

#[test]
fn lists_lookups_and_slots_match_the_js() {
    let g = g();
    let list = emojis();
    for c in g["resolved"].as_array().unwrap() {
        let out = match resolve_guild_emoji(&list, c["in"].as_str().unwrap()) {
            Ok(e) => json!({ "ok": true, "id": e.id }),
            Err(reason) => json!({ "ok": false, "reason": reason }),
        };
        assert_eq!(out, c["out"], "resolveGuildEmoji({})", c["in"]);
    }
    for c in g["choices"].as_array().unwrap() {
        let out: Vec<Value> = emoji_choices(&list, c["in"].as_str().unwrap()).into_iter().map(|(n, v)| json!({ "name": n, "value": v })).collect();
        assert_eq!(json!(out), c["out"], "emojiChoices({})", c["in"]);
    }
    for c in g["slots"].as_array().unwrap() {
        let tier = c["in"][0].as_i64().unwrap();
        let features: Vec<String> = c["in"][1].as_array().unwrap().iter().map(|f| f.as_str().unwrap().to_string()).collect();
        assert_eq!(json!(emoji_slot_limit(tier, &features)), c["limit"]);
        assert_eq!(serde_json::to_value(count_emoji_slots(&list, tier, &features)).unwrap(), c["slots"]);
    }
    let pages = build_list_pages(&list, &count_emoji_slots(&list, 1, &[]), 3);
    assert_eq!(serde_json::to_value(&pages).unwrap(), g["pages"]);
    assert!(build_list_pages(&[], &count_emoji_slots(&[], 0, &[]), 20).is_empty());
    let lines: Vec<String> = list.iter().map(format_emoji_line).collect();
    assert_eq!(json!(lines), g["lines"]);
}

fn code_of(s: &str) -> SafeFetchCode {
    match s {
        "INVALID_URL" => SafeFetchCode::InvalidUrl,
        "BLOCKED_ADDRESS" => SafeFetchCode::BlockedAddress,
        "DNS_FAILED" => SafeFetchCode::DnsFailed,
        "TIMEOUT" => SafeFetchCode::Timeout,
        "TOO_LARGE" => SafeFetchCode::TooLarge,
        "TOO_MANY_REDIRECTS" => SafeFetchCode::TooManyRedirects,
        "HTTP_ERROR" => SafeFetchCode::HttpError,
        "BAD_CONTENT_TYPE" => SafeFetchCode::BadContentType,
        "BAD_IMAGE" => SafeFetchCode::BadImage,
        _ => SafeFetchCode::Network,
    }
}

#[test]
fn image_sniffing_and_error_texts_match_the_js() {
    let g = g();
    for c in g["sniff"].as_array().unwrap() {
        let bytes: Vec<u8> = c["in"].as_array().unwrap().iter().map(|b| b.as_u64().unwrap() as u8).collect();
        let out = sniff_image(&bytes).map(|(mime, ext, animated)| json!({ "mime": mime, "ext": ext, "animated": animated }));
        assert_eq!(json!(out), c["out"], "sniff({bytes:?})");
    }
    for c in g["fetchErrs"].as_array().unwrap() {
        let mut e = SafeFetchError::new(code_of(c["code"].as_str().unwrap()), c["message"].as_str());
        e.status = c["status"].as_u64().map(|s| s as u16);
        assert_eq!(describe_fetch_error(&DownloadError::Fetch(e), c["fromCdn"].as_bool().unwrap()), c["out"].as_str().unwrap(), "{c}");
    }
    assert_eq!(describe_fetch_error(&DownloadError::Other("x".into()), false), "The image could not be downloaded.");
    for c in g["emojiErrs"].as_array().unwrap() {
        let err = EmojiError::Discord { code: c["code"].as_u64(), message: c["message"].as_str().unwrap().into() };
        assert_eq!(describe_emoji_error(&err, c["action"] == "delete"), c["out"].as_str().unwrap(), "{c}");
    }
    assert_eq!(describe_emoji_error(&EmojiError::Timeout, false), g["timeouts"][0].as_str().unwrap());
    assert_eq!(describe_emoji_error(&EmojiError::Timeout, true), g["timeouts"][1].as_str().unwrap());
}

#[test]
fn the_upload_guard_releases_only_its_own_token_and_expires_stale_ones() {
    let guard = UploadGuard::default();
    let t1 = guard.begin("g", 0).unwrap();
    assert!(guard.begin("g", 1000).is_none(), "one upload at a time");
    assert!(guard.begin("other", 1000).is_some(), "per guild");
    let t2 = guard.begin("g", IN_FLIGHT_STALE_MS).expect("a stale upload no longer blocks");
    guard.end("g", t1);
    assert!(guard.is_busy("g"), "the stale token cannot clear the newer entry");
    guard.end("g", t2);
    assert!(!guard.is_busy("g"));
}

// ── Through the router ────────────────────────────────────────────────────────

const G: &str = "950000000000000001";
const OWNER: &str = "950000000000000009";
const ADMIN: &str = "950000000000000002";
const ADMIN_ROLE: &str = "950000000000000005";
const BOT_ROLE: &str = "950000000000000006";

fn app(bot_perms: Permissions) -> TestApp {
    let guild = FakeGuild::new(G, OWNER)
        .role(ADMIN_ROLE, 3, Permissions::ADMINISTRATOR)
        .role(BOT_ROLE, 2, bot_perms)
        .member(ADMIN, &[ADMIN_ROLE])
        .member(TEST_BOT_ID, &[BOT_ROLE]);
    let mut json = guild.json();
    json["emojis"] = json!([
        { "id": "111111111111111111", "name": "blob", "animated": false, "managed": false, "available": true, "require_colons": true, "roles": [] },
        { "id": "222222222222222222", "name": "twitch", "animated": false, "managed": true, "available": true, "require_colons": true, "roles": [] }
    ]);
    let t = TestApp::new().with_guilds([G]);
    t.set_discord(bot_core::testing::offline_discord_as(&[json]));
    let mut reg = bot_core::Registry::new();
    feat_backups::register(&mut reg);
    t.install(reg);
    t
}

fn add(emoji: &str) -> FakeInteraction {
    FakeInteraction::slash("emoji").guild(G).user(ADMIN).roles(&[ADMIN_ROLE]).permissions(Permissions::ADMINISTRATOR).subcommand("add").option("emoji", STRING, json!(emoji))
}

#[tokio::test]
async fn add_refuses_bad_input_before_the_defer() {
    let t = app(Permissions::MANAGE_GUILD_EXPRESSIONS);
    let cases = [
        ("not an emoji", "❌  Invalid Emoji"),
        ("😀", "❌  Invalid Emoji"),
        (":blob:", "❌  Invalid Emoji"),
        ("123456789012345678", "❌  Invalid Emoji"),
        ("<:blob:111111111111111111>", "❌  Already Added"),
    ];
    for (input, title) in cases {
        bot_core::router::route_interaction(&t, add(input).build()).await;
        let call = t.responses.calls().last().cloned().unwrap();
        assert_eq!(call.method, "reply", "{input}");
        assert!(call.ephemeral());
        assert_eq!(call.embed_title(), Some(title), "{input}: {:?}", call.embed_description());
    }
    bot_core::router::route_interaction(&t, add("<:other:333333333333333333>").option("role", ROLE, json!(G)).build()).await;
    assert_eq!(t.responses.calls().last().unwrap().embed_title(), Some("❌  Invalid Role"));
    bot_core::router::route_interaction(&t, add("<:other:333333333333333333>").option("name", STRING, json!("Blob")).build()).await;
    assert_eq!(t.responses.calls().last().unwrap().embed_title(), Some("❌  Name Taken"));
    // The bot cannot upload.
    let weak = app(Permissions::VIEW_CHANNEL);
    bot_core::router::route_interaction(&weak, add("<:other:333333333333333333>").build()).await;
    assert_eq!(weak.responses.calls().last().unwrap().embed_title(), Some("❌  Missing Permissions"));
}

#[tokio::test]
async fn add_uploads_a_sniffed_data_uri_and_releases_the_guard() {
    let t = app(Permissions::MANAGE_GUILD_EXPRESSIONS);
    let hooks = t.ext::<EmojiTestHooks>();
    let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0, 1, 2];
    let png2 = png.clone();
    *hooks.download.lock().unwrap() = Some(Arc::new(move |_| {
        let b = png2.clone();
        Box::pin(async move { Ok(b) })
    }));
    let seen = Arc::new(std::sync::Mutex::new(vec![]));
    let seen2 = seen.clone();
    *hooks.create.lock().unwrap() = Some(Arc::new(move |_g, name, image, role, reason| {
        seen2.lock().unwrap().push((name.clone(), image, role, reason));
        Box::pin(async move { Ok(EmojiInfo { id: "444444444444444444".into(), name, animated: false, managed: false, available: true, role_count: 0 }) })
    }));
    bot_core::router::route_interaction(&t, add("<:blob:333333333333333333>").build()).await;
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"]);
    let call = t.responses.calls().last().cloned().unwrap();
    assert_eq!(call.embed_title(), Some("✅  Emoji Added"));
    let desc = call.embed_description().unwrap();
    assert!(desc.contains("`blob` was taken, so it was named `blob_2`."), "{desc}");
    assert!(desc.contains("**Slots:** Static 2/50"), "{desc}");
    let (name, image, _, reason) = seen.lock().unwrap()[0].clone();
    assert_eq!(name, "blob_2");
    assert!(image.starts_with("data:image/png;base64,"));
    assert!(reason.ends_with(&format!("({ADMIN}) via /emoji add")));
    assert!(!t.ext::<UploadGuard>().is_busy(G));

    // A create that outlasts the timeout reports "queued" and keeps the guard until it settles.
    *hooks.create_timeout_ms.lock().unwrap() = Some(20);
    let release = Arc::new(tokio::sync::Notify::new());
    let r2 = release.clone();
    *hooks.create.lock().unwrap() = Some(Arc::new(move |_g, name, _i, _r, _re| {
        let r = r2.clone();
        Box::pin(async move {
            r.notified().await;
            Ok(EmojiInfo { id: "555555555555555555".into(), name, animated: false, managed: false, available: true, role_count: 0 })
        })
    }));
    bot_core::router::route_interaction(&t, add("<:fresh:666666666666666666>").build()).await;
    assert_eq!(t.responses.calls().last().unwrap().embed_title(), Some("⚠️  Upload Queued"));
    assert!(t.ext::<UploadGuard>().is_busy(G));
    bot_core::router::route_interaction(&t, add("<:again:777777777777777777>").build()).await;
    assert_eq!(t.responses.calls().last().unwrap().embed_title(), Some("❌  Upload In Progress"));
    release.notify_one();
    for _ in 0..100 {
        if !t.ext::<UploadGuard>().is_busy(G) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(!t.ext::<UploadGuard>().is_busy(G));

    // A file that is not an image answers publicly and releases the guard.
    *hooks.download.lock().unwrap() = Some(Arc::new(|_| Box::pin(async { Ok(b"<svg xmlns='x'>".to_vec()) })));
    bot_core::router::route_interaction(&t, add("<:svg:888888888888888888>").build()).await;
    assert_eq!(t.responses.calls().last().unwrap().embed_title(), Some("❌  Unsupported Image"));
    assert!(!t.ext::<UploadGuard>().is_busy(G));
    // Discord errors are mapped.
    *hooks.download.lock().unwrap() = Some(Arc::new(move |_| {
        let b = png.clone();
        Box::pin(async move { Ok(b) })
    }));
    *hooks.create.lock().unwrap() = Some(Arc::new(|_g, _n, _i, _r, _re| Box::pin(async { Err(EmojiError::Discord { code: Some(30008), message: "Max".into() }) })));
    bot_core::router::route_interaction(&t, add("<:full:999999999999999999>").build()).await;
    let call = t.responses.calls().last().cloned().unwrap();
    assert_eq!(call.embed_title(), Some("❌  Upload Failed"));
    assert_eq!(call.embed_description(), Some("This server has no free static emoji slots."));
}

#[tokio::test]
async fn delete_refuses_unknown_and_managed_emojis_and_autocomplete_lists_ids() {
    let t = app(Permissions::MANAGE_GUILD_EXPRESSIONS);
    let del = |q: &str| {
        FakeInteraction::slash("emoji").guild(G).user(ADMIN).roles(&[ADMIN_ROLE]).permissions(Permissions::ADMINISTRATOR).subcommand("delete").option("emoji", STRING, json!(q)).build()
    };
    bot_core::router::route_interaction(&t, del("nothing")).await;
    let call = t.responses.calls().last().cloned().unwrap();
    assert_eq!(call.embed_title(), Some("❌  Emoji Not Found"));
    assert!(call.ephemeral());
    bot_core::router::route_interaction(&t, del("twitch")).await;
    assert_eq!(t.responses.calls().last().unwrap().embed_title(), Some("❌  Managed Emoji"));
    let ac = FakeInteraction::autocomplete("emoji").guild(G).user(ADMIN).roles(&[ADMIN_ROLE]).permissions(Permissions::ADMINISTRATOR).subcommand("delete").focused("emoji", "bl").build();
    bot_core::router::route_interaction(&t, ac).await;
    let choices = t.responses.calls().last().unwrap().choices();
    assert_eq!(serde_json::to_value(choices).unwrap()[0]["value"], json!("111111111111111111"));
}

#[test]
fn name_from_url_decodes_like_decode_uri_component() {
    // decodeURIComponent throws on `%` not followed by two hex digits, and the raw segment is kept.
    let urls = ["https://example.com/ab%+1cd.png", "https://example.com/x%4g_y.png", "https://example.com/ok%5Fname.png"];
    let js = bot_core::testing::run_node("rust/fixtures/gen/feat-backups-audit.mjs", &[&["nameFromUrl"][..], &urls[..]].concat());
    for c in js.as_array().unwrap() {
        assert_eq!(json!(name_from_url(c["in"].as_str().unwrap())), c["out"], "nameFromUrl({})", c["in"]);
    }
}
