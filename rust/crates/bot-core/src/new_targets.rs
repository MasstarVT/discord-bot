//! Port of `src/utils/newTargets.js`: the "or create a new one" companions of role and channel pickers.
//!
//! A setup option such as `channel` gets a string companion `new_channel` ([`new_channel_option`] /
//! [`new_role_option`], declared right after the picker; a picker that was required becomes optional). The command
//! calls [`resolve_new_options`] after its defer and before it reads the picker. A typed name reuses a role or channel
//! with exactly that name, or creates one, and then the command runs as if it had been picked: the JS patches
//! discord.js's private `_hoistedOptions`; here the leaf option itself is rewritten inside `ctx.interaction`, so
//! `ctx.get_channel_id("channel")` / `get_role_id` / `get_channel` / `get_role` all see it.
//!
//! A private option (log or transcript channels) reuses an existing channel only when no more people can see it than
//! a channel made here; [`reuse_note`] tells the command to say so. Nothing here fails: problems answer the
//! interaction with an error and return `false`.

use serde_json::Value;
use serenity::all::{
    ChannelId, ChannelType, CommandDataOptionValue, Guild, GuildChannel, GuildId, Member, PermissionOverwrite, PermissionOverwriteType,
    Permissions, Role, RoleId, User, UserId,
};
use serenity::builder::{CreateChannel, EditRole};

use crate::command::{CommandOption, option_type};
use crate::context::App;
use crate::embed_builder::error_embed;
use crate::interaction::CommandCtx;
use crate::interaction_helpers::{DANGEROUS_PERMISSIONS, Payload, missing_from, safe_reply};
use crate::markdown::escape_markdown;
use crate::permissions::{self, js_has, permission_names};
use crate::privacy_guards::{MemberRef, member_channel_problem};

/// The companion's name prefix.
pub const NEW_PREFIX: &str = "new_";
const NAME_MAX: u16 = 100;
const CREATE_TIMEOUT_MS: u64 = 15_000;
const MEMBER_LOOKUP_MS: u64 = 3_000;

// ── Builders ──────────────────────────────────────────────────────────────────

/// `newRoleOption(o, base)`: the companion `new_<base>` of a role picker.
pub fn new_role_option(base: &str) -> CommandOption {
    CommandOption::string(format!("{NEW_PREFIX}{base}"), "Or type a name to create a new role (reuses an exact match).").min_length(1).max_length(NAME_MAX)
}

