//! Port of `src/utils/permissionChecker.js`: the permission level of a guild member.
//!
//! OWNER = `BOT_OWNER_ID`; ADMIN = guild owner, Administrator or Manage Server; MODERATOR = a role in
//! `GuildSettings.modRoleIds`, or Kick / Ban / Moderate Members. The level is computed from **guild-level** permissions
//! (discord.js `member.permissions`), never from the channel-resolved permissions Discord sends with an interaction:
//! [`MemberAccess::from_cache`] reads them from the serenity cache.

use bot_config::PermissionLevel;
use bot_db::GuildSettings;
use serenity::all::{Cache, GuildId, Member, Permissions};

/// What the level is computed from: a plain view of a guild member.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemberAccess {
    pub user_id: String,
    /// The guild's owner (`member.guild.ownerId`), when known.
    pub guild_owner_id: Option<String>,
    /// Guild-level permissions (`member.permissions`); `None` when unknown (no bit counts then).
    pub permissions: Option<Permissions>,
    /// The member's role ids (`member.roles.cache`).
    pub role_ids: Vec<String>,
}

impl MemberAccess {
    /// Builds the view from a serenity member. Permissions come from the cached guild (guild-level, owner and
    /// Administrator included); when the guild is not cached, the permissions Discord sent with the member (present on
    /// interaction members) are used. With the cached guild the role list is discord.js `member.roles.cache` (@everyone
    /// first, roles the guild no longer has dropped).
    pub fn from_cache(cache: Option<&Cache>, guild_id: GuildId, member: &Member) -> Self {
        let mut access = Self {
            user_id: member.user.id.to_string(),
            guild_owner_id: None,
            permissions: member.permissions,
            role_ids: member.roles.iter().map(|r| r.to_string()).collect(),
        };
        if let Some(guild) = cache.and_then(|c| c.guild(guild_id)) {
            access.guild_owner_id = Some(guild.owner_id.to_string());
            access.permissions = Some(guild.member_permissions(member));
            // `member.roles.cache`: @everyone included, roles the guild no longer has dropped.
            access.role_ids = crate::interaction_helpers::member_role_ids(&guild, member).iter().map(|r| r.to_string()).collect();
        }
        access
    }
}

/// `resolvePermissionLevel(member, guildSettings)`. `bot_owner_id` is `BOT_OWNER_ID` (see [`crate::AppConfig`]).
pub fn resolve_permission_level(member: Option<&MemberAccess>, settings: Option<&GuildSettings>, bot_owner_id: Option<&str>) -> PermissionLevel {
    let Some(member) = member else { return PermissionLevel::Member };
    if bot_owner_id.is_some_and(|owner| owner == member.user_id) {
        return PermissionLevel::Owner;
    }
    let has = |p: Permissions| member.permissions.is_some_and(|perms| perms.contains(p));
    if member.guild_owner_id.as_deref() == Some(member.user_id.as_str()) {
        return PermissionLevel::Admin;
    }
    if has(Permissions::ADMINISTRATOR) || has(Permissions::MANAGE_GUILD) {
        return PermissionLevel::Admin;
    }
    if let Some(s) = settings
        && s.mod_role_ids.iter().any(|id| member.role_ids.iter().any(|r| r == id))
    {
        return PermissionLevel::Moderator;
    }
    if has(Permissions::KICK_MEMBERS) || has(Permissions::BAN_MEMBERS) || has(Permissions::MODERATE_MEMBERS) {
        return PermissionLevel::Moderator;
    }
    PermissionLevel::Member
}

/// `hasPermissionLevel(member, required, guildSettings)`.
pub fn has_permission_level(member: Option<&MemberAccess>, required: PermissionLevel, settings: Option<&GuildSettings>, bot_owner_id: Option<&str>) -> bool {
    resolve_permission_level(member, settings, bot_owner_id) >= required
}
