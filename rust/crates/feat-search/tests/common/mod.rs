//! Shared helpers of the feat-search tests: the registry, embed reduction, settings rows and a fake Discord.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bot_core::error::BoxFuture;
use bot_core::prelude::*;
use feat_search::ops::{DiscordOps, RestError};
use serde_json::{Value, json};
use serenity::all::{ChannelId, Guild, GuildId, Member, MessageId, RoleId, UserId};
use serenity::builder::CreateEmbed;

pub fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_search::register(&mut reg);
    reg
}

/// An embed reduced as `feat-search-golden.mjs` reduces the JS one.
pub fn reduce_embed(e: &CreateEmbed, keep_timestamp: bool) -> Value {
    let j = serde_json::to_value(e).unwrap_or(Value::Null);
    let mut out = serde_json::Map::new();
    for k in ["title", "description", "url", "color"] {
        if let Some(v) = j.get(k).filter(|v| !v.is_null()) {
            out.insert(k.into(), v.clone());
        }
    }
    if let Some(t) = j.pointer("/footer/text").filter(|v| !v.is_null()) {
        out.insert("footer".into(), t.clone());
    }
    if let Some(t) = j.pointer("/thumbnail/url").filter(|v| !v.is_null()) {
        out.insert("thumbnail".into(), t.clone());
    }
    let fields: Vec<Value> = j
        .get("fields")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|f| json!({ "name": f["name"], "value": f["value"], "inline": f["inline"] == true }))
        .collect();
    out.insert("fields".into(), Value::Array(fields));
    if keep_timestamp && let Some(ts) = j.get("timestamp").and_then(Value::as_str) {
        let ms = chrono::DateTime::parse_from_rfc3339(ts).map(|d| d.timestamp_millis()).unwrap_or(0);
        out.insert("timestamp".into(), json!(ms));
    }
    Value::Object(out)
}

/// A complete `GuildSettings` row with the schema defaults (guild 900000000000000001), edited by `f`.
pub fn settings_with(f: impl FnOnce(&mut bot_db::GuildSettings)) -> bot_db::GuildSettings {
    let path = bot_core::testing::repo_root().join("rust/fixtures/golden/mockdb/baseline.json");
    let base: Value = serde_json::from_str(&std::fs::read_to_string(path).expect("baseline")).expect("json");
    let mut s = bot_db::GuildSettings::from_json(&base["GuildSettings"]["min"]).expect("a GuildSettings row");
    f(&mut s);
    s
}

/// The settings a JS fixture passed (`null` or a partial object with `embedColor`).
pub async fn settings_from(v: &Value) -> Option<Settings> {
    if v.is_null() {
        return None;
    }
    let color = v["embedColor"].as_i64().map(|c| c as i32);
    Some(Arc::new(settings_with(|s| s.embed_color = color)))
}

// ── A fake Discord ────────────────────────────────────────────────────────────

/// One REST call the fake received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    AddRole { user: String, role: String, reason: String },
    RemoveRole { user: String, role: String, reason: String },
    Nick { user: String, nick: Option<String>, reason: String },
    Reply { channel: String, message: String, content: String },
    Delete { channel: String, message: String },
    Send { channel: String, content: String, roles: Vec<String> },
    FetchMember { user: String },
    FetchAll { guild: String },
}

/// A Discord whose guilds are serenity `Guild`s built from GUILD_CREATE JSON; role and nickname edits change them.
#[derive(Default)]
pub struct FakeDiscord {
    pub guilds: Mutex<HashMap<GuildId, Guild>>,
    pub calls: Mutex<Vec<Call>>,
    /// Discord error codes the next calls of a kind fail with (`"add"`, `"remove"`, `"nick"`, `"reply"`, `"send"`).
    pub fail: Mutex<HashMap<&'static str, Vec<u64>>>,
    /// Members `fetch_member` finds that are not in the guild's cache.
    pub remote_members: Mutex<HashMap<UserId, Member>>,
    pub next_message: Mutex<u64>,
    /// Milliseconds each role edit takes (to test coalescing).
    pub edit_delay_ms: Mutex<u64>,
}

pub const BOT: &str = bot_core::testing::TEST_BOT_ID;

impl FakeDiscord {
    pub fn new(guilds: &[Value]) -> Arc<Self> {
        let me = Self::default();
        for g in guilds {
            me.put_guild(g);
        }
        Arc::new(me)
    }