/// `newChannelOption(o, base, noun)`: the companion `new_<base>` of a channel picker; `noun` is what the description
/// calls it ("channel", "category", "voice channel").
pub fn new_channel_option(base: &str, noun: &str) -> CommandOption {
    CommandOption::string(format!("{NEW_PREFIX}{base}"), format!("Or type a name to create a new {noun} (reuses an exact match)."))
        .min_length(1)
        .max_length(NAME_MAX)
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

/// The types a picker's new channel may be, in the order tried: text, voice, category, announcement, forum, stage.
pub const CREATE_ORDER: [u8; 6] = [0, 2, 4, 5, 15, 13];
const SLUGGED: [u8; 3] = [0, 5, 15];

/// `channelTypeFor(channelTypes)`: the type to create for a picker's `channel_types` (none means any: text). `None`
/// when only threads are allowed.
pub fn channel_type_for(channel_types: &[u8]) -> Option<u8> {
    if channel_types.is_empty() {
        return Some(0);
    }
    CREATE_ORDER.iter().copied().find(|t| channel_types.contains(t))
}

/// How Discord stores a text channel name: trimmed, lowercase, whitespace runs become dashes.
pub fn slug(name: &str) -> String {
    let lower = bot_util::js::lower(bot_util::js::trim(name));
    let mut out = String::with_capacity(lower.len());
    let mut in_space = false;
    for c in lower.chars() {
        if bot_util::js::is_js_space(c) {
            if !in_space {
                out.push('-');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

/// `findByName(items, name, { slugged })`: an exact (case-sensitive) match wins over a case-insensitive one.
pub fn find_by_name<'a, T>(items: impl IntoIterator<Item = &'a T>, name: &str, slugged: bool, name_of: impl Fn(&T) -> &str) -> Option<&'a T>
where
    T: 'a,
{
    let want = if slugged { slug(name) } else { bot_util::js::trim(name).to_string() };
    let lower = bot_util::js::lower(&want);
    let mut loose = None;
    for item in items {
        let n = name_of(item);
        if n == want {
            return Some(item);
        }
        if loose.is_none() && bot_util::js::lower(n) == lower {
            loose = Some(item);
        }
    }
    loose
}

/// `leafDefinitions(data, interaction)`: the option definitions of the subcommand that ran, from the command payload.
pub fn leaf_definitions(payload: &Value, group: Option<&str>, sub: Option<&str>) -> Vec<Value> {
    let mut opts = payload.get("options").and_then(Value::as_array).cloned().unwrap_or_default();
    let find = |opts: &[Value], ty: u8, name: &str| {
        opts.iter()
            .find(|o| o["type"].as_u64() == Some(u64::from(ty)) && o["name"].as_str() == Some(name))
            .and_then(|o| o.get("options").and_then(Value::as_array).cloned())
            .unwrap_or_default()
    };
    if let Some(g) = group {
        opts = find(&opts, option_type::SUB_COMMAND_GROUP, g);
    }
    if let Some(s) = sub {
        opts = find(&opts, option_type::SUB_COMMAND, s);
    }
    opts
}

/// A `new_<base>` companion and its picker's definition.
#[derive(Debug, Clone, PartialEq)]
pub struct Companion {
    pub new_name: String,
    pub base: Value,
}

/// `companionPairs(defs)`: every `new_<base>` string option whose sibling `<base>` is a role or channel picker.
pub fn companion_pairs(defs: &[Value]) -> Vec<Companion> {
    let mut out = Vec::new();
    for d in defs {
        let Some(name) = d["name"].as_str() else { continue };
        if d["type"].as_u64() != Some(u64::from(option_type::STRING)) || !name.starts_with(NEW_PREFIX) {
            continue;
        }
        let base_name = &name[NEW_PREFIX.len()..];
        if let Some(base) = defs.iter().find(|b| b["name"].as_str() == Some(base_name)) {
            let ty = base["type"].as_u64();
            if ty == Some(u64::from(option_type::ROLE)) || ty == Some(u64::from(option_type::CHANNEL)) {
                out.push(Companion { new_name: name.to_string(), base: base.clone() });
            }
        }
    }
    out
}

/// What went wrong creating something (`describeCreateError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateError {
    Timeout,
    /// A Discord error code (`None` for a network error).
    Discord(Option<u64>),
}

/// `describeCreateError(err, noun)`.
pub fn describe_create_error(err: &CreateError, noun: &str) -> String {
    match err {
        CreateError::Timeout => format!("Discord took too long to create the {noun}. Try again; if it appeared meanwhile, I'll reuse it."),
        CreateError::Discord(Some(30005)) => "This server already has the maximum number of roles (250). Delete one or pick an existing role.".into(),
        CreateError::Discord(Some(30013)) => "This server already has the maximum number of channels (500). Delete one or pick an existing channel.".into(),
        CreateError::Discord(Some(50013)) => format!("I'm missing a permission to create that {noun}. Check my role's permissions and try again."),
        CreateError::Discord(Some(50035)) => format!("Discord rejected that name. Try a different {noun} name."),
        CreateError::Discord(_) => format!("The {noun} could not be created. Please try again in a moment."),
    }
}

/// `auditReason(interaction)`: `/<command group sub> by <username> (<id>)`, at most 500 characters.
pub fn audit_reason(command: &str, group: Option<&str>, sub: Option<&str>, user: &User) -> String {
    let path: Vec<&str> = [Some(command), group, sub].into_iter().flatten().filter(|s| !s.is_empty()).collect();
    bot_util::text::clamp_text(&format!("/{} by {} ({})", path.join(" "), user.name, user.id), 500)
}

/// `permissionProblem(interaction, perm, noun)`: the invoker and the bot both need the permission to create.
pub fn permission_problem(member_perms: Option<Permissions>, bot_guild_perms: Option<Permissions>, perm: &str, noun: &str) -> Option<String> {
    let label = if perm == "ManageRoles" { "Manage Roles" } else { "Manage Channels" };
    let flag = permissions::flag(perm).unwrap_or(Permissions::empty());
    if !member_perms.is_some_and(|p| js_has(p, flag)) {
        return Some(format!("You need the **{label}** permission to create a {noun}. Pick an existing one instead."));
    }
    if !missing_from(bot_guild_perms, &[perm]).is_empty() {
        return Some(format!("I need the **{label}** permission to create a {noun}."));
    }
    None
}

/// The bot permissions a private channel grants the bot (only those it has server-wide).
pub const PRIVATE_BOT_ALLOW: [&str; 5] = ["ViewChannel", "SendMessages", "EmbedLinks", "AttachFiles", "ReadMessageHistory"];

/// `privateOverwrites(interaction)`: hidden from `@everyone`; visible to the bot (its server-wide permissions among
/// [`PRIVATE_BOT_ALLOW`]) and the invoker.
pub fn private_overwrites(guild_id: GuildId, bot: Option<(UserId, Permissions)>, invoker: UserId) -> Vec<PermissionOverwrite> {
    let mut out = vec![PermissionOverwrite {
        allow: Permissions::empty(),
        deny: Permissions::VIEW_CHANNEL,
        kind: PermissionOverwriteType::Role(RoleId::new(guild_id.get())),
    }];
    if let Some((me, perms)) = bot {
        let allow = PRIVATE_BOT_ALLOW.iter().filter_map(|k| permissions::flag(k)).filter(|f| js_has(perms, *f)).fold(Permissions::empty(), |a, f| a | f);
        out.push(PermissionOverwrite { allow, deny: Permissions::empty(), kind: PermissionOverwriteType::Member(me) });
    }
    if bot.is_none_or(|(me, _)| me != invoker) {
        out.push(PermissionOverwrite { allow: Permissions::VIEW_CHANNEL, deny: Permissions::empty(), kind: PermissionOverwriteType::Member(invoker) });
    }
    out
}

/// Why an existing channel may not stand in for a private one (`WIDER_REASONS` keys).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wider {
    Everyone,
    Invoker,
    Others,
}

impl Wider {
    /// `WIDER_REASONS[k]`.
    pub fn reason(self) -> &'static str {
        match self {
            Self::Everyone => "everyone can see it",
            Self::Invoker => "you cannot see it",
            Self::Others => "roles or members besides you, me and server admins can see it",
        }
    }
}

/// `reuseNote(interaction)`: the reply line for a command whose private option used an existing channel, else `None`.
pub fn reuse_note(ctx: &CommandCtx) -> Option<String> {
    reuse_note_for(&ctx.reused_private)
}

/// [`reuse_note`] for a list of channel ids.
pub fn reuse_note_for(ids: &[ChannelId]) -> Option<String> {
    if ids.is_empty() {
        return None;
    }
    let list = ids.iter().map(|id| format!("<#{id}>")).collect::<Vec<_>>().join(", ");
    Some(format!("ℹ️ Reused the existing {list} instead of creating a new private channel."))
}

// ── Discord side ──────────────────────────────────────────────────────────────

/// Per-app state: the create-or-reuse lock (`createLock`), per guild and name.
#[derive(Default)]
struct NewTargetsState {
    lock: crate::sync::KeyedMutex,
}

/// The outcome of obtaining one entity.
#[allow(clippy::large_enum_variant)] // one per typed name
enum Obtained {
    Role(Role, bool),
    Channel(GuildChannel, bool),
    Failed { title: Option<String>, message: String },
}

/// Options of [`resolve_new_options`] (`{ required, private }`).
#[derive(Debug, Clone, Default)]
pub struct NewOptions<'a> {
    /// Pickers that need a value from either the picker or its companion.
    pub required: &'a [&'a str],
    /// Channel pickers whose new channel is hidden from `@everyone`.
    pub private: &'a [&'a str],
}

