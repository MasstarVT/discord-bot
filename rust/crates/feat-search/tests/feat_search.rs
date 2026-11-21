//! Offline tests of feat-search: the pure rules of `searchService.js`, `afkService.js`, `plugins/afk.js` and
//! `plugins/liverole.js` against JS-produced fixtures (`rust/fixtures/gen/feat-search-golden.mjs`), the golden command
//! and plugin checks, and the behaviour tests ported from `test/search.test.js`, `test/afk.test.js`,
//! `test/liverole.test.js` and `test/sec-misc.test.js` (canned HTTP responses, a fake Discord, the in-memory cache).

mod common;
#[path = "parts/search_tests.rs"]
mod search_tests;
#[path = "parts/liverole_tests.rs"]
mod liverole_tests;

use serde_json::{Value, json};

use common::*;
use feat_search::afk_service as afk;
use feat_search::plugins::afk as afk_plugin;
use feat_search::plugins::liverole as live;
use feat_search::search_service as svc;

fn golden(name: &str) -> Value {
    let path = bot_core::testing::repo_root().join("rust/fixtures/golden/feat-search").join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e} (run node rust/fixtures/gen/feat-search-golden.mjs)", path.display()));
    serde_json::from_str(&text).expect("golden json")
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn opt_s(v: &Value) -> Option<&str> {
    v.as_str()
}

// ── Declarations ──────────────────────────────────────────────────────────────

#[test]
fn commands_and_plugins_match_the_js() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &["afk", "liverole", "search"]);
    bot_core::testing::assert_plugin_matches_golden(&reg, "afk");
    bot_core::testing::assert_plugin_matches_golden(&reg, "liverole");
}

#[test]
fn hook_order_and_customids() {
    // No component of this crate declares a customId (the paginator and confirm prompts are core collectors).
    let reg = registry();
    for id in ["afk", "liverole"] {
        let p = reg.plugin_named(id).expect("declared");
        for h in &p.hooks {
            assert!(bot_config::hook_index(h.event().as_str(), h.id()).is_some(), "{id}: {} in HOOK_ORDER", h.event().as_str());
        }
    }
    assert!(bot_config::hook_index("messageCreate", "afk") > bot_config::hook_index("messageCreate", "automod"), "after automod");
}

// ── search: pure helpers against the JS ──────────────────────────────────────

#[test]
fn search_text_helpers_match_the_js() {
    let g = golden("search");
    for c in g["cleanQuery"].as_array().into_iter().flatten() {
        assert_eq!(svc::clean_query(s(&c[0])), s(&c[1]), "cleanQuery {:?}", c[0]);
    }
    for c in g["keyForm"].as_array().into_iter().flatten() {
        assert_eq!(svc::key_form(s(&c[0])), s(&c[1]), "keyForm {:?}", c[0]);
    }
    for c in g["cacheKey"].as_array().into_iter().flatten() {
        assert_eq!(svc::cache_key(svc::Api::AniList, s(&c[0])), s(&c[1]));
        assert_eq!(svc::cache_key(svc::Api::Urban, s(&c[0])), s(&c[2]));
        assert_eq!(svc::cache_key(svc::Api::PokeApi, s(&c[0])), s(&c[3]));
    }
    for c in g["pokemonSlug"].as_array().into_iter().flatten() {
        let want = match &c[1] {
            Value::Null => None,
            v if v["kind"] == "id" => Some(svc::PokemonQuery::Id(v["id"].as_i64().unwrap_or(0))),
            v => Some(svc::PokemonQuery::Name(s(&v["slug"]).to_string())),
        };
        assert_eq!(svc::pokemon_slug(s(&c[0])), want, "pokemonSlug {:?}", c[0]);
    }
    for c in g["titleCase"].as_array().into_iter().flatten() {
        assert_eq!(svc::title_case(s(&c[0])), s(&c[1]));
    }
    for c in g["stripAniListHtml"].as_array().into_iter().flatten() {
        let max = c[1].as_u64().unwrap_or(0) as usize;
        assert_eq!(svc::strip_anilist_html(s(&c[0]), max), s(&c[2]), "stripAniListHtml {:?} {max}", c[0]);
    }
    for c in g["cleanUrbanText"].as_array().into_iter().flatten() {
        let max = c[1].as_u64().unwrap_or(0) as usize;
        assert_eq!(svc::clean_urban_text(s(&c[0]), max), s(&c[2]), "cleanUrbanText {:?} {max}", c[0]);
    }
    for c in g["stripUrbanBrackets"].as_array().into_iter().flatten() {
        assert_eq!(svc::strip_urban_brackets(s(&c[0])), s(&c[1]));
    }
    for c in g["cleanFlavorText"].as_array().into_iter().flatten() {
        assert_eq!(svc::clean_flavor_text(s(&c[0])), s(&c[1]), "cleanFlavorText {:?}", c[0]);
    }
    for c in g["defangMentions"].as_array().into_iter().flatten() {
        assert_eq!(svc::defang_mentions(s(&c[0])), s(&c[1]));
    }
    let now = g["NOW"].as_i64().unwrap_or(0);
    for c in g["parseRetryAfter"].as_array().into_iter().flatten() {
        assert_eq!(svc::parse_retry_after(opt_s(&c[0]), now), c[1].as_i64(), "parseRetryAfter {:?}", c[0]);
    }
    assert_eq!(svc::parse_retry_after(None, now), None);
    assert_eq!(g["query"], svc::ANILIST_QUERY);
    assert!(svc::ANILIST_QUERY.contains("sort: [SEARCH_MATCH, POPULARITY_DESC]"));
    for c in g["requestBody"].as_array().into_iter().flatten() {
        let kind = if c[0] == "MANGA" { svc::MediaType::Manga } else { svc::MediaType::Anime };
        assert_eq!(svc::anilist_request_body(kind, s(&c[1]), c[2] == true), s(&c[3]), "byte-identical request body");
    }
}