    pub fn put_guild(&self, g: &Value) {
        let event: serenity::all::GuildCreateEvent = serde_json::from_value(g.clone()).unwrap_or_else(|e| panic!("GUILD_CREATE: {e}"));
        self.guilds.lock().unwrap().insert(event.guild.id, event.guild);
    }

    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    pub fn clear(&self) {
        self.calls.lock().unwrap().clear();
    }

    pub fn fail_next(&self, kind: &'static str, code: u64) {
        self.fail.lock().unwrap().entry(kind).or_default().push(code);
    }

    fn take_failure(&self, kind: &'static str) -> Option<RestError> {
        let mut f = self.fail.lock().unwrap();
        let list = f.get_mut(kind)?;
        if list.is_empty() {
            return None;
        }
        let code = list.remove(0);
        Some(RestError::new(Some(code), format!("fake error {code}")))
    }

    pub fn edit<R>(&self, guild: &str, f: impl FnOnce(&mut Guild) -> R) -> R {
        let id = GuildId::new(guild.parse().expect("guild id"));
        let mut g = self.guilds.lock().unwrap();
        f(g.get_mut(&id).expect("guild"))
    }

    pub fn roles_of(&self, guild: &str, user: &str) -> Vec<String> {
        self.edit(guild, |g| {
            g.members.get(&UserId::new(user.parse().unwrap())).map(|m| m.roles.iter().map(|r| r.to_string()).collect()).unwrap_or_default()
        })
    }

    pub fn nick_of(&self, guild: &str, user: &str) -> Option<String> {
        self.edit(guild, |g| g.members.get(&UserId::new(user.parse().unwrap())).and_then(|m| m.nick.clone()))
    }

    /// Sets a member's presence activities (JSON activities, as the gateway sends them).
    pub fn set_presence(&self, guild: &str, user: &str, activities: Value) {
        let p: serenity::all::Presence = serde_json::from_value(json!({
            "user": { "id": user }, "status": "online", "activities": activities, "client_status": {}
        }))
        .expect("presence");
        self.edit(guild, |g| {
            g.presences.insert(UserId::new(user.parse().unwrap()), p);
        });
    }

    pub fn presence(&self, guild: &str, user: &str) -> Option<serenity::all::Presence> {
        self.edit(guild, |g| g.presences.get(&UserId::new(user.parse().unwrap())).cloned())
    }
}

/// A Streaming activity.
pub fn streaming() -> Value {
    json!([{ "type": 1, "name": "Twitch", "url": "https://www.twitch.tv/someone", "details": "Ranked", "state": "Valorant", "created_at": 0 }])
}

/// Implements [`DiscordOps`] for an `Arc<FakeDiscord>`.
pub struct FakeOps(pub Arc<FakeDiscord>);

impl DiscordOps for FakeOps {
    fn with_guild(&self, _app: &App, guild_id: GuildId, f: &mut dyn FnMut(&Guild)) {
        let g = self.0.guilds.lock().unwrap().get(&guild_id).cloned();
        if let Some(g) = g {
            f(&g);
        }
    }

    fn bot_id(&self, _app: &App) -> Option<UserId> {
        Some(UserId::new(BOT.parse().unwrap()))
    }

    fn fetch_member(&self, _app: &App, guild_id: GuildId, user_id: UserId) -> BoxFuture<Option<Member>> {
        let d = self.0.clone();
        Box::pin(async move {
            d.calls.lock().unwrap().push(Call::FetchMember { user: user_id.to_string() });
            // A fetch reads Discord's own copy (the remote one when a test set it), not the cache.
            let remote = d.remote_members.lock().unwrap().get(&user_id).cloned();
            remote.or_else(|| d.guilds.lock().unwrap().get(&guild_id).and_then(|g| g.members.get(&user_id).cloned()))
        })
    }

    fn add_role(&self, _app: &App, guild_id: GuildId, user_id: UserId, role_id: RoleId, reason: String) -> BoxFuture<Result<(), RestError>> {
        let d = self.0.clone();
        Box::pin(async move {
            let delay = *d.edit_delay_ms.lock().unwrap();
            if delay > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
            d.calls.lock().unwrap().push(Call::AddRole { user: user_id.to_string(), role: role_id.to_string(), reason });
            if let Some(e) = d.take_failure("add") {
                return Err(e);
            }
            let remote = d.remote_members.lock().unwrap().remove(&user_id);
            let mut gs = d.guilds.lock().unwrap();
            if let Some(g) = gs.get_mut(&guild_id) {
                if let Some(m) = remote {
                    g.members.insert(user_id, m);
                }
                if let Some(m) = g.members.get_mut(&user_id)
                    && !m.roles.contains(&role_id)
                {
                    m.roles.push(role_id);
                }
            }
            Ok(())
        })
    }

