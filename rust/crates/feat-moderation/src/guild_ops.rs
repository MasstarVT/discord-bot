//! The Discord side of moderation: plain views of the guild, its members and roles (built from the serenity cache), and
//! the REST calls the JS made through discord.js managers (`guild.bans`, `member.timeout`, `member.roles.add`, ...).
//!
//! The JS service takes discord.js objects; here the service takes `&App` plus ids, reads the cached guild through
//! [`load_guild`] and calls Discord through [`ModRest`]. Production uses [`SerenityRest`] (the bot's `Http`); tests
//! install a fake with [`set_rest`], so every decision of the service runs offline exactly as it does in production.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock};

use bot_core::App;
use bot_core::error::BoxFuture;
use bot_core::interaction_helpers::{RoleCheck, dangerous_of};
use bot_core::permissions::{self, js_has};
use chrono::{DateTime, Utc};
use serenity::all::{
    Ban, ChannelId, CreateMessage, Guild, GuildId, Http, Member, Message, MessageId, Permissions, Role, RoleId, User, UserId,
};
use serenity::http::{LightMethod, MessagePagination, Request, Route};

const CDN: &str = "https://cdn.discordapp.com";

// ── Errors ──────────────────────────────────────────────────────────────────────

/// A failed Discord call: the JSON error code (`err.code`) and the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordError {
    pub code: Option<u64>,
    pub message: String,
}

impl DiscordError {
    pub fn new(code: Option<u64>, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    /// A Discord API error with a JSON code.
    pub fn code(code: u64, message: impl Into<String>) -> Self {
        Self::new(Some(code), message)
    }

    pub fn from_serenity(e: &serenity::Error) -> Self {
        Self { code: bot_core::discord::error_code(e), message: bot_core::discord::error_message(e) }
    }

    /// No connection to Discord (offline, before login).
    pub fn offline() -> Self {
        Self::new(None, "Discord is not connected")
    }
}

impl fmt::Display for DiscordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.code {
            Some(c) => write!(f, "{} (code {c})", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for DiscordError {}

/// A REST call's result.
pub type Rest<T> = BoxFuture<Result<T, DiscordError>>;

// ── REST ────────────────────────────────────────────────────────────────────────

/// The audit-log actions moderation reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditKind {
    /// `AuditLogEvent.MemberBanAdd` (22).
    BanAdd,
    /// `AuditLogEvent.MemberBanRemove` (23).
    BanRemove,
}

/// One audit-log entry (`GuildAuditLogsEntry`): the fields moderation reads.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditEntry {
    pub target_id: Option<String>,
    pub executor_id: Option<String>,
    /// `entry.executor` when Discord sent the user.
    pub executor: Option<User>,
    pub reason: Option<String>,
    /// `entry.createdTimestamp` (from the entry's snowflake).
    pub created_ms: i64,
}

/// Every Discord REST call moderation makes. Each method is one discord.js call of the JS.
pub trait ModRest: Send + Sync + 'static {
    /// `guild.members.fetch(id)`.
    fn get_member(&self, guild: GuildId, user: UserId) -> Rest<Member>;
    /// `client.users.fetch(id)`.
    fn get_user(&self, user: UserId) -> Rest<User>;
    /// `guild.bans.fetch({ user, force: true })`: `Ok(None)` is Discord's 10026 (Unknown Ban).
    fn get_ban(&self, guild: GuildId, user: UserId) -> Rest<Option<Ban>>;
    /// `guild.bans.fetch({ limit, after })`: one page of the ban list.
    fn get_bans(&self, guild: GuildId, after: Option<UserId>, limit: u16) -> Rest<Vec<Ban>>;
    /// `guild.bans.create(id, { reason, deleteMessageSeconds })`.
    fn ban(&self, guild: GuildId, user: UserId, delete_message_seconds: u32, reason: String) -> Rest<()>;
    /// `guild.bans.remove(id, reason)`.
    fn unban(&self, guild: GuildId, user: UserId, reason: String) -> Rest<()>;
    /// `member.kick(reason)`.
    fn kick(&self, guild: GuildId, user: UserId, reason: String) -> Rest<()>;
    /// `member.timeout(ms, reason)` (`None` lifts the timeout).
    fn timeout(&self, guild: GuildId, user: UserId, until: Option<DateTime<Utc>>, reason: String) -> Rest<()>;
    /// `member.roles.add(role, reason)` (one role).
    fn add_role(&self, guild: GuildId, user: UserId, role: RoleId, reason: String) -> Rest<()>;
    /// `member.roles.remove(role, reason)`.
    fn remove_role(&self, guild: GuildId, user: UserId, role: RoleId, reason: String) -> Rest<()>;
    /// `user.send(payload)`: returns where the DM landed, so it can be taken back.
    fn send_dm(&self, user: UserId, message: CreateMessage) -> Rest<(ChannelId, MessageId)>;
    /// `message.delete()`.
    fn delete_message(&self, channel: ChannelId, message: MessageId) -> Rest<()>;
    /// `channel.send(payload)`.
    fn send_message(&self, channel: ChannelId, message: CreateMessage) -> Rest<()>;
    /// `guild.fetchAuditLogs({ type, limit })`.
    fn audit_entries(&self, guild: GuildId, kind: AuditKind, limit: u8) -> Rest<Vec<AuditEntry>>;
    /// `guild.roles.fetch()`: every role id of the guild.
    fn get_role_ids(&self, guild: GuildId) -> Rest<Vec<RoleId>>;
    /// `guild.roles.create({ name, permissions: [], reason })`.
    fn create_role(&self, guild: GuildId, name: String, reason: String) -> Rest<Role>;
    /// `channel.permissionOverwrites.edit(role, { X: false, ... }, { reason })`: denies `deny` for the role and keeps
    /// the overwrite's other bits (`allow` minus the denied bits, `deny` plus them).
    fn deny_for_role(&self, channel: ChannelId, role: RoleId, allow: Permissions, deny: Permissions, reason: String) -> Rest<()>;
    /// `channel.messages.fetch({ limit, before, cache: false })`.
    fn get_messages(&self, channel: ChannelId, before: Option<MessageId>, limit: u8) -> Rest<Vec<Message>>;
    /// `channel.bulkDelete(ids, false)`.
    fn bulk_delete(&self, channel: ChannelId, ids: Vec<MessageId>) -> Rest<()>;
    /// `channel.edit({ rateLimitPerUser, reason })`.
    fn edit_slowmode(&self, channel: ChannelId, seconds: u16, reason: String) -> Rest<()>;
}