async fn fail(ctx: &CommandCtx, title: &str, message: &str) -> bool {
    safe_reply(&ctx.responder, Payload::embed(error_embed(title, Some(message), None)).components(Vec::new()).no_mentions()).await;
    false
}

/// What the create steps need, owned (they run on the per-guild lock).
#[derive(Clone)]
struct Inputs {
    app: App,
    guild_id: GuildId,
    user: User,
    member_perms: Option<Permissions>,
    member_roles: Option<Vec<RoleId>>,
    reason: String,
}

/// `resolveNewOptions(interaction, data, { required, private })`: creates or reuses every role or channel typed into
/// a `new_<base>` companion and substitutes it for the picker. Call after the defer and before reading any picker.
/// `true` to continue; `false` when an error was already shown.
pub async fn resolve_new_options(ctx: &mut CommandCtx, opts: NewOptions<'_>) -> bool {
    let Some(command) = ctx.app.commands().get(ctx.command_name()) else {
        return fail(ctx, "Something Went Wrong", "The role or channel could not be prepared. Please try again in a moment.").await;
    };
    let group = ctx.subcommand_group();
    let sub = ctx.subcommand();
    let defs = leaf_definitions(&command.payload(), group.as_deref(), sub.as_deref());
    let reason = audit_reason(ctx.command_name(), group.as_deref(), sub.as_deref(), ctx.user());
    for pair in companion_pairs(&defs) {
        let base_name = pair.base["name"].as_str().unwrap_or_default().to_string();
        let is_role = pair.base["type"].as_u64() == Some(u64::from(option_type::ROLE));
        let kind = if is_role { "role" } else { "channel" };
        let raw = ctx.get_string(&pair.new_name);
        let name = raw.as_deref().map(|r| bot_util::js::trim(r).to_string()).unwrap_or_default();
        let picked = ctx.has_option(&base_name);

        if raw.is_some() && name.is_empty() {
            let msg = format!("Type a name in `{}`, or pick an existing {kind} in `{base_name}`.", pair.new_name);
            return fail(ctx, "Name Required", &msg).await;
        }
        if !name.is_empty() && picked {
            let msg = format!("Pick an existing {kind} in `{base_name}` **or** type a new name in `{}`, not both.", pair.new_name);
            return fail(ctx, "Pick One", &msg).await;
        }
        if name.is_empty() {
            if !picked && opts.required.contains(&base_name.as_str()) {
                let title = format!("Missing {}", if is_role { "Role" } else { "Channel" });
                let msg = format!("Pick a {kind} in `{base_name}`, or type a name in `{}` to create one.", pair.new_name);
                return fail(ctx, &title, &msg).await;
            }
            continue;
        }

        let Some(guild_id) = ctx.guild_id() else {
            return fail(ctx, "Something Went Wrong", "The role or channel could not be prepared. Please try again in a moment.").await;
        };
        let inputs = Inputs {
            app: ctx.app.clone(),
            guild_id,
            user: ctx.user().clone(),
            member_perms: ctx.member().and_then(|m| m.permissions),
            member_roles: ctx.member().map(|m| m.roles.clone()),
            reason: reason.clone(),
        };
        let hidden = opts.private.contains(&base_name.as_str());
        let res = if is_role {
            obtain_role(inputs, name.clone(), base_name.clone()).await
        } else {
            let types: Vec<u8> = pair.base["channel_types"].as_array().map(|a| a.iter().filter_map(|v| v.as_u64()).map(|v| v as u8).collect()).unwrap_or_default();
            obtain_channel(inputs, name.clone(), base_name.clone(), types, hidden).await
        };
        let (id, created) = match res {
            Obtained::Failed { title, message } => {
                let title = title.unwrap_or_else(|| format!("Could Not Create {}", if is_role { "Role" } else { "Channel" }));
                return fail(ctx, &title, &message).await;
            }
            Obtained::Role(role, created) => {
                let id = role.id.get();
                substitute_role(ctx, &pair.new_name, &base_name, role);
                (id, created)
            }
            Obtained::Channel(channel, created) => {
                let id = channel.id.get();
                substitute_channel(ctx, &pair.new_name, &base_name, &channel);
                (id, created)
            }
        };
        if created {
            bot_util::log_info!(format!("[newTargets] {guild_id}: created {kind} {id} \"{name}\" for {reason}"));
        } else if !is_role && hidden {
            ctx.reused_private.push(ChannelId::new(id));
        }
    }
    true
}