#[test]
fn escape_markdown_matches_discord_js() {
    let g = golden("search");
    for c in g["escapeMarkdown"].as_array().into_iter().flatten() {
        assert_eq!(svc::escape_markdown_default(s(&c[0])), s(&c[1]), "escapeMarkdown {:?}", c[0]);
        assert_eq!(bot_core::markdown::escape_markdown(s(&c[0])), s(&c[1]), "the core escape agrees");
        assert_eq!(svc::escape_markdown_block(s(&c[0])), s(&c[2]), "escapeMarkdown with maskedLink and heading {:?}", c[0]);
    }
}

#[test]
fn interpret_anilist_response_matches_the_js() {
    let g = golden("search");
    let now = g["NOW"].as_i64().unwrap_or(0);
    for c in g["interpret"].as_array().into_iter().flatten() {
        let input = &c[0];
        let headers: indexmap::IndexMap<String, String> =
            input["headers"].as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), s(v).to_string())).collect()).unwrap_or_default();
        let body = (!input["body"].is_null()).then(|| input["body"].clone());
        let got = match svc::interpret_anilist_response(input["status"].as_u64().unwrap_or(0) as u16, &headers, body.as_ref(), now) {
            svc::AniListVerdict::RateLimited { pause_until } => json!({ "kind": "rate-limited", "pauseUntil": pause_until }),
            svc::AniListVerdict::Error { status, pause_until } => json!({ "kind": "error", "status": status, "pauseUntil": pause_until }),
            svc::AniListVerdict::Ok { media, pause_until } => json!({ "kind": "ok", "media": media, "pauseUntil": pause_until }),
        };
        assert_eq!(got, c[1], "interpret {input}");
    }
}

#[test]
fn projections_match_the_js_and_round_trip() {
    let g = golden("search");
    for c in g["projectAniList"].as_array().into_iter().flatten() {
        let media = c[0].as_array().cloned().unwrap_or_default();
        let kind = if c[1] == "MANGA" { svc::MediaType::Manga } else { svc::MediaType::Anime };
        let got = svc::project_anilist_results(&media, kind, c[2] == true);
        assert_eq!(serde_json::to_value(&got).unwrap_or(Value::Null), c[3], "projectAniListResults {}", c[0]);
        let back: Vec<svc::AniListResult> = serde_json::from_value(c[3].clone()).expect("the JS object reads");
        assert_eq!(back, got);
    }
    for c in g["projectPokemon"].as_array().into_iter().flatten() {
        let p = (!c[0].is_null()).then(|| c[0].clone());
        let sp = (!c[1].is_null()).then(|| c[1].clone());
        let got = svc::project_pokemon(p.as_ref(), sp.as_ref());
        assert_eq!(serde_json::to_value(&got).unwrap_or(Value::Null), c[2], "projectPokemon {} {}", c[0], c[1]);
        let back: svc::PokemonInfo = serde_json::from_value(c[2].clone()).expect("the JS object reads");
        assert_eq!(back, got);
    }
    for c in g["projectUrban"].as_array().into_iter().flatten() {
        let got = svc::project_urban(Some(&c[0]));
        assert_eq!(serde_json::to_value(&got).unwrap_or(Value::Null), c[1], "projectUrban {}", c[0]);
    }
    let m = &g["matchPokemonIndex"];
    let entries: Vec<(String, i64)> = serde_json::from_value(m["entries"].clone()).expect("entries");
    for c in m["cases"].as_array().into_iter().flatten() {
        let got: Vec<Value> = svc::match_pokemon_index(&entries, s(&c[0]), c[1].as_u64().unwrap_or(25) as usize)
            .into_iter()
            .map(|ch| json!({ "name": ch.name, "value": ch.value }))
            .collect();
        assert_eq!(Value::Array(got), c[2], "matchPokemonIndex {:?}", c[0]);
    }
}