/// The production [`ModRest`]: the bot's serenity `Http`.
pub struct SerenityRest(pub Arc<Http>);

fn map<T>(r: serenity::Result<T>) -> Result<T, DiscordError> {
    r.map_err(|e| DiscordError::from_serenity(&e))
}

/// `X-Audit-Log-Reason`: 1-512 URL-encoded characters (every byte but `[A-Za-z0-9]` percent-encoded, as serenity does).
fn reason_header<'a>(req: Request<'a>, reason: &str) -> Request<'a> {
    if reason.is_empty() {
        return req;
    }
    let mut encoded = String::with_capacity(reason.len() * 3);
    for b in reason.bytes() {
        if b.is_ascii_alphanumeric() {
            encoded.push(b as char);
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }
    let mut req = req.headers(Some(Default::default()));
    if let (Some(headers), Ok(value)) = (req.headers_mut().as_mut(), encoded.parse()) {
        headers.insert("X-Audit-Log-Reason", value);
    }
    req
}

fn opt_reason(reason: &str) -> Option<&str> {
    (!reason.is_empty()).then_some(reason)
}

impl ModRest for SerenityRest {
    fn get_member(&self, guild: GuildId, user: UserId) -> Rest<Member> {
        let http = self.0.clone();
        Box::pin(async move { map(http.get_member(guild, user).await) })
    }

    fn get_user(&self, user: UserId) -> Rest<User> {
        let http = self.0.clone();
        Box::pin(async move { map(http.get_user(user).await) })
    }

