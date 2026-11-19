//! What the starboard engine reads from the Discord cache and asks of Discord's REST API (`guild.channels.cache`,
//! `client.emojis.cache`, `checkChannelUsable`, `copyLeaksPrivate`, `channel.messages.fetch`, `reaction.users.fetch`,
//! `channel.send`, `messages.edit`, `messages.delete`, `message.react`, `reaction.users.remove`).
//!
//! [`SerenityStarboardIo`] is the production implementation (the serenity cache and HTTP client of the app). Tests
//! install a fake with [`set_starboard_io`], as the JS tests pass fake discord.js objects.

use std::sync::{Arc, RwLock};

use bot_core::App;
use bot_core::interaction_helpers::{ChannelUsable, PermTarget, cached_guild, channel_usable_verdict, missing_bot_permissions};
use bot_core::permissions;
use serde_json::Value;
use serenity::all::{ChannelId, Guild, GuildChannel, GuildId, Member, Message, MessageId, ReactionType, User, UserId};

use crate::discord_io::{DiscordError, parse_id};

/// A guild channel or thread as the starboard reads it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SbChannel {
    pub id: String,
    /// `channel.guildId` (a REST fetch may return a channel of another guild).
    pub guild_id: Option<String>,
    pub name: String,
    /// Discord's numeric channel type.
    pub kind: u8,
    pub parent_id: Option<String>,
    pub nsfw: bool,
    /// `channel.isThread()`.
    pub is_thread: bool,
    /// A thread's `parent.nsfw`.
    pub parent_nsfw: bool,
    /// A thread's `parent.parentId` (the category of the thread's channel).
    pub parent_parent_id: Option<String>,
    /// The bot's permission bits in a channel fetched over REST while missing from the cache (`permissionsFor` on
    /// the fetched channel); `None` for a cached channel, whose permissions come from the cache.
    pub bot_permissions: Option<u64>,
}

/// A custom emoji of the guild (`guild.emojis.cache`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SbEmoji {
    pub id: String,
    pub name: String,
    pub animated: bool,
}

impl SbEmoji {
    /// `emoji.toString()`.
    pub fn mention(&self) -> String {
        format!("<{}:{}:{}>", if self.animated { "a" } else { "" }, self.name, self.id)
    }
}

/// The cached guild as the starboard reads it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SbGuild {
    pub id: String,
    /// `guild.available`.
    pub available: bool,
    pub channels: Vec<SbChannel>,
    pub emojis: Vec<SbEmoji>,
}

impl SbGuild {
    /// `guild.channels.cache.get(id)`.
    pub fn channel(&self, id: &str) -> Option<&SbChannel> {
        self.channels.iter().find(|c| c.id == id)
    }
}

/// A user of a reaction (`reaction.users.fetch` entries).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactUser {
    pub id: String,
    pub bot: bool,
}