/// Replaces the `new_<base>` leaf with `<base>` holding the role (`substituteOption`).
fn substitute_role(ctx: &mut CommandCtx, new_name: &str, base: &str, role: Role) {
    let id = role.id;
    // discord.js `roles._add`: a role made over REST is in the cache at once (role checks see it).
    if let Some(g) = ctx.interaction.guild_id {
        crate::discord::cache_created_role(&ctx.app, g, &role);
    }
    ctx.interaction.data.resolved.roles.insert(id, role);
    if let Some(o) = ctx.leaf_options_mut().iter_mut().find(|o| o.name == new_name) {
        o.name = base.to_string();
        o.value = CommandDataOptionValue::Role(id);
    }
}

/// Replaces the `new_<base>` leaf with `<base>` holding the channel (`substituteOption`).
fn substitute_channel(ctx: &mut CommandCtx, new_name: &str, base: &str, channel: &GuildChannel) {
    // discord.js `channels._add`: a channel made over REST is in the cache at once (channel checks see it).
    crate::discord::cache_created_channel(&ctx.app, channel);
    let partial = serde_json::json!({
        "id": channel.id, "name": channel.name, "type": u8::from(channel.kind), "permissions": null, "parent_id": channel.parent_id,
    });
    if let Ok(p) = serde_json::from_value::<serenity::all::PartialChannel>(partial) {
        ctx.interaction.data.resolved.channels.insert(channel.id, p);
    }
    if let Some(o) = ctx.leaf_options_mut().iter_mut().find(|o| o.name == new_name) {
        o.name = base.to_string();
        o.value = CommandDataOptionValue::Channel(channel.id);
    }
}