    fn get_ban(&self, guild: GuildId, user: UserId) -> Rest<Option<Ban>> {
        let http = self.0.clone();
        Box::pin(async move {
            match http.get_ban(guild, user).await {
                Ok(b) => Ok(b),
                Err(e) if bot_core::discord::error_code(&e) == Some(10026) => Ok(None),
                Err(e) => Err(DiscordError::from_serenity(&e)),
            }
        })
    }

    fn get_bans(&self, guild: GuildId, after: Option<UserId>, limit: u16) -> Rest<Vec<Ban>> {
        let http = self.0.clone();
        Box::pin(async move {
            // serenity's `get_bans` caps `limit` at 255; Discord allows 1000.
            let mut params = vec![("limit", limit.to_string())];
            if let Some(a) = after {
                params.push(("after", a.to_string()));
            }
            let req = Request::new(Route::GuildBans { guild_id: guild }, LightMethod::Get).params(Some(params));
            map(http.fire::<Vec<Ban>>(req).await)
        })
    }

    fn ban(&self, guild: GuildId, user: UserId, delete_message_seconds: u32, reason: String) -> Rest<()> {
        let http = self.0.clone();
        Box::pin(async move {
            // serenity's `ban_user` only takes whole days; discord.js sends `delete_message_seconds` as given.
            let req = Request::new(Route::GuildBan { guild_id: guild, user_id: user }, LightMethod::Put)
                .params(Some(vec![("delete_message_seconds", delete_message_seconds.to_string())]));
            let req = reason_header(req, &reason);
            map(http.request(req).await).map(|_| ())
        })
    }

    fn unban(&self, guild: GuildId, user: UserId, reason: String) -> Rest<()> {
        let http = self.0.clone();
        Box::pin(async move { map(http.remove_ban(guild, user, opt_reason(&reason)).await) })
    }

    fn kick(&self, guild: GuildId, user: UserId, reason: String) -> Rest<()> {
        let http = self.0.clone();
        Box::pin(async move { map(http.kick_member(guild, user, opt_reason(&reason)).await) })
    }

    fn timeout(&self, guild: GuildId, user: UserId, until: Option<DateTime<Utc>>, reason: String) -> Rest<()> {
        let http = self.0.clone();
        Box::pin(async move {
            let body = serde_json::json!({ "communication_disabled_until": until.map(|u| bot_db::format_js_date(&u)) });
            map(http.edit_member(guild, user, &body, opt_reason(&reason)).await).map(|_| ())
        })
    }

    fn add_role(&self, guild: GuildId, user: UserId, role: RoleId, reason: String) -> Rest<()> {
        let http = self.0.clone();
        Box::pin(async move { map(http.add_member_role(guild, user, role, opt_reason(&reason)).await) })
    }

    fn remove_role(&self, guild: GuildId, user: UserId, role: RoleId, reason: String) -> Rest<()> {
        let http = self.0.clone();
        Box::pin(async move { map(http.remove_member_role(guild, user, role, opt_reason(&reason)).await) })
    }

    fn send_dm(&self, user: UserId, message: CreateMessage) -> Rest<(ChannelId, MessageId)> {
        let http = self.0.clone();
        Box::pin(async move {
            let channel = map(user.create_dm_channel(http.as_ref()).await)?;
            let sent = map(channel.id.send_message(http.as_ref(), message).await)?;
            Ok((sent.channel_id, sent.id))
        })
    }

    fn delete_message(&self, channel: ChannelId, message: MessageId) -> Rest<()> {
        let http = self.0.clone();
        Box::pin(async move { map(http.delete_message(channel, message, None).await) })
    }

    fn send_message(&self, channel: ChannelId, message: CreateMessage) -> Rest<()> {
        let http = self.0.clone();
        Box::pin(async move { map(channel.send_message(http.as_ref(), message).await).map(|_| ()) })
    }

