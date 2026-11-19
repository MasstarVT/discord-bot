//! Fakes shared by the dash-server tests: ports of createFakes / createRecordFakes / the role-check records of
//! test/dashboard.test.js and the login harness of test/sec-dashboard.test.js, recording their calls in the JSON shape
//! the golden script writes.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use dash_auth::auth::TokenGrant;
use dash_auth::DashboardConfig;
use dash_auth::config::load_dashboard_config_from_map;
use dash_collections::discord::{
    DashboardDiscord, DiscordApiError, DiscordResources, InviteRef, MemberProfile, MemberRoles, MessageRef,
};
use dash_collections::store::{ActionAnswer, Actor, ListAnswer, ListOpts, StoreError};
use dash_collections::{RecordError, RecordErrorCode, RecordsBridge, Saved, SettingsBridge, SettingsWriteCtx, SettingsWriteError, SettingsWritten};
use dash_schema::FieldError;
use dash_server::discord_api::{BotGuild, CurrentUser, OAuthGuild, ServerDiscord, user_avatar_url};
use dash_server::server::{BotJoinRow, BotJoins};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use tower::ServiceExt;

pub const NOW: f64 = 1_800_000_000_000.0;
pub const SECRET: &str = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
pub const USER_ID: &str = "111111111111111111";
pub const OWNER_ID: &str = "999999999999999999";
pub const TOKEN: &str = "user-access-token-abc123";
pub const GUILD_OWNER: &str = "555555555555555555";
pub const BOT_ID: &str = "123456789012345678";

pub mod g {
    pub const ZETA: &str = "200000000000000001";
    pub const ALPHA: &str = "200000000000000002";
    pub const BETA: &str = "200000000000000003";
    pub const PLAIN: &str = "200000000000000004";
    pub const BROKEN: &str = "200000000000000005";
    pub const STRANGER: &str = "200000000000000099";
    pub const BOT_ONLY: &str = "200000000000000006";
}
pub mod ch {
    pub const TEXT: &str = "300000000000000001";
    pub const VOICE: &str = "300000000000000002";
    pub const CATEGORY: &str = "300000000000000003";
    pub const NEWS: &str = "300000000000000004";
    pub const STALE: &str = "300000000000000098";
    pub const GHOST: &str = "300000000000000099";
}
pub mod rl {
    pub const LOW: &str = "400000000000000001";
    pub const ADMIN: &str = "400000000000000002";
    pub const MID: &str = "400000000000000003";
    pub const MANAGED: &str = "400000000000000004";
    pub const HIGH: &str = "400000000000000005";
    pub const STALE: &str = "400000000000000098";
}

