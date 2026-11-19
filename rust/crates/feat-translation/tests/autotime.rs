//! Port of `test/autotime.test.js` and the /timestamp, /timezone parts of `test/timestamp.test.js` (the parsers
//! themselves are bot-util's), the golden checks for the time features against JS output, the commands through the
//! router, and the UserTimezone table and `usertz:{u}` cache in both cross-language directions.

use bot_core::command::option_type;
use bot_core::prelude::*;
use bot_core::router::route_interaction;
use bot_core::settings::MemorySettingsStore;
use bot_core::testing::{DbTestApp, FakeGuild, FakeInteraction, TEST_BOT_ID, TestApp, database_url, load_golden, redis_url, run_node};
use bot_db::GuildSettings;
use bot_util::jsre::JsRegex;
use bot_util::time::parse_when_ms;
use bot_util::timezones::{ResolvedTimezone, ZoneSource};
use feat_translation::commands::{autotime as cmd, timestamp as ts};
use feat_translation::plugins::autotime::{self as at, ChannelInfo, TimeMessage};
use feat_translation::user_timezone_service as utz;
use serde_json::{Value, json};
use serenity::all::{GuildId, Permissions as P};

const G: &str = "940000000000000001";
const G2: &str = "940000000000000002";
const CHI: &str = "America/Chicago";
const ROLE_CST: &str = "111111111111111111";
const BOT_ROLE: &str = "820000000000000001";
const ADMIN_ROLE: &str = "820000000000000002";
const ADMIN: &str = "600000000000000009";
/// Tuesday, Oct 20 2026, 10:00 in Chicago (CDT, UTC-5).
const NOW: i64 = 1_792_508_400_000;

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn settings_row(guild: &str, fields: Value) -> GuildSettings {
    MemorySettingsStore::new_row(guild, fields.as_object().cloned().unwrap_or_default()).unwrap()
}

fn base_settings(extra: Value) -> GuildSettings {
    let mut f = json!({
        "autoTimeEnabled": true, "timezone": "Europe/Berlin", "timezoneRoles": [{ "roleId": ROLE_CST, "zone": CHI }], "autoTimeIgnoredChannelIds": [],
    });
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        f[k] = v;
    }
    settings_row(G, f)
}

fn fake_guild(id: &str) -> FakeGuild {
    let bot = P::VIEW_CHANNEL | P::SEND_MESSAGES | P::SEND_MESSAGES_IN_THREADS | P::READ_MESSAGE_HISTORY | P::EMBED_LINKS;
    FakeGuild::new(id, "600000000000000099")
        .role(BOT_ROLE, 1, bot)
        .member(TEST_BOT_ID, &[BOT_ROLE])
        .role(ROLE_CST, 3, P::empty())
        .role(ADMIN_ROLE, 2, P::MANAGE_GUILD)
        .member(ADMIN, &[ADMIN_ROLE])
        .channel("810000000000000001", 0, "general", &[])
        .channel("810000000000000002", 0, "no-perms", &[(BOT_ROLE, false, P::empty(), P::SEND_MESSAGES)])
        .channel("810000000000000003", 4, "A Category", &[])
}

fn app() -> TestApp {
    let t = TestApp::new().with_guilds([G, G2]).with_fake_guilds(&[fake_guild(G)]);
    t.clock.set(NOW);
    t
}

fn channel(id: &str) -> ChannelInfo {
    ChannelInfo { id: id.into(), ..Default::default() }
}

fn message(ch: ChannelInfo, user: &str, content: &str, roles: &[&str]) -> TimeMessage {
    TimeMessage { guild_id: G.into(), channel: ch, author_id: user.into(), content: content.into(), member_roles: roles.iter().map(|r| r.to_string()).collect() }
}

/// A saved zone (or none) the way the JS cache holds it, so the lookup never reaches the (offline) database.
async fn saved_zone(t: &TestApp, user: &str, zone: Option<&str>) {
    t.cache().set(&utz::user_timezone_key(user), &utz::cache_value(zone), 3600).await;
}

async fn reply_of(t: &TestApp, msg: &TimeMessage, settings: Option<&GuildSettings>) -> Option<String> {
    let answer = at::read_message(t, msg, settings, t.clock.now_ms_f64()).await;
    at::reply_content(&answer)
}

trait NowF64 {
    fn now_ms_f64(&self) -> f64;
}

impl NowF64 for bot_core::clock::ManualClock {
    fn now_ms_f64(&self) -> f64 {
        use bot_core::clock::Clock;
        self.now_ms() as f64
    }
}

// ── Golden ─────────────────────────────────────────────────────────────────────

fn golden() -> Value {
    load_golden("feat-translation/golden.json")
}

fn embed_json(e: &serenity::builder::CreateEmbed) -> Value {
    let mut v = serde_json::to_value(e).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.remove("timestamp");
        o.remove("type");
    }
    v
}

