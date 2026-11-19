//! The "settingsBridge.writeSettings (database)" cases of test/dashboard.test.js, on the Rust bridge.

mod common;

use std::sync::Arc;

use bot_core::settings::SettingsPatch;
use common::*;
use dash_collections::testing::FakeDiscord;
use dash_collections::{DashboardDiscord, DiscordResources, SettingsBridge, SettingsWriteCtx, SettingsWriteError, SettingsWritten};
use dash_schema::{read_settings_values, to_stored_changes, validate_settings_changes};
use dashboard::settings_bridge::DashboardSettings;
use serde_json::{Map, Value, json};

const GID: &str = "955200000000000001";
const LEGACY: &str = "955200000000000002";
const ROUND: &str = "955200000000000003";
const USER_ID: &str = "111111111111111111";
const CH_TEXT_: &str = "300000000000000001";
const CH_NEWS: &str = "300000000000000004";
const RL_LOW: &str = "400000000000000001";
const RL_MID: &str = "400000000000000003";

fn fake_resources(g: &str) -> DiscordResources {
    serde_json::from_value(json!({
        "guild": { "id": g, "name": "zeta", "icon": null, "memberCount": 12, "ownerId": "555555555555555555" },
        "channels": [
            { "id": CH_TEXT_, "name": "general", "type": 0, "kind": "text", "parentId": null, "position": 0, "nsfw": false },
            { "id": "300000000000000002", "name": "Lounge", "type": 2, "kind": "voice", "parentId": null, "position": 1, "nsfw": false },
            { "id": "300000000000000003", "name": "INFO", "type": 4, "kind": "category", "parentId": null, "position": 0, "nsfw": false },
            { "id": CH_NEWS, "name": "news", "type": 5, "kind": "announcement", "parentId": "300000000000000003", "position": 0, "nsfw": false }
        ],
        "roles": [], "emojis": [],
        "bot": { "userId": "123456789012345678", "topRolePosition": 6, "manageRoles": true, "createInvite": true }
    }))
    .unwrap()
}

struct Env {
    app: bot_core::App,
    bridge: DashboardSettings,
    discord: Arc<FakeDiscord>,
}

impl Env {
    fn valid(changes: Value) -> Map<String, Value> {
        validate_settings_changes(&json!({ "changes": changes })).unwrap_or_else(|e| panic!("{}", e.message)).changes
    }

    async fn write(&self, g: &str, changes: Value) -> Result<SettingsWritten, SettingsWriteError> {
        let d: Arc<dyn DashboardDiscord> = self.discord.clone();
        self.bridge.write_settings(g, Self::valid(changes), SettingsWriteCtx { user_id: USER_ID.into(), discord: Some(d) }).await
    }