/// The cache reads and REST calls of the starboard engine.
#[async_trait::async_trait]
pub trait StarboardIo: Send + Sync + 'static {
    /// `client.user.id`.
    fn bot_user_id(&self, app: &App) -> Option<String>;
    /// The cached guild.
    fn guild(&self, app: &App, guild_id: &str) -> Option<SbGuild>;
    /// `copyLeaksPrivate(source, target)` over the cached guild (`target` may be uncached).
    fn leaks_private(&self, app: &App, guild_id: &str, source_id: &str, target_id: &str) -> bool;
    /// `checkChannelUsable(channel, { needs })` for the bot.
    fn channel_usable(&self, app: &App, guild_id: &str, channel_id: &str, needs: &[&str]) -> ChannelUsable;
    /// `missingBotPermissions(guild, perms)` (server-wide).
    fn guild_missing(&self, app: &App, guild_id: &str, perms: &[&str]) -> Vec<String>;
    /// `client.emojis.cache.get(id)` (every cached guild's custom emojis).
    fn custom_emoji(&self, app: &App, emoji_id: &str) -> Option<SbEmoji>;
    /// `guild.members.resolve(userId)`: the cached member (discord.js `message.member` reads it, for the nickname and
    /// the server avatar of a repost). `None` by default.
    fn cached_member(&self, _app: &App, _guild_id: &str, _user_id: &str) -> Option<Member> {
        None
    }

    /// `client.channels.fetch(id)`: `Ok(None)` for a channel that is not a guild channel.
    async fn fetch_channel(&self, app: &App, channel_id: &str) -> Result<Option<SbChannel>, DiscordError>;
    /// `channel.messages.fetch(id)` with its live reactions.
    async fn fetch_message(&self, app: &App, channel_id: &str, message_id: &str) -> Result<Message, DiscordError>;
    /// One page of `reaction.users.fetch({ type, limit: 100, after })`; `burst` is `ReactionType.Burst`.
    async fn reaction_users(
        &self,
        app: &App,
        channel_id: &str,
        message_id: &str,
        emoji: &ReactionType,
        burst: bool,
        after: Option<&str>,
    ) -> Result<Vec<ReactUser>, DiscordError>;
    /// `channel.send(payload)`; the new message's id.
    async fn send(&self, app: &App, channel_id: &str, payload: &Value) -> Result<String, DiscordError>;
    /// `channel.messages.edit(id, payload)`.
    async fn edit(&self, app: &App, channel_id: &str, message_id: &str, payload: &Value) -> Result<(), DiscordError>;
    /// `channel.messages.delete(id)`.
    async fn delete(&self, app: &App, channel_id: &str, message_id: &str) -> Result<(), DiscordError>;
    /// `message.react(emoji)`.
    async fn react(&self, app: &App, channel_id: &str, message_id: &str, emoji: &ReactionType) -> Result<(), DiscordError>;
    /// `reaction.users.remove(userId)`.
    async fn remove_reaction(&self, app: &App, channel_id: &str, message_id: &str, emoji: &ReactionType, user_id: &str) -> Result<(), DiscordError>;
}

#[derive(Default)]
struct IoSlot(RwLock<Option<Arc<dyn StarboardIo>>>);

/// Replaces the starboard's Discord layer of this app (tests).
pub fn set_starboard_io(app: &App, io: Arc<dyn StarboardIo>) {
    *app.ext::<IoSlot>().0.write().unwrap_or_else(|e| e.into_inner()) = Some(io);
}

/// The starboard's Discord layer: the override, else serenity.
pub fn starboard_io(app: &App) -> Arc<dyn StarboardIo> {
    app.ext::<IoSlot>().0.read().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_else(|| Arc::new(SerenityStarboardIo))
}

/// The production layer: the app's serenity cache and HTTP client. Offline (no connection) every call fails.
#[derive(Debug, Clone, Copy, Default)]
pub struct SerenityStarboardIo;

fn offline() -> DiscordError {
    DiscordError::other("Discord is not connected")
}

fn cid(s: &str) -> Result<ChannelId, DiscordError> {
    parse_id(s).map(ChannelId::new).ok_or_else(|| DiscordError::code(10003))
}

fn mid(s: &str) -> Result<MessageId, DiscordError> {
    parse_id(s).map(MessageId::new).ok_or_else(|| DiscordError::code(10008))
}

/// A serenity channel as [`SbChannel`] (the parent read from the guild when cached).
pub fn sb_channel_of(guild: Option<&Guild>, ch: &GuildChannel) -> SbChannel {
    let is_thread = permissions::is_thread(ch);
    let parent = if is_thread { ch.parent_id.and_then(|p| guild.and_then(|g| g.channels.get(&p))) } else { None };
    SbChannel {
        id: ch.id.to_string(),
        guild_id: Some(ch.guild_id.to_string()),
        name: ch.name.clone(),
        kind: u8::from(ch.kind),
        parent_id: ch.parent_id.map(|p| p.to_string()),
        nsfw: ch.nsfw,
        is_thread,
        parent_nsfw: parent.is_some_and(|p| p.nsfw),
        parent_parent_id: parent.and_then(|p| p.parent_id).map(|p| p.to_string()),
        bot_permissions: None,
    }
}

