//! Port of `test/antiraid.test.js`, plus the cross-language checks of the `AntiRaidState` row and the unlock job.
//!
//! Offline: the pure rules (join window, bit maths, account age), the embeds against the JS, the permission problems
//! and the plugin declaration. With `TEST_DATABASE_URL`: lockdown start and end against the real `AntiRaidState`
//! table, a fake guild and a fake REST (bits saved before the role is edited, idempotent end, retry after a failed
//! restore, one lockdown per burst), the join handler, the unlock job, the End button, the `/antiraid` command, and
//! both directions of the JS round trip.

use std::sync::{Arc, Mutex};

use bot_core::prelude::*;
use bot_core::testing::{DbTestApp, FakeGuild, FakeInteraction, FakeRest, TEST_BOT_ID, TestApp, database_url, run_node};
use feat_automod::anti_raid_service::{self as svc, Ended, SEND_MASK, Started, StartInput, raid};
use feat_automod::plugins::antiraid as plug;
use feat_moderation::moderation_service::{ModActionInput, ModCode, ModError};
use serde_json::{Value, json};
use serenity::all::{GuildId, Member, Permissions};

const OWNER: &str = "956006000000000003";
const BOT_ROLE: &str = "956006000000000020";
const MOD_ROLE: &str = "956006000000000021";
const ADMIN_ROLE: &str = "956006000000000022";
const MODERATOR: &str = "956006000000000007";
const MEMBER: &str = "956006000000000008";
const ADMIN: &str = "956006000000000009";

fn everyone() -> Permissions {
    Permissions::VIEW_CHANNEL
        | Permissions::SEND_MESSAGES
        | Permissions::SEND_MESSAGES_IN_THREADS
        | Permissions::CREATE_PUBLIC_THREADS
        | Permissions::ADD_REACTIONS
        | Permissions::READ_MESSAGE_HISTORY
}

fn bot_send() -> Permissions {
    Permissions::from_bits_truncate(SEND_MASK)
}

fn alert_of(g: &str) -> String {
    format!("{}99", &g[..g.len() - 2])
}

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_automod::register(&mut reg);
    reg
}

// ── Pure rules ────────────────────────────────────────────────────────────────

#[test]
fn clamp_int_harsher_and_the_moderation_source() {
    assert_eq!(svc::clamp_int(Some(500.0), raid::THRESHOLD, 10), 100);
    assert_eq!(svc::clamp_int(Some(1.0), raid::THRESHOLD, 10), 3);
    assert_eq!(svc::clamp_int(None, raid::THRESHOLD, 10), 10);
    assert_eq!(svc::clamp_int(Some(f64::NAN), raid::THRESHOLD, 10), 10);
    assert_eq!(svc::clamp_int(Some(12.9), raid::THRESHOLD, 10), 12);
    assert_eq!(svc::harsher(Some("ALERT"), Some("KICK")), Some("KICK"));
    assert_eq!(svc::harsher(Some("BAN"), Some("KICK")), Some("BAN"));
    assert_eq!(svc::harsher(Some("KICK"), Some("BAN")), Some("BAN"));
    assert_eq!(svc::harsher(None, Some("ALERT")), Some("ALERT"));
    assert_eq!(svc::harsher(Some("KICK"), None), Some("KICK"));
    assert_eq!(svc::harsher(None, None), None);
    assert_eq!(svc::moderation_source(), "antiraid");
    assert!(feat_moderation::moderation_service::MOD_SOURCES.contains(&svc::moderation_source()));
}

#[test]
fn the_chat_lock_takes_only_the_four_send_bits() {
    let view = Permissions::VIEW_CHANNEL.bits();
    assert_eq!(SEND_MASK & view, 0);
    let (taken, rest) = svc::split_send(everyone().bits());
    assert_eq!(taken, (Permissions::SEND_MESSAGES | Permissions::SEND_MESSAGES_IN_THREADS | Permissions::CREATE_PUBLIC_THREADS).bits());
    assert_eq!(rest, (Permissions::VIEW_CHANNEL | Permissions::ADD_REACTIONS | Permissions::READ_MESSAGE_HISTORY).bits());
    let t = (Permissions::SEND_MESSAGES | Permissions::CREATE_PUBLIC_THREADS).bits();
    let current = (Permissions::VIEW_CHANNEL | Permissions::ADD_REACTIONS | Permissions::EMBED_LINKS).bits();
    assert_eq!(svc::restored_bits(current, Some(&t.to_string())), current | t);
    assert_eq!(svc::restored_bits(current, None), current);
    assert_eq!(svc::restored_bits(current, Some(&(t | Permissions::ADMINISTRATOR.bits()).to_string())), current | t);
    assert_eq!(svc::restored_bits(current, Some("garbage")), current);
    assert_eq!(svc::parse_prior(None), None);
    assert_eq!(svc::parse_prior(Some("0")), Some(0));
}

#[tokio::test]
async fn the_join_window_trips_at_the_threshold_and_clears_after_a_trip() {
    let t = TestApp::new();
    let t0 = 1_000_000;
    let r = |g: &str, at: i64, now: i64| svc::record_join(&t, g, at, now, 3, 10);
    assert_eq!(r("W1", t0, t0), svc::JoinWindow { count: 1, tripped: false });
    assert_eq!(r("W1", t0 + 4000, t0 + 4000), svc::JoinWindow { count: 2, tripped: false });
    assert_eq!(r("W1", t0 + 9000, t0 + 9000), svc::JoinWindow { count: 3, tripped: true });
    assert_eq!(r("W1", t0 + 9500, t0 + 9500), svc::JoinWindow { count: 1, tripped: false }, "the burst is spent");
    r("W2", t0, t0);
    r("W2", t0 + 1000, t0 + 1000);
    assert!(!r("W2", t0 + 20_000, t0 + 20_000).tripped);
    assert_eq!(r("W3", t0 - 60_000, t0).count, 0);
    r("W4", t0, t0);
    r("W4", t0, t0);
    assert_eq!(r("W5", t0, t0).count, 1);
}

#[test]
fn detection_settings_are_clamped_and_account_age_is_in_whole_days() {
    let s = bot_db::GuildSettings { anti_raid_join_threshold: 1, anti_raid_join_seconds: 9999, anti_raid_lockdown_minutes: 1, ..Default::default() };
    assert_eq!(svc::detection_of(Some(&s)), svc::Detection { threshold: 3, seconds: 300, minutes: 5 });
    assert_eq!(svc::detection_of(None), svc::Detection { threshold: 10, seconds: 10, minutes: 30 });
    let now = 1_800_000_000_000i64;
    let day = 86_400_000.0;
    let seven = bot_db::GuildSettings { anti_raid_min_account_age_days: 7, ..Default::default() };
    assert_eq!(svc::young_account(Some(now as f64 - 2.5 * day), Some(&seven), now), Some(svc::Young { age_days: 2, min_days: 7 }));
    assert_eq!(svc::young_account(Some(now as f64 - 8.0 * day), Some(&seven), now), None);
    assert_eq!(svc::young_account(Some(now as f64 - day), Some(&bot_db::GuildSettings::default()), now), None);
    assert_eq!(svc::young_account(None, Some(&seven), now), None);
}

#[test]
fn is_active_needs_a_start_and_a_future_end() {
    use chrono::{TimeZone, Utc};
    let now = 1_800_000_000_000i64;
    let at = |ms: i64| Utc.timestamp_millis_opt(ms).single();
    let row = |a: Option<i64>, u: Option<i64>| bot_db::AntiRaidState {
        guild_id: "G".into(),
        locked_at: a.and_then(at),
        locked_until: u.and_then(at),
        locked_by_id: None,
        reason: None,
        prior_everyone_send: None,
        invites_paused_until: None,
        updated_at: Utc::now(),
    };
    assert!(!svc::is_active(None, now));
    assert!(!svc::is_active(Some(&row(None, None)), now));
    assert!(!svc::is_active(Some(&row(Some(now - 1000), Some(now - 1))), now));
    assert!(svc::is_active(Some(&row(Some(now - 1000), Some(now + 1000))), now));
}

// ── Embeds and texts against the JS ───────────────────────────────────────────

fn js_calls(calls: &[Value]) -> Vec<Value> {
    let out = run_node("rust/fixtures/gen/feat-automod-js.mjs", &["call", &serde_json::to_string(calls).unwrap_or_default()]);
    out.as_array().cloned().unwrap_or_default().into_iter().map(|r| r.get("ok").cloned().unwrap_or(json!({ "error": r }))).collect()
}

