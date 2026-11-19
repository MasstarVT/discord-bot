//! discord.js permission semantics on serenity models: the `PermissionFlagsBits` names, `PermissionsBitField#has`
//! (Administrator satisfies everything), `toArray`, and `GuildChannel#permissionsFor` / `GuildMember#permissions`.
//!
//! Serenity computes channel permissions its own way (it strips "unusable" permissions, for example everything when
//! View Channel is missing). The JS checks (`missingBotPermissions`, `memberChannelProblem`, `everyoneCanView`, ...)
//! use discord.js's algorithm, so these functions reproduce it exactly: guild owner or Administrator is every
//! permission; otherwise the roles' permissions (with `@everyone`), then the `@everyone` overwrite, the member's role
//! overwrites (all denies, then all allows), and the member overwrite. A thread uses its parent channel.

use serenity::all::{ChannelId, ChannelType, Guild, GuildChannel, Member, PermissionOverwriteType, Permissions, Role, RoleId, UserId};

/// `PermissionFlagsBits` in discord.js order (names and bits). `ManageEmojisAndStickers` and
/// `ManageGuildExpressions` share a bit, as in discord.js.
pub const PERMISSION_FLAGS: &[(&str, u64)] = &[
    ("CreateInstantInvite", 1),
    ("KickMembers", 1 << 1),
    ("BanMembers", 1 << 2),
    ("Administrator", 1 << 3),
    ("ManageChannels", 1 << 4),
    ("ManageGuild", 1 << 5),
    ("AddReactions", 1 << 6),
    ("ViewAuditLog", 1 << 7),
    ("PrioritySpeaker", 1 << 8),
    ("Stream", 1 << 9),
    ("ViewChannel", 1 << 10),
    ("SendMessages", 1 << 11),
    ("SendTTSMessages", 1 << 12),
    ("ManageMessages", 1 << 13),
    ("EmbedLinks", 1 << 14),
    ("AttachFiles", 1 << 15),
    ("ReadMessageHistory", 1 << 16),
    ("MentionEveryone", 1 << 17),
    ("UseExternalEmojis", 1 << 18),
    ("ViewGuildInsights", 1 << 19),
    ("Connect", 1 << 20),
    ("Speak", 1 << 21),
    ("MuteMembers", 1 << 22),
    ("DeafenMembers", 1 << 23),
    ("MoveMembers", 1 << 24),
    ("UseVAD", 1 << 25),
    ("ChangeNickname", 1 << 26),
    ("ManageNicknames", 1 << 27),
    ("ManageRoles", 1 << 28),
    ("ManageWebhooks", 1 << 29),
    ("ManageEmojisAndStickers", 1 << 30),
    ("ManageGuildExpressions", 1 << 30),
    ("UseApplicationCommands", 1 << 31),
    ("RequestToSpeak", 1 << 32),
    ("ManageEvents", 1 << 33),
    ("ManageThreads", 1 << 34),
    ("CreatePublicThreads", 1 << 35),
    ("CreatePrivateThreads", 1 << 36),
    ("UseExternalStickers", 1 << 37),
    ("SendMessagesInThreads", 1 << 38),
    ("UseEmbeddedActivities", 1 << 39),
    ("ModerateMembers", 1 << 40),
    ("ViewCreatorMonetizationAnalytics", 1 << 41),
    ("UseSoundboard", 1 << 42),
    ("CreateGuildExpressions", 1 << 43),
    ("CreateEvents", 1 << 44),
    ("UseExternalSounds", 1 << 45),
    ("SendVoiceMessages", 1 << 46),
    ("SetVoiceChannelStatus", 1 << 48),
    ("SendPolls", 1 << 49),
    ("UseExternalApps", 1 << 50),
    ("PinMessages", 1 << 51),
    ("BypassSlowmode", 1 << 52),
];

/// `PermissionsBitField.All`: every bit discord.js knows.
pub fn all_bits() -> Permissions {
    Permissions::from_bits_retain(PERMISSION_FLAGS.iter().fold(0, |acc, (_, b)| acc | b))
}

/// `PermissionFlagsBits[key]`.
pub fn flag(key: &str) -> Option<Permissions> {
    PERMISSION_FLAGS.iter().find(|(k, _)| *k == key).map(|(_, b)| Permissions::from_bits_retain(*b))
}

/// `Object.hasOwn(PermissionFlagsBits, key)`.
pub fn is_flag(key: &str) -> bool {
    flag(key).is_some()
}

/// `bits.has(flag)` (checkAdmin = true): Administrator has every permission.
pub fn js_has(bits: Permissions, needed: Permissions) -> bool {
    bits.contains(Permissions::ADMINISTRATOR) || bits.contains(needed)
}

/// `bits.has(flag, false)`: only the bits really set.
pub fn has_exact(bits: Permissions, needed: Permissions) -> bool {
    bits.contains(needed)
}

