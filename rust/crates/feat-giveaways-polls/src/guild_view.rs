//! Cached-guild lookups both services share: the bot's permissions in a channel (cached or fetched), the executor's,
//! role pings, and the guild's availability. Ports the pieces of `checkChannelUsable`, `channel.permissionsFor` and
//! `pingableRoleIds` the JS calls on discord.js objects.

use bot_core::App;
use bot_core::discord::GuildState;
use bot_core::interaction_helpers::{ChannelUsable, Pingable, cached_channel, channel_usable_verdict, pingable_role_ids};
use bot_core::permissions::{self, js_has};
use serenity::all::{ChannelId, Guild, GuildChannel, GuildId, Member, Permissions, RoleId, UserId};

use crate::discord_ops::{DiscordError, ops};

/// A snowflake string as a non-zero id.
pub fn snowflake(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|v| *v != 0)
}

pub fn guild_id_of(s: &str) -> Option<GuildId> {
    snowflake(s).map(GuildId::new)
}

pub fn channel_id_of(s: &str) -> Option<ChannelId> {
    snowflake(s).map(ChannelId::new)
}

/// The bot's user id (`client.user.id`).
pub fn bot_id(app: &App) -> Option<UserId> {
    app.discord().map(|d| d.bot_user_id())
}

/// `interaction.inCachedGuild()`: the guild is in the cache (no clone).
pub fn guild_cached(app: &App, guild_id: GuildId) -> bool {
    app.discord().is_some_and(|d| d.cache.guild(guild_id).is_some())
}

/// `guild.available !== false` for a guild this process holds.
pub fn guild_available(app: &App, guild_id: &str) -> bool {
    app.guild_directory().guild_state(guild_id) == GuildState::Available
}

/// The role ids of a cached guild (`guild.roles.cache`), empty when the guild is not cached.
pub fn role_ids(app: &App, guild_id: &str) -> Vec<String> {
    let Some(g) = guild_id_of(guild_id) else { return Vec::new() };
    app.discord().and_then(|d| d.cache.guild(g).map(|guild| guild.roles.keys().map(|r| r.to_string()).collect())).unwrap_or_default()
}

/// `guild.roles.cache.has(id)` as a closure over a snapshot.
pub fn role_exists_fn(app: &App, guild_id: &str) -> impl Fn(&str) -> bool + Send + Sync + 'static + use<> {
    let ids: std::collections::HashSet<String> = role_ids(app, guild_id).into_iter().collect();
    move |id: &str| ids.contains(id)
}

fn bot_roles(guild: &Guild, bot: UserId) -> Vec<RoleId> {
    guild.members.get(&bot).map(|m| m.roles.clone()).unwrap_or_default()
}

/// The bot's permissions in `channel` (cached or fetched), computed against the cached guild.
pub fn bot_permissions_in(app: &App, guild_id: GuildId, channel: &GuildChannel) -> Option<Permissions> {
    let bot = bot_id(app)?;
    let guild = app.discord()?.cache.guild(guild_id)?;
    permissions::member_permissions_in(&guild, channel, bot, &bot_roles(&guild, bot), true)
}

/// `checkChannelUsable(channel, { needs })` for a channel that may not be in the cache.
pub fn channel_usable(app: &App, guild_id: GuildId, channel: &GuildChannel, needs: &[&str]) -> ChannelUsable {
    channel_usable_verdict(Some(channel), bot_permissions_in(app, guild_id, channel), needs)
}

/// `channel.permissionsFor(member).has(perms)` (false when it cannot be computed).
pub fn member_has_in(app: &App, guild_id: GuildId, channel: &GuildChannel, member: &Member, perms: Permissions) -> bool {
    let Some(guild) = app.discord().and_then(|d| d.cache.guild(guild_id)) else { return false };
    permissions::member_permissions_in(&guild, channel, member.user.id, &member.roles, true).is_some_and(|p| js_has(p, perms))
}

/// `channel.permissionsFor(guild.members.me).has(perms)`.
pub fn bot_has_in(app: &App, guild_id: GuildId, channel: &GuildChannel, perms: Permissions) -> bool {
    bot_permissions_in(app, guild_id, channel).is_some_and(|p| js_has(p, perms))
}

/// `pingableRoleIds(channel, executor, roleIds)`, with `executor` an id and its roles (`None`: no executor).
pub fn pingable(app: &App, guild_id: GuildId, channel: &GuildChannel, executor: Option<(UserId, Vec<RoleId>)>, role_ids: &[String]) -> Pingable {
    let Some(guild) = app.discord().and_then(|d| d.cache.guild(guild_id)) else {
        return Pingable { allowed: Vec::new(), refused: role_ids.to_vec() };
    };
    let exec = executor.as_ref().map(|(id, roles)| (*id, roles.as_slice()));
    pingable_role_ids(&guild, channel, exec, role_ids)
}

/// `pingableRoleIds(channel, guild.members.me, ids)`: the bot as executor.
pub fn pingable_by_bot(app: &App, guild_id: GuildId, channel: &GuildChannel, role_ids: &[String]) -> Pingable {
    let exec = bot_id(app).and_then(|b| app.discord()?.cache.guild(guild_id).map(|g| (b, bot_roles(&g, b))));
    match exec {
        Some(e) => pingable(app, guild_id, channel, Some(e), role_ids),
        None => pingable(app, guild_id, channel, None, role_ids),
    }
}

/// The giveaway `fetchChannel(guild, channelId)`: cache first, then REST. `Ok(None)` only on 10003 (Unknown
/// Channel); any other error is returned (a cache miss is never a deletion).
pub async fn fetch_guild_channel(app: &App, guild_id: &str, channel_id: &str) -> Result<Option<GuildChannel>, DiscordError> {
    if let (Some(g), Some(c)) = (guild_id_of(guild_id), channel_id_of(channel_id))
        && let Some(ch) = cached_channel(app, g, c)
    {
        return Ok(Some(ch));
    }
    match ops(app).fetch_channel(channel_id).await {
        Ok(Some(ch)) if ch.guild_id.to_string() == guild_id => Ok(Some(ch)),
        // A channel of another guild is not in `guild.channels`: discord.js answers Unknown Channel.
        Ok(_) => Ok(None),
        Err(e) if e.code == Some(10003) => Ok(None),
        Err(e) => Err(e),
    }
}

/// A cached guild member (`guild.members.cache.get(id)`).
pub fn cached_member(app: &App, guild_id: &str, user_id: &str) -> Option<Member> {
    let g = guild_id_of(guild_id)?;
    let u = UserId::new(snowflake(user_id)?);
    app.discord()?.cache.guild(g)?.members.get(&u).cloned()
}