/// The cached guild as [`SbGuild`].
pub fn sb_guild_of(app: &App, guild: &Guild) -> SbGuild {
    let mut channels: Vec<SbChannel> = guild.channels.values().map(|c| sb_channel_of(Some(guild), c)).collect();
    channels.extend(guild.threads.iter().map(|t| sb_channel_of(Some(guild), t)));
    let available = app.guild_directory().guild_state(&guild.id.to_string()) == bot_core::discord::GuildState::Available;
    SbGuild {
        id: guild.id.to_string(),
        available,
        channels,
        emojis: guild.emojis.values().map(|e| SbEmoji { id: e.id.to_string(), name: e.name.clone(), animated: e.animated }).collect(),
    }
}

fn gid(s: &str) -> Option<GuildId> {
    parse_id(s).map(GuildId::new)
}

#[async_trait::async_trait]
impl StarboardIo for SerenityStarboardIo {
    fn bot_user_id(&self, app: &App) -> Option<String> {
        app.discord().map(|d| d.bot_user_id().to_string())
    }

    fn guild(&self, app: &App, guild_id: &str) -> Option<SbGuild> {
        let guild = cached_guild(app, gid(guild_id)?)?;
        Some(sb_guild_of(app, &guild))
    }

    fn leaks_private(&self, app: &App, guild_id: &str, source_id: &str, target_id: &str) -> bool {
        let Some(guild) = gid(guild_id).and_then(|g| cached_guild(app, g)) else { return true };
        let find = |id: &str| parse_id(id).and_then(|c| permissions::find_channel(&guild, ChannelId::new(c)));
        bot_core::privacy_guards::copy_leaks_private(&guild, find(source_id), find(target_id))
    }

    fn channel_usable(&self, app: &App, guild_id: &str, channel_id: &str, needs: &[&str]) -> ChannelUsable {
        let (Some(g), Some(c)) = (gid(guild_id), parse_id(channel_id).map(ChannelId::new)) else {
            return channel_usable_verdict(None, None, needs);
        };
        bot_core::interaction_helpers::check_channel_usable(app, g, Some(c), needs)
    }

    fn guild_missing(&self, app: &App, guild_id: &str, perms: &[&str]) -> Vec<String> {
        match gid(guild_id) {
            Some(g) => missing_bot_permissions(app, PermTarget::Guild(g), perms),
            None => perms.iter().map(|p| p.to_string()).collect(),
        }
    }

    fn custom_emoji(&self, app: &App, emoji_id: &str) -> Option<SbEmoji> {
        let d = app.discord()?;
        let id = serenity::all::EmojiId::new(parse_id(emoji_id)?);
        for g in d.cache.guilds() {
            if let Some(guild) = d.cache.guild(g)
                && let Some(e) = guild.emojis.get(&id)
            {
                return Some(SbEmoji { id: e.id.to_string(), name: e.name.clone(), animated: e.animated });
            }
        }
        None
    }

    async fn fetch_channel(&self, app: &App, channel_id: &str) -> Result<Option<SbChannel>, DiscordError> {
        let http = app.http().ok_or_else(offline)?;
        let channel = http.get_channel(cid(channel_id)?).await?;
        Ok(channel.guild().map(|c| {
            // discord.js `client.channels.fetch` caches what it fetched (`channels._add`): an uncached thread a reaction
            // came from is then known to the privacy and NSFW checks of this sync and of the next ones.
            bot_core::discord::cache_created_channel(app, &c);
            let guild = cached_guild(app, c.guild_id);
            SbChannel { bot_permissions: crate::stats_io::fetched_bot_permissions(app, &c), ..sb_channel_of(guild.as_ref(), &c) }
        }))
    }

