//! What the statistics-channel engine reads from the Discord cache (`guild.memberCount`, `guild.members.cache`,
//! `guild.presences.cache`, `guild.channels.cache`, `guild.roles.cache`, `missingBotPermissions`) and asks of Discord
//! (`guild.channels.fetch`, `channel.setName`, `guild.channels.create`, `channel.delete`, `guild.fetch()`,
//! `fetchAllMembers`).
//!
//! [`SerenityStatsIo`] is the production implementation. Tests install a fake with [`set_stats_io`].

use std::sync::{Arc, RwLock};

use bot_core::App;
use bot_core::guild_members::FetchFailure;
use bot_core::interaction_helpers::{PermTarget, cached_guild, missing_bot_permissions};
use bot_util::placeholder_parser::PlaceholderGuild;
use serde_json::{Value, json};
use serenity::all::{ChannelId, Guild, GuildChannel, GuildId};

use crate::discord_io::{DiscordError, parse_id};

/// A channel as the counters read it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StatsChannel {
    pub id: String,
    pub name: String,
    /// Discord's numeric channel type.
    pub kind: u8,
    /// `channel.isThread()`.
    pub is_thread: bool,
    /// The bot's permission bits in a channel fetched over REST while missing from the cache (`permissionsFor` on
    /// the fetched channel); `None` for a cached channel, whose permissions come from the cache.
    pub bot_permissions: Option<u64>,
}

/// A cached member: the bot flag and the role ids.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StatsMember {
    pub bot: bool,
    pub roles: Vec<String>,
}

/// The cached guild as the counters read it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StatsGuild {
    pub id: String,
    pub name: String,
    /// `guild.available`.
    pub available: bool,
    /// `guild.memberCount`.
    pub member_count: u64,
    /// `guild.members.cache`.
    pub members: Vec<StatsMember>,
    /// The `status` of every cached presence.
    pub presences: Vec<String>,
    /// `guild.channels.cache` (threads included).
    pub channels: Vec<StatsChannel>,
    /// `guild.roles.cache`: `(id, name)`, `@everyone` included.
    pub roles: Vec<(String, String)>,
    /// `guild.premiumSubscriptionCount`.
    pub boosts: u64,
    /// `guild.members.me` is cached.
    pub has_me: bool,
    /// The guild as the placeholders read it (`{server}` and friends).
    pub placeholder: Option<PlaceholderGuild>,
}

impl StatsGuild {
    /// `guild.channels.cache.get(id)`.
    pub fn channel(&self, id: &str) -> Option<&StatsChannel> {
        self.channels.iter().find(|c| c.id == id)
    }

    /// `guild.roles.cache.get(id)?.name`.
    pub fn role_name(&self, id: &str) -> Option<&str> {
        self.roles.iter().find(|(r, _)| r == id).map(|(_, n)| n.as_str())
    }

    /// `guild.roles.cache.has(id)`.
    pub fn has_role(&self, id: &str) -> bool {
        self.roles.iter().any(|(r, _)| r == id)
    }
}

/// A permission overwrite of a new counter channel (`{ id, type, allow, deny }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overwrite {
    pub id: String,
    /// `OverwriteType`: 0 role, 1 member.
    pub kind: u8,
    pub allow: u64,
    pub deny: u64,
}

impl Overwrite {
    /// The API JSON (`{ id, type, allow, deny }` with the bitfields as strings, as discord.js sends them).
    pub fn to_json(&self) -> Value {
        json!({ "id": self.id, "type": self.kind, "allow": self.allow.to_string(), "deny": self.deny.to_string() })
    }
}

/// `guild.channels.create({ name, type: GuildVoice, parent, permissionOverwrites, reason })`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateVoiceChannel {
    pub name: String,
    pub parent: Option<String>,
    pub overwrites: Vec<Overwrite>,
    pub reason: String,
}

impl CreateVoiceChannel {
    /// The API JSON body.
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "type": 2,
            "parent_id": self.parent,
            "permission_overwrites": self.overwrites.iter().map(Overwrite::to_json).collect::<Vec<_>>(),
        })
    }
}

