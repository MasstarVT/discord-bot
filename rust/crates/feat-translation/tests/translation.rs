//! Port of `test/translation-fixes.test.js` and `test/translate-stats.test.js` (the service parts), the golden checks
//! against JS-produced fixtures (`rust/fixtures/gen/feat-translation-golden.mjs`), and the database and Redis tests
//! in both cross-language directions (`rust/fixtures/gen/feat-translation-js.mjs`).
//!
//! Offline: every HTTP request goes to a fake transport that serves canned responses; nothing touches the network.

use std::sync::{Arc, Mutex};

use bot_core::clock::Clock;
use bot_core::command::option_type;
use bot_core::prelude::*;
use bot_core::router::route_interaction;
use bot_core::testing::{DbTestApp, FakeGuild, FakeInteraction, TEST_BOT_ID, TestApp, database_url, load_golden, offline_discord_as, redis_url, run_node};
use feat_translation::translate_stats_service as stats;
use feat_translation::translation_service::{
    self as svc, AutoCtx, AutoEmbedArgs, AutoMessage, ConfigOverride, ErrorCode, HttpRequest, HttpResponse, HttpTransport, Provider, ReplyError,
    TargetText, TransportError, TranslationError,
};
use futures::FutureExt;
use serde_json::{Value, json};

// ── Fake transport ──────────────────────────────────────────────────────────────

type Handler = Box<dyn Fn(&HttpRequest, usize) -> Result<HttpResponse, TransportError> + Send + Sync>;

struct Fake {
    calls: Mutex<Vec<HttpRequest>>,
    handler: Handler,
}

#[async_trait::async_trait]
impl HttpTransport for Fake {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportError> {
        let n = {
            let mut c = self.calls.lock().unwrap();
            c.push(req.clone());
            c.len()
        };
        (self.handler)(&req, n)
    }
}

impl Fake {
    fn calls(&self) -> Vec<HttpRequest> {
        self.calls.lock().unwrap().clone()
    }
    fn len(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
    fn hosts(&self) -> Vec<String> {
        self.calls().iter().map(|c| c.url.split("//").nth(1).unwrap_or("").split('/').next().unwrap_or("").to_string()).collect()
    }
}

fn install(app: &App, handler: impl Fn(&HttpRequest, usize) -> Result<HttpResponse, TransportError> + Send + Sync + 'static) -> Arc<Fake> {
    let fake = Arc::new(Fake { calls: Mutex::new(Vec::new()), handler: Box::new(handler) });
    svc::set_transport(app, fake.clone());
    fake
}

fn google(detected: &str, text: &str) -> impl Fn(&HttpRequest, usize) -> Result<HttpResponse, TransportError> + Send + Sync + 'static {
    let (d, t) = (detected.to_string(), text.to_string());
    move |_, _| Ok(HttpResponse::json(200, &json!([[[t, "S"]], null, d])))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 3 <= b.len()
            && let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `new URL(url).searchParams.get(name)`.
fn param(url: &str, name: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (k == name).then(|| percent_decode(v))
    })
}

/// The text a Google request carries: the `q` of a GET url or of a POST form body.
fn q_of(c: &HttpRequest) -> String {
    if c.method == "POST" { percent_decode(c.body.as_deref().unwrap_or("").trim_start_matches("q=")) } else { param(&c.url, "q").unwrap_or_default() }
}

fn google_app() -> TestApp {
    let t = TestApp::new();
    svc::reset(&t);
    svc::configure(&t, ConfigOverride { provider: Some(Provider::Google), result_cache: Some(false), ..Default::default() });
    t
}

fn lt_app(fallback: bool) -> TestApp {
    let t = TestApp::new();
    svc::reset(&t);
    svc::configure(
        &t,
        ConfigOverride {
            provider: Some(Provider::LibreTranslate),
            lt_url: Some("http://lt.test".into()),
            lt_key: Some(String::new()),
            lt_fallback_url: Some(if fallback { "http://lt-backup.test".into() } else { String::new() }),
            result_cache: Some(false),
        },
    );
    t
}

fn embed_json(e: &serenity::builder::CreateEmbed) -> Value {
    let mut v = serde_json::to_value(e).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.remove("timestamp");
        o.remove("type");
        if let Some(fields) = o.get_mut("fields").and_then(Value::as_array_mut) {
            for f in fields {
                if f.get("inline") == Some(&Value::Bool(false)) {
                    f.as_object_mut().map(|m| m.remove("inline"));
                }
            }
        }
    }
    v
}

/// The JS embed JSON (`toJSON()` minus timestamp), with `inline: false` dropped like ours.
fn js_embed(v: &Value) -> Value {
    let mut v = v.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("hasTimestamp");
        if let Some(fields) = o.get_mut("fields").and_then(Value::as_array_mut) {
            for f in fields {
                if f.get("inline") == Some(&Value::Bool(false)) {
                    f.as_object_mut().map(|m| m.remove("inline"));
                }
            }
        }
    }
    v
}

fn golden() -> Value {
    load_golden("feat-translation/golden.json")
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn opt_s(v: &Value) -> Option<String> {
    v.as_str().map(String::from)
}

// ── Golden: codes, names, text preparation ──────────────────────────────────────

#[tokio::test]
async fn golden_language_codes_and_names() {
    let g = golden();
    for case in g["normalizeLangCode"].as_array().unwrap() {
        let got = case[0].as_str().and_then(svc::normalize_lang_code);
        assert_eq!(got, opt_s(&case[1]), "normalizeLangCode({})", case[0]);
    }
    for case in g["resolveLanguageInput"].as_array().unwrap() {
        assert_eq!(svc::resolve_language_input(s(&case[0])).map(String::from), opt_s(&case[1]), "resolveLanguageInput({})", case[0]);
    }
    for case in g["suggestLanguage"].as_array().unwrap() {
        assert_eq!(svc::suggest_language(s(&case[0])).map(String::from), opt_s(&case[1]), "suggestLanguage({})", case[0]);
    }
    for case in g["languageName"].as_array().unwrap() {
        assert_eq!(svc::language_name(case[0].as_str()), s(&case[1]), "languageName({})", case[0]);
    }
    assert_eq!(svc::language_name(None), s(&g["languageNameNull"]));
    for case in g["langFlag"].as_array().unwrap() {
        assert_eq!(svc::lang_flag(s(&case[0])), s(&case[1]), "langFlag({})", case[0]);
    }
    for case in g["normalizeTargetList"].as_array().unwrap() {
        let inputs: Vec<String> = case[0].as_array().unwrap().iter().map(|v| s(v).to_string()).collect();
        assert_eq!(serde_json::to_value(svc::normalize_target_list(&inputs)).unwrap(), case[1], "{inputs:?}");
    }
    for case in g["toGoogleCode"].as_array().unwrap() {
        assert_eq!(svc::to_google_code(s(&case[0])), s(&case[1]));
    }
    for case in g["toLtCode"].as_array().unwrap() {
        let raw: std::collections::HashSet<String> = case[1].as_array().unwrap().iter().map(|v| s(v).to_string()).collect();
        assert_eq!(svc::to_lt_code(s(&case[0]), &raw), s(&case[2]), "{case}");
    }
    let t = google_app();
    let mut entries = svc::get_languages(&t);
    entries.sort_by(|a, b| bot_util::locale_compare(a.1, b.1));
    assert_eq!(json!(entries.iter().map(|(c, _)| *c).collect::<Vec<_>>()), g["languagesSorted"], "/translate languages order");
    for case in g["languageChoices"].as_array().unwrap() {
        assert_eq!(serde_json::to_value(svc::language_choices(&t, s(&case[0]))).unwrap(), case[1], "languageChoices({})", case[0]);
    }
}

#[test]
fn golden_text_preparation() {
    let g = golden();
    for case in g["text"].as_array().unwrap() {
        let input = s(&case["input"]);
        assert_eq!(svc::is_link_only(input), case["isLinkOnly"].as_bool().unwrap(), "isLinkOnly({input:?})");
        assert_eq!(svc::clean_for_translation(input), s(&case["clean"]), "clean({input:?})");
        assert_eq!(svc::has_translatable_text(input), case["translatable"].as_bool().unwrap(), "translatable({input:?})");
        let prepared = svc::prepare_auto_input_for(Provider::Google, input).map(|p| serde_json::to_value(p).unwrap()).unwrap_or(Value::Null);
        assert_eq!(prepared, case["prepared"], "prepareAutoInput({input:?})");
    }
    for case in g["isEcho"].as_array().unwrap() {
        assert_eq!(svc::is_echo(s(&case[0]), s(&case[1])), case[2].as_bool().unwrap(), "{case}");
    }
    for case in g["clampForRequest"].as_array().unwrap() {
        let out = svc::clamp_for_request(s(&case["text"]), case["maxCodePoints"].as_u64().unwrap() as usize, case["maxEncodedBytes"].as_u64().map(|n| n as usize));
        assert_eq!(serde_json::to_value(out).unwrap(), case["out"]);
    }
    for case in g["splitForRequests"].as_array().unwrap() {
        let out = svc::split_for_requests(s(&case["text"]), case["max"].as_u64().unwrap() as usize);
        assert_eq!(serde_json::to_value(out).unwrap(), case["out"], "splitForRequests max {}", case["max"]);
    }
    for case in g["splitForEmbeds"].as_array().unwrap() {
        let max = case["max"].as_u64().map(|n| n as usize).unwrap_or(svc::EMBED_PART_MAX);
        let parts = case["parts"].as_u64().map(|n| n as usize).unwrap_or(svc::EMBED_MAX_PARTS);
        assert_eq!(serde_json::to_value(svc::split_for_embeds(s(&case["text"]), max, parts)).unwrap(), case["out"]);
    }
    for case in g["balanceMarkdown"].as_array().unwrap() {
        assert_eq!(svc::balance_markdown(s(&case[0])), s(&case[1]), "{case}");
    }
    for case in g["formatTargetLine"].as_array().unwrap() {
        assert_eq!(svc::format_target_line(s(&case[0]), s(&case[1])), s(&case[2]));
    }
}

#[tokio::test]
async fn golden_auto_translate_embeds_and_errors() {
    let g = golden();
    for case in g["autoEmbed"].as_array().unwrap() {
        let args = &case["args"];
        let results: Vec<TargetText> =
            args["results"].as_array().unwrap().iter().map(|r| TargetText { target: s(&r["target"]).into(), text: s(&r["text"]).into() }).collect();
        let settings = args["settings"]["embedColor"].as_i64().map(|c| {
            bot_core::settings::MemorySettingsStore::new_row("900000000000000777", json!({ "embedColor": c }).as_object().unwrap().clone()).unwrap()
        });
        let embed = svc::build_auto_translate_embed(&AutoEmbedArgs {
            results: &results,
            detected: args["detected"].as_str(),
            truncated_input: args["truncatedInput"].as_bool().unwrap_or(false),
            had_spoiler: args["hadSpoiler"].as_bool().unwrap_or(false),
            paused_stop: args["pausedStop"].as_bool().unwrap_or(false),
            settings: settings.as_ref(),
        });
        assert_eq!(embed_json(&embed), js_embed(&case["embed"]));
        assert_eq!(case["embed"]["hasTimestamp"], false, "no timestamp, like the JS EmbedBuilder");
    }
    assert_eq!(g["autoReplyPayload"], json!({ "allowedMentions": { "parse": [], "repliedUser": false }, "failIfNotExists": true }));
    let payload = svc::build_auto_reply_payload(serenity::builder::CreateEmbed::new(), &bot_core::testing::fake_message("700000000000000001", "900000000000000001", "800000000000000001", "600000000000000001", "x"));
    let p = serde_json::to_value(&payload).unwrap();
    // serenity also writes empty `users` / `roles` lists, which Discord reads like absent ones.
    assert_eq!((p["allowed_mentions"]["parse"].clone(), p["allowed_mentions"]["replied_user"].clone()), (json!([]), json!(false)));
    assert_eq!(p["message_reference"]["fail_if_not_exists"], true);

    let t = google_app();
    t.clock.set(g["describeError"]["now"].as_i64().unwrap());
    for case in g["describeError"]["cases"].as_array().unwrap() {
        let provider = if s(&case["provider"]) == "google" { Provider::Google } else { Provider::LibreTranslate };
        svc::configure(&t, ConfigOverride { provider: Some(provider), ..Default::default() });
        let err = case["code"].as_str().map(|c| {
            let code = ErrorCode::parse(c).unwrap();
            let mut e = TranslationError::new(code, c);
            e.retry_after_ms = case["extra"]["retryAfterMs"].as_f64();
            e.lang = case["extra"]["lang"].as_str().map(String::from);
            e.status = case["extra"]["status"].as_u64().map(|n| n as u16);
            e
        });
        let d = svc::describe_translation_error(&t, err.as_ref());
        assert_eq!((d.level, d.log), (s(&case["level"]), s(&case["log"])), "{case}");
        assert_eq!(embed_json(&d.embed), js_embed(&case["embed"]), "{}", case["code"]);
    }
}

fn golden_responder(name: &str) -> impl Fn(&HttpRequest, usize) -> Result<HttpResponse, TransportError> + Send + Sync + 'static {
    let name = name.to_string();
    move |_, n| {
        Ok(match name.as_str() {
            "google get" => HttpResponse::json(200, &json!([[["It is hot", "S"]], null, "ja"])),
            "google post" => HttpResponse::json(200, &json!([[["translated", "S"]], null, "zh-CN"])),
            "google chunks" => HttpResponse::json(200, &json!([[[format!("T{n}"), "S"]], null, "zh-CN"])),
            "google segments" => HttpResponse::json(200, &json!([[["Hello ", "a"], ["world", "b"], [null, "c"], [5]], null, "iw"])),
            "google 429" => HttpResponse::text(429, "no").with_header("retry-after", "7200"),
            "google 500" => HttpResponse::text(500, "no"),
            "google bad json" => HttpResponse::text(200, "nope"),
            "google bad shape" => HttpResponse::json(200, &json!({ "a": 1 })),
            "google detect" => HttpResponse::json(200, &json!([[["T", "S"]], null, "en"])),
            "targets" => HttpResponse::json(200, &json!([[["T", "S"]], null, "ja"])),
            "lt translate" => HttpResponse::json(200, &json!({ "translatedText": "x", "detectedLanguage": { "language": "zh-Hant", "confidence": 80 } })),
            "lt unsupported" => HttpResponse::json(400, &json!({ "error": "zz is not supported as source language" })),
            "lt detect" => HttpResponse::json(200, &json!([{ "language": "nb", "confidence": 90 }])),
            "lt 429" => HttpResponse::text(429, "slow"),
            other => panic!("no responder for {other}"),
        })
    }
}