    fn audit_entries(&self, guild: GuildId, kind: AuditKind, limit: u8) -> Rest<Vec<AuditEntry>> {
        use serenity::all::audit_log::{Action, MemberAction};
        let http = self.0.clone();
        Box::pin(async move {
            let action = match kind {
                AuditKind::BanAdd => Action::Member(MemberAction::BanAdd),
                AuditKind::BanRemove => Action::Member(MemberAction::BanRemove),
            };
            let logs = map(http.get_audit_logs(guild, Some(action), None, None, Some(limit)).await)?;
            Ok(logs
                .entries
                .iter()
                .map(|e| AuditEntry {
                    target_id: e.target_id.map(|t| t.to_string()),
                    executor_id: Some(e.user_id.to_string()),
                    executor: logs.users.get(&e.user_id).cloned(),
                    reason: e.reason.clone(),
                    created_ms: e.id.created_at().unix_timestamp() * 1000 + i64::from(e.id.created_at().timestamp_subsec_millis()),
                })
                .collect())
        })
    }

    fn get_role_ids(&self, guild: GuildId) -> Rest<Vec<RoleId>> {
        let http = self.0.clone();
        Box::pin(async move { map(http.get_guild_roles(guild).await).map(|roles| roles.into_iter().map(|r| r.id).collect()) })
    }

    fn create_role(&self, guild: GuildId, name: String, reason: String) -> Rest<Role> {
        let http = self.0.clone();
        Box::pin(async move {
            let body = serde_json::json!({ "name": name, "permissions": "0" });
            map(http.create_role(guild, &body, opt_reason(&reason)).await)
        })
    }

    fn deny_for_role(&self, channel: ChannelId, role: RoleId, allow: Permissions, deny: Permissions, reason: String) -> Rest<()> {
        let http = self.0.clone();
        Box::pin(async move {
            let body = serde_json::json!({ "allow": allow.bits().to_string(), "deny": deny.bits().to_string(), "type": 0 });
            map(http.create_permission(channel, serenity::all::TargetId::new(role.get()), &body, opt_reason(&reason)).await)
        })
    }

    fn get_messages(&self, channel: ChannelId, before: Option<MessageId>, limit: u8) -> Rest<Vec<Message>> {
        let http = self.0.clone();
        Box::pin(async move { map(http.get_messages(channel, before.map(MessagePagination::Before), Some(limit)).await) })
    }

    fn bulk_delete(&self, channel: ChannelId, ids: Vec<MessageId>) -> Rest<()> {
        let http = self.0.clone();
        Box::pin(async move {
            let body = serde_json::json!({ "messages": ids.iter().map(|m| m.to_string()).collect::<Vec<_>>() });
            map(http.delete_messages(channel, &body, None).await)
        })
    }

    fn edit_slowmode(&self, channel: ChannelId, seconds: u16, reason: String) -> Rest<()> {
        let http = self.0.clone();
        Box::pin(async move {
            let body = serde_json::json!({ "rate_limit_per_user": seconds });
            map(http.edit_channel(channel, &body, opt_reason(&reason)).await).map(|_| ())
        })
    }
}

/// The [`ModRest`] of an app with no Discord connection: every call fails as an unreachable Discord would.
pub struct OfflineRest;

fn offline<T: Send + 'static>() -> Rest<T> {
    Box::pin(async { Err(DiscordError::offline()) })
}

