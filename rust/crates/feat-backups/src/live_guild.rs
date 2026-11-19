//! The Discord side of the restore engine ([`crate::guild_snapshot`]): the guild view read from serenity's cache (what
//! discord.js reads from `guild.roles.cache`, `guild.channels.cache` and `guild.members.me`) and every REST call the
//! restore makes, with discord.js's request bodies. Also the summary message of a restore (`postSummary`).

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{Value, json};
use serenity::all::{
    ChannelId, ChannelType, CreateAllowedMentions, CreateEmbed, CreateMessage, ForumEmoji, Guild, GuildChannel, GuildId, PermissionOverwriteType,
    Permissions, Role, RoleId, UserId,
};
use serenity::model::guild::audit_log::{Action, ChannelAction, RoleAction};
use serenity::model::id::TargetId;

use bot_core::context::App;
use bot_core::interaction_helpers::{cached_guild, check_channel_usable};
use bot_core::permissions as P;

use crate::guild_snapshot::{
    ApiError, AuditEntry, AuditKind, ChannelCreate, ChannelEdit, ChannelView, GuildView, MeView, OverwriteView, OwKind, RestoreApi, RoleData, RoleView,
    TagView,
};

fn api_err(e: serenity::Error) -> ApiError {
    let code = bot_core::discord::error_code(&e);
    ApiError { message: bot_core::discord::error_message(&e), code: code.map(|c| c.to_string()), timeout: false }
}

/// A role of the cache as the engine sees it.
pub fn role_view(r: &Role) -> RoleView {
    let primary = r.colours.primary_colour.0;
    RoleView {
        id: r.id.to_string(),
        name: r.name.clone(),
        color: if primary != 0 { primary } else { r.colour.0 },
        hoist: r.hoist,
        mentionable: r.mentionable,
        permissions: r.permissions.bits() as u128,
        raw_position: r.position as i64,
        unicode_emoji: r.unicode_emoji.clone(),
        managed: r.managed,
    }
}

fn is_thread_kind(k: ChannelType) -> bool {
    matches!(k, ChannelType::NewsThread | ChannelType::PublicThread | ChannelType::PrivateThread)
}

/// discord.js `GuildChannel#manageable`.
fn manageable(guild: Option<&Guild>, ch: &GuildChannel, bot: UserId) -> bool {
    let Some(guild) = guild else { return true };
    if guild.owner_id == bot {
        return true;
    }
    let Some(me) = guild.members.get(&bot) else { return false };
    let Some(p) = P::member_permissions_in(guild, ch, bot, &me.roles, false) else { return false };
    if p.contains(Permissions::ADMINISTRATOR) {
        return true;
    }
    if me.communication_disabled_until.is_some_and(|t| t.unix_timestamp() * 1000 > chrono::Utc::now().timestamp_millis()) {
        return false;
    }
    let need = if matches!(ch.kind, ChannelType::Voice | ChannelType::Stage) {
        Permissions::MANAGE_CHANNELS | Permissions::CONNECT
    } else {
        Permissions::VIEW_CHANNEL | Permissions::MANAGE_CHANNELS
    };
    p.contains(need)
}

