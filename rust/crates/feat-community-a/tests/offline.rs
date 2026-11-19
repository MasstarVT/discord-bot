//! Offline tests (no PostgreSQL, no Redis): the ported pure rules of `test/bumps.test.js`, `test/suggestions.test.js`
//! and `test/confessions.test.js`, the golden checks (commands, plugins), and the JS-produced fixtures (embeds,
//! components, modals, customIds, job payloads, text renders) in `rust/fixtures/golden/feat-community-a/`.

mod common;

use bot_core::prelude::*;
use bot_core::testing::TestApp;
use chrono::{TimeZone, Utc};
use common::{bot_message, golden, no_ts, norm};
use feat_community_a::bump_service as bump;
use serde_json::{Value, json};

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_community_a::register(&mut reg);
    reg
}

// ── Declarations ──────────────────────────────────────────────────────────────

#[test]
fn commands_and_plugins_match_the_js() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &["suggestion", "confession", "bumps"]);
    bot_core::testing::assert_plugin_matches_golden(&reg, "suggestions");
    bot_core::testing::assert_plugin_matches_golden(&reg, "confessions");
    bot_core::testing::assert_plugin_matches_golden(&reg, "bumps");
    // Only this crate's prefixes and job types are judged (the other crates are not in this registry).
    let mine: Vec<String> = bot_core::testing::persistent_handler_problems(&reg)
        .into_iter()
        .filter(|p| ["\"suggestion", "\"confession", "bump."].iter().any(|k| p.contains(k)))
        .collect();
    assert!(mine.is_empty(), "{mine:?}");
}

#[test]
fn hook_order_has_every_hook() {
    for (event, id) in [("botMessage", "bumps"), ("channelDelete", "bumps"), ("roleDelete", "bumps"), ("messageDelete", "suggestions"), ("channelDelete", "suggestions"), ("messageDelete", "confessions"), ("channelDelete", "confessions")] {
        assert!(bot_config::hook_index(event, id).is_some(), "{event} {id}");
    }
}

// ── Bumps: pure rules ─────────────────────────────────────────────────────────

#[test]
fn the_site_table_holds_disboard() {
    let g = golden("bumps");
    assert_eq!(bump::BUMP_SITES.len(), 1);
    let d = &bump::BUMP_SITES[0];
    assert_eq!(d.key, "disboard");
    assert_eq!(d.bot_id, g["sites"]["disboard"]["botId"]);
    assert_eq!(d.name, g["sites"]["disboard"]["name"]);
    assert_eq!(json!(d.cooldown_ms), g["sites"]["disboard"]["cooldownMs"]);
    assert_eq!(bump::site_key_for_bot("302050872383242240"), Some("disboard"));
    assert_eq!(bump::site_key_for_bot("1"), None);
    assert_eq!(bump::site_key_for_bot(""), None);
    assert_eq!(json!(bump::BUMP_COUNT_MAX), g["bumpCountMax"]);
    assert_eq!(json!(bump::LEADERBOARD_SIZE), g["leaderboardSize"]);
    assert!(bump::is_bump_site_message("302050872383242240"));
    assert!(!bump::is_bump_site_message("999"));
}

#[test]
fn is_disboard_success_matches_the_js_on_every_fixture() {
    let g = golden("bumps");
    for case in g["disboard"].as_array().unwrap() {
        let embeds: Vec<serenity::all::Embed> = serde_json::from_value(case["embeds"].clone()).unwrap();
        assert_eq!(json!(bump::is_disboard_success(&embeds)), case["success"], "{case}");
    }
}

#[test]
fn bumper_of_prefers_the_interaction_metadata_and_never_a_bot() {
    let m = bot_message("1", "2", "3", "302050872383242240", json!([]), Some(("600000000000000001", false)), "2026-01-01T00:00:00.000Z");
    assert_eq!(bump::bumper_of(&m).map(|u| u.id.to_string()).as_deref(), Some("600000000000000001"));
    let botty = bot_message("1", "2", "3", "302050872383242240", json!([]), Some(("600000000000000001", true)), "2026-01-01T00:00:00.000Z");
    assert!(bump::bumper_of(&botty).is_none());
    let none = bot_message("1", "2", "3", "302050872383242240", json!([]), None, "2026-01-01T00:00:00.000Z");
    assert!(bump::bumper_of(&none).is_none());
    // The deprecated `interaction` field is used when there is no metadata.
    let mut j = serde_json::to_value(&none).unwrap();
    j["interaction"] = json!({ "id": "1", "type": 2, "name": "bump", "user": { "id": "600000000000000005", "username": "x", "discriminator": "0", "avatar": null } });
    let old: serenity::all::Message = serde_json::from_value(j).unwrap();
    assert_eq!(bump::bumper_of(&old).map(|u| u.id.to_string()).as_deref(), Some("600000000000000005"));
}