#[tokio::test]
async fn search_embeds_match_the_js() {
    let g = golden("search");
    for c in g["aniListEmbeds"].as_array().into_iter().flatten() {
        let results: Vec<svc::AniListResult> = serde_json::from_value(c[0].clone()).expect("results");
        let kind = if c[1] == "MANGA" { svc::MediaType::Manga } else { svc::MediaType::Anime };
        let settings = settings_from(&c[2]).await;
        let got: Vec<Value> = svc::build_anilist_embeds(&results, kind, settings.as_deref(), c[3].as_i64().unwrap_or(0)).iter().map(|e| reduce_embed(e, false)).collect();
        assert_eq!(Value::Array(got), c[4], "buildAniListEmbeds {}", c[0]);
    }
    for c in g["pokemonEmbeds"].as_array().into_iter().flatten() {
        let info: svc::PokemonInfo = serde_json::from_value(c[0].clone()).expect("info");
        let settings = settings_from(&c[1]).await;
        assert_eq!(reduce_embed(&svc::build_pokemon_embed(&info, settings.as_deref()), false), c[2], "buildPokemonEmbed {}", c[0]);
    }
    for c in g["urbanEmbeds"].as_array().into_iter().flatten() {
        let results: Vec<svc::UrbanResult> = serde_json::from_value(c[0].clone()).expect("results");
        let settings = settings_from(&c[1]).await;
        let got: Vec<Value> =
            svc::build_urban_embeds(&results, settings.as_deref()).iter().zip(&results).map(|(e, r)| reduce_embed(e, r.written_on.is_some())).collect();
        assert_eq!(Value::Array(got), c[2], "buildUrbanEmbeds {}", c[0]);
    }
    for c in g["noResults"].as_array().into_iter().flatten() {
        let kind = match s(&c[0]) {
            "manga" => svc::NoResultsKind::Manga,
            "urban" => svc::NoResultsKind::Urban,
            "pokemon" => svc::NoResultsKind::Pokemon,
            _ => svc::NoResultsKind::Anime,
        };
        assert_eq!(reduce_embed(&svc::no_results_embed(kind, s(&c[1]), c[2] == true), false), c[3], "noResultsEmbed {c}");
    }
    for c in g["errors"].as_array().into_iter().flatten() {
        let err = opt_s(&c[0]).map(|code| {
            let code = match code {
                "RATE_LIMITED" => svc::SearchCode::RateLimited,
                "COOLDOWN" => svc::SearchCode::Cooldown,
                "TIMEOUT" => svc::SearchCode::Timeout,
                _ => svc::SearchCode::Unavailable,
            };
            let mut e = svc::SearchError::new(code, Some(svc::Api::AniList)).status(c[1].as_u64().map(|v| v as u16));
            e.retry_at = c[2].as_i64();
            e
        });
        assert_eq!(reduce_embed(&svc::search_error_embed(err.as_ref(), "AniList"), false), c[3], "searchErrorEmbed {c}");
        assert_eq!(reduce_embed(&svc::search_error_embed(err.as_ref(), "Urban Dictionary"), false), c[4]);
    }
}