/// The cache reads and REST calls of the counters.
#[async_trait::async_trait]
pub trait StatsIo: Send + Sync + 'static {
    /// The cached guild, `None` when it is not in this shard's cache.
    fn guild(&self, app: &App, guild_id: &str) -> Option<StatsGuild>;
    /// `client.user.id`.
    fn bot_user_id(&self, app: &App) -> Option<String>;
    /// `missingBotPermissions(channel ?? guild, perms)`.
    fn missing_bot_permissions(&self, app: &App, guild_id: &str, channel_id: Option<&str>, perms: &[&str]) -> Vec<String>;

    /// `guild.channels.fetch(id)`: `Ok(None)` when Discord answered without a channel.
    async fn fetch_channel(&self, app: &App, guild_id: &str, channel_id: &str) -> Result<Option<StatsChannel>, DiscordError>;
    /// `channel.setName(name, reason)`: the channel's name afterwards.
    async fn rename_channel(&self, app: &App, guild_id: &str, channel_id: &str, name: &str, reason: &str) -> Result<Option<String>, DiscordError>;
    /// `guild.channels.create(...)`.
    async fn create_channel(&self, app: &App, guild_id: &str, req: &CreateVoiceChannel) -> Result<StatsChannel, DiscordError>;
    /// `channel.delete(reason)` / `guild.channels.delete(id, reason)`.
    async fn delete_channel(&self, app: &App, guild_id: &str, channel_id: &str, reason: &str) -> Result<(), DiscordError>;
    /// `(await guild.fetch()).approximatePresenceCount`.
    async fn approximate_presence_count(&self, app: &App, guild_id: &str) -> Result<Option<u64>, DiscordError>;
    /// `fetchAllMembers(guild, { timeoutMs, maxMembers })`: on success the member cache holds every member.
    async fn fetch_all_members(&self, app: &App, guild_id: &str, timeout_ms: u64, max_members: u64) -> Result<(), FetchFailure>;
}

#[derive(Default)]
struct IoSlot(RwLock<Option<Arc<dyn StatsIo>>>);

/// Replaces the counters' Discord layer of this app (tests).
pub fn set_stats_io(app: &App, io: Arc<dyn StatsIo>) {
    *app.ext::<IoSlot>().0.write().unwrap_or_else(|e| e.into_inner()) = Some(io);
}

/// The counters' Discord layer: the override, else serenity.
pub fn stats_io(app: &App) -> Arc<dyn StatsIo> {
    app.ext::<IoSlot>().0.read().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_else(|| Arc::new(SerenityStatsIo))
}

/// The production layer: the app's serenity cache and HTTP client.
#[derive(Debug, Clone, Copy, Default)]
pub struct SerenityStatsIo;

fn offline() -> DiscordError {
    DiscordError::other("Discord is not connected")
}

fn channel_of(c: &GuildChannel) -> StatsChannel {
    StatsChannel { id: c.id.to_string(), name: c.name.clone(), kind: u8::from(c.kind), is_thread: bot_core::permissions::is_thread(c), bot_permissions: None }
}

/// The bot's permissions in a channel that is not in the cache, from the cached guild (`channel.permissionsFor(me)`).
pub fn fetched_bot_permissions(app: &App, channel: &GuildChannel) -> Option<u64> {
    let d = app.discord()?;
    let bot = d.cache.current_user().id;
    let guild = cached_guild(app, channel.guild_id)?;
    let me = bot_core::permissions::me(&guild, bot)?;
    bot_core::permissions::member_permissions_in(&guild, channel, bot, &me.roles, true).map(|p| p.bits())
}

/// A serenity guild as [`StatsGuild`].
pub fn stats_guild_of(app: &App, guild: &Guild) -> StatsGuild {
    let bot = app.discord().map(|d| d.bot_user_id());
    let mut channels: Vec<StatsChannel> = guild.channels.values().map(channel_of).collect();
    channels.extend(guild.threads.iter().map(channel_of));
    StatsGuild {
        id: guild.id.to_string(),
        name: guild.name.clone(),
        available: app.guild_directory().guild_state(&guild.id.to_string()) == bot_core::discord::GuildState::Available,
        member_count: guild.member_count,
        members: guild.members.values().map(|m| StatsMember { bot: m.user.bot, roles: m.roles.iter().map(|r| r.to_string()).collect() }).collect(),
        presences: guild.presences.values().map(|p| p.status.name().to_string()).collect(),
        channels,
        roles: guild.roles.values().map(|r| (r.id.to_string(), r.name.clone())).collect(),
        boosts: guild.premium_subscription_count.unwrap_or(0),
        has_me: bot.is_some_and(|b| guild.members.contains_key(&b)),
        placeholder: Some(bot_core::adapters::placeholder_guild(guild)),
    }
}

