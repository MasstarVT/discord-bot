//! Serenity models as the plain views [`crate::log_events`] reads, with discord.js's defaults: `displayAvatarURL()`
//! and `imageURL()` are `.webp` (`.gif` for an animated hash) without a size, `displayAvatarURL({ size: 256 })` adds
//! `?size=256`, `user.tag` is the username for migrated accounts, `role.hexColor` is lower-case `#rrggbb`.

use std::collections::HashSet;

use bot_core::App;
use serenity::all::{
    ChannelId, Emoji, Guild, GuildChannel, GuildId, InviteCreateEvent, Member, Message, PartialGuild, PermissionOverwriteType, Role, Timestamp, User,
    VoiceState,
};

use crate::log_events::{
    LogAttachment, LogChannel, LogEmoji, LogGuild, LogInvite, LogMember, LogMemberRole, LogMessage, LogOverwrite, LogRole, LogRoleRef, LogTag,
    LogThread, LogUser, LogVoiceState,
};

const CDN: &str = "https://cdn.discordapp.com";

fn ms(t: &Timestamp) -> i64 {
    t.timestamp_millis()
}

fn dynamic(route: &str, hash: &str, size: Option<u32>) -> String {
    let ext = if hash.starts_with("a_") { "gif" } else { "webp" };
    match size {
        Some(s) => format!("{CDN}{route}.{ext}?size={s}"),
        None => format!("{CDN}{route}.{ext}"),
    }
}

/// `user.displayAvatarURL(size ? { size } : undefined)`.
pub fn display_avatar_url(user: &User, size: Option<u32>) -> String {
    match &user.avatar {
        Some(h) => dynamic(&format!("/avatars/{}/{h}", user.id), &h.to_string(), size),
        None => bot_core::adapters::default_avatar_url(user.id, &bot_core::adapters::discriminator(user)),
    }
}

/// `member.avatarURL({ size: 256 })` (the server avatar), or `None`.
pub fn member_avatar_url(guild_id: GuildId, member: &Member) -> Option<String> {
    member.avatar.as_ref().map(|h| dynamic(&format!("/guilds/{guild_id}/users/{}/avatars/{h}", member.user.id), &h.to_string(), Some(256)))
}

/// `guild.iconURL()`.
pub fn guild_icon_url(guild_id: GuildId, icon: Option<&serenity::all::ImageHash>) -> Option<String> {
    icon.map(|h| dynamic(&format!("/icons/{guild_id}/{h}"), &h.to_string(), None))
}

/// A user as [`LogUser`].
pub fn log_user(user: &User) -> LogUser {
    LogUser {
        id: user.id.to_string(),
        username: Some(user.name.clone()),
        tag: Some(bot_core::adapters::user_tag(user)),
        global_name: user.global_name.clone(),
        bot: user.bot,
        created_timestamp: Some(bot_core::adapters::snowflake_ms(user.id.get()) as i64),
        avatar_url: Some(display_avatar_url(user, None)),
        avatar_url256: Some(display_avatar_url(user, Some(256))),
    }
}

/// The ids of the guild's cached channels and threads (`guild.channels.cache`), `None` when the guild is not cached.
pub fn known_channels(app: &App, guild_id: GuildId) -> Option<HashSet<String>> {
    let d = app.discord()?;
    let g = d.cache.guild(guild_id)?;
    Some(g.channels.keys().map(|c| c.to_string()).chain(g.threads.iter().map(|t| t.id.to_string())).collect())
}

/// A message as [`LogMessage`]. Role mentions are named from the guild's cache (unknown roles are skipped, as
/// discord.js's `mentions.roles` does).
pub fn log_message(app: &App, message: &Message) -> LogMessage {
    let guild = message.guild_id.and_then(|g| bot_core::interaction_helpers::cached_guild(app, g));
    let mention_roles = message
        .mention_roles
        .iter()
        .filter_map(|r| guild.as_ref().and_then(|g| g.roles.get(r)).map(|role| LogRoleRef { id: r.to_string(), name: role.name.clone() }))
        .collect();
    LogMessage {
        id: message.id.to_string(),
        partial: false,
        content: Some(message.content.clone()),
        snapshot_contents: message.message_snapshots.iter().map(|s| s.content.clone()).collect(),
        author: Some(log_user(&message.author)),
        channel_id: Some(message.channel_id.to_string()),
        webhook_id: message.webhook_id.map(|w| w.to_string()),
        created_timestamp: Some(ms(&message.timestamp)),
        edited_timestamp: message.edited_timestamp.as_ref().map(ms),
        attachments: message
            .attachments
            .iter()
            .map(|a| LogAttachment { name: Some(a.filename.clone()), size: Some(u64::from(a.size)), url: Some(a.url.clone()) })
            .collect(),
        embed_count: message.embeds.len(),
        url: Some(message.link()),
        mention_users: message.mentions.iter().map(log_user).collect(),
        mention_roles,
        mention_everyone: message.mention_everyone,
    }
}

