//! The Discord REST calls of temp voice and voice roles, behind one trait.
//!
//! The JS sends every REST call of `tempVoiceService.js` through `discordCall(label, fn)`, and every role call of
//! `voiceRoleService.js` through `member.roles.add/remove`. Here those calls go through [`VoiceRest`]. The default is
//! [`SerenityRest`] (serenity's HTTP client, `None` offline). Tests install their own implementation per app with
//! [`set_rest`]; it can then apply the matching gateway events to the offline serenity cache, as Discord would. The
//! read side (channels, voice states, members, roles) is always the serenity cache.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bot_core::App;
use serenity::all::{
    ChannelId, ChannelType, GuildChannel, GuildId, Member, MessageId, PermissionOverwrite, PermissionOverwriteType, Permissions, RoleId,
    UserId,
};
use serenity::builder::{CreateChannel, CreateMessage, EditChannel, EditMember, EditMessage};

/// A failed REST call: Discord's JSON error code when it sent one (`err.code` / `err.rawError.code`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestError {
    pub code: Option<u64>,
    pub message: String,
}

impl RestError {
    pub fn new(code: Option<u64>, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    /// No Discord connection (offline, or the client is not up yet).
    pub fn offline() -> Self {
        Self::new(None, "Discord is not connected")
    }

    pub fn from_serenity(err: &serenity::Error) -> Self {
        Self::new(bot_core::discord::error_code(err), bot_core::discord::error_message(err))
    }
}

impl std::fmt::Display for RestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RestError {}

pub type RestResult<T> = Result<T, RestError>;

/// One permission overwrite: `type` 0 is a role, 1 a member. The bits are kept as `u64` (PinMessages, bit 51, is
/// not a serenity flag).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Overwrite {
    pub id: String,
    pub kind: u8,
    pub allow: u64,
    pub deny: u64,
}

impl Overwrite {
    /// The serenity overwrite (unknown bits kept).
    pub fn to_serenity(&self) -> Option<PermissionOverwrite> {
        let id = self.id.parse::<u64>().ok().filter(|v| *v != 0)?;
        let kind = if self.kind == 1 { PermissionOverwriteType::Member(UserId::new(id)) } else { PermissionOverwriteType::Role(RoleId::new(id)) };
        Some(PermissionOverwrite { allow: Permissions::from_bits_retain(self.allow), deny: Permissions::from_bits_retain(self.deny), kind })
    }

    /// `{ id, type, allow, deny }` as discord.js serialises an overwrite (bits as strings).
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "id": self.id, "type": self.kind, "allow": self.allow.to_string(), "deny": self.deny.to_string() })
    }
}

/// `guild.channels.create({ name, type, parent, userLimit, bitrate, topic, permissionOverwrites, reason })`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewChannel {
    pub name: String,
    pub kind: ChannelType,
    pub parent: Option<ChannelId>,
    pub user_limit: Option<u32>,
    pub bitrate: Option<u32>,
    pub topic: Option<String>,
    pub overwrites: Vec<Overwrite>,
    pub reason: Option<String>,
}

