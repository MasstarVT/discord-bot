//! The Discord operations reaction roles need, behind one trait ([`RrDiscord`]), so the role engine, the hooks and the
//! command run the same against serenity ([`SerenityRr`], the production one) and against a recording fake in tests.
//! The JS called discord.js objects directly (`guild.roles.cache`, `member.roles.add`, `message.react`, ...); each
//! method names the call it stands for.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bot_core::App;
use bot_core::discord::GuildState;
use bot_core::interaction_helpers::{PermTarget, RoleCheck, cached_channel, check_assignable_role_cached, missing_bot_permissions};
use bot_core::permissions;
use serde_json::Value;
use serenity::all::{ChannelId, EmojiId, GuildId, Member, Message, MessageId, ReactionType, RoleId, UserId};

use super::pure::{EmojiInfo, EmojiLookup, normalize_emoji_key};

/// A failed Discord call: the API error code when there is one (`err.code`), and the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DErr {
    pub code: Option<i64>,
    pub message: String,
}

impl DErr {
    pub fn code(code: i64) -> Self {
        Self { code: Some(code), message: format!("Discord {code}") }
    }

    pub fn other(message: impl Into<String>) -> Self {
        Self { code: None, message: message.into() }
    }

    pub fn of(err: &serenity::Error) -> Self {
        Self { code: bot_core::discord::error_code(err).map(|c| c as i64), message: bot_core::discord::error_message(err) }
    }
}

impl std::fmt::Display for DErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DErr {}

pub type DResult<T> = Result<T, DErr>;

/// One reaction of a message (`message.reactions.cache`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionInfo {
    /// The custom emoji id, or `None` for a standard emoji.
    pub id: Option<String>,
    pub name: Option<String>,
    pub animated: bool,
    /// The bot reacted with it.
    pub me: bool,
}

impl ReactionInfo {
    /// `reactionKey(r)`: `normalizeEmojiKey(r.emoji.id ?? r.emoji.name)`.
    pub fn key(&self) -> String {
        normalize_emoji_key(self.id.as_deref().or(self.name.as_deref()).unwrap_or_default())
    }

    /// The reaction of a stored emoji (`<:name:id>`, `<a:name:id>` or a standard emoji).
    pub fn of_stored(stored: &str, me: bool) -> Self {
        match super::pure::component_emoji(Some(stored)) {
            Some(super::pure::ComponentEmoji::Custom { id, name, animated }) => Self { id: Some(id), name: Some(name), animated, me },
            _ => Self { id: None, name: Some(stored.to_string()), animated: false, me },
        }
    }

    pub fn reaction_type(&self) -> ReactionType {
        match self.id.as_deref().and_then(|i| i.parse::<u64>().ok()).filter(|v| *v != 0) {
            Some(id) => ReactionType::Custom { animated: self.animated, id: EmojiId::new(id), name: self.name.clone() },
            None => ReactionType::Unicode(self.name.clone().unwrap_or_default()),
        }
    }

    fn of_serenity(r: &ReactionType, me: bool) -> Self {
        match r {
            ReactionType::Custom { animated, id, name } => Self { id: Some(id.to_string()), name: name.clone(), animated: *animated, me },
            ReactionType::Unicode(n) => Self { id: None, name: Some(n.clone()), animated: false, me },
            _ => Self { id: None, name: None, animated: false, me },
        }
    }
}

/// A message (`channel.messages.fetch(id)`), with what reaction roles read of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageInfo {
    pub id: String,
    pub channel_id: String,
    pub guild_id: Option<String>,
    pub content: String,
    pub first_embed_title: Option<String>,
    pub reactions: Vec<ReactionInfo>,
}

impl MessageInfo {
    /// `message.url`.
    pub fn url(&self) -> String {
        format!("https://discord.com/channels/{}/{}/{}", self.guild_id.as_deref().unwrap_or("@me"), self.channel_id, self.id)
    }