/// A member as [`LogMember`] (roles with their positions from the guild cache).
pub fn log_member(app: &App, guild_id: GuildId, member: &Member) -> LogMember {
    let guild = bot_core::interaction_helpers::cached_guild(app, guild_id);
    LogMember {
        id: member.user.id.to_string(),
        user: Some(log_user(&member.user)),
        guild_id: Some(guild_id.to_string()),
        nickname: member.nick.clone(),
        avatar: member.avatar.map(|h| h.to_string()),
        avatar_url256: member_avatar_url(guild_id, member),
        partial: false,
        joined_timestamp: member.joined_at.as_ref().map(ms),
        // discord.js `member.roles.cache`: the member's roles the guild cache has, each with its rank (`role.position`:
        // raw position, ties broken by id), not the raw API position.
        roles: match guild.as_ref() {
            Some(g) => member
                .roles
                .iter()
                .filter_map(|r| bot_core::permissions::role_position(g, *r).map(|p| LogMemberRole { id: r.to_string(), position: p as i64 }))
                .collect(),
            None => member.roles.iter().map(|r| LogMemberRole { id: r.to_string(), position: 0 }).collect(),
        },
        timeout_until: member.communication_disabled_until.as_ref().map(ms),
    }
}

/// A user whose member was not cached (a partial member: only the user is known).
pub fn partial_member(guild_id: GuildId, user: &User) -> LogMember {
    LogMember { id: user.id.to_string(), user: Some(log_user(user)), guild_id: Some(guild_id.to_string()), partial: true, ..Default::default() }
}

/// A voice state as [`LogVoiceState`].
pub fn log_voice_state(v: &VoiceState) -> LogVoiceState {
    LogVoiceState {
        channel_id: v.channel_id.map(|c| c.to_string()),
        server_mute: v.mute,
        server_deaf: v.deaf,
        self_deaf: v.self_deaf,
        self_mute: v.self_mute,
        streaming: v.self_stream.unwrap_or(false),
    }
}

fn parent_name(app: &App, guild_id: GuildId, parent: Option<ChannelId>) -> Option<String> {
    parent.and_then(|p| bot_core::interaction_helpers::cached_channel(app, guild_id, p)).map(|c| c.name)
}

fn is_voice_based(kind: u8) -> bool {
    kind == 2 || kind == 13
}

/// A guild channel as [`LogChannel`].
pub fn log_channel(app: &App, guild_id: GuildId, c: &GuildChannel) -> LogChannel {
    let kind = u8::from(c.kind);
    LogChannel {
        id: c.id.to_string(),
        name: c.name.clone(),
        kind,
        topic: c.topic.clone(),
        nsfw: c.nsfw,
        rate_limit_per_user: c.rate_limit_per_user.map(u32::from),
        parent_id: c.parent_id.map(|p| p.to_string()),
        parent_name: parent_name(app, guild_id, c.parent_id),
        bitrate: if is_voice_based(kind) { c.bitrate } else { None },
        user_limit: if is_voice_based(kind) { Some(c.user_limit.unwrap_or(0)) } else { None },
        rtc_region: if is_voice_based(kind) { Some(c.rtc_region.clone()) } else { None },
        overwrites: c
            .permission_overwrites
            .iter()
            .map(|o| {
                let (id, kind) = match o.kind {
                    PermissionOverwriteType::Member(u) => (u.to_string(), 1),
                    PermissionOverwriteType::Role(r) => (r.to_string(), 0),
                    _ => (String::new(), 0),
                };
                LogOverwrite { id, kind, allow: o.allow.bits(), deny: o.deny.bits() }
            })
            .collect(),
    }
}

/// A thread as [`LogThread`] (forum tags named from the cached parent).
pub fn log_thread(app: &App, guild_id: GuildId, t: &GuildChannel) -> LogThread {
    let parent = t.parent_id.and_then(|p| bot_core::interaction_helpers::cached_channel(app, guild_id, p));
    LogThread {
        id: t.id.to_string(),
        name: Some(t.name.clone()),
        kind: u8::from(t.kind),
        parent_id: t.parent_id.map(|p| p.to_string()),
        owner_id: t.owner_id.map(|o| o.to_string()),
        auto_archive_duration: t.thread_metadata.as_ref().map(|m| u32::from(u16::from(m.auto_archive_duration))),
        rate_limit_per_user: Some(u32::from(t.rate_limit_per_user.unwrap_or(0))),
        locked: t.thread_metadata.as_ref().map(|m| m.locked),
        invitable: if u8::from(t.kind) == 12 { t.thread_metadata.as_ref().map(|m| m.invitable) } else { None },
        applied_tags: Some(t.applied_tags.iter().map(|x| x.to_string()).collect()),
        available_tags: parent.map(|p| p.available_tags.iter().map(|x| LogTag { id: x.id.to_string(), name: x.name.clone() }).collect()).unwrap_or_default(),
    }
}

/// A thread known only by its ids (an uncached deleted thread).
pub fn bare_thread(id: ChannelId, parent: Option<ChannelId>) -> LogThread {
    LogThread { id: id.to_string(), kind: 11, parent_id: parent.map(|p| p.to_string()), ..Default::default() }
}