#[test]
fn bump_texts_keys_and_job_match_the_js() {
    let g = golden("bumps");
    assert_eq!(bump::DEFAULT_REMINDER_MESSAGE, g["defaultReminder"]);
    assert_eq!(bump::DEFAULT_THANKS_MESSAGE, g["defaultThanks"]);
    assert_eq!(g["dashboardDefaults"]["reminder"], g["defaultReminder"]);
    assert_eq!(g["dashboardDefaults"]["thanks"], g["defaultThanks"]);
    assert_eq!(bump::reminder_dedupe_key("G1", "disboard"), g["dedupeKey"]);
    let e = no_ts(serde_json::to_value(bump::module_off_embed()).unwrap());
    assert_eq!(e, g["moduleOff"]);
    let run_at = Utc.timestamp_millis_opt(g["job"]["runAt"].as_str().map(|s| bot_db::parse_js_date(s).unwrap().timestamp_millis()).unwrap()).unwrap();
    let job = bump::reminder_job("900000000000000077", "disboard", run_at);
    assert_eq!(job.job_type.as_str(), g["job"]["type"]);
    assert_eq!(job.payload, g["job"]["payload"]);
    assert_eq!(job.dedupe_key.as_deref(), g["job"]["dedupeKey"].as_str());
    assert_eq!(bot_db::format_js_date(&job.run_at), g["job"]["runAt"]);
    assert!(matches!(job.on_conflict, OnConflict::Replace));
    assert_eq!(json!(bump::raw_sql().iter().map(|r| r.0).collect::<Vec<_>>()), g["rawSqlNames"]);
}

#[test]
fn thanks_and_reminder_texts_render_like_the_js() {
    let g = golden("bumps");
    for case in g["thanks"].as_array().unwrap() {
        let next = Utc.timestamp_millis_opt(case["nextMs"].as_i64().unwrap()).unwrap();
        let t = case["template"].as_str();
        let text = bump::thanks_text(t, Default::default(), "Disboard", Some(7), &next);
        let expect = case["text"].as_str().unwrap();
        assert_eq!(text.as_deref().unwrap_or(""), if expect.trim().is_empty() { "" } else { expect }, "{case}");
        let unknown = bump::thanks_text(t, Default::default(), "Disboard", None, &next);
        let expect = case["textUnknownCount"].as_str().unwrap();
        assert_eq!(unknown.as_deref().unwrap_or(""), if expect.trim().is_empty() { "" } else { expect });
    }
    for case in g["reminders"].as_array().unwrap() {
        let t = case["template"].as_str();
        assert_eq!(bump::reminder_content(t, Default::default(), "Disboard", &[]), case["text"].as_str().unwrap(), "{case}");
        assert_eq!(bump::reminder_content(t, Default::default(), "Disboard", &["123456789012345678".into()]), case["withRole"].as_str().unwrap());
    }
}

