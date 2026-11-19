//! Port of `src/utils/privacyGuards.js`: access and privacy checks for features that copy or configure content across
//! channels (starboards, automations, custom commands, saved embeds). Pure and synchronous over a cached guild;
//! nothing here reads or writes anything else.

use serenity::all::{ChannelType, Guild, GuildChannel, Permissions, RoleId, UserId};

use crate::permissions::{self, everyone_role_id, js_has, permission_names};

/// `everyoneCanView(channel)`: whether `@everyone` can view it. A private thread never; any other thread follows its
/// parent. `None` when that cannot be told (no channel, or a thread whose parent is not cached).
pub fn everyone_can_view(guild: &Guild, channel: Option<&GuildChannel>) -> Option<bool> {
    let channel = channel?;
    if channel.kind == ChannelType::PrivateThread {
        return Some(false);
    }
    let everyone = guild.roles.get(&everyone_role_id(guild))?;
    permissions::role_permissions_in(guild, channel, everyone, true).map(|p| js_has(p, Permissions::VIEW_CHANNEL))
}

/// `copyLeaksPrivate(source, target)`: copying from `source` into `target` would show content to people who cannot see
/// where it came from (the source is hidden or unknown, the target visible or unknown). The same channel never leaks.
pub fn copy_leaks_private(guild: &Guild, source: Option<&GuildChannel>, target: Option<&GuildChannel>) -> bool {
    if let (Some(s), Some(t)) = (source, target)
        && s.id == t.id
    {
        return false;
    }
    everyone_can_view(guild, source) != Some(true) && everyone_can_view(guild, target) != Some(false)
}

/// The member a check is about: id, role ids and guild-level permissions (`member.permissions`).
#[derive(Debug, Clone, Copy)]
pub struct MemberRef<'a> {
    pub user_id: UserId,
    pub roles: &'a [RoleId],
    /// Guild-level permissions; `None` computes them from the guild's roles.
    pub permissions: Option<Permissions>,
}

impl<'a> MemberRef<'a> {
    pub fn of(member: &'a serenity::all::Member) -> Self {
        Self { user_id: member.user.id, roles: &member.roles, permissions: None }
    }
}

/// The default `needs` of `memberChannelProblem`.
pub const MEMBER_CHANNEL_NEEDS: [&str; 2] = ["ViewChannel", "SendMessages"];

/// `memberChannelProblem(channel, member, needs)`: the refusal text when the member cannot use the channel that way,
/// else `None`. The guild owner and Administrators are exempt; a thread swaps Send Messages for Send Messages in
/// Threads. A missing member or channel is not checked.
pub fn member_channel_problem(guild: &Guild, channel: Option<&GuildChannel>, member: Option<MemberRef<'_>>, needs: &[&str]) -> Option<String> {
    let (channel, member) = (channel?, member?);
    let guild_perms = member.permissions.unwrap_or_else(|| permissions::guild_permissions(guild, member.user_id, member.roles));
    if member.user_id == guild.owner_id || guild_perms.contains(Permissions::ADMINISTRATOR) {
        return None;
    }
    let keys: Vec<&str> =
        if permissions::is_thread(channel) { needs.iter().map(|k| if *k == "SendMessages" { "SendMessagesInThreads" } else { k }).collect() } else { needs.to_vec() };
    let perms = permissions::member_permissions_in(guild, channel, member.user_id, member.roles, true);
    let missing: Vec<&str> = match perms {
        Some(p) => keys.into_iter().filter(|k| !permissions::flag(k).is_some_and(|f| js_has(p, f))).collect(),
        None => keys,
    };
    if missing.is_empty() {
        return None;
    }
    Some(format!("You need **{}** in <#{}> to do that.", permission_names(&missing).join(", "), channel.id))
}

/// `roleHierarchyProblem(guild, member, roleIds)`: the refusal text when any role is at or above the member's highest
/// role (the guild owner is exempt), else `None`. Roles the server does not have are ignored.
pub fn role_hierarchy_problem<S: AsRef<str>>(guild: &Guild, member: Option<MemberRef<'_>>, role_ids: &[S]) -> Option<String> {
    let member = member?;
    if member.user_id == guild.owner_id || role_ids.is_empty() {
        return None;
    }
    let top = permissions::highest_position(guild, member.roles);
    role_ids.iter().map(|s| s.as_ref()).find(|id| {
        id.parse::<u64>().ok().filter(|v| *v != 0).and_then(|v| permissions::role_position(guild, RoleId::new(v))).is_some_and(|p| p >= top)
    })
    .map(|id| format!("<@&{id}> is at or above your highest role."))
}