impl ModRest for OfflineRest {
    fn get_member(&self, _: GuildId, _: UserId) -> Rest<Member> {
        offline()
    }
    fn get_user(&self, _: UserId) -> Rest<User> {
        offline()
    }
    fn get_ban(&self, _: GuildId, _: UserId) -> Rest<Option<Ban>> {
        offline()
    }
    fn get_bans(&self, _: GuildId, _: Option<UserId>, _: u16) -> Rest<Vec<Ban>> {
        offline()
    }
    fn ban(&self, _: GuildId, _: UserId, _: u32, _: String) -> Rest<()> {
        offline()
    }
    fn unban(&self, _: GuildId, _: UserId, _: String) -> Rest<()> {
        offline()
    }
    fn kick(&self, _: GuildId, _: UserId, _: String) -> Rest<()> {
        offline()
    }
    fn timeout(&self, _: GuildId, _: UserId, _: Option<DateTime<Utc>>, _: String) -> Rest<()> {
        offline()
    }
    fn add_role(&self, _: GuildId, _: UserId, _: RoleId, _: String) -> Rest<()> {
        offline()
    }
    fn remove_role(&self, _: GuildId, _: UserId, _: RoleId, _: String) -> Rest<()> {
        offline()
    }
    fn send_dm(&self, _: UserId, _: CreateMessage) -> Rest<(ChannelId, MessageId)> {
        offline()
    }
    fn delete_message(&self, _: ChannelId, _: MessageId) -> Rest<()> {
        offline()
    }
    fn send_message(&self, _: ChannelId, _: CreateMessage) -> Rest<()> {
        offline()
    }
    fn audit_entries(&self, _: GuildId, _: AuditKind, _: u8) -> Rest<Vec<AuditEntry>> {
        offline()
    }
    fn get_role_ids(&self, _: GuildId) -> Rest<Vec<RoleId>> {
        offline()
    }
    fn create_role(&self, _: GuildId, _: String, _: String) -> Rest<Role> {
        offline()
    }
    fn deny_for_role(&self, _: ChannelId, _: RoleId, _: Permissions, _: Permissions, _: String) -> Rest<()> {
        offline()
    }
    fn get_messages(&self, _: ChannelId, _: Option<MessageId>, _: u8) -> Rest<Vec<Message>> {
        offline()
    }
    fn bulk_delete(&self, _: ChannelId, _: Vec<MessageId>) -> Rest<()> {
        offline()
    }
    fn edit_slowmode(&self, _: ChannelId, _: u16, _: String) -> Rest<()> {
        offline()
    }
}

/// The per-app REST override (tests).
#[derive(Default)]
pub struct RestOverride(RwLock<Option<Arc<dyn ModRest>>>);

/// Makes moderation call `rest` instead of Discord for this app (tests and tools).
pub fn set_rest(app: &App, rest: Arc<dyn ModRest>) {
    let slot = app.ext::<RestOverride>();
    *slot.0.write().unwrap_or_else(|e| e.into_inner()) = Some(rest);
}

/// The REST client moderation uses: the override, else the bot's `Http`, else [`OfflineRest`].
pub fn rest(app: &App) -> Arc<dyn ModRest> {
    if let Some(r) = app.ext::<RestOverride>().0.read().unwrap_or_else(|e| e.into_inner()).clone() {
        return r;
    }
    match app.http() {
        Some(http) => Arc::new(SerenityRest(http)),
        None => Arc::new(OfflineRest),
    }
}

// ── Views ───────────────────────────────────────────────────────────────────────

/// A user as moderation shows it: `user.tag`, `user.username`, `displayAvatarURL()`. Fields the JS read with `?.` are
/// optional (a bare `{ id }` target).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserRef {
    pub id: String,
    pub username: Option<String>,
    pub tag: Option<String>,
    pub avatar_url: Option<String>,
    pub bot: bool,
}

impl UserRef {
    /// `{ id }`.
    pub fn id(id: impl Into<String>) -> Self {
        Self { id: id.into(), ..Default::default() }
    }

    pub fn of(user: &User) -> Self {
        Self {
            id: user.id.to_string(),
            username: Some(user.name.clone()),
            tag: Some(bot_core::adapters::user_tag(user)),
            avatar_url: Some(display_avatar_url(user)),
            bot: user.bot,
        }
    }

    /// `user?.tag ?? user?.username ?? user?.id ?? 'unknown'`.
    pub fn label(&self) -> &str {
        self.tag.as_deref().or(self.username.as_deref()).unwrap_or(&self.id)
    }

    /// The `tagOf(user)` of the commands: the label, backticks replaced, at most 100 characters.
    pub fn tag_of(&self) -> String {
        bot_util::text::clamp_text(&self.label().replace('`', "'"), 100)
    }

    /// True when the object carries a name (`target?.tag || target?.username`).
    pub fn has_name(&self) -> bool {
        self.tag.as_deref().is_some_and(|t| !t.is_empty()) || self.username.as_deref().is_some_and(|u| !u.is_empty())
    }
}

/// `user.displayAvatarURL()` with discord.js's defaults (webp, a gif for an animated hash, no size).
pub fn display_avatar_url(user: &User) -> String {
    match &user.avatar {
        Some(h) => {
            let h = h.to_string();
            let ext = if h.starts_with("a_") { "gif" } else { "webp" };
            format!("{CDN}/avatars/{}/{h}.{ext}", user.id)
        }
        None => bot_core::adapters::default_avatar_url(user.id, &bot_core::adapters::discriminator(user)),
    }
}