/// A channel of the cache (or one a call returned) as the engine sees it.
pub fn channel_view(guild: Option<&Guild>, ch: &GuildChannel, bot: UserId) -> ChannelView {
    let mut overwrites = IndexMap::new();
    for o in &ch.permission_overwrites {
        let (id, kind) = match o.kind {
            PermissionOverwriteType::Member(u) => (u.to_string(), OwKind::Member),
            PermissionOverwriteType::Role(r) => (r.to_string(), OwKind::Role),
            _ => continue,
        };
        overwrites.insert(id.clone(), OverwriteView { id, kind, allow: o.allow.bits() as u128, deny: o.deny.bits() as u128 });
    }
    ChannelView {
        id: ch.id.to_string(),
        kind: u8::from(ch.kind) as i64,
        name: ch.name.clone(),
        parent_id: ch.parent_id.map(|p| p.to_string()),
        raw_position: ch.position as i64,
        topic: ch.topic.clone(),
        nsfw: ch.nsfw,
        rate_limit_per_user: ch.rate_limit_per_user.map(i64::from),
        default_auto_archive_duration: ch.default_auto_archive_duration.map(|d| u16::from(d) as i64),
        bitrate: ch.bitrate.map(i64::from),
        user_limit: ch.user_limit.map(i64::from),
        tags: ch
            .available_tags
            .iter()
            .map(|t| {
                let (emoji_id, emoji_name) = match &t.emoji {
                    Some(ForumEmoji::Id(id)) => (Some(id.to_string()), None),
                    Some(ForumEmoji::Name(n)) => (None, Some(n.clone())),
                    _ => (None, None),
                };
                TagView { name: t.name.clone(), moderated: t.moderated, emoji_id, emoji_name }
            })
            .collect(),
        overwrites,
        manageable: manageable(guild, ch, bot),
        is_thread: is_thread_kind(ch.kind),
    }
}

/// discord.js `guild.maximumBitrate`.
pub fn maximum_bitrate(features: &[String], premium_tier: u8) -> i64 {
    if features.iter().any(|f| f == "VIP_REGIONS") {
        return 384_000;
    }
    match premium_tier {
        1 => 128_000,
        2 => 256_000,
        3 => 384_000,
        _ => 96_000,
    }
}

/// The guild as the engine sees it, from a cached serenity guild.
pub fn guild_view(guild: &Guild, bot: UserId) -> GuildView {
    let me = guild.members.get(&bot).map(|m| {
        let highest = std::iter::once(RoleId::new(guild.id.get()))
            .chain(m.roles.iter().copied())
            .filter_map(|r| guild.roles.get(&r))
            .map(|r| r.position as i64)
            .max()
            .unwrap_or(0);
        MeView { id: bot.to_string(), permissions: P::guild_permissions(guild, bot, &m.roles).bits() as u128, highest_position: highest }
    });
    let mut roles: Vec<&Role> = guild.roles.values().collect();
    roles.sort_by_key(|r| r.id.get());
    let mut channels: Vec<&GuildChannel> = guild.channels.values().chain(guild.threads.iter()).collect();
    channels.sort_by_key(|c| c.id.get());
    GuildView {
        id: guild.id.to_string(),
        name: guild.name.clone(),
        owner_id: guild.owner_id.to_string(),
        roles: roles.into_iter().map(|r| (r.id.to_string(), role_view(r))).collect(),
        channels: channels.into_iter().map(|c| (c.id.to_string(), channel_view(Some(guild), c, bot))).collect(),
        features: guild.features.clone(),
        maximum_bitrate: Some(maximum_bitrate(&guild.features, u8::from(guild.premium_tier))),
        me,
        member_ids: guild.members.keys().map(|u| u.to_string()).collect(),
    }
}

fn id_u64(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|n| *n != 0)
}

fn bad_id() -> ApiError {
    ApiError::new("Invalid snowflake", None::<String>)
}

/// What a restore post says (`postSummary`'s payload).
#[derive(Debug, Clone)]
pub struct SummaryPost {
    /// `<@starter>`, when someone started it.
    pub content: Option<String>,
    pub embed: CreateEmbed,
    /// The one user the post may ping.
    pub ping_user: Option<String>,
}

/// The guild of a running restore, through serenity: the view from the cache and every call over REST.
#[derive(Clone)]
pub struct LiveGuild {
    pub app: App,
    pub guild_id: GuildId,
}

impl LiveGuild {
    pub fn new(app: &App, guild_id: &str) -> Self {
        Self { app: app.clone(), guild_id: GuildId::new(id_u64(guild_id).unwrap_or(1)) }
    }

    fn bot_id(&self) -> Option<UserId> {
        self.app.discord().map(|d| d.cache.current_user().id)
    }

    fn http(&self) -> Result<std::sync::Arc<serenity::all::Http>, ApiError> {
        self.app.http().ok_or_else(|| ApiError::new("Discord is not connected", None::<String>))
    }

