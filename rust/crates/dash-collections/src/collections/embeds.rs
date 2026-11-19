//! Port of `src/dashboard/collections/embeds.js`: the `embeds` collection (saved embeds) and the pure helpers the
//! collections-core stores share: rendering a template into a Discord REST message without a gateway client, the
//! role-ping and role-assignment policies the slash commands apply to the member running them, and the mapping of
//! Discord REST failures to RecordErrors.

use bot_util::placeholder_parser::{PlaceholderChannel, PlaceholderGuild, PlaceholderUser, RenderContext};
use serde_json::{Map, Value, json};

use crate::discord::{DiscordApiError, DiscordResources, ResRole};
use crate::errors::RecordError;
use crate::perms::{flag_bit, flags, has_flag};
use crate::store::{DashboardMember, DashboardUser, StoreCtx, StoreError};
use crate::util::{raw, strip_undefined, iso, is_snowflake, is_snowflake_value, or_null, slice16, truthy};

/// Channel kinds a bot message can be posted in (text and announcement channels, as the slash commands allow).
pub const POSTABLE_CHANNEL_KINDS: [&str; 2] = ["text", "announcement"];

/// A name slug: what normalizeEmbedName and sanitizeName leave unchanged (RegExp source for the `pattern` key).
pub const SLUG_PATTERN: &str = "(?!-)(?!.*--)[a-z0-9_-]{1,32}(?<!-)";
pub const SLUG_MESSAGE: &str = "Use lowercase letters, numbers, - and _ only (up to 32 characters), e.g. server-rules.";

const CDN: &str = "https://cdn.discordapp.com";