    async fn cleanup(&self) {
        for g in [GID, LEGACY, ROUND] {
            sqlx::query(r#"DELETE FROM "GuildSettings" WHERE "guildId" = $1"#).bind(g).execute(self.app.db()).await.unwrap();
            self.app.settings().invalidate_settings(g).await;
        }
    }
}

async fn env() -> Option<(Env, tokio::sync::MutexGuard<'static, ()>)> {
    let (db, redis) = urls()?;
    let lock = SERIAL.lock().await;
    let app = dashboard_app(&db, &redis).await;
    let discord = Arc::new(FakeDiscord::new());
    for g in [GID, LEGACY, ROUND] {
        discord.set_resources(g, fake_resources(g));
    }
    let e = Env { bridge: DashboardSettings::new(app.clone()), app, discord };
    e.cleanup().await;
    Some((e, lock))
}

fn field_err(r: Result<SettingsWritten, SettingsWriteError>) -> String {
    match r {
        Err(SettingsWriteError::Field(f)) => f.path,
        Err(SettingsWriteError::Internal(e)) => panic!("internal: {e}"),
        Ok(_) => panic!("accepted"),
    }
}

#[tokio::test]
async fn automod_rules_and_lists_merge_into_the_stored_config_a_v1_config_is_upgraded_first() {
    let Some((e, _l)) = env().await else { return };
    e.app
        .settings()
        .update_settings(LEGACY, SettingsPatch::new().set("automodConfig", json!({ "filterInvites": true, "automodAction": "KICK", "badWords": ["legacy"] })))
        .await
        .unwrap();
    let w = e.write(LEGACY, json!({ "automodRuleCaps": { "action": "delete", "threshold": 90 }, "automodBadWords": ["legacy", "new one"] })).await.unwrap();
    let cfg = &w.row["automodConfig"];
    assert_eq!(cfg["version"], json!(2));
    assert_eq!(cfg["rules"]["invites"]["action"], json!("kick"), "the legacy invite filter survives");
    assert_eq!((cfg["rules"]["caps"]["action"].clone(), cfg["rules"]["caps"]["threshold"].clone()), (json!("delete"), json!(90)));
    assert_eq!(cfg["badWords"], json!(["legacy", "new one"]));
    let stored = e.app.settings().get_settings_with(LEGACY, true).await.unwrap();
    assert_eq!(stored.automod_config.as_ref().unwrap()["version"], json!(2));
    let values = read_settings_values(Some(&e.bridge.read_guild_settings(LEGACY).await.unwrap()));
    assert_eq!(values["automodRuleInvites"]["action"], json!("kick"));
    e.cleanup().await;
}

#[tokio::test]
async fn escalation_keeps_tier_ids_cross_field_rules_refuse_and_save_nothing() {
    let Some((e, _l)) = env().await else { return };
    e.write(GID, json!({ "infractionEscalation": [{ "warnCount": 3, "withinSeconds": null, "action": "TIMEOUT", "durationSeconds": 600 }] })).await.unwrap();
    let w = e
        .write(
            GID,
            json!({ "infractionEscalation": [
                { "warnCount": 3, "withinSeconds": null, "action": "KICK", "durationSeconds": null },
                { "warnCount": 5, "withinSeconds": null, "action": "BAN", "durationSeconds": null }
            ] }),
        )
        .await
        .unwrap();
    let tiers: Vec<(Value, Value)> = w.row["infractionEscalation"].as_array().unwrap().iter().map(|t| (t["id"].clone(), t["action"].clone())).collect();
    assert_eq!(tiers, vec![(json!(1), json!("KICK")), (json!(2), json!("BAN"))]);
    let r = e.write(GID, json!({ "infractionEscalation": [{ "warnCount": 2, "withinSeconds": null, "action": "MUTE", "durationSeconds": null }] })).await;
    assert_eq!(field_err(r), "infractionEscalation.0.action");
    assert_eq!(field_err(e.write(GID, json!({ "xpMin": 500 })).await), "xpMin");
    let after = e.app.settings().get_settings_with(GID, true).await.unwrap();
    assert_eq!(after.infraction_escalation.as_ref().and_then(Value::as_array).map(Vec::len), Some(2));
    assert_eq!(after.xp_min, 15);
    e.cleanup().await;
}

#[tokio::test]
async fn a_welcome_embed_write_keeps_the_effective_text_and_the_card_flag_a_cleared_json_column_is_sql_null() {
    let Some((e, _l)) = env().await else { return };
    let w = e.write(GID, json!({ "welcomeEmbed": { "title": "Hello {user}" } })).await.unwrap();
    assert_eq!(w.row["welcomeMessage"], json!(dash_schema::constants().default_texts["welcomeMessage"]), "the text in effect is frozen");
    assert_eq!(w.row["welcomeCardStyle"]["enabled"], json!(true));
    assert_eq!(w.row["welcomeEmbed"]["title"], json!("Hello {user}"));
    e.write(GID, json!({ "welcomeEmbed": null, "welcomeDmEmbed": null })).await.unwrap();
    let cleared: bool = sqlx::query_scalar(r#"SELECT "welcomeEmbed" IS NULL FROM "GuildSettings" WHERE "guildId" = $1"#).bind(GID).fetch_one(e.app.db()).await.unwrap();
    assert!(cleared);
    e.cleanup().await;
}

#[tokio::test]
async fn side_effects_captcha_kicks_the_captcha_panel_birthdays_balances_and_the_moderation_log() {
    let Some((e, _l)) = env().await else { return };
    let key = format!("captcha.timeout:{GID}:{USER_ID}");
    e.app
        .scheduler()
        .schedule_job(
            bot_core::scheduler::ScheduleJob::new(GID, bot_config::JobType::CaptchaTimeout, chrono::Utc::now() + chrono::Duration::hours(1))
                .payload(json!({ "userId": USER_ID }))
                .dedupe_key(key.clone()),
        )
        .await
        .unwrap();
    e.write(GID, json!({ "captchaTimeoutMinutes": 30 })).await.unwrap();
    let zero = e.write(GID, json!({ "captchaTimeoutMinutes": 0 })).await.unwrap();
    assert_eq!(zero.notices, vec!["Cancelled 1 scheduled kick of unverified members.".to_string()]);
    assert!(e.app.scheduler().get_job(&key).await.is_none());

    e.app.settings().update_settings(GID, SettingsPatch::new().set("captchaChannelId", CH_TEXT_).set("captchaMessageId", "700000000000000001")).await.unwrap();
    let panel = e.write(GID, json!({ "captchaVerifyButton": { "label": "Let me in", "style": "PRIMARY" } })).await.unwrap();
    assert!(panel.notices.is_empty(), "{:?}", panel.notices);
    let edit = e.discord.calls().into_iter().rev().find(|c| c.method == "editMessage").expect("the panel was edited");
    assert_eq!((edit.args["channelId"].clone(), edit.args["messageId"].clone()), (json!(CH_TEXT_), json!("700000000000000001")));
    let payload = &edit.args["payload"];
    assert_eq!(payload["components"][0]["components"][0]["label"], json!("Let me in"));
    assert_eq!(payload["allowed_mentions"], json!({ "parse": [] }));
    assert!(payload["embeds"][0]["description"].as_str().unwrap().contains("zeta"), "{{server}} rendered with the server name: {payload}");

    e.write(GID, json!({ "birthdayChannelId": CH_TEXT_ })).await.unwrap();
    assert_eq!(e.app.scheduler().get_job(&format!("birthday.tick:{GID}")).await.map(|j| j.status.to_string()), Some("PENDING".to_string()));

    for (user, balance) in [("955200000000000101", 900), ("955200000000000102", 50)] {
        sqlx::query(r#"INSERT INTO "EconomyAccount" ("id", "guildId", "userId", "balance", "updatedAt") VALUES ($1, $2, $3, $4, now())"#)
            .bind(bot_db::new_cuid())
            .bind(GID)
            .bind(user)
            .bind(balance)
            .execute(e.app.db())
            .await
            .unwrap();
    }
    let clamp = e.write(GID, json!({ "economyMaxBalance": 100, "economyStartingBalance": 10 })).await.unwrap();
    assert_eq!(clamp.notices, vec!["Lowered 1 balance to the new maximum.".to_string()]);
    let balances: Vec<i32> = sqlx::query_scalar(r#"SELECT "balance" FROM "EconomyAccount" WHERE "guildId" = $1 ORDER BY "userId""#).bind(GID).fetch_all(e.app.db()).await.unwrap();
    assert_eq!(balances, vec![100, 50]);

    e.write(GID, json!({ "modLogChannelId": CH_NEWS })).await.unwrap();
    let sent = e.discord.calls().into_iter().rev().find(|c| c.method == "sendMessage").expect("the confirmation was posted");
    assert_eq!(sent.args["channelId"], json!(CH_NEWS));
    assert!(sent.args["payload"]["embeds"][0]["title"].as_str().unwrap().contains("Moderation Log"));
    e.cleanup().await;
}

#[tokio::test]
async fn every_field_round_trips_through_the_database() {
    let Some((e, _l)) = env().await else { return };
    let defaults = read_settings_values(None);
    let mut changes: Map<String, Value> = defaults
        .iter()
        .filter(|(f, v)| !v.is_null() || dash_schema::settings_field(f).is_some_and(|d| d.nullable()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (k, v) in json!({
        "welcomeChannelId": CH_TEXT_, "goodbyeChannelId": CH_TEXT_, "levelUpChannelId": CH_NEWS, "achievementChannelId": CH_TEXT_,
        "captchaRoleId": RL_LOW, "liveRoleId": RL_MID, "muteRoleId": RL_MID, "embedColor": 0x123456, "customPrefix": "?",
        "logEvents": ["voiceJoin"], "xpBoostRoles": [{ "roleId": RL_MID, "multiplier": 2.5 }], "automodBadWords": ["nope"],
        "infractionEscalation": [{ "warnCount": 2, "withinSeconds": 86_400, "action": "MUTE", "durationSeconds": null }],
        "economyGameChannelIds": [CH_TEXT_], "welcomeDmEmbed": { "title": "DM" }, "modVisibility": "ANONYMOUS", "xpMultiplier": 1.5
    })
    .as_object()
    .unwrap()
    {
        changes.insert(k.clone(), v.clone());
    }
    let w = e.write(ROUND, Value::Object(changes.clone())).await.unwrap();
    let back = read_settings_values(Some(&w.row));
    let expected = read_settings_values(Some(&Value::Object(to_stored_changes(&Env::valid(Value::Object(changes.clone())), None))));
    for field in changes.keys() {
        assert_eq!(back[field], expected[field], "{field}");
    }
    let again = read_settings_values(Some(&e.bridge.read_guild_settings(ROUND).await.unwrap()));
    assert_eq!(again, back, "what was saved is what is read");
    assert_eq!(back["customPrefix"], json!("?"));
    assert_eq!(back["embedColor"], json!(0x123456));
    assert_eq!(back["xpBoostRoles"], json!([{ "roleId": RL_MID, "multiplier": 2.5 }]));
    assert_eq!(back["welcomeDmEmbed"]["title"], json!("DM"));
    assert_eq!(back["infractionEscalation"][0]["withinSeconds"], json!(86_400));
    e.cleanup().await;
}

#[tokio::test]
async fn a_card_background_that_cannot_be_downloaded_is_refused_with_its_path() {
    let Some((e, _l)) = env().await else { return };
    let r = e.write(GID, json!({ "rankCardStyle": { "backgroundUrl": "https://127.0.0.1/bg.png" } })).await;
    assert_eq!(field_err(r), "rankCardStyle.backgroundUrl");
    e.cleanup().await;
}

#[test]
fn the_captcha_panel_context_is_settings_bridge_guild_like_animated_icons_stay_png() {
    // settingsBridge.js guildLike: iconURL is `${CDN}/icons/${id}/${icon}.png?size=256` for every icon (the embeds
    // store's context writes animated icons as .gif).
    let mut res = fake_resources(GID);
    res.guild.icon = Some("a_0123456789abcdef0123456789abcdef".into());
    let ctx = dashboard::settings_bridge::panel_render_context(&res, CH_TEXT_);
    let g = ctx.guild.as_ref().expect("guild");
    assert_eq!(
        g.icon_url.as_deref(),
        Some(format!("https://cdn.discordapp.com/icons/{GID}/a_0123456789abcdef0123456789abcdef.png?size=256").as_str())
    );
    assert_eq!(g.member_count, Some(12.0));
    assert_eq!(ctx.channel.as_ref().map(|c| (c.id.as_str(), c.name.as_deref(), c.kind)), Some((CH_TEXT_, Some("general"), Some(0))));
    res.guild.icon = None;
    assert_eq!(dashboard::settings_bridge::panel_render_context(&res, CH_TEXT_).guild.and_then(|g| g.icon_url), None);
}