    /// `message.reactions.cache.find((r) => reactionKey(r) === key)`.
    pub fn reaction(&self, key: &str) -> Option<&ReactionInfo> {
        self.reactions.iter().find(|r| r.key() == key)
    }

    pub fn of_serenity(m: &Message, guild_id: Option<&str>) -> Self {
        Self {
            id: m.id.to_string(),
            channel_id: m.channel_id.to_string(),
            guild_id: m.guild_id.map(|g| g.to_string()).or_else(|| guild_id.map(str::to_string)),
            content: m.content.clone(),
            first_embed_title: m.embeds.first().and_then(|e| e.title.clone()),
            reactions: m.reactions.iter().map(|r| ReactionInfo::of_serenity(&r.reaction_type, r.me)).collect(),
        }
    }
}

/// A guild channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelInfo {
    pub id: String,
    pub name: String,
    /// `channel.isTextBased()`.
    pub text_based: bool,
}

/// A role of the guild (`guild.roles.cache`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleInfo {
    pub id: String,
    pub name: String,
    /// Its place in the role list (higher is higher).
    pub position: i64,
    pub managed: bool,
    /// `role.editable`: the bot has Manage Roles and its highest role is above this one.
    pub editable: bool,
}

/// The Discord calls of reaction roles.
#[async_trait]
pub trait RrDiscord: Send + Sync {
    /// `client.user.id`.
    fn bot_id(&self) -> Option<String>;
    /// `guild.available !== false` (and the guild is cached).
    fn guild_available(&self, guild_id: &str) -> bool;
    /// `guild.roles.cache.get(id)`.
    fn cached_role(&self, guild_id: &str, role_id: &str) -> Option<RoleInfo>;
    /// `guild.roles.cache` (with @everyone).
    fn guild_roles(&self, guild_id: &str) -> Vec<RoleInfo>;
    /// `guild.members.me.permissions.has(ManageRoles)`.
    fn bot_can_manage_roles(&self, guild_id: &str) -> bool;
    /// `checkAssignableRole(guild, role, { executor })` (dangerous permissions refused).
    fn check_assignable(&self, guild_id: &str, role_id: &str, executor: Option<&Member>) -> RoleCheck;
    /// `guild.roles.fetch(id, { force: true })`: `Ok(true)` when it exists, `Ok(false)` when Discord says it does not.
    async fn fetch_role_exists(&self, guild_id: &str, role_id: &str) -> DResult<bool>;
    /// `guild.roles.create(body)`.
    async fn create_role(&self, guild_id: &str, body: &Value, reason: &str) -> DResult<RoleInfo>;
    /// `member.fetch(true)`: the member's role ids, read over REST.
    async fn member_roles(&self, guild_id: &str, user_id: &str) -> DResult<Vec<String>>;
    /// `member.roles.add(roleId, reason)` (one id, never the array form).
    async fn add_role(&self, guild_id: &str, user_id: &str, role_id: &str, reason: &str) -> DResult<()>;
    /// `member.roles.remove(roleId, reason)`.
    async fn remove_role(&self, guild_id: &str, user_id: &str, role_id: &str, reason: &str) -> DResult<()>;
    /// `guild.channels.cache.get(id)`.
    fn cached_channel(&self, guild_id: &str, channel_id: &str) -> Option<ChannelInfo>;
    /// `guild.channels.fetch(id)`.
    async fn fetch_channel(&self, guild_id: &str, channel_id: &str) -> DResult<ChannelInfo>;
    /// `channel.permissionsFor(guild.members.me).has(ManageMessages)` for a cached channel.
    fn bot_has_manage_messages(&self, guild_id: &str, channel_id: &str) -> bool;
    /// `channel.messages.fetch(id)`.
    async fn fetch_message(&self, guild_id: &str, channel_id: &str, message_id: &str) -> DResult<MessageInfo>;
    /// `channel.send(body)`.
    async fn send_message(&self, guild_id: &str, channel_id: &str, body: &Value) -> DResult<MessageInfo>;
    /// `message.edit(body)`.
    async fn edit_message(&self, channel_id: &str, message_id: &str, body: &Value) -> DResult<()>;
    /// `message.delete()`.
    async fn delete_message(&self, channel_id: &str, message_id: &str) -> DResult<()>;
    /// `message.react(stored)`.
    async fn react(&self, channel_id: &str, message_id: &str, stored: &str) -> DResult<()>;
    /// `reaction.users.remove(userId)`.
    async fn remove_user_reaction(&self, channel_id: &str, message_id: &str, reaction: &ReactionInfo, user_id: &str) -> DResult<()>;
    /// `reaction.remove()`: everyone's reaction of that emoji.
    async fn remove_emoji_reactions(&self, channel_id: &str, message_id: &str, reaction: &ReactionInfo) -> DResult<()>;
    /// `guild.emojis.cache`.
    fn guild_emojis(&self, guild_id: &str) -> Vec<EmojiInfo>;
    /// `client.emojis.cache.get(id)`.
    fn client_emoji(&self, emoji_id: &str) -> Option<EmojiInfo>;
}