/// A role (`Role`): rank, permissions, managed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleView {
    pub id: String,
    pub name: String,
    /// discord.js `role.position` (raw position, ties broken by id).
    pub position: usize,
    pub permissions: Permissions,
    pub managed: bool,
}

/// A guild member as moderation reads it (`GuildMember`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberView {
    pub user: UserRef,
    pub role_ids: Vec<String>,
    /// `member.permissions` (guild-level; owner and Administrator give everything).
    pub permissions: Permissions,
    /// `member.roles.highest.position`.
    pub highest: usize,
    pub manageable: bool,
    pub kickable: bool,
    pub bannable: bool,
    pub moderatable: bool,
    /// `member.communicationDisabledUntil`.
    pub communication_disabled_until: Option<DateTime<Utc>>,
}

impl MemberView {
    pub fn id(&self) -> &str {
        &self.user.id
    }

    /// `member.permissions.has(flag)` (Administrator implies every flag).
    pub fn has(&self, flag: Permissions) -> bool {
        js_has(self.permissions, flag)
    }

    /// `member.isCommunicationDisabled()`.
    pub fn is_communication_disabled(&self, now_ms: i64) -> bool {
        self.communication_disabled_until.is_some_and(|t| t.timestamp_millis() > now_ms)
    }

    /// `member.roles.cache.has(id)`.
    pub fn has_role(&self, id: &str) -> bool {
        self.role_ids.iter().any(|r| r == id)
    }

    /// The [`bot_core::permission_checker::MemberAccess`] of this member (for `resolvePermissionLevel`).
    pub fn access(&self, owner_id: &str) -> bot_core::permission_checker::MemberAccess {
        bot_core::permission_checker::MemberAccess {
            user_id: self.user.id.clone(),
            guild_owner_id: Some(owner_id.to_string()),
            permissions: Some(self.permissions),
            role_ids: self.role_ids.clone(),
        }
    }

    /// Builds the view of a member of a cached guild; the flags follow discord.js (`manageable`, `kickable`,
    /// `bannable`, `moderatable`).
    pub fn of(guild: &Guild, bot_id: UserId, member: &Member) -> Self {
        let uid = member.user.id;
        let perms = permissions::guild_permissions(guild, uid, &member.roles);
        let highest = permissions::highest_position(guild, &member.roles);
        let me = permissions::me(guild, bot_id);
        let me_perms = me.map(|m| permissions::guild_permissions(guild, bot_id, &m.roles)).unwrap_or_else(Permissions::empty);
        let manageable = if uid == guild.owner_id || uid == bot_id {
            false
        } else if bot_id == guild.owner_id {
            true
        } else {
            me.is_some_and(|m| permissions::highest_position(guild, &m.roles) > highest)
        };
        Self {
            user: UserRef::of(&member.user),
            role_ids: member.roles.iter().map(|r| r.to_string()).collect(),
            permissions: perms,
            highest,
            manageable,
            kickable: manageable && js_has(me_perms, Permissions::KICK_MEMBERS),
            bannable: manageable && js_has(me_perms, Permissions::BAN_MEMBERS),
            moderatable: !js_has(perms, Permissions::ADMINISTRATOR) && manageable && js_has(me_perms, Permissions::MODERATE_MEMBERS),
            communication_disabled_until: member.communication_disabled_until.map(|t| *t),
        }
    }
}

/// The guild as moderation reads it: owner, the bot's own member, the roles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuildView {
    pub id: String,
    pub name: String,
    pub owner_id: String,
    pub bot_id: String,
    /// `guild.available`.
    pub available: bool,
    /// `guild.members.me`.
    pub me: Option<MemberView>,
    pub roles: HashMap<String, RoleView>,
}

impl GuildView {
    pub fn role(&self, id: &str) -> Option<&RoleView> {
        self.roles.get(id)
    }