fn state_of(locked_by: Option<&str>, reason: Option<&str>) -> bot_db::AntiRaidState {
    use chrono::TimeZone;
    bot_db::AntiRaidState {
        guild_id: "G".into(),
        locked_at: chrono::Utc.timestamp_millis_opt(1_767_225_600_000).single(),
        locked_until: chrono::Utc.timestamp_millis_opt(1_767_227_400_000).single(),
        locked_by_id: locked_by.map(str::to_string),
        reason: reason.map(str::to_string),
        prior_everyone_send: None,
        invites_paused_until: None,
        updated_at: chrono::Utc::now(),
    }
}

fn settings_of(over: Value) -> bot_db::GuildSettings {
    let mut base = json!({
        "guildId": "G", "antiRaidEnabled": true, "antiRaidJoinThreshold": 10, "antiRaidJoinSeconds": 10, "antiRaidAction": "ALERT",
        "antiRaidPauseInvites": false, "antiRaidLockChat": false, "antiRaidLockdownMinutes": 30, "antiRaidMinAccountAgeDays": 0,
        "antiRaidYoungAction": "KICK", "antiRaidAlertChannelId": null, "modLogChannelId": null,
    });
    if let (Some(b), Some(o)) = (base.as_object_mut(), over.as_object()) {
        for (k, v) in o {
            b.insert(k.clone(), v.clone());
        }
    }
    let row = bot_core::settings::MemorySettingsStore::new_row("G", base.as_object().cloned().unwrap_or_default());
    row.unwrap_or_else(|e| panic!("settings row: {e}"))
}

/// The parts of an embed both sides set. A field without `inline` is `inline: false` to Discord (discord.js leaves the
/// key out, serenity writes `false`).
fn embed_core(v: &Value) -> Value {
    let fields: Vec<Value> = v["fields"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|f| json!({ "name": f["name"], "value": f["value"], "inline": f.get("inline").and_then(Value::as_bool).unwrap_or(false) }))
        .collect();
    json!({ "title": v["title"], "description": v["description"], "color": v["color"], "fields": fields, "footer": v["footer"] })
}

#[test]
fn texts_and_embeds_agree_with_the_js() {
    let effects = |chat: &'static str, invites: &'static str, errors: &[&str]| svc::Effects { chat, invites, errors: errors.iter().map(|s| s.to_string()).collect() };
    let mut calls = Vec::new();
    let mut want = Vec::new();
    // Start alerts.
    let cases = [
        (state_of(None, None), json!({ "antiRaidAction": "KICK" }), effects("locked", "paused", &[]), Some(svc::Trigger { joins: 12, seconds: 10 }), vec!["Kicking members: I need **Kick Members**.".to_string()]),
        (state_of(Some("123456789012345678"), Some("spam wave")), json!({}), effects("partial", "failed", &["Invites not paused: I need **Manage Server**.", "Kicking members: I need **Kick Members**."]), None, vec!["Kicking members: I need **Kick Members**.".to_string()]),
        (state_of(None, None), json!({ "antiRaidAction": "BAN" }), effects("nothing", "already", &[]), None, vec![]),
    ];
    for (state, over, eff, trigger, problems) in &cases {
        let js_state = json!({ "lockedAt": "2026-01-01T00:00:00.000Z", "lockedUntil": "2026-01-01T00:30:00.000Z", "lockedById": state.locked_by_id, "reason": state.reason });
        let js_eff = json!({ "chat": eff.chat, "invites": eff.invites, "errors": eff.errors });
        let js_trigger = trigger.map(|t| json!({ "joins": t.joins, "seconds": t.seconds })).unwrap_or(Value::Null);
        calls.push(json!(["startEmbed", js_state, over, js_eff, js_trigger, problems]));
        let s = settings_of(over.clone());
        want.push(serde_json::to_value(svc::build_start_embed(state, Some(&s), Some(eff), *trigger, problems)).unwrap_or_default());
    }
    // End notes.
    let ended = |tally: Option<svc::Tally>, pending: bool, eff: svc::Effects| Ended { tally, pending_restore: pending, effects: eff, ..Default::default() };
    let end_cases = [
        ("expired", None, ended(Some(svc::Tally { kicked: 4, banned: 2, failed: 1, flagged: 0 }), false, effects("restored", "resumed", &[]))),
        ("manual", Some("123456789012345678"), ended(None, true, effects("failed", "none", &["Could not give @everyone back Send Messages."]))),
        ("manual", None, ended(Some(svc::Tally { kicked: 0, banned: 0, failed: 0, flagged: 3 }), false, effects("partialRestore", "foreign", &[]))),
    ];
    for (how, by, res) in &end_cases {
        let js_res = json!({
            "tally": res.tally.map(|t| json!({ "kicked": t.kicked, "banned": t.banned, "failed": t.failed, "flagged": t.flagged })),
            "pendingRestore": res.pending_restore,
            "effects": { "chat": res.effects.chat, "invites": res.effects.invites, "errors": res.effects.errors },
        });
        calls.push(json!(["endEmbed", how, by, js_res]));
        want.push(serde_json::to_value(svc::build_end_embed(how, *by, Some(res))).unwrap_or_default());
    }
    for (age, more, action) in [(1, 2, "KICK"), (0, 0, "ALERT"), (5, 1, "BAN")] {
        calls.push(json!(["youngEmbed", "5", "x#0", age, 7, action, more]));
        let report = svc::YoungReport { user_id: "5".into(), user_tag: "x#0".into(), age_days: age, min_days: 7, action: action.into() };
        want.push(serde_json::to_value(svc::build_young_embed(&report, more as u32)).unwrap_or_default());
    }
    let js = js_calls(&calls);
    for ((call, w), g) in calls.iter().zip(&want).zip(&js) {
        assert_eq!(embed_core(w), embed_core(g), "{call}");
    }

    // The button row, config lines, bit names, priors and clamps.
    let row = serde_json::to_value(svc::end_button_row()).unwrap_or_default();
    let js_row = js_calls(&[json!(["endButtonRow"])]).pop().unwrap_or_default();
    let button = |r: &Value| {
        let b = &r["components"][0];
        json!([r["type"], b["type"], b["style"], b["custom_id"], b["label"], b["emoji"]["name"], b.get("disabled").and_then(Value::as_bool).unwrap_or(false)])
    };
    assert_eq!(button(&row), button(&js_row));
    let mut calls = Vec::new();
    let mut want = Vec::new();
    for over in [
        json!({}),
        json!({ "antiRaidMinAccountAgeDays": 1, "antiRaidYoungAction": "BAN", "antiRaidAlertChannelId": "123456789012345678", "antiRaidPauseInvites": true, "antiRaidLockdownMinutes": 90 }),
        json!({ "antiRaidMinAccountAgeDays": 400, "modLogChannelId": "123456789012345679", "antiRaidLockChat": true, "antiRaidJoinThreshold": 500, "antiRaidAction": "BAN" }),
    ] {
        calls.push(json!(["configLines", over]));
        want.push(json!(svc::config_lines(&settings_of(over.clone()))));
    }
    for bits in [0u64, SEND_MASK, Permissions::SEND_MESSAGES.bits(), (Permissions::CREATE_PRIVATE_THREADS | Permissions::SEND_MESSAGES_IN_THREADS).bits()] {
        calls.push(json!(["bitNames", bits.to_string()]));
        want.push(json!(svc::bit_names(bits)));
    }
    for p in ["2048", "0", "garbage", " 274877906944 ", "", "-1", "0x800", "99999999999999999999999"] {
        calls.push(json!(["parsePrior", p]));
        want.push(json!(svc::parse_prior(Some(p)).map(|b| b.to_string())));
        calls.push(json!(["restoredBits", "1024", p]));
        want.push(json!(svc::restored_bits(1024, Some(p)).to_string()));
    }
    for (v, range, fb) in [(json!(500), "THRESHOLD", 10), (json!("x"), "SECONDS", 10), (json!(12.9), "MINUTES", 30), (json!(-3), "AGE_DAYS", 0)] {
        calls.push(json!(["clampInt", v, range, fb]));
        let r = match range {
            "THRESHOLD" => raid::THRESHOLD,
            "SECONDS" => raid::SECONDS,
            "MINUTES" => raid::MINUTES,
            _ => raid::AGE_DAYS,
        };
        want.push(json!(svc::clamp_int(v.as_f64(), r, fb)));
    }
    let js = js_calls(&calls);
    for ((call, w), g) in calls.iter().zip(&want).zip(&js) {
        assert_eq!(w, g, "{call}");
    }
}