fn error_json(e: &TranslationError) -> Value {
    json!({
        "code": e.code.as_str(), "message": e.message, "status": e.status, "lang": e.lang,
        "retryAfterMs": e.retry_after_ms.map(bot_core::command::js_number),
    })
}

#[tokio::test]
async fn golden_requests_responses_and_cache_writes() {
    let g = golden();
    for case in g["requests"].as_array().unwrap() {
        let name = s(&case["name"]);
        let cfg = &case["config"];
        let t = TestApp::new();
        svc::reset(&t);
        svc::configure(
            &t,
            ConfigOverride {
                provider: Some(if s(&cfg["provider"]) == "google" { Provider::Google } else { Provider::LibreTranslate }),
                lt_url: cfg["ltUrl"].as_str().map(String::from),
                lt_key: Some(cfg["ltKey"].as_str().unwrap_or("").to_string()),
                lt_fallback_url: Some(String::new()),
                result_cache: cfg["resultCache"].as_bool(),
            },
        );
        let fake = install(&t, golden_responder(name));
        let (result, error): (Value, Value) = match name {
            "google get" => wrap(svc::translate_text(&t, "今日はとても暑いですね", "en", "auto", 1).await),
            "google post" => wrap(svc::translate_text(&t, &"好".repeat(1500), "en", "auto", 4).await),
            "google chunks" => wrap(svc::translate_text(&t, &"好".repeat(3000), "en", "auto", 4).await),
            "google segments" => wrap(svc::translate_text(&t, "שלום עולם", "zh-tw", "he", 1).await),
            "google 429" | "google 500" | "google bad json" | "google bad shape" => {
                wrap(svc::translate_text(&t, if name == "google 429" { "hello there" } else { "hello" }, "es", "auto", 1).await)
            }
            "google detect" => wrap(svc::detect_language(&t, "今天天氣很好，我們去公園玩吧 https://x.y").await),
            "targets" => (serde_json::to_value(svc::translate_to_targets(&t, "今日はとても暑いですね", &["ja", "en", "es"], None).await).unwrap(), Value::Null),
            "lt translate" => wrap(svc::translate_text(&t, "hi", "no", "auto", 1).await),
            "lt unsupported" => wrap(svc::translate_text(&t, "hi", "cy", "ja", 1).await),
            "lt detect" => wrap(svc::detect_language(&t, "Dette er en norsk setning").await),
            "lt 429" => wrap(svc::translate_text(&t, "hi", "es", "auto", 1).await),
            other => panic!("unknown case {other}"),
        };
        assert_eq!(result, case["result"], "{name}: result");
        assert_eq!(error, case["error"], "{name}: error");
        let calls: Vec<Value> = fake
            .calls()
            .iter()
            .map(|c| {
                let headers: serde_json::Map<String, Value> = c.headers.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
                json!({ "url": c.url, "method": c.method, "headers": headers, "body": c.body })
            })
            .collect();
        assert_eq!(Value::Array(calls), case["calls"], "{name}: requests");
        let mut keys = t.cache_backend.keys();
        keys.sort();
        let mut want: Vec<&str> = case["cache"].as_array().unwrap().iter().map(|e| s(&e["key"])).collect();
        want.sort();
        assert_eq!(keys, want, "{name}: cache keys");
        for entry in case["cache"].as_array().unwrap() {
            let key = s(&entry["key"]);
            let raw: Value = serde_json::from_str(&t.cache_backend.raw(key).unwrap()).unwrap();
            if key.ends_with(":pausedUntil") {
                let pause = case["error"]["retryAfterMs"].as_f64().unwrap();
                assert_eq!(raw.as_f64(), Some(t.clock.now_ms() as f64 + pause), "{name}: the pause ends now + {pause} ms");
                assert!(raw.is_i64() || raw.is_u64(), "written as an integer, like JSON.stringify");
            } else {
                assert_eq!(raw, entry["value"], "{name}: {key}");
            }
            assert_eq!(t.cache_backend.pttl(key), entry["ttl"].as_i64().unwrap() * 1000, "{name}: ttl of {key}");
        }
        assert_eq!(svc::translation_paused(&t), case["paused"].as_bool().unwrap(), "{name}: paused");
    }
}

fn wrap<T: serde::Serialize>(r: Result<T, TranslationError>) -> (Value, Value) {
    match r {
        Ok(v) => (serde_json::to_value(v).unwrap(), Value::Null),
        Err(e) => (Value::Null, error_json(&e)),
    }
}

#[test]
fn golden_stat_keys_and_raw_sql() {
    let g = golden();
    for case in g["statKeys"].as_array().unwrap() {
        let keys: Vec<Value> = stats::stat_keys_values(&case[0], &case[1]).into_iter().map(|(d, l)| json!([d, l])).collect();
        assert_eq!(Value::Array(keys), case[2], "statKeys({}, {})", case[0], case[1]);
    }
    let ours: Vec<svc::RawSql> = svc::raw_sql().into_iter().chain(stats::raw_sql()).collect();
    let js = g["rawSql"].as_array().unwrap();
    assert_eq!(ours.len(), js.len());
    for (o, j) in ours.iter().zip(js) {
        assert_eq!(o.name, s(&j["name"]));
        // Prisma's `sql` text has `?` placeholders; ours are numbered.
        let mut n = 0;
        let js_sql: String = s(&j["sql"]).chars().map(|c| if c == '?' { n += 1; format!("${n}") } else { c.to_string() }).collect();
        assert_eq!(o.sql, js_sql, "{}: same statement text", o.name);
        assert_eq!(Value::Array(o.values.clone()), j["values"], "{}: same values", o.name);
    }
}