#[test]
fn a_long_third_party_body_is_cut_first_so_no_pass_is_quadratic() {
    // review-feeds.test.js: the inputs of the JS test; the API allows a body of up to 512 KB.
    let cases: Vec<(&str, String)> = vec![
        ("spaces", format!("{}a", " ".repeat(200_000))),
        ("spoilers", "~!x".repeat(100_000)),
        ("brackets", "[".repeat(200_000)),
    ];
    let started = std::time::Instant::now();
    for (name, body) in &cases {
        let t = std::time::Instant::now();
        assert!(bot_util::js::utf16_len(&svc::strip_anilist_html(body, 1000)) <= 1000);
        eprintln!("stripAniListHtml {name}: {:?}", t.elapsed());
    }
    for (name, body) in [("brackets", "[".repeat(200_000)), ("bracket-a", "[a".repeat(100_000))] {
        let t = std::time::Instant::now();
        assert!(bot_util::js::utf16_len(&svc::clean_urban_text(&body, 1800)) <= 1800);
        eprintln!("cleanUrbanText {name}: {:?}", t.elapsed());
    }
    assert!(started.elapsed() < std::time::Duration::from_secs(6), "took {:?}", started.elapsed());
    // The visible part is unchanged: the text that survives the clamp comes from the start.
    assert_eq!(svc::strip_anilist_html("First line<br>Second ~!secret!~ end", 1000), "First line
Second ||secret|| end");
    let out = svc::strip_anilist_html(&format!("{}~!late spoiler", "word ".repeat(300)), 1000);
    assert!(bot_util::js::utf16_len(&out) <= 1000 && out.starts_with("word word"));
    assert_eq!(svc::clean_urban_text("a [b] c", 100), "a b c");
    assert!(bot_util::js::utf16_len(&svc::clean_urban_text(&"x".repeat(5000), 100)) <= 100);
}

// ── afk: pure helpers against the JS ─────────────────────────────────────────

#[test]
fn afk_text_rules_match_the_js() {
    let g = golden("afk");
    for c in g["cleanReason"].as_array().into_iter().flatten() {
        assert_eq!(afk::clean_reason(opt_s(&c[0])).as_deref(), opt_s(&c[1]), "cleanReason {:?}", c[0]);
    }
    assert_eq!(afk::clean_reason(None), None);
    for c in g["plainText"].as_array().into_iter().flatten() {
        assert_eq!(afk::plain_text(s(&c[0]), c[1].as_u64().unwrap_or(0) as usize), s(&c[2]), "plainText {:?}", c[0]);
    }
    for c in g["nameWithoutPrefix"].as_array().into_iter().flatten() {
        assert_eq!(afk::name_without_prefix(s(&c[0])), s(&c[1]));
    }
    for c in g["afkLength"].as_array().into_iter().flatten() {
        assert_eq!(afk::afk_length(c[0].as_i64().unwrap_or(0)), s(&c[1]), "afkLength {}", c[0]);
    }
    for c in g["readPings"].as_array().into_iter().flatten() {
        assert_eq!(serde_json::to_value(afk::read_pings(&c[0])).unwrap_or(Value::Null), c[1], "readPings {}", c[0]);
    }
    for c in g["prefixedNickname"].as_array().into_iter().flatten() {
        assert_eq!(afk::prefixed_nickname(s(&c[0])).as_deref(), opt_s(&c[1]), "prefixedNickname {:?}", c[0]);
    }
    for c in g["restoredNickname"].as_array().into_iter().flatten() {
        let got = afk::restored_nickname(opt_s(&c[0]), opt_s(&c[1]));
        let want = if c[2] == "undefined" { None } else { Some(opt_s(&c[2]).map(str::to_string)) };
        assert_eq!(got, want, "restoredNickname {c}");
    }
    for c in g["ignored"].as_array().into_iter().flatten() {
        let place = afk::ChannelPlace {
            id: s(&c[1]["id"]).into(),
            parent_id: opt_s(&c[1]["parentId"]).map(str::to_string),
            grandparent_id: opt_s(&c[1]["grandparentId"]).map(str::to_string),
        };
        let list: Vec<String> = serde_json::from_value(c[0].clone()).unwrap_or_default();
        let settings = settings_with(|s| s.afk_ignored_channel_ids = list.clone());
        assert_eq!(afk::is_ignored_channel(Some(&settings), Some(&place)), c[2] == true, "isIgnoredChannel {c}");
    }
    assert!(!afk::is_ignored_channel(None, Some(&afk::ChannelPlace { id: "C1".into(), ..Default::default() })));
    let now = g["NOW"].as_i64().unwrap_or(0);
    let since = g["since"].as_i64().unwrap_or(0);
    for c in g["welcome"].as_array().into_iter().flatten() {
        assert_eq!(afk_plugin::welcome_text("Bob", since, c[0].as_i64().unwrap_or(0), &c[1], "G", now), s(&c[2]), "welcomeText {c}");
    }
    for c in g["notice"].as_array().into_iter().flatten() {
        assert_eq!(afk_plugin::notice_text("Bob", opt_s(&c[0]), since), s(&c[1]), "noticeText {c}");
    }
    for c in g["shownName"].as_array().into_iter().flatten() {
        assert_eq!(afk_plugin::shown_name(opt_s(&c[0])), s(&c[1]), "shownName {c}");
    }
    assert_eq!(afk::jump_link("G", "C", "M"), s(&g["jumpLink"]));
}

// ── liverole: pure helpers against the JS ────────────────────────────────────

fn presence_json(activities: &Value) -> Value {
    json!({ "user": { "id": "600000000000000001" }, "status": "online", "activities": activities.as_array().cloned().unwrap_or_default().iter().map(activity_json).collect::<Vec<_>>(), "client_status": {} })
}

fn activity_json(a: &Value) -> Value {
    let mut o = json!({ "type": a["type"], "name": a["name"], "created_at": 0 });
    for k in ["url", "details", "state"] {
        if !a[k].is_null() {
            o[k] = a[k].clone();
        }
    }
    o
}

fn presence_of(v: &Value) -> Option<serenity::all::Presence> {
    if v.is_null() {
        return None;
    }
    Some(serde_json::from_value(presence_json(&v["activities"])).expect("presence"))
}

#[test]
fn liverole_rules_match_the_js() {
    let g = golden("liverole");
    for c in g["streamLabel"].as_array().into_iter().flatten() {
        assert_eq!(live::stream_label(presence_of(&c[0]).as_ref()).as_deref(), opt_s(&c[1]), "streamLabel {}", c[0]);
    }
    for c in g["safeStreamUrl"].as_array().into_iter().flatten() {
        assert_eq!(live::safe_stream_url(opt_s(&c[0])).as_deref(), opt_s(&c[1]), "safeStreamUrl {:?}", c[0]);
    }
    for c in g["streamVars"].as_array().into_iter().flatten() {
        let a = &c[0];
        let got = live::stream_vars_of(s(&a["name"]), opt_s(&a["url"]), opt_s(&a["details"]), opt_s(&a["state"]));
        assert_eq!(serde_json::to_value(&got).unwrap_or(Value::Null), c[1], "streamVars {a}");
    }
    for c in g["planMember"].as_array().into_iter().flatten() {
        let mode = if c[0] == "clear" { live::Mode::Clear } else { live::Mode::Sync };
        let got = live::plan_member(mode, c[1] == true, c[2] == true, c[3] == true).map(|a| a.as_str());
        assert_eq!(got, opt_s(&c[4]), "planMember {c}");
    }
    for c in g["defersRemoval"].as_array().into_iter().flatten() {
        let mode = if c[0] == "clear" { live::Mode::Clear } else { live::Mode::Sync };
        let source = match s(&c[1]) {
            "presence" => live::Source::Presence,
            "recheck" => live::Source::Recheck,
            _ => live::Source::Sync,
        };
        assert_eq!(live::defers_removal(mode, source, c[2] == true, c[3] == true), c[4] == true, "defersRemoval {c}");
    }
    assert_eq!(g["STOP_GRACE_MS"].as_u64(), Some(live::limits::STOP_GRACE_MS));
    assert_eq!(live::RECHECK_JOB_TYPE, "liverole.recheck");
    assert_eq!(live::recheck_job_key("111", "222"), s(&g["recheckJobKey"]));
    for c in g["planSync"].as_array().into_iter().flatten() {
        let v = |x: &Value| -> Vec<String> { serde_json::from_value(x.clone()).unwrap_or_default() };
        let ok = v(&c[4]);
        let mode = if c[0] == "clear" { live::Mode::Clear } else { live::Mode::Sync };
        let plan = live::plan_sync(mode, &v(&c[1]), &v(&c[2]), &v(&c[3]), |id| ok.iter().any(|o| o == id));
        assert_eq!(json!({ "add": plan.add, "remove": plan.remove, "check": plan.check }), c[5], "planSync {c}");
    }
    for c in g["meets"].as_array().into_iter().flatten() {
        // The JS uses plain strings as ids; the Rust side compares snowflakes, so map each to a stable number.
        fn id(x: &str) -> &str {
            match x {
                "G" => "900000000000000001",
                "R" => "820000000000000001",
                "X" => "820000000000000009",
                other => other,
            }
        }
        let roles: Vec<serenity::all::RoleId> =
            c[2].as_array().into_iter().flatten().filter_map(|r| id(s(r)).parse::<u64>().ok()).map(serenity::all::RoleId::new).collect();
        let got = live::meets_requirement(opt_s(&c[0]).map(id), c[1] == true, &roles, id(s(&c[3])));
        assert_eq!(got, c[4] == true, "meetsRequirement {c}");
    }
    for c in g["classify"].as_array().into_iter().flatten() {
        let kind = match live::classify_role_error(c[0].as_u64()) {
            live::RoleErrorKind::Gone => "gone",
            live::RoleErrorKind::RoleMissing => "role-missing",
            live::RoleErrorKind::Perms => "perms",
            live::RoleErrorKind::Error => "error",
        };
        assert_eq!(kind, s(&c[1]));
    }
    for c in g["formatSyncResult"].as_array().into_iter().flatten() {
        assert_eq!(live::format_sync_result(sync_result_of(&c[0]).as_ref()), s(&c[1]), "formatSyncResult {}", c[0]);
    }
    for c in g["problems"].as_array().into_iter().flatten() {
        let p = &c[0];
        let manage = (!p["manage"].is_null()).then(|| bot_core::interaction_helpers::RoleCheck {
            ok: p["manage"]["ok"] == true,
            reason: opt_s(&p["manage"]["reason"]).map(str::to_string),
            dangerous: Vec::new(),
        });
        let input = live::ProblemInput {
            enabled: p["enabled"] == true,
            live_role_id: opt_s(&p["liveRoleId"]),
            role_exists: p["roleExists"] == true,
            manage: manage.as_ref(),
            dangerous_names: serde_json::from_value(p["dangerousNames"].clone()).unwrap_or_default(),
            required_role_id: opt_s(&p["requiredRoleId"]),
            required_role_exists: p["requiredRoleExists"] == true,
            holder_count: p["holderCount"].as_u64().unwrap_or(0) as usize,
        };
        assert_eq!(json!(live::live_role_problems(&input)), c[1], "liveRoleProblems {p}");
    }
    for c in g["shouldHandle"].as_array().into_iter().flatten() {
        let peeked = (!c[2].is_null()).then(|| {
            settings_with(|s| {
                s.live_role_enabled = c[2]["liveRoleEnabled"] == true;
                s.live_role_id = opt_s(&c[2]["liveRoleId"]).map(str::to_string);
            })
        });
        let got = live::should_handle_presence(presence_of(&c[0]).as_ref(), presence_of(&c[1]).as_ref(), peeked.as_ref());
        assert_eq!(got, c[3] == true, "shouldHandlePresence {c}");
    }
    assert_eq!(g["DEFAULT_LIVE_ANNOUNCE_MESSAGE"], live::DEFAULT_LIVE_ANNOUNCE_MESSAGE);
}

fn sync_result_of(v: &Value) -> Option<live::SyncResult> {
    if v.is_null() {
        return None;
    }
    if v["ok"] != true {
        let code = match s(&v["code"]) {
            "settings" => live::SyncFailure::Settings,
            "off" => live::SyncFailure::Off,
            "no-role" => live::SyncFailure::NoRole,
            "role-missing" => live::SyncFailure::RoleMissing,
            "cannot-manage" => live::SyncFailure::CannotManage,
            "presence-unavailable" => live::SyncFailure::PresenceUnavailable,
            _ => live::SyncFailure::Error,
        };
        return Some(live::SyncResult::Failed { code, reason: opt_s(&v["reason"]).map(str::to_string) });
    }
    let n = |k: &str| v[k].as_u64().unwrap_or(0) as usize;
    Some(live::SyncResult::Done(live::SyncDone {
        mode: if v["mode"] == "clear" { live::Mode::Clear } else { live::Mode::Sync },
        added: n("added"),
        removed: n("removed"),
        failed: n("failed"),
        deferred: n("deferred"),
        adds_blocked: opt_s(&v["addsBlocked"]).map(str::to_string),
        complete: v["complete"] == true,
    }))
}
