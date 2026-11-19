//! The Discord REST calls the giveaway and poll flows make, behind one small per-app seam.
//!
//! The JS services call `channel.send`, `channel.messages.edit`, `message.delete`, `channel.messages.fetch`,
//! `guild.channels.fetch` / `client.channels.fetch` and `guild.members.fetch` directly; the JS tests replace those
//! objects with stubs. Here the same calls go through [`DiscordOps`]: production uses serenity's HTTP client
//! ([`HttpOps`]), tests and the dashboard may install another implementation with [`set_discord_ops`]. Message bodies
//! are the JSON Discord receives (the `toJSON()` of the discord.js builders), so they can be compared with the JS.
//! Every decision stays outside this module.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bot_core::App;
use serde_json::Value;
use serenity::all::{ChannelId, GuildChannel, GuildId, Member, MessageId, UserId};

/// A failed Discord call: the JSON error code when Discord sent one (`err.code` in discord.js).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordError {
    pub code: Option<u64>,
    pub message: String,
}

impl DiscordError {
    pub fn new(code: Option<u64>, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    /// A Discord error code.
    pub fn code(code: u64) -> Self {
        Self { code: Some(code), message: format!("Discord error {code}") }
    }

    /// The bot is not connected (no HTTP client): every REST call fails like a network error.
    pub fn offline() -> Self {
        Self { code: None, message: "Discord is not connected".into() }
    }

    pub fn of(err: &serenity::Error) -> Self {
        Self { code: bot_core::discord::error_code(err), message: bot_core::discord::error_message(err) }
    }
}

impl std::fmt::Display for DiscordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(c) => write!(f, "{} (code {c})", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for DiscordError {}

/// A message the bot sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentMessage {
    pub id: String,
    pub channel_id: String,
    pub guild_id: Option<String>,
}

impl SentMessage {
    /// `message.url`.
    pub fn url(&self) -> String {
        format!("https://discord.com/channels/{}/{}/{}", self.guild_id.as_deref().unwrap_or("@me"), self.channel_id, self.id)
    }
}

/// The REST calls of the giveaway and poll flows. Ids are snowflake strings.
#[async_trait]
pub trait DiscordOps: Send + Sync + 'static {
    /// `channel.send(body)`.
    async fn send_message(&self, guild_id: &str, channel_id: &str, body: Value) -> Result<SentMessage, DiscordError>;
    /// `channel.messages.edit(messageId, body)`.
    async fn edit_message(&self, channel_id: &str, message_id: &str, body: Value) -> Result<(), DiscordError>;
    /// `message.delete()`.
    async fn delete_message(&self, channel_id: &str, message_id: &str) -> Result<(), DiscordError>;
    /// `channel.messages.fetch(messageId)`: `Ok` when the message exists.
    async fn fetch_message(&self, channel_id: &str, message_id: &str) -> Result<(), DiscordError>;
    /// `guild.channels.fetch(id)` over REST (the caller looked in the cache first). `Ok(None)`: not a guild channel.
    async fn fetch_channel(&self, channel_id: &str) -> Result<Option<GuildChannel>, DiscordError>;
    /// One member over REST; `Ok(None)` when they are not in the guild (Unknown Member / Unknown User).
    async fn fetch_member(&self, guild_id: &str, user_id: &str) -> Result<Option<Member>, DiscordError>;
}

fn id(s: &str) -> Result<u64, DiscordError> {
    s.parse::<u64>().ok().filter(|v| *v != 0).ok_or_else(|| DiscordError::new(Some(50035), format!("Invalid snowflake \"{s}\"")))
}

/// The production implementation: serenity's HTTP client (`app.http()`), offline when absent.
pub struct HttpOps {
    http: Option<Arc<serenity::all::Http>>,
}

impl HttpOps {
    pub fn new(app: &App) -> Self {
        Self { http: app.http() }
    }

    fn http(&self) -> Result<&serenity::all::Http, DiscordError> {
        self.http.as_deref().ok_or_else(DiscordError::offline)
    }
}

#[async_trait]
impl DiscordOps for HttpOps {
    async fn send_message(&self, guild_id: &str, channel_id: &str, body: Value) -> Result<SentMessage, DiscordError> {
        let m = self.http()?.send_message(ChannelId::new(id(channel_id)?), Vec::new(), &body).await.map_err(|e| DiscordError::of(&e))?;
        Ok(SentMessage {
            id: m.id.to_string(),
            channel_id: m.channel_id.to_string(),
            guild_id: m.guild_id.map(|g| g.to_string()).or_else(|| Some(guild_id.to_string())),
        })
    }

    async fn edit_message(&self, channel_id: &str, message_id: &str, body: Value) -> Result<(), DiscordError> {
        self.http()?
            .edit_message(ChannelId::new(id(channel_id)?), MessageId::new(id(message_id)?), &body, Vec::new())
            .await
            .map(|_| ())
            .map_err(|e| DiscordError::of(&e))
    }

    async fn delete_message(&self, channel_id: &str, message_id: &str) -> Result<(), DiscordError> {
        self.http()?.delete_message(ChannelId::new(id(channel_id)?), MessageId::new(id(message_id)?), None).await.map_err(|e| DiscordError::of(&e))
    }

    async fn fetch_message(&self, channel_id: &str, message_id: &str) -> Result<(), DiscordError> {
        self.http()?.get_message(ChannelId::new(id(channel_id)?), MessageId::new(id(message_id)?)).await.map(|_| ()).map_err(|e| DiscordError::of(&e))
    }

    async fn fetch_channel(&self, channel_id: &str) -> Result<Option<GuildChannel>, DiscordError> {
        let ch = self.http()?.get_channel(ChannelId::new(id(channel_id)?)).await.map_err(|e| DiscordError::of(&e))?;
        Ok(ch.guild())
    }

    async fn fetch_member(&self, guild_id: &str, user_id: &str) -> Result<Option<Member>, DiscordError> {
        match self.http()?.get_member(GuildId::new(id(guild_id)?), UserId::new(id(user_id)?)).await {
            Ok(m) => Ok(Some(m)),
            Err(e) => match bot_core::discord::error_code(&e) {
                Some(10007) | Some(10013) => Ok(None),
                _ => Err(DiscordError::of(&e)),
            },
        }
    }
}

/// The per-app override slot.
#[derive(Default)]
pub struct OpsSlot(RwLock<Option<Arc<dyn DiscordOps>>>);

/// Installs `ops` for this app (tests, or a host without a gateway connection).
pub fn set_discord_ops(app: &App, ops: Arc<dyn DiscordOps>) {
    *app.ext::<OpsSlot>().0.write().unwrap_or_else(|e| e.into_inner()) = Some(ops);
}

/// Whether [`set_discord_ops`] installed an override (tests, or a host without a gateway).
pub fn has_override(app: &App) -> bool {
    app.ext::<OpsSlot>().0.read().unwrap_or_else(|e| e.into_inner()).is_some()
}

/// The REST seam of this app: the installed override, else serenity's HTTP client.
pub fn ops(app: &App) -> Arc<dyn DiscordOps> {
    if let Some(o) = app.ext::<OpsSlot>().0.read().unwrap_or_else(|e| e.into_inner()).clone() {
        return o;
    }
    Arc::new(HttpOps::new(app))
}