// ── Ported: test/translation-fixes.test.js ───────────────────────────────────────

#[test]
fn link_only_check_is_linear_on_hostile_input() {
    for input in [format!("{} x", "http://".repeat(5000)), format!("{} !", "http://a".repeat(500))] {
        let t0 = std::time::Instant::now();
        assert!(!svc::is_link_only(&input));
        assert!(t0.elapsed().as_millis() < 500, "took too long");
    }
}

#[tokio::test]
async fn language_codes_and_registry() {
    assert_eq!(svc::LANGUAGES.len(), 74);
    assert_eq!(svc::language_name(Some("ceb")), "Cebuano");
    assert_eq!(svc::resolve_language_input("jp"), None);
    assert_eq!(svc::suggest_language("jp"), Some("ja"));
    assert_eq!(svc::lang_flag("zh-tw"), "🇹🇼");
    let list = svc::normalize_target_list(&["es", "ES", " es ", "iw", "he", "xx"]);
    assert_eq!((list.valid, list.invalid), (vec!["es".to_string(), "he".to_string()], vec!["xx".to_string()]));
    let t = google_app();
    let zh: Vec<String> = svc::language_choices(&t, "zh").into_iter().map(|c| c.value).collect();
    assert_eq!(&zh[..2], &["zh".to_string(), "zh-tw".to_string()]);
    assert!(svc::language_choices(&t, "trad").iter().any(|c| c.value == "zh-tw"));
    assert_eq!(svc::language_choices(&t, "").len(), 25);
    assert_eq!(svc::language_choices(&t, "iw")[0].value, "he");
}

#[test]
fn request_sizing_is_lossless_and_bounded() {
    let input = "今日はとても暑いですね。".repeat(125);
    let pieces = svc::split_for_requests(&input, 6000);
    assert!(pieces.len() >= 2);
    for p in &pieces {
        assert!(bot_util::js::encoded_len(&p.text) <= 6000);
    }
    assert_eq!(pieces.iter().map(|p| format!("{}{}", p.text, p.sep)).collect::<String>(), input);
    let emoji = svc::split_for_requests(&"😀".repeat(1500), 6000);
    assert_eq!(emoji.iter().map(|p| p.text.as_str()).collect::<String>(), "😀".repeat(1500));
    let r = svc::clamp_for_request(&"Привет как дела ".repeat(125), 1000, Some(6000));
    assert!(r.truncated && bot_util::js::encoded_len(&r.text) <= 6000 && r.text.chars().count() <= 1000);
    let short = svc::clamp_for_request("short", 1000, None);
    assert_eq!((short.text.as_str(), short.truncated), ("short", false));
    assert_eq!(svc::split_for_embeds(&"x".repeat(4096), 4000, 3).iter().map(|p| p.len()).collect::<Vec<_>>(), vec![4000, 96]);
    let words = svc::split_for_embeds(&"word ".repeat(3000), 4000, 3);
    assert_eq!(words.len(), 3);
    assert!(words[2].ends_with('…'));
}

#[test]
fn auto_translate_embed_stays_inside_discord_limits() {
    let long = "palabra ".repeat(400);
    let results: Vec<TargetText> = ["es", "fr", "de", "it", "pt"].iter().map(|t| TargetText { target: t.to_string(), text: long.clone() }).collect();
    let e = embed_json(&svc::build_auto_translate_embed(&AutoEmbedArgs { results: &results, detected: Some("en"), ..Default::default() }));
    let d = s(&e["description"]);
    assert!(d.encode_utf16().count() <= 4096);
    for line in d.split('\n') {
        assert!(line.ends_with('…'), "{line}");
    }
    assert_eq!(e["footer"]["text"], "Detected: English");
    let spoiler = [TargetText { target: "es".into(), text: "hola **mundo".into() }, TargetText { target: "fr".into(), text: "bonjour".into() }];
    let e = embed_json(&svc::build_auto_translate_embed(&AutoEmbedArgs { results: &spoiler, detected: Some("en"), had_spoiler: true, ..Default::default() }));
    for line in s(&e["description"]).split('\n') {
        let body = line.split(" — ").nth(1).unwrap();
        assert!(body.starts_with("||") && body.ends_with("||"), "{line}");
    }
}

const JAPANESE: &str = "今日はとても暑いですね";

#[tokio::test]
async fn japanese_en_ja_is_one_request_with_sl_auto() {
    let t = google_app();
    let fake = install(&t, google("ja", "T"));
    let out = svc::translate_to_targets(&t, JAPANESE, &["en", "ja"], None).await;
    assert_eq!(fake.len(), 1);
    let c = &fake.calls()[0];
    assert_eq!((param(&c.url, "sl").as_deref(), param(&c.url, "tl").as_deref()), (Some("auto"), Some("en")));
    assert_eq!(out.results.iter().map(|r| r.target.as_str()).collect::<Vec<_>>(), vec!["en"]);
    assert_eq!(out.detected.as_deref(), Some("ja"));
}

#[tokio::test]
async fn japanese_ja_en_drops_the_ja_line_and_reuses_the_source() {
    let t = google_app();
    let fake = install(&t, google("ja", "T"));
    let out = svc::translate_to_targets(&t, JAPANESE, &["ja", "en"], None).await;
    let calls = fake.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!((param(&calls[0].url, "sl").as_deref(), param(&calls[0].url, "tl").as_deref()), (Some("auto"), Some("ja")));
    assert_eq!((param(&calls[1].url, "sl").as_deref(), param(&calls[1].url, "tl").as_deref()), (Some("ja"), Some("en")));
    assert_eq!(out.results.iter().map(|r| r.target.as_str()).collect::<Vec<_>>(), vec!["en"]);
    assert!(calls.iter().all(|c| !c.url.contains("sl=zh-CN")));
}

#[tokio::test]
async fn hebrew_iw_traditional_chinese_and_spanish() {
    let t = google_app();
    let fake = install(&t, google("iw", "T"));
    let out = svc::translate_to_targets(&t, "שלום לכולם", &["he", "en"], None).await;
    assert_eq!(fake.len(), 2);
    assert_eq!(out.detected.as_deref(), Some("he"));
    assert_eq!(param(&fake.calls()[1].url, "sl").as_deref(), Some("he"));

    let fake = install(&t, google("es", "T"));
    let out = svc::translate_to_targets(&t, "hola a todos", &["en"], None).await;
    assert_eq!((fake.len(), out.results.len()), (1, 1));

    let fake = install(&t, google("zh-TW", "T"));
    let out = svc::translate_to_targets(&t, "今天天氣很好", &["zh-tw", "en"], None).await;
    assert_eq!(param(&fake.calls()[0].url, "tl").as_deref(), Some("zh-TW"));
    assert_eq!(out.detected.as_deref(), Some("zh-tw"));
    assert_eq!(param(&fake.calls()[1].url, "sl").as_deref(), Some("zh-TW"));

    // Google calling Traditional Chinese "en" does not drop the English target (seen live).
    let fake = install(&t, google("en", "T"));
    let out = svc::translate_to_targets(&t, "今天天氣很好，我們去公園玩吧", &["en", "es"], None).await;
    assert_eq!(fake.len(), 2);
    assert_eq!(out.results.iter().map(|r| r.target.as_str()).collect::<Vec<_>>(), vec!["en", "es"]);
    assert_eq!(out.detected.as_deref(), Some("zh-tw"));
    assert_eq!(param(&fake.calls()[1].url, "sl").as_deref(), Some("zh-TW"));
}

#[tokio::test]
async fn an_impossible_detection_is_rejected_and_an_echo_dropped() {
    let t = google_app();
    install(&t, google("en", "T"));
    assert_eq!(svc::translate_text(&t, "Привет всем", "es", "auto", 1).await.unwrap().detected, None);
    assert_eq!(svc::translate_text(&t, "hello there", "es", "auto", 1).await.unwrap().detected.as_deref(), Some("en"));
    assert_eq!(svc::translate_text(&t, "I love 寿司", "es", "auto", 1).await.unwrap().detected.as_deref(), Some("en"));
    assert_eq!(svc::detect_language(&t, "今天天氣很好，我們去公園玩吧").await.unwrap().code.as_deref(), Some("zh-tw"));

    install(&t, |req, _| Ok(HttpResponse::json(200, &json!([[[q_of(req), "S"]], null, "en"]))));
    assert!(svc::translate_to_targets(&t, "same text here", &["es"], None).await.results.is_empty());
}

#[tokio::test]
async fn circuit_breaker_trips_on_429_and_later_calls_make_no_request() {
    let t = google_app();
    let fake = install(&t, |_, _| Ok(HttpResponse::text(429, "<html>Sorry...</html>")));
    let err = svc::translate_text(&t, "hello there", "es", "auto", 1).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::RateLimited);
    assert_eq!(fake.len(), 1, "one request, then the breaker trips");
    assert!(fake.calls()[0].url.contains("client=gtx"));
    assert!(svc::translation_paused(&t));
    assert!(svc::pause_remaining_ms(&t) > 0.0);
    let err = svc::translate_text(&t, "hello there", "es", "auto", 1).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Paused);
    assert_eq!(fake.len(), 1);
    assert_eq!(embed_json(&svc::describe_translation_error(&t, Some(&err)).embed)["title"], "⚠️  Translation Paused");

    let t = google_app();
    install(&t, |_, _| Ok(HttpResponse::text(429, "")));
    let out = svc::translate_to_targets(&t, "hello there", &["es", "fr"], None).await;
    assert!(out.paused_stop && out.results.is_empty());

    // The pause is shared through Redis: another shard (app) with the same cache would read it at its next sync.
    let d = |c| svc::describe_translation_error(&t, Some(&TranslationError::of(c)));
    assert_eq!(embed_json(&d(ErrorCode::UnsupportedLanguage).embed)["title"], "❌  Language Not Supported");
    assert_eq!(embed_json(&d(ErrorCode::Timeout).embed)["title"], "❌  Translation Failed");
    assert_eq!(d(ErrorCode::Network).log, "warn");
    assert_eq!(svc::describe_translation_error(&t, None).log, "error");
}