#[tokio::test]
async fn golden_time_helpers() {
    let g = golden();
    for case in g["answerText"].as_array().unwrap() {
        let times: Vec<(String, f64)> = case[0].as_array().unwrap().iter().map(|t| (s(&t["text"]).to_string(), t["ms"].as_f64().unwrap())).collect();
        assert_eq!(at::answer_text(&times), s(&case[1]));
    }
    for case in g["zoneHintText"].as_array().unwrap() {
        assert_eq!(at::zone_hint_text(s(&case[0]), case[1].as_bool().unwrap()), s(&case[2]));
    }
    for case in g["isIgnoredChannel"].as_array().unwrap() {
        let settings = case[0].as_object().map(|_| settings_row(G, case[0].clone()));
        let ch = case[1].as_object().map(|c| ChannelInfo {
            id: c.get("id").and_then(Value::as_str).unwrap_or("").into(),
            parent_id: c.get("parentId").and_then(Value::as_str).map(String::from),
            grandparent_id: c.get("parent").and_then(|p| p.get("parentId")).and_then(Value::as_str).map(String::from),
            is_thread: false,
        });
        assert_eq!(at::is_ignored_channel(settings.as_ref(), ch.as_ref()), case[2].as_bool().unwrap(), "{case}");
    }
    assert_eq!(cmd::HOW_IT_WORKS, s(&g["howItWorks"]));
    for case in g["roughRelative"].as_array().unwrap() {
        assert_eq!(ts::rough_relative(case[0].as_f64().unwrap()), s(&case[1]), "{case}");
    }
    for case in g["futureDayExample"].as_array().unwrap() {
        let (words, iso) = ts::future_day_example(s(&case[0]), case[1].as_f64().unwrap());
        assert_eq!(json!({ "words": words, "iso": iso }), case[2]);
    }
    for case in g["suggestWhen"].as_array().unwrap() {
        let got: Vec<Value> =
            ts::suggest_when(s(&case[0]), s(&case[1]), case[2].as_f64().unwrap()).into_iter().map(|(name, value)| json!({ "name": name, "value": value })).collect();
        assert_eq!(Value::Array(got), case[3], "suggestWhen({})", case[0]);
    }
    for case in g["timestampEmbed"].as_array().unwrap() {
        let parsed = parse_when_ms(s(&case["q"]), s(&case["zone"]), case["now"].as_f64().unwrap()).unwrap();
        let r = &case["resolved"];
        let source = match s(&r["source"]) {
            "personal" => ZoneSource::Personal,
            "role" => ZoneSource::Role,
            "server" => ZoneSource::Server,
            _ => ZoneSource::Default,
        };
        let resolved = ResolvedTimezone { zone: s(&r["zone"]).into(), source, role_id: r["roleId"].as_str().map(String::from), role_count: r["roleCount"].as_u64().unwrap() as usize };
        let o = &case["opts"];
        let settings = o["settings"]["embedColor"].as_i64().map(|c| settings_row(G, json!({ "embedColor": c })));
        let e = ts::build_timestamp_embed(&parsed, &resolved, o["style"].as_str().unwrap_or("F"), o["personalUnknown"].as_bool().unwrap_or(false), settings.as_ref());
        let mut want = case["embed"].clone();
        want.as_object_mut().map(|m| m.remove("hasTimestamp"));
        assert_eq!(embed_json(&e), want, "{}", case["q"]);
    }
    let t = app();
    let guild = bot_core::interaction_helpers::cached_guild(&t, GuildId::new(G.parse().unwrap()));
    for case in g["timezoneRolesLine"].as_array().unwrap() {
        let settings = settings_row(G, case[0].clone());
        assert_eq!(cmd::timezone_roles_line(Some(&settings), guild.as_ref()), s(&case[1]));
    }
    assert_eq!(utz::user_timezone_key("123456789012345678"), s(&g["userTimezoneKey"]));
    let styles: Vec<Value> = ts::STYLES.iter().map(|(v, n)| json!({ "value": v, "name": n })).collect();
    assert_eq!(Value::Array(styles), g["styles"]);
    assert_eq!(json!(ts::EXAMPLES), g["examples"]);
}

// ── The author's zone ─────────────────────────────────────────────────────────

