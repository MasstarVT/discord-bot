//! Shared test support: a fake Discord REST implementation that applies the matching gateway events to the offline
//! serenity cache (as Discord would), and a guild builder with voice channels, categories and voice states.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bot_core::App;
use feat_tempvoice::discord_port::{NewChannel, Overwrite, RestError, RestResult, VoiceRest};
use serde_json::{Value, json};
use serenity::all::{ChannelId, GuildChannel, GuildId, Member, MessageId, Permissions, RoleId, UserId};
use serenity::builder::{CreateMessage, EditMessage};

pub use bot_core::testing::TEST_BOT_ID;

/// Ids handed out by fakes: one block per fake, so parallel tests never share a channel id.
static NEXT_BASE: AtomicU64 = AtomicU64::new(1_300_000_000_000_000_000);

/// One recorded REST call.
#[derive(Debug, Clone)]
pub struct Call {
    pub op: String,
    pub data: Value,
}

/// The fake REST: records every call, fails the ops told to, and updates the serenity cache.
pub struct FakeRest {
    app: App,
    pub calls: Mutex<Vec<Call>>,
    /// op -> Discord error code (every call of that op fails).
    pub fail: Mutex<HashMap<String, u64>>,
    /// Role calls update the member's cached roles (the gateway echo); off: the cache lags.
    pub role_echo: std::sync::atomic::AtomicBool,
    /// Created channels reach the cache from the REST call (as the gateway echo would); off: the echo lags.
    pub channel_echo: std::sync::atomic::AtomicBool,
    /// Milliseconds a set_name call takes (the rate-limit wait).
    pub name_delay_ms: AtomicU64,
    next_id: AtomicU64,
    messages: Mutex<Vec<(ChannelId, MessageId)>>,
}

impl FakeRest {
    pub fn install(app: &App) -> Arc<FakeRest> {
        let f = Arc::new(FakeRest {
            app: app.clone(),
            calls: Mutex::default(),
            fail: Mutex::default(),
            role_echo: std::sync::atomic::AtomicBool::new(true),
            channel_echo: std::sync::atomic::AtomicBool::new(true),
            name_delay_ms: AtomicU64::new(0),
            next_id: AtomicU64::new(NEXT_BASE.fetch_add(1_000_000, Ordering::Relaxed)),
            messages: Mutex::default(),
        });
        feat_tempvoice::discord_port::set_rest(app, f.clone());
        f
    }

    pub fn fail_op(&self, op: &str, code: u64) {
        self.fail.lock().unwrap().insert(op.to_string(), code);
    }

    pub fn clear_fail(&self, op: &str) {
        self.fail.lock().unwrap().remove(op);
    }

    pub fn ops(&self) -> Vec<String> {
        self.calls.lock().unwrap().iter().map(|c| c.op.clone()).collect()
    }

    pub fn calls_of(&self, op: &str) -> Vec<Value> {
        self.calls.lock().unwrap().iter().filter(|c| c.op == op).map(|c| c.data.clone()).collect()
    }

    pub fn clear(&self) {
        self.calls.lock().unwrap().clear();
    }

    fn record(&self, op: &str, data: Value) -> RestResult<()> {
        self.calls.lock().unwrap().push(Call { op: op.to_string(), data });
        match self.fail.lock().unwrap().get(op) {
            Some(code) => Err(RestError::new(Some(*code), format!("fake {op} failed with {code}"))),
            None => Ok(()),
        }
    }

    fn new_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn cache(&self) -> Arc<serenity::all::Cache> {
        self.app.discord().expect("offline discord installed").cache.clone()
    }

    fn cached_channel(&self, guild: GuildId, channel: ChannelId) -> Option<GuildChannel> {
        self.cache().guild(guild).and_then(|g| g.channels.get(&channel).cloned())
    }

    fn update_channel(&self, guild: GuildId, channel: ChannelId, edit: impl FnOnce(&mut Value)) -> RestResult<()> {
        let Some(ch) = self.cached_channel(guild, channel) else { return Err(RestError::new(Some(10003), "Unknown Channel")) };
        let mut j = serde_json::to_value(&ch).unwrap();
        edit(&mut j);
        let mut ev: serenity::all::ChannelUpdateEvent = serde_json::from_value(j).unwrap();
        self.cache().update(&mut ev);
        Ok(())
    }