#[tokio::test]
async fn permission_problems_name_what_the_settings_need() {
    let g = "956006000000000101";
    let none = FakeGuild::new(g, OWNER).everyone(everyone()).member(TEST_BOT_ID, &[]);
    let t = TestApp::new().with_fake_guilds(&[none]);
    let gid = GuildId::new(g.parse().unwrap_or(1));
    let s = settings_of(json!({ "antiRaidAction": "BAN", "antiRaidMinAccountAgeDays": 7, "antiRaidYoungAction": "KICK", "antiRaidPauseInvites": true, "antiRaidLockChat": true }));
    let text = svc::permission_problems(&t, gid, Some(&s)).join("\n");
    for needle in ["Pausing invites: I need **Manage Server**", "Locking chat: I need **Manage Roles**", "Kicking members: I need **Kick Members**", "Banning members: I need **Ban Members**"] {
        assert!(text.contains(needle), "{needle}: {text}");
    }
    assert!(text.contains("none of my roles has **Send Messages, Create Public Threads, Send Messages in Threads**"), "{text}");
    let quiet = settings_of(json!({}));
    assert!(svc::permission_problems(&t, gid, Some(&quiet)).is_empty());
    assert_eq!(svc::config_lines(&s).len(), 5);
}

#[tokio::test]
async fn plugin_declaration_and_hook_order() {
    let reg = registry();
    let p = reg.plugins().iter().find(|p| p.id == "antiraid").expect("plugin");
    assert_eq!(p.module, Some(ModuleKey::Antiraid));
    let order = |pt: bot_config::HookPoint| bot_config::HOOK_ORDER.iter().find(|(h, _)| *h == pt).map(|(_, l)| l.to_vec()).unwrap_or_default();
    let add = order(bot_config::HookPoint::GuildMemberAdd);
    let at = |id: &str| add.iter().position(|x| *x == id);
    assert!(at("antiraid") > at("moderation"));
    for later in ["logging", "captcha", "welcome"] {
        assert!(at("antiraid") < at(later), "before {later}");
    }
    assert!(order(bot_config::HookPoint::ChannelDelete).contains(&"antiraid"));
    assert!(plug::may_be_alert_channel(&TestApp::new(), "NO-MEMO", "X"));
}

// ── With the database: lockdowns against a fake guild and a fake REST ─────────

struct Raid {
    t: DbTestApp,
    rest: FakeRest,
    g: &'static str,
    gid: GuildId,
    mods: Arc<Mutex<Vec<ModActionInput>>>,
    result: Arc<Mutex<Option<ModCode>>>,
}

fn fake_guild(g: &str, bot: Permissions, every: Permissions) -> FakeGuild {
    FakeGuild::new(g, OWNER)
        .everyone(every)
        .role(BOT_ROLE, 5, bot)
        .role(MOD_ROLE, 2, Permissions::KICK_MEMBERS)
        .role(ADMIN_ROLE, 3, Permissions::MANAGE_GUILD)
        .member(TEST_BOT_ID, &[BOT_ROLE])
        .member(MODERATOR, &[MOD_ROLE])
        .member(MEMBER, &[])
        .member(ADMIN, &[ADMIN_ROLE])
        .channel(&alert_of(g), 0, "raid-alerts", &[])
}

fn full_bot() -> Permissions {
    bot_send() | Permissions::MANAGE_ROLES | Permissions::MANAGE_GUILD | Permissions::KICK_MEMBERS | Permissions::BAN_MEMBERS | Permissions::VIEW_CHANNEL | Permissions::EMBED_LINKS
}