    /// `checkAssignableRole(guild, role, { executor, allowDangerous })` on the views: exists, not @everyone, not
    /// managed, the bot has Manage Roles, below the bot's highest role, below the executor's (the owner is exempt),
    /// then the dangerous permissions.
    pub fn check_assignable_role(&self, role: Option<&RoleView>, executor: Option<&MemberView>, allow_dangerous: bool) -> RoleCheck {
        let fail = |reason: &str| RoleCheck { ok: false, reason: Some(reason.to_string()), dangerous: Vec::new() };
        let Some(role) = role else { return fail("Role not found.") };
        if role.id == self.id {
            return fail("@everyone cannot be assigned.");
        }
        if role.managed {
            return fail("That role is managed by an integration.");
        }
        let Some(me) = &self.me else { return fail("I need the Manage Roles permission.") };
        if !me.has(Permissions::MANAGE_ROLES) {
            return fail("I need the Manage Roles permission.");
        }
        if role.position >= me.highest {
            return fail("That role is above my highest role.");
        }
        if let Some(ex) = executor
            && ex.id() != self.owner_id
            && role.position >= ex.highest
        {
            return fail("That role is above your highest role.");
        }
        let dangerous = dangerous_of(role.permissions);
        if !dangerous.is_empty() && !allow_dangerous {
            let names = permissions::permission_names(&dangerous).join(", ");
            return RoleCheck { ok: false, reason: Some(format!("That role has dangerous permissions: {names}.")), dangerous };
        }
        RoleCheck { ok: true, reason: None, dangerous }
    }
}

/// A cached guild with its views: what the JS reached through `guild`.
#[derive(Debug, Clone)]
pub struct LoadedGuild {
    pub id: GuildId,
    pub bot_id: UserId,
    pub view: GuildView,
    pub guild: Guild,
}

impl LoadedGuild {
    /// The view of a cached member (`guild.members.cache.get(id)`).
    pub fn cached_member(&self, user: UserId) -> Option<MemberView> {
        self.guild.members.get(&user).map(|m| MemberView::of(&self.guild, self.bot_id, m))
    }

    /// A view of any member object of this guild (a REST fetch, an event payload).
    pub fn member_view(&self, member: &Member) -> MemberView {
        MemberView::of(&self.guild, self.bot_id, member)
    }

    /// `guild.members.cache.get(id) ?? guild.members.fetch(id)`: `Ok(None)` only for 10007 / 10013; any other
    /// failure is an error, because a transient error must never read as "the member left".
    pub async fn member_or_none(&self, rest: &dyn ModRest, user: UserId) -> Result<Option<MemberView>, DiscordError> {
        if let Some(m) = self.cached_member(user) {
            return Ok(Some(m));
        }
        match rest.get_member(self.id, user).await {
            Ok(m) => Ok(Some(self.member_view(&m))),
            Err(e) if matches!(e.code, Some(10007) | Some(10013)) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// The cached guild as moderation reads it; `None` when it is not cached (offline, or not a guild of this shard).
pub fn load_guild(app: &App, guild_id: GuildId) -> Option<LoadedGuild> {
    let d = app.discord()?;
    let bot_id = d.cache.current_user().id;
    let guild = d.cache.guild(guild_id)?.clone();
    let available = app.guild_directory().guild_state(&guild_id.to_string()) == bot_core::discord::GuildState::Available;
    let roles = guild
        .roles
        .values()
        .map(|r| {
            let view = RoleView {
                id: r.id.to_string(),
                name: r.name.clone(),
                position: permissions::role_position(&guild, r.id).unwrap_or(0),
                permissions: r.permissions,
                managed: r.managed,
            };
            (view.id.clone(), view)
        })
        .collect();
    let me = guild.members.get(&bot_id).map(|m| MemberView::of(&guild, bot_id, m));
    let view =
        GuildView { id: guild_id.to_string(), name: guild.name.clone(), owner_id: guild.owner_id.to_string(), bot_id: bot_id.to_string(), available, me, roles };
    Some(LoadedGuild { id: guild_id, bot_id, view, guild })
}

/// Parses a snowflake string (`None` for anything else, and for 0).
pub fn snowflake(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|v| *v != 0)
}