#[tokio::test]
async fn author_zone_own_then_role_never_the_server_timezone() {
    let t = app();
    let guild = bot_core::interaction_helpers::cached_guild(&t, GuildId::new(G.parse().unwrap()));
    let s = base_settings(json!({}));
    saved_zone(&t, "U-own", Some("Asia/Tokyo")).await;
    saved_zone(&t, "U-role", None).await;
    saved_zone(&t, "U-none", None).await;
    let m = |u: &str, roles: &[&str]| message(channel("810000000000000001"), u, "x", roles);
    assert_eq!(at::author_zone(&t, &m("U-own", &[ROLE_CST]), guild.as_ref(), &s).await, Some(Some("Asia/Tokyo".into())), "own zone beats the role");
    assert_eq!(at::author_zone(&t, &m("U-role", &[ROLE_CST]), guild.as_ref(), &s).await, Some(Some(CHI.into())));
    assert_eq!(at::author_zone(&t, &m("U-none", &[]), guild.as_ref(), &s).await, Some(None), "the server timezone is not used");
    // The offline database fails the lookup.
    assert_eq!(at::author_zone(&t, &m("U-fail-zone", &[ROLE_CST]), guild.as_ref(), &s).await, Some(Some(CHI.into())), "a failed lookup: the role still counts");
    assert_eq!(at::author_zone(&t, &m("U-fail-zone2", &[]), guild.as_ref(), &s).await, None, "a failed lookup and no role: unknown");
}

// ── The message handler ───────────────────────────────────────────────────────

#[tokio::test]
async fn a_member_with_a_timezone_role_gets_timestamps() {
    let t = app();
    saved_zone(&t, "U-reply", None).await;
    let s = base_settings(json!({}));
    let content = reply_of(&t, &message(channel("810000000000000001"), "U-reply", "raid tomorrow at 8pm?", &[ROLE_CST]), Some(&s)).await.unwrap();
    let re = JsRegex::new(r"^🕒 `tomorrow at 8pm` → <t:\d+:F> \(<t:\d+:R>\)\n-# Everyone sees this in their own timezone\.$", "u");
    assert!(re.test(&content), "{content}");
    assert!(!content.contains("Chicago"), "the author's zone is not shown");
    // tomorrow (Oct 21) 8 pm CDT
    assert!(content.contains("<t:1792630800:F>"), "{content}");
}

#[tokio::test]
async fn members_without_a_zone_get_a_note_once_per_server() {
    let t = app();
    saved_zone(&t, "U-nozone", None).await;
    let s = base_settings(json!({}));
    let plain = reply_of(&t, &message(channel("C-nozone"), "U-nozone", "see you tomorrow 5pm", &[]), Some(&s)).await;
    assert_eq!(plain.as_deref(), Some("⚠️ I can't convert `tomorrow 5pm` without your timezone. Pick a timezone role, or save yours with `/timezone set`."));
    let named = reply_of(&t, &message(channel("C-nozone2"), "U-nozone", "see you tomorrow 5pm EST", &[]), Some(&s)).await.unwrap();
    assert!(!named.contains("timezone set"), "every time named its zone: no note");
    assert_eq!(reply_of(&t, &message(channel("C-nozone3"), "U-nozone", "or fri 6pm", &[]), Some(&s)).await, None, "once per member per server");
    let mut elsewhere = message(channel("C-nozone4"), "U-nozone", "or fri 6pm", &[]);
    elsewhere.guild_id = G2.into();
    assert!(reply_of(&t, &elsewhere, Some(&s)).await.is_some(), "another server gets its own note");
}

#[tokio::test]
async fn the_timezone_note_variants() {
    let t = app();
    for u in ["U-hint1", "U-hint2", "U-hint3", "U-hint5"] {
        saved_zone(&t, u, None).await;
    }
    let no_roles = reply_of(&t, &message(channel("C-hint1"), "U-hint1", "fri 5pm", &[]), Some(&base_settings(json!({ "timezoneRoles": [] })))).await;
    assert_eq!(no_roles.as_deref(), Some("⚠️ I can't convert `fri 5pm` without your timezone. Save yours with `/timezone set`."));
    let gone_role = base_settings(json!({ "timezoneRoles": [{ "roleId": "222222222222222222", "zone": CHI }] }));
    let gone = reply_of(&t, &message(channel("C-hint2"), "U-hint2", "fri 5pm", &[]), Some(&gone_role)).await.unwrap();
    assert!(!gone.contains("timezone role"), "a linked role that was deleted does not count");

    let mixed = reply_of(&t, &message(channel("C-hint3"), "U-hint3", "fri 5pm or sat 6pm EST", &[]), Some(&base_settings(json!({})))).await.unwrap();
    let lines: Vec<&str> = mixed.split('\n').collect();
    assert_eq!(lines.len(), 3);
    assert!(lines[0].starts_with("🕒 `sat 6pm EST` → <t:"));
    assert_eq!(lines[2], at::zone_hint_text("fri 5pm", true));

    assert_eq!(reply_of(&t, &message(channel("C-hint4"), "U-fail-hint", "fri 5pm", &[]), Some(&base_settings(json!({})))).await, None, "a failed lookup: no note");
    let ignored = base_settings(json!({ "autoTimeIgnoredChannelIds": ["C-hint-ign"] }));
    assert_eq!(reply_of(&t, &message(channel("C-hint-ign"), "U-hint5", "fri 5pm", &[]), Some(&ignored)).await, None);
}