/// A role as [`LogRole`].
pub fn log_role(role: &Role) -> LogRole {
    LogRole {
        id: role.id.to_string(),
        name: role.name.clone(),
        hex_color: Some(format!("#{:06x}", role.colour.0)),
        hoist: role.hoist,
        mentionable: role.mentionable,
        icon: role.icon.map(|h| h.to_string()),
        unicode_emoji: role.unicode_emoji.clone(),
        managed: role.managed,
        permissions: role.permissions.bits(),
        created_timestamp: Some(bot_core::adapters::snowflake_ms(role.id.get()) as i64),
    }
}

/// The cached (old) guild as [`LogGuild`].
pub fn log_guild(g: &Guild) -> LogGuild {
    LogGuild {
        id: g.id.to_string(),
        name: Some(g.name.clone()),
        icon: g.icon.map(|h| h.to_string()),
        banner: g.banner.clone(),
        splash: g.splash.map(|h| h.to_string()),
        description: g.description.clone(),
        owner_id: Some(g.owner_id.to_string()),
        verification_level: Some(u8::from(g.verification_level)),
        explicit_content_filter: Some(u8::from(g.explicit_content_filter)),
        default_message_notifications: Some(u8::from(g.default_message_notifications)),
        afk_channel_id: g.afk_metadata.as_ref().map(|a| a.afk_channel_id.to_string()),
        afk_timeout: Some(g.afk_metadata.as_ref().map(|a| u64::from(u16::from(a.afk_timeout))).unwrap_or(300)),
        system_channel_id: g.system_channel_id.map(|c| c.to_string()),
        rules_channel_id: g.rules_channel_id.map(|c| c.to_string()),
        public_updates_channel_id: g.public_updates_channel_id.map(|c| c.to_string()),
        vanity_url_code: g.vanity_url_code.clone(),
        preferred_locale: Some(g.preferred_locale.clone()),
        premium_tier: Some(u8::from(g.premium_tier)),
        mfa_level: Some(u8::from(g.mfa_level)),
        nsfw_level: Some(u8::from(g.nsfw_level)),
        premium_progress_bar_enabled: Some(g.premium_progress_bar_enabled),
        icon_url: guild_icon_url(g.id, g.icon.as_ref()),
    }
}

/// The updated guild (GUILD_UPDATE's partial guild) as [`LogGuild`].
pub fn log_partial_guild(g: &PartialGuild) -> LogGuild {
    LogGuild {
        id: g.id.to_string(),
        name: Some(g.name.clone()),
        icon: g.icon.map(|h| h.to_string()),
        banner: g.banner.clone(),
        splash: g.splash.map(|h| h.to_string()),
        description: g.description.clone(),
        owner_id: Some(g.owner_id.to_string()),
        verification_level: Some(u8::from(g.verification_level)),
        explicit_content_filter: Some(u8::from(g.explicit_content_filter)),
        default_message_notifications: Some(u8::from(g.default_message_notifications)),
        afk_channel_id: g.afk_metadata.as_ref().map(|a| a.afk_channel_id.to_string()),
        afk_timeout: Some(g.afk_metadata.as_ref().map(|a| u64::from(u16::from(a.afk_timeout))).unwrap_or(300)),
        system_channel_id: g.system_channel_id.map(|c| c.to_string()),
        rules_channel_id: g.rules_channel_id.map(|c| c.to_string()),
        public_updates_channel_id: g.public_updates_channel_id.map(|c| c.to_string()),
        vanity_url_code: g.vanity_url_code.clone(),
        preferred_locale: Some(g.preferred_locale.clone()),
        premium_tier: Some(u8::from(g.premium_tier)),
        mfa_level: Some(u8::from(g.mfa_level)),
        nsfw_level: Some(u8::from(g.nsfw_level)),
        premium_progress_bar_enabled: Some(g.premium_progress_bar_enabled),
        icon_url: guild_icon_url(g.id, g.icon.as_ref()),
    }
}

/// An emoji as [`LogEmoji`].
pub fn log_emoji(e: &Emoji) -> LogEmoji {
    LogEmoji {
        id: e.id.to_string(),
        name: e.name.clone(),
        animated: e.animated,
        roles: e.roles.iter().map(|r| r.to_string()).collect(),
        image_url: Some(format!("{CDN}/emojis/{}.webp", e.id)),
        mention: format!("<{}:{}:{}>", if e.animated { "a" } else { "" }, e.name, e.id),
    }
}

/// An INVITE_CREATE event as [`LogInvite`].
pub fn log_invite_create(i: &InviteCreateEvent) -> LogInvite {
    let created = ms(&i.created_at);
    LogInvite {
        code: i.code.clone(),
        channel_id: Some(i.channel_id.to_string()),
        inviter: i.inviter.as_ref().map(log_user),
        max_uses: Some(u64::from(i.max_uses)),
        expires_timestamp: if i.max_age > 0 { Some(created + i64::from(i.max_age) * 1000) } else { None },
        temporary: i.temporary,
    }
}