/// `new PermissionsBitField(bits).toArray()`: every key whose bit is set (`PermissionsBitField#toArray` checks without
/// the Administrator shortcut).
pub fn to_array(bits: Permissions) -> Vec<&'static str> {
    PERMISSION_FLAGS.iter().filter(|(_, b)| has_exact(bits, Permissions::from_bits_retain(*b))).map(|(k, _)| *k).collect()
}

/// The display labels that differ from splitting the key (`PERMISSION_LABELS`).
const PERMISSION_LABELS: &[(&str, &str)] = &[
    ("ManageGuild", "Manage Server"),
    ("ModerateMembers", "Timeout Members"),
    ("CreateInstantInvite", "Create Invite"),
    ("ManageGuildExpressions", "Manage Expressions"),
    ("ManageEmojisAndStickers", "Manage Emojis and Stickers"),
    ("MentionEveryone", "Mention @everyone"),
    ("SendTTSMessages", "Send TTS Messages"),
    ("UseVAD", "Use Voice Activity"),
    ("ViewAuditLog", "View Audit Log"),
    ("SendMessagesInThreads", "Send Messages in Threads"),
    ("UseApplicationCommands", "Use Application Commands"),
];

/// One key's label: `PERMISSION_LABELS[k] ?? k.replace(/([a-z])([A-Z])/g, '$1 $2')`.
pub fn permission_label(key: &str) -> String {
    if let Some((_, l)) = PERMISSION_LABELS.iter().find(|(k, _)| *k == key) {
        return (*l).to_string();
    }
    let chars: Vec<char> = key.chars().collect();
    let mut out = String::with_capacity(key.len() + 4);
    let mut i = 0;
    while i < chars.len() {
        out.push(chars[i]);
        // The regex consumes both characters, so "aBC" splits once ("a BC").
        if chars[i].is_ascii_lowercase() && chars.get(i + 1).is_some_and(|c| c.is_ascii_uppercase()) {
            out.push(' ');
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        i += 1;
    }
    out
}

/// `permissionNames(keys)`: labels of `PermissionFlagsBits` keys (unknown keys dropped), unique, in input order.
pub fn permission_names<S: AsRef<str>>(keys: &[S]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for k in keys {
        let k = k.as_ref();
        if !is_flag(k) {
            continue;
        }
        let label = permission_label(k);
        if !out.contains(&label) {
            out.push(label);
        }
    }
    out
}

/// `permissionNames(bitfield)`: labels of every flag the bitfield `has()`.
pub fn permission_names_of_bits(bits: Permissions) -> Vec<String> {
    permission_names(&to_array(bits))
}

/// Discord's `@everyone` role id (the guild id).
pub fn everyone_role_id(guild: &Guild) -> RoleId {
    RoleId::new(guild.id.get())
}

/// A channel or thread of the guild.
pub fn find_channel(guild: &Guild, id: ChannelId) -> Option<&GuildChannel> {
    guild.channels.get(&id).or_else(|| guild.threads.iter().find(|t| t.id == id))
}

/// Whether the channel is a thread (`channel.isThread()`).
pub fn is_thread(channel: &GuildChannel) -> bool {
    matches!(channel.kind, ChannelType::PublicThread | ChannelType::PrivateThread | ChannelType::NewsThread)
}

/// `channel.isTextBased()`: text, announcement, voice and stage channels and threads have messages.
pub fn is_text_based(channel: &GuildChannel) -> bool {
    matches!(
        channel.kind,
        ChannelType::Text
            | ChannelType::News
            | ChannelType::Voice
            | ChannelType::Stage
            | ChannelType::PublicThread
            | ChannelType::PrivateThread
            | ChannelType::NewsThread
    )
}

/// The channel whose overwrites apply: a thread's parent (`ThreadChannel#permissionsFor` defers to it), else itself.
/// `None` for a thread whose parent is not cached (discord.js returns null).
pub fn overwrite_channel<'a>(guild: &'a Guild, channel: &'a GuildChannel) -> Option<&'a GuildChannel> {
    if is_thread(channel) { channel.parent_id.and_then(|p| guild.channels.get(&p)) } else { Some(channel) }
}

fn role_perms(guild: &Guild, id: &RoleId) -> Permissions {
    guild.roles.get(id).map(|r| r.permissions).unwrap_or_else(Permissions::empty)
}

/// `GuildMember#permissions`: guild owner is everything; otherwise the OR of the member's roles and `@everyone`.
pub fn guild_permissions(guild: &Guild, user_id: UserId, roles: &[RoleId]) -> Permissions {
    if user_id == guild.owner_id {
        return all_bits();
    }
    let mut p = role_perms(guild, &everyone_role_id(guild));
    for r in roles {
        p |= role_perms(guild, r);
    }
    p
}

/// `GuildChannel#permissionsFor(member, checkAdmin)` (`memberPermissions`). `None` for a thread without a cached parent.
pub fn member_permissions_in(guild: &Guild, channel: &GuildChannel, user_id: UserId, roles: &[RoleId], check_admin: bool) -> Option<Permissions> {
    let channel = overwrite_channel(guild, channel)?;
    if check_admin && user_id == guild.owner_id {
        return Some(all_bits());
    }
    let mut perms = role_perms(guild, &everyone_role_id(guild));
    for r in roles {
        perms |= role_perms(guild, r);
    }
    if check_admin && perms.contains(Permissions::ADMINISTRATOR) {
        return Some(all_bits());
    }
    let everyone = everyone_role_id(guild);
    let (mut ev_allow, mut ev_deny) = (Permissions::empty(), Permissions::empty());
    let (mut role_allow, mut role_deny) = (Permissions::empty(), Permissions::empty());
    let (mut mem_allow, mut mem_deny) = (Permissions::empty(), Permissions::empty());
    for ow in &channel.permission_overwrites {
        let id = match ow.kind {
            PermissionOverwriteType::Role(r) => r.get(),
            PermissionOverwriteType::Member(u) => u.get(),
            _ => continue,
        };
        if id == everyone.get() {
            ev_allow = ow.allow;
            ev_deny = ow.deny;
        } else if roles.iter().any(|r| r.get() == id) {
            role_allow |= ow.allow;
            role_deny |= ow.deny;
        } else if id == user_id.get() {
            mem_allow = ow.allow;
            mem_deny = ow.deny;
        }
    }
    perms = (perms & !ev_deny) | ev_allow;
    perms = (perms & !role_deny) | role_allow;
    perms = (perms & !mem_deny) | mem_allow;
    Some(perms)
}

/// `channel.permissionsFor(member)` for a cached member.
pub fn member_channel_permissions(guild: &Guild, channel: &GuildChannel, member: &Member) -> Option<Permissions> {
    member_permissions_in(guild, channel, member.user.id, &member.roles, true)
}

/// `channel.permissionsFor(role, checkAdmin)` (`rolePermissions`): the role plus `@everyone`, then the `@everyone`
/// and the role's overwrites. `None` for a thread without a cached parent.
pub fn role_permissions_in(guild: &Guild, channel: &GuildChannel, role: &Role, check_admin: bool) -> Option<Permissions> {
    let channel = overwrite_channel(guild, channel)?;
    if check_admin && role.permissions.contains(Permissions::ADMINISTRATOR) {
        return Some(all_bits());
    }
    let mut p = role.permissions | role_perms(guild, &everyone_role_id(guild));
    let find = |id: u64| {
        channel.permission_overwrites.iter().find(|ow| match ow.kind {
            PermissionOverwriteType::Role(r) => r.get() == id,
            PermissionOverwriteType::Member(u) => u.get() == id,
            _ => false,
        })
    };
    if let Some(ow) = find(guild.id.get()) {
        p = (p & !ow.deny) | ow.allow;
    }
    if let Some(ow) = find(role.id.get()) {
        p = (p & !ow.deny) | ow.allow;
    }
    Some(p)
}

/// discord.js `Role#position`: the role's rank among the guild's roles (raw position, ties broken by id: the older
/// role ranks higher). `None` for a role the guild does not have.
pub fn role_position(guild: &Guild, role_id: RoleId) -> Option<usize> {
    let me = guild.roles.get(&role_id)?;
    Some(
        guild
            .roles
            .values()
            .filter(|r| if me.position == r.position { me.id.get() < r.id.get() } else { me.position > r.position })
            .count(),
    )
}

/// `member.roles.highest.position`: the highest rank among the member's roles and `@everyone`.
pub fn highest_position(guild: &Guild, roles: &[RoleId]) -> usize {
    std::iter::once(everyone_role_id(guild)).chain(roles.iter().copied()).filter_map(|r| role_position(guild, r)).max().unwrap_or(0)
}

/// The bot's member in a cached guild (`guild.members.me`).
pub fn me(guild: &Guild, bot_id: UserId) -> Option<&Member> {
    guild.members.get(&bot_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_follow_the_js_regex() {
        assert_eq!(permission_label("ManageRoles"), "Manage Roles");
        assert_eq!(permission_label("ManageGuild"), "Manage Server");
        assert_eq!(permission_label("SendTTSMessages"), "Send TTS Messages");
        assert_eq!(permission_label("ViewChannel"), "View Channel");
        assert_eq!(permission_label("UseVAD"), "Use Voice Activity");
        assert_eq!(permission_names(&["ManageRoles", "ManageRoles", "Nope", "ViewChannel"]), vec!["Manage Roles", "View Channel"]);
    }

    #[test]
    fn administrator_has_everything() {
        let admin = Permissions::ADMINISTRATOR;
        assert!(js_has(admin, Permissions::MANAGE_ROLES));
        assert!(!has_exact(admin, Permissions::MANAGE_ROLES));
        assert_eq!(to_array(admin), vec!["Administrator"]);
    }
}