#[test]
fn bumps_command_helpers() {
    use feat_community_a::commands::bumps::{leaderboard_lines, plural, stamp, status_fields};
    assert_eq!(plural(1, "bump"), "1 bump");
    assert_eq!(plural(1234, "bump"), "1,234 bumps");
    assert_eq!(plural(0, "member"), "0 members");
    let d = Utc.timestamp_millis_opt(1_767_323_045_678).unwrap();
    assert_eq!(stamp(&d, "R"), "<t:1767323045:R>");
    let at = |ms: i64| Utc.timestamp_millis_opt(ms).unwrap();
    let state = |next: i64, bumper: Option<&str>| bot_db::BumpState {
        guild_id: "G".into(),
        site: "disboard".into(),
        last_bump_at: at(1_000_000),
        last_bumper_id: bumper.map(String::from),
        channel_id: "C".into(),
        next_bump_at: at(next),
        reminded_at: None,
    };
    assert_eq!(status_fields(&[], 0)[0].1, "No bump seen yet. Use `/bump` with Disboard; I'll count it and remind you in time.");
    assert_eq!(status_fields(&[state(2_000_000, Some("U"))], 3_000_000)[0].1, "Last bump: <t:1000:R> by <@U>\nNext bump: **now**, the server can be bumped!");
    assert_eq!(status_fields(&[state(9_000_000, None)], 3_000_000)[0].1, "Last bump: <t:1000:R>\nNext bump: <t:9000:R> (<t:9000:t>)");
    let row = |u: &str, c: i32| bot_db::BumpCount { guild_id: "G".into(), user_id: u.into(), count: c, last_bump_at: at(0) };
    let rows: Vec<_> = (0..5).map(|i| row(&format!("U{i}"), 10 - i)).collect();
    let lines = leaderboard_lines(&rows);
    assert_eq!(lines[0], "🥇 <@U0> — 10 bumps");
    assert_eq!(lines[2], "🥉 <@U2> — 8 bumps");
    assert_eq!(lines[3], "**4.** <@U3> — 7 bumps");
    assert_eq!(leaderboard_lines(&[row("A", 1)])[0], "🥇 <@A> — 1 bump");
}

#[tokio::test]
async fn bump_hook_filters_follow_the_memoised_settings() {
    let g = "930000000000000090";
    let t = TestApp::new().with_guilds([g]);
    assert!(bump::channel_delete_filter(&t, g, "1"), "no memo: runs");
    assert!(bump::role_delete_filter(&t, g, "1"));
    t.settings().update_settings(g, SettingsPatch::new().set("bumpChannelId", "800000000000000001").set("bumpRoleId", "820000000000000001")).await.unwrap();
    assert!(bump::channel_delete_filter(&t, g, "800000000000000001"));
    assert!(!bump::channel_delete_filter(&t, g, "800000000000000002"));
    assert!(bump::role_delete_filter(&t, g, "820000000000000001"));
    assert!(!bump::role_delete_filter(&t, g, "820000000000000002"));
}

#[tokio::test]
async fn mark_seen_is_bounded_and_per_app() {
    let t = TestApp::new();
    assert!(bump::mark_seen(&t, "1"));
    assert!(!bump::mark_seen(&t, "1"));
    assert!(!bump::mark_seen(&t, ""));
    for i in 2..=501 {
        bump::mark_seen(&t, &i.to_string());
    }
    assert!(bump::mark_seen(&t, "1"), "the oldest id was forgotten after 500 more");
    let other = TestApp::new();
    assert!(bump::mark_seen(&other, "2"));
    bump::reset_seen(&t);
    assert!(bump::mark_seen(&t, "2"));
}

#[tokio::test]
async fn bumps_member_subcommands_refuse_while_the_module_is_off() {
    let g = "930000000000000091";
    let t = TestApp::new().with_guilds([g]);
    t.install(registry());
    t.settings().update_settings(g, SettingsPatch::new().set("bumpsEnabled", false)).await.unwrap();
    for sub in ["status", "leaderboard"] {
        t.responses.clear();
        bot_core::router::route_interaction(&t, bot_core::testing::FakeInteraction::slash("bumps").guild(g).user("600000000000000001").subcommand(sub).build()).await;
        let calls = t.responses.calls();
        assert_eq!(t.responses.names(), vec!["reply"], "{sub}");
        assert!(calls[0].ephemeral());
        assert_eq!(calls[0].embed_title(), Some("⚠️  Module Disabled"));
    }
}

// ── Suggestions: pure rules and builders ──────────────────────────────────────

use feat_community_a::suggestion_service as sug;

fn suggestion_of(row: &Value) -> bot_db::Suggestion {
    let mut r = row.clone();
    r["updatedAt"] = r["createdAt"].clone();
    serde_json::from_value(r).expect("a Suggestion row")
}

