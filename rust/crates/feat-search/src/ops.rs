//! The Discord side of this crate (cache reads and REST writes) behind one replaceable object per app.
//!
//! The JS reads discord.js structures (`guild.roles.cache`, `guild.presences.cache`, `member.setNickname`, ...); the
//! JS tests swap them for fakes. Here every read of a cached guild and every REST write of the AFK and live-role code
//! goes through [`DiscordOps`]: production uses [`SerenityOps`] (the serenity cache and HTTP client of
//! `app.discord()`), tests install their own with [`set_ops`]. The decisions themselves stay in plain functions.

use std::sync::{Arc, Mutex};

use bot_core::error::BoxFuture;
use bot_core::prelude::*;
use serenity::all::{ChannelId, Guild, GuildId, Member, MessageFlags, MessageId, MessageReference, RoleId, UserId};
use serenity::builder::{CreateAllowedMentions, CreateMessage, EditMember};

/// A failed Discord REST call: the JSON error code when Discord sent one, and the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestError {
    pub code: Option<u64>,
    pub message: String,
}

impl RestError {
    pub fn new(code: Option<u64>, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    fn of(err: &serenity::Error) -> Self {
        Self { code: bot_core::discord::error_code(err), message: bot_core::discord::error_message(err) }
    }

    /// `err?.code ?? ''` as the JS logs it.
    pub fn code_text(&self) -> String {
        self.code.map(|c| c.to_string()).unwrap_or_default()
    }
}

impl std::fmt::Display for RestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Everything this crate reads from the Discord cache or writes through REST.
pub trait DiscordOps: Send + Sync + 'static {
    /// Calls `f` with the cached guild; does nothing when the guild is not cached (unavailable).
    fn with_guild(&self, app: &App, guild_id: GuildId, f: &mut dyn FnMut(&Guild));
    /// The bot's own user id (`client.user.id`).
    fn bot_id(&self, app: &App) -> Option<UserId>;
    /// `guild.members.fetch({ user, force: true })`.
    fn fetch_member(&self, app: &App, guild_id: GuildId, user_id: UserId) -> BoxFuture<Option<Member>>;
    /// `member.roles.add(roleId, reason)`.
    fn add_role(&self, app: &App, guild_id: GuildId, user_id: UserId, role_id: RoleId, reason: String) -> BoxFuture<Result<(), RestError>>;
    /// `member.roles.remove(roleId, reason)`.
    fn remove_role(&self, app: &App, guild_id: GuildId, user_id: UserId, role_id: RoleId, reason: String) -> BoxFuture<Result<(), RestError>>;
    /// `member.setNickname(nick, reason)`; `None` clears the nickname.
    fn set_nickname(&self, app: &App, guild_id: GuildId, user_id: UserId, nick: Option<String>, reason: String) -> BoxFuture<Result<(), RestError>>;
    /// `message.reply({ content, allowedMentions: { parse: [], repliedUser: false }, failIfNotExists: false, flags:
    /// SuppressEmbeds })`: the id of the reply.
    fn reply(&self, app: &App, channel_id: ChannelId, message_id: MessageId, content: String) -> BoxFuture<Result<MessageId, RestError>>;
    /// `message.delete()`.
    fn delete_message(&self, app: &App, channel_id: ChannelId, message_id: MessageId) -> BoxFuture<Result<(), RestError>>;
    /// `channel.send({ content, allowedMentions: { parse: [], roles } })`.
    fn send(&self, app: &App, channel_id: ChannelId, content: String, roles: Vec<String>) -> BoxFuture<Result<(), RestError>>;
    /// `fetchAllMembers(guild, { timeoutMs, maxMembers })`: `Err(reason)` when it did not complete.
    fn fetch_all_members(&self, app: &App, guild_id: GuildId, timeout_ms: u64, max_members: u64) -> BoxFuture<Result<(), String>>;
}

/// Production: the serenity cache and HTTP client of `app.discord()`. Offline (no connection) every read is empty and
/// every write fails, as an unreachable Discord would.
#[derive(Debug, Default, Clone, Copy)]
pub struct SerenityOps;

fn offline() -> RestError {
    RestError::new(None, "Discord is not connected")
}

impl DiscordOps for SerenityOps {
    fn with_guild(&self, app: &App, guild_id: GuildId, f: &mut dyn FnMut(&Guild)) {
        let Some(d) = app.discord() else { return };
        if let Some(g) = d.cache.guild(guild_id) {
            f(&g);
        }
    }

    fn bot_id(&self, app: &App) -> Option<UserId> {
        app.discord().map(|d| d.cache.current_user().id)
    }

    fn fetch_member(&self, app: &App, guild_id: GuildId, user_id: UserId) -> BoxFuture<Option<Member>> {
        let http = app.http();
        Box::pin(async move { http?.get_member(guild_id, user_id).await.ok() })
    }

    fn add_role(&self, app: &App, guild_id: GuildId, user_id: UserId, role_id: RoleId, reason: String) -> BoxFuture<Result<(), RestError>> {
        let http = app.http();
        Box::pin(async move {
            let http = http.ok_or_else(offline)?;
            http.add_member_role(guild_id, user_id, role_id, Some(&reason)).await.map_err(|e| RestError::of(&e))
        })
    }