fn something_wrong() -> Obtained {
    Obtained::Failed { title: Some("Something Went Wrong".into()), message: "The role or channel could not be prepared. Please try again in a moment.".into() }
}

async fn obtain_role(i: Inputs, name: String, base_name: String) -> Obtained {
    let lock = i.app.ext::<NewTargetsState>();
    let key = format!("{}:role:{}", i.guild_id, bot_util::js::lower(&name));
    lock.lock.run(&key, move || async move { obtain_role_locked(i, name, base_name).await }).await
}

async fn obtain_role_locked(i: Inputs, name: String, base_name: String) -> Obtained {
    let Some(d) = i.app.discord() else { return something_wrong() };
    let bot = d.cache.current_user().id;
    let Some(guild) = d.cache.guild(i.guild_id).map(|g| g.clone()) else { return something_wrong() };
    let mut candidates: Vec<&Role> = guild.roles.values().filter(|r| r.id.get() != guild.id.get() && !r.managed).collect();
    candidates.sort_by_key(|r| r.id.get());
    if let Some(existing) = find_by_name(candidates.iter().copied(), &name, false, |r| r.name.as_str()) {
        let dangerous: Vec<&str> =
            DANGEROUS_PERMISSIONS.iter().copied().filter(|k| permissions::flag(k).is_some_and(|f| existing.permissions.contains(f))).collect();
        if !dangerous.is_empty() {
            return Obtained::Failed {
                title: Some("Role Name Taken".into()),
                message: format!(
                    "A role named **{}** already exists and has powerful permissions ({}). Pick it in `{base_name}` if you really mean it, or type another name.",
                    escape_markdown(&existing.name),
                    permission_names(&dangerous).join(", ")
                ),
            };
        }
        return Obtained::Role(existing.clone(), false);
    }
    let me = permissions::me(&guild, bot);
    let bot_guild_perms = me.map(|m| permissions::guild_permissions(&guild, bot, &m.roles));
    if let Some(problem) = permission_problem(i.member_perms, bot_guild_perms, "ManageRoles", "role") {
        return Obtained::Failed { title: None, message: problem };
    }
    let highest_is_everyone = me.is_some_and(|m| {
        let top = permissions::highest_position(&guild, &m.roles);
        permissions::role_position(&guild, permissions::everyone_role_id(&guild)) == Some(top)
            && !m.roles.iter().any(|r| permissions::role_position(&guild, *r) == Some(top))
    });
    if highest_is_everyone {
        return Obtained::Failed {
            title: Some("Cannot Create Role".into()),
            message: "I have no role of my own, so a role I create would sit above my highest role and I couldn't give it out. Give me a role near the top of **Server Settings → Roles** (or re-invite me so Discord adds one), then try again.".into(),
        };
    }
    let http = d.http.clone();
    let (h, gid, n, reason) = (http.clone(), i.guild_id, name.clone(), i.reason.clone());
    let create = async move {
        let builder = EditRole::new().name(&n).permissions(Permissions::empty()).mentionable(false).hoist(false).audit_log_reason(&reason);
        gid.create_role(h.as_ref(), builder).await
    };
    let created = bot_util::async_util::with_timeout(create, CREATE_TIMEOUT_MS, "create role").await;
    let role = match created {
        Ok(r) => r,
        Err(e) => {
            let err = match &e {
                bot_util::async_util::WithTimeoutError::Timeout(_) => CreateError::Timeout,
                bot_util::async_util::WithTimeoutError::Inner(se) => CreateError::Discord(crate::discord::error_code(se)),
            };
            bot_util::log_warn!(format!("[newTargets] {}: creating role \"{name}\" failed", i.guild_id), bot_util::logger::LogArg::Error { message: e.to_string(), stack: None });
            return Obtained::Failed { title: None, message: describe_create_error(&err, "role") };
        }
    };
    // checkAssignableRole's test, on the guild with the new role in it.
    if let Some(me) = me {
        let mut with_new = guild.clone();
        with_new.roles.insert(role.id, role.clone());
        let new_pos = permissions::role_position(&with_new, role.id).unwrap_or(0);
        if new_pos >= permissions::highest_position(&with_new, &me.roles) {
            if let Err(e) = http.delete_role(i.guild_id, role.id, Some(&i.reason)).await {
                bot_util::log_warn!(format!("[newTargets] {}: could not delete role {}", i.guild_id, role.id), bot_util::logger::LogArg::Error { message: e.to_string(), stack: None });
            }
            return Obtained::Failed {
                title: Some("Cannot Create Role".into()),
                message: "The new role ended up above my highest role, so I deleted it again. Move my role higher in **Server Settings → Roles**, then try again.".into(),
            };
        }
    }
    Obtained::Role(role, true)
}