    /// Moves `user` in the cache (as the gateway's VOICE_STATE_UPDATE would).
    pub fn set_voice(&self, guild: GuildId, user: UserId, channel: Option<ChannelId>) {
        set_voice(&self.cache(), guild, user, channel);
    }
}

fn ow_json(o: &Overwrite) -> Value {
    json!({ "id": o.id, "type": o.kind, "allow": o.allow.to_string(), "deny": o.deny.to_string() })
}

/// Applies a voice state change to the cache.
pub fn set_voice(cache: &serenity::all::Cache, guild: GuildId, user: UserId, channel: Option<ChannelId>) {
    let member = cache.guild(guild).and_then(|g| g.members.get(&user).cloned());
    let mut j = json!({
        "guild_id": guild.to_string(), "channel_id": channel.map(|c| c.to_string()), "user_id": user.to_string(), "session_id": "s",
        "deaf": false, "mute": false, "self_deaf": false, "self_mute": false, "self_video": false, "suppress": false,
        "request_to_speak_timestamp": null,
    });
    if let Some(m) = member {
        j["member"] = serde_json::to_value(&m).unwrap();
    }
    let mut ev: serenity::all::VoiceStateUpdateEvent = serde_json::from_value(j).unwrap();
    cache.update(&mut ev);
}

/// Sets a member's cached roles (as GUILD_MEMBER_UPDATE would).
pub fn set_roles(cache: &serenity::all::Cache, guild: GuildId, user: UserId, roles: &[RoleId]) {
    let Some(m) = cache.guild(guild).and_then(|g| g.members.get(&user).cloned()) else { return };
    let j = json!({
        "guild_id": guild.to_string(), "nick": m.nick, "joined_at": "2025-01-01T00:00:00.000000+00:00",
        "roles": roles.iter().map(|r| r.to_string()).collect::<Vec<_>>(), "user": m.user, "premium_since": null,
        "deaf": false, "mute": false, "flags": 0,
    });
    let mut ev: serenity::all::GuildMemberUpdateEvent = serde_json::from_value(j).unwrap();
    cache.update(&mut ev);
}

#[async_trait]
impl VoiceRest for FakeRest {
    async fn create_channel(&self, guild: GuildId, spec: NewChannel) -> RestResult<GuildChannel> {
        let data = json!({
            "name": spec.name, "type": u8::from(spec.kind), "parent": spec.parent.map(|p| p.to_string()), "userLimit": spec.user_limit,
            "bitrate": spec.bitrate, "topic": spec.topic, "overwrites": spec.overwrites.iter().map(ow_json).collect::<Vec<_>>(), "reason": spec.reason,
        });
        self.record("create_channel", data)?;
        let id = self.new_id();
        let mut j = json!({
            "id": id.to_string(), "type": u8::from(spec.kind), "guild_id": guild.to_string(), "name": spec.name, "position": 0,
            "parent_id": spec.parent.map(|p| p.to_string()), "permission_overwrites": spec.overwrites.iter().map(ow_json).collect::<Vec<_>>(),
            "nsfw": false, "rate_limit_per_user": 0, "last_message_id": null, "topic": spec.topic,
        });
        if spec.kind == serenity::all::ChannelType::Voice {
            j["user_limit"] = json!(spec.user_limit.unwrap_or(0));
            j["bitrate"] = json!(spec.bitrate.unwrap_or(64000));
        }
        let channel: GuildChannel = serde_json::from_value(j.clone()).unwrap();
        if self.channel_echo.load(Ordering::Relaxed) {
            let mut ev: serenity::all::ChannelCreateEvent = serde_json::from_value(j).unwrap();
            self.cache().update(&mut ev);
        }
        Ok(channel)
    }