#[test]
fn suggestion_parse_toggle_and_constants_match_the_js() {
    let g = golden("suggestions");
    assert_eq!(json!(sug::RENDER_INTERVAL_MS), g["constants"]["RENDER_INTERVAL_MS"]);
    assert_eq!(json!(sug::NOTE_MAX), g["constants"]["NOTE_MAX"]);
    assert_eq!(sug::ANONYMOUS_NAME, g["constants"]["ANONYMOUS_NAME"]);
    for case in g["parse"].as_array().unwrap() {
        let out = sug::parse_content(case["raw"].as_str());
        let expect = &case["out"];
        match out {
            Ok(c) => assert_eq!((json!(true), json!(c)), (expect["ok"].clone(), expect["content"].clone())),
            Err(e) => assert_eq!(json!(if e == sug::ContentError::Empty { "empty" } else { "too-long" }), expect["error"]),
        }
    }
    for case in g["toggle"].as_array().unwrap() {
        let (next, result) = sug::toggle_vote(case["current"].as_i64().unwrap() as i32, case["value"].as_i64().unwrap() as i32);
        assert_eq!(json!(next), case["out"]["next"]);
        assert_eq!(result.as_str(), case["out"]["result"]);
    }
    for st in bot_db::SuggestionStatus::ALL {
        let info = sug::status_info(*st);
        let js = &g["statusInfo"][st.as_str()];
        assert_eq!((json!(info.label), json!(info.emoji), json!(info.color), json!(info.dm)), (js["label"].clone(), js["emoji"].clone(), js["color"].clone(), js["dm"].clone()));
        assert_eq!(!sug::is_voting_open(*st), g["closed"].as_array().unwrap().contains(&json!(st.as_str())));
    }
    assert_eq!(json!(sug::raw_sql().iter().map(|r| r.0).collect::<Vec<_>>()), g["rawSqlNames"]);
    assert_eq!(no_ts(serde_json::to_value(sug::module_off_embed()).unwrap()), g["moduleOff"]);
}

#[test]
fn cooldown_remaining_counts_from_the_last_suggestion() {
    let last = Utc.timestamp_millis_opt(1_000_000).unwrap();
    assert_eq!(sug::cooldown_remaining_ms(Some(&last), 60.0, 1_030_000), 30_000);
    assert_eq!(sug::cooldown_remaining_ms(Some(&last), 60.0, 2_000_000), 0);
    assert_eq!(sug::cooldown_remaining_ms(None, 60.0, 0), 0);
    assert_eq!(sug::cooldown_remaining_ms(Some(&last), 0.0, 0), 0);
    assert_eq!(sug::cooldown_remaining_ms(Some(&last), -5.0, 0), 0);
}

#[test]
fn suggestion_posts_buttons_dms_and_the_form_match_the_js() {
    let g = golden("suggestions");
    let author = sug::Author { name: "Some Member".into(), icon_url: Some("https://cdn.discordapp.com/embed/avatars/0.png".into()) };
    let long = sug::Author { name: "x".repeat(300), icon_url: None };
    let now = Utc::now();
    for post in g["posts"].as_array().unwrap() {
        let s = suggestion_of(&post["row"]);
        assert_eq!(norm(&serde_json::to_value(sug::build_suggestion_embed(&s, Some(&author))).unwrap()), norm(&post["embed"]), "{}", post["row"]);
        assert_eq!(norm(&serde_json::to_value(sug::build_suggestion_embed(&s, Some(&long))).unwrap()), norm(&post["embedNoIcon"]));
        assert_eq!(serde_json::to_value(sug::build_suggestion_embed(&s, None)).unwrap()["timestamp"], post["embed"]["timestamp"]);
        assert_eq!(norm(&serde_json::to_value(sug::build_suggestion_components(&s)).unwrap()), norm(&post["components"]));
        assert_eq!(json!(sug::jump_url(&s)), post["jumpUrl"]);
        assert_eq!(norm(&serde_json::to_value(sug::build_review_dm(Some("Test Guild"), &s, &now)).unwrap()), norm(&post["dm"]));
        assert_eq!(norm(&serde_json::to_value(sug::build_review_dm(None, &s, &now)).unwrap()), norm(&post["dmNoGuild"]));
    }
    for (i, anonymous) in [false, true].into_iter().enumerate() {
        assert_eq!(norm(&serde_json::to_value(sug::build_suggestion_modal(anonymous)).unwrap()), norm(&g["modal"][i]));
    }
}