fn gid(s: &str) -> Option<GuildId> {
    parse_id(s).map(GuildId::new)
}

fn cid(s: &str) -> Result<ChannelId, DiscordError> {
    parse_id(s).map(ChannelId::new).ok_or_else(|| DiscordError::code(10003))
}

#[async_trait::async_trait]
impl StatsIo for SerenityStatsIo {
    fn guild(&self, app: &App, guild_id: &str) -> Option<StatsGuild> {
        let guild = cached_guild(app, gid(guild_id)?)?;
        Some(stats_guild_of(app, &guild))
    }

    fn bot_user_id(&self, app: &App) -> Option<String> {
        app.discord().map(|d| d.bot_user_id().to_string())
    }

    fn missing_bot_permissions(&self, app: &App, guild_id: &str, channel_id: Option<&str>, perms: &[&str]) -> Vec<String> {
        let Some(g) = gid(guild_id) else { return perms.iter().map(|p| p.to_string()).collect() };
        let target = match channel_id.and_then(parse_id) {
            Some(c) => PermTarget::Channel(g, ChannelId::new(c)),
            None => PermTarget::Guild(g),
        };
        missing_bot_permissions(app, target, perms)
    }

    async fn fetch_channel(&self, app: &App, _guild_id: &str, channel_id: &str) -> Result<Option<StatsChannel>, DiscordError> {
        let http = app.http().ok_or_else(offline)?;
        let channel = http.get_channel(cid(channel_id)?).await?;
        Ok(channel.guild().map(|c| StatsChannel { bot_permissions: fetched_bot_permissions(app, &c), ..channel_of(&c) }))
    }

    async fn rename_channel(&self, app: &App, _guild_id: &str, channel_id: &str, name: &str, reason: &str) -> Result<Option<String>, DiscordError> {
        let http = app.http().ok_or_else(offline)?;
        let updated = http.edit_channel(cid(channel_id)?, &json!({ "name": name }), Some(reason)).await?;
        Ok(Some(updated.name))
    }

    async fn create_channel(&self, app: &App, guild_id: &str, req: &CreateVoiceChannel) -> Result<StatsChannel, DiscordError> {
        let http = app.http().ok_or_else(offline)?;
        let g = gid(guild_id).ok_or_else(|| DiscordError::code(10004))?;
        let created = http.create_channel(g, &req.to_json(), Some(&req.reason)).await?;
        // `guild.channels.create` caches what it created: the new counter is known before its CHANNEL_CREATE arrives.
        bot_core::discord::cache_created_channel(app, &created);
        Ok(channel_of(&created))
    }

    async fn delete_channel(&self, app: &App, _guild_id: &str, channel_id: &str, reason: &str) -> Result<(), DiscordError> {
        let http = app.http().ok_or_else(offline)?;
        http.delete_channel(cid(channel_id)?, Some(reason)).await?;
        Ok(())
    }

    async fn approximate_presence_count(&self, app: &App, guild_id: &str) -> Result<Option<u64>, DiscordError> {
        let http = app.http().ok_or_else(offline)?;
        let g = gid(guild_id).ok_or_else(|| DiscordError::code(10004))?;
        Ok(http.get_guild_with_counts(g).await?.approximate_presence_count)
    }

    async fn fetch_all_members(&self, app: &App, guild_id: &str, timeout_ms: u64, max_members: u64) -> Result<(), FetchFailure> {
        let g = gid(guild_id).ok_or(FetchFailure::Error)?;
        bot_core::guild_members::fetch_all_members(app, g, timeout_ms, max_members).await.map(|_| ())
    }
}