    async fn delete_channel(&self, guild: GuildId, channel: ChannelId, reason: &str) -> RestResult<()> {
        self.record("delete_channel", json!({ "channel": channel.to_string(), "reason": reason }))?;
        let Some(ch) = self.cached_channel(guild, channel) else { return Err(RestError::new(Some(10003), "Unknown Channel")) };
        let mut ev: serenity::all::ChannelDeleteEvent = serde_json::from_value(serde_json::to_value(&ch).unwrap()).unwrap();
        self.cache().update(&mut ev);
        Ok(())
    }

    async fn fetch_channel(&self, guild: GuildId, channel: ChannelId) -> RestResult<GuildChannel> {
        self.record("fetch_channel", json!({ "channel": channel.to_string() }))?;
        self.cached_channel(guild, channel).ok_or_else(|| RestError::new(Some(10003), "Unknown Channel"))
    }

    async fn set_overwrites(&self, guild: GuildId, channel: ChannelId, overwrites: Vec<Overwrite>, reason: Option<&str>) -> RestResult<()> {
        let list: Vec<Value> = overwrites.iter().map(ow_json).collect();
        self.record("set_overwrites", json!({ "channel": channel.to_string(), "overwrites": list, "reason": reason }))?;
        // Serenity truncates unknown permission bits when it reads an overwrite.
        let kept: Vec<Value> = overwrites
            .iter()
            .map(|o| json!({ "id": o.id, "type": o.kind, "allow": Permissions::from_bits_truncate(o.allow).bits().to_string(), "deny": Permissions::from_bits_truncate(o.deny).bits().to_string() }))
            .collect();
        self.update_channel(guild, channel, |j| j["permission_overwrites"] = Value::Array(kept))
    }

    async fn set_name(&self, guild: GuildId, channel: ChannelId, name: &str, reason: &str) -> RestResult<()> {
        let delay = self.name_delay_ms.load(Ordering::Relaxed);
        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
        self.record("set_name", json!({ "channel": channel.to_string(), "name": name, "reason": reason }))?;
        let n = name.to_string();
        self.update_channel(guild, channel, |j| j["name"] = json!(n))
    }

    async fn set_user_limit(&self, guild: GuildId, channel: ChannelId, limit: u32, reason: &str) -> RestResult<()> {
        self.record("set_user_limit", json!({ "channel": channel.to_string(), "limit": limit, "reason": reason }))?;
        self.update_channel(guild, channel, |j| j["user_limit"] = json!(limit))
    }

    async fn move_member(&self, guild: GuildId, user: UserId, channel: Option<ChannelId>, reason: &str) -> RestResult<()> {
        self.record("move_member", json!({ "user": user.to_string(), "channel": channel.map(|c| c.to_string()), "reason": reason }))?;
        self.set_voice(guild, user, channel);
        Ok(())
    }

    async fn fetch_member(&self, guild: GuildId, user: UserId) -> RestResult<Member> {
        self.record("fetch_member", json!({ "user": user.to_string() }))?;
        self.cache().guild(guild).and_then(|g| g.members.get(&user).cloned()).ok_or_else(|| RestError::new(Some(10007), "Unknown Member"))
    }

    async fn send_message(&self, _guild: GuildId, channel: ChannelId, message: CreateMessage) -> RestResult<MessageId> {
        self.record("send_message", json!({ "channel": channel.to_string(), "message": serde_json::to_value(&message).unwrap() }))?;
        let id = MessageId::new(self.new_id());
        self.messages.lock().unwrap().push((channel, id));
        Ok(id)
    }

    async fn pin_message(&self, _guild: GuildId, channel: ChannelId, message: MessageId, reason: &str) -> RestResult<()> {
        self.record("pin_message", json!({ "channel": channel.to_string(), "message": message.to_string(), "reason": reason }))
    }

    async fn fetch_message(&self, _guild: GuildId, channel: ChannelId, message: MessageId) -> RestResult<()> {
        self.record("fetch_message", json!({ "channel": channel.to_string(), "message": message.to_string() }))?;
        if self.messages.lock().unwrap().contains(&(channel, message)) { Ok(()) } else { Err(RestError::new(Some(10008), "Unknown Message")) }
    }

    async fn edit_message(&self, _guild: GuildId, channel: ChannelId, message: MessageId, edit: EditMessage) -> RestResult<()> {
        self.record("edit_message", json!({ "channel": channel.to_string(), "message": message.to_string(), "edit": serde_json::to_value(&edit).unwrap() }))
    }