fn is_image_hash(s: &str) -> bool {
    let h = s.strip_prefix("a_").unwrap_or(s);
    h.len() == 32 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `messageUrl(guildId, channelId, messageId)`: the jump link of a posted message, or None.
pub fn message_url(guild_id: Option<&Value>, channel_id: Option<&Value>, message_id: Option<&Value>) -> Option<String> {
    if is_snowflake_value(guild_id) && is_snowflake_value(channel_id) && is_snowflake_value(message_id) {
        let s = |v: Option<&Value>| crate::util::js_string(v);
        Some(format!("https://discord.com/channels/{}/{}/{}", s(guild_id), s(channel_id), s(message_id)))
    } else {
        None
    }
}

/// [`message_url`] for plain strings.
pub fn message_url_str(guild_id: &str, channel_id: &str, message_id: &str) -> Option<String> {
    (is_snowflake(guild_id) && is_snowflake(channel_id) && is_snowflake(message_id))
        .then(|| format!("https://discord.com/channels/{guild_id}/{channel_id}/{message_id}"))
}

/// `embedItem(row)`: a SavedEmbed row as a collection item (wire shapes).
pub fn embed_item(row: &Value) -> Value {
    let posted = truthy(row.get("messageId")) && truthy(row.get("channelId"));
    strip_undefined(json!({
        "id": raw(row.get("id")),
        "name": raw(row.get("name")),
        "template": bot_core::custom_embed::normalize_template(row.get("template").unwrap_or(&Value::Null)),
        "status": if posted { "Posted" } else { "Draft" },
        "channelId": or_null(row.get("channelId")),
        "messageId": or_null(row.get("messageId")),
        "messageUrl": if posted { message_url(row.get("guildId"), row.get("channelId"), row.get("messageId")).map_or(Value::Null, Value::String) } else { Value::Null },
        "updatedAt": iso(row.get("updatedAt")),
    }))
}

/// `guildIconUrl(guild)` of embeds.js: animated icons as GIF like discord.js's iconURL(); None without an icon.
pub fn guild_icon_url(id: &str, icon: Option<&str>) -> Option<String> {
    let icon = icon.filter(|i| is_snowflake(id) && is_image_hash(i))?;
    let ext = if icon.starts_with("a_") { "gif" } else { "png" };
    Some(format!("{CDN}/icons/{id}/{icon}.{ext}?size=256"))
}

/// `userAvatarUrl(user)` of embeds.js: the default avatar when there is none.
pub fn user_avatar_url(id: &str, avatar: Option<&str>) -> String {
    let id = if is_snowflake(id) { id } else { "0" };
    if let Some(a) = avatar.filter(|a| is_image_hash(a)) {
        let ext = if a.starts_with("a_") { "gif" } else { "png" };
        return format!("{CDN}/avatars/{id}/{a}.{ext}?size=256");
    }
    let n: u128 = id.parse().unwrap_or(0);
    format!("{CDN}/embed/avatars/{}.png", (n >> 22) % 6)
}

/// The creation time (ms) encoded in a snowflake.
fn snowflake_time(id: &str) -> Option<f64> {
    if !is_snowflake(id) {
        return None;
    }
    let n: u128 = id.parse().ok()?;
    Some(((n >> 22) + 1_420_070_400_000) as f64)
}

/// `renderContextFor(resources, { channelId, user, userId })`: the placeholder render context for a message the
/// dashboard posts. The guild and channel come from getGuildResources, {user} is the dashboard user (no member).
pub fn render_context_for(
    resources: Option<&DiscordResources>,
    channel_id: Option<&str>,
    user: Option<&DashboardUser>,
    user_id: Option<&str>,
) -> RenderContext {
    let g = resources.map(|r| &r.guild);
    let gid = g.map(|g| g.id.clone()).unwrap_or_default();
    let guild = PlaceholderGuild {
        id: gid.clone(),
        name: Some(g.map(|g| g.name.clone()).unwrap_or_default()),
        owner_id: g.and_then(|g| g.owner_id.clone()),
        member_count: Some(g.and_then(|g| g.member_count).unwrap_or(0) as f64),
        verification_level: Some(g.map_or(0, |g| g.verification_level).to_string()),
        joined_timestamp: None,
        created_timestamp: snowflake_time(&gid),
        premium_subscription_count: Some(g.map_or(0, |g| g.premium_subscription_count).to_string()),
        premium_tier: Some(g.map_or(0, |g| g.premium_tier).to_string()),
        icon_url: g.and_then(|g| guild_icon_url(&g.id, g.icon.as_deref())),
    };
    let channel = channel_id.filter(|c| !c.is_empty()).map(|cid| {
        let c = resources.and_then(|r| r.channel(cid));
        PlaceholderChannel { id: cid.to_string(), name: Some(c.map(|c| c.name.clone()).unwrap_or_default()), kind: Some(c.map_or(0, |c| c.kind_id)) }
    });
    let id = user.map(|u| u.id.as_str()).or(user_id).unwrap_or("");
    let u = is_snowflake(id).then(|| PlaceholderUser {
        id: id.to_string(),
        username: Some(user.and_then(|u| u.username.clone()).unwrap_or_default()),
        tag: None,
        discriminator: Some("0".into()),
        global_name: user.and_then(|u| u.global_name.clone()),
        bot: false,
        created_timestamp: snowflake_time(id),
        avatar_url: Some(user_avatar_url(id, user.and_then(|u| u.avatar.as_deref()))),
    });
    RenderContext { member: None, user: u, guild: Some(guild), channel, vars: None, args: None }
}

/// `restAllowedMentions({ users, roles })`: users only (plus the given roles); @everyone and @here never ping.
pub fn rest_allowed_mentions(users: bool, roles: &[String]) -> Value {
    let mut ids: Vec<String> = Vec::new();
    for id in roles {
        if is_snowflake(id) && !ids.contains(id) {
            ids.push(id.clone());
        }
    }
    ids.truncate(100);
    let mut out = Map::new();
    out.insert("parse".into(), if users { json!(["users"]) } else { json!([]) });
    if !ids.is_empty() {
        out.insert("roles".into(), json!(ids));
    }
    Value::Object(out)
}

/// `toRestMessage(rendered, { edit, allowedMentions })`: the payload of renderTemplate (as JSON: `{ content?, embeds,
/// components }`, see `bot_core::custom_embed::Rendered::to_json`) as a Discord REST message body. `edit`: an empty
/// content clears the old text. `allowed_mentions` defaults to [`rest_allowed_mentions`]`(true, [])`.
pub fn to_rest_message(rendered: Option<&Value>, edit: bool, allowed_mentions: Option<Value>) -> Value {
    let list = |k: &str| rendered.and_then(|r| r.get(k)).and_then(Value::as_array).cloned().unwrap_or_default();
    let mut body = Map::new();
    body.insert("embeds".into(), Value::Array(list("embeds")));
    body.insert("components".into(), Value::Array(list("components")));
    body.insert("allowed_mentions".into(), allowed_mentions.unwrap_or_else(|| rest_allowed_mentions(true, &[])));
    match rendered.and_then(|r| r.get("content")).and_then(Value::as_str) {
        Some(c) if !c.is_empty() => {
            body.insert("content".into(), Value::String(c.to_string()));
        }
        _ if edit => {
            body.insert("content".into(), Value::String(String::new()));
        }
        _ => {}
    }
    Value::Object(body)
}

/// `memberFor(ctx, resources)`: `ctx.member` with `owner` also set when getGuildResources names the user the owner.
pub fn member_for(ctx: &StoreCtx, resources: Option<&DiscordResources>) -> Option<DashboardMember> {
    member_for_parts(ctx.member.as_ref(), &ctx.user_id, resources)
}

/// [`member_for`] from its parts.
pub fn member_for_parts(member: Option<&DashboardMember>, user_id: &str, resources: Option<&DiscordResources>) -> Option<DashboardMember> {
    let owner_id = resources.and_then(|r| r.guild.owner_id.as_deref()).filter(|o| !o.is_empty());
    if member.is_none_or(|m| !m.owner) && owner_id.is_some_and(|o| !user_id.is_empty() && o == user_id) {
        let mut m = member.cloned().unwrap_or_default();
        m.owner = true;
        return Some(m);
    }
    member.cloned()
}

/// `memberCanMentionAll(member)`: the server owner, Administrator or Mention @everyone.
pub fn member_can_mention_all(member: Option<&DashboardMember>) -> bool {
    let Some(m) = member else { return false };
    if m.owner {
        return true;
    }
    has_flag(&m.permissions, flags::ADMINISTRATOR) || has_flag(&m.permissions, flags::MENTION_EVERYONE)
}

fn role_name(role: &ResRole, id: &str) -> String {
    if role.name.is_empty() { format!("the role {id}") } else { format!("@{}", role.name) }
}

fn is_everyone_role(role: &ResRole, resources: Option<&DiscordResources>) -> bool {
    role.everyone || resources.is_some_and(|r| role.id == r.guild.id)
}

/// `pingRoleProblem(resources, roleId, member)`: the ping-role rule of /reminder, /feed and /tag.
pub fn ping_role_problem(resources: Option<&DiscordResources>, role_id: &str, member: Option<&DashboardMember>) -> Option<String> {
    let Some(role) = resources.and_then(|r| r.role(role_id)) else {
        return Some("That role does not exist in this server any more. Pick another one.".into());
    };
    if is_everyone_role(role, resources) {
        return Some("@everyone can't be pinged. Use a role members can opt into.".into());
    }
    if role.mentionable == Some(true) || member_can_mention_all(member) {
        return None;
    }
    if role.mentionable == Some(false) && member.is_some() {
        return Some(format!(
            "{} is not mentionable and you lack the Mention @everyone, @here and All Roles permission. Make the role mentionable in Server Settings → Roles, or pick another one.",
            role_name(role, role_id)
        ));
    }
    Some(format!(
        "The dashboard could not check whether you may ping {}. Make the role mentionable in Server Settings → Roles, or set it with the slash command in Discord.",
        role_name(role, role_id)
    ))
}

/// `splitPingableRoles(roleIds, resources, member)`: the role-ping rule of /embed publish.
pub fn split_pingable_roles(role_ids: &[String], resources: Option<&DiscordResources>, member: Option<&DashboardMember>) -> (Vec<String>, Vec<String>) {
    let mut allowed = Vec::new();
    let mut refused = Vec::new();
    let mut seen: Vec<&String> = Vec::new();
    for id in role_ids {
        if seen.contains(&id) {
            continue;
        }
        seen.push(id);
        let ok = resources.and_then(|r| r.role(id)).is_some_and(|role| {
            !role.everyone
                && resources.is_none_or(|r| *id != r.guild.id)
                && (role.mentionable == Some(true) || member_can_mention_all(member))
        });
        if ok { allowed.push(id.clone()) } else { refused.push(id.clone()) }
    }
    (allowed, refused)
}

/// Permissions a role handed out by a custom command must not carry (checkAssignableRole's DANGEROUS_PERMISSIONS).
pub const DANGEROUS_ROLE_PERMISSIONS: [&str; 10] = [
    "Administrator", "ManageGuild", "ManageRoles", "BanMembers", "KickMembers", "ManageChannels", "ManageWebhooks",
    "MentionEveryone", "ModerateMembers", "ManageMessages",
];

fn permission_label(k: &str) -> &'static str {
    match k {
        "Administrator" => "Administrator",
        "ManageGuild" => "Manage Server",
        "ManageRoles" => "Manage Roles",
        "BanMembers" => "Ban Members",
        "KickMembers" => "Kick Members",
        "ManageChannels" => "Manage Channels",
        "ManageWebhooks" => "Manage Webhooks",
        "MentionEveryone" => "Mention @everyone",
        "ModerateMembers" => "Timeout Members",
        "ManageMessages" => "Manage Messages",
        _ => "",
    }
}