/// The [`EmojiLookup`] of one guild.
pub struct GuildEmojis<'a> {
    pub ops: &'a dyn RrDiscord,
    pub guild_id: &'a str,
}

impl EmojiLookup for GuildEmojis<'_> {
    fn guild_emojis(&self) -> Vec<EmojiInfo> {
        self.ops.guild_emojis(self.guild_id)
    }
    fn client_emoji(&self, id: &str) -> Option<EmojiInfo> {
        self.ops.client_emoji(id)
    }
}

/// The per-app override of the Discord operations (tests install a fake).
#[derive(Default)]
pub struct RrDiscordSlot(RwLock<Option<Arc<dyn RrDiscord>>>);

/// The Discord operations of this app: the override, else serenity.
pub fn discord_ops(app: &App) -> Arc<dyn RrDiscord> {
    if let Some(o) = app.ext::<RrDiscordSlot>().0.read().unwrap_or_else(|e| e.into_inner()).clone() {
        return o;
    }
    Arc::new(SerenityRr { app: app.clone() })
}

/// Installs other Discord operations for this app (tests).
pub fn set_discord_ops(app: &App, ops: Arc<dyn RrDiscord>) {
    *app.ext::<RrDiscordSlot>().0.write().unwrap_or_else(|e| e.into_inner()) = Some(ops);
}

/// The production [`RrDiscord`]: the serenity cache and REST client of the app. Offline (no Discord) every REST call
/// fails with no code and the cache is empty.
pub struct SerenityRr {
    pub app: App,
}

fn id64(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|v| *v != 0)
}

fn offline() -> DErr {
    DErr::other("Discord is not connected")
}

fn bad_id() -> DErr {
    DErr::other("invalid snowflake")
}

impl SerenityRr {
    fn http(&self) -> DResult<Arc<serenity::all::Http>> {
        self.app.http().ok_or_else(offline)
    }

    fn with_guild<T>(&self, guild_id: &str, f: impl FnOnce(&serenity::all::Guild, UserId) -> T) -> Option<T> {
        let d = self.app.discord()?;
        let bot = d.cache.current_user().id;
        let guild = d.cache.guild(GuildId::new(id64(guild_id)?))?;
        Some(f(&guild, bot))
    }

    fn role_info(guild: &serenity::all::Guild, bot: UserId, role: &serenity::all::Role) -> RoleInfo {
        let me = permissions::me(guild, bot);
        let editable = !role.managed
            && me.is_some_and(|me| {
                permissions::js_has(permissions::guild_permissions(guild, bot, &me.roles), serenity::all::Permissions::MANAGE_ROLES)
                    && permissions::role_position(guild, role.id).unwrap_or(0) < permissions::highest_position(guild, &me.roles)
            });
        RoleInfo {
            id: role.id.to_string(),
            name: role.name.clone(),
            position: permissions::role_position(guild, role.id).unwrap_or(0) as i64,
            managed: role.managed,
            editable,
        }
    }
}