#[test]
fn an_anonymous_post_leaks_neither_the_author_id_nor_the_name() {
    let g = golden("suggestions");
    let s = suggestion_of(&g["posts"][1]["row"]);
    assert!(s.anonymous);
    let author = sug::Author { name: "Some Member".into(), icon_url: Some("https://cdn/x.png".into()) };
    let text = serde_json::to_string(&sug::build_suggestion_payload(&s, Some(&author)).to_message()).unwrap();
    assert!(!text.contains(&s.author_id));
    assert!(!text.contains("Some Member"));
    assert!(text.contains("Anonymous"));
}

#[tokio::test]
async fn the_author_avatar_is_display_avatar_url_with_the_discord_js_defaults() {
    // member.displayAvatarURL() / user.displayAvatarURL(): webp (gif when animated), no size.
    let g = "934000000000000001";
    let (plain, animated) = ("600000000000000001", "600000000000000002");
    let guild = bot_core::testing::FakeGuild::new(g, "600000000000000099")
        .member_with(plain, &[], |m| m["user"]["avatar"] = json!("0123456789abcdef0123456789abcdef"))
        .member_with(animated, &[], |m| m["user"]["avatar"] = json!("a_0123456789abcdef0123456789abcdef"));
    let t = TestApp::new().with_guilds([g]).with_fake_guilds(&[guild]);
    let mut s: bot_db::Suggestion = serde_json::from_value(json!({
        "id": "s1", "guildId": g, "number": 1, "authorId": plain, "anonymous": false, "content": "x", "status": "OPEN",
        "channelId": "1", "messageId": null, "threadId": null, "upvotes": 0, "downvotes": 0, "reviewerId": null,
        "reviewNote": null, "reviewedAt": null, "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-01T00:00:00.000Z"
    }))
    .expect("a suggestion row");
    let gid = serenity::all::GuildId::new(g.parse().unwrap());
    let a = sug::author_for(&t, gid, &s).await;
    assert_eq!(a.icon_url.as_deref(), Some("https://cdn.discordapp.com/avatars/600000000000000001/0123456789abcdef0123456789abcdef.webp"));
    s.author_id = animated.into();
    let a = sug::author_for(&t, gid, &s).await;
    assert_eq!(a.icon_url.as_deref(), Some("https://cdn.discordapp.com/avatars/600000000000000002/a_0123456789abcdef0123456789abcdef.gif"));
}

#[tokio::test]
async fn submit_problem_order_module_channel_anonymous() {
    let t = TestApp::new();
    let g = "930000000000000092";
    let s = t.settings().update_settings(g, SettingsPatch::new().set("suggestionsEnabled", false).set("suggestionChannelId", "1")).await.unwrap();
    assert_eq!(serde_json::to_value(sug::submit_problem(&s, false)).unwrap()["title"], "⚠️  Module Disabled");
    let s = t.settings().update_settings(g, SettingsPatch::new().set("suggestionsEnabled", true).set("suggestionChannelId", Value::Null)).await.unwrap();
    assert_eq!(serde_json::to_value(sug::submit_problem(&s, false)).unwrap()["title"], "❌  No Suggestion Channel");
    let s = t.settings().update_settings(g, SettingsPatch::new().set("suggestionChannelId", "1").set("suggestionAllowAnonymous", false)).await.unwrap();
    assert_eq!(serde_json::to_value(sug::submit_problem(&s, true)).unwrap()["title"], "❌  Anonymous Not Allowed");
    assert!(sug::submit_problem(&s, false).is_none());
    let e = serde_json::to_value(sug::cooldown_embed(90_000, 1_000_000)).unwrap();
    assert_eq!(e["description"], "You can post another suggestion <t:1090:R>.");
}