#[tokio::test]
async fn ignored_channels_categories_thread_parents_and_no_settings() {
    let t = app();
    let s = base_settings(json!({ "autoTimeIgnoredChannelIds": ["C-ign", "CAT-ign"] }));
    let say = |ch: ChannelInfo| {
        let t = &t;
        let s = &s;
        async move {
            let user = format!("U-{}", ch.id);
            saved_zone(t, &user, None).await;
            reply_of(t, &message(ch, &user, "tomorrow 5pm", &[ROLE_CST]), Some(s)).await.is_some()
        }
    };
    assert!(!say(channel("C-ign")).await, "the channel");
    assert!(!say(ChannelInfo { id: "C-in-cat".into(), parent_id: Some("CAT-ign".into()), ..Default::default() }).await, "a channel of the category");
    assert!(!say(ChannelInfo { id: "T-1".into(), parent_id: Some("C-ign".into()), is_thread: true, ..Default::default() }).await, "a thread of the channel");
    assert!(
        !say(ChannelInfo { id: "T-2".into(), parent_id: Some("C-x".into()), grandparent_id: Some("CAT-ign".into()), is_thread: true }).await,
        "a thread under the category"
    );
    assert!(say(channel("C-ok")).await);
    saved_zone(&t, "U-x", None).await;
    assert_eq!(reply_of(&t, &message(channel("C-nosettings"), "U-x", "tomorrow 5pm", &[ROLE_CST]), None).await, None);
    assert!(!at::is_ignored_channel(None, Some(&channel("C"))));
    assert!(!at::is_ignored_channel(Some(&s), None));
}

#[tokio::test]
async fn cooldowns_one_answer_per_member_and_the_same_instant_once() {
    let t = app();
    for u in ["U-a", "U-b", "U-c"] {
        saved_zone(&t, u, None).await;
    }
    let s = base_settings(json!({}));
    let ch = || channel("C-cool");
    assert!(reply_of(&t, &message(ch(), "U-a", "fri 5pm?", &[ROLE_CST]), Some(&s)).await.is_some());
    assert_eq!(reply_of(&t, &message(ch(), "U-a", "or sat 6pm?", &[ROLE_CST]), Some(&s)).await, None, "the same member within 5 s");
    assert_eq!(reply_of(&t, &message(ch(), "U-b", "yes fri 5pm works", &[ROLE_CST]), Some(&s)).await, None, "the same instant again");
    let other = reply_of(&t, &message(ch(), "U-c", "I can do fri 5pm or sun 3pm", &[ROLE_CST]), Some(&s)).await.unwrap();
    assert!(other.starts_with("🕒 `sun 3pm`"), "only the new time is answered");
    assert_eq!(other.split('\n').count(), 2);
}

/// The cooldowns run on the app clock (`createThrottle` on `Date.now()`): after 5 s the member is answered again,
/// and after 10 min the same instant is answered again in the channel.
#[tokio::test]
async fn cooldowns_expire_on_the_app_clock() {
    let t = app();
    saved_zone(&t, "U-x", None).await;
    let s = base_settings(json!({}));
    let ch = || channel("C-clock");
    assert!(reply_of(&t, &message(ch(), "U-x", "sat 6pm?", &[ROLE_CST]), Some(&s)).await.is_some());
    assert_eq!(reply_of(&t, &message(ch(), "U-x", "sun 7pm?", &[ROLE_CST]), Some(&s)).await, None, "within 5 s");
    t.clock.advance(at::MEMBER_COOLDOWN_MS as i64 + 1);
    assert!(reply_of(&t, &message(ch(), "U-x", "sun 7pm?", &[ROLE_CST]), Some(&s)).await.is_some(), "5 s later the member is answered");
    t.clock.advance(at::MEMBER_COOLDOWN_MS as i64 + 1);
    assert_eq!(reply_of(&t, &message(ch(), "U-x", "sat 6pm again", &[ROLE_CST]), Some(&s)).await, None, "the same instant within 10 min");
    t.clock.advance(at::REPEAT_COOLDOWN_MS as i64);
    let again = reply_of(&t, &message(ch(), "U-x", "sat 6pm again", &[ROLE_CST]), Some(&s)).await;
    assert!(again.is_some_and(|r| r.contains("`sat 6pm`")), "after 10 min the instant is answered again");
}

#[tokio::test]
async fn times_to_answer_caps_and_skips_stamped_messages() {
    let t = app();
    saved_zone(&t, "U-cap", None).await;
    saved_zone(&t, "U-cap2", None).await;
    let s = base_settings(json!({}));
    let many = message(channel("C-cap"), "U-cap", "tomorrow 1pm 2pm 3pm 4pm 5pm", &[ROLE_CST]);
    assert_eq!(at::times_to_answer(&t, &many, Some(&s), NOW as f64).await.len(), bot_config::tables::limits::AUTOTIME_PER_MESSAGE);
    let stamped = message(channel("C-cap"), "U-cap2", "it is <t:1792537200:F>, so fri 5pm", &[ROLE_CST]);
    assert!(at::times_to_answer(&t, &stamped, Some(&s), NOW as f64).await.is_empty());
}