    fn cached_member(&self, app: &App, guild_id: &str, user_id: &str) -> Option<Member> {
        let d = app.discord()?;
        let user = UserId::new(parse_id(user_id)?);
        d.cache.guild(gid(guild_id)?)?.members.get(&user).cloned()
    }

    async fn fetch_message(&self, app: &App, channel_id: &str, message_id: &str) -> Result<Message, DiscordError> {
        // Always over REST: serenity's message cache never applies reaction events (discord.js's does), so a cached
        // message still holds the reactions it had when it was posted, usually none, and no sync would ever count.
        let http = app.http().ok_or_else(offline)?;
        Ok(http.get_message(cid(channel_id)?, mid(message_id)?).await?)
    }

    async fn reaction_users(
        &self,
        app: &App,
        channel_id: &str,
        message_id: &str,
        emoji: &ReactionType,
        burst: bool,
        after: Option<&str>,
    ) -> Result<Vec<ReactUser>, DiscordError> {
        use serenity::http::{LightMethod, Request, Route};
        let http = app.http().ok_or_else(offline)?;
        let data = emoji.as_data();
        let mut params = vec![("limit", "100".to_string())];
        if let Some(a) = after {
            params.push(("after", a.to_string()));
        }
        // ReactionType.Normal = 0, Burst = 1 (super reactions).
        params.push(("type", if burst { "1" } else { "0" }.to_string()));
        let req = Request::new(
            Route::ChannelMessageReactionEmoji { channel_id: cid(channel_id)?, message_id: mid(message_id)?, reaction: &data },
            LightMethod::Get,
        )
        .params(Some(params));
        let users: Vec<User> = http.fire(req).await?;
        Ok(users.into_iter().map(|u| ReactUser { id: u.id.to_string(), bot: u.bot }).collect())
    }

    async fn send(&self, app: &App, channel_id: &str, payload: &Value) -> Result<String, DiscordError> {
        let http = app.http().ok_or_else(offline)?;
        let m = http.send_message(cid(channel_id)?, Vec::new(), payload).await?;
        Ok(m.id.to_string())
    }

    async fn edit(&self, app: &App, channel_id: &str, message_id: &str, payload: &Value) -> Result<(), DiscordError> {
        let http = app.http().ok_or_else(offline)?;
        http.edit_message(cid(channel_id)?, mid(message_id)?, payload, Vec::new()).await?;
        Ok(())
    }

    async fn delete(&self, app: &App, channel_id: &str, message_id: &str) -> Result<(), DiscordError> {
        let http = app.http().ok_or_else(offline)?;
        http.delete_message(cid(channel_id)?, mid(message_id)?, None).await?;
        Ok(())
    }

    async fn react(&self, app: &App, channel_id: &str, message_id: &str, emoji: &ReactionType) -> Result<(), DiscordError> {
        let http = app.http().ok_or_else(offline)?;
        http.create_reaction(cid(channel_id)?, mid(message_id)?, emoji).await?;
        Ok(())
    }

    async fn remove_reaction(&self, app: &App, channel_id: &str, message_id: &str, emoji: &ReactionType, user_id: &str) -> Result<(), DiscordError> {
        let http = app.http().ok_or_else(offline)?;
        let user = parse_id(user_id).map(UserId::new).ok_or_else(|| DiscordError::code(10013))?;
        http.delete_reaction(cid(channel_id)?, mid(message_id)?, user, emoji).await?;
        Ok(())
    }
}

/// Discord's numeric channel types used here.
pub mod channel_kind {
    pub const GUILD_TEXT: u8 = 0;
    pub const GUILD_VOICE: u8 = 2;
    pub const GUILD_CATEGORY: u8 = 4;
    pub const GUILD_ANNOUNCEMENT: u8 = 5;
    pub const GUILD_STAGE_VOICE: u8 = 13;
    pub const GUILD_FORUM: u8 = 15;
    pub const GUILD_MEDIA: u8 = 16;
}