    fn remove_role(&self, _app: &App, guild_id: GuildId, user_id: UserId, role_id: RoleId, reason: String) -> BoxFuture<Result<(), RestError>> {
        let d = self.0.clone();
        Box::pin(async move {
            let delay = *d.edit_delay_ms.lock().unwrap();
            if delay > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
            d.calls.lock().unwrap().push(Call::RemoveRole { user: user_id.to_string(), role: role_id.to_string(), reason });
            if let Some(e) = d.take_failure("remove") {
                return Err(e);
            }
            if let Some(m) = d.guilds.lock().unwrap().get_mut(&guild_id).and_then(|g| g.members.get_mut(&user_id)) {
                m.roles.retain(|r| *r != role_id);
            }
            Ok(())
        })
    }

    fn set_nickname(&self, _app: &App, guild_id: GuildId, user_id: UserId, nick: Option<String>, reason: String) -> BoxFuture<Result<(), RestError>> {
        let d = self.0.clone();
        Box::pin(async move {
            d.calls.lock().unwrap().push(Call::Nick { user: user_id.to_string(), nick: nick.clone(), reason });
            if let Some(e) = d.take_failure("nick") {
                return Err(e);
            }
            if let Some(m) = d.guilds.lock().unwrap().get_mut(&guild_id).and_then(|g| g.members.get_mut(&user_id)) {
                m.nick = nick;
            }
            Ok(())
        })
    }

    fn reply(&self, _app: &App, channel_id: ChannelId, message_id: MessageId, content: String) -> BoxFuture<Result<MessageId, RestError>> {
        let d = self.0.clone();
        Box::pin(async move {
            d.calls.lock().unwrap().push(Call::Reply { channel: channel_id.to_string(), message: message_id.to_string(), content });
            if let Some(e) = d.take_failure("reply") {
                return Err(e);
            }
            let mut n = d.next_message.lock().unwrap();
            *n += 1;
            Ok(MessageId::new(990_000_000_000_000_000 + *n))
        })
    }

    fn delete_message(&self, _app: &App, channel_id: ChannelId, message_id: MessageId) -> BoxFuture<Result<(), RestError>> {
        let d = self.0.clone();
        Box::pin(async move {
            d.calls.lock().unwrap().push(Call::Delete { channel: channel_id.to_string(), message: message_id.to_string() });
            Ok(())
        })
    }

    fn send(&self, _app: &App, channel_id: ChannelId, content: String, roles: Vec<String>) -> BoxFuture<Result<(), RestError>> {
        let d = self.0.clone();
        Box::pin(async move {
            d.calls.lock().unwrap().push(Call::Send { channel: channel_id.to_string(), content, roles });
            match d.take_failure("send") {
                Some(e) => Err(e),
                None => Ok(()),
            }
        })
    }

    fn fetch_all_members(&self, _app: &App, guild_id: GuildId, _timeout_ms: u64, _max_members: u64) -> BoxFuture<Result<(), String>> {
        let d = self.0.clone();
        Box::pin(async move {
            d.calls.lock().unwrap().push(Call::FetchAll { guild: guild_id.to_string() });
            let remote: Vec<(UserId, Member)> = d.remote_members.lock().unwrap().drain().collect();
            if let Some(g) = d.guilds.lock().unwrap().get_mut(&guild_id) {
                for (id, m) in remote {
                    g.members.insert(id, m);
                }
            }
            Ok(())
        })
    }
}

/// A member as GUILD_CREATE sends it.
pub fn member_json(id: &str, roles: &[&str], nick: Option<&str>, bot: bool) -> Value {
    json!({
        "user": { "id": id, "username": format!("user{id}"), "discriminator": "0", "avatar": null, "bot": bot, "global_name": null },
        "roles": roles, "nick": nick, "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0
    })
}

/// A member the cache built from a presence update (serenity, like discord.js, adds one with no roles and no
/// `joined_at`: a partial member).
pub fn partial_member(guild: &str, id: &str) -> Member {
    let mut m = member(guild, id, &[], None);
    m.joined_at = None;
    m
}

/// A serenity `Member` of a guild.
pub fn member(guild: &str, id: &str, roles: &[&str], nick: Option<&str>) -> Member {
    let mut v = member_json(id, roles, nick, false);
    v["guild_id"] = json!(guild);
    serde_json::from_value(v).expect("member")
}

pub mod search_flows;