async fn obtain_channel(i: Inputs, name: String, base_name: String, channel_types: Vec<u8>, hidden: bool) -> Obtained {
    let Some(kind) = channel_type_for(&channel_types) else {
        return Obtained::Failed { title: None, message: "That option only accepts threads, which I cannot create. Pick an existing one.".into() };
    };
    let slugged = SLUGGED.contains(&kind);
    let key_name = if slugged { slug(&name) } else { bot_util::js::lower(bot_util::js::trim(&name)) };
    let lock = i.app.ext::<NewTargetsState>();
    let key = format!("{}:channel:{kind}:{key_name}", i.guild_id);
    lock.lock.run(&key, move || async move { obtain_channel_locked(i, name, base_name, channel_types, kind, hidden).await }).await
}

async fn obtain_channel_locked(i: Inputs, name: String, base_name: String, channel_types: Vec<u8>, kind: u8, hidden: bool) -> Obtained {
    let noun = if kind == 4 { "category" } else { "channel" };
    let allowed: Vec<u8> = if channel_types.is_empty() { vec![kind] } else { channel_types };
    let slugged = SLUGGED.contains(&kind);
    let Some(d) = i.app.discord() else { return something_wrong() };
    let bot = d.cache.current_user().id;
    let Some(guild) = d.cache.guild(i.guild_id).map(|g| g.clone()) else { return something_wrong() };
    let mut candidates: Vec<&GuildChannel> = guild.channels.values().filter(|c| allowed.contains(&u8::from(c.kind))).collect();
    candidates.sort_by_key(|c| c.id.get());
    if let Some(existing) = find_by_name(candidates.iter().copied(), &name, slugged, |c| c.name.as_str()) {
        let wider = if hidden { private_reuse_problem(&i, &guild, bot, existing).await } else { None };
        if let Some(w) = wider {
            let pickable = w != Wider::Invoker;
            let head = if pickable { format!("<#{}>", existing.id) } else { format!("A {noun} with that name") };
            let tail = if pickable { format!("Pick it in `{base_name}` if you really mean it, or type another name.") } else { "Type another name.".into() };
            return Obtained::Failed {
                title: Some("Channel Name Taken".into()),
                message: format!("{head} already exists and {}, but this {noun} is created private. {tail}", w.reason()),
            };
        }
        return Obtained::Channel(existing.clone(), false);
    }
    let me = permissions::me(&guild, bot);
    let bot_guild_perms = me.map(|m| permissions::guild_permissions(&guild, bot, &m.roles));
    if let Some(problem) = permission_problem(i.member_perms, bot_guild_perms, "ManageChannels", noun) {
        return Obtained::Failed { title: None, message: problem };
    }
    let overwrites = hidden.then(|| private_overwrites(i.guild_id, bot_guild_perms.map(|p| (bot, p)), i.user.id));
    let (h, gid, n, reason) = (d.http.clone(), i.guild_id, name.clone(), i.reason.clone());
    let create = async move {
        let mut builder = CreateChannel::new(&n).kind(ChannelType::from(kind)).audit_log_reason(&reason);
        if let Some(ow) = overwrites {
            builder = builder.permissions(ow);
        }
        gid.create_channel(h.as_ref(), builder).await
    };
    let created = bot_util::async_util::with_timeout(create, CREATE_TIMEOUT_MS, "create channel").await;
    match created {
        Ok(c) => Obtained::Channel(c, true),
        Err(e) => {
            let err = match &e {
                bot_util::async_util::WithTimeoutError::Timeout(_) => CreateError::Timeout,
                bot_util::async_util::WithTimeoutError::Inner(se) => CreateError::Discord(crate::discord::error_code(se)),
            };
            bot_util::log_warn!(format!("[newTargets] {}: creating {noun} \"{name}\" failed", i.guild_id), bot_util::logger::LogArg::Error { message: e.to_string(), stack: None });
            Obtained::Failed { title: None, message: describe_create_error(&err, noun) }
        }
    }
}