    async fn add_role(&self, guild: GuildId, user: UserId, role: RoleId, reason: &str) -> RestResult<()> {
        self.record("add_role", json!({ "user": user.to_string(), "role": role.to_string(), "reason": reason }))?;
        if self.role_echo.load(Ordering::Relaxed) {
            let cache = self.cache();
            let mut roles = cache.guild(guild).and_then(|g| g.members.get(&user).map(|m| m.roles.clone())).unwrap_or_default();
            if !roles.contains(&role) {
                roles.push(role);
            }
            set_roles(&cache, guild, user, &roles);
        }
        Ok(())
    }

    async fn remove_role(&self, guild: GuildId, user: UserId, role: RoleId, reason: &str) -> RestResult<()> {
        self.record("remove_role", json!({ "user": user.to_string(), "role": role.to_string(), "reason": reason }))?;
        if self.role_echo.load(Ordering::Relaxed) {
            let cache = self.cache();
            let roles: Vec<RoleId> =
                cache.guild(guild).and_then(|g| g.members.get(&user).map(|m| m.roles.clone())).unwrap_or_default().into_iter().filter(|r| *r != role).collect();
            set_roles(&cache, guild, user, &roles);
        }
        Ok(())
    }
}

// ── Guilds ─────────────────────────────────────────────────────────────────────

/// A guild built piece by piece (on top of `FakeGuild`'s roles and members) with channels that have parents, voice
/// channels with limits, voice states and an AFK channel.
#[derive(Clone)]
pub struct TestGuild {
    pub base: bot_core::testing::FakeGuild,
    pub id: String,
    channels: Vec<Value>,
    voice: Vec<(String, String)>,
    afk: Option<String>,
    bots: Vec<String>,
}

impl TestGuild {
    /// A guild whose bot member has a role with `bot_perms`, and `owner` owns it.
    pub fn new(id: &str, owner: &str, bot_perms: Permissions) -> Self {
        let base = bot_core::testing::FakeGuild::new(id, owner)
            .role("810000000000000001", 50, bot_perms)
            .member(TEST_BOT_ID, &["810000000000000001"])
            .member(owner, &[]);
        Self { base, id: id.into(), channels: Vec::new(), voice: Vec::new(), afk: None, bots: Vec::new() }
    }

    pub fn role(mut self, id: &str, position: u16, perms: Permissions) -> Self {
        self.base = self.base.role(id, position, perms);
        self
    }

    pub fn member(mut self, id: &str, roles: &[&str]) -> Self {
        self.base = self.base.member(id, roles);
        self
    }

    /// A bot member (not the test bot).
    pub fn bot_member(mut self, id: &str) -> Self {
        self.base = self.base.member(id, &[]);
        self.bots.push(id.into());
        self
    }

    /// A channel of `kind` (0 text, 2 voice, 4 category) under `parent`, with overwrites `(id, is_member, allow, deny)`.
    pub fn channel(mut self, id: &str, kind: u8, name: &str, parent: Option<&str>, overwrites: &[(&str, bool, u64, u64)]) -> Self {
        let ows: Vec<Value> =
            overwrites.iter().map(|(oid, m, a, d)| json!({ "id": oid, "type": u8::from(*m), "allow": a.to_string(), "deny": d.to_string() })).collect();
        let mut j = json!({
            "id": id, "type": kind, "name": name, "position": self.channels.len(), "permission_overwrites": ows, "nsfw": false,
            "parent_id": parent, "topic": null, "last_message_id": null, "rate_limit_per_user": 0,
        });
        if kind == 2 {
            j["user_limit"] = json!(0);
            j["bitrate"] = json!(64000);
        }
        self.channels.push(j);
        self
    }

    /// `user` is connected to `channel`.
    pub fn voice(mut self, user: &str, channel: &str) -> Self {
        self.voice.push((user.into(), channel.into()));
        self
    }

    pub fn afk(mut self, channel: &str) -> Self {
        self.afk = Some(channel.into());
        self
    }