    /// The cached guild, when there is one.
    pub fn cached(&self) -> Option<Guild> {
        cached_guild(&self.app, self.guild_id)
    }

    /// `guild.members.fetchMe()`: the bot's member over REST, when the cache has none.
    pub async fn fetch_me(&self) -> Option<MeView> {
        let bot = self.bot_id()?;
        let http = self.app.http()?;
        let fut = async move { http.get_member(self.guild_id, bot).await };
        let member = tokio::time::timeout(std::time::Duration::from_millis(10_000), fut).await.ok()?.ok()?;
        let guild = self.cached()?;
        let highest = std::iter::once(RoleId::new(guild.id.get()))
            .chain(member.roles.iter().copied())
            .filter_map(|r| guild.roles.get(&r))
            .map(|r| r.position as i64)
            .max()
            .unwrap_or(0);
        Some(MeView { id: bot.to_string(), permissions: P::guild_permissions(&guild, bot, &member.roles).bits() as u128, highest_position: highest })
    }

    /// `checkChannelUsable(channel).ok` for the default needs (View Channel, Send Messages, Embed Links).
    pub fn channel_usable(&self, channel_id: &str) -> bool {
        let Some(id) = id_u64(channel_id) else { return false };
        check_channel_usable(&self.app, self.guild_id, Some(ChannelId::new(id)), &["ViewChannel", "SendMessages", "EmbedLinks"]).ok
    }

    /// `channel.send({ content, embeds, allowedMentions })`.
    pub async fn send_channel(&self, channel_id: &str, post: &SummaryPost) -> Result<(), ApiError> {
        let http = self.http()?;
        let id = id_u64(channel_id).ok_or_else(bad_id)?;
        let mentions = match &post.ping_user {
            Some(u) => CreateAllowedMentions::new().users(id_u64(u).map(UserId::new)),
            None => CreateAllowedMentions::new(),
        };
        let mut msg = CreateMessage::new().embed(post.embed.clone()).allowed_mentions(mentions);
        if let Some(c) = &post.content {
            msg = msg.content(c.clone());
        }
        ChannelId::new(id).send_message(http.as_ref(), msg).await.map(|_| ()).map_err(api_err)
    }

    /// `client.users.fetch(id)` then `user.send({ embeds })`.
    pub async fn send_dm(&self, user_id: &str, embed: &CreateEmbed) -> Result<(), ApiError> {
        let http = self.http()?;
        let user = UserId::new(id_u64(user_id).ok_or_else(bad_id)?);
        let dm = user.create_dm_channel(http.as_ref()).await.map_err(api_err)?;
        dm.id.send_message(http.as_ref(), CreateMessage::new().embed(embed.clone())).await.map(|_| ()).map_err(api_err)
    }
}

#[async_trait]
impl RestoreApi for LiveGuild {
    fn view(&self) -> GuildView {
        match (self.cached(), self.bot_id()) {
            (Some(g), Some(bot)) => guild_view(&g, bot),
            _ => GuildView { id: self.guild_id.to_string(), ..Default::default() },
        }
    }

    async fn edit_everyone(&self, permissions: u128, reason: &str) -> Result<(), ApiError> {
        let http = self.http()?;
        let body = json!({ "permissions": permissions.to_string() });
        http.edit_role(self.guild_id, RoleId::new(self.guild_id.get()), &body, Some(reason)).await.map(|_| ()).map_err(api_err)
    }

    async fn create_role(&self, data: &RoleData, reason: &str) -> Result<RoleView, ApiError> {
        let http = self.http()?;
        let role = http.create_role(self.guild_id, &data.to_body(), Some(reason)).await.map_err(api_err)?;
        // discord.js `roles.create` adds the role to the cache at once; the next batch (2 s later) and the summary read
        // the cache, which must not wait for the gateway's GUILD_ROLE_CREATE.
        bot_core::discord::cache_created_role(&self.app, self.guild_id, &role);
        Ok(role_view(&role))
    }