#[tokio::test]
async fn the_shared_pause_is_read_from_the_cache() {
    let t = google_app();
    let until = t.clock.now_ms() + 120_000;
    t.cache().set("translate:google:pausedUntil", &until, 120).await;
    let fake = install(&t, google("en", "T"));
    let err = svc::translate_text(&t, "hello there", "es", "auto", 1).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Paused);
    assert_eq!(fake.len(), 0);
}

#[tokio::test]
async fn long_input_get_post_and_chunks() {
    let t = google_app();
    let fake = install(&t, google("ja", "T"));
    svc::translate_text(&t, JAPANESE, "en", "auto", 1).await.unwrap();
    let c = &fake.calls()[0];
    assert_eq!(c.method, "GET");
    assert_eq!(param(&c.url, "q").as_deref(), Some(JAPANESE));

    let input = "好".repeat(1500);
    let fake = install(&t, google("zh-CN", "translated"));
    let r = svc::translate_text(&t, &input, "en", "auto", svc::TRANSLATE_MAX_CHUNKS).await.unwrap();
    let c = &fake.calls()[0];
    assert_eq!((fake.len(), c.method), (1, "POST"));
    assert!(c.url.len() < 200, "the URL carries no text");
    assert_eq!(q_of(c), input);
    assert!(c.headers.iter().any(|(k, v)| k == "Content-Type" && v.contains("x-www-form-urlencoded")));
    assert_eq!((r.text.as_str(), r.detected.as_deref()), ("translated", Some("zh")));

    let fake = install(&t, |_, _| Ok(HttpResponse::text(400, "bad")));
    let err = svc::translate_text(&t, &input, "en", "auto", 1).await.unwrap_err();
    assert_eq!((err.code, fake.len()), (ErrorCode::HttpError, 1));

    let big = "好".repeat(3000);
    let fake = install(&t, |_, n| Ok(HttpResponse::json(200, &json!([[[format!("T{n}"), "S"]], null, "zh-CN"]))));
    let r = svc::translate_text(&t, &big, "en", "auto", svc::TRANSLATE_MAX_CHUNKS).await.unwrap();
    let n = fake.len();
    assert!((2..=svc::TRANSLATE_MAX_CHUNKS).contains(&n));
    assert_eq!(r.text, (1..=n).map(|i| format!("T{i}")).collect::<String>());
    let err = svc::translate_text(&t, &big, "en", "auto", 1).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::TooLong);
    assert_eq!(fake.len(), n);
}

#[tokio::test]
async fn result_cache_serves_a_repeat_and_keeps_the_js_shape() {
    let t = google_app();
    svc::configure(&t, ConfigOverride { result_cache: Some(true), ..Default::default() });
    let fake = install(&t, google("es", "hello everyone"));
    let a = svc::translate_text(&t, "  hola a todos ", "en", "auto", 1).await.unwrap();
    let b = svc::translate_text(&t, "hola a todos", "en", "auto", 1).await.unwrap();
    assert_eq!(a, b);
    assert_eq!(fake.len(), 1, "the second call is a cache hit (the key hashes the trimmed text)");
    let key = svc::result_key(Provider::Google, "en", "hola a todos");
    assert_eq!(serde_json::from_str::<Value>(&t.cache_backend.raw(&key).unwrap()).unwrap(), json!({ "t": "hello everyone", "d": "es" }));
    // A value the JS wrote with a null detection reads back.
    t.cache_backend.put_raw(&svc::result_key(Provider::Google, "en", "x y"), r#"{"t":"X Y","d":null}"#, Some(60_000));
    let r = svc::translate_text(&t, "x y", "en", "auto", 1).await.unwrap();
    assert_eq!((r.text.as_str(), r.detected), ("X Y", None));
    assert_eq!(fake.len(), 1);
}

// ── LibreTranslate ────────────────────────────────────────────────────────────

#[tokio::test]
async fn libretranslate_language_list_drives_support_and_code_mapping() {
    let t = lt_app(false);
    let posts: Arc<Mutex<Vec<Value>>> = Arc::default();
    let p = posts.clone();
    let fake = install(&t, move |req, _| {
        if req.url.ends_with("/languages") {
            return Ok(HttpResponse::json(200, &json!([{ "code": "en" }, { "code": "nb" }, { "code": "zh-Hans" }, { "code": "zh-Hant" }, { "code": "he" }])));
        }
        p.lock().unwrap().push(serde_json::from_str(req.body.as_deref().unwrap()).unwrap());
        Ok(HttpResponse::json(200, &json!({ "translatedText": "x", "detectedLanguage": { "language": "zh-Hant" } })))
    });
    let mut support = svc::load_language_support(&t, false).await.unwrap();
    support.sort();
    assert_eq!(support, vec!["en", "he", "no", "zh", "zh-tw"]);
    assert_eq!(svc::is_supported_target(&t, "no"), Some(true));
    assert_eq!(svc::is_supported_target(&t, "cy"), Some(false));
    let mut langs: Vec<&str> = svc::get_languages(&t).into_iter().map(|(c, _)| c).collect();
    langs.sort();
    assert_eq!(langs, vec!["en", "he", "no", "zh", "zh-tw"]);

    let r = svc::translate_text(&t, "hi", "no", "auto", 1).await.unwrap();
    let first = posts.lock().unwrap()[0].clone();
    assert_eq!((first["target"].clone(), first["source"].clone()), (json!("nb"), json!("auto")));
    assert_eq!(r.detected.as_deref(), Some("zh-tw"));
    svc::translate_text(&t, "hi", "zh-tw", "zh-tw", 1).await.unwrap();
    let second = posts.lock().unwrap()[1].clone();
    assert_eq!((second["target"].clone(), second["source"].clone()), (json!("zh-Hant"), json!("zh-Hant")));

    let before = fake.len();
    let err = svc::translate_text(&t, "hi", "cy", "auto", 1).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::UnsupportedLanguage);
    assert_eq!(fake.len(), before, "no request for an unsupported language");
    assert_eq!(svc::provider_label(&t), "LibreTranslate");
}

#[tokio::test]
async fn libretranslate_failing_list_and_runtime_rejection() {
    let t = lt_app(false);
    install(&t, |req, _| {
        Ok(if req.url.ends_with("/languages") { HttpResponse::text(500, "down") } else { HttpResponse::json(400, &json!({ "error": "cy is not supported" })) })
    });
    assert_eq!(svc::load_language_support(&t, false).await, None);
    assert_eq!(svc::is_supported_target(&t, "cy"), None);
    assert_eq!(svc::get_languages(&t).len(), 74);
    let err = svc::translate_text(&t, "hi", "cy", "auto", 1).await.unwrap_err();
    assert_eq!((err.code, err.lang.as_deref()), (ErrorCode::UnsupportedLanguage, Some("cy")));
    assert_eq!(svc::is_supported_target(&t, "cy"), Some(false));
}

#[tokio::test]
async fn libretranslate_api_key_and_detect() {
    let t = lt_app(false);
    svc::configure(&t, ConfigOverride { lt_key: Some("secret".into()), ..Default::default() });
    let fake = install(&t, |_, _| Ok(HttpResponse::json(200, &json!([{ "language": "nb", "confidence": 90 }]))));
    let found = svc::detect_language(&t, "Dette er en norsk setning").await.unwrap();
    assert_eq!((found.code.as_deref(), found.name.as_str()), (Some("no"), "Norwegian"));
    let c = &fake.calls()[0];
    assert_eq!(serde_json::from_str::<Value>(c.body.as_deref().unwrap()).unwrap()["api_key"], "secret");
    assert_eq!(c.url, "http://lt.test/detect");

    let t = lt_app(false);
    install(&t, |_, _| Ok(HttpResponse::json(200, &json!([{ "language": "en", "confidence": 0 }]))));
    let found = svc::detect_language(&t, "Guten Morgen, wie geht es dir?").await.unwrap();
    assert_eq!((found.code, found.name.as_str()), (None, "Unknown"));
    install(&t, |_, _| Ok(HttpResponse::json(200, &json!([{ "language": "en", "confidence": 97 }]))));
    assert_eq!(svc::detect_language(&t, "Good morning, how are you?").await.unwrap().code.as_deref(), Some("en"));
}

fn lt_ok() -> Result<HttpResponse, TransportError> {
    Ok(HttpResponse::json(200, &json!({ "translatedText": "hello", "detectedLanguage": { "language": "es" } })))
}

fn main_down(req: &HttpRequest, _: usize) -> Result<HttpResponse, TransportError> {
    if req.url.starts_with("http://lt.test") { Err(TransportError::Network("fetch failed".into())) } else { lt_ok() }
}

#[tokio::test]
async fn fallback_on_network_error_and_main_skipped_until_the_window_ends() {
    let t = lt_app(true);
    let fake = install(&t, main_down);
    assert_eq!(svc::translate_text(&t, "hola", "en", "auto", 1).await.unwrap().text, "hello");
    assert_eq!(fake.hosts(), vec!["lt.test", "lt-backup.test"]);
    let calls = fake.calls();
    assert_eq!(calls[1].body, calls[0].body, "same request");
    assert!(calls[1].url.ends_with("/translate"));
    svc::translate_text(&t, "buenos dias", "en", "auto", 1).await.unwrap();
    assert_eq!(&fake.hosts()[2..], &["lt-backup.test".to_string()], "main server skipped");

    t.clock.advance(svc::LT_FAILOVER_MS + 1);
    let fake = install(&t, |_, _| lt_ok());
    svc::translate_text(&t, "hola amigo", "en", "auto", 1).await.unwrap();
    assert_eq!(fake.hosts(), vec!["lt.test"]);
    svc::translate_text(&t, "hola amigos", "en", "auto", 1).await.unwrap();
    assert_eq!(fake.hosts(), vec!["lt.test", "lt.test"], "stays on the main server once it answers");
}