#[async_trait]
impl RrDiscord for SerenityRr {
    fn bot_id(&self) -> Option<String> {
        Some(self.app.discord()?.cache.current_user().id.to_string())
    }

    fn guild_available(&self, guild_id: &str) -> bool {
        self.app.guild_directory().guild_state(guild_id) == GuildState::Available
    }

    fn cached_role(&self, guild_id: &str, role_id: &str) -> Option<RoleInfo> {
        let rid = RoleId::new(id64(role_id)?);
        self.with_guild(guild_id, |g, bot| g.roles.get(&rid).map(|r| Self::role_info(g, bot, r))).flatten()
    }

    fn guild_roles(&self, guild_id: &str) -> Vec<RoleInfo> {
        self.with_guild(guild_id, |g, bot| g.roles.values().map(|r| Self::role_info(g, bot, r)).collect()).unwrap_or_default()
    }

    fn bot_can_manage_roles(&self, guild_id: &str) -> bool {
        self.with_guild(guild_id, |g, bot| {
            permissions::me(g, bot)
                .is_some_and(|me| permissions::js_has(permissions::guild_permissions(g, bot, &me.roles), serenity::all::Permissions::MANAGE_ROLES))
        })
        .unwrap_or(false)
    }

    fn check_assignable(&self, guild_id: &str, role_id: &str, executor: Option<&Member>) -> RoleCheck {
        match (id64(guild_id), id64(role_id)) {
            (Some(g), Some(r)) => check_assignable_role_cached(&self.app, GuildId::new(g), RoleId::new(r), executor, false),
            _ => RoleCheck { ok: false, reason: Some("Role not found.".into()), dangerous: Vec::new() },
        }
    }