#[tokio::test]
async fn handle_message_checks_the_bot_permissions_before_replying() {
    let t = app();
    saved_zone(&t, "U-p1", None).await;
    saved_zone(&t, "U-p2", None).await;
    let s = base_settings(json!({}));
    let sent: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    for (ch, user) in [("810000000000000001", "U-p1"), ("810000000000000002", "U-p2")] {
        let r = sent.clone();
        let msg = message(channel(ch), user, "tomorrow 5pm", &[ROLE_CST]);
        at::handle_message(&t, &msg, Some(&s), move |content| async move {
            r.lock().unwrap().push(content);
            Ok(())
        })
        .await;
    }
    assert_eq!(sent.lock().unwrap().len(), 1, "no Send Messages in the second channel: no reply");
    assert_eq!(at::reply_needs(&ChannelInfo { is_thread: true, ..Default::default() })[1], "SendMessagesInThreads");
}

#[tokio::test]
async fn the_hook_filter_uses_the_memoised_settings() {
    let t = app();
    let ch = |id: &str| channel(id);
    assert!(!at::may_answer(&t, "940000000000000009", "hello", &ch("C")));
    assert!(at::may_answer(&t, "940000000000000009", "fri 5pm", &ch("C-ign")), "no memo: the handler decides");
    t.settings()
        .update_settings("940000000000000009", SettingsPatch::new().set("autoTimeEnabled", true).set("autoTimeIgnoredChannelIds", json!(["C-ign", "CAT-ign"])))
        .await
        .unwrap();
    assert!(at::may_answer(&t, "940000000000000009", "fri 5pm", &ch("C-any")));
    assert!(!at::may_answer(&t, "940000000000000009", "fri 5pm", &ch("C-ign")), "an ignored channel never reaches the handler");
    let thread = ChannelInfo { id: "T".into(), parent_id: Some("C-x".into()), grandparent_id: Some("CAT-ign".into()), is_thread: true };
    assert!(!at::may_answer(&t, "940000000000000009", "fri 5pm", &thread), "nor a thread under an ignored category");
    assert!(!at::may_answer(&t, "940000000000000009", "hello", &ch("C-any")));
}

#[tokio::test]
async fn channel_info_reads_the_parent_chain_from_the_cache() {
    let guild = fake_guild(G).channel("810000000000000010", 0, "in-cat", &[]).thread("810000000000000011", 11, "810000000000000010");
    let mut json = guild.json();
    // The text channel sits in the category.
    for c in json["channels"].as_array_mut().unwrap() {
        if c["id"] == "810000000000000010" {
            c["parent_id"] = json!("810000000000000003");
        }
    }
    let t = TestApp::new().with_guilds([G]).with_discord_guilds(&[json]);
    let info = ChannelInfo::resolve(&t, GuildId::new(G.parse().unwrap()), serenity::all::ChannelId::new(810000000000000011), None);
    assert_eq!(info.parent_id.as_deref(), Some("810000000000000010"));
    assert_eq!(info.grandparent_id.as_deref(), Some("810000000000000003"));
    assert!(info.is_thread);
}

// ── User timezone memo ────────────────────────────────────────────────────────

