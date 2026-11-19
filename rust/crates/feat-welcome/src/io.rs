//! The Discord REST calls this crate makes, behind one small seam.
//!
//! The JS services call discord.js objects directly (`channel.send`, `member.send`, `member.roles.add`,
//! `guild.members.fetch`, `client.users.fetch`, `channel.messages.edit/delete`). Here those calls go through
//! [`DiscordIo`], so the decisions around them (fallback sends, skipped roles, reverted submissions) run the same in
//! production (serenity's HTTP client) and in tests (a recording fake installed with [`set_io`]).

use std::sync::{Arc, RwLock};

use bot_core::App;
use bot_core::error::BoxFuture;
use bot_cards::welcome_card::BackgroundCheck;
use bot_cards::{WelcomeCardData, WelcomeCardStyle};
use bot_core::interaction_helpers::Payload;
use serenity::all::{ChannelId, GuildId, Member, MessageId, RoleId, User, UserId};

/// A failed REST call: the Discord error code when there is one (`err.code`), and the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoError {
    pub code: Option<u64>,
    pub message: String,
}

impl IoError {
    pub fn new(code: Option<u64>, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    /// No connection to Discord (offline app).
    pub fn offline() -> Self {
        Self::new(None, "Discord is not connected")
    }

    fn of(err: &serenity::Error) -> Self {
        Self { code: bot_core::discord::error_code(err), message: bot_core::discord::error_message(err) }
    }
}

impl std::fmt::Display for IoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for IoError {}

/// The REST calls of the welcome and applications services.
pub trait DiscordIo: Send + Sync {
    /// `channel.send(payload)`: the new message's id.
    fn send_message(&self, channel_id: ChannelId, payload: Payload) -> BoxFuture<Result<MessageId, IoError>>;
    /// `user.send(payload)` (a DM).
    fn send_dm(&self, user_id: UserId, payload: Payload) -> BoxFuture<Result<(), IoError>>;
    /// `member.roles.add(roleId, reason)`.
    fn add_role(&self, guild_id: GuildId, user_id: UserId, role_id: RoleId, reason: Option<String>) -> BoxFuture<Result<(), IoError>>;
    /// `guild.members.fetch({ user, force: true })`.
    fn fetch_member(&self, guild_id: GuildId, user_id: UserId) -> BoxFuture<Result<Member, IoError>>;
    /// `client.users.fetch(id)`.
    fn fetch_user(&self, user_id: UserId) -> BoxFuture<Result<User, IoError>>;
    /// `channel.messages.edit(id, payload)`.
    fn edit_message(&self, channel_id: ChannelId, message_id: MessageId, payload: Payload) -> BoxFuture<Result<(), IoError>>;
    /// `channel.messages.delete(id)`.
    fn delete_message(&self, channel_id: ChannelId, message_id: MessageId) -> BoxFuture<Result<(), IoError>>;
    /// `generateWelcomeCard(member, guild, style)` (downloads the avatar and the background).
    fn render_card(&self, data: WelcomeCardData, style: WelcomeCardStyle) -> BoxFuture<Result<Vec<u8>, String>> {
        Box::pin(async move { bot_cards::generate_welcome_card(&data, Some(&style)).await.map_err(|e| e.to_string()) })
    }
    /// `validateCardBackground(url)` (downloads the image).
    fn validate_background(&self, url: String) -> BoxFuture<BackgroundCheck> {
        Box::pin(async move { bot_cards::welcome_card::validate_card_background(Some(&url)).await })
    }
}

/// The production implementation: serenity's HTTP client from `app.http()` (every call fails offline).
pub struct SerenityIo(pub App);

impl DiscordIo for SerenityIo {
    fn send_message(&self, channel_id: ChannelId, payload: Payload) -> BoxFuture<Result<MessageId, IoError>> {
        let http = self.0.http();
        Box::pin(async move {
            let http = http.ok_or_else(IoError::offline)?;
            channel_id.send_message(http.as_ref(), payload.to_message()).await.map(|m| m.id).map_err(|e| IoError::of(&e))
        })
    }

    fn send_dm(&self, user_id: UserId, payload: Payload) -> BoxFuture<Result<(), IoError>> {
        let http = self.0.http();
        Box::pin(async move {
            let http = http.ok_or_else(IoError::offline)?;
            let channel = user_id.create_dm_channel(http.as_ref()).await.map_err(|e| IoError::of(&e))?;
            channel.id.send_message(http.as_ref(), payload.to_message()).await.map(|_| ()).map_err(|e| IoError::of(&e))
        })
    }

    fn add_role(&self, guild_id: GuildId, user_id: UserId, role_id: RoleId, reason: Option<String>) -> BoxFuture<Result<(), IoError>> {
        let http = self.0.http();
        Box::pin(async move {
            let http = http.ok_or_else(IoError::offline)?;
            http.add_member_role(guild_id, user_id, role_id, reason.as_deref()).await.map_err(|e| IoError::of(&e))
        })
    }

    fn fetch_member(&self, guild_id: GuildId, user_id: UserId) -> BoxFuture<Result<Member, IoError>> {
        let http = self.0.http();
        Box::pin(async move {
            let http = http.ok_or_else(IoError::offline)?;
            http.get_member(guild_id, user_id).await.map_err(|e| IoError::of(&e))
        })
    }

    fn fetch_user(&self, user_id: UserId) -> BoxFuture<Result<User, IoError>> {
        let http = self.0.http();
        let cached = self.0.discord().and_then(|d| d.cache.user(user_id).map(|u| u.clone()));
        Box::pin(async move {
            if let Some(u) = cached {
                return Ok(u);
            }
            let http = http.ok_or_else(IoError::offline)?;
            http.get_user(user_id).await.map_err(|e| IoError::of(&e))
        })
    }

    fn edit_message(&self, channel_id: ChannelId, message_id: MessageId, payload: Payload) -> BoxFuture<Result<(), IoError>> {
        let http = self.0.http();
        Box::pin(async move {
            let http = http.ok_or_else(IoError::offline)?;
            channel_id.edit_message(http.as_ref(), message_id, payload.to_message_edit()).await.map(|_| ()).map_err(|e| IoError::of(&e))
        })
    }

    fn delete_message(&self, channel_id: ChannelId, message_id: MessageId) -> BoxFuture<Result<(), IoError>> {
        let http = self.0.http();
        Box::pin(async move {
            let http = http.ok_or_else(IoError::offline)?;
            channel_id.delete_message(http.as_ref(), message_id).await.map_err(|e| IoError::of(&e))
        })
    }
}

/// The per-app override slot (`app.ext`): `None` uses [`SerenityIo`].
#[derive(Default)]
pub struct IoSlot(RwLock<Option<Arc<dyn DiscordIo>>>);

/// Replaces the Discord calls of this app (tests install a recording fake).
pub fn set_io(app: &App, io: Arc<dyn DiscordIo>) {
    let slot = app.ext::<IoSlot>();
    *slot.0.write().unwrap_or_else(|e| e.into_inner()) = Some(io);
}

/// The Discord calls of this app.
pub fn io(app: &App) -> Arc<dyn DiscordIo> {
    let slot = app.ext::<IoSlot>();
    let current = slot.0.read().unwrap_or_else(|e| e.into_inner()).clone();
    current.unwrap_or_else(|| Arc::new(SerenityIo(app.clone())))
}