    fn remove_role(&self, app: &App, guild_id: GuildId, user_id: UserId, role_id: RoleId, reason: String) -> BoxFuture<Result<(), RestError>> {
        let http = app.http();
        Box::pin(async move {
            let http = http.ok_or_else(offline)?;
            http.remove_member_role(guild_id, user_id, role_id, Some(&reason)).await.map_err(|e| RestError::of(&e))
        })
    }

    fn set_nickname(&self, app: &App, guild_id: GuildId, user_id: UserId, nick: Option<String>, reason: String) -> BoxFuture<Result<(), RestError>> {
        let http = app.http();
        Box::pin(async move {
            let http = http.ok_or_else(offline)?;
            // An empty nickname resets it (discord.js sends `nick: null`; Discord treats both the same).
            let edit = EditMember::new().nickname(nick.unwrap_or_default());
            http.edit_member(guild_id, user_id, &edit, Some(&reason)).await.map(|_| ()).map_err(|e| RestError::of(&e))
        })
    }

    fn reply(&self, app: &App, channel_id: ChannelId, message_id: MessageId, content: String) -> BoxFuture<Result<MessageId, RestError>> {
        let http = app.http();
        Box::pin(async move {
            let http = http.ok_or_else(offline)?;
            let msg = CreateMessage::new()
                .content(content)
                .reference_message(MessageReference::from((channel_id, message_id)).fail_if_not_exists(false))
                .allowed_mentions(CreateAllowedMentions::new().replied_user(false))
                .flags(MessageFlags::SUPPRESS_EMBEDS);
            channel_id.send_message(http.as_ref(), msg).await.map(|m| m.id).map_err(|e| RestError::of(&e))
        })
    }

    fn delete_message(&self, app: &App, channel_id: ChannelId, message_id: MessageId) -> BoxFuture<Result<(), RestError>> {
        let http = app.http();
        Box::pin(async move {
            let http = http.ok_or_else(offline)?;
            http.delete_message(channel_id, message_id, None).await.map_err(|e| RestError::of(&e))
        })
    }

    fn send(&self, app: &App, channel_id: ChannelId, content: String, roles: Vec<String>) -> BoxFuture<Result<(), RestError>> {
        let http = app.http();
        Box::pin(async move {
            let http = http.ok_or_else(offline)?;
            let ids: Vec<RoleId> = roles.iter().filter_map(|r| r.parse::<u64>().ok()).filter(|v| *v != 0).map(RoleId::new).collect();
            let msg = CreateMessage::new().content(content).allowed_mentions(CreateAllowedMentions::new().roles(ids));
            channel_id.send_message(http.as_ref(), msg).await.map(|_| ()).map_err(|e| RestError::of(&e))
        })
    }

    fn fetch_all_members(&self, app: &App, guild_id: GuildId, timeout_ms: u64, max_members: u64) -> BoxFuture<Result<(), String>> {
        let app = app.clone();
        Box::pin(async move {
            bot_core::guild_members::fetch_all_members(&app, guild_id, timeout_ms, max_members).await.map(|_| ()).map_err(|f| f.reason().to_string())
        })
    }
}

/// The ops installed for an app (`app.ext`), and the live-role presence switch.
#[derive(Default)]
struct OpsSlot {
    ops: Mutex<Option<Arc<dyn DiscordOps>>>,
    presences: Mutex<Option<bool>>,
}

/// The [`DiscordOps`] of this app: the one [`set_ops`] installed, else [`SerenityOps`].
pub fn ops(app: &App) -> Arc<dyn DiscordOps> {
    let slot = app.ext::<OpsSlot>();
    let installed = slot.ops.lock().unwrap_or_else(|e| e.into_inner()).clone();
    installed.unwrap_or_else(|| Arc::new(SerenityOps))
}

/// Installs the [`DiscordOps`] this app uses (tests).
pub fn set_ops(app: &App, ops: Arc<dyn DiscordOps>) {
    *app.ext::<OpsSlot>().ops.lock().unwrap_or_else(|e| e.into_inner()) = Some(ops);
}

/// `presencesEnabled()` (`DISABLE_PRESENCES`), unless [`set_presences_enabled`] overrode it for this app.
pub fn presences_enabled(app: &App) -> bool {
    let over = *app.ext::<OpsSlot>().presences.lock().unwrap_or_else(|e| e.into_inner());
    over.unwrap_or_else(bot_util::gateway_features::presences_enabled)
}

/// Overrides `presencesEnabled()` for this app only (tests run in parallel, so they never touch the environment).
pub fn set_presences_enabled(app: &App, on: bool) {
    *app.ext::<OpsSlot>().presences.lock().unwrap_or_else(|e| e.into_inner()) = Some(on);
}

/// Runs `f` on the cached guild; `None` when it is not cached.
pub fn view<R>(app: &App, guild_id: GuildId, f: impl FnOnce(&Guild) -> R) -> Option<R> {
    let mut f = Some(f);
    let mut out = None;
    ops(app).with_guild(app, guild_id, &mut |g| {
        if let Some(f) = f.take() {
            out = Some(f(g));
        }
    });
    out
}

/// A snowflake string as an id (`None` for anything that is not one).
pub fn snowflake(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|v| *v != 0)
}