/// Every Discord REST call these modules make.
#[async_trait]
pub trait VoiceRest: Send + Sync {
    /// `guild.channels.create(..)`.
    async fn create_channel(&self, guild: GuildId, spec: NewChannel) -> RestResult<GuildChannel>;
    /// `channel.delete(reason)` / `guild.channels.delete(id, reason)`.
    async fn delete_channel(&self, guild: GuildId, channel: ChannelId, reason: &str) -> RestResult<()>;
    /// `guild.channels.fetch(id)`.
    async fn fetch_channel(&self, guild: GuildId, channel: ChannelId) -> RestResult<GuildChannel>;
    /// `channel.permissionOverwrites.set(list, reason)`.
    async fn set_overwrites(&self, guild: GuildId, channel: ChannelId, overwrites: Vec<Overwrite>, reason: Option<&str>) -> RestResult<()>;
    /// `channel.setName(name, reason)`.
    async fn set_name(&self, guild: GuildId, channel: ChannelId, name: &str, reason: &str) -> RestResult<()>;
    /// `channel.setUserLimit(n, reason)`.
    async fn set_user_limit(&self, guild: GuildId, channel: ChannelId, limit: u32, reason: &str) -> RestResult<()>;
    /// `member.voice.setChannel(channel, reason)`, or `member.voice.disconnect(reason)` with `None`.
    async fn move_member(&self, guild: GuildId, user: UserId, channel: Option<ChannelId>, reason: &str) -> RestResult<()>;
    /// `guild.members.fetch(id)` (REST; callers look in the cache first).
    async fn fetch_member(&self, guild: GuildId, user: UserId) -> RestResult<Member>;
    /// `channel.send(payload)`: the new message's id.
    async fn send_message(&self, guild: GuildId, channel: ChannelId, message: CreateMessage) -> RestResult<MessageId>;
    /// `message.pin(reason)`.
    async fn pin_message(&self, guild: GuildId, channel: ChannelId, message: MessageId, reason: &str) -> RestResult<()>;
    /// `channel.messages.fetch(id)` (whether it exists).
    async fn fetch_message(&self, guild: GuildId, channel: ChannelId, message: MessageId) -> RestResult<()>;
    /// `message.edit(payload)`.
    async fn edit_message(&self, guild: GuildId, channel: ChannelId, message: MessageId, edit: EditMessage) -> RestResult<()>;
    /// `member.roles.add(roleId, reason)`.
    async fn add_role(&self, guild: GuildId, user: UserId, role: RoleId, reason: &str) -> RestResult<()>;
    /// `member.roles.remove(roleId, reason)`.
    async fn remove_role(&self, guild: GuildId, user: UserId, role: RoleId, reason: &str) -> RestResult<()>;
}

/// The production implementation: serenity's HTTP client from `app.http()`.
pub struct SerenityRest {
    app: App,
}

impl SerenityRest {
    pub fn new(app: App) -> Self {
        Self { app }
    }

    fn http(&self) -> RestResult<Arc<serenity::all::Http>> {
        self.app.http().ok_or_else(RestError::offline)
    }
}

fn map<T>(r: Result<T, serenity::Error>) -> RestResult<T> {
    r.map_err(|e| RestError::from_serenity(&e))
}

#[async_trait]
impl VoiceRest for SerenityRest {
    async fn create_channel(&self, guild: GuildId, spec: NewChannel) -> RestResult<GuildChannel> {
        let http = self.http()?;
        let mut b = CreateChannel::new(spec.name).kind(spec.kind).permissions(spec.overwrites.iter().filter_map(Overwrite::to_serenity));
        if let Some(p) = spec.parent {
            b = b.category(p);
        }
        if let Some(n) = spec.user_limit {
            b = b.user_limit(n);
        }
        if let Some(n) = spec.bitrate {
            b = b.bitrate(n);
        }
        if let Some(t) = spec.topic {
            b = b.topic(t);
        }
        let reason = spec.reason.unwrap_or_default();
        if !reason.is_empty() {
            b = b.audit_log_reason(&reason);
        }
        map(guild.create_channel(http.as_ref(), b).await)
    }

    async fn delete_channel(&self, _guild: GuildId, channel: ChannelId, reason: &str) -> RestResult<()> {
        let http = self.http()?;
        map(http.delete_channel(channel, Some(reason).filter(|r| !r.is_empty())).await.map(|_| ()))
    }

    async fn fetch_channel(&self, _guild: GuildId, channel: ChannelId) -> RestResult<GuildChannel> {
        let http = self.http()?;
        let ch = map(http.get_channel(channel).await)?;
        ch.guild().ok_or_else(|| RestError::new(None, "not a guild channel"))
    }