#[tokio::test]
async fn render_debounce_renders_at_once_then_collapses_a_burst() {
    use std::sync::{Arc, Mutex};
    let t = TestApp::new();
    let calls: Arc<Mutex<Vec<String>>> = Arc::default();
    let c = calls.clone();
    sug::set_render_interval(&t, Some(60));
    sug::set_render_impl(
        &t,
        Some(Arc::new(move |_app, id| {
            let c = c.clone();
            Box::pin(async move {
                c.lock().unwrap().push(id);
                Ok(sug::RenderResult::Ok)
            })
        })),
    );
    let settle = |ms| tokio::time::sleep(std::time::Duration::from_millis(ms));
    sug::request_render(&t, "D1");
    settle(20).await;
    assert_eq!(calls.lock().unwrap().len(), 1, "rendered at once");
    for _ in 0..40 {
        sug::request_render(&t, "D1");
    }
    settle(10).await;
    assert_eq!(calls.lock().unwrap().len(), 1, "held inside the window");
    // The window is measured on the app clock; move it on, then let the timer fire.
    t.clock.advance(60);
    settle(150).await;
    assert_eq!(calls.lock().unwrap().len(), 2, "one trailing render for the whole burst");
    sug::request_render(&t, "D2");
    settle(20).await;
    assert_eq!(calls.lock().unwrap().len(), 3, "another suggestion is independent");

    // cancelRender drops a queued render.
    sug::request_render(&t, "D2");
    sug::cancel_render(&t, "D2");
    t.clock.advance(1000);
    settle(150).await;
    assert_eq!(calls.lock().unwrap().len(), 3);
}

// ── Confessions: pure rules and builders ──────────────────────────────────────

use feat_community_a::confession_service as conf;

fn confession_row(over: Value) -> bot_db::Confession {
    let mut r = json!({
        "id": "abc123", "guildId": "G", "number": 7, "authorId": "600000000000000001", "content": "I ate the last slice", "hasImage": false,
        "status": "PENDING", "channelId": "POST", "messageId": null, "reviewMessageId": null, "reviewerId": null, "reviewedAt": null,
        "createdAt": "2026-01-01T00:00:00.000Z"
    });
    for (k, v) in over.as_object().cloned().unwrap_or_default() {
        r[k] = v;
    }
    serde_json::from_value(r).expect("a Confession row")
}

#[test]
fn confession_parse_links_and_constants_match_the_js() {
    let g = golden("confessions");
    assert_eq!(conf::IMAGE_NAME, g["constants"]["IMAGE_NAME"]);
    assert_eq!(json!(conf::IMAGE_TIMEOUT_MS), g["constants"]["IMAGE_TIMEOUT_MS"]);
    assert_eq!(json!(conf::EMBED_TEXT_MAX), g["constants"]["EMBED_TEXT_MAX"]);
    for st in bot_db::ConfessionStatus::ALL {
        let info = conf::status_info(*st);
        let js = &g["statusInfo"][st.as_str()];
        assert_eq!((json!(info.label), json!(info.emoji), json!(info.color)), (js["label"].clone(), js["emoji"].clone(), js["color"].clone()));
    }
    assert_eq!(json!(conf::raw_sql().iter().map(|r| r.0).collect::<Vec<_>>()), g["rawSqlNames"]);
    assert_eq!(no_ts(serde_json::to_value(conf::module_off_embed()).unwrap()), g["moduleOff"]);
    for case in g["parse"].as_array().unwrap() {
        match conf::parse_content(case["raw"].as_str()) {
            Ok(c) => assert_eq!(json!(c), case["out"]["content"]),
            Err(e) => assert_eq!(json!(if e == conf::ContentError::Empty { "empty" } else { "too-long" }), case["out"]["error"]),
        }
    }
    for case in g["links"].as_array().unwrap() {
        assert_eq!(json!(conf::contains_link(case["text"].as_str().unwrap())), case["link"], "{}", case["text"]);
    }
}