#[tokio::test]
async fn timeout_and_gateway_errors_fail_over_but_others_do_not() {
    let fails: Vec<Box<dyn Fn() -> Result<HttpResponse, TransportError> + Send + Sync>> = vec![
        Box::new(|| Err(TransportError::Timeout)),
        Box::new(|| Ok(HttpResponse::text(502, "bad gateway"))),
        Box::new(|| Ok(HttpResponse::text(503, "unavailable"))),
        Box::new(|| Ok(HttpResponse::text(504, "gateway timeout"))),
    ];
    for fail in fails {
        let t = lt_app(true);
        let fake = install(&t, move |req, _| if req.url.starts_with("http://lt.test") { fail() } else { lt_ok() });
        assert_eq!(svc::translate_text(&t, "hola", "en", "auto", 1).await.unwrap().text, "hello");
        assert_eq!(fake.hosts(), vec!["lt.test", "lt-backup.test"]);
    }

    let t = lt_app(true);
    let fake = install(&t, |_, _| Ok(HttpResponse::json(400, &json!({ "error": "cy is not supported" }))));
    assert_eq!(svc::translate_text(&t, "hi", "cy", "auto", 1).await.unwrap_err().code, ErrorCode::UnsupportedLanguage);
    assert_eq!(fake.hosts(), vec!["lt.test"]);
    let fake = install(&t, |_, _| Ok(HttpResponse::text(500, "boom")));
    let err = svc::translate_text(&t, "hi", "es", "auto", 1).await.unwrap_err();
    assert_eq!((err.code, err.status), (ErrorCode::HttpError, Some(500)));
    assert_eq!(fake.hosts(), vec!["lt.test"]);
    let fake = install(&t, |_, _| Ok(HttpResponse::text(429, "slow down")));
    assert_eq!(svc::translate_text(&t, "hi", "es", "auto", 1).await.unwrap_err().code, ErrorCode::RateLimited);
    assert_eq!(fake.hosts(), vec!["lt.test"]);
    assert!(svc::translation_paused(&t));

    // Without a fallback, a network error fails as before; both down fails with the fallback error.
    let t = lt_app(false);
    let fake = install(&t, main_down);
    assert_eq!(svc::translate_text(&t, "hola", "en", "auto", 1).await.unwrap_err().code, ErrorCode::Network);
    assert_eq!(fake.hosts(), vec!["lt.test"]);
    let t = lt_app(true);
    let fake = install(&t, |_, _| Err(TransportError::Network("fetch failed".into())));
    assert_eq!(svc::translate_text(&t, "hola", "en", "auto", 1).await.unwrap_err().code, ErrorCode::Network);
    assert_eq!(fake.hosts(), vec!["lt.test", "lt-backup.test"]);
}

#[tokio::test]
async fn fallback_lacking_a_language_fails_that_request_only() {
    let t = lt_app(true);
    let fake = install(&t, |req, _| {
        if req.url.starts_with("http://lt.test") {
            Err(TransportError::Network("fetch failed".into()))
        } else {
            Ok(HttpResponse::json(400, &json!({ "error": "ko is not supported" })))
        }
    });
    let err = svc::translate_text(&t, "hello", "ko", "auto", 1).await.unwrap_err();
    assert_eq!((err.code, err.lang.as_deref()), (ErrorCode::HttpError, Some("ko")));
    assert_ne!(svc::is_supported_target(&t, "ko"), Some(false), "not remembered as unsupported");
    assert_eq!(fake.hosts(), vec!["lt.test", "lt-backup.test"]);
    t.clock.advance(svc::LT_FAILOVER_MS + 1);
    let fake = install(&t, |_, _| Ok(HttpResponse::json(200, &json!({ "translatedText": "annyeong", "detectedLanguage": { "language": "en", "confidence": 90 } }))));
    assert_eq!(svc::translate_text(&t, "hello", "ko", "auto", 1).await.unwrap().text, "annyeong");
    assert_eq!(fake.hosts(), vec!["lt.test"]);
}

#[tokio::test]
async fn fallback_language_lists() {
    // The list loads from the fallback while the main server is down.
    let t = lt_app(true);
    let fake = install(&t, |req, _| {
        if req.url.starts_with("http://lt.test") { Err(TransportError::Network("fetch failed".into())) } else { Ok(HttpResponse::json(200, &json!([{ "code": "en" }, { "code": "es" }, { "code": "ja" }]))) }
    });
    let mut s1 = svc::load_language_support(&t, false).await.unwrap();
    s1.sort();
    assert_eq!(s1, vec!["en", "es", "ja"]);
    assert_eq!(fake.calls().iter().map(|c| c.url.clone()).collect::<Vec<_>>(), vec!["http://lt.test/languages", "http://lt-backup.test/languages"]);

    // A fallback list never replaces the main server's list.
    let t = lt_app(true);
    install(&t, |_, _| Ok(HttpResponse::json(200, &json!([{ "code": "en" }, { "code": "es" }, { "code": "ko" }]))));
    svc::load_language_support(&t, false).await;
    let fake = install(&t, |req, _| {
        if req.url.starts_with("http://lt.test") { Err(TransportError::Network("fetch failed".into())) } else { Ok(HttpResponse::json(200, &json!([{ "code": "en" }, { "code": "es" }]))) }
    });
    let mut s2 = svc::load_language_support(&t, true).await.unwrap();
    s2.sort();
    assert_eq!(fake.hosts(), vec!["lt.test", "lt-backup.test"]);
    assert_eq!(s2, vec!["en", "es", "ko"]);
    assert_eq!(svc::is_supported_target(&t, "ko"), Some(true));

    // A list only the fallback gave is re-fetched after LT_LANGUAGES_RETRY_MS.
    let t = lt_app(true);
    install(&t, |req, _| {
        if req.url.starts_with("http://lt.test") { Err(TransportError::Network("fetch failed".into())) } else { Ok(HttpResponse::json(200, &json!([{ "code": "en" }, { "code": "es" }]))) }
    });
    svc::load_language_support(&t, false).await;
    assert_eq!(svc::is_supported_target(&t, "ko"), Some(false), "during the outage");
    t.clock.advance(svc::LT_LANGUAGES_RETRY_MS.max(svc::LT_FAILOVER_MS) + 1);
    let fake = install(&t, |_, _| Ok(HttpResponse::json(200, &json!([{ "code": "en" }, { "code": "es" }, { "code": "ko" }]))));
    let mut s3 = svc::load_language_support(&t, false).await.unwrap();
    s3.sort();
    assert_eq!(s3, vec!["en", "es", "ko"]);
    assert_eq!(fake.hosts(), vec!["lt.test"]);
}

#[tokio::test]
async fn concurrent_list_loads_share_one_request() {
    let t = lt_app(false);
    let fake = install(&t, |_, _| Ok(HttpResponse::json(200, &json!([{ "code": "en" }]))));
    let (a, b) = tokio::join!(svc::load_language_support(&t, false), svc::load_language_support(&t, false));
    assert_eq!((a.clone(), b), (Some(vec!["en".to_string()]), Some(vec!["en".to_string()])));
    assert_eq!(fake.len(), 1);
}

#[tokio::test]
async fn answers_from_the_fallback_are_not_cached() {
    let t = lt_app(true);
    svc::configure(&t, ConfigOverride { result_cache: Some(true), ..Default::default() });
    install(&t, main_down);
    svc::translate_text(&t, "hola", "en", "auto", 1).await.unwrap();
    assert!(t.cache_backend.keys().iter().all(|k| !k.starts_with("tr:")));
    let t = lt_app(true);
    svc::configure(&t, ConfigOverride { result_cache: Some(true), ..Default::default() });
    install(&t, |_, _| lt_ok());
    svc::translate_text(&t, "hola", "en", "auto", 1).await.unwrap();
    assert_eq!(t.cache_backend.keys().iter().filter(|k| k.starts_with("tr:")).count(), 1, "answers from the main server still are");
}

// ── handleAutoTranslate ───────────────────────────────────────────────────────

struct Run {
    replies: Arc<Mutex<Vec<Value>>>,
}

fn message(id: &str, content: &str) -> AutoMessage {
    AutoMessage { id: id.into(), channel_id: "c1".into(), content: content.into(), ..Default::default() }
}

async fn run_auto(t: &TestApp, msg: &AutoMessage, automod_deleted: bool, check_permissions: bool, fail: Option<u64>) -> Run {
    let replies: Arc<Mutex<Vec<Value>>> = Arc::default();
    let r = replies.clone();
    let ctx = AutoCtx { guild_id: "g1", message: msg, automod_deleted, settings: async { None }.boxed(), check_permissions };
    svc::handle_auto_translate(t, ctx, move |embed| async move {
        if let Some(code) = fail {
            return Err(ReplyError { code: Some(code), message: "Unknown Message".into() });
        }
        r.lock().unwrap().push(embed_json(&embed));
        Ok(())
    })
    .await;
    Run { replies }
}

fn auto_app() -> TestApp {
    let t = google_app();
    svc::prime_channel_config(&t, "g1", &[("c1", &["es", "en"])]);
    t
}

#[tokio::test]
async fn auto_translate_one_request_one_reply() {
    let t = auto_app();
    let fake = install(&t, google("en", "Buenos días a todos"));
    let run = run_auto(&t, &message("m1", "good morning everyone"), false, false, None).await;
    assert_eq!(fake.len(), 1);
    assert_eq!(param(&fake.calls()[0].url, "tl").as_deref(), Some("es"));
    let replies = run.replies.lock().unwrap().clone();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["footer"]["text"], "Detected: English");
    assert!(s(&replies[0]["description"]).contains("Buenos días a todos"));
    assert_eq!(svc::auto_inflight(&t), 0, "the in-flight slot is released");
}