/// `isServerAdmin(guild, id)`: whether a member holds Manage Server (or Administrator); false when that cannot be told.
async fn is_server_admin(app: &App, guild: &Guild, id: UserId) -> bool {
    let member: Option<Member> = match guild.members.get(&id) {
        Some(m) => Some(m.clone()),
        None => match app.http() {
            Some(http) => {
                let gid = guild.id;
                bot_util::async_util::with_timeout(async move { gid.member(http.as_ref(), id).await }, MEMBER_LOOKUP_MS, "fetch member").await.ok()
            }
            None => None,
        },
    };
    member.is_some_and(|m| js_has(permissions::guild_permissions(guild, m.user.id, &m.roles), Permissions::MANAGE_GUILD))
}

/// `privateReuseProblem(interaction, channel)`.
async fn private_reuse_problem(i: &Inputs, guild: &Guild, bot: UserId, channel: &GuildChannel) -> Option<Wider> {
    let everyone_sees = guild
        .roles
        .get(&permissions::everyone_role_id(guild))
        .and_then(|r| permissions::role_permissions_in(guild, channel, r, true))
        .is_none_or(|p| js_has(p, Permissions::VIEW_CHANNEL));
    if everyone_sees {
        return Some(Wider::Everyone);
    }
    let roles = i.member_roles.clone().or_else(|| guild.members.get(&i.user.id).map(|m| m.roles.clone()));
    let Some(roles) = roles else { return Some(Wider::Invoker) };
    if member_channel_problem(guild, Some(channel), Some(MemberRef { user_id: i.user.id, roles: &roles, permissions: None }), &["ViewChannel"]).is_some() {
        return Some(Wider::Invoker);
    }
    let bot_role = guild.roles.values().find(|r| r.tags.bot_id == Some(bot)).map(|r| r.id.get());
    for ow in &channel.permission_overwrites {
        if !js_has(ow.allow, Permissions::VIEW_CHANNEL) {
            continue;
        }
        let (id, is_role) = match ow.kind {
            PermissionOverwriteType::Role(r) => (r.get(), true),
            PermissionOverwriteType::Member(u) => (u.get(), false),
            _ => continue,
        };
        if id == guild.id.get() || id == bot.get() || id == i.user.id.get() || Some(id) == bot_role {
            continue;
        }
        if is_role {
            if guild.roles.get(&RoleId::new(id)).is_some_and(|r| js_has(r.permissions, Permissions::MANAGE_GUILD)) {
                continue;
            }
        } else if is_server_admin(&i.app, guild, UserId::new(id)).await {
            continue;
        }
        return Some(Wider::Others);
    }
    None
}