/// `assignRoleProblem(resources, roleId, member)`: the role-action rule of /tag action add.
pub fn assign_role_problem(resources: Option<&DiscordResources>, role_id: &str, member: Option<&DashboardMember>) -> Option<String> {
    let Some(role) = resources.and_then(|r| r.role(role_id)) else {
        return Some("That role does not exist in this server any more. Pick another one.".into());
    };
    if is_everyone_role(role, resources) {
        return Some("@everyone cannot be given or removed.".into());
    }
    if role.managed {
        return Some("That role is managed by an integration and cannot be given by the bot.".into());
    }
    if role.assignable == Some(false) {
        return Some("That role is above the bot's highest role. Move the bot's role higher in Server Settings → Roles, or pick a lower role.".into());
    }
    if let Some(m) = member
        && !m.owner
        && let Some(top) = m.highest_role_position
        && role.position >= top
    {
        return Some("That role is not below your highest role, so you cannot have the bot give it.".into());
    }
    if let Some(perms) = &role.permissions {
        let bad: Vec<&str> = DANGEROUS_ROLE_PERMISSIONS.iter().copied().filter(|k| flag_bit(k).is_some_and(|f| has_flag(perms, f))).collect();
        if !bad.is_empty() {
            return Some(format!("That role has dangerous permissions: {}.", bad.iter().map(|k| permission_label(k)).collect::<Vec<_>>().join(", ")));
        }
        return None;
    }
    match role.dangerous {
        Some(true) => Some(
            "That role has moderator or admin permissions (such as Administrator, Manage Server, Ban or Kick Members). Custom commands never hand those out; pick a role without them."
                .into(),
        ),
        Some(false) => None,
        None => Some("The dashboard could not check this role's permissions. Add role actions with /tag action add in Discord.".into()),
    }
}

