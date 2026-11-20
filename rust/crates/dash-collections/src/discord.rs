//! The dashboard's Discord REST client as the record stores see it (`ctx.discord`, src/dashboard/discordApi.js): the
//! trait dash-server implements, the shapes it answers (`getGuildResources`, member profiles, message references) and
//! its error type. Only the methods the stores and the server's record routes call are part of the trait.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `DiscordApiError`: a failed Discord request. `code` is 'unauthenticated' | 'forbidden' | 'not_found' |
/// 'rate_limited' | 'unavailable'; `discord_code` is Discord's JSON error code when the answer had one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscordApiError {
    pub message: String,
    /// HTTP status (0 when the request never got an answer).
    #[serde(default)]
    pub status: u16,
    pub code: String,
    /// Whole seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord_code: Option<i64>,
}

impl DiscordApiError {
    pub fn new(message: impl Into<String>, status: u16, code: impl Into<String>) -> Self {
        Self { message: message.into(), status, code: code.into(), retry_after: None, discord_code: None }
    }

    pub fn with_discord_code(mut self, code: i64) -> Self {
        self.discord_code = Some(code);
        self
    }

    /// `err.retryAfter` (whole seconds), as a 429 answer carries it.
    pub fn with_retry_after(mut self, secs: u64) -> Self {
        self.retry_after = Some(secs);
        self
    }
}

/// One method of the dashboard's Discord client (`ctx.discord.<method>`), for [`DashboardDiscord::supports`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiscordMethod {
    GetGuildResources,
    GetMember,
    SearchMembers,
    GetUsers,
    SendMessage,
    EditMessage,
    DeleteMessage,
    CreateInvite,
}

impl DiscordMethod {
    /// The JS method name.
    pub fn js_name(self) -> &'static str {
        match self {
            Self::GetGuildResources => "getGuildResources",
            Self::GetMember => "getMember",
            Self::SearchMembers => "searchMembers",
            Self::GetUsers => "getUsers",
            Self::SendMessage => "sendMessage",
            Self::EditMessage => "editMessage",
            Self::DeleteMessage => "deleteMessage",
            Self::CreateInvite => "createInvite",
        }
    }
}

impl std::fmt::Display for DiscordApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DiscordApiError {}

/// `getGuildResources(...).guild`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ResGuild {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
    pub member_count: Option<i64>,
    pub owner_id: Option<String>,
    pub premium_tier: i64,
    pub premium_subscription_count: i64,
    pub verification_level: i64,
}

/// One channel of `getGuildResources` (sidebar order).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ResChannel {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind_id: i64,
    /// `channelKind(type)`: 'text', 'voice', 'category', ...
    pub kind: String,
    pub parent_id: Option<String>,
    pub position: i64,
    pub nsfw: bool,
}

/// One role of `getGuildResources` (highest first). The optional flags are always set by the dashboard's client; a
/// test double may leave them out, and the role rules then answer "could not check" as the JS does for `undefined`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ResRole {
    pub id: String,
    pub name: String,
    pub color: Option<String>,
    pub position: i64,
    pub managed: bool,
    pub everyone: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mentionable: Option<bool>,
    /// Below the bot's highest role, not managed, not @everyone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignable: Option<bool>,
    /// Carries a moderator or admin permission (`DANGEROUS_PERMISSIONS`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dangerous: Option<bool>,
    /// Raw permission bits, when a caller has them (the dashboard's client does not send them).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<String>,
}

/// One custom emoji of `getGuildResources`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ResEmoji {
    pub id: String,
    pub name: String,
    pub animated: bool,
}

/// `getGuildResources(...).bot`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ResBot {
    pub user_id: String,
    pub top_role_position: i64,
    pub manage_roles: bool,
    pub create_invite: bool,
}

/// What `getGuildResources(guildId)` answers (`mapGuildResources`): JSON-equal to the JS object.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscordResources {
    pub guild: ResGuild,
    pub channels: Vec<ResChannel>,
    pub roles: Vec<ResRole>,
    pub emojis: Vec<ResEmoji>,
    pub bot: ResBot,
}