pub fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `loadDashboardConfig(baseEnv(extra)).config`.
pub fn config(extra: &[(&str, &str)]) -> DashboardConfig {
    let mut env: HashMap<String, String> = [
        ("CLIENT_ID", "123456789012345678"),
        ("DISCORD_TOKEN", "bot-token"),
        ("DISCORD_CLIENT_SECRET", "client-secret"),
        ("DASHBOARD_SESSION_SECRET", SECRET),
        ("DATABASE_URL", "postgresql://u:p@localhost/db"),
        ("REDIS_URL", "redis://localhost:6379"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    for (k, v) in extra {
        env.insert(k.to_string(), v.to_string());
    }
    load_dashboard_config_from_map(&env).config
}

fn user_guilds() -> Vec<OAuthGuild> {
    let g = |id: &str, name: &str, icon: Option<String>, owner: bool, perms: &str| OAuthGuild {
        id: id.into(),
        name: name.into(),
        icon,
        owner,
        permissions: perms.into(),
    };
    vec![
        g(g::ZETA, "zeta", None, false, "32"),
        g(g::ALPHA, "Alpha", Some("b".repeat(32)), true, "0"),
        g(g::BETA, "beta", None, false, "8"),
        g(g::PLAIN, "Plain", None, false, "1024"),
        g(g::BROKEN, "Broken", None, true, "0"),
    ]
}

const BOT_GUILDS: [&str; 5] = [g::ZETA, g::BETA, g::PLAIN, g::BROKEN, g::BOT_ONLY];

pub fn fake_resources(guild_id: &str) -> DiscordResources {
    let role = |id: &str, name: &str, position: i64, extra: Value| {
        let mut base = json!({
            "id": id, "name": name, "color": null, "position": position, "managed": false, "everyone": false,
            "mentionable": false, "assignable": position < 6, "dangerous": false,
        });
        if let (Value::Object(b), Value::Object(e)) = (&mut base, extra) {
            b.extend(e);
        }
        base
    };
    serde_json::from_value(json!({
        "guild": { "id": guild_id, "name": "zeta", "icon": null, "memberCount": 12, "ownerId": GUILD_OWNER },
        "channels": [
            { "id": ch::TEXT, "name": "general", "type": 0, "kind": "text", "parentId": null, "position": 0, "nsfw": false },
            { "id": ch::VOICE, "name": "Lounge", "type": 2, "kind": "voice", "parentId": null, "position": 1, "nsfw": false },
            { "id": ch::CATEGORY, "name": "INFO", "type": 4, "kind": "category", "parentId": null, "position": 0, "nsfw": false },
            { "id": ch::NEWS, "name": "news", "type": 5, "kind": "announcement", "parentId": ch::CATEGORY, "position": 0, "nsfw": false },
        ],
        "roles": [
            role(rl::HIGH, "Above the bot", 8, json!({})),
            role(rl::MANAGED, "Some bot", 5, json!({ "managed": true, "assignable": false })),
            role(rl::MID, "Mid", 3, json!({})),
            role(rl::ADMIN, "Admins", 2, json!({ "dangerous": true })),
            role(rl::LOW, "Low", 1, json!({})),
            role(guild_id, "@everyone", 0, json!({ "everyone": true, "assignable": false })),
        ],
        "emojis": [{ "id": "600000000000000001", "name": "coin", "animated": false }],
        "bot": { "userId": BOT_ID, "topRolePosition": 6, "manageRoles": true, "createInvite": guild_id != g::PLAIN },
    }))
    .expect("fake resources")
}

fn err(message: &str, status: u16, code: &str) -> DiscordApiError {
    DiscordApiError::new(message, status, code)
}

/// The calls of `createFakes()`, in the JSON shape of the golden.
pub type Calls = Arc<Mutex<Value>>;

fn push(calls: &Calls, key: &str, v: Value) {
    if let Some(a) = lock(calls).get_mut(key).and_then(Value::as_array_mut) {
        a.push(v);
    }
}

/// createFakes().discord.
pub struct FakeDiscord {
    pub calls: Calls,
    pub invite_deny: Mutex<HashSet<String>>,
}

#[async_trait]
impl DashboardDiscord for FakeDiscord {
    async fn get_guild_resources(&self, guild_id: &str, fresh: bool) -> Result<Arc<DiscordResources>, DiscordApiError> {
        push(&self.calls, "resources", json!({ "guildId": guild_id, "fresh": fresh }));
        if guild_id == g::BETA {
            return Err(err("Discord returned HTTP 503", 503, "unavailable"));
        }
        Ok(Arc::new(fake_resources(guild_id)))
    }
    async fn get_member(&self, guild_id: &str, user_id: &str) -> Result<Option<MemberRoles>, DiscordApiError> {
        push(&self.calls, "getMember", json!({ "guildId": guild_id, "userId": user_id }));
        Ok(Some(MemberRoles { roles: vec![rl::LOW.into(), rl::MID.into()] }))
    }
    async fn search_members(&self, _guild_id: &str, query: &str) -> Result<Vec<MemberProfile>, DiscordApiError> {
        Ok(vec![MemberProfile {
            id: USER_ID.into(),
            username: "alice".into(),
            global_name: Some("Alice".into()),
            nick: Some(query.into()),
            avatar_url: user_avatar_url(Some(USER_ID), None),
            bot: false,
        }])
    }
    async fn get_users(&self, _ids: &[String]) -> Result<HashMap<String, Option<MemberProfile>>, DiscordApiError> {
        Err(err("not in the fake", 0, "unavailable"))
    }
    async fn send_message(&self, _c: &str, _p: Value) -> Result<MessageRef, DiscordApiError> {
        Err(err("not in the fake", 0, "unavailable"))
    }
    async fn edit_message(&self, _c: &str, _m: &str, _p: Value) -> Result<MessageRef, DiscordApiError> {
        Err(err("not in the fake", 0, "unavailable"))
    }
    async fn delete_message(&self, _c: &str, _m: &str) -> Result<(), DiscordApiError> {
        Err(err("not in the fake", 0, "unavailable"))
    }
    async fn create_invite(&self, channel_id: &str, reason: &str) -> Result<InviteRef, DiscordApiError> {
        push(&self.calls, "invites", json!({ "channelId": channel_id, "reason": reason }));
        if lock(&self.invite_deny).contains(channel_id) {
            return Err(err("Discord returned HTTP 403", 403, "forbidden").with_discord_code(50013));
        }
        Ok(InviteRef { code: "abc123".into(), url: "https://discord.gg/abc123".into(), expires_at: Some("2026-10-13T12:30:00.000Z".into()) })
    }
}

#[async_trait]
impl ServerDiscord for FakeDiscord {
    async fn get_current_user(&self, _token: &str) -> Result<CurrentUser, DiscordApiError> {
        Ok(CurrentUser { id: USER_ID.into(), username: "alice".into(), global_name: Some("Alice".into()), avatar: None })
    }
    async fn get_user_guilds(&self, _uid: &str, token: &str, allow_stale: bool) -> Result<Arc<Vec<OAuthGuild>>, DiscordApiError> {
        push(&self.calls, "getUserGuilds", json!({ "token": token, "allowStale": allow_stale }));
        match token {
            "stale-limited-token" | "stale-down-token" => {
                if allow_stale {
                    return Ok(Arc::new(user_guilds()));
                }
                if token == "stale-limited-token" {
                    let mut e = err("Discord returned HTTP 429", 429, "rate_limited");
                    e.retry_after = Some(4);
                    return Err(e);
                }
                Err(err("Discord request failed", 0, "unavailable"))
            }
            "expired-token" => Err(err("Discord returned HTTP 401", 401, "unauthenticated")),
            "limited-token" => {
                let mut e = err("Discord returned HTTP 429", 429, "rate_limited");
                e.retry_after = Some(7);
                Err(e)
            }
            "down-token" => Err(err("Discord request failed", 0, "unavailable")),
            // The JS fake answers null, which the server meets with a TypeError (500).
            "broken-token" => panic!("getUserGuilds returned no list"),
            _ => Ok(Arc::new(user_guilds())),
        }
    }
    fn forget_user(&self, user_id: &str) {
        push(&self.calls, "forgetUser", json!(user_id));
    }
    async fn get_bot_guilds(&self) -> Result<Arc<Vec<BotGuild>>, DiscordApiError> {
        let mine = user_guilds();
        Ok(Arc::new(
            BOT_GUILDS
                .iter()
                .map(|id| {
                    let u = mine.iter().find(|g| g.id == *id);
                    BotGuild {
                        id: id.to_string(),
                        name: u.map_or_else(|| "Bot only".to_string(), |g| g.name.clone()),
                        icon: u.and_then(|g| g.icon.clone()),
                    }
                })
                .collect(),
        ))
    }
    async fn is_bot_in_guild(&self, guild_id: &str, _verify: bool) -> Result<bool, DiscordApiError> {
        Ok(BOT_GUILDS.contains(&guild_id))
    }
    fn invite_url(&self, guild_id: Option<&str>) -> String {
        format!("https://discord.com/oauth2/authorize?client_id=123456789012345678&guild_id={}", guild_id.unwrap_or("undefined"))
    }
    async fn exchange_code(&self, code: &str) -> anyhow::Result<TokenGrant> {
        push(&self.calls, "exchangeCode", json!(code));
        if code != "good-code" {
            anyhow::bail!("token exchange returned HTTP 400");
        }
        Ok(TokenGrant { access_token: TOKEN.into(), expires_in: 604_800.0 })
    }
    async fn revoke_token(&self, token: &str) -> bool {
        push(&self.calls, "revokeToken", json!(token));
        true
    }
}

/// createFakes().settings: the same merge and cross-field rules as settingsBridge, over one shared row.
pub struct FakeSettings {
    pub calls: Calls,
    pub row: Mutex<Value>,
}

#[async_trait]
impl SettingsBridge for FakeSettings {
    async fn read_guild_settings(&self, guild_id: &str) -> anyhow::Result<Value> {
        if guild_id == g::BROKEN {
            anyhow::bail!("database down");
        }
        let mut row = lock(&self.row).clone();
        row["guildId"] = json!(guild_id);
        Ok(row)
    }
    async fn write_settings(&self, guild_id: &str, changes: Map<String, Value>, ctx: SettingsWriteCtx) -> Result<SettingsWritten, SettingsWriteError> {
        push(
            &self.calls,
            "writeSettings",
            json!({ "guildId": guild_id, "changes": changes, "userId": ctx.user_id, "hasDiscord": ctx.discord.is_some() }),
        );
        if guild_id == g::BROKEN {
            return Err(SettingsWriteError::Internal(anyhow::anyhow!("database down")));
        }
        let row = lock(&self.row).clone();
        let data = dash_schema::to_stored_changes(&changes, Some(&row));
        let mut merged = row.as_object().cloned().unwrap_or_default();
        merged.extend(data);
        let merged = Value::Object(merged);
        if let Some(p) = dash_schema::check_cross_fields(Some(&merged), Some(&changes)) {
            return Err(SettingsWriteError::Field(FieldError::new(p.path, p.message)));
        }
        *lock(&self.row) = merged.clone();
        let mut out = merged;
        out["guildId"] = json!(guild_id);
        let notices = if changes.contains_key("economyMaxBalance") { vec!["Lowered 2 balances to the new maximum.".to_string()] } else { vec![] };
        Ok(SettingsWritten { row: out, notices })
    }
}

pub struct Fakes {
    pub discord: Arc<FakeDiscord>,
    pub settings: Arc<FakeSettings>,
    pub calls: Calls,
}

pub fn create_fakes() -> Fakes {
    let calls: Calls = Arc::new(Mutex::new(json!({
        "writeSettings": [], "exchangeCode": [], "revokeToken": [], "forgetUser": [], "getUserGuilds": [], "resources": [],
        "getMember": [], "invites": [],
    })));
    let discord = Arc::new(FakeDiscord { calls: calls.clone(), invite_deny: Mutex::new(HashSet::from([ch::TEXT.to_string()])) });
    let row = json!({
        "guildId": g::ZETA, "modEnabled": true, "xpEnabled": false, "voiceXpEnabled": true, "levelingEnabled": true,
        "captchaLockdownPrior": true, "captchaMessageId": "700000000000000001", "timezone": "Europe/Berlin", "customPrefix": "",
        "embedColor": 0x5865f2, "logIgnoredChannelIds": [ch::STALE], "modRoleIds": [rl::STALE], "xpMin": 15, "xpMax": 25,
    });
    let settings = Arc::new(FakeSettings { calls: calls.clone(), row: Mutex::new(row) });
    Fakes { discord, settings, calls }
}

/// createRecordFakes(): records of the THINGS collection.
pub struct FakeRecords {
    pub calls: Mutex<Vec<Value>>,
    pub items: Mutex<IndexMap<String, Value>>,
}

impl FakeRecords {
    pub fn new() -> Self {
        let mut items = IndexMap::new();
        items.insert("t1".to_string(), json!({ "id": "t1", "name": "One", "channelId": ch::STALE, "roleId": null }));
        items.insert("t3".to_string(), json!({ "id": "t3", "name": "Locked", "channelId": ch::TEXT, "roleId": null }));
        Self { calls: Mutex::new(Vec::new()), items: Mutex::new(items) }
    }
    fn call(&self, v: Value) {
        lock(&self.calls).push(v);
    }
}

fn actor_json(a: &Actor) -> Value {
    serde_json::to_value(a).unwrap_or(Value::Null)
}

#[async_trait]
impl RecordsBridge for FakeRecords {
    async fn list(&self, guild_id: &str, key: &str, opts: ListOpts) -> Result<ListAnswer, StoreError> {
        self.call(json!(["list", guild_id, key, { "query": opts.query, "cursor": opts.cursor }]));
        Ok(ListAnswer::items(lock(&self.items).values().cloned().collect()))
    }
    async fn get(&self, guild_id: &str, key: &str, id: &str) -> Result<Option<Value>, StoreError> {
        self.call(json!(["get", guild_id, key, id]));
        Ok(lock(&self.items).get(id).cloned())
    }
    async fn create(&self, guild_id: &str, key: &str, data: Map<String, Value>, actor: &Actor) -> Result<Saved, StoreError> {
        self.call(json!(["create", guild_id, key, data, actor_json(actor)]));
        let name = data.get("name").and_then(Value::as_str).unwrap_or_default();
        match name {
            "taken" => return Err(RecordError::new(RecordErrorCode::Conflict, "A thing with that name exists.").with_path("name").into()),
            "full" => return Err(RecordError::new(RecordErrorCode::Limit, "This server already has 5 things.").into()),
            "boom" => return Err(StoreError::Internal(anyhow::anyhow!("database down"))),
            "half" => return Err(RecordError::new(RecordErrorCode::Failed, "The thing was saved, but it was not posted.").saved().into()),
            "noted" => {
                let mut item = Map::new();
                item.insert("id".into(), json!("t5"));
                item.extend(data);
                return Ok(Saved::with_message(Value::Object(item), Some("Saved with a note.")));
            }
            "refused" => return Err(DiscordApiError::new("Discord returned HTTP 403", 403, "forbidden").with_discord_code(50013).into()),
            _ => {}
        }
        let mut item = Map::new();
        item.insert("id".into(), json!("t2"));
        item.extend(data);
        let item = Value::Object(item);
        lock(&self.items).insert("t2".into(), item.clone());
        Ok(Saved::item(item))
    }
    async fn update(&self, guild_id: &str, key: &str, id: &str, data: Map<String, Value>, actor: &Actor) -> Result<Saved, StoreError> {
        self.call(json!(["update", guild_id, key, id, data, actor_json(actor)]));
        let mut items = lock(&self.items);
        let mut item = items.get(id).and_then(Value::as_object).cloned().unwrap_or_default();
        item.extend(data);
        items.insert(id.to_string(), Value::Object(item.clone()));
        Ok(Saved::item(Value::Object(item)))
    }
    async fn remove(&self, guild_id: &str, key: &str, id: &str, actor: &Actor) -> Result<(), StoreError> {
        self.call(json!(["remove", guild_id, key, id, actor_json(actor)]));
        if lock(&self.items).shift_remove(id).is_none() {
            return Err(RecordError::not_found("That thing does not exist any more.").into());
        }
        Ok(())
    }
    async fn action(&self, guild_id: &str, key: &str, id: &str, action_id: &str, data: Map<String, Value>, actor: &Actor) -> Result<ActionAnswer, StoreError> {
        self.call(json!(["action", guild_id, key, id, action_id, data, actor_json(actor)]));
        if id == "t3" {
            return Err(RecordError::new(RecordErrorCode::Discord, "The bot cannot post in that channel.").into());
        }
        Ok(ActionAnswer { item: lock(&self.items).get(id).cloned(), message: Some("Posted.".into()) })
    }
}

/// The records of the role-check describe: get and update only.
pub struct RoleRecords {
    pub items: Mutex<IndexMap<String, Value>>,
}

impl RoleRecords {
    pub fn new() -> Self {
        let mut items = IndexMap::new();
        for v in [
            json!({ "id": "p1", "name": "One", "pingRoleIds": [rl::ADMIN, rl::MID], "roleId": null, "rewardRoleId": null }),
            json!({ "id": "a1", "actions": [{ "type": "TAKE", "roleIds": [rl::MID] }, { "type": "GIVE", "roleIds": [rl::ADMIN] }] }),
            json!({ "id": "l-high", "name": "High", "enabled": false, "actions": [{ "roleId": rl::LOW }, { "roleId": rl::MID }] }),
            json!({ "id": "l-low", "name": "Low", "enabled": false, "actions": [{ "roleId": rl::LOW }] }),
            json!({ "id": "n-high", "name": "NoFlag", "enabled": false, "pingRoleIds": [], "roleId": rl::MID, "rewardRoleId": null }),
        ] {
            items.insert(v["id"].as_str().unwrap_or_default().to_string(), v);
        }
        Self { items: Mutex::new(items) }
    }
}

#[async_trait]
impl RecordsBridge for RoleRecords {
    async fn list(&self, _g: &str, _k: &str, _o: ListOpts) -> Result<ListAnswer, StoreError> {
        Err(StoreError::Internal(anyhow::anyhow!("records.list is not a function")))
    }
    async fn get(&self, _g: &str, _k: &str, id: &str) -> Result<Option<Value>, StoreError> {
        Ok(lock(&self.items).get(id).cloned())
    }
    async fn create(&self, _g: &str, _k: &str, _d: Map<String, Value>, _a: &Actor) -> Result<Saved, StoreError> {
        Err(StoreError::Internal(anyhow::anyhow!("records.create is not a function")))
    }
    async fn update(&self, _g: &str, _k: &str, id: &str, data: Map<String, Value>, _a: &Actor) -> Result<Saved, StoreError> {
        let mut items = lock(&self.items);
        let mut item = items.get(id).and_then(Value::as_object).cloned().unwrap_or_default();
        item.extend(data);
        items.insert(id.to_string(), Value::Object(item.clone()));
        Ok(Saved::item(Value::Object(item)))
    }
    async fn remove(&self, _g: &str, _k: &str, _i: &str, _a: &Actor) -> Result<(), StoreError> {
        Err(StoreError::Internal(anyhow::anyhow!("records.remove is not a function")))
    }
    async fn action(&self, _g: &str, _k: &str, _i: &str, _x: &str, _d: Map<String, Value>, _a: &Actor) -> Result<ActionAnswer, StoreError> {
        Err(StoreError::Internal(anyhow::anyhow!("records.action is not a function")))
    }
}

/// The login harness of test/sec-dashboard.test.js: every code but `legit_code` fails; `slow*` codes wait for a gate.
pub struct LoginDiscord {
    pub exchanges: Mutex<Vec<String>>,
    pub gate: tokio::sync::Semaphore,
}

impl LoginDiscord {
    pub fn new(open: bool) -> Self {
        Self { exchanges: Mutex::new(Vec::new()), gate: tokio::sync::Semaphore::new(if open { tokio::sync::Semaphore::MAX_PERMITS } else { 0 }) }
    }
    pub fn release(&self) {
        self.gate.add_permits(1_000_000);
    }
}

#[async_trait]
impl DashboardDiscord for LoginDiscord {
    async fn get_guild_resources(&self, _g: &str, _f: bool) -> Result<Arc<DiscordResources>, DiscordApiError> {
        Err(err("no", 0, "unavailable"))
    }
    async fn get_member(&self, _g: &str, _u: &str) -> Result<Option<MemberRoles>, DiscordApiError> {
        Err(err("no", 0, "unavailable"))
    }
    async fn search_members(&self, _g: &str, _q: &str) -> Result<Vec<MemberProfile>, DiscordApiError> {
        Err(err("no", 0, "unavailable"))
    }
    async fn get_users(&self, _i: &[String]) -> Result<HashMap<String, Option<MemberProfile>>, DiscordApiError> {
        Err(err("no", 0, "unavailable"))
    }
    async fn send_message(&self, _c: &str, _p: Value) -> Result<MessageRef, DiscordApiError> {
        Err(err("no", 0, "unavailable"))
    }
    async fn edit_message(&self, _c: &str, _m: &str, _p: Value) -> Result<MessageRef, DiscordApiError> {
        Err(err("no", 0, "unavailable"))
    }
    async fn delete_message(&self, _c: &str, _m: &str) -> Result<(), DiscordApiError> {
        Err(err("no", 0, "unavailable"))
    }
    async fn create_invite(&self, _c: &str, _r: &str) -> Result<InviteRef, DiscordApiError> {
        Err(err("no", 0, "unavailable"))
    }
}

#[async_trait]
impl ServerDiscord for LoginDiscord {
    async fn get_current_user(&self, _t: &str) -> Result<CurrentUser, DiscordApiError> {
        Ok(CurrentUser { id: "955050000000000001".into(), username: "admin".into(), global_name: None, avatar: None })
    }
    async fn get_user_guilds(&self, _u: &str, _t: &str, _s: bool) -> Result<Arc<Vec<OAuthGuild>>, DiscordApiError> {
        Err(err("no", 0, "unavailable"))
    }
    async fn get_bot_guilds(&self) -> Result<Arc<Vec<BotGuild>>, DiscordApiError> {
        Err(err("no", 0, "unavailable"))
    }
    async fn is_bot_in_guild(&self, _g: &str, _v: bool) -> Result<bool, DiscordApiError> {
        Err(err("no", 0, "unavailable"))
    }
    fn invite_url(&self, _g: Option<&str>) -> String {
        "/dashboard".into()
    }
    async fn exchange_code(&self, code: &str) -> anyhow::Result<TokenGrant> {
        lock(&self.exchanges).push(code.to_string());
        if code.starts_with("slow") {
            let _permit = self.gate.acquire().await;
        }
        if code == "legit_code" {
            return Ok(TokenGrant { access_token: "token".into(), expires_in: 3600.0 });
        }
        anyhow::bail!("token exchange returned HTTP 400")
    }
}

/// An empty settings bridge (`settings: {}` in the login harness).
pub struct NoSettings;

#[async_trait]
impl SettingsBridge for NoSettings {
    async fn read_guild_settings(&self, _g: &str) -> anyhow::Result<Value> {
        anyhow::bail!("settings.readGuildSettings is not a function")
    }
    async fn write_settings(&self, _g: &str, _c: Map<String, Value>, _x: SettingsWriteCtx) -> Result<SettingsWritten, SettingsWriteError> {
        Err(SettingsWriteError::Internal(anyhow::anyhow!("settings.writeSettings is not a function")))
    }
}

/// deps.botJoins of the "who added the bot" case.
pub struct FakeJoins {
    pub failing: Mutex<bool>,
    pub asked: Mutex<Vec<Vec<String>>>,
}

#[async_trait]
impl BotJoins for FakeJoins {
    async fn list_bot_joins(&self, ids: &[String]) -> anyhow::Result<HashMap<String, BotJoinRow>> {
        lock(&self.asked).push(ids.to_vec());
        if *lock(&self.failing) {
            anyhow::bail!("database down");
        }
        let joined = "2026-10-01T12:00:00.000Z".to_string();
        let mut out = HashMap::new();
        for id in ids {
            if id == g::ZETA {
                out.insert(id.clone(), BotJoinRow {
                    inviter_id: Some("200000000000000001".into()),
                    inviter_name: Some("alice".into()),
                    source: Some("audit".into()),
                    joined_at: joined.clone(),
                });
            } else if id == g::BOT_ONLY {
                out.insert(id.clone(), BotJoinRow { inviter_id: None, inviter_name: None, source: None, joined_at: joined.clone() });
            }
        }
        Ok(out)
    }
}

/// A sealed session cookie (`sessionCookie(overrides)`): `dash_session=<sealed>`.
pub fn session_cookie(overrides: Value) -> String {
    let key = dash_auth::auth::derive_session_key(SECRET).expect("key");
    let mut payload = json!({ "uid": USER_ID, "username": "alice", "globalName": "Alice", "avatar": null, "accessToken": TOKEN, "exp": (NOW as i64) + 3_600_000 });
    if let (Value::Object(p), Value::Object(o)) = (&mut payload, overrides) {
        p.extend(o);
    }
    format!("dash_session={}", dash_auth::auth::seal_session(&payload, &key))
}

/// One answer: status, headers and body.
pub struct Answer {
    pub status: u16,
    pub headers: http::HeaderMap,
    pub body: Vec<u8>,
}

impl Answer {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers.get(name).map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
    }
    pub fn cookies(&self) -> Vec<String> {
        self.headers.get_all("set-cookie").iter().map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned()).collect()
    }
}

/// Sends one request through the router (as from 127.0.0.1, so X-Forwarded-For is believed).
pub async fn call(router: &Router, method: &str, path: &str, headers: &[(&str, String)], body: Option<Vec<u8>>) -> Answer {
    let mut builder = http::Request::builder().method(method).uri(path);
    for (k, v) in headers {
        builder = builder.header(*k, v);
    }
    if let Some(b) = &body
        && !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-length"))
    {
        builder = builder.header("content-length", b.len().to_string());
    }
    let mut req = builder.body(Body::from(body.unwrap_or_default())).expect("request");
    req.extensions_mut().insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 40000))));
    let res = router.clone().oneshot(req).await.expect("infallible");
    let status = res.status().as_u16();
    let headers = res.headers().clone();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.expect("body").to_vec();
    Answer { status, headers, body }
}

pub const WRITE: [(&str, &str); 3] = [("content-type", "application/json"), ("x-dashboard", "1"), ("origin", "http://localhost:8080")];

pub fn write_headers(cookie: &str) -> Vec<(&'static str, String)> {
    let mut h: Vec<(&str, String)> = WRITE.iter().map(|(k, v)| (*k, v.to_string())).collect();
    h.push(("cookie", cookie.to_string()));
    h
}
