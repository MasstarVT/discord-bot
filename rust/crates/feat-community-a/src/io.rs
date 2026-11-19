//! The Discord REST calls the suggestion, confession and bump services make, behind one narrow trait.
//!
//! The JS services call discord.js objects directly (`channel.send`, `messages.edit`, `message.startThread`,
//! `user.send`) and their tests replace those objects with stubs. Here every call goes through [`CommunityIo`]:
//! production uses [`SerenityIo`] (serenity's HTTP client; every call fails when the bot is offline, as a REST call
//! without a connection fails), and a test installs its own implementation with [`set_io`], the way the JS tests stub
//! `channel.send`. Only REST goes through here; the serenity cache (channels, roles, permissions) is read directly.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bot_core::App;
use bot_core::interaction_helpers::Payload;
use serenity::all::{AutoArchiveDuration, ChannelId, Message, MessageId, User, UserId};
use serenity::builder::CreateThread;

/// A failed REST call: the Discord error code (`err.code`) when there is one, and the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoError {
    pub code: Option<u64>,
    pub message: String,
}

impl IoError {
    pub fn new(code: Option<u64>, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    /// The error of a call made while the bot is not connected.
    pub fn offline() -> Self {
        Self::new(None, "the bot is not connected to Discord")
    }
}

impl std::fmt::Display for IoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for IoError {}

impl From<serenity::Error> for IoError {
    fn from(e: serenity::Error) -> Self {
        Self { code: bot_core::discord::error_code(&e), message: bot_core::discord::error_message(&e) }
    }
}

/// The REST calls of this crate.
#[async_trait]
pub trait CommunityIo: Send + Sync + 'static {
    /// `channel.send(payload)`: the new message's id.
    async fn send(&self, channel_id: ChannelId, payload: Payload) -> Result<MessageId, IoError>;
    /// `channel.messages.edit(id, payload)` / `message.edit(payload)`.
    async fn edit(&self, channel_id: ChannelId, message_id: MessageId, payload: Payload) -> Result<(), IoError>;
    /// `channel.messages.delete(id)`.
    async fn delete_message(&self, channel_id: ChannelId, message_id: MessageId) -> Result<(), IoError>;
    /// `channel.messages.fetch(id)`.
    async fn fetch_message(&self, channel_id: ChannelId, message_id: MessageId) -> Result<Message, IoError>;
    /// `client.channels.fetch(id)` for a channel the cache does not hold: `Ok` when it exists.
    async fn fetch_channel(&self, channel_id: ChannelId) -> Result<(), IoError>;
    /// `message.startThread({ name, autoArchiveDuration: OneWeek, reason })`: the thread's id.
    async fn start_thread(&self, channel_id: ChannelId, message_id: MessageId, name: &str, reason: &str) -> Result<ChannelId, IoError>;
    /// `channel.delete(reason)`.
    async fn delete_channel(&self, channel_id: ChannelId, reason: &str) -> Result<(), IoError>;
    /// `client.users.fetch(id)`.
    async fn fetch_user(&self, user_id: UserId) -> Result<User, IoError>;
    /// `user.send(payload)`.
    async fn dm(&self, user_id: UserId, payload: Payload) -> Result<(), IoError>;
}

/// The production implementation: serenity's HTTP client.
pub struct SerenityIo(pub App);

impl SerenityIo {
    fn http(&self) -> Result<Arc<serenity::all::Http>, IoError> {
        self.0.http().ok_or_else(IoError::offline)
    }
}

#[async_trait]
impl CommunityIo for SerenityIo {
    async fn send(&self, channel_id: ChannelId, payload: Payload) -> Result<MessageId, IoError> {
        let http = self.http()?;
        Ok(channel_id.send_message(http.as_ref(), payload.to_message()).await?.id)
    }

    async fn edit(&self, channel_id: ChannelId, message_id: MessageId, payload: Payload) -> Result<(), IoError> {
        let http = self.http()?;
        channel_id.edit_message(http.as_ref(), message_id, payload.to_message_edit()).await?;
        Ok(())
    }

    async fn delete_message(&self, channel_id: ChannelId, message_id: MessageId) -> Result<(), IoError> {
        let http = self.http()?;
        channel_id.delete_message(http.as_ref(), message_id).await?;
        Ok(())
    }

    async fn fetch_message(&self, channel_id: ChannelId, message_id: MessageId) -> Result<Message, IoError> {
        let http = self.http()?;
        Ok(channel_id.message(http.as_ref(), message_id).await?)
    }

    async fn fetch_channel(&self, channel_id: ChannelId) -> Result<(), IoError> {
        let http = self.http()?;
        http.get_channel(channel_id).await?;
        Ok(())
    }

    async fn start_thread(&self, channel_id: ChannelId, message_id: MessageId, name: &str, reason: &str) -> Result<ChannelId, IoError> {
        let http = self.http()?;
        // No `type`: like discord.js `message.startThread`, Discord picks a public or news thread from the channel.
        let builder = CreateThread::new(name).auto_archive_duration(AutoArchiveDuration::OneWeek).audit_log_reason(reason);
        Ok(channel_id.create_thread_from_message(http.as_ref(), message_id, builder).await?.id)
    }

    async fn delete_channel(&self, channel_id: ChannelId, reason: &str) -> Result<(), IoError> {
        let http = self.http()?;
        http.delete_channel(channel_id, Some(reason)).await?;
        Ok(())
    }

    async fn fetch_user(&self, user_id: UserId) -> Result<User, IoError> {
        let http = self.http()?;
        Ok(user_id.to_user(http.as_ref()).await?)
    }

    async fn dm(&self, user_id: UserId, payload: Payload) -> Result<(), IoError> {
        let http = self.http()?;
        let dm = user_id.create_dm_channel(http.as_ref()).await?;
        dm.id.send_message(http.as_ref(), payload.to_message()).await?;
        Ok(())
    }
}

/// The installed implementation, one per app (empty: [`SerenityIo`]).
#[derive(Default)]
pub struct IoSlot(RwLock<Option<Arc<dyn CommunityIo>>>);

/// The REST implementation in use for `app`.
pub fn io(app: &App) -> Arc<dyn CommunityIo> {
    let slot = app.ext::<IoSlot>();
    let installed = slot.0.read().unwrap_or_else(|e| e.into_inner()).clone();
    installed.unwrap_or_else(|| Arc::new(SerenityIo(app.clone())))
}

/// Replaces the REST implementation for `app` (tests); `None` restores [`SerenityIo`].
pub fn set_io(app: &App, implementation: Option<Arc<dyn CommunityIo>>) {
    let slot = app.ext::<IoSlot>();
    *slot.0.write().unwrap_or_else(|e| e.into_inner()) = implementation;
}

/// A snowflake string as a non-zero id.
pub fn snowflake(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|v| *v != 0)
}

/// The bot's own user id (`client.user.id`), when connected.
pub fn bot_user_id(app: &App) -> Option<UserId> {
    app.discord().map(|d| d.bot_user_id())
}