    async fn edit_role(&self, role_id: &str, data: &RoleData, reason: &str) -> Result<(), ApiError> {
        let http = self.http()?;
        let id = RoleId::new(id_u64(role_id).ok_or_else(bad_id)?);
        http.edit_role(self.guild_id, id, &data.to_body(), Some(reason)).await.map(|_| ()).map_err(api_err)
    }

    async fn set_role_positions(&self, entries: &[(String, i64)]) -> Result<(), ApiError> {
        let http = self.http()?;
        // `guild.roles.setPositions(entries)`: the whole-body PATCH, without an audit-log reason.
        let mut positions = Vec::with_capacity(entries.len());
        for (id, p) in entries {
            positions.push((RoleId::new(id_u64(id).ok_or_else(bad_id)?), u16::try_from(*p).unwrap_or(u16::MAX)));
        }
        bot_core::discord::set_role_positions(&http, self.guild_id, &positions, None).await.map(|_| ()).map_err(api_err)
    }

    async fn create_channel(&self, data: &ChannelCreate, reason: &str) -> Result<ChannelView, ApiError> {
        let http = self.http()?;
        let bot = self.bot_id().unwrap_or(UserId::new(1));
        let ch = http.create_channel(self.guild_id, &data.to_body(), Some(reason)).await.map_err(api_err)?;
        // `channels.create` caches the channel at once (see create_role).
        bot_core::discord::cache_created_channel(&self.app, &ch);
        let mut v = channel_view(self.cached().as_ref(), &ch, bot);
        v.manageable = true; // the bot just made it
        Ok(v)
    }

    async fn edit_channel(&self, channel_id: &str, data: &ChannelEdit, reason: &str) -> Result<(), ApiError> {
        let http = self.http()?;
        let id = ChannelId::new(id_u64(channel_id).ok_or_else(bad_id)?);
        http.edit_channel(id, &data.to_body(), Some(reason)).await.map(|_| ()).map_err(api_err)
    }

    async fn set_channel_positions(&self, entries: &[(String, i64)]) -> Result<(), ApiError> {
        let http = self.http()?;
        let body = Value::Array(entries.iter().map(|(id, p)| json!({ "id": id, "position": p })).collect());
        http.edit_guild_channel_positions(self.guild_id, &body).await.map_err(api_err)
    }

    async fn put_overwrite(&self, channel_id: &str, target: &str, kind: OwKind, allow: u128, deny: u128, reason: &str) -> Result<(), ApiError> {
        let http = self.http()?;
        let channel = ChannelId::new(id_u64(channel_id).ok_or_else(bad_id)?);
        let target = TargetId::new(id_u64(target).ok_or_else(bad_id)?);
        let body = json!({ "type": kind.code(), "allow": allow.to_string(), "deny": deny.to_string() });
        http.create_permission(channel, target, &body, Some(reason)).await.map_err(api_err)
    }

    async fn fetch_member(&self, user_id: &str) -> Result<bool, ApiError> {
        let http = self.http()?;
        let id = UserId::new(id_u64(user_id).ok_or_else(bad_id)?);
        http.get_member(self.guild_id, id).await.map(|_| true).map_err(api_err)
    }

    async fn audit_log(&self, kind: AuditKind, limit: u8) -> Result<Vec<AuditEntry>, ApiError> {
        let http = self.http()?;
        let action = match kind {
            AuditKind::RoleCreate => Action::Role(RoleAction::Create),
            AuditKind::ChannelCreate => Action::Channel(ChannelAction::Create),
        };
        let logs = http.get_audit_logs(self.guild_id, Some(action), None, None, Some(limit)).await.map_err(api_err)?;
        Ok(logs
            .entries
            .iter()
            .map(|e| AuditEntry {
                target_id: e.target_id.map(|t| t.get().to_string()),
                executor_id: Some(e.user_id.to_string()),
                reason: e.reason.clone(),
                created_ms: e.id.created_at().timestamp_millis(),
            })
            .collect())
    }
}