/// Discord's answers to the role edit (echoes the new permissions) and the incident actions (echoes the body).
fn script(rest: &FakeRest, g: &str) {
    let gg = g.to_string();
    rest.on_with("PATCH", "/guilds/*/roles/*", move |call| {
        let perms = call.body["permissions"].as_str().unwrap_or("0").to_string();
        (200, json!({ "id": gg, "name": "@everyone", "color": 0, "hoist": false, "position": 0, "permissions": perms, "managed": false, "mentionable": false, "flags": 0,
            "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null } }))
    });
    rest.on_with("PUT", "/guilds/*/incident-actions", |call| {
        (200, json!({ "invites_disabled_until": call.body.get("invites_disabled_until").cloned().unwrap_or(Value::Null), "dms_disabled_until": call.body.get("dms_disabled_until").cloned().unwrap_or(Value::Null) }))
    });
}

impl Raid {
    async fn new(g: &'static str, guild: FakeGuild, over: Value) -> Option<Self> {
        let url = database_url()?;
        let t = DbTestApp::new(&url).await.with_guilds([g]);
        t.reset_guilds(&[g]).await;
        let rest = FakeRest::new();
        script(&rest, g);
        t.set_discord(rest.discord(&[guild.json()]));
        let mut patch = SettingsPatch::new()
            .set("antiRaidEnabled", true)
            .set("antiRaidJoinThreshold", 3)
            .set("antiRaidJoinSeconds", 10)
            .set("antiRaidAction", "KICK")
            .set("antiRaidPauseInvites", true)
            .set("antiRaidLockChat", true)
            .set("antiRaidAlertChannelId", alert_of(g));
        if let Some(o) = over.as_object() {
            for (k, v) in o {
                patch.insert(k, v.clone());
            }
        }
        t.settings().update_settings(g, patch).await.expect("settings");
        let mods: Arc<Mutex<Vec<ModActionInput>>> = Arc::default();
        let result: Arc<Mutex<Option<ModCode>>> = Arc::default();
        let (rec, res) = (mods.clone(), result.clone());
        feat_automod::automod_service::set_mod_executor(
            &t,
            Arc::new(move |_app, input| {
                rec.lock().unwrap_or_else(|e| e.into_inner()).push(input);
                let code = *res.lock().unwrap_or_else(|e| e.into_inner());
                Box::pin(async move {
                    let mut e = ModError::new(code.unwrap_or(ModCode::DiscordError), "failed");
                    e.applied = code.is_none(); // no code: the member was removed
                    Err(e)
                })
            }),
        );
        Some(Self { t, rest, g, gid: GuildId::new(g.parse().unwrap_or(1)), mods, result })
    }

    async fn settings(&self) -> Settings {
        self.t.settings().get_settings_with(self.g, true).await.expect("settings")
    }

    async fn row(&self) -> Option<bot_db::AntiRaidState> {
        svc::load_state(self.t.db(), self.g).await.expect("row")
    }

    fn everyone_bits(&self) -> u64 {
        svc::everyone_bits(&self.t, self.gid).unwrap_or_default()
    }

    fn role_edits(&self) -> Vec<Value> {
        self.rest.calls_to("PATCH", "/guilds/*/roles/*").into_iter().map(|c| c.body).collect()
    }

    fn incident_calls(&self) -> Vec<Value> {
        self.rest.calls_to("PUT", "/guilds/*/incident-actions").into_iter().map(|c| c.body).collect()
    }

    fn alerts(&self) -> Vec<Value> {
        self.rest.calls_to("POST", &format!("/channels/{}/messages", alert_of(self.g))).into_iter().map(|c| c.body).collect()
    }

    async fn start(&self, by: Option<&str>, minutes: Option<i64>) -> Started {
        let s = self.settings().await;
        svc::start_lockdown(&self.t, self.gid, StartInput { guild_id: Some(self.gid), settings: Some(s), started_by_id: by.map(str::to_string), reason: Some("spam wave".into()), minutes, trigger: None }).await
    }

    async fn expire(&self) {
        sqlx::query(r#"UPDATE "AntiRaidState" SET "lockedUntil" = (now() AT TIME ZONE 'UTC') - interval '1 second' WHERE "guildId" = $1"#)
            .bind(self.g)
            .execute(self.t.db())
            .await
            .expect("expire");
        svc::reset_memory(&self.t);
    }

    async fn done(self) {
        self.t.reset_guilds(&[self.g]).await;
    }
}

fn member(id: &str, joined: &str) -> Member {
    serde_json::from_value(json!({
        "user": { "id": id, "username": format!("user{id}"), "discriminator": "0", "avatar": null },
        "roles": [], "joined_at": joined, "deaf": false, "mute": false, "flags": 0, "guild_id": "1"
    }))
    .expect("member")
}

/// A user id whose snowflake says the account is `age_days` old.
fn user_aged(age_days: f64, n: u64) -> String {
    let created = chrono::Utc::now().timestamp_millis() as f64 - age_days * 86_400_000.0;
    let ms = (created as u64).saturating_sub(1_420_070_400_000);
    ((ms << 22) | n).to_string()
}

fn now_iso() -> String {
    bot_util::js::to_iso_string(chrono::Utc::now().timestamp_millis() as f64).unwrap_or_default()
}

#[tokio::test]
async fn start_saves_the_bits_before_editing_everyone_and_end_restores_exactly_them() {
    let Some(r) = Raid::new("956006000000001001", fake_guild("956006000000001001", full_bot(), everyone()), json!({})).await else { return eprintln!("skipped: TEST_DATABASE_URL not set") };
    let taken = (Permissions::SEND_MESSAGES | Permissions::SEND_MESSAGES_IN_THREADS | Permissions::CREATE_PUBLIC_THREADS).bits();
    let res = r.start(Some("123456789012345678"), None).await;
    let Started::Ok { started: true, effects: Some(effects), state, minutes: Some(30) } = res else { panic!("{res:?}") };
    assert_eq!((effects.chat, effects.invites, effects.errors.len()), ("locked", "paused", 0), "{:?}", effects.errors);
    let row = r.row().await.expect("row");
    assert_eq!(row.prior_everyone_send, Some(taken.to_string()));
    assert_eq!((row.locked_by_id.as_deref(), row.reason.as_deref()), (Some("123456789012345678"), Some("spam wave")));
    let (at, until) = (row.locked_at.expect("at"), row.locked_until.expect("until"));
    assert_eq!(until.timestamp_millis() - at.timestamp_millis(), 30 * 60_000);
    assert_eq!(r.role_edits(), vec![json!({ "permissions": (everyone().bits() & !taken).to_string() })]);
    assert_eq!(r.rest.calls_to("PATCH", "/guilds/*/roles/*")[0].reason.as_deref(), Some("Anti-Raid: raid lockdown"));
    assert_eq!(r.everyone_bits(), (Permissions::VIEW_CHANNEL | Permissions::ADD_REACTIONS | Permissions::READ_MESSAGE_HISTORY).bits());
    let inc = r.incident_calls();
    assert_eq!(inc.len(), 1);
    let paused = row.invites_paused_until.expect("paused").timestamp_millis();
    assert_eq!(inc[0]["invites_disabled_until"], json!(bot_util::js::to_iso_string(paused as f64)));
    assert_eq!(paused, until.timestamp_millis());
    assert_eq!(state.invites_paused_until.map(|d| d.timestamp_millis()), Some(paused));
    let job = r.t.scheduler().get_job(&format!("antiraid.unlock:{}", r.g)).await.expect("job");
    assert_eq!((job.r#type.as_str(), job.payload.clone(), job.run_at.timestamp_millis()), ("antiraid.unlock", json!({}), until.timestamp_millis()));
    assert_eq!(svc::peek_locked(&r.t, r.g, chrono::Utc::now().timestamp_millis()), Some(true));

    // A second start while it is on changes nothing.
    assert!(matches!(r.start(None, None).await, Started::Ok { started: false, .. }));
    assert_eq!(r.role_edits().len(), 1);

    let end = svc::end_lockdown(&r.t, r.gid, false).await.expect("end");
    assert!(end.was_active && end.restored);
    assert_eq!((end.effects.chat, end.effects.invites), ("restored", "resumed"));
    assert_eq!(r.everyone_bits(), everyone().bits());
    assert_eq!(r.incident_calls().last().map(|b| b["invites_disabled_until"].clone()), Some(Value::Null));
    let row = r.row().await.expect("row");
    assert!(row.locked_at.is_none() && row.locked_until.is_none() && row.prior_everyone_send.is_none() && row.invites_paused_until.is_none());
    assert!(r.t.scheduler().get_job(&format!("antiraid.unlock:{}", r.g)).await.is_none());
    assert_eq!(svc::peek_locked(&r.t, r.g, chrono::Utc::now().timestamp_millis()), Some(false));
    let again = svc::end_lockdown(&r.t, r.gid, false).await.expect("end");
    assert!(!again.was_active);
    assert_eq!(r.role_edits().len(), 2, "a second end edits nothing");
    r.done().await;
}

#[tokio::test]
async fn a_burst_of_starts_makes_one_lockdown_and_a_failed_restore_is_retried() {
    let Some(r) = Raid::new("956006000000001002", fake_guild("956006000000001002", full_bot(), everyone()), json!({ "antiRaidPauseInvites": false })).await else { return };
    let all = futures_join(&r).await;
    assert_eq!(all.iter().filter(|s| matches!(s, Started::Ok { started: true, .. })).count(), 1);
    assert_eq!(r.role_edits().len(), 1, "@everyone is edited once");
    let taken = (Permissions::SEND_MESSAGES | Permissions::SEND_MESSAGES_IN_THREADS | Permissions::CREATE_PUBLIC_THREADS).bits();
    assert_eq!(r.row().await.and_then(|x| x.prior_everyone_send), Some(taken.to_string()));

    r.rest.fail("PATCH", "/guilds/*/roles/*", 403, 50013, "Missing Permissions");
    let res = svc::end_lockdown(&r.t, r.gid, false).await.expect("end");
    assert!(res.was_active && res.pending_restore && res.permanent);
    assert_eq!(res.effects.chat, "failed");
    assert!(res.effects.errors[0].contains("Send Messages"));
    let row = r.row().await.expect("row");
    assert!(row.locked_at.is_none());
    assert_eq!(row.prior_everyone_send, Some(taken.to_string()), "the bits are kept");
    assert!(r.t.scheduler().get_job(&format!("antiraid.unlock:{}", r.g)).await.is_some(), "the job stays");
    script(&r.rest, r.g);
    let retry = svc::end_lockdown(&r.t, r.gid, false).await.expect("end");
    assert!(!retry.was_active && retry.restored);
    assert_eq!(r.everyone_bits(), everyone().bits());
    assert!(r.row().await.and_then(|x| x.prior_everyone_send).is_none());
    r.done().await;
}

async fn futures_join(r: &Raid) -> Vec<Started> {
    let s = r.settings().await;
    let mut handles = Vec::new();
    for _ in 0..5 {
        let (t, g, s) = (r.t.app.clone(), r.gid, s.clone());
        handles.push(tokio::spawn(async move { svc::start_lockdown(&t, g, StartInput { settings: Some(s), ..Default::default() }).await }));
    }
    let mut out = Vec::new();
    for h in handles {
        out.push(h.await.expect("join"));
    }
    out
}

#[tokio::test]
async fn missing_permissions_cost_an_effect_not_the_lockdown_and_blocked_bits_are_named() {
    let Some(r) = Raid::new("956006000000001003", fake_guild("956006000000001003", Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES | Permissions::EMBED_LINKS, everyone()), json!({})).await else { return };
    let Started::Ok { started: true, effects: Some(e), .. } = r.start(None, None).await else { panic!("not started") };
    assert_eq!((e.chat, e.invites), ("failed", "failed"));
    let text = e.errors.join("\n");
    assert!(text.contains("Manage Roles") && text.contains("Manage Server"));
    assert!(r.row().await.and_then(|x| x.prior_everyone_send).is_none());
    assert!(r.role_edits().is_empty());
    r.done().await;

    // The bot holds only some send bits: the rest stay with @everyone and are named.
    let with_private = everyone() | Permissions::CREATE_PRIVATE_THREADS;
    let partial_bot = Permissions::SEND_MESSAGES | Permissions::SEND_MESSAGES_IN_THREADS | Permissions::CREATE_PUBLIC_THREADS | Permissions::MANAGE_ROLES | Permissions::VIEW_CHANNEL | Permissions::EMBED_LINKS;
    let Some(r) = Raid::new("956006000000001004", fake_guild("956006000000001004", partial_bot, with_private), json!({ "antiRaidPauseInvites": false })).await else { return };
    let Started::Ok { effects: Some(e), .. } = r.start(None, None).await else { panic!("not started") };
    assert_eq!(e.chat, "partial");
    assert!(e.errors.join("\n").contains("Create Private Threads"));
    let taken = (Permissions::SEND_MESSAGES | Permissions::SEND_MESSAGES_IN_THREADS | Permissions::CREATE_PUBLIC_THREADS).bits();
    assert_eq!(r.row().await.and_then(|x| x.prior_everyone_send), Some(taken.to_string()));
    assert_eq!(r.everyone_bits(), (Permissions::VIEW_CHANNEL | Permissions::ADD_REACTIONS | Permissions::READ_MESSAGE_HISTORY | Permissions::CREATE_PRIVATE_THREADS).bits());
    let end = svc::end_lockdown(&r.t, r.gid, false).await.expect("end");
    assert!(end.restored && !end.pending_restore && end.effects.errors.is_empty());
    assert_eq!(r.everyone_bits(), with_private.bits());
    r.done().await;
}

#[tokio::test]
async fn a_job_that_cannot_be_scheduled_undoes_the_lockdown() {
    let Some(url) = database_url() else { return };
    let g = "956006000000001005";
    let (t, failing) = DbTestApp::with_failing_jobs(&url).await;
    let t = t.with_guilds([g]);
    t.reset_guilds(&[g]).await;
    let rest = FakeRest::new();
    script(&rest, g);
    t.set_discord(rest.discord(&[fake_guild(g, full_bot(), everyone()).json()]));
    t.settings().update_settings(g, SettingsPatch::new().set("antiRaidLockChat", true)).await.expect("settings");
    failing.set_failing(true);
    let gid = GuildId::new(g.parse().unwrap_or(1));
    let res = svc::start_lockdown(&t, gid, StartInput::default()).await;
    assert!(matches!(res, Started::Failed { .. }));
    assert!(svc::load_state(t.db(), g).await.ok().flatten().is_some_and(|r| r.locked_at.is_none()));
    assert!(rest.calls_to("PATCH", "/guilds/*/roles/*").is_empty(), "nothing was changed on Discord");
    assert_eq!(svc::peek_locked(&t, g, chrono::Utc::now().timestamp_millis()), None);
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn a_join_flood_starts_one_lockdown_and_the_joiners_are_kicked() {
    let Some(r) = Raid::new("956006000000001006", fake_guild("956006000000001006", full_bot(), everyone()), json!({ "antiRaidPauseInvites": false })).await else { return };
    let s = r.settings().await;
    let old = |n: u64| member(&format!("17592884729911{n:04}"), &now_iso());
    for n in 0..2 {
        let out = plug::handle_member_add(&r.t, None, r.gid, &old(n), Some(s.clone())).await.expect("out");
        assert!(out.action.is_none(), "below the threshold nothing happens");
    }
    let state = bot_core::hooks::HookState::default();
    let trip = plug::handle_member_add(&r.t, Some(&state), r.gid, &old(2), Some(s.clone())).await.expect("out");
    assert!(trip.raid);
    assert!(matches!(trip.start.expect("start").await.expect("join"), Started::Ok { started: true, .. }));
    assert_eq!(trip.action.as_deref(), Some("KICK"));
    assert_eq!(state.get::<Value>(plug::STATE_KEY), Some(json!({ "action": "KICK" })));
    let alerts = r.alerts();
    assert_eq!(alerts.len(), 1);
    assert!(alerts[0]["embeds"][0]["description"].as_str().unwrap_or_default().contains("**3** members joined within **10** seconds"));
    assert_eq!(alerts[0]["components"][0]["components"][0]["custom_id"], "antiraid:end");
    assert_eq!(alerts[0]["allowed_mentions"]["parse"], json!([]));

    let later = plug::handle_member_add(&r.t, None, r.gid, &old(3), Some(s.clone())).await.expect("out");
    assert!(!later.raid);
    assert_eq!(later.action.as_deref(), Some("KICK"));
    assert_eq!(r.alerts().len(), 1, "one alert per lockdown");
    let mods = r.mods.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(mods.len(), 2);
    let c = &mods[1];
    assert_eq!((c.action.as_str(), c.source.as_str(), c.silent, c.skip_escalation, c.delete_message_seconds), ("KICK", "antiraid", true, true, None));
    assert!(c.reason.as_deref().unwrap_or_default().starts_with("Anti-Raid: raid lockdown"));
    assert!(c.log_fields[0].1.to_lowercase().contains("raid lockdown"));
    assert_eq!(svc::tally_of(&r.t, r.g).kicked, 2);

    // Failures are counted, staff and absent members are not.
    *r.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(ModCode::Immune);
    plug::handle_member_add(&r.t, None, r.gid, &old(4), Some(s.clone())).await;
    *r.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(ModCode::Hierarchy);
    plug::handle_member_add(&r.t, None, r.gid, &old(5), Some(s.clone())).await;
    assert_eq!(svc::tally_of(&r.t, r.g).failed, 1);

    // The End button: a member is refused, a moderator ends it.
    r.t.install(registry());
    let click = |user: &str, roles: &[&str]| FakeInteraction::button("antiraid:end").guild(r.g).user(user).roles(roles).build();
    bot_core::router::route_interaction(&r.t, click(MEMBER, &[])).await;
    assert!(r.t.responses.calls().last().is_some_and(|c| c.embed_description().unwrap_or_default().contains("Only moderators") && c.ephemeral()));
    assert!(r.row().await.is_some_and(|x| x.locked_at.is_some()));
    bot_core::router::route_interaction(&r.t, click(MODERATOR, &[MOD_ROLE])).await;
    let names = r.t.responses.names();
    assert!(names.ends_with(&["deferUpdate", "editReply", "followUp"]), "{names:?}");
    let calls = r.t.responses.calls();
    assert_eq!(calls[calls.len() - 2].body["components"], json!([]));
    assert!(calls[calls.len() - 1].embed_title().unwrap_or_default().contains("Lockdown Ended") && calls[calls.len() - 1].ephemeral());
    assert!(r.row().await.is_some_and(|x| x.locked_at.is_none()));
    assert_eq!(r.everyone_bits(), everyone().bits());
    let note = r.alerts().last().cloned().unwrap_or_default();
    assert!(note["embeds"][0]["description"].as_str().unwrap_or_default().contains(&format!("Ended by <@{MODERATOR}>")));
    assert!(note["embeds"][0]["description"].as_str().unwrap_or_default().contains("2 kicked, 1 could not be removed"));
    bot_core::router::route_interaction(&r.t, click(MODERATOR, &[MOD_ROLE])).await;
    assert!(r.t.responses.calls().last().is_some_and(|c| c.embed_title().unwrap_or_default().contains("No Lockdown")));
    bot_core::router::route_interaction(&r.t, FakeInteraction::button("antiraid:other").guild(r.g).user(MODERATOR).roles(&[MOD_ROLE]).build()).await;
    assert!(r.t.responses.calls().last().is_some_and(|c| c.content() == Some("This button is no longer active.")));
    r.done().await;
}

#[tokio::test]
async fn young_accounts_get_their_own_action_and_the_alert_is_throttled() {
    let Some(r) = Raid::new(
        "956006000000001007",
        fake_guild("956006000000001007", full_bot(), everyone()),
        json!({ "antiRaidMinAccountAgeDays": 7, "antiRaidYoungAction": "BAN", "antiRaidJoinThreshold": 50 }),
    )
    .await
    else {
        return;
    };
    let s = r.settings().await;
    let out = plug::handle_member_add(&r.t, None, r.gid, &member(&user_aged(2.5, 1), &now_iso()), Some(s.clone())).await.expect("out");
    assert_eq!(out.action.as_deref(), Some("BAN"));
    let mods = r.mods.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(mods[0].action, "BAN");
    assert_eq!(mods[0].reason.as_deref(), Some("Anti-Raid: account is 2 days old (minimum 7)"));
    assert_eq!(mods[0].delete_message_seconds, Some(3600));
    assert_eq!(r.alerts().len(), 1);
    assert!(r.alerts()[0]["embeds"][0]["fields"].to_string().contains("Banned"));
    assert!(r.row().await.is_none(), "no lockdown for a single young account");
    plug::handle_member_add(&r.t, None, r.gid, &member(&user_aged(1.0, 2), &now_iso()), Some(s.clone())).await;
    plug::handle_member_add(&r.t, None, r.gid, &member(&user_aged(1.0, 3), &now_iso()), Some(s.clone())).await;
    assert_eq!(r.mods.lock().unwrap_or_else(|e| e.into_inner()).len(), 3);
    assert_eq!(r.alerts().len(), 1, "one alert per minute");
    assert!(plug::handle_member_add(&r.t, None, r.gid, &member(&user_aged(400.0, 4), &now_iso()), Some(s.clone())).await.expect("out").action.is_none());

    // Both a raider and too young: the harsher action, once, and the reason names what decided it.
    svc::start_lockdown(&r.t, r.gid, StartInput { settings: Some(s.clone()), ..Default::default() }).await;
    plug::handle_member_add(&r.t, None, r.gid, &member(&user_aged(1.0, 5), &now_iso()), Some(s.clone())).await;
    let last = r.mods.lock().unwrap_or_else(|e| e.into_inner()).last().cloned().expect("call");
    assert_eq!(last.action, "BAN");
    assert!(!last.reason.unwrap_or_default().contains("raid lockdown"));
    r.done().await;
}

#[tokio::test]
async fn the_unlock_job_ends_a_due_lockdown_and_fails_permanently_without_permission() {
    let Some(r) = Raid::new("956006000000001008", fake_guild("956006000000001008", full_bot(), everyone()), json!({ "antiRaidPauseInvites": false, "antiRaidEnabled": false })).await else { return };
    svc::start_lockdown(&r.t, r.gid, StartInput::default()).await;
    assert_eq!(plug::handle_unlock_job(&r.t, r.gid).await.ok(), Some(JobOutcome::Done));
    assert!(r.row().await.is_some_and(|x| x.locked_at.is_some()), "not due yet: left alone");
    r.expire().await;
    // A lockdown that ran out is ended by the next join (the self-heal) or by the job: here the job, through the scheduler.
    let job = r.t.scheduler().get_job(&format!("antiraid.unlock:{}", r.g)).await.expect("job");
    r.t.install(registry());
    r.t.scheduler().run_job(&r.t, job).await;
    assert!(r.row().await.is_some_and(|x| x.locked_at.is_none()), "ended although the module is off");
    assert_eq!(r.everyone_bits(), everyone().bits());
    let note = r.alerts().last().cloned().unwrap_or_default();
    assert!(note["embeds"][0]["title"].as_str().unwrap_or_default().contains("ended") && note["embeds"][0]["description"].as_str().unwrap_or_default().contains("ran out"));
    let posted = r.alerts().len();
    assert_eq!(plug::handle_unlock_job(&r.t, r.gid).await.ok(), Some(JobOutcome::Done));
    assert_eq!(r.alerts().len(), posted, "a retry posts nothing");

    svc::start_lockdown(&r.t, r.gid, StartInput::default()).await;
    r.expire().await;
    r.rest.fail("PATCH", "/guilds/*/roles/*", 500, 0, "timeout");
    let err = plug::handle_unlock_job(&r.t, r.gid).await.expect_err("retried");
    assert!(!PermanentJobError::is(&err) && err.to_string().contains("Send Messages"));
    r.rest.fail("PATCH", "/guilds/*/roles/*", 403, 50013, "Missing Permissions");
    let err = plug::handle_unlock_job(&r.t, r.gid).await.expect_err("permanent");
    assert!(PermanentJobError::is(&err));
    r.done().await;
}

#[tokio::test]
async fn a_lockdown_in_the_table_is_found_after_a_restart_and_an_expired_one_is_healed() {
    let Some(r) = Raid::new("956006000000001009", fake_guild("956006000000001009", full_bot(), everyone()), json!({ "antiRaidPauseInvites": false, "antiRaidLockChat": false })).await else { return };
    let s = r.settings().await;
    svc::start_lockdown(&r.t, r.gid, StartInput { settings: Some(s.clone()), ..Default::default() }).await;
    svc::reset_memory(&r.t); // a restart
    let out = plug::handle_member_add(&r.t, None, r.gid, &member("175928847299110001", &now_iso()), Some(s.clone())).await.expect("out");
    assert!(!out.raid);
    assert_eq!(out.action.as_deref(), Some("KICK"));
    r.expire().await;
    let out = plug::handle_member_add(&r.t, None, r.gid, &member("175928847299110002", &now_iso()), Some(s.clone())).await.expect("out");
    assert!(out.action.is_none(), "an expired lockdown does not action anyone");
    for _ in 0..50 {
        if r.row().await.is_some_and(|x| x.locked_at.is_none()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(r.row().await.is_some_and(|x| x.locked_at.is_none()), "the self-heal ended it");
    r.done().await;
}

#[tokio::test]
async fn the_command_shows_changes_starts_and_ends() {
    use bot_core::command::option_type::{BOOLEAN, CHANNEL, INTEGER, STRING};
    let Some(r) = Raid::new("956006000000001010", fake_guild("956006000000001010", Permissions::MANAGE_GUILD | Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES | Permissions::EMBED_LINKS, everyone()), json!({ "antiRaidEnabled": false, "antiRaidAction": "ALERT", "antiRaidLockChat": false })).await else { return };
    r.t.install(registry());
    let run = |group: Option<&str>, sub: &str, opts: Vec<(&str, u8, Value)>| {
        let mut f = FakeInteraction::slash("antiraid").guild(r.g).user(ADMIN).roles(&[ADMIN_ROLE]).channel("956006000000001050").permissions(Permissions::MANAGE_GUILD);
        f = match group {
            Some(g) => f.group(g, sub),
            None => f.subcommand(sub),
        };
        for (n, k, v) in opts {
            f = f.option(n, k, v);
        }
        f.build()
    };
    bot_core::router::route_interaction(&r.t, run(None, "config", vec![])).await;
    assert!(r.t.responses.calls().last().and_then(|c| c.embed_description().map(str::to_string)).unwrap_or_default().contains("Detection:** 3 joins within 10 seconds"));
    bot_core::router::route_interaction(
        &r.t,
        run(None, "config", vec![
            ("joins", INTEGER, json!(8)), ("seconds", INTEGER, json!(20)), ("action", STRING, json!("BAN")), ("lock_chat", BOOLEAN, json!(true)),
            ("min_account_age", INTEGER, json!(3)), ("young_action", STRING, json!("KICK")), ("enabled", BOOLEAN, json!(true)),
        ]),
    )
    .await;
    let s = r.settings().await;
    assert_eq!(
        (s.anti_raid_join_threshold, s.anti_raid_join_seconds, s.anti_raid_action.as_str(), s.anti_raid_lock_chat, s.anti_raid_min_account_age_days, s.anti_raid_young_action.as_str(), s.anti_raid_enabled),
        (8, 20, "BAN", true, 3, "KICK", true)
    );
    let last = r.t.responses.calls().last().cloned().expect("answer");
    assert!(last.embed_title().unwrap_or_default().contains("Anti-Raid Updated"));
    let d = last.embed_description().unwrap_or_default().to_string();
    assert!(d.contains("Locking chat: I need **Manage Roles**") && d.contains("Banning members: I need **Ban Members**") && !d.contains("module is off"), "{d}");
    bot_core::router::route_interaction(&r.t, run(None, "config", vec![("enabled", BOOLEAN, json!(false))])).await;
    assert!(r.t.responses.calls().last().and_then(|c| c.embed_description().map(str::to_string)).unwrap_or_default().contains("module is off"));
    bot_core::router::route_interaction(&r.t, run(None, "config", vec![("alert_channel", CHANNEL, json!("956006000000001077"))])).await;
    assert!(r.t.responses.calls().last().and_then(|c| c.embed_title().map(str::to_string)).unwrap_or_default().contains("Cannot Use That Channel"));
    // Audit: a picked channel that is not cached (discord.js had `_add`ed it from the resolved data) is fetched and used.
    r.rest.on("GET", "/channels/956006000000001078", 200, json!({ "id": "956006000000001078", "type": 0, "guild_id": r.g, "name": "late-alerts", "position": 7, "permission_overwrites": [] }));
    bot_core::router::route_interaction(&r.t, run(None, "config", vec![("alert_channel", CHANNEL, json!("956006000000001078"))])).await;
    assert!(r.t.responses.calls().last().and_then(|c| c.embed_title().map(str::to_string)).unwrap_or_default().contains("Anti-Raid Updated"));
    assert_eq!(r.settings().await.anti_raid_alert_channel_id.as_deref(), Some("956006000000001078"));
    bot_core::router::route_interaction(&r.t, run(None, "config", vec![("alert_channel", CHANNEL, json!(alert_of(r.g)))])).await;
    assert_eq!(r.settings().await.anti_raid_alert_channel_id, Some(alert_of(r.g)));

    bot_core::router::route_interaction(&r.t, run(Some("lockdown"), "start", vec![("minutes", INTEGER, json!(15)), ("reason", STRING, json!("spam wave"))])).await;
    let last = r.t.responses.calls().last().cloned().expect("answer");
    assert!(last.embed_title().unwrap_or_default().contains("Lockdown started"));
    assert!(last.embed_description().unwrap_or_default().contains(&format!("Started by <@{ADMIN}>: spam wave")));
    assert!(last.content().unwrap_or_default().contains("module is off"));
    let row = r.row().await.expect("row");
    assert_eq!(row.locked_by_id.as_deref(), Some(ADMIN));
    assert_eq!(row.locked_until.map(|u| u.timestamp_millis()).unwrap_or(0) - row.locked_at.map(|a| a.timestamp_millis()).unwrap_or(0), 15 * 60_000);
    assert_eq!(r.alerts().len(), 1);
    bot_core::router::route_interaction(&r.t, run(Some("lockdown"), "start", vec![])).await;
    assert!(r.t.responses.calls().last().and_then(|c| c.embed_title().map(str::to_string)).unwrap_or_default().contains("Already Locked Down"));
    bot_core::router::route_interaction(&r.t, run(None, "status", vec![])).await;
    let body = r.t.responses.calls().last().map(|c| c.body.to_string()).unwrap_or_default();
    assert!(body.contains("🔒 On until") && body.contains("❌ Off"));
    bot_core::router::route_interaction(&r.t, run(Some("lockdown"), "end", vec![])).await;
    assert!(r.t.responses.calls().last().and_then(|c| c.embed_title().map(str::to_string)).unwrap_or_default().contains("ended"));
    assert!(r.row().await.is_some_and(|x| x.locked_at.is_none()));
    bot_core::router::route_interaction(&r.t, run(Some("lockdown"), "end", vec![])).await;
    assert!(r.t.responses.calls().last().and_then(|c| c.embed_title().map(str::to_string)).unwrap_or_default().contains("No Lockdown"));
    bot_core::router::route_interaction(&r.t, run(None, "status", vec![])).await;
    assert!(r.t.responses.calls().last().map(|c| c.body.to_string()).unwrap_or_default().contains("🔓 Off"));

    // Deleting the alert channel clears it from the settings.
    plug::on_channel_delete(&r.t, r.gid, "OTHER", Some(r.settings().await)).await.expect("delete");
    assert_eq!(r.settings().await.anti_raid_alert_channel_id, Some(alert_of(r.g)));
    plug::on_channel_delete(&r.t, r.gid, &alert_of(r.g), Some(r.settings().await)).await.expect("delete");
    assert_eq!(r.settings().await.anti_raid_alert_channel_id, None);
    r.done().await;
}

#[tokio::test]
async fn js_started_lockdowns_end_in_rust_and_rust_started_ones_read_in_js() {
    let Some(r) = Raid::new("956006000000001011", fake_guild("956006000000001011", full_bot(), everyone()), json!({})).await else { return };
    // JS writes (a guild it cannot edit: the effects fail, the row and the job stand).
    let js = run_node("rust/fixtures/gen/feat-automod-js.mjs", &["raidstart", r.g, "45"]);
    assert!(js.get("error").is_none(), "{js}");
    assert_eq!(js["res"]["started"], true);
    let row = r.row().await.expect("row");
    assert!(svc::is_active(Some(&row), chrono::Utc::now().timestamp_millis()));
    assert_eq!((row.locked_by_id.as_deref(), row.reason.as_deref(), row.prior_everyone_send.as_deref()), (Some("800000000000000009"), Some("spam wave"), None));
    assert_eq!(row.locked_until.map(|u| u.timestamp_millis()).unwrap_or(0) - row.locked_at.map(|a| a.timestamp_millis()).unwrap_or(0), 45 * 60_000);
    let job = r.t.scheduler().get_job(&format!("antiraid.unlock:{}", r.g)).await.expect("job");
    assert_eq!((job.r#type.as_str(), job.payload.clone()), ("antiraid.unlock", json!({})));
    assert_eq!(job.run_at, row.locked_until.expect("until"));
    let ours = svc::unlock_job(r.g, row.locked_until.expect("until"));
    assert_eq!((ours.dedupe_key.as_deref(), ours.payload.clone()), (job.dedupe_key.as_deref(), job.payload.clone()));
    // Rust ends it; the JS reads it back cleared.
    let end = svc::end_lockdown(&r.t, r.gid, false).await.expect("end");
    assert!(end.was_active);
    let back = run_node("rust/fixtures/gen/feat-automod-js.mjs", &["raidread", r.g]);
    assert_eq!(back["active"], false);
    assert_eq!(back["row"]["lockedAt"], Value::Null);
    assert_eq!(back["job"], Value::Null);

    // Rust starts; the JS sees the lockdown and its job.
    let Started::Ok { started: true, .. } = r.start(Some(ADMIN), Some(20)).await else { panic!("not started") };
    let back = run_node("rust/fixtures/gen/feat-automod-js.mjs", &["raidread", r.g]);
    let row = r.row().await.expect("row");
    assert_eq!(back["active"], true);
    assert_eq!(back["row"]["lockedById"], ADMIN);
    assert_eq!(back["row"]["reason"], "spam wave");
    assert_eq!(back["row"]["priorEveryoneSend"], json!(row.prior_everyone_send));
    assert_eq!(back["row"]["lockedUntil"], json!(bot_util::js::to_iso_string(row.locked_until.expect("until").timestamp_millis() as f64)));
    assert_eq!(back["job"]["type"], "antiraid.unlock");
    assert_eq!(back["job"]["dedupeKey"], format!("antiraid.unlock:{}", r.g));
    assert_eq!(back["job"]["payload"], json!({}));
    assert_eq!(back["job"]["runAt"], back["row"]["lockedUntil"]);
    r.done().await;
}

// ── Review cases: invite pauses, partial restores, pending restores, fallbacks ─────────────────

fn private_threads() -> u64 {
    Permissions::CREATE_PRIVATE_THREADS.bits()
}

fn no_private() -> Permissions {
    Permissions::SEND_MESSAGES | Permissions::SEND_MESSAGES_IN_THREADS | Permissions::CREATE_PUBLIC_THREADS
}

async fn seed_prior(r: &Raid, prior: u64) {
    r.t.settings().ensure_guild_settings(r.g).await.expect("ensure");
    bot_db::InsertBuilder::new(&bot_db::ANTI_RAID_STATE_INFO)
        .value("guildId", json!(r.g))
        .and_then(|b| b.value("priorEveryoneSend", json!(prior.to_string())))
        .and_then(|b| b.build())
        .expect("insert")
        .execute(r.t.db())
        .await
        .expect("seed");
}

fn bot_role_with(r: &Raid, perms: Permissions) {
    let role: serenity::all::Role = serde_json::from_value(json!({
        "id": BOT_ROLE, "name": "bot", "color": 0, "hoist": false, "position": 5, "permissions": perms.bits().to_string(),
        "managed": false, "mentionable": false, "flags": 0, "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null }
    }))
    .expect("role");
    assert!(bot_core::discord::cache_created_role(&r.t, r.gid, &role));
}

#[tokio::test]
async fn the_invite_pause_is_capped_saved_as_set_and_only_ours_is_resumed() {
    let Some(r) = Raid::new("956006000000001020", fake_guild("956006000000001020", full_bot(), everyone()), json!({ "antiRaidLockChat": false })).await else { return };
    let Started::Ok { effects: Some(e), minutes: Some(1440), state, .. } = r.start(None, Some(1440)).await else { panic!("not started") };
    assert_eq!(e.invites, "paused");
    let row = r.row().await.expect("row");
    let set = row.invites_paused_until.expect("paused").timestamp_millis();
    assert!(set - chrono::Utc::now().timestamp_millis() < 24 * 3_600_000, "under 24 h from now");
    assert_eq!(r.incident_calls()[0]["invites_disabled_until"], json!(bot_util::js::to_iso_string(set as f64)));
    assert_eq!(state.invites_paused_until.map(|d| d.timestamp_millis()), Some(set));
    assert!(row.locked_until.expect("until").timestamp_millis() - set > raid::INVITE_MATCH_MS);
    let end = svc::end_lockdown(&r.t, r.gid, false).await.expect("end");
    assert_eq!(end.effects.invites, "resumed");
    assert_eq!(r.incident_calls().last().map(|b| b["invites_disabled_until"].clone()), Some(Value::Null));
    assert!(r.row().await.and_then(|x| x.invites_paused_until).is_none());
    r.done().await;

    // A longer pause was already on: the bot sets none, stores none, and never resumes it.
    let longer = bot_util::js::to_iso_string((chrono::Utc::now().timestamp_millis() + 40 * 60_000) as f64).unwrap_or_default();
    let g = fake_guild("956006000000001021", full_bot(), everyone()).field("incidents_data", json!({ "invites_disabled_until": longer, "dms_disabled_until": null }));
    let Some(r) = Raid::new("956006000000001021", g, json!({ "antiRaidLockChat": false })).await else { return };
    let Started::Ok { effects: Some(e), .. } = r.start(None, None).await else { panic!("not started") };
    assert_eq!(e.invites, "already");
    assert!(r.incident_calls().is_empty(), "a longer pause is left alone");
    assert!(r.row().await.and_then(|x| x.invites_paused_until).is_none());
    let end = svc::end_lockdown(&r.t, r.gid, false).await.expect("end");
    assert_eq!(end.effects.invites, "foreign");
    assert!(r.incident_calls().is_empty(), "nothing was resumed");
    r.done().await;

    // Invites not paused by the setting: ending touches nothing.
    let Some(r) = Raid::new("956006000000001022", fake_guild("956006000000001022", full_bot(), everyone()), json!({ "antiRaidLockChat": false, "antiRaidPauseInvites": false })).await else { return };
    let Started::Ok { effects: Some(e), .. } = r.start(None, None).await else { panic!("not started") };
    assert_eq!((e.chat, e.invites), ("skipped", "skipped"));
    assert_eq!(svc::end_lockdown(&r.t, r.gid, false).await.expect("end").effects.invites, "none");
    assert!(r.incident_calls().is_empty() && r.role_edits().is_empty());
    r.done().await;
}

#[tokio::test]
async fn a_saved_bit_the_bot_cannot_give_back_does_not_stop_the_others() {
    let bot = no_private() | Permissions::MANAGE_ROLES | Permissions::VIEW_CHANNEL | Permissions::EMBED_LINKS;
    let Some(r) = Raid::new("956006000000001023", fake_guild("956006000000001023", bot, Permissions::VIEW_CHANNEL), json!({})).await else { return };
    seed_prior(&r, SEND_MASK).await;
    let res = svc::end_lockdown(&r.t, r.gid, false).await.expect("end");
    assert!(res.pending_restore && res.permanent && !res.restored);
    assert_eq!(res.effects.chat, "partialRestore");
    let errors = res.effects.errors.join("\n");
    assert!(errors.contains("Create Private Threads") && !errors.contains("Send Messages"), "{errors}");
    assert_eq!(r.everyone_bits(), (Permissions::VIEW_CHANNEL | no_private()).bits());
    assert_eq!(r.row().await.and_then(|x| x.prior_everyone_send), Some(private_threads().to_string()), "only the bit still owed stays saved");
    let note = serde_json::to_value(svc::build_end_embed("manual", None, Some(&res))).unwrap_or_default();
    assert!(note["fields"].to_string().to_lowercase().contains("partly"));
    // Once a role of the bot has it, ending again finishes the job.
    bot_role_with(&r, Permissions::from_bits_truncate(SEND_MASK) | Permissions::MANAGE_ROLES | Permissions::VIEW_CHANNEL);
    let retry = svc::end_lockdown(&r.t, r.gid, false).await.expect("end");
    assert!(retry.restored);
    assert_eq!(r.everyone_bits(), Permissions::VIEW_CHANNEL.bits() | SEND_MASK);
    assert!(r.row().await.and_then(|x| x.prior_everyone_send).is_none());
    r.done().await;
}

#[tokio::test]
async fn a_pending_restore_is_carried_into_a_new_lockdown_and_nothing_is_taken_twice() {
    let Some(r) = Raid::new("956006000000001024", fake_guild("956006000000001024", full_bot(), Permissions::VIEW_CHANNEL), json!({ "antiRaidPauseInvites": false })).await else { return };
    seed_prior(&r, SEND_MASK).await;
    let Started::Ok { effects: Some(e), .. } = r.start(None, None).await else { panic!("not started") };
    assert_eq!(e.chat, "nothing", "@everyone cannot send already");
    assert!(r.role_edits().is_empty());
    assert_eq!(r.row().await.and_then(|x| x.prior_everyone_send), Some(SEND_MASK.to_string()), "the first lockdown's bits are not forgotten");
    svc::end_lockdown(&r.t, r.gid, false).await.expect("end");
    assert_eq!(r.everyone_bits(), Permissions::VIEW_CHANNEL.bits() | SEND_MASK);
    r.done().await;

    // Nothing pending and nothing to take: nothing is saved, edited or restored.
    let Some(r) = Raid::new("956006000000001025", fake_guild("956006000000001025", full_bot(), Permissions::VIEW_CHANNEL), json!({ "antiRaidPauseInvites": false })).await else { return };
    let Started::Ok { effects: Some(e), .. } = r.start(None, None).await else { panic!("not started") };
    assert_eq!(e.chat, "nothing");
    assert!(r.row().await.and_then(|x| x.prior_everyone_send).is_none());
    let end = svc::end_lockdown(&r.t, r.gid, false).await.expect("end");
    assert!(end.was_active);
    assert!(r.role_edits().is_empty());
    r.done().await;
}

#[tokio::test]
async fn a_bot_with_administrator_takes_every_send_bit_and_one_holding_none_takes_nothing() {
    let with_private = everyone() | Permissions::CREATE_PRIVATE_THREADS;
    let Some(r) = Raid::new("956006000000001026", fake_guild("956006000000001026", Permissions::ADMINISTRATOR, with_private), json!({ "antiRaidPauseInvites": false })).await else { return };
    let Started::Ok { effects: Some(e), .. } = r.start(None, None).await else { panic!("not started") };
    assert_eq!(e.chat, "locked");
    assert_eq!(r.row().await.and_then(|x| x.prior_everyone_send), Some((no_private().bits() | private_threads()).to_string()));
    svc::end_lockdown(&r.t, r.gid, false).await.expect("end");
    assert_eq!(r.everyone_bits(), with_private.bits());
    r.done().await;

    let bare = Permissions::MANAGE_ROLES | Permissions::VIEW_CHANNEL | Permissions::EMBED_LINKS;
    let Some(r) = Raid::new("956006000000001027", fake_guild("956006000000001027", bare, with_private), json!({ "antiRaidPauseInvites": false })).await else { return };
    let Started::Ok { started: true, effects: Some(e), .. } = r.start(None, None).await else { panic!("not started") };
    assert_eq!(e.chat, "failed");
    assert!(e.errors.join("\n").contains("Send Messages"));
    assert!(r.role_edits().is_empty(), "@everyone is not touched");
    assert!(r.row().await.and_then(|x| x.prior_everyone_send).is_none());
    assert!(!svc::end_lockdown(&r.t, r.gid, false).await.expect("end").pending_restore);
    r.done().await;
}

#[tokio::test]
async fn the_unlock_job_says_out_loud_when_everyone_cannot_be_given_back_even_on_a_retry() {
    // @everyone can only view; the bot's role holds no send bit, but may post in the alert channel by an overwrite.
    let bot = Permissions::MANAGE_ROLES | Permissions::VIEW_CHANNEL | Permissions::EMBED_LINKS;
    let g = "956006000000001028";
    let guild = fake_guild(g, bot, Permissions::VIEW_CHANNEL).edit_channel(&alert_of(g), |c| {
        c["permission_overwrites"] = json!([{ "id": BOT_ROLE, "type": 0, "allow": Permissions::SEND_MESSAGES.bits().to_string(), "deny": "0" }]);
    });
    let Some(r) = Raid::new(g, guild, json!({})).await else { return };
    seed_prior(&r, Permissions::SEND_MESSAGES.bits()).await;
    let err = plug::handle_unlock_job(&r.t, r.gid).await.expect_err("permanent");
    assert!(PermanentJobError::is(&err));
    let note = r.alerts().last().cloned().expect("a note was posted although no lockdown was active");
    let attention = note["embeds"][0]["fields"].as_array().and_then(|f| f.iter().find(|x| x["name"] == "Needs attention").cloned()).unwrap_or_default();
    assert!(attention["value"].as_str().unwrap_or_default().contains("Send Messages"));
    r.done().await;
}

#[tokio::test]
async fn alerts_fall_back_to_the_moderation_log_and_nothing_is_posted_without_a_channel() {
    let Some(r) = Raid::new("956006000000001029", fake_guild("956006000000001029", full_bot(), everyone()), json!({ "antiRaidLockChat": false, "antiRaidPauseInvites": false })).await else { return };
    let state = state_of(None, None);
    let mut s = (*r.settings().await).clone();
    s.anti_raid_alert_channel_id = Some("956006000000001098".into()); // deleted
    s.mod_log_channel_id = Some(alert_of(r.g));
    let posted = svc::announce_start(&r.t, r.gid, Some(&s), &state, None, None).await;
    assert_eq!(posted.map(|(c, _)| c.to_string()), Some(alert_of(r.g)));
    s.anti_raid_alert_channel_id = None;
    s.mod_log_channel_id = None;
    assert!(svc::announce_start(&r.t, r.gid, Some(&s), &state, None, None).await.is_none());
    r.done().await;
}

#[tokio::test]
async fn bots_are_never_counted_alert_only_reports_and_admins_and_mod_roles_end_it() {
    let Some(r) = Raid::new("956006000000001030", fake_guild("956006000000001030", full_bot(), everyone()), json!({ "antiRaidPauseInvites": false, "antiRaidAction": "ALERT" })).await else { return };
    let s = r.settings().await;
    let bot = |n: u64| {
        let mut m = member(&format!("17592884729912{n:04}"), &now_iso());
        m.user.bot = true;
        m
    };
    for n in 0..6 {
        assert!(plug::handle_member_add(&r.t, None, r.gid, &bot(n), Some(s.clone())).await.is_none());
    }
    assert!(r.row().await.is_none(), "bots do not fill the window");
    svc::start_lockdown(&r.t, r.gid, StartInput { settings: Some(s.clone()), ..Default::default() }).await;
    assert!(plug::handle_member_add(&r.t, None, r.gid, &bot(9), Some(s.clone())).await.is_none());
    let out = plug::handle_member_add(&r.t, None, r.gid, &member("175928847299130001", &now_iso()), Some(s.clone())).await.expect("out");
    assert_eq!(out.action.as_deref(), Some("ALERT"));
    assert!(r.mods.lock().unwrap_or_else(|e| e.into_inner()).is_empty(), "nobody is kicked or banned");
    assert_eq!(svc::tally_of(&r.t, r.g).flagged, 1);
    // An admin (Manage Server) ends it with the button.
    r.t.install(registry());
    bot_core::router::route_interaction(&r.t, FakeInteraction::button("antiraid:end").guild(r.g).user(ADMIN).roles(&[ADMIN_ROLE]).build()).await;
    assert!(r.row().await.is_some_and(|x| x.locked_at.is_none()));
    let note = r.alerts().last().cloned().unwrap_or_default();
    assert!(note["embeds"][0]["description"].as_str().unwrap_or_default().contains("1 reported"));
    r.done().await;
}
