//! src/dashboard/settingsBridge.js ported to bins/dashboard `settings_bridge`, against the JS bridge on the same
//! database and Redis (rust/fixtures/gen/dashboard-js.mjs): the normalised rows both read, the same writes on twin
//! guilds (rows, notices, Discord calls, refusals), and each side reading what the other wrote.

mod common;

use std::sync::Arc;

use common::*;
use dash_collections::testing::FakeDiscord;
use dash_collections::{DashboardDiscord, DiscordResources, SettingsBridge, SettingsWriteCtx, SettingsWriteError};
use dashboard::settings_bridge::{DashboardSettings, normalize_row};
use serde_json::{Map, Value, json};

const A: &str = "950000000000000001"; // written by the JS bridge
const B: &str = "950000000000000002"; // written by the Rust bridge
const PANEL_MESSAGE: &str = "930000000000000001";

fn resources(g: &str) -> Value {
    json!({
        "guild": { "id": g, "name": "Test Server", "icon": null, "memberCount": 3, "ownerId": OWNER, "premiumTier": 0,
                   "premiumSubscriptionCount": 0, "verificationLevel": 1 },
        "channels": [
            { "id": CH_TEXT, "name": "general", "type": 0, "kind": "text", "parentId": null, "position": 0, "nsfw": false },
            { "id": CH_LOG, "name": "mod-log", "type": 0, "kind": "text", "parentId": null, "position": 1, "nsfw": false }
        ],
        "roles": [
            { "id": g, "name": "@everyone", "color": null, "position": 0, "managed": false, "everyone": true, "mentionable": true, "assignable": false, "dangerous": false },
            { "id": ROLE_PLAIN, "name": "Member", "color": null, "position": 1, "managed": false, "everyone": false, "mentionable": false, "assignable": true, "dangerous": false }
        ],
        "emojis": [],
        "bot": { "userId": "600000000000000999", "topRolePosition": 5, "manageRoles": true, "createInvite": true }
    })
}