#[tokio::test]
async fn auto_translate_same_language_target_gets_no_reply() {
    let t = google_app();
    svc::prime_channel_config(&t, "g1", &[("c1", &["es"])]);
    let fake = install(&t, google("es", "T"));
    let run = run_auto(&t, &message("m2", "hola a todos, buenos días"), false, false, None).await;
    assert_eq!(fake.len(), 1);
    assert!(run.replies.lock().unwrap().is_empty());
}

#[tokio::test]
async fn auto_translate_skips() {
    let cases: Vec<(&str, AutoMessage)> = vec![
        ("link-only message", message("m3", "https://a.com https://b.com")),
        ("emoji only", message("m4", "❤️")),
        ("system message", AutoMessage { system: true, ..message("m5", "good morning everyone") }),
        ("bot author", AutoMessage { author_bot: true, ..message("m6", "good morning everyone") }),
        ("webhook message", AutoMessage { webhook_id: Some("123".into()), ..message("m7", "good morning everyone") }),
        ("unconfigured channel", AutoMessage { channel_id: "c2".into(), ..message("m8", "good morning everyone") }),
    ];
    for (label, msg) in cases {
        let t = auto_app();
        let fake = install(&t, google("en", "T"));
        let run = run_auto(&t, &msg, false, false, None).await;
        assert_eq!((fake.len(), run.replies.lock().unwrap().len()), (0, 0), "skips: {label}");
    }
    // A message automod deleted (ctx.state) and one marked as bot-deleted.
    let t = auto_app();
    let fake = install(&t, google("en", "T"));
    let a = run_auto(&t, &message("m9", "good morning everyone"), true, false, None).await;
    bot_core::deletion_tracker::mark_bot_deletion(&t, &["m10"], Some("automod"), None);
    let b = run_auto(&t, &message("m10", "good morning everyone"), false, false, None).await;
    assert_eq!(fake.len(), 0);
    assert_eq!(a.replies.lock().unwrap().len() + b.replies.lock().unwrap().len(), 0);
    // A reply to a deleted message does not fail; a channel the bot cannot use makes no request.
    install(&t, google("en", "T"));
    run_auto(&t, &message("m11", "good morning everyone"), false, false, Some(10008)).await;
    let fake = install(&t, google("en", "T"));
    let run = run_auto(&t, &message("m12", "good morning everyone"), false, true, None).await;
    assert_eq!((fake.len(), run.replies.lock().unwrap().len()), (0, 0));
}

#[tokio::test]
async fn auto_translate_respects_the_inflight_cap() {
    let t = auto_app();
    let gate = Arc::new(tokio::sync::Notify::new());
    let fake = install(&t, google("en", "Hola"));
    let mut tasks = Vec::new();
    // Six slow runs fill the slots; the seventh is skipped.
    for i in 0..svc::AUTO_MAX_INFLIGHT {
        let (app, g) = (t.app.clone(), gate.clone());
        tasks.push(tokio::spawn(async move {
            let msg = message(&format!("slow{i}"), "good morning everyone");
            let ctx = AutoCtx { guild_id: "g1", message: &msg, automod_deleted: false, settings: async { None }.boxed(), check_permissions: false };
            svc::handle_auto_translate(&app, ctx, move |_| async move {
                g.notified().await;
                Ok(())
            })
            .await;
        }));
    }
    while svc::auto_inflight(&t) < svc::AUTO_MAX_INFLIGHT {
        tokio::task::yield_now().await;
    }
    let before = fake.len();
    let run = run_auto(&t, &message("busy", "good morning everyone"), false, false, None).await;
    assert_eq!((fake.len(), run.replies.lock().unwrap().len()), (before, 0), "busy: skipped without a request");
    gate.notify_waiters();
    for _ in 0..100 {
        if svc::auto_inflight(&t) == 0 {
            break;
        }
        gate.notify_waiters();
        tokio::task::yield_now().await;
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(svc::auto_inflight(&t), 0);
}

// ── Declarations ──────────────────────────────────────────────────────────────

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_translation::register(&mut reg);
    reg
}

#[test]
fn commands_and_plugins_match_the_js() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &["autotranslate", "autotime", "translate", "timestamp", "timezone"]);
    bot_core::testing::assert_plugin_matches_golden(&reg, "translation-fixes");
    bot_core::testing::assert_plugin_matches_golden(&reg, "autotime");
    assert!(reg.plugin_named("translation-fixes").is_some());
    let order = |id: &str| bot_config::hook_index("messageCreate", id);
    assert!(order("translation-fixes") > order("automod"));
}

#[tokio::test]
async fn translation_hook_filter() {
    let t = google_app();
    assert!(!feat_translation::plugins::translation_fixes::filter(&t, ""));
    assert!(feat_translation::plugins::translation_fixes::filter(&t, "hola"));
}

// ── /translate through the router ──────────────────────────────────────────────

fn slash_text(text: &str, target: &str) -> FakeInteraction {
    FakeInteraction::slash("translate")
        .subcommand("text")
        .option("text", option_type::STRING, json!(text))
        .option("target_language", option_type::STRING, json!(target))
}

#[tokio::test]
async fn translate_text_works_in_a_dm_and_shows_source_and_target() {
    let t = google_app();
    t.install(registry());
    install(&t, google("ja", "It is very hot today, isn't it"));
    route_interaction(&t, slash_text(JAPANESE, "English").build()).await;
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"]);
    let calls = t.responses.calls();
    let e = &calls[1].body["embeds"][0];
    assert!(s(&e["description"]).contains("very hot today"));
    let fields: Vec<String> = e["fields"].as_array().unwrap().iter().map(|f| format!("{}={}", s(&f["name"]), s(&f["value"]))).collect();
    assert_eq!(fields, vec!["Source=Japanese", "Target=English"]);
    assert_eq!(e["footer"]["text"], format!("Original: {JAPANESE}"));
}

#[tokio::test]
async fn translate_text_splits_a_long_result_and_reports_real_errors() {
    let t = google_app();
    t.install(registry());
    install(&t, google("es", &"word ".repeat(2000)));
    route_interaction(&t, slash_text(&"hola ".repeat(250), "en").build()).await;
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply", "followUp", "followUp"]);
    let calls = t.responses.calls();
    assert!(calls[1].embed_title().unwrap_or("").contains("1/3"));
    for f in &calls[2..] {
        assert!(f.embed_description().unwrap_or("").encode_utf16().count() <= 4000);
        assert_eq!(f.body["allowed_mentions"]["parse"], json!([]));
    }

    t.responses.clear();
    install(&t, |_, _| Err(TransportError::Network("fetch failed".into())));
    route_interaction(&t, slash_text("hello there", "es").build()).await;
    let calls = t.responses.calls();
    assert_eq!(t.responses.names(), vec!["deferReply", "deleteReply", "followUp"]);
    assert!(calls[2].embed_description().unwrap_or("").contains("Could not reach"));
    assert!(calls[2].ephemeral());

    // Paused: answered before deferring.
    install(&t, |_, _| Ok(HttpResponse::text(429, "")));
    let _ = svc::translate_text(&t, "hello there", "es", "auto", 1).await;
    t.responses.clear();
    route_interaction(&t, slash_text("hello there", "es").build()).await;
    let calls = t.responses.calls();
    assert_eq!(t.responses.names(), vec!["reply"]);
    assert_eq!(calls[0].embed_title(), Some("⚠️  Translation Paused"));
    assert!(calls[0].ephemeral());
}

#[tokio::test]
async fn translate_text_rejects_unknown_languages_and_empty_text() {
    let t = google_app();
    t.install(registry());
    let fake = install(&t, google("en", "T"));
    route_interaction(&t, slash_text("hello there", "jp").build()).await;
    assert!(t.responses.calls()[0].embed_description().unwrap_or("").contains("Did you mean `ja` (Japanese)?"));
    t.responses.clear();
    route_interaction(&t, slash_text("❤️ <@123456789012345678>", "es").build()).await;
    assert_eq!(t.responses.calls()[0].embed_title(), Some("❌  Nothing to Translate"));
    assert_eq!(fake.len(), 0);
}

#[tokio::test]
async fn translate_detect_and_languages() {
    let t = google_app();
    t.install(registry());
    install(&t, google("iw", "T"));
    let detect = |text: &str| FakeInteraction::slash("translate").subcommand("detect").option("text", option_type::STRING, json!(text)).build();
    route_interaction(&t, detect("שלום לכולם")).await;
    assert!(t.responses.calls()[1].embed_description().unwrap_or("").contains("**Hebrew** (`he`)"));
    t.responses.clear();
    install(&t, google("ceb", "T"));
    route_interaction(&t, detect("Maayong buntag sa tanan")).await;
    let calls = t.responses.calls();
    assert!(calls[1].embed_description().unwrap_or("").contains("**Cebuano** (`ceb`)"));
    assert!(s(&calls[1].body["embeds"][0]["footer"]["text"]).contains("Detected only"));

    t.responses.clear();
    route_interaction(&t, FakeInteraction::slash("translate").subcommand("languages").build()).await;
    let calls = t.responses.calls();
    assert!(calls[0].ephemeral());
    let e = &calls[1].body["embeds"][0];
    assert_eq!(s(&e["description"]).split('\n').count(), 74);
    assert!(s(&e["footer"]["text"]).starts_with("74 languages · backend: Google"));
    assert!(s(&e["fields"][0]["value"]).contains("`iw`→`he`"));
}

#[tokio::test]
async fn translate_autocomplete_lists_languages() {
    let t = google_app();
    t.install(registry());
    route_interaction(&t, FakeInteraction::autocomplete("translate").subcommand("text").focused("target_language", "zh").build()).await;
    let choices = t.responses.calls()[0].choices();
    assert_eq!(choices[0]["value"], "zh");
    assert_eq!(choices[1]["value"], "zh-tw");
}

// ── Database: channels, the cap, the cache, the stats, /autotranslate ───────────

const G1: &str = "930000000000000001";
const G2: &str = "930000000000000002";
const G3: &str = "930000000000000003";
const G4: &str = "930000000000000004";
const G5: &str = "930000000000000005";