/// Discord JSON error codes the stores branch on (`DISCORD_CODES`).
pub mod discord_codes {
    pub const UNKNOWN_CHANNEL: i64 = 10003;
    pub const UNKNOWN_MESSAGE: i64 = 10008;
    pub const MISSING_ACCESS: i64 = 50001;
    pub const CANNOT_EDIT_OTHERS: i64 = 50005;
    pub const MISSING_PERMISSIONS: i64 = 50013;
    pub const INVALID_FORM_BODY: i64 = 50035;
}

/// The write that failed (`op` of discordFailure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscordOp {
    Send,
    Edit,
    Delete,
}

/// `discordFailure`'s second argument: `{ op, channel, channelId, embeds }` (`embeds` defaults to true).
#[derive(Debug, Clone)]
pub struct FailureInfo<'a> {
    pub op: DiscordOp,
    /// `{ id, name }` of the channel, when known.
    pub channel_name: Option<&'a str>,
    pub channel_id: Option<&'a str>,
    pub embeds: bool,
}

impl<'a> FailureInfo<'a> {
    pub fn new(op: DiscordOp) -> Self {
        Self { op, channel_name: None, channel_id: None, embeds: true }
    }
}

fn channel_label(name: Option<&str>, id: Option<&str>) -> String {
    match name.filter(|n| !n.is_empty()) {
        Some(n) => format!("#{n}"),
        None => format!("the channel {}", id.unwrap_or("")).trim().to_string(),
    }
}

/// `discordFailure(err, info)` for a DiscordApiError: the RecordError the admin can act on.
pub fn discord_failure_of(err: &DiscordApiError, info: &FailureInfo) -> RecordError {
    let place = channel_label(info.channel_name, info.channel_id);
    if err.code == "forbidden"
        || err.discord_code == Some(discord_codes::MISSING_ACCESS)
        || err.discord_code == Some(discord_codes::MISSING_PERMISSIONS)
    {
        return match info.op {
            DiscordOp::Send => {
                let needs = format!("View Channel, Send Messages{}", if info.embeds { " and Embed Links" } else { "" });
                RecordError::discord(format!("The bot can't post in {place}: it needs {needs} there."))
            }
            DiscordOp::Edit => RecordError::discord(format!(
                "The bot can't edit its message in {place}: it needs View Channel, Read Message History and Embed Links there."
            )),
            DiscordOp::Delete => RecordError::discord(format!("The bot can't delete the message in {place}: it needs View Channel there.")),
        };
    }
    if err.discord_code == Some(discord_codes::INVALID_FORM_BODY) || err.status == 400 {
        return RecordError::invalid("Discord rejected the message. Check the links and images (Discord must be able to load them).").with_path("template");
    }
    if err.code == "not_found" {
        let first = slice16(&place, 1);
        let rest = &place[first.len()..];
        return RecordError::discord(format!("{}{rest} no longer exists.", first.to_uppercase()));
    }
    if err.code == "rate_limited" {
        return RecordError::discord("Discord is rate limiting the bot right now. Try again in a few seconds.");
    }
    RecordError::discord("Discord did not answer. Try again in a moment.")
}

/// `discordFailure(err, info)`: a Discord failure becomes a RecordError; any other error is returned unchanged.
pub fn discord_failure(err: StoreError, info: &FailureInfo) -> StoreError {
    match err {
        StoreError::Discord(e) => StoreError::Record(discord_failure_of(&e, info)),
        other => other,
    }
}

/// `isDiscordApiError(err)`.
pub fn is_discord_api_error(err: &StoreError) -> bool {
    matches!(err, StoreError::Discord(_))
}