#[tokio::test]
async fn user_timezone_memo_cache_and_unknown() {
    let t = app();
    utz::reset_user_timezone_memo(&t);
    assert_eq!(utz::peek_user_timezone(&t, "1"), None);
    assert_eq!(utz::get_user_timezone(&t, "1", true).await, None, "cacheOnly miss: unknown");
    // A JS-written cache value is a hit; an invalid zone is none; a value without `zone` is a miss.
    t.cache_backend.put_raw("usertz:2", r#"{"zone":"Asia/Tokyo"}"#, Some(3_600_000));
    assert_eq!(utz::get_user_timezone(&t, "2", true).await, Some(Some("Asia/Tokyo".into())));
    assert_eq!(utz::peek_user_timezone(&t, "2"), Some(Some("Asia/Tokyo".into())));
    t.clock.advance(utz::MEMO_TTL_MS + 1);
    assert_eq!(utz::peek_user_timezone(&t, "2"), None, "the memo expires after 30 s");
    t.cache_backend.put_raw("usertz:3", r#"{"zone":"Mars/Base"}"#, Some(3_600_000));
    assert_eq!(utz::get_user_timezone(&t, "3", true).await, Some(None));
    t.cache_backend.put_raw("usertz:4", r#"{}"#, Some(3_600_000));
    assert_eq!(utz::get_user_timezone(&t, "4", true).await, None);
    assert_eq!(utz::get_user_timezone(&t, "4", false).await, None, "the offline database fails: unknown, never an error");
    assert_eq!(utz::read_cache_value(&json!([])), None);
    assert_eq!(serde_json::to_string(&utz::cache_value(None)).unwrap(), r#"{"zone":null}"#);
}

// ── The commands ──────────────────────────────────────────────────────────────

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_translation::register(&mut reg);
    reg
}

fn admin_slash(group: Option<&str>, sub: &str) -> FakeInteraction {
    let i = FakeInteraction::slash("autotime").guild(G).user(ADMIN).roles(&[ADMIN_ROLE]).permissions(P::MANAGE_GUILD);
    match group {
        Some(g) => i.group(g, sub),
        None => i.subcommand(sub),
    }
}

#[tokio::test]
async fn autotime_command_enable_status_and_ignore_list() {
    let t = app();
    t.install(registry());
    route_interaction(&t, admin_slash(None, "enable").build()).await;
    let calls = t.responses.calls();
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"]);
    assert!(!calls[0].ephemeral(), "admin config commands answer publicly");
    assert_eq!(calls[1].embed_title(), Some("✅  Auto Timestamps On"));
    assert_eq!(calls[1].embed_description().map(String::from), Some(format!("{}\n\nNo timezone roles are linked yet. `/reactionrole create template:timezones-us` (or `timezones-world`) posts a panel that creates and links them, or link roles you have with `/settings timezone-role add`.", cmd::HOW_IT_WORKS)));
    assert!(t.settings().get_settings(G).await.unwrap().auto_time_enabled);

    t.responses.clear();
    route_interaction(&t, admin_slash(Some("ignore"), "add").option("channel", option_type::CHANNEL, json!("810000000000000003")).build()).await;
    assert_eq!(t.responses.calls()[1].embed_description(), Some("Times typed in any channel of **A Category** will not be answered."));
    t.responses.clear();
    route_interaction(&t, admin_slash(Some("ignore"), "add").option("channel", option_type::CHANNEL, json!("810000000000000003")).build()).await;
    assert_eq!(t.responses.calls()[1].embed_title(), Some("⚠️  Already Ignored"));
    t.responses.clear();
    route_interaction(&t, admin_slash(Some("ignore"), "add").option("channel", option_type::CHANNEL, json!("810000000000000099")).build()).await;
    assert_eq!(t.responses.calls()[1].embed_description(), Some("Times typed in <#810000000000000099> will not be answered."));

    t.responses.clear();
    route_interaction(&t, admin_slash(None, "status").build()).await;
    let call = &t.responses.calls()[1];
    assert_eq!(call.embed_title(), Some("🕒 Auto Timestamps"));
    let fields: Vec<(String, String)> =
        call.body["embeds"][0]["fields"].as_array().unwrap().iter().map(|f| (s(&f["name"]).into(), s(&f["value"]).into())).collect();
    assert_eq!(
        fields,
        vec![
            ("Status".into(), "✅ On".into()),
            ("Ignored channels (2/100)".into(), "<#810000000000000003>, #deleted-channel (`810000000000000099`)".into()),
        ]
    );

    t.responses.clear();
    route_interaction(&t, admin_slash(None, "disable").build()).await;
    assert_eq!(t.responses.calls()[1].embed_title(), Some("✅  Auto Timestamps Off"));
    t.responses.clear();
    route_interaction(&t, admin_slash(Some("ignore"), "remove").option("channel", option_type::CHANNEL, json!("810000000000000099")).build()).await;
    assert_eq!(
        t.responses.calls()[1].embed_description(),
        Some("Times typed in <#810000000000000099> are answered again.\n\n⚠️ The **Auto Timestamps** module is off. Turn it on with `/autotime enable`.")
    );
    t.responses.clear();
    route_interaction(&t, admin_slash(Some("ignore"), "remove").option("channel", option_type::CHANNEL, json!("810000000000000099")).build()).await;
    assert_eq!(t.responses.calls()[1].embed_description(), Some("<#810000000000000099> is not on the ignore list."));
    assert_eq!(t.settings().get_settings(G).await.unwrap().auto_time_ignored_channel_ids, vec!["810000000000000003".to_string()]);
}

#[tokio::test]
async fn autotime_ignore_list_has_a_cap() {
    let t = app();
    t.install(registry());
    let full: Vec<String> = (0..100).map(|n| format!("8200000000000{n:05}")).collect();
    t.settings().update_settings(G, SettingsPatch::new().set("autoTimeIgnoredChannelIds", json!(full))).await.unwrap();
    route_interaction(&t, admin_slash(Some("ignore"), "add").option("channel", option_type::CHANNEL, json!("810000000000000001")).build()).await;
    assert_eq!(t.responses.calls()[1].embed_title(), Some("❌  Limit Reached"));
    assert_eq!(t.responses.calls()[1].embed_description(), Some("You can ignore at most 100 channels. Remove one with `/autotime ignore remove` first."));
}

fn member_slash(name: &str) -> FakeInteraction {
    FakeInteraction::slash(name).guild(G).user("600000000000000077").roles(&[ROLE_CST])
}

#[tokio::test]
async fn timestamp_command_private_public_and_unreadable() {
    let t = app();
    t.install(registry());
    t.settings_store.put(base_settings(json!({})));
    saved_zone(&t, "600000000000000077", None).await;
    route_interaction(&t, member_slash("timestamp").option("when", option_type::STRING, json!("fri 7pm")).option("style", option_type::STRING, json!("R")).build()).await;
    let calls = t.responses.calls();
    assert_eq!(t.responses.names(), vec!["reply"]);
    assert!(calls[0].ephemeral());
    assert_eq!(calls[0].embed_title(), Some("🕒 Timestamp"));
    let d = calls[0].embed_description().unwrap_or("").to_string();
    // Friday Oct 23 2026, 7 pm CDT, read in the member's role zone.
    assert!(d.starts_with("**Fri, Oct 23, 2026, 7:00 PM** · America/Chicago (UTC-05:00)\nRead in your <@&111111111111111111> role."), "{d}");
    let codes: Vec<String> = JsRegex::new(r"`<t:\d+:([tTdDfFR])>`", "").find_all(&bot_util::jsre::u16s(&d)).iter().map(|m| m.group(&bot_util::jsre::u16s(&d), 1).unwrap_or_default()).collect();
    assert_eq!(codes, vec!["R", "F", "f", "D", "d", "t", "T"]);

    t.responses.clear();
    route_interaction(&t, member_slash("timestamp").option("when", option_type::STRING, json!("fri 7pm")).option("public", option_type::BOOLEAN, json!(true)).build()).await;
    let call = &t.responses.calls()[0];
    assert!(!call.ephemeral());
    assert_eq!(call.content(), Some("🕒 <t:1792800000:F> (<t:1792800000:R>)"));

    t.responses.clear();
    route_interaction(&t, member_slash("timestamp").option("when", option_type::STRING, json!("banana `7pm`")).build()).await;
    let call = &t.responses.calls()[0];
    assert_eq!(call.embed_title(), Some("❌  Could Not Read That Time"));
    assert_eq!(
        call.embed_description(),
        Some("I couldn't read `banana 7pm` as a time.\nTry `fri 7pm`, `tomorrow 18:30`, `next mon 9am`, `oct 27 7:30pm`, `2026-10-27 19:00`, `in 2h` or `7pm CST`.\nTimes are read in **America/Chicago (UTC-05:00)**, your <@&111111111111111111> role.")
    );

    t.responses.clear();
    route_interaction(&t, FakeInteraction::autocomplete("timestamp").guild(G).user("600000000000000077").roles(&[ROLE_CST]).focused("when", "fri 7pm").build()).await;
    let choices = t.responses.calls()[0].choices();
    assert_eq!(choices[0]["value"], "fri 7pm");
    assert_eq!(choices[0]["name"], "Fri, Oct 23, 2026, 7:00 PM · America/Chicago (UTC-05:00) · in 3 days");
}

#[tokio::test]
async fn timezone_autocomplete_puts_the_abbreviation_first() {
    let t = app();
    t.install(registry());
    route_interaction(&t, FakeInteraction::autocomplete("timezone").guild(G).subcommand("set").focused("zone", "cst").build()).await;
    let choices = t.responses.calls()[0].choices();
    assert_eq!(choices[0]["value"], CHI);
    assert!(s(&choices[0]["name"]).ends_with("· CST"));
}

// ── Database: /timezone and the UserTimezone table, both directions ─────────────

async fn db_app() -> Option<DbTestApp> {
    let url = database_url()?;
    let t = match redis_url() {
        Some(r) => DbTestApp::with_redis(&url, &r).await,
        None => DbTestApp::new(&url).await,
    }
    .with_guilds([G]);
    t.reset_guilds(&[G]).await;
    Some(t)
}

async fn forget(t: &DbTestApp, user: &str) {
    sqlx::query(r#"DELETE FROM "UserTimezone" WHERE "userId" = $1"#).bind(user).execute(t.db()).await.unwrap();
    t.cache().del_one(&utz::user_timezone_key(user)).await;
    utz::reset_user_timezone_memo(t);
}

#[tokio::test]
async fn js_and_rust_read_each_others_user_timezones() {
    let Some(t) = db_app().await else { return };
    let (u1, u2) = ("640000000000000001", "640000000000000002");
    forget(&t, u1).await;
    forget(&t, u2).await;

    // JS writes, Rust reads (Redis first, then the row once the cache entry is gone).
    assert_eq!(run_node("rust/fixtures/gen/feat-translation-js.mjs", &["tz-set", u1, "Asia/Tokyo"]), json!({ "ok": true }));
    assert_eq!(utz::get_user_timezone(&t, u1, false).await, Some(Some("Asia/Tokyo".into())));
    t.cache().del_one(&utz::user_timezone_key(u1)).await;
    utz::reset_user_timezone_memo(&t);
    assert_eq!(utz::get_user_timezone(&t, u1, false).await, Some(Some("Asia/Tokyo".into())), "from the row");
    if redis_url().is_some() {
        assert_eq!(run_node("rust/fixtures/gen/feat-translation-js.mjs", &["cache-get", &utz::user_timezone_key(u1)]), json!({ "zone": "Asia/Tokyo" }), "the miss refilled the cache in the JS shape");
    }

    // Rust writes, JS reads; clearing writes the cached miss both sides read as none.
    utz::set_user_timezone(&t, u2, CHI).await.unwrap();
    assert_eq!(run_node("rust/fixtures/gen/feat-translation-js.mjs", &["tz-get", u2]), json!(CHI));
    utz::set_user_timezone(&t, u2, "Europe/London").await.unwrap();
    assert_eq!(run_node("rust/fixtures/gen/feat-translation-js.mjs", &["tz-get", u2]), json!("Europe/London"), "an upsert");
    assert!(utz::clear_user_timezone(&t, u2).await.unwrap());
    assert!(!utz::clear_user_timezone(&t, u2).await.unwrap(), "nothing left to clear");
    assert_eq!(run_node("rust/fixtures/gen/feat-translation-js.mjs", &["tz-get", u2]), Value::Null);
    if redis_url().is_some() {
        assert_eq!(run_node("rust/fixtures/gen/feat-translation-js.mjs", &["cache-get", &utz::user_timezone_key(u2)]), json!({ "zone": null }));
    }
    assert_eq!(run_node("rust/fixtures/gen/feat-translation-js.mjs", &["tz-clear", u1]), json!(true));
    t.cache().del_one(&utz::user_timezone_key(u1)).await;
    utz::reset_user_timezone_memo(&t);
    assert_eq!(utz::get_user_timezone(&t, u1, false).await, Some(None));
    forget(&t, u1).await;
    forget(&t, u2).await;
}

#[tokio::test]
async fn timezone_command_set_show_clear() {
    let Some(t) = db_app().await else { return };
    let user = "640000000000000003";
    forget(&t, user).await;
    t.install(registry());
    let tz = |sub: &str| FakeInteraction::slash("timezone").guild(G).user(user).subcommand(sub);

    route_interaction(&t, tz("set").option("zone", option_type::STRING, json!("IST")).build()).await;
    assert_eq!(t.responses.names(), vec!["reply"]);
    assert_eq!(t.responses.calls()[0].embed_title(), Some("❌  Ambiguous Timezone"));
    assert!(t.responses.calls()[0].ephemeral());

    t.responses.clear();
    route_interaction(&t, tz("set").option("zone", option_type::STRING, json!("cst")).build()).await;
    let calls = t.responses.calls();
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"]);
    assert!(calls[0].ephemeral());
    assert_eq!(calls[1].embed_title(), Some("✅  Timezone Saved"));
    let d = calls[1].embed_description().unwrap_or("").to_string();
    assert!(d.starts_with("Your timezone is now **America/Chicago (UTC-0"), "{d}");
    assert!(d.ends_with("CST is read as America/Chicago time, so it follows daylight saving."), "{d}");
    assert_eq!(run_node("rust/fixtures/gen/feat-translation-js.mjs", &["tz-get", user]), json!(CHI));

    t.responses.clear();
    route_interaction(&t, tz("show").build()).await;
    let d = t.responses.calls()[1].embed_description().unwrap_or("").to_string();
    assert!(d.starts_with("/timestamp reads your times in **America/Chicago (UTC-0"), "{d}");
    assert!(d.contains("from your saved timezone (`/timezone set`)."));
    assert!(d.contains("• Saved timezone: `America/Chicago`\n• Timezone role: none\n• Server timezone: `UTC`"), "{d}");

    t.responses.clear();
    route_interaction(&t, tz("clear").build()).await;
    let call = &t.responses.calls()[1];
    assert_eq!(call.embed_title(), Some("✅  Timezone Cleared"));
    assert_eq!(call.embed_description(), Some("Your saved timezone is gone. /timestamp uses **UTC**, from this server's timezone."));
    t.responses.clear();
    route_interaction(&t, tz("clear").build()).await;
    assert_eq!(t.responses.calls()[1].embed_title(), Some("ℹ️  No Saved Timezone"));
    forget(&t, user).await;
    t.reset_guilds(&[G]).await;
}