impl DiscordResources {
    /// `roles.find((r) => r.id === id)`.
    pub fn role(&self, id: &str) -> Option<&ResRole> {
        self.roles.iter().find(|r| r.id == id)
    }

    /// `channels.find((c) => c.id === id)`.
    pub fn channel(&self, id: &str) -> Option<&ResChannel> {
        self.channels.iter().find(|c| c.id == id)
    }

    /// `emojis.find((e) => e.id === id)`.
    pub fn emoji(&self, id: &str) -> Option<&ResEmoji> {
        self.emojis.iter().find(|e| e.id == id)
    }
}

/// A member or user as the dashboard shows it (`memberProfile`): searchMembers' entries and getUsers' values.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MemberProfile {
    pub id: String,
    pub username: String,
    pub global_name: Option<String>,
    pub nick: Option<String>,
    pub avatar_url: String,
    pub bot: bool,
}

/// `getMember(guildId, userId)`: the member's role ids.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemberRoles {
    pub roles: Vec<String>,
}

/// `sendMessage` / `editMessage`: the message posted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRef {
    pub id: String,
    pub channel_id: String,
}

/// `createInvite`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteRef {
    pub code: String,
    pub url: String,
    pub expires_at: Option<String>,
}

/// The dashboard's Discord REST client (bot token), as `ctx.discord` of the record stores. Every method fails with a
/// [`DiscordApiError`] when Discord cannot answer. Message payloads are Discord REST message JSON; the client keeps
/// their `allowed_mentions` safe (never @everyone/@here).
#[async_trait]
pub trait DashboardDiscord: Send + Sync {
    /// `getGuildResources(guildId, { fresh })`: cached for 30 s unless `fresh`.
    async fn get_guild_resources(&self, guild_id: &str, fresh: bool) -> Result<Arc<DiscordResources>, DiscordApiError>;
    /// `getGuildResources(guildId, { swr: true })`: for a display read only (never a check of ids or roles). A client
    /// with a stale-tolerant cache may answer from an entry up to 10 minutes old while it reloads in the background;
    /// the default is the plain 30 s read.
    async fn get_guild_resources_display(&self, guild_id: &str) -> Result<Arc<DiscordResources>, DiscordApiError> {
        self.get_guild_resources(guild_id, false).await
    }
    /// `getMember(guildId, userId)`: None when the user is not in the guild.
    async fn get_member(&self, guild_id: &str, user_id: &str) -> Result<Option<MemberRoles>, DiscordApiError>;
    /// `searchMembers(guildId, query)` (at most 25; a user id finds that one member).
    async fn search_members(&self, guild_id: &str, query: &str) -> Result<Vec<MemberProfile>, DiscordApiError>;
    /// `getUsers(ids)`: id -> profile (None for an unknown user), at most 100 ids.
    async fn get_users(&self, ids: &[String]) -> Result<HashMap<String, Option<MemberProfile>>, DiscordApiError>;
    /// `sendMessage(channelId, payload)`.
    async fn send_message(&self, channel_id: &str, payload: Value) -> Result<MessageRef, DiscordApiError>;
    /// `editMessage(channelId, messageId, payload)`.
    async fn edit_message(&self, channel_id: &str, message_id: &str, payload: Value) -> Result<MessageRef, DiscordApiError>;
    /// `deleteMessage(channelId, messageId)`: a message that is already gone counts as deleted.
    async fn delete_message(&self, channel_id: &str, message_id: &str) -> Result<(), DiscordApiError>;
    /// `createInvite(channelId, { reason })`.
    async fn create_invite(&self, channel_id: &str, reason: &str) -> Result<InviteRef, DiscordApiError>;
    /// `typeof ctx.discord[method] === 'function'`: whether this client offers `method`. The real client offers all
    /// of them (the default); a test double that stands for a partial JS client answers false for the missing ones.
    fn supports(&self, method: DiscordMethod) -> bool {
        let _ = method;
        true
    }
}