    pub fn json(&self) -> Value {
        let mut g = self.base.json();
        let mut channels = g["channels"].as_array().cloned().unwrap_or_default();
        channels.extend(self.channels.iter().cloned());
        g["channels"] = Value::Array(channels);
        if let Some(members) = g["members"].as_array_mut() {
            for m in members {
                let id = m["user"]["id"].as_str().unwrap_or_default().to_string();
                if self.bots.contains(&id) {
                    m["user"]["bot"] = json!(true);
                }
            }
        }
        g["voice_states"] = Value::Array(
            self.voice
                .iter()
                .map(|(u, c)| {
                    json!({
                        "channel_id": c, "user_id": u, "session_id": "s", "deaf": false, "mute": false, "self_deaf": false,
                        "self_mute": false, "self_video": false, "suppress": false, "request_to_speak_timestamp": null,
                    })
                })
                .collect(),
        );
        if let Some(a) = &self.afk {
            g["afk_channel_id"] = json!(a);
        }
        g
    }
}

/// Installs the guilds on the app's offline Discord connection (bot user known) and marks them available.
pub fn install_guilds(app: &App, guilds: &[TestGuild]) {
    let json: Vec<Value> = guilds.iter().map(TestGuild::json).collect();
    app.set_discord(bot_core::testing::offline_discord_as(&json));
    app.set_guild_directory(Arc::new(bot_core::discord::StaticGuilds::available(guilds.iter().map(|g| g.id.clone()))));
}

pub fn gid(s: &str) -> GuildId {
    GuildId::new(s.parse().unwrap())
}

pub fn cid(s: &str) -> ChannelId {
    ChannelId::new(s.parse().unwrap())
}

pub fn uid(s: &str) -> UserId {
    UserId::new(s.parse().unwrap())
}

/// The cached member.
pub fn member(app: &App, guild: &str, user: &str) -> Member {
    app.discord().unwrap().cache.guild(gid(guild)).and_then(|g| g.members.get(&uid(user)).cloned()).expect("member cached")
}

/// The cached channel.
pub fn channel(app: &App, guild: &str, id: &str) -> Option<GuildChannel> {
    app.discord().unwrap().cache.guild(gid(guild)).and_then(|g| g.channels.get(&cid(id)).cloned())
}

/// The cached voice channel of a user.
pub fn voice_of(app: &App, guild: &str, user: &str) -> Option<String> {
    app.discord().unwrap().cache.guild(gid(guild)).and_then(|g| g.voice_states.get(&uid(user)).and_then(|v| v.channel_id)).map(|c| c.to_string())
}

/// The embed JSON without its timestamp.
pub fn embed_json(e: &serenity::builder::CreateEmbed) -> Value {
    let mut j = serde_json::to_value(e).unwrap();
    if let Some(o) = j.as_object_mut() {
        o.remove("timestamp");
        o.remove("type");
    }
    j
}

/// Discord's 'all permissions' as raw bits for the bot role.
pub fn admin() -> Permissions {
    Permissions::ADMINISTRATOR
}

/// The bot permissions temp voice needs, without Administrator.
pub fn voice_perms() -> Permissions {
    Permissions::MANAGE_CHANNELS
        | Permissions::MANAGE_ROLES
        | Permissions::MOVE_MEMBERS
        | Permissions::VIEW_CHANNEL
        | Permissions::CONNECT
        | Permissions::SEND_MESSAGES
        | Permissions::EMBED_LINKS
        | Permissions::READ_MESSAGE_HISTORY
        | Permissions::MANAGE_MESSAGES
        | Permissions::PRIORITY_SPEAKER
}

/// A voice state as the gateway sends it.
pub fn voice_state(guild: GuildId, user: UserId, channel: Option<ChannelId>) -> serenity::all::VoiceState {
    serde_json::from_value(json!({
        "guild_id": guild.to_string(), "channel_id": channel.map(|c| c.to_string()), "user_id": user.to_string(), "session_id": "s",
        "deaf": false, "mute": false, "self_deaf": false, "self_mute": false, "self_video": false, "suppress": false,
        "request_to_speak_timestamp": null,
    }))
    .unwrap()
}