async fn seed(app: &bot_core::App, g: &str) {
    sqlx::query(r#"DELETE FROM "GuildSettings" WHERE "guildId" = $1"#).bind(g).execute(app.db()).await.unwrap();
    app.settings().invalidate_settings(g).await;
    app.settings().ensure_guild_settings(g).await.unwrap();
    sqlx::query(r#"UPDATE "GuildSettings" SET "captchaChannelId" = $2, "captchaMessageId" = $3, "economyMaxBalance" = 1000000, "captchaTimeoutMinutes" = 10 WHERE "guildId" = $1"#)
        .bind(g)
        .bind(CH_TEXT)
        .bind(PANEL_MESSAGE)
        .execute(app.db())
        .await
        .unwrap();
    for (user, balance) in [(ALICE, 900_000), (BOB, 600_000), (OWNER, 10)] {
        sqlx::query(r#"INSERT INTO "EconomyAccount" ("id", "guildId", "userId", "balance", "updatedAt") VALUES ($1, $2, $3, $4, now())"#)
            .bind(bot_db::new_cuid())
            .bind(g)
            .bind(user)
            .bind(balance)
            .execute(app.db())
            .await
            .unwrap();
    }
    let at = chrono::Utc::now() + chrono::Duration::minutes(10);
    for user in [ALICE, BOB] {
        app.scheduler()
            .schedule_job(
                bot_core::scheduler::ScheduleJob::new(g, bot_config::JobType::CaptchaTimeout, at)
                    .payload(json!({ "userId": user }))
                    .dedupe_key(format!("captcha:{g}:{user}")),
            )
            .await
            .unwrap();
    }
    // Drop the cached row so both bridges read the seeded one.
    app.settings().invalidate_settings(g).await;
}

/// A row without what differs between two guilds (ids, times), guild ids replaced.
fn comparable(row: &Value, g: &str) -> Value {
    let v = drop_keys(row, &["id", "createdAt", "updatedAt"]);
    replace_strings(&v, &[(g.to_string(), "<guild>".to_string())])
}

fn methods(calls: &[Value]) -> Vec<String> {
    calls.iter().map(|c| c["method"].as_str().unwrap_or_default().to_string()).collect()
}

async fn rust_write(bridge: &DashboardSettings, discord: &Arc<FakeDiscord>, g: &str, body: &Value) -> Result<(Map<String, Value>, Value, Vec<String>), Value> {
    let valid = dash_schema::validate_settings_changes(body).map_err(|e| json!({ "invalid": { "message": e.message, "path": e.path } }))?;
    let d: Arc<dyn DashboardDiscord> = discord.clone();
    match bridge.write_settings(g, valid.changes.clone(), SettingsWriteCtx { user_id: "600000000000000001".into(), discord: Some(d) }).await {
        Ok(w) => Ok((valid.changes, w.row, w.notices)),
        Err(SettingsWriteError::Field(f)) => Err(json!({ "name": "FieldError", "path": f.path, "message": f.message })),
        Err(SettingsWriteError::Internal(e)) => Err(json!({ "internal": e.to_string() })),
    }
}

#[tokio::test]
async fn both_bridges_read_the_seeded_rows_the_same() {
    let Some((db, redis)) = urls() else { return };
    let _one = SERIAL.lock().await;
    let app = dashboard_app(&db, &redis).await;
    let bridge = DashboardSettings::new(app.clone());
    // The two rows the JS toolchain seeded (one with only required fields, one with every column filled).
    for g in ["900000000000000001", "900000000000000002"] {
        let js = node(JS, &["settings-read", g]);
        let rust = bridge.read_guild_settings(g).await.unwrap();
        assert_json_eq(&format!("readGuildSettings({g})"), &rust, &js);
        // Key order too: readSettingsValues walks it.
        assert_eq!(rust.as_object().unwrap().keys().collect::<Vec<_>>(), js.as_object().unwrap().keys().collect::<Vec<_>>());
    }
}

#[tokio::test]
async fn the_same_write_through_both_bridges() {
    let Some((db, redis)) = urls() else { return };
    let _one = SERIAL.lock().await;
    let app = dashboard_app(&db, &redis).await;
    let bridge = DashboardSettings::new(app.clone());
    seed(&app, A).await;
    seed(&app, B).await;

    // Every side effect at once: voice sessions dropped, captcha kicks cancelled, the panel edited, birthdays
    // rescheduled, balances lowered, the moderation log confirmed, automod upgraded, the welcome text kept.
    let welcome = json!({ "title": "Welcome {user.name}!", "description": "Glad you are here in {server}.", "color": 3447003 });
    let body = json!({ "changes": {
        "xpEnabled": false, "captchaTimeoutMinutes": 0, "captchaVerifyButton": { "label": "I am human", "style": "SUCCESS" },
        "birthdayHour": 7, "birthdayChannelId": CH_TEXT, "economyMaxBalance": 700000, "modLogChannelId": CH_LOG, "automodEnabled": true,
        "welcomeEmbed": welcome
    }});
    // Open voice sessions of the guilds (dropped without credit).
    for g in [A, B] {
        app.cache().set(&format!("voice:start:{g}:{ALICE}"), &json!(1_700_000_000_000i64), 600).await;
    }

    let js = node(JS, &["settings-write", A, &body.to_string(), &resources(A).to_string()]);
    assert!(js.get("error").is_none(), "{js}");
    let discord = Arc::new(FakeDiscord::new());
    discord.set_resources(B, serde_json::from_value::<DiscordResources>(resources(B)).unwrap());
    let (changes, row, notices) = rust_write(&bridge, &discord, B, &body).await.expect("the Rust write succeeds");

    assert_json_eq("validated changes", &Value::Object(changes), &js["changes"]);
    assert_json_eq("written row", &comparable(&row, B), &comparable(&js["row"], A));
    assert_eq!(json!(notices), js["notices"]);
    assert_eq!(
        notices,
        vec!["Cancelled 2 scheduled kicks of unverified members.".to_string(), "Lowered 1 balance to the new maximum.".to_string()]
    );
    let rust_calls: Vec<Value> = discord.calls().into_iter().map(|c| json!({ "method": c.method, "args": c.args })).collect();
    assert_eq!(methods(&rust_calls), methods(js["calls"].as_array().unwrap()));
    assert_eq!(methods(&rust_calls), vec!["getGuildResources", "editMessage", "sendMessage"]);
    // The panel edit: the same message, content, button row and no pings.
    let js_edit = &js["calls"][1]["args"];
    let rust_edit = &rust_calls[1]["args"];
    let rust_edit_list: Vec<Value> = match rust_edit {
        Value::Array(a) => a.clone(),
        Value::Object(_) => vec![rust_edit["channelId"].clone(), rust_edit["messageId"].clone(), rust_edit["payload"].clone()],
        _ => panic!("{rust_edit}"),
    };
    assert_eq!(rust_edit_list[0], js_edit[0]);
    assert_eq!(rust_edit_list[1], js_edit[1]);
    let (rp, jp) = (&rust_edit_list[2], &js_edit[2]);
    assert_eq!(rp["content"], jp["content"]);
    assert_eq!(rp["allowed_mentions"], json!({ "parse": [] }));
    assert_eq!(rp["embeds"][0]["description"], jp["embeds"][0]["description"]);
    assert_eq!(rp["components"][0]["components"][0]["label"], jp["components"][0]["components"][0]["label"]);
    assert_eq!(rp["components"][0]["components"][0]["custom_id"], jp["components"][0]["components"][0]["custom_id"]);

    // The side effects in the database and Redis are the same on both guilds.
    for g in [A, B] {
        let kicks: i64 = sqlx::query_scalar(r#"SELECT count(*) FROM "ScheduledJob" WHERE "guildId" = $1 AND "type" = 'captcha.timeout'"#).bind(g).fetch_one(app.db()).await.unwrap();
        assert_eq!(kicks, 0, "{g}");
        let top: i32 = sqlx::query_scalar(r#"SELECT max("balance") FROM "EconomyAccount" WHERE "guildId" = $1"#).bind(g).fetch_one(app.db()).await.unwrap();
        assert_eq!(top, 700_000, "{g}");
        let birthday: Option<String> =
            sqlx::query_scalar(r#"SELECT "dedupeKey" FROM "ScheduledJob" WHERE "guildId" = $1 AND "type" = 'birthday.tick'"#).bind(g).fetch_optional(app.db()).await.unwrap().flatten();
        assert_eq!(birthday, Some(format!("birthday.tick:{g}")), "{g}");
        assert_eq!(app.cache().get(&format!("voice:start:{g}:{ALICE}")).await, None, "{g}");
    }

    // Each side reads what the other wrote.
    let rust_reads_js = bridge.read_guild_settings(A).await.unwrap();
    assert_json_eq("Rust reads the JS write", &rust_reads_js, &js["row"]);
    let js_reads_rust = node(JS, &["settings-read", B]);
    assert_json_eq("JS reads the Rust write", &js_reads_rust, &row);

    // Saving again changes nothing: no notices, no confirmation post, and the panel is edited again.
    let again = json!({ "changes": { "economyMaxBalance": 700000, "modLogChannelId": CH_LOG } });
    let js2 = node(JS, &["settings-write", A, &again.to_string(), &resources(A).to_string()]);
    let d2 = Arc::new(FakeDiscord::new());
    let (_, _, n2) = rust_write(&bridge, &d2, B, &again).await.unwrap();
    assert_eq!(json!(n2), js2["notices"]);
    assert!(d2.calls().is_empty());
    assert_eq!(js2["calls"], json!([]));

    for g in [A, B] {
        sqlx::query(r#"DELETE FROM "GuildSettings" WHERE "guildId" = $1"#).bind(g).execute(app.db()).await.unwrap();
    }
}

#[tokio::test]
async fn refusals_and_panel_notices_match() {
    let Some((db, redis)) = urls() else { return };
    let _one = SERIAL.lock().await;
    let app = dashboard_app(&db, &redis).await;
    let bridge = DashboardSettings::new(app.clone());
    seed(&app, A).await;
    seed(&app, B).await;

    // A cross-field rule of the merged row (nothing saved), and a card background that cannot be downloaded.
    for body in [
        json!({ "changes": { "automodBlockedDomains": ["example.com"], "automodAllowedDomains": ["example.com"] } }),
        json!({ "changes": { "welcomeCardStyle": { "backgroundUrl": "http://127.0.0.1/card.png" } } }),
        json!({ "changes": { "rankCardStyle": { "backgroundUrl": "https://localhost/card.png" } } }),
    ] {
        let js = node(JS, &["settings-write", A, &body.to_string(), "null"]);
        let rust = rust_write(&bridge, &Arc::new(FakeDiscord::new()), B, &body).await.expect_err("refused");
        assert_eq!(rust["path"], js["error"]["path"], "{body}: {js}");
        assert_eq!(rust["message"], js["error"]["message"], "{body}: {js}");
        assert_eq!(rust["name"], js["error"]["name"]);
    }

    // The panel notices: Discord down, the channel gone, no panel posted.
    let body = json!({ "changes": { "captchaVerifyButton": { "label": "Verify me", "style": "PRIMARY" } } });
    let js = node(JS, &["settings-write", A, &body.to_string(), "null"]);
    let (_, _, n) = rust_write(&bridge, &Arc::new(FakeDiscord::new()), B, &body).await.unwrap();
    assert_eq!(json!(n), js["notices"]);
    assert_eq!(n, vec!["Saved, but the posted panel could not be updated right now. Saving again retries.".to_string()]);

    let mut gone = resources(A);
    gone["channels"] = json!([]);
    let js = node(JS, &["settings-write", A, &body.to_string(), &gone.to_string()]);
    let d = Arc::new(FakeDiscord::new());
    let mut gone_b = resources(B);
    gone_b["channels"] = json!([]);
    d.set_resources(B, serde_json::from_value(gone_b).unwrap());
    let (_, _, n) = rust_write(&bridge, &d, B, &body).await.unwrap();
    assert_eq!(json!(n), js["notices"]);
    assert_eq!(n, vec!["Saved, but the verification channel was deleted: run /captcha setup in Discord.".to_string()]);

    for g in [A, B] {
        sqlx::query(r#"UPDATE "GuildSettings" SET "captchaMessageId" = NULL WHERE "guildId" = $1"#).bind(g).execute(app.db()).await.unwrap();
        app.settings().invalidate_settings(g).await;
    }
    let js = node(JS, &["settings-write", A, &body.to_string(), "null"]);
    let (_, _, n) = rust_write(&bridge, &Arc::new(FakeDiscord::new()), B, &body).await.unwrap();
    assert_eq!(json!(n), js["notices"]);
    assert_eq!(n, vec!["Saved. No verification panel is posted yet: post it with /captcha setup in Discord.".to_string()]);

    for g in [A, B] {
        sqlx::query(r#"DELETE FROM "GuildSettings" WHERE "guildId" = $1"#).bind(g).execute(app.db()).await.unwrap();
    }
}

#[test]
fn normalize_row_upgrades_and_heals_like_the_js() {
    // A v1 automod config and a legacy welcome embed with card: false.
    let mut row = bot_db::GuildSettings::from_json(&Value::Object(bot_core::settings::default_row_json(
        &bot_db::GUILD_SETTINGS_INFO,
        json!({ "id": "c1", "guildId": "950000000000000009" }).as_object().unwrap(),
    ))).expect("a default row parses");
    row.welcome_embed = Some(json!({ "title": "Hi", "card": false }));
    let v = normalize_row(&row);
    assert_eq!(v["welcomeCardStyle"]["enabled"], json!(false));
    assert_eq!(v["automodConfig"]["version"], json!(2));
    assert_eq!(v["goodbyeEmbed"], Value::Null);
}