#[test]
fn confession_messages_match_the_js_and_never_name_the_author() {
    let g = golden("confessions");
    let row = confession_row(json!({ "id": "cmconf0000000000000000001", "guildId": "900000000000000077", "number": 5, "content": "I *never* read the rules", "channelId": "800000000000000001", "createdAt": "2026-01-02T03:04:05.678Z" }));
    let now = Utc::now();
    let post = serde_json::to_value(conf::build_confession_embed(row.number, &row.content, None, &now)).unwrap();
    assert_eq!(norm(&post), norm(&g["post"]));
    assert!(post["timestamp"].as_str().is_some_and(|t| t.starts_with(&now.format("%Y-%m-%dT%H:%M").to_string())), "stamped with the posting time");
    assert_eq!(norm(&serde_json::to_value(conf::build_confession_embed(row.number, &row.content, Some("confession.png"), &now)).unwrap()), norm(&g["postImage"]));
    assert_eq!(norm(&serde_json::to_value(conf::build_confession_embed(row.number, &"z".repeat(4100), None, &now)).unwrap()), norm(&g["postLong"]));
    let review = serde_json::to_value(conf::build_review_embed(&row, None)).unwrap();
    assert_eq!(norm(&review), norm(&g["review"]));
    assert_eq!(review["timestamp"], g["review"]["timestamp"], "staff see when it was submitted");
    assert_eq!(norm(&serde_json::to_value(conf::build_review_embed(&row, Some("confession.gif"))).unwrap()), norm(&g["reviewImage"]));
    assert_eq!(norm(&serde_json::to_value(conf::build_review_components(&row.id)).unwrap()), norm(&g["reviewComponents"]));
    assert_eq!(norm(&serde_json::to_value(conf::build_confession_modal()).unwrap()), norm(&g["modal"]));
    let panel = conf::build_panel_payload();
    assert_eq!(norm(&serde_json::to_value(panel.embeds.clone().unwrap()).unwrap()), norm(&g["panel"]["embeds"]));
    assert_eq!(norm(&serde_json::to_value(panel.components.clone().unwrap()).unwrap()), norm(&g["panel"]["components"]));
    assert_eq!(serde_json::to_value(panel.to_message()).unwrap()["allowed_mentions"]["parse"], json!([]));
    for v in [&post, &review] {
        assert!(!v.to_string().contains(&row.author_id));
    }
}

#[test]
fn image_problems_match_the_js() {
    let g = golden("confessions");
    for case in g["imageProblems"].as_array().unwrap() {
        let a = conf::AttachmentInfo {
            content_type: case["attachment"]["contentType"].as_str().map(String::from),
            size: case["attachment"]["size"].as_u64().unwrap(),
            url: String::new(),
        };
        let got = conf::image_problem(case["allow"].as_bool().unwrap(), Some(&a)).map(|e| no_ts(serde_json::to_value(e).unwrap()));
        assert_eq!(json!(got), case["problem"], "{case}");
    }
    assert!(conf::image_problem(false, None).is_none(), "nothing attached");
}

#[test]
fn err_tag_names_and_codes_only() {
    let db: anyhow::Error = bot_db::DbError::Timeout("authorId 600000000000000001 'a secret'".into()).into();
    assert_eq!(conf::err_tag(&db), "DbError");
    let io: anyhow::Error = feat_community_a::io::IoError::new(Some(50013), "Missing Permissions").into();
    assert_eq!(conf::err_tag(&io), "DiscordAPIError 50013");
    assert_eq!(conf::err_tag(&anyhow::anyhow!("authorId 600000000000000001")), "Error");
}

#[tokio::test]
async fn deleting_the_review_channel_unsets_both_channels() {
    let t = TestApp::new();
    let s = t.settings().update_settings("930000000000000093", SettingsPatch::new().set("confessionChannelId", "A").set("confessionReviewChannelId", "B")).await.unwrap();
    assert_eq!(conf::channel_delete_update(&s, "A"), vec!["confessionChannelId"]);
    assert_eq!(conf::channel_delete_update(&s, "B"), vec!["confessionReviewChannelId", "confessionChannelId"]);
    assert!(conf::channel_delete_update(&s, "C").is_empty());
}

/// The JS test that greps the service: `authorId` is read only in the cooldown lookup, the block and the reveal, and
/// never on a line that builds an embed or sends a message.
#[test]
fn anonymity_the_author_is_read_only_where_it_must_be() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut uses = Vec::new();
    for f in ["src/confession_service.rs", "src/commands/confession.rs", "src/plugins/confessions.rs"] {
        let src = std::fs::read_to_string(root.join(f)).unwrap();
        for line in src.lines() {
            let t = line.trim_start();
            if t.starts_with("//") {
                continue;
            }
            if line.contains("author_id") || line.contains("authorId") {
                uses.push(line.trim().to_string());
            }
        }
    }
    assert!(uses.len() <= 14, "author_id appears on {} code lines:\n{}", uses.len(), uses.join("\n"));
    for u in &uses {
        for bad in ["embed", "Embed", "send(", "title(", "description("] {
            assert!(!u.contains(bad), "{u}");
        }
    }
}