async fn db_app(guild: &'static str) -> Option<DbTestApp> {
    let url = database_url()?;
    let t = match redis_url() {
        Some(r) => DbTestApp::with_redis(&url, &r).await,
        None => DbTestApp::new(&url).await,
    }
    .with_guilds([guild]);
    t.reset_guilds(&[guild]).await;
    t.cache().del_one(&svc::channels_key(guild)).await;
    svc::reset(&t);
    svc::configure(&t, ConfigOverride { provider: Some(Provider::Google), result_cache: Some(false), ..Default::default() });
    Some(t)
}

#[tokio::test]
async fn raw_sql_statements_run_in_a_rolled_back_transaction() {
    let Some(url) = database_url() else { return };
    let pool = bot_core::testing::connect_test_pool(&url).await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(r#"INSERT INTO "GuildSettings" ("id", "guildId", "updatedAt") VALUES ($1, '0', (now() AT TIME ZONE 'UTC')) ON CONFLICT DO NOTHING"#)
        .bind(bot_db::new_cuid())
        .execute(&mut *tx)
        .await
        .unwrap();
    for entry in svc::raw_sql().into_iter().chain(stats::raw_sql()) {
        let mut q = sqlx::query(sqlx::AssertSqlSafe(entry.sql.clone()));
        for v in &entry.values {
            q = match v {
                Value::String(x) => q.bind(x.clone()),
                Value::Number(n) => q.bind(n.as_i64().unwrap()),
                other => panic!("unexpected bind {other}"),
            };
        }
        q.execute(&mut *tx).await.unwrap_or_else(|e| panic!("{}: {e}", entry.name));
    }
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn save_translate_channel_caps_and_reports_exists() {
    let Some(t) = db_app(G1).await else { return };
    t.settings().ensure_guild_settings(G1).await.unwrap();
    let langs = vec!["es".to_string(), "ja".to_string()];
    match svc::save_translate_channel(&t, G1, "800000000000000001", &langs, true).await.unwrap() {
        svc::SaveOutcome::Saved { created, row } => {
            assert!(created);
            assert_eq!(row.target_langs, langs);
            assert!(bot_db::is_cuid(&row.id));
        }
        other => panic!("{other:?}"),
    }
    match svc::save_translate_channel(&t, G1, "800000000000000001", &["fr".to_string()], true).await.unwrap() {
        svc::SaveOutcome::Saved { created, row } => assert_eq!((created, row.target_langs), (false, vec!["fr".to_string()])),
        other => panic!("{other:?}"),
    }
    assert_eq!(svc::save_translate_channel(&t, G1, "800000000000000001", &langs, false).await.unwrap(), svc::SaveOutcome::Exists);
    for n in 2..=bot_config::tables::limits::TRANSLATE_CHANNELS {
        svc::save_translate_channel(&t, G1, &format!("8000000000000{n:05}"), &langs, true).await.unwrap();
    }
    assert_eq!(svc::save_translate_channel(&t, G1, "899999999999999999", &langs, true).await.unwrap(), svc::SaveOutcome::Limit { count: 50 });
    svc::refresh_translate_cache(&t, G1).await;
    let config = svc::get_guild_translate_config(&t, G1).await.unwrap();
    assert_eq!(config.len(), 50);
    assert_eq!(config.get("800000000000000001"), Some(&vec!["fr".to_string()]));
    t.reset_guilds(&[G1]).await;
}

#[tokio::test]
async fn js_and_rust_share_the_channel_config_rows_and_cache() {
    let Some(t) = db_app(G2).await else { return };
    // JS writes (row + Redis), Rust reads.
    let js = run_node("rust/fixtures/gen/feat-translation-js.mjs", &["save-channel", G2, "800000000000000021", r#"["es","ja"]"#]);
    assert_eq!(js, json!({ "ok": true, "created": true, "targetLangs": ["es", "ja"] }));
    let config = svc::get_guild_translate_config(&t, G2).await.unwrap();
    assert_eq!(config.get("800000000000000021"), Some(&vec!["es".to_string(), "ja".to_string()]));
    if redis_url().is_some() {
        // The JS cache entry reads as rows, and a legacy list in it is cleaned on read.
        let legacy = json!([{ "channelId": "800000000000000099", "targetLangs": ["ES", "iw", "he", "xx", "Japanese", "zh-Hant", "fr", "de", "it"] }]);
        t.cache().set(&svc::channels_key(G2), &legacy, 300).await;
        svc::reset(&t);
        let config = svc::get_guild_translate_config(&t, G2).await.unwrap();
        assert_eq!(config.get("800000000000000099"), Some(&vec!["es".to_string(), "he".to_string(), "ja".to_string(), "zh-tw".to_string(), "fr".to_string()]));
    }

    // Rust writes (row + Redis), JS reads both.
    t.settings().ensure_guild_settings(G2).await.unwrap();
    svc::save_translate_channel(&t, G2, "800000000000000022", &["zh-tw".to_string()], true).await.unwrap();
    svc::refresh_translate_cache(&t, G2).await;
    let rows = run_node("rust/fixtures/gen/feat-translation-js.mjs", &["rows", G2]);
    assert_eq!(rows, json!([{ "channelId": "800000000000000021", "targetLangs": ["es", "ja"] }, { "channelId": "800000000000000022", "targetLangs": ["zh-tw"] }]));
    let config = run_node("rust/fixtures/gen/feat-translation-js.mjs", &["config", G2]);
    let mut entries = config.as_array().unwrap().clone();
    entries.sort_by_key(|e| e[0].as_str().unwrap_or("").to_string());
    assert_eq!(entries, vec![json!(["800000000000000021", ["es", "ja"]]), json!(["800000000000000022", ["zh-tw"]])]);
    if redis_url().is_some() {
        let cached = run_node("rust/fixtures/gen/feat-translation-js.mjs", &["cache-get", &svc::channels_key(G2)]);
        let mut cached = cached.as_array().unwrap().clone();
        cached.sort_by_key(|e| e["channelId"].as_str().unwrap_or("").to_string());
        assert_eq!(cached, vec![json!({ "channelId": "800000000000000021", "targetLangs": ["es", "ja"] }), json!({ "channelId": "800000000000000022", "targetLangs": ["zh-tw"] })]);
    }
    t.cache().del_one(&svc::channels_key(G2)).await;
    t.reset_guilds(&[G2]).await;
}

#[tokio::test]
async fn js_and_rust_count_translations_in_the_same_rows() {
    let Some(t) = db_app(G3).await else { return };
    let ch = "800000000000000031";
    stats::record_translation(&t, G3, ch, stats::StatVia::Auto, Some("es"), &["ja", "en", "ja"]).await;
    let entry = json!({ "guildId": G3, "channelId": ch, "via": "auto", "detected": "es", "targets": ["en"] }).to_string();
    assert_eq!(run_node("rust/fixtures/gen/feat-translation-js.mjs", &["stat-record", &entry]), json!({ "ok": true }));
    stats::record_translation(&t, G3, ch, stats::StatVia::Command, None, &Vec::<String>::new()).await;
    // DMs and bad ids are ignored, and failures stay inside.
    stats::record_translation(&t, "g1", ch, stats::StatVia::Auto, Some("es"), &["en"]).await;
    let rows = run_node("rust/fixtures/gen/feat-translation-js.mjs", &["stats", G3]);
    assert_eq!(
        rows,
        json!([
            { "channelId": ch, "via": "auto", "direction": "from", "lang": "es", "count": 2 },
            { "channelId": ch, "via": "auto", "direction": "to", "lang": "en", "count": 2 },
            { "channelId": ch, "via": "auto", "direction": "to", "lang": "ja", "count": 1 },
            { "channelId": ch, "via": "command", "direction": "from", "lang": "und", "count": 1 },
        ])
    );
    t.reset_guilds(&[G3]).await;
}

fn admin(guild: &str) -> FakeInteraction {
    FakeInteraction::slash("autotranslate").guild(guild).user("600000000000000009").roles(&["820000000000000002"]).permissions(serenity::all::Permissions::MANAGE_GUILD)
}

fn guild_with_channel(guild: &str, channel: &str, allowed: bool) -> FakeGuild {
    use serenity::all::Permissions as P;
    let bot = if allowed { P::VIEW_CHANNEL | P::SEND_MESSAGES | P::EMBED_LINKS | P::READ_MESSAGE_HISTORY } else { P::VIEW_CHANNEL };
    FakeGuild::new(guild, "600000000000000099")
        .role("820000000000000001", 1, bot)
        .member(TEST_BOT_ID, &["820000000000000001"])
        .role("820000000000000002", 2, P::MANAGE_GUILD)
        .member("600000000000000009", &["820000000000000002"])
        .channel(channel, 0, "general", &[])
}

fn field(call: &bot_core::interaction::RecordedCall, prefix: &str) -> Option<String> {
    call.body["embeds"][0]["fields"].as_array()?.iter().find(|f| s(&f["name"]).starts_with(prefix)).map(|f| s(&f["value"]).to_string())
}

#[tokio::test]
async fn autotranslate_enables_disables_and_validates() {
    let Some(t) = db_app(G4).await else { return };
    let ch = "800000000000000041";
    t.install(registry());
    t.set_discord(offline_discord_as(&[guild_with_channel(G4, ch, true).json()]));
    let langs = |b: FakeInteraction, l: &[&str]| {
        l.iter().enumerate().fold(b, |b, (i, v)| b.option(&format!("language{}", i + 1), option_type::STRING, json!(v)))
    };
    let enable = |l: &[&str]| langs(admin(G4).option("enable", option_type::BOOLEAN, json!(true)).option("channel", option_type::CHANNEL, json!(ch)), l).build();

    // Turning it off needs no language and ignores a bad one.
    route_interaction(&t, langs(admin(G4).option("enable", option_type::BOOLEAN, json!(false)).channel(ch), &["definitely-not-a-language"]).build()).await;
    let calls = t.responses.calls();
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"]);
    assert!(!calls[0].ephemeral());
    assert_eq!(calls[1].embed_title(), Some("⚠️  Not Enabled"));
    assert_eq!(field(&calls[1], "Auto-translate channels").as_deref(), Some("None"));

    t.responses.clear();
    route_interaction(&t, enable(&["es", "ES", "iw", "he", "Japanese"])).await;
    let calls = t.responses.calls();
    assert_eq!(calls[1].embed_title(), Some("✅  Auto-Translation Enabled"));
    let rows = svc::load_rows(t.db(), G4).await.unwrap();
    assert_eq!(rows[0].target_langs, vec![json!("es"), json!("he"), json!("ja")]);
    assert!(t.settings().get_settings(G4).await.unwrap().translation_enabled);
    assert_eq!(field(&calls[1], "Module").as_deref(), Some("The **Auto-Translate** module was off and is now on."));
    assert_eq!(field(&calls[1], "Auto-translate channels (1)").as_deref(), Some(format!("<#{ch}> → es, he, ja").as_str()));

    t.responses.clear();
    route_interaction(&t, admin(G4).option("enable", option_type::BOOLEAN, json!(false)).option("channel", option_type::CHANNEL, json!(ch)).build()).await;
    let calls = t.responses.calls();
    assert_eq!(calls[1].embed_title(), Some("✅  Auto-Translation Disabled"));
    assert!(svc::load_rows(t.db(), G4).await.unwrap().is_empty());
    assert_eq!(field(&calls[1], "Auto-translate channels").as_deref(), Some("None"));

    t.responses.clear();
    route_interaction(&t, enable(&["es", "jp"])).await;
    let calls = t.responses.calls();
    assert_eq!(calls[1].embed_title(), Some("❌  Invalid Language"));
    assert!(calls[1].embed_description().unwrap_or("").contains("Did you mean `ja` (Japanese)?"));
    t.responses.clear();
    route_interaction(&t, enable(&[])).await;
    assert_eq!(t.responses.calls()[1].embed_title(), Some("❌  No Languages"));
    assert!(svc::load_rows(t.db(), G4).await.unwrap().is_empty());
    t.reset_guilds(&[G4]).await;
}

/// The JS checks the resolved option channel itself (`checkChannelUsable(channel)`), so a channel the cache misses
/// (an archived thread, say) is checked as Discord returns it, not refused as "Channel not found".
#[tokio::test]
async fn autotranslate_checks_a_channel_the_cache_does_not_have() {
    const G6: &str = "930000000000000006";
    let Some(t) = db_app(G6).await else { return };
    let (ch, fresh) = ("800000000000000061", "800000000000000062");
    t.install(registry());
    let rest = bot_core::testing::FakeRest::new();
    t.set_discord(rest.discord(&[guild_with_channel(G6, ch, true).json()]));
    rest.on(
        "GET",
        &format!("/channels/{fresh}"),
        200,
        json!({ "id": fresh, "type": 0, "guild_id": G6, "name": "fresh", "position": 1, "permission_overwrites": [] }),
    );
    let resolver = guild_with_channel(G6, ch, true).channel(fresh, 0, "fresh", &[]);
    let i = admin(G6)
        .option("enable", option_type::BOOLEAN, json!(true))
        .option("channel", option_type::CHANNEL, json!(fresh))
        .option("language1", option_type::STRING, json!("es"))
        .resolve_from(&resolver);
    route_interaction(&t, i.build()).await;
    let calls = t.responses.calls();
    assert_eq!(calls[1].embed_title(), Some("✅  Auto-Translation Enabled"), "{:?}", calls[1].embed_description());
    assert_eq!(svc::load_rows(t.db(), G6).await.unwrap().len(), 1);
    t.reset_guilds(&[G6]).await;
}

#[tokio::test]
async fn autotranslate_refuses_a_channel_it_cannot_post_in_and_enforces_the_cap() {
    let Some(t) = db_app(G5).await else { return };
    let (ch, locked) = ("800000000000000051", "800000000000000052");
    t.install(registry());
    let guild = guild_with_channel(G5, ch, true).channel(locked, 0, "locked", &[("820000000000000001", false, serenity::all::Permissions::empty(), serenity::all::Permissions::EMBED_LINKS)]);
    t.set_discord(offline_discord_as(&[guild.json()]));
    let enable = |c: &str| admin(G5).option("enable", option_type::BOOLEAN, json!(true)).option("channel", option_type::CHANNEL, json!(c)).option("language1", option_type::STRING, json!("es"));
    route_interaction(&t, enable(locked).build()).await;
    let calls = t.responses.calls();
    assert_eq!(calls[1].embed_title(), Some("❌  Missing Permissions"));
    assert!(!calls[1].embed_description().unwrap_or("").contains(".."), "no doubled punctuation");
    assert_eq!(
        calls[1].embed_description(),
        Some(format!("I can't post translations in <#{locked}>. I need **Embed Links** in <#{locked}>.").as_str())
    );
    assert!(svc::load_rows(t.db(), G5).await.unwrap().is_empty());

    t.settings().ensure_guild_settings(G5).await.unwrap();
    for n in 0..bot_config::tables::limits::TRANSLATE_CHANNELS {
        svc::save_translate_channel(&t, G5, &format!("8100000000000{n:05}"), &["es".to_string()], true).await.unwrap();
    }
    t.responses.clear();
    route_interaction(&t, enable(ch).build()).await;
    assert_eq!(t.responses.calls()[1].embed_title(), Some("❌  Limit Reached"));
    assert_eq!(t.responses.calls()[1].embed_description(), Some("This server already has 50 auto-translate channels (max 50). Turn one off first."));
    t.reset_guilds(&[G5]).await;
}

/// Generated emoji-, CJK- and markdown-heavy text through the text preparation and sizing helpers, the language
/// lookups and reconcileDetected, against Node 24 (rust/fixtures/gen/feat-translation-fuzz.mjs). Lists every mismatch.
#[tokio::test]
async fn text_preparation_matches_the_js_on_generated_input() {
    use feat_translation::translation_service as ts;
    let g = load_golden("feat-translation/fuzz.json");
    let texts = g["texts"].as_array().cloned().unwrap_or_default();
    assert!(texts.len() > 400, "the fixture has its texts");
    let t = TestApp::new();
    let mut bad: Vec<String> = Vec::new();
    let mut check = |what: &str, input: &Value, js: &Value, ours: Value| {
        if &ours != js {
            bad.push(format!("{what} {input}\n  js   {js}\n  rust {ours}"));
        }
    };
    let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
    for c in &texts {
        let text = s(&c["t"]);
        let input = &c["t"];
        check("clean", input, &c["clean"], json!(ts::clean_for_translation(&text)));
        check("has", input, &c["has"], json!(ts::has_translatable_text(&text)));
        check("linkOnly", input, &c["linkOnly"], json!(ts::is_link_only(&text)));
        check("autoGoogle", input, &c["autoGoogle"], json!(ts::prepare_auto_input_for(ts::Provider::Google, &text)));
        check("autoLt", input, &c["autoLt"], json!(ts::prepare_auto_input_for(ts::Provider::LibreTranslate, &text)));
        let cp = c["clampArgs"][0].as_u64().unwrap_or(1) as usize;
        let bytes = c["clampArgs"][1].as_u64().map(|b| b as usize);
        check(&format!("clamp {cp} {bytes:?}"), input, &c["clamp"], json!(ts::clamp_for_request(&text, cp, bytes)));
        let sm = c["splitMax"].as_u64().unwrap_or(1) as usize;
        check(&format!("split {sm}"), input, &c["split"], json!(ts::split_for_requests(&text, sm)));
        let (em, ep) = (c["embArgs"][0].as_u64().unwrap_or(1) as usize, c["embArgs"][1].as_u64().unwrap_or(1) as usize);
        check(&format!("embeds {em} {ep}"), input, &c["emb"], json!(ts::split_for_embeds(&text, em, ep)));
        check("balance", input, &c["balance"], json!(ts::balance_markdown(&text)));
        check(&format!("echo {}", c["echoWith"]), input, &c["echo"], json!(ts::is_echo(&text, &s(&c["echoWith"]))));
    }
    for c in g["choices"].as_array().into_iter().flatten() {
        check("choices", &c[0], &c[1], json!(ts::language_choices(&t, &s(&c[0]))));
    }
    for c in g["codes"].as_array().into_iter().flatten() {
        let code = s(&c[0]);
        check("normalizeLangCode", &c[0], &c[1], json!(ts::normalize_lang_code(&code)));
        check("resolveLanguageInput", &c[0], &c[2], json!(ts::resolve_language_input(&code)));
        check("suggestLanguage", &c[0], &c[3], json!(ts::suggest_language(&code)));
    }
    for c in g["reconcile"].as_array().into_iter().flatten() {
        let ours = ts::reconcile_detected(&s(&c[0]), c[1].as_str().map(String::from));
        check(&format!("reconcile {}", c[1]), &c[0], &c[2], json!(ours));
    }
    assert!(bad.is_empty(), "{} mismatches:\n{}", bad.len(), bad.join("\n"));
}

/// A cached `targetLangs` element that is not a string resolves only by `String(value)` as a language name, as the
/// JS `normalizeTargetList` does (its `normalizeLangCode` refuses non-strings): `["es"]` is not Spanish, `["Japanese"]`
/// is Japanese. Expected value from `normalizeTargetList([['es'],'ja',5,null,true,{},['Japanese'],'Spanish'])`.
#[tokio::test]
async fn cached_non_string_targets_resolve_like_the_js() {
    use feat_translation::translation_service as ts;
    let t = TestApp::new();
    let g = "960200000000000001";
    let rows = json!([{ "channelId": "960200000000000002", "targetLangs": [["es"], "ja", 5, null, true, {}, ["Japanese"], "Spanish"] }]);
    t.app.cache().set(&ts::channels_key(g), &rows, 300).await;
    let map = ts::get_guild_translate_config(&t, g).await.expect("read from the cache");
    assert_eq!(map.get("960200000000000002"), Some(&vec!["ja".to_string(), "es".to_string()]));
}