    async fn set_overwrites(&self, _guild: GuildId, channel: ChannelId, overwrites: Vec<Overwrite>, reason: Option<&str>) -> RestResult<()> {
        let http = self.http()?;
        let mut b = EditChannel::new().permissions(overwrites.iter().filter_map(Overwrite::to_serenity));
        if let Some(r) = reason.filter(|r| !r.is_empty()) {
            b = b.audit_log_reason(r);
        }
        map(channel.edit(http.as_ref(), b).await.map(|_| ()))
    }

    async fn set_name(&self, _guild: GuildId, channel: ChannelId, name: &str, reason: &str) -> RestResult<()> {
        let http = self.http()?;
        map(channel.edit(http.as_ref(), EditChannel::new().name(name).audit_log_reason(reason)).await.map(|_| ()))
    }

    async fn set_user_limit(&self, _guild: GuildId, channel: ChannelId, limit: u32, reason: &str) -> RestResult<()> {
        let http = self.http()?;
        map(channel.edit(http.as_ref(), EditChannel::new().user_limit(limit).audit_log_reason(reason)).await.map(|_| ()))
    }

    async fn move_member(&self, guild: GuildId, user: UserId, channel: Option<ChannelId>, reason: &str) -> RestResult<()> {
        let http = self.http()?;
        let b = match channel {
            Some(c) => EditMember::new().voice_channel(c),
            None => EditMember::new().disconnect_member(),
        };
        map(guild.edit_member(http.as_ref(), user, b.audit_log_reason(reason)).await.map(|_| ()))
    }

    async fn fetch_member(&self, guild: GuildId, user: UserId) -> RestResult<Member> {
        let http = self.http()?;
        map(http.get_member(guild, user).await)
    }

    async fn send_message(&self, _guild: GuildId, channel: ChannelId, message: CreateMessage) -> RestResult<MessageId> {
        let http = self.http()?;
        map(channel.send_message(http.as_ref(), message).await.map(|m| m.id))
    }

    async fn pin_message(&self, _guild: GuildId, channel: ChannelId, message: MessageId, reason: &str) -> RestResult<()> {
        let http = self.http()?;
        map(http.pin_message(channel, message, Some(reason)).await)
    }

    async fn fetch_message(&self, _guild: GuildId, channel: ChannelId, message: MessageId) -> RestResult<()> {
        let http = self.http()?;
        map(http.get_message(channel, message).await.map(|_| ()))
    }

    async fn edit_message(&self, _guild: GuildId, channel: ChannelId, message: MessageId, edit: EditMessage) -> RestResult<()> {
        let http = self.http()?;
        map(channel.edit_message(http.as_ref(), message, edit).await.map(|_| ()))
    }

    async fn add_role(&self, guild: GuildId, user: UserId, role: RoleId, reason: &str) -> RestResult<()> {
        let http = self.http()?;
        map(http.add_member_role(guild, user, role, Some(reason)).await)
    }

    async fn remove_role(&self, guild: GuildId, user: UserId, role: RoleId, reason: &str) -> RestResult<()> {
        let http = self.http()?;
        map(http.remove_member_role(guild, user, role, Some(reason)).await)
    }
}

/// The per-app choice of implementation.
#[derive(Default)]
pub struct RestSlot(RwLock<Option<Arc<dyn VoiceRest>>>);

/// The REST implementation of this app: the one set with [`set_rest`], else serenity.
pub fn rest(app: &App) -> Arc<dyn VoiceRest> {
    let slot = app.ext::<RestSlot>();
    let chosen = slot.0.read().unwrap_or_else(|e| e.into_inner()).clone();
    chosen.unwrap_or_else(|| Arc::new(SerenityRest::new(app.clone())))
}

/// Replaces the REST implementation of this app (tests, or a bot that wraps the calls).
pub fn set_rest(app: &App, rest: Arc<dyn VoiceRest>) {
    *app.ext::<RestSlot>().0.write().unwrap_or_else(|e| e.into_inner()) = Some(rest);
}