    async fn fetch_role_exists(&self, guild_id: &str, role_id: &str) -> DResult<bool> {
        let g = GuildId::new(id64(guild_id).ok_or_else(bad_id)?);
        let roles = self.http()?.get_guild_roles(g).await.map_err(|e| DErr::of(&e))?;
        // discord.js `roles.fetch(id, { force: true })` caches what it fetched, so a role that exists but was not
        // cached is usable at once.
        match roles.iter().find(|r| r.id.to_string() == role_id) {
            Some(r) => {
                bot_core::discord::cache_created_role(&self.app, g, r);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn create_role(&self, guild_id: &str, body: &Value, reason: &str) -> DResult<RoleInfo> {
        let g = GuildId::new(id64(guild_id).ok_or_else(bad_id)?);
        let role = self.http()?.create_role(g, body, Some(reason)).await.map_err(|e| DErr::of(&e))?;
        // discord.js `guild.roles.create` caches the new role at once.
        bot_core::discord::cache_created_role(&self.app, g, &role);
        Ok(self
            .with_guild(guild_id, |gd, bot| Self::role_info(gd, bot, &role))
            .unwrap_or(RoleInfo { id: role.id.to_string(), name: role.name.clone(), position: 0, managed: role.managed, editable: false }))
    }

    async fn member_roles(&self, guild_id: &str, user_id: &str) -> DResult<Vec<String>> {
        let g = GuildId::new(id64(guild_id).ok_or_else(bad_id)?);
        let u = UserId::new(id64(user_id).ok_or_else(bad_id)?);
        let m = self.http()?.get_member(g, u).await.map_err(|e| DErr::of(&e))?;
        Ok(m.roles.iter().map(|r| r.to_string()).collect())
    }

    async fn add_role(&self, guild_id: &str, user_id: &str, role_id: &str, reason: &str) -> DResult<()> {
        let (g, u, r) = (id64(guild_id).ok_or_else(bad_id)?, id64(user_id).ok_or_else(bad_id)?, id64(role_id).ok_or_else(bad_id)?);
        self.http()?.add_member_role(GuildId::new(g), UserId::new(u), RoleId::new(r), Some(reason)).await.map_err(|e| DErr::of(&e))
    }

    async fn remove_role(&self, guild_id: &str, user_id: &str, role_id: &str, reason: &str) -> DResult<()> {
        let (g, u, r) = (id64(guild_id).ok_or_else(bad_id)?, id64(user_id).ok_or_else(bad_id)?, id64(role_id).ok_or_else(bad_id)?);
        self.http()?.remove_member_role(GuildId::new(g), UserId::new(u), RoleId::new(r), Some(reason)).await.map_err(|e| DErr::of(&e))
    }

    fn cached_channel(&self, guild_id: &str, channel_id: &str) -> Option<ChannelInfo> {
        let c = cached_channel(&self.app, GuildId::new(id64(guild_id)?), ChannelId::new(id64(channel_id)?))?;
        Some(ChannelInfo { id: c.id.to_string(), name: c.name.clone(), text_based: permissions::is_text_based(&c) })
    }

    async fn fetch_channel(&self, guild_id: &str, channel_id: &str) -> DResult<ChannelInfo> {
        if let Some(c) = self.cached_channel(guild_id, channel_id) {
            return Ok(c);
        }
        let c = ChannelId::new(id64(channel_id).ok_or_else(bad_id)?);
        match self.http()?.get_channel(c).await.map_err(|e| DErr::of(&e))? {
            serenity::all::Channel::Guild(gc) => {
                // discord.js refuses a channel of another guild with GuildChannelUnowned, an error without a
                // numeric code (so the panel is 'failed', not flagged missing), and caches what it fetched.
                if gc.guild_id.to_string() != guild_id {
                    return Err(DErr::other("The fetched channel does not belong to this manager's guild."));
                }
                bot_core::discord::cache_created_channel(&self.app, &gc);
                Ok(ChannelInfo { id: gc.id.to_string(), name: gc.name.clone(), text_based: permissions::is_text_based(&gc) })
            }
            _ => Err(DErr::other("The fetched channel does not belong to this manager's guild.")),
        }
    }

    fn bot_has_manage_messages(&self, guild_id: &str, channel_id: &str) -> bool {
        let (Some(g), Some(c)) = (id64(guild_id), id64(channel_id)) else { return false };
        if cached_channel(&self.app, GuildId::new(g), ChannelId::new(c)).is_none() {
            return false;
        }
        missing_bot_permissions(&self.app, PermTarget::Channel(GuildId::new(g), ChannelId::new(c)), &["ManageMessages"]).is_empty()
    }

    async fn fetch_message(&self, guild_id: &str, channel_id: &str, message_id: &str) -> DResult<MessageInfo> {
        let c = ChannelId::new(id64(channel_id).ok_or_else(bad_id)?);
        let m = MessageId::new(id64(message_id).ok_or_else(|| DErr::code(10008))?);
        // REST first: serenity's message cache does not follow reaction events (discord.js's does), and the callers
        // read the reactions. Without a REST client (offline) the cached copy is all there is.
        let Some(http) = self.app.http() else {
            return match self.app.discord().and_then(|d| d.cache.message(c, m).map(|msg| msg.clone())) {
                Some(msg) => Ok(MessageInfo::of_serenity(&msg, Some(guild_id))),
                None => Err(offline()),
            };
        };
        let msg = http.get_message(c, m).await.map_err(|e| DErr::of(&e))?;
        Ok(MessageInfo::of_serenity(&msg, Some(guild_id)))
    }

    async fn send_message(&self, guild_id: &str, channel_id: &str, body: &Value) -> DResult<MessageInfo> {
        let c = ChannelId::new(id64(channel_id).ok_or_else(bad_id)?);
        let msg = self.http()?.send_message(c, Vec::new(), body).await.map_err(|e| DErr::of(&e))?;
        Ok(MessageInfo::of_serenity(&msg, Some(guild_id)))
    }

    async fn edit_message(&self, channel_id: &str, message_id: &str, body: &Value) -> DResult<()> {
        let c = ChannelId::new(id64(channel_id).ok_or_else(bad_id)?);
        let m = MessageId::new(id64(message_id).ok_or_else(|| DErr::code(10008))?);
        self.http()?.edit_message(c, m, body, Vec::new()).await.map(|_| ()).map_err(|e| DErr::of(&e))
    }

    async fn delete_message(&self, channel_id: &str, message_id: &str) -> DResult<()> {
        let c = ChannelId::new(id64(channel_id).ok_or_else(bad_id)?);
        let m = MessageId::new(id64(message_id).ok_or_else(|| DErr::code(10008))?);
        self.http()?.delete_message(c, m, None).await.map_err(|e| DErr::of(&e))
    }

    async fn react(&self, channel_id: &str, message_id: &str, stored: &str) -> DResult<()> {
        let c = ChannelId::new(id64(channel_id).ok_or_else(bad_id)?);
        let m = MessageId::new(id64(message_id).ok_or_else(|| DErr::code(10008))?);
        let rt = ReactionInfo::of_stored(stored, true).reaction_type();
        self.http()?.create_reaction(c, m, &rt).await.map_err(|e| DErr::of(&e))
    }

    async fn remove_user_reaction(&self, channel_id: &str, message_id: &str, reaction: &ReactionInfo, user_id: &str) -> DResult<()> {
        let c = ChannelId::new(id64(channel_id).ok_or_else(bad_id)?);
        let m = MessageId::new(id64(message_id).ok_or_else(|| DErr::code(10008))?);
        let http = self.http()?;
        let rt = reaction.reaction_type();
        if self.bot_id().as_deref() == Some(user_id) {
            return http.delete_reaction_me(c, m, &rt).await.map_err(|e| DErr::of(&e));
        }
        let u = UserId::new(id64(user_id).ok_or_else(bad_id)?);
        http.delete_reaction(c, m, u, &rt).await.map_err(|e| DErr::of(&e))
    }

    async fn remove_emoji_reactions(&self, channel_id: &str, message_id: &str, reaction: &ReactionInfo) -> DResult<()> {
        let c = ChannelId::new(id64(channel_id).ok_or_else(bad_id)?);
        let m = MessageId::new(id64(message_id).ok_or_else(|| DErr::code(10008))?);
        self.http()?.delete_message_reaction_emoji(c, m, &reaction.reaction_type()).await.map_err(|e| DErr::of(&e))
    }

    fn guild_emojis(&self, guild_id: &str) -> Vec<EmojiInfo> {
        self.with_guild(guild_id, |g, bot| g.emojis.values().map(|e| emoji_info(g, bot, e)).collect()).unwrap_or_default()
    }

    fn client_emoji(&self, emoji_id: &str) -> Option<EmojiInfo> {
        let d = self.app.discord()?;
        let bot = d.cache.current_user().id;
        let id = EmojiId::new(id64(emoji_id)?);
        for gid in d.cache.guilds() {
            if let Some(g) = d.cache.guild(gid)
                && let Some(e) = g.emojis.get(&id)
            {
                return Some(emoji_info(&g, bot, e));
            }
        }
        None
    }
}

fn emoji_info(guild: &serenity::all::Guild, bot: UserId, e: &serenity::all::Emoji) -> EmojiInfo {
    let limited = !e.roles.is_empty() && permissions::me(guild, bot).is_some_and(|me| !me.roles.iter().any(|r| e.roles.contains(r)));
    EmojiInfo { id: e.id.to_string(), name: e.name.clone(), animated: e.animated, available: e.available, limited_for_bot: limited }
}
