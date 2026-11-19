//! Port of `src/commands/utility/tag.js`: `/tag`, custom commands (the MEE6 `!name` commands). Anyone can run, list and
//! inspect a command; creating and changing them needs ADMIN (subcommand permissions). The command is not gated,
//! because the `trigger` group is a config command that works while the module is off; `execute` itself refuses every
//! other subcommand while the Custom Commands module is off, with the router's wording. Management replies are
//! ephemeral and never ping; execution is [`crate::tag_service::run_tag`], shared with the prefix hook.
//!
//! The `trigger` group: auto-responders for matching messages. Config commands: they work while the module is off
//! and warn when a trigger will not fire yet. Replies are public (the admin convention).

use bot_config::tables::limits;
use bot_core::adapters::render_context;
use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::embed_builder::apply_guild_color;
use bot_core::interaction::member_access;
use bot_core::interaction_helpers::{
    Choice, Payload, PermTarget, bot_permissions_for, cached_guild, check_assignable_role_cached, check_channel_usable, check_channel_usable_for,
    missing_bot_permissions, missing_from, pingable_role_ids, resolve_channel, respond_autocomplete, safe_reply,
};
use bot_core::paginator::{PaginateOptions, paginate};
use bot_core::prelude::*;
use bot_core::privacy_guards::{MEMBER_CHANNEL_NEEDS, MemberRef, member_channel_problem, role_hierarchy_problem};
use bot_db::{AutoTrigger, CooldownScope, CustomTag, GuildSettings};
use bot_util::placeholder_parser::{RenderContext, placeholder_help, render_placeholders_detailed};
use bot_util::text::{clamp_text, default_more, format_number, join_with_limit};
use serde_json::{Value, json};
use serenity::all::{ChannelId, Guild, GuildChannel, GuildId, Member, RoleId};
use serenity::builder::{CreateAttachment, CreateEmbed, CreateEmbedFooter};

use crate::tag_io::DiscordIo;
use crate::tag_service::{
    self as svc, AccessLists, ActionKind, ChannelScope, IdKind, NewTag, NewTrigger, Patch, RunOpts, Source, TagAction, TagError, action_label, actions_to_json,
    as_tag_error, build_usage, check_tag_roles, code_safe, cooldown_label, describe_action, effective_prefix, format_id_list, get_effective_actions,
    legacy_actions, match_label, normalize_actions, normalize_tag_name, one_line, primary_message_index, sync_legacy_columns, validate_trigger_pattern,
};

// ── Declaration ───────────────────────────────────────────────────────────────

fn name_opt() -> Opt {
    Opt::string("name", "Command name").required(true).autocomplete(true).max_length(50)
}

fn trig_opt() -> Opt {
    Opt::string("trigger", "Pattern, #number or ID").required(true).autocomplete(true).max_length(100)
}

fn match_opt() -> Opt {
    Opt::string("match_type", "Match type (default: contains)")
        .choice("Contains", "CONTAINS")
        .choice("Exact", "EXACT")
        .choice("Starts with", "STARTS_WITH")
        .choice("Ends with", "ENDS_WITH")
        .choice("Regex", "REGEX")
}

const ACCESS_CHANNEL_TYPES: [u8; 5] =
    [channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT, channel_type::GUILD_FORUM, channel_type::GUILD_VOICE, channel_type::GUILD_CATEGORY];
const TRIG_CHANNEL_TYPES: [u8; 4] = [channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT, channel_type::GUILD_FORUM, channel_type::GUILD_VOICE];

/// The command's data (`new SlashCommandBuilder()...`).
pub fn data() -> SlashCommand {
    let tag_actions = limits::TAG_ACTIONS as i64;
    let tag_responses = limits::TAG_RESPONSES as i64;
    SlashCommand::new("tag", "Custom commands (MEE6 !name): run, create and manage this server's commands.")
        .option(
            Opt::sub_command("use", "Run a custom command.")
                .option(name_opt())
                .option(Opt::string("args", "Arguments for {1}, {2}, … separated by spaces").max_length(1000)),
        )
        .option(
            Opt::sub_command("create", "Create a custom command (admin).")
                .option(Opt::string("name", "Name: a-z, 0-9, - and _ (used as !name)").required(true).max_length(32))
                .option(Opt::string("content", "Reply text; supports {user}, {1} arguments and other variables").required(true).max_length(4000))
                .option(Opt::boolean("embed", "Reply as an embed (up to 4000 characters)"))
                .option(Opt::string("description", "Shown in /tag list").max_length(100)),
        )
        .option(
            Opt::sub_command("edit", "Edit a custom command (admin).")
                .option(name_opt())
                .option(Opt::string("content", "New reply text").max_length(4000))
                .option(Opt::boolean("embed", "Reply as an embed"))
                .option(Opt::string("description", "New description (\"-\" removes it)").max_length(100))
                .option(Opt::boolean("delete_usage", "Delete the !name message after it runs")),
        )
        .option(Opt::sub_command("delete", "Delete a custom command (admin).").option(name_opt()))
        .option(Opt::sub_command("toggle", "Enable or disable a custom command (admin).").option(name_opt()))
        .option(Opt::sub_command("info", "Show a custom command's settings and usage.").option(name_opt()))
        .option(Opt::sub_command("raw", "Get the exact text of a custom command.").option(name_opt()))
        .option(Opt::sub_command("list", "List this server's custom commands."))
        .option(Opt::sub_command("placeholders", "Show the variables and {1} arguments you can use."))
        .option(
            Opt::sub_command("cooldown", "Set a cooldown: none, server-wide or per user (admin).")
                .option(name_opt())
                .option(
                    Opt::string("scope", "Who the cooldown applies to")
                        .required(true)
                        .choice("None", "NONE")
                        .choice("Server-wide", "SERVER")
                        .choice("Per user", "USER"),
                )
                .option(Opt::string("duration", "e.g. 30s, 10m, 1d (max 365d)").max_length(32)),
        )
        .option(
            Opt::sub_command("permissions", "Allow or deny roles and channels (admin).")
                .option(name_opt())
                .option(
                    Opt::string("rule", "What to do")
                        .required(true)
                        .choice("Allow", "allow")
                        .choice("Deny", "deny")
                        .choice("Remove from lists", "remove")
                        .choice("Clear all lists", "clear"),
                )
                .option(Opt::role("role", "Role"))
                .option(Opt::channel("channel", "Channel or category").channel_types(&ACCESS_CHANNEL_TYPES)),
        )
        .option(
            Opt::sub_command_group("action", "What a custom command does (MEE6 action templates).")
                .option(
                    Opt::sub_command("add", "Add an action (admin).")
                        .option(name_opt())
                        .option(
                            Opt::string("type", "Action")
                                .required(true)
                                .choice("Reply in channel", "reply")
                                .choice("Reply by DM", "dm")
                                .choice("Send to a channel", "send")
                                .choice("Give a role", "addRole")
                                .choice("Remove a role", "removeRole")
                                .choice("Toggle a role", "toggleRole"),
                        )
                        .option(Opt::string("text", "Message text (reply, DM, send)").max_length(4000))
                        .option(Opt::channel("channel", "Target channel (send)").channel_types(&[channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT]))
                        .option(Opt::role("role", "Role (give, remove, toggle)"))
                        .option(Opt::boolean("embed", "Send the text as an embed"))
                        .option(Opt::role("ping_role", "Let the text ping this role")),
                )
                .option(
                    Opt::sub_command("remove", "Remove an action (admin).").option(name_opt()).option(
                        Opt::integer("action", "Action number").required(true).min_value(1).max_value(tag_actions).autocomplete(true),
                    ),
                )
                .option(Opt::sub_command("list", "List a command's actions.").option(name_opt())),
        )
        .option(
            Opt::sub_command_group("response", "Random replies (MEE6 Add a message).")
                .option(
                    Opt::sub_command("add", "Add a random reply to a message action (admin).")
                        .option(name_opt())
                        .option(Opt::string("text", "Reply text").required(true).max_length(4000))
                        .option(
                            Opt::integer("action", "Action number (default: first message action)").min_value(1).max_value(tag_actions).autocomplete(true),
                        ),
                )
                .option(
                    Opt::sub_command("remove", "Remove a random reply (admin).")
                        .option(name_opt())
                        .option(Opt::integer("action", "Action number").required(true).min_value(1).max_value(tag_actions).autocomplete(true))
                        .option(Opt::integer("reply", "Reply number").required(true).min_value(1).max_value(tag_responses).autocomplete(true)),
                ),
        )
        .option(
            Opt::sub_command_group("trigger", "Auto-responders for matching messages (custom commands).")
                .option(Opt::sub_command("enable", "Turn on custom commands and triggers."))
                // `disable` only pauses triggers; /tag keeps working.
                .option(Opt::sub_command("disable", "Turn off triggers; /tag keeps working."))
                .option(
                    Opt::sub_command("add", "Add a trigger.")
                        .option(Opt::string("pattern", "Word, phrase or regex to watch for").required(true).max_length(500))
                        .option(Opt::string("response", "Reply text (supports variables)").required(true).max_length(2000))
                        .option(match_opt())
                        .option(Opt::integer("cooldown", "Seconds per user (0 = none)").min_value(0).max_value(86400))
                        .option(Opt::boolean("delete_message", "Delete the matching message"))
                        .option(Opt::boolean("case_sensitive", "Match case"))
                        .option(Opt::channel("channel", "Only in this channel").channel_types(&TRIG_CHANNEL_TYPES)),
                )
                .option(
                    Opt::sub_command("edit", "Change a trigger.")
                        .option(trig_opt())
                        .option(Opt::string("pattern", "New pattern").max_length(500))
                        .option(Opt::string("response", "New reply text").max_length(2000))
                        .option(match_opt())
                        .option(Opt::integer("cooldown", "Seconds per user (0 = none)").min_value(0).max_value(86400))
                        .option(Opt::boolean("delete_message", "Delete the matching message"))
                        .option(Opt::boolean("case_sensitive", "Match case"))
                        .option(Opt::channel("channel", "Add or remove an allowed channel").channel_types(&TRIG_CHANNEL_TYPES))
                        .option(Opt::role("exempt_role", "Add or remove an exempt role"))
                        .option(Opt::boolean("reset_scope", "Clear channels and exempt roles first")),
                )
                .option(Opt::sub_command("remove", "Delete a trigger.").option(trig_opt()))
                .option(Opt::sub_command("toggle", "Pause or resume a trigger.").option(trig_opt()))
                .option(Opt::sub_command("info", "Show a trigger's details.").option(trig_opt()))
                .option(Opt::sub_command("list", "List triggers with their numbers and IDs.")),
        )
}

/// The command (`module 'customcommands'`, `gated = false`, MEMBER with ADMIN subcommands).
pub fn command() -> CommandDef {
    use PermissionLevel::{Admin, Member};
    let mut def = CommandDef::new("utility", data(), Some(ModuleKey::Customcommands), execute)
        .gated(false)
        .permission_level(Member)
        .autocomplete(autocomplete);
    for (path, level) in [
        ("create", Admin),
        ("edit", Admin),
        ("delete", Admin),
        ("toggle", Admin),
        ("cooldown", Admin),
        ("permissions", Admin),
        ("action add", Admin),
        ("action remove", Admin),
        ("action list", Member),
        ("response add", Admin),
        ("response remove", Admin),
        ("trigger enable", Admin),
        ("trigger disable", Admin),
        ("trigger add", Admin),
        ("trigger edit", Admin),
        ("trigger remove", Admin),
        ("trigger toggle", Admin),
        ("trigger info", Admin),
        ("trigger list", Admin),
    ] {
        def = def.subcommand_permission(path, level);
    }
    def
}

// ── Helpers ───────────────────────────────────────────────────────────────────

const LIST_PAGE_SIZE: usize = 15;

/// `respond(interaction, embed)`: an ephemeral answer that never pings.
async fn respond(ctx: &CommandCtx, embed: CreateEmbed) -> BotResult {
    safe_reply(&ctx.responder, Payload::embed(embed).no_mentions().ephemeral(true)).await;
    Ok(())
}

async fn fail(ctx: &CommandCtx, err: &TagError) -> BotResult {
    respond(ctx, error_embed(&err.title, Some(&err.message), None)).await
}

/// `notFoundText(name)`.
pub fn not_found_text(name: &str) -> String {
    format!("No custom command named `{}` exists here.", code_safe(&clamp_text(name, 50)))
}

async fn not_found(ctx: &CommandCtx, name: &str, hint: bool) -> BotResult {
    let text = format!("{}{}", not_found_text(name), if hint { " See `/tag list`." } else { "" });
    respond(ctx, error_embed("Not Found", Some(&text), None)).await
}

fn gid(ctx: &CommandCtx) -> anyhow::Result<GuildId> {
    ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/tag outside a guild"))
}

/// `guarded(interaction, mutator)`: a command that gives or takes a role at or above the admin's highest role cannot
/// be changed, enabled or disabled by that admin (the guild owner is exempt). Checked on the row as it is stored when
/// the write happens.
#[derive(Clone)]
struct Guard {
    guild: Option<Guild>,
    member: Option<Member>,
}

impl Guard {
    fn of(ctx: &CommandCtx) -> Self {
        Self { guild: ctx.guild_id().and_then(|g| cached_guild(&ctx.app, g)), member: ctx.member().cloned() }
    }

    fn check(&self, tag: &CustomTag) -> Result<(), TagError> {
        let ids: Vec<String> = get_effective_actions(tag).into_iter().filter_map(|a| a.role_id).collect();
        let (Some(g), Some(m)) = (&self.guild, &self.member) else { return Ok(()) };
        match role_hierarchy_problem(g, Some(MemberRef::of(m)), &ids) {
            Some(problem) => Err(TagError::new(
                "HIERARCHY",
                "Role Not Allowed",
                format!("{problem}\nThis command gives or takes a role at or above your highest role, so you cannot change it."),
            )),
            None => Ok(()),
        }
    }
}

/// `cleanDescription(s)`.
pub fn clean_description(s: Option<&str>) -> Option<String> {
    let t = clamp_text(bot_util::js::trim(s.unwrap_or("")), svc::DESCRIPTION_MAX);
    if t.is_empty() { None } else { Some(t) }
}

/// `lengthProblem(text, embed)`: the explanation when `text` exceeds the limit of plain or embed replies.
pub fn length_problem(text: &str, embed: bool) -> Option<String> {
    let n = bot_util::js::utf16_len(text);
    let limit = if embed { svc::EMBED_TEXT_MAX } else { svc::PLAIN_MAX };
    if n <= limit {
        return None;
    }
    Some(if embed {
        format!("Embed replies can be at most 4,000 characters (this one is {n}).")
    } else {
        format!("Plain-text replies can be at most 2,000 characters (this one is {n}). Use `embed:True` for up to 4,000.")
    })
}

fn interaction_render_ctx(ctx: &CommandCtx, args: Option<Vec<String>>) -> RenderContext {
    let mut r = render_context(&ctx.app, ctx.guild_id(), ctx.user(), ctx.member(), Some(ctx.channel_id()));
    r.args = args;
    r
}

/// `⚠️ Unknown variables (shown as typed): …` for tokens no variable engine knows.
pub fn unknown_variables_line(texts: &[&str], render: &RenderContext) -> Option<String> {
    let mut seen: Vec<String> = Vec::new();
    for t in texts {
        for tok in render_placeholders_detailed(t, render, None).unknown_tokens {
            if !seen.contains(&tok) {
                seen.push(tok);
            }
        }
    }
    unknown_line(&seen)
}

fn unknown_line(tokens: &[String]) -> Option<String> {
    if tokens.is_empty() {
        return None;
    }
    let items: Vec<String> = tokens.iter().map(|t| format!("`{}`", code_safe(t))).collect();
    Some(format!("⚠️ Unknown variables (shown as typed): {}", join_with_limit(&items, ", ", 400, default_more)))
}

fn unknown_for(ctx: &CommandCtx, text: &str) -> Option<String> {
    unknown_variables_line(&[text], &interaction_render_ctx(ctx, Some(Vec::new())))
}

/// `shownName(prefix, name)`.
pub fn shown_name(prefix: Option<&str>, name: &str) -> String {
    match prefix {
        None => format!("/tag use {name}"),
        Some(p) => format!("{p}{name}"),
    }
}

const PREFIX_OFF_NOTE: &str = "Prefix commands are off (`/settings prefix`); run it with `/tag use`.";

/// `member.roles.cache` ids (with @everyone) of the invoking member.
fn member_roles(app: &App, guild_id: Option<GuildId>, member: Option<&Member>) -> Option<Vec<String>> {
    let (g, m) = (guild_id?, member?);
    let guild = cached_guild(app, g);
    Some(svc::member_role_ids(guild.as_ref(), &g.to_string(), &m.roles))
}

/// `memberCannotSee(tag, member)`: disabled commands and commands their roles cannot run.
pub fn member_cannot_see(tag: &CustomTag, roles: Option<&[String]>) -> bool {
    !tag.enabled || check_tag_roles(AccessLists::of(tag), roles).is_err()
}

fn level_of(app: &App, guild_id: Option<GuildId>, member: Option<&Member>, settings: &GuildSettings) -> PermissionLevel {
    let access = member_access(app, guild_id, member);
    resolve_permission_level(access.as_ref(), Some(settings), app.config().bot_owner_id.as_deref())
}

fn is_admin(ctx: &CommandCtx, settings: &GuildSettings) -> bool {
    level_of(&ctx.app, ctx.guild_id(), ctx.member(), settings) >= PermissionLevel::Admin
}

/// `hiddenFromMember(tag, interaction, settings)`: members never see disabled or role-restricted commands; admins can.
fn hidden_from_member(ctx: &CommandCtx, tag: &CustomTag, settings: &GuildSettings) -> bool {
    member_cannot_see(tag, member_roles(&ctx.app, ctx.guild_id(), ctx.member()).as_deref()) && !is_admin(ctx, settings)
}

/// `timeTag(date, 'R')`.
pub fn time_tag(date: &bot_db::sqlx::types::chrono::DateTime<bot_db::sqlx::types::chrono::Utc>) -> String {
    format!("<t:{}:R>", date.timestamp_millis().div_euclid(1000))
}

fn fmt_int(n: i64) -> String {
    format_number(n as f64)
}

// ── Execute ───────────────────────────────────────────────────────────────────

/// The router's wording for a refused (module off) command.
pub fn module_off_embed() -> CreateEmbed {
    let label = ModuleKey::Customcommands.info().label;
    warn_embed(
        "Module Disabled",
        Some(&format!("The **{label}** module is turned off on this server. An admin can enable it with `/module enable customcommands`.")),
        None,
    )
}

fn settings_unavailable() -> CreateEmbed {
    error_embed("Temporarily Unavailable", Some("Server settings could not be loaded. Please try again in a moment."), None)
}

/// `settingsInBudget(interaction)`: the memo, else a lookup bounded by what is left of the 3 s window.
async fn settings_in_budget(app: &App, guild_id: Option<GuildId>, budget_ms: u64) -> Option<Settings> {
    let g = guild_id?.to_string();
    if let Some(hit) = app.settings().peek_settings(&g) {
        return Some(hit);
    }
    if budget_ms == 0 {
        return None;
    }
    let a = app.clone();
    let load = async move { Ok::<_, std::convert::Infallible>(a.settings().get_settings_safe(&g).await) };
    bot_util::async_util::with_timeout(load, budget_ms, "tag settings").await.ok().flatten()
}

const HANDLED: [&str; 16] = [
    "use",
    "create",
    "edit",
    "delete",
    "toggle",
    "info",
    "raw",
    "list",
    "placeholders",
    "cooldown",
    "permissions",
    "action add",
    "action remove",
    "action list",
    "response add",
    "response remove",
];

async fn execute(ctx: CommandCtx) -> BotResult {
    let group = ctx.subcommand_group();
    let sub = ctx.subcommand().unwrap_or_default();
    if group.as_deref() == Some("trigger") {
        return execute_trigger(ctx, &sub).await;
    }
    let key = match &group {
        Some(g) => format!("{g} {sub}"),
        None => sub.clone(),
    };
    if !HANDLED.contains(&key.as_str()) {
        return Ok(());
    }
    // The command is not gated (the trigger group is a config command), so the module toggle is checked here.
    let Some(settings) = settings_in_budget(&ctx.app, ctx.guild_id(), ctx.ack_budget_ms()).await else {
        safe_reply(&ctx.responder, Payload::embed(settings_unavailable()).ephemeral(true)).await;
        return Ok(());
    };
    if !is_module_enabled(Some(&settings), Some(ModuleKey::Customcommands)) {
        safe_reply(&ctx.responder, Payload::embed(module_off_embed()).ephemeral(true)).await;
        return Ok(());
    }
    if key != "use" && key != "placeholders" {
        ctx.defer_reply(true).await?;
    }
    let outcome = match key.as_str() {
        "use" => use_command(&ctx, &settings).await,
        "create" => create(&ctx, &settings).await,
        "edit" => edit(&ctx).await,
        "delete" => remove(&ctx).await,
        "toggle" => toggle(&ctx).await,
        "info" => info(&ctx, &settings).await,
        "raw" => raw(&ctx, &settings).await,
        "list" => list(&ctx, &settings).await,
        "placeholders" => placeholders(&ctx, &settings).await,
        "cooldown" => cooldown(&ctx).await,
        "permissions" => permissions(&ctx, &settings).await,
        "action add" => action_add(&ctx).await,
        "action remove" => action_remove(&ctx).await,
        "action list" => action_list(&ctx, &settings).await,
        "response add" => response_add(&ctx).await,
        "response remove" => response_remove(&ctx).await,
        _ => Ok(()),
    };
    match outcome {
        Err(e) if as_tag_error(&e).is_some() => {
            let err = as_tag_error(&e).cloned().unwrap_or_else(|| TagError::new("ERROR", "Error", ""));
            fail(&ctx, &err).await
        }
        other => other, // the router's generic error reply
    }
}

fn required_string(ctx: &CommandCtx, name: &str) -> anyhow::Result<String> {
    ctx.get_string(name).ok_or_else(|| anyhow::anyhow!("Required option \"{name}\" not found."))
}

fn required_integer(ctx: &CommandCtx, name: &str) -> anyhow::Result<i64> {
    ctx.get_integer(name).ok_or_else(|| anyhow::anyhow!("Required option \"{name}\" not found."))
}

// ── use ───────────────────────────────────────────────────────────────────────

async fn use_command(ctx: &CommandCtx, settings: &Settings) -> BotResult {
    let guild_id = gid(ctx)?;
    let g = guild_id.to_string();
    let raw_name = required_string(ctx, "name")?;
    let name = normalize_tag_name(&raw_name);
    if name.is_empty() {
        return not_found(ctx, &raw_name, true).await;
    }
    let (a, g2, n2) = (ctx.app.clone(), g.clone(), name.clone());
    let lookup = bot_util::async_util::with_timeout(async move { svc::find_tag(&a, &g2, &n2).await }, ctx.ack_budget_ms(), "tag lookup").await;
    let tag = match lookup {
        Ok(t) => t,
        Err(bot_util::async_util::WithTimeoutError::Timeout(_)) => {
            return respond(ctx, warn_embed("Temporarily Unavailable", Some("I could not load that command in time. Please try again."), None)).await;
        }
        Err(bot_util::async_util::WithTimeoutError::Inner(e)) => return Err(e),
    };
    let Some(tag) = tag else { return not_found(ctx, &name, true).await };
    if !tag.enabled {
        return respond(ctx, error_embed("Disabled", Some("This custom command is turned off."), None)).await;
    }
    let args = svc::parse_args(&ctx.get_string("args").unwrap_or_default());
    let guild = cached_guild(&ctx.app, guild_id);
    // `interaction.channel`: an uncached one (a thread) is fetched, so its parent and category count for access.
    let channel = match resolve_channel(&ctx.app, guild_id, ctx.channel_id()).await {
        Some(c) => ChannelScope::of(guild.as_ref(), &c),
        None => ChannelScope::bare(&ctx.channel_id().to_string()),
    };
    let io = DiscordIo::new(&ctx.app, guild_id, Some(ctx.responder.clone()));
    let opts = RunOpts {
        source: Source::Slash,
        guild_id: g,
        channel,
        member_roles: member_roles(&ctx.app, ctx.guild_id(), ctx.member()),
        user_id: ctx.user().id.to_string(),
        args,
        settings: Some(settings),
        prefix: effective_prefix(Some(&settings.custom_prefix)),
        render: interaction_render_ctx(ctx, None),
        rng: None,
        check_permissions: true,
        ack_budget_ms: Some(ctx.ack_budget_ms()),
        message: None,
    };
    svc::run_tag(&ctx.app, &tag, opts, &io).await;
    Ok(())
}

// ── create ────────────────────────────────────────────────────────────────────

async fn create(ctx: &CommandCtx, settings: &Settings) -> BotResult {
    let g = gid(ctx)?.to_string();
    let name = match svc::validate_new_tag_name(&required_string(ctx, "name")?) {
        Ok(n) => n,
        Err(reason) => return respond(ctx, error_embed("Invalid Name", Some(reason), None)).await,
    };
    let content = required_string(ctx, "content")?;
    if bot_util::js::trim(&content).is_empty() {
        return respond(ctx, error_embed("Empty", Some("The reply text cannot be empty."), None)).await;
    }
    let embed = ctx.get_boolean("embed").unwrap_or(false);
    if let Some(problem) = length_problem(&content, embed) {
        return respond(ctx, error_embed("Too Long", Some(&problem), None)).await;
    }
    let description = clean_description(ctx.get_string("description").as_deref());

    ctx.app.settings().ensure_guild_settings(&g).await?;
    let created = svc::create_tag_row(
        &ctx.app,
        NewTag { guild_id: g.clone(), name: name.clone(), content: content.clone(), use_embed: embed, description, created_by: ctx.user().id.to_string() },
    )
    .await;
    let row = match created {
        Ok(r) => r,
        Err(e) if svc::is_unique_violation(&e) => {
            return respond(
                ctx,
                error_embed("Already Exists", Some(&format!("A custom command named `{name}` already exists. Use `/tag edit` to change it.")), None),
            )
            .await;
        }
        Err(e) => return Err(e), // TagError LIMIT is rendered by execute()
    };
    svc::invalidate_tag_index(&ctx.app, &g);

    let prefix = effective_prefix(Some(&settings.custom_prefix));
    let mut lines = vec![match &prefix {
        None => format!("Run it with `/tag use name:{name}`. {PREFIX_OFF_NOTE}"),
        Some(p) => format!("Run it with `{p}{name}` or `/tag use name:{name}`."),
    }];
    let usage = build_usage(&row, prefix.as_deref(), false);
    if usage != format!("{}{name}", prefix.clone().unwrap_or_default()) && usage != format!("/tag use name:{name}") {
        lines.push(format!("Usage: `{usage}`"));
    }
    if let Some(u) = unknown_for(ctx, &content) {
        lines.push(u);
    }
    lines.push("More: /tag action add · /tag permissions · /tag cooldown".into());
    respond(ctx, success_embed("Custom Command Created", Some(&lines.join("\n")), None)).await
}

// ── edit ──────────────────────────────────────────────────────────────────────

/// The mutator of `/tag edit` (pure): the fields to write for new content, embed flag, description and delete flag.
pub fn edit_patch(
    tag: &CustomTag,
    content: Option<&str>,
    embed: Option<bool>,
    description: Option<&str>,
    delete_usage: Option<bool>,
) -> Result<Patch, TagError> {
    let mut out: Patch = Vec::new();
    if content.is_some() || embed.is_some() {
        match normalize_actions(tag.actions.as_ref()) {
            None => {
                // Legacy tag: the text and the embed flag live in the columns.
                let use_embed = embed.unwrap_or(tag.use_embed);
                let text = content.unwrap_or(&tag.content);
                if let Some(problem) = length_problem(text, use_embed) {
                    return Err(TagError::new("TOO_LONG", "Too Long", problem));
                }
                out.push(("content", json!(text)));
                out.push(("useEmbed", json!(use_embed)));
            }
            Some(actions) => {
                let Some(i) = primary_message_index(&actions) else {
                    return Err(TagError::new("NO_MESSAGE", "No Message Action", "This command has no message action. Add one with `/tag action add`."));
                };
                let mut copy = actions;
                if let Some(c) = content {
                    copy[i].responses[0] = c.to_string();
                }
                if let Some(e) = embed {
                    copy[i].embed = e;
                }
                let limit = if copy[i].embed { svc::EMBED_TEXT_MAX } else { svc::PLAIN_MAX };
                for (k, r) in copy[i].responses.iter().enumerate() {
                    let n = bot_util::js::utf16_len(r);
                    if n > limit {
                        return Err(TagError::new(
                            "TOO_LONG",
                            "Too Long",
                            format!(
                                "Reply {} of action {} is {n} characters; {}.",
                                k + 1,
                                i + 1,
                                if copy[i].embed { "embed text allows 4,000" } else { "plain text allows 2,000" }
                            ),
                        ));
                    }
                }
                out.push(("actions", actions_to_json(&copy)));
                out.extend(sync_legacy_columns(&copy));
            }
        }
    }
    if let Some(d) = description {
        out.push(("description", if d == "-" { Value::Null } else { json!(clean_description(Some(d))) }));
    }
    if let Some(d) = delete_usage {
        out.push(("deleteInvocation", json!(d)));
    }
    Ok(out)
}

async fn edit(ctx: &CommandCtx) -> BotResult {
    let guild_id = gid(ctx)?;
    let g = guild_id.to_string();
    let raw_name = required_string(ctx, "name")?;
    let name = normalize_tag_name(&raw_name);
    if name.is_empty() {
        return not_found(ctx, &raw_name, false).await;
    }
    let content = ctx.get_string("content");
    let embed = ctx.get_boolean("embed");
    let description = ctx.get_string("description");
    let delete_usage = ctx.get_boolean("delete_usage");
    if content.is_none() && embed.is_none() && description.is_none() && delete_usage.is_none() {
        return respond(ctx, error_embed("Nothing to Change", Some("Give at least one of content, embed, description or delete_usage."), None)).await;
    }
    if content.as_deref().is_some_and(|c| bot_util::js::trim(c).is_empty()) {
        return respond(ctx, error_embed("Empty", Some("The reply text cannot be empty."), None)).await;
    }
    let guard = Guard::of(ctx);
    let (c, d) = (content.clone(), description.clone());
    let row = svc::write_tag(&ctx.app, &g, &name, move |tag| {
        guard.check(tag)?;
        edit_patch(tag, c.as_deref(), embed, d.as_deref(), delete_usage)
    })
    .await?;

    let mut lines: Vec<String> = Vec::new();
    if content.is_some() {
        lines.push("Reply text updated.".into());
    }
    if let Some(e) = embed {
        lines.push(format!("Embed: **{}**.", if e { "on" } else { "off" }));
    }
    if let Some(d) = &description {
        lines.push((if d == "-" { "Description removed." } else { "Description updated." }).into());
    }
    if let Some(d) = delete_usage {
        lines.push(format!("Delete usage: **{}**.", if d { "on" } else { "off" }));
    }
    if delete_usage == Some(true) && !missing_bot_permissions(&ctx.app, PermTarget::Guild(guild_id), &["ManageMessages"]).is_empty() {
        lines.push("⚠️ I need **Manage Messages** to delete usages; without it the message stays.".into());
    }
    if let Some(c) = &content
        && let Some(u) = unknown_for(ctx, c)
    {
        lines.push(u);
    }
    respond(ctx, success_embed("Custom Command Updated", Some(&format!("`{}`\n{}", row.name, lines.join("\n"))), None)).await
}

// ── delete / toggle ───────────────────────────────────────────────────────────

async fn remove(ctx: &CommandCtx) -> BotResult {
    let g = gid(ctx)?.to_string();
    let raw_name = required_string(ctx, "name")?;
    let Some(tag) = svc::find_tag(&ctx.app, &g, &raw_name).await? else {
        let n = normalize_tag_name(&raw_name);
        return not_found(ctx, if n.is_empty() { &raw_name } else { &n }, false).await;
    };
    if !svc::delete_tag_row(&ctx.app, &tag.id).await? {
        return not_found(ctx, &tag.name, false).await;
    }
    svc::clear_tag_cooldowns(&ctx.app, &g, &tag.id).await;
    svc::invalidate_tag_index(&ctx.app, &g);
    respond(ctx, success_embed("Custom Command Deleted", Some(&format!("`{}` ({} uses) was deleted.", tag.name, fmt_int(i64::from(tag.uses)))), None)).await
}

async fn toggle(ctx: &CommandCtx) -> BotResult {
    let g = gid(ctx)?.to_string();
    let raw_name = required_string(ctx, "name")?;
    let name = normalize_tag_name(&raw_name);
    if name.is_empty() {
        return not_found(ctx, &raw_name, false).await;
    }
    let guard = Guard::of(ctx);
    let row = svc::write_tag(&ctx.app, &g, &name, move |t| {
        guard.check(t)?;
        Ok(vec![("enabled", json!(!t.enabled))])
    })
    .await?;
    respond(
        ctx,
        success_embed(
            if row.enabled { "Custom Command Enabled" } else { "Custom Command Disabled" },
            Some(&format!("`{}` is now **{}**.", row.name, if row.enabled { "on" } else { "off" })),
            None,
        ),
    )
    .await
}

// ── cooldown ──────────────────────────────────────────────────────────────────

/// What `/tag cooldown` writes, or the refusal embed's title and text.
pub fn cooldown_patch(scope: &str, duration: Option<&str>) -> Result<(Patch, &'static str), (&'static str, &'static str)> {
    if scope == "NONE" {
        let note = if duration.is_some_and(|d| !d.is_empty()) { " (duration ignored)" } else { "" };
        return Ok((vec![("cooldownScope", json!("NONE")), ("cooldownSeconds", json!(0))], note));
    }
    let Some(duration) = duration.filter(|d| !d.is_empty()) else {
        return Err(("Duration Needed", "Give a duration such as 30s, 10m, 2h or 1d."));
    };
    let ms = match bot_util::parse_duration::parse_duration(duration) {
        Some(ms) if ms >= 1000 => ms,
        _ => return Err(("Invalid Duration", "I couldn't read that. Use something like 30s, 10m, 2h or 1d (at least 1 second).")),
    };
    if ms > svc::COOLDOWN_MAX_SECONDS * 1000 {
        return Err(("Too Long", "The longest cooldown is 365 days."));
    }
    let seconds = bot_util::text::js_round(ms as f64 / 1000.0) as i64;
    Ok((vec![("cooldownScope", json!(scope)), ("cooldownSeconds", json!(seconds))], ""))
}

async fn cooldown(ctx: &CommandCtx) -> BotResult {
    let g = gid(ctx)?.to_string();
    let raw_name = required_string(ctx, "name")?;
    let name = normalize_tag_name(&raw_name);
    if name.is_empty() {
        return not_found(ctx, &raw_name, false).await;
    }
    let scope = required_string(ctx, "scope")?;
    let (patch, note) = match cooldown_patch(&scope, ctx.get_string("duration").as_deref()) {
        Ok(p) => p,
        Err((title, text)) => return respond(ctx, error_embed(title, Some(text), None)).await,
    };
    let guard = Guard::of(ctx);
    let row = svc::write_tag(&ctx.app, &g, &name, move |t| {
        guard.check(t)?;
        Ok(patch.clone())
    })
    .await?;
    svc::clear_tag_cooldowns(&ctx.app, &g, &row.id).await;
    let label = cooldown_label(row.cooldown_scope, i64::from(row.cooldown_seconds));
    respond(ctx, success_embed("Cooldown Updated", Some(&format!("`{}`: **{label}**{note}", row.name)), None)).await
}

// ── permissions (allow and deny lists) ────────────────────────────────────────

/// The four lists after applying `rule` (`allow` / `deny` / `remove`) to a role and a channel, or the LIMIT error.
pub fn apply_access_rule(tag: &CustomTag, rule: &str, role: Option<&str>, channel: Option<&str>) -> Result<Patch, TagError> {
    let mut lists: [(&'static str, Vec<String>); 4] = [
        ("allowRoleIds", tag.allow_role_ids.clone()),
        ("denyRoleIds", tag.deny_role_ids.clone()),
        ("allowChannelIds", tag.allow_channel_ids.clone()),
        ("denyChannelIds", tag.deny_channel_ids.clone()),
    ];
    let mut apply = |id: &str, allow: usize, deny: usize| {
        let without = |l: &mut Vec<String>| l.retain(|x| x != id);
        let put = |l: &mut Vec<String>| {
            if !l.iter().any(|x| x == id) {
                l.push(id.to_string());
            }
        };
        match rule {
            "allow" => {
                put(&mut lists[allow].1);
                without(&mut lists[deny].1);
            }
            "deny" => {
                put(&mut lists[deny].1);
                without(&mut lists[allow].1);
            }
            _ => {
                without(&mut lists[allow].1);
                without(&mut lists[deny].1);
            }
        }
    };
    if let Some(r) = role {
        apply(r, 0, 1);
    }
    if let Some(c) = channel {
        apply(c, 2, 3);
    }
    if lists.iter().any(|(_, l)| l.len() > svc::TAG_ACCESS_MAX) {
        return Err(TagError::new("LIMIT", "Limit Reached", format!("A list can hold at most {} entries.", svc::TAG_ACCESS_MAX)));
    }
    Ok(lists.into_iter().map(|(k, v)| (k, json!(v))).collect())
}

async fn permissions(ctx: &CommandCtx, settings: &Settings) -> BotResult {
    let guild_id = gid(ctx)?;
    let g = guild_id.to_string();
    let raw_name = required_string(ctx, "name")?;
    let name = normalize_tag_name(&raw_name);
    if name.is_empty() {
        return not_found(ctx, &raw_name, false).await;
    }
    let rule = required_string(ctx, "rule")?;
    let role = ctx.get_role_id("role").map(|r| r.to_string());
    let channel = ctx.get_channel_id("channel").map(|c| c.to_string());
    if rule != "clear" {
        if role.is_none() && channel.is_none() {
            return respond(ctx, error_embed("Nothing Selected", Some("Pick a role, a channel, or both."), None)).await;
        }
        if role.as_deref() == Some(g.as_str()) {
            return respond(
                ctx,
                error_embed(
                    "Not Allowed",
                    Some("@everyone cannot be used here. Use /tag toggle to turn the command off, or rule: Clear all lists to remove limits."),
                    None,
                ),
            )
            .await;
        }
    }
    let guard = Guard::of(ctx);
    let row = svc::write_tag(&ctx.app, &g, &name, move |tag| {
        guard.check(tag)?;
        if rule == "clear" {
            return Ok(vec![("allowRoleIds", json!([])), ("denyRoleIds", json!([])), ("allowChannelIds", json!([])), ("denyChannelIds", json!([]))]);
        }
        apply_access_rule(tag, &rule, role.as_deref(), channel.as_deref())
    })
    .await?;
    let guild = cached_guild(&ctx.app, guild_id);
    let embed = apply_guild_color(success_embed("Permissions Updated", Some(&format!("`{}`", row.name)), None), Some(settings))
        .field("Allowed roles", format_id_list(&row.allow_role_ids, IdKind::Role, guild.as_ref(), "Everyone", 1024), true)
        .field("Denied roles", format_id_list(&row.deny_role_ids, IdKind::Role, guild.as_ref(), "None", 1024), true)
        .field("Allowed channels", format_id_list(&row.allow_channel_ids, IdKind::Channel, guild.as_ref(), "Everywhere", 1024), true)
        .field("Denied channels", format_id_list(&row.deny_channel_ids, IdKind::Channel, guild.as_ref(), "None", 1024), true);
    respond(ctx, embed).await
}

// ── action add / remove / list ────────────────────────────────────────────────

/// The option sanity of `/tag action add`: options that do not belong to the action are refused, never ignored.
/// `Err((title, text))` is the refusal.
pub fn action_option_problem(
    kind: &str,
    text: Option<&str>,
    has_channel: bool,
    has_role: bool,
    embed_given: bool,
    has_ping: bool,
) -> Option<(&'static str, &'static str)> {
    let blank = text.is_none_or(|t| bot_util::js::trim(t).is_empty());
    match kind {
        "reply" | "dm" => {
            if blank {
                return Some(("Missing Text", "This action needs `text`."));
            }
            if has_role || has_channel {
                return Some(("Wrong Option", "`role`/`channel` are not used by this action."));
            }
            if kind == "dm" && has_ping {
                return Some(("Wrong Option", "DMs never ping roles."));
            }
        }
        "send" => {
            if blank || !has_channel {
                return Some(("Missing Option", "This action needs `text` and `channel`."));
            }
            if has_role {
                return Some(("Wrong Option", "`role` is not used by this action."));
            }
        }
        _ => {
            if !has_role {
                return Some(("Missing Role", "This action needs `role`."));
            }
            if text.is_some() || has_channel || embed_given || has_ping {
                return Some(("Wrong Option", "`text`, `channel`, `embed` and `ping_role` are not used by role actions."));
            }
        }
    }
    None
}

/// The action list with `action` appended (the LIMIT error past `LIMITS.TAG_ACTIONS`), and its new length.
pub fn append_action(tag: &CustomTag, action: &TagAction) -> Result<(Patch, usize), TagError> {
    let mut list = normalize_actions(tag.actions.as_ref()).unwrap_or_else(|| legacy_actions(&tag.content, tag.use_embed));
    if list.len() >= limits::TAG_ACTIONS {
        return Err(TagError::new("LIMIT", "Limit Reached", format!("A custom command can have at most {} actions.", limits::TAG_ACTIONS)));
    }
    list.push(action.clone());
    let mut patch: Patch = vec![("actions", actions_to_json(&list))];
    patch.extend(sync_legacy_columns(&list));
    Ok((patch, list.len()))
}

async fn action_add(ctx: &CommandCtx) -> BotResult {
    let guild_id = gid(ctx)?;
    let g = guild_id.to_string();
    let raw_name = required_string(ctx, "name")?;
    let name = normalize_tag_name(&raw_name);
    if name.is_empty() {
        return not_found(ctx, &raw_name, false).await;
    }
    let kind = required_string(ctx, "type")?;
    let text = ctx.get_string("text");
    let channel = ctx.get_channel_id("channel");
    let role = ctx.get_role_id("role");
    let embed_opt = ctx.get_boolean("embed");
    let ping_role = ctx.get_role_id("ping_role");
    let embed = embed_opt.unwrap_or(false);

    // 1. Option sanity.
    if let Some((title, msg)) = action_option_problem(&kind, text.as_deref(), channel.is_some(), role.is_some(), embed_opt.is_some(), ping_role.is_some()) {
        return respond(ctx, error_embed(title, Some(msg), None)).await;
    }

    let action = if let (Some(k @ (ActionKind::AddRole | ActionKind::RemoveRole | ActionKind::ToggleRole)), Some(role)) = (ActionKind::parse(&kind), role) {
        // 4. Role actions: the executor must be allowed to hand out the role too.
        let r = check_assignable_role_cached(&ctx.app, guild_id, role, ctx.member(), false);
        if !r.ok {
            return respond(ctx, error_embed("Role Not Allowed", r.reason.as_deref(), None)).await;
        }
        TagAction::role(k, &role.to_string())
    } else {
        let text = text.clone().unwrap_or_default();
        // 2. Text length.
        if let Some(problem) = length_problem(&text, embed) {
            return respond(ctx, error_embed("Too Long", Some(&problem), None)).await;
        }
        // 3. The send channel (the JS had the option's resolved channel: an uncached one is fetched over REST).
        let send_channel = match channel.filter(|_| kind == "send") {
            Some(c) => resolve_channel(&ctx.app, guild_id, c).await,
            None => None,
        };
        if kind == "send"
            && let Some(c) = channel
        {
            let needs: &[&str] = if embed { &["ViewChannel", "SendMessages", "EmbedLinks"] } else { &["ViewChannel", "SendMessages"] };
            let usable = match &send_channel {
                Some(ch) => check_channel_usable_for(&ctx.app, guild_id, ch, needs),
                None => check_channel_usable(&ctx.app, guild_id, Some(c), needs),
            };
            if !usable.ok {
                let reason = usable.reason.unwrap_or_else(|| format!("I cannot post in <#{c}>."));
                return respond(ctx, error_embed("Missing Permissions", Some(&reason), None)).await;
            }
            // The admin must be able to post there too: a command is not a way into a channel the admin cannot use.
            if let Some(guild) = cached_guild(&ctx.app, guild_id)
                && let Some(access) = member_channel_problem(&guild, send_channel.as_ref(), ctx.member().map(MemberRef::of), &MEMBER_CHANNEL_NEEDS)
            {
                return respond(ctx, error_embed("No Access", Some(&access), None)).await;
            }
        }
        // 5. ping_role: the executor needs Mention Everyone, or the role must be mentionable.
        if let Some(p) = ping_role {
            if p.get() == guild_id.get() {
                return respond(ctx, error_embed("Not Allowed", Some("@everyone can never be pinged by a custom command."), None)).await;
            }
            let target = match send_channel {
                Some(ch) => Some(ch),
                None => resolve_channel(&ctx.app, guild_id, if kind == "send" { channel.unwrap_or(ctx.channel_id()) } else { ctx.channel_id() }).await,
            };
            if !ping_allowed(ctx, guild_id, target.as_ref(), p) {
                return respond(
                    ctx,
                    error_embed(
                        "Not Allowed",
                        Some("You need the **Mention @everyone, @here and All Roles** permission to make a command ping a role that is not mentionable."),
                        None,
                    ),
                )
                .await;
            }
        }
        // 6. Build the action.
        let mut a = match kind.as_str() {
            "send" => TagAction::send(&channel.map(|c| c.to_string()).unwrap_or_default(), embed, &text),
            "dm" => TagAction::reply(true, embed, &text),
            _ => TagAction::reply(false, embed, &text),
        };
        if let Some(p) = ping_role
            && kind != "dm"
        {
            a.ping_role_ids = vec![p.to_string()];
        }
        a
    };

    // 7. Write.
    let guard = Guard::of(ctx);
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (a2, n2) = (action.clone(), count.clone());
    svc::write_tag(&ctx.app, &g, &name, move |tag| {
        guard.check(tag)?;
        let (patch, n) = append_action(tag, &a2)?;
        n2.store(n, std::sync::atomic::Ordering::SeqCst);
        Ok(patch)
    })
    .await?;

    // 8. Reply.
    let guild = cached_guild(&ctx.app, guild_id);
    let mut lines = vec![format!("Action {}: {}", count.load(std::sync::atomic::Ordering::SeqCst), describe_action(&action, guild.as_ref()))];
    if let Some(t) = text.as_deref().filter(|t| !t.is_empty())
        && let Some(u) = unknown_for(ctx, t)
    {
        lines.push(u);
    }
    respond(ctx, success_embed("Action Added", Some(&lines.join("\n")), None)).await
}

/// `pingableRoleIds(target, interaction.member, [role]).refused` is empty. Unknown guild or channel: refused.
fn ping_allowed(ctx: &CommandCtx, guild_id: GuildId, target: Option<&GuildChannel>, role: RoleId) -> bool {
    let Some(guild) = cached_guild(&ctx.app, guild_id) else { return false };
    let Some(channel) = target else { return false };
    let executor = ctx.member().map(|m| (m.user.id, m.roles.as_slice()));
    pingable_role_ids(&guild, channel, executor, &[role.to_string()]).refused.is_empty()
}

/// The action list without action `n` (1-based), the removed action, or the refusal.
pub fn remove_action(tag: &CustomTag, n: i64) -> Result<(Patch, TagAction), TagError> {
    let list = normalize_actions(tag.actions.as_ref()).unwrap_or_else(|| legacy_actions(&tag.content, tag.use_embed));
    if n > list.len() as i64 || n < 1 {
        return Err(TagError::new("NOT_FOUND", "Not Found", format!("This command has {} action(s).", list.len())));
    }
    if list.len() == 1 {
        return Err(TagError::new("LAST", "Last Action", "A custom command needs at least one action. Delete the command with /tag delete instead."));
    }
    let i = (n - 1) as usize;
    let removed = list[i].clone();
    let next: Vec<TagAction> = list.into_iter().enumerate().filter(|(k, _)| *k != i).map(|(_, a)| a).collect();
    let mut patch: Patch = vec![("actions", actions_to_json(&next))];
    patch.extend(sync_legacy_columns(&next));
    Ok((patch, removed))
}

async fn action_remove(ctx: &CommandCtx) -> BotResult {
    let guild_id = gid(ctx)?;
    let g = guild_id.to_string();
    let raw_name = required_string(ctx, "name")?;
    let name = normalize_tag_name(&raw_name);
    if name.is_empty() {
        return not_found(ctx, &raw_name, false).await;
    }
    let n = required_integer(ctx, "action")?;
    let guard = Guard::of(ctx);
    let removed: std::sync::Arc<std::sync::Mutex<Option<TagAction>>> = Default::default();
    let r2 = removed.clone();
    svc::write_tag(&ctx.app, &g, &name, move |tag| {
        guard.check(tag)?;
        let (patch, a) = remove_action(tag, n)?;
        *r2.lock().unwrap_or_else(|e| e.into_inner()) = Some(a);
        Ok(patch)
    })
    .await?;
    let guild = cached_guild(&ctx.app, guild_id);
    let removed = removed.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let text = removed.map(|a| describe_action(&a, guild.as_ref())).unwrap_or_default();
    respond(ctx, success_embed("Action Removed", Some(&format!("Removed: {text}")), None)).await
}

/// The lines of `/tag action list`.
pub fn action_list_lines(actions: &[TagAction], guild: Option<&Guild>) -> Vec<String> {
    let mut lines = Vec::new();
    for (i, a) in actions.iter().enumerate() {
        lines.push(format!("`{}.` {}", i + 1, describe_action(a, guild)));
        if a.is_message() {
            for (k, r) in a.responses.iter().enumerate() {
                lines.push(format!("↳ `{}` {}", k + 1, code_safe(&clamp_text(&one_line(r), 80))));
            }
        }
    }
    lines
}

async fn action_list(ctx: &CommandCtx, settings: &Settings) -> BotResult {
    let guild_id = gid(ctx)?;
    let raw_name = required_string(ctx, "name")?;
    let tag = svc::find_tag(&ctx.app, &guild_id.to_string(), &raw_name).await?;
    let Some(tag) = tag.filter(|t| !hidden_from_member(ctx, t, settings)) else {
        let n = normalize_tag_name(&raw_name);
        return not_found(ctx, if n.is_empty() { &raw_name } else { &n }, false).await;
    };
    svc::warn_unknown_actions_format(&ctx.app, &tag);
    let guild = cached_guild(&ctx.app, guild_id);
    let lines = action_list_lines(&get_effective_actions(&tag), guild.as_ref());
    let body = join_with_limit(&lines, "\n", 4000, default_more);
    respond(ctx, apply_guild_color(info_embed(&format!("Actions of {}", tag.name), Some(&body), None), Some(settings))).await
}

// ── response add / remove (random replies) ────────────────────────────────────

/// The action list with a random reply added to action `action` (1-based; default the first message action).
/// Returns the patch, the action number and its new reply count.
pub fn add_response(tag: &CustomTag, action: Option<i64>, text: &str) -> Result<(Patch, usize, usize), TagError> {
    let mut list = normalize_actions(tag.actions.as_ref()).unwrap_or_else(|| legacy_actions(&tag.content, tag.use_embed));
    let i: i64 = match action {
        Some(a) if a != 0 => a - 1,
        _ => primary_message_index(&list).map(|i| i as i64).unwrap_or(-1),
    };
    if i < 0 || i >= list.len() as i64 || !list[i as usize].is_message() {
        return Err(TagError::new("NO_MESSAGE", "No Message Action", "Pick a reply, DM or send action."));
    }
    let i = i as usize;
    if list[i].responses.len() >= limits::TAG_RESPONSES {
        return Err(TagError::new("LIMIT", "Limit Reached", format!("A message action can have at most {} replies.", limits::TAG_RESPONSES)));
    }
    if let Some(problem) = length_problem(text, list[i].embed) {
        return Err(TagError::new("TOO_LONG", "Too Long", problem));
    }
    list[i].responses.push(text.to_string());
    let count = list[i].responses.len();
    let mut patch: Patch = vec![("actions", actions_to_json(&list))];
    patch.extend(sync_legacy_columns(&list));
    Ok((patch, i + 1, count))
}

async fn response_add(ctx: &CommandCtx) -> BotResult {
    let g = gid(ctx)?.to_string();
    let raw_name = required_string(ctx, "name")?;
    let name = normalize_tag_name(&raw_name);
    if name.is_empty() {
        return not_found(ctx, &raw_name, false).await;
    }
    let text = required_string(ctx, "text")?;
    if bot_util::js::trim(&text).is_empty() {
        return respond(ctx, error_embed("Empty", Some("The reply text cannot be empty."), None)).await;
    }
    let action = ctx.get_integer("action");
    let guard = Guard::of(ctx);
    let out: std::sync::Arc<std::sync::Mutex<(usize, usize)>> = Default::default();
    let (o2, t2) = (out.clone(), text.clone());
    svc::write_tag(&ctx.app, &g, &name, move |tag| {
        guard.check(tag)?;
        let (patch, at, count) = add_response(tag, action, &t2)?;
        *o2.lock().unwrap_or_else(|e| e.into_inner()) = (at, count);
        Ok(patch)
    })
    .await?;
    let (at, count) = *out.lock().unwrap_or_else(|e| e.into_inner());
    let mut lines = vec![format!("Action {at} now has {count} replies; one is picked at random each time.")];
    if let Some(u) = unknown_for(ctx, &text) {
        lines.push(u);
    }
    respond(ctx, success_embed("Reply Added", Some(&lines.join("\n")), None)).await
}

/// The action list without reply `k` of action `n` (both 1-based), and the replies left.
pub fn remove_response(tag: &CustomTag, n: i64, k: i64) -> Result<(Patch, usize), TagError> {
    let mut list = normalize_actions(tag.actions.as_ref()).unwrap_or_else(|| legacy_actions(&tag.content, tag.use_embed));
    let idx = usize::try_from(n - 1).ok().filter(|i| *i < list.len() && list[*i].is_message());
    let Some(i) = idx else { return Err(TagError::new("NOT_FOUND", "Not Found", format!("Action {n} is not a message action."))) };
    let len = list[i].responses.len();
    if k > len as i64 {
        return Err(TagError::new("NOT_FOUND", "Not Found", format!("Action {n} has {len} reply(ies).")));
    }
    if len == 1 {
        return Err(TagError::new("LAST", "Last Reply", "This is the only reply of the action. Remove the action with /tag action remove instead."));
    }
    let at = usize::try_from(k - 1).unwrap_or(0).min(len - 1);
    list[i].responses.remove(at);
    let left = list[i].responses.len();
    let mut patch: Patch = vec![("actions", actions_to_json(&list))];
    patch.extend(sync_legacy_columns(&list));
    Ok((patch, left))
}

async fn response_remove(ctx: &CommandCtx) -> BotResult {
    let g = gid(ctx)?.to_string();
    let raw_name = required_string(ctx, "name")?;
    let name = normalize_tag_name(&raw_name);
    if name.is_empty() {
        return not_found(ctx, &raw_name, false).await;
    }
    let n = required_integer(ctx, "action")?;
    let k = required_integer(ctx, "reply")?;
    let guard = Guard::of(ctx);
    let left = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let l2 = left.clone();
    svc::write_tag(&ctx.app, &g, &name, move |tag| {
        guard.check(tag)?;
        let (patch, l) = remove_response(tag, n, k)?;
        l2.store(l, std::sync::atomic::Ordering::SeqCst);
        Ok(patch)
    })
    .await?;
    let left = left.load(std::sync::atomic::Ordering::SeqCst);
    respond(ctx, success_embed("Reply Removed", Some(&format!("Removed reply {k} of action {n}; {left} left.")), None)).await
}

// ── info ──────────────────────────────────────────────────────────────────────

/// The `/tag info` embed (title, description and fields; footer `ID: …`).
pub fn info_embed_for(tag: &CustomTag, prefix: Option<&str>, guild: Option<&Guild>, settings: Option<&GuildSettings>) -> CreateEmbed {
    let mut usage_lines: Vec<String> = Vec::new();
    if let Some(p) = prefix {
        usage_lines.push(format!("`{}`", build_usage(tag, Some(p), false)));
    }
    usage_lines.push(format!("`{}`", build_usage(tag, Some("!"), true)));
    if prefix.is_none() {
        usage_lines.push(PREFIX_OFF_NOTE.into());
    }
    let mut access: Vec<String> = Vec::new();
    for (label, ids, kind) in [
        ("Allowed roles", &tag.allow_role_ids, IdKind::Role),
        ("Denied roles", &tag.deny_role_ids, IdKind::Role),
        ("Allowed channels", &tag.allow_channel_ids, IdKind::Channel),
        ("Denied channels", &tag.deny_channel_ids, IdKind::Channel),
    ] {
        if !ids.is_empty() {
            access.push(format!("{label}: {}", format_id_list(ids, kind, guild, "None", 240)));
        }
    }
    let action_lines: Vec<String> = get_effective_actions(tag).iter().enumerate().map(|(i, a)| format!("{}. {}", i + 1, describe_action(a, guild))).collect();
    let actions_text = join_with_limit(&action_lines, "\n", 1024, default_more);
    let description = tag.description.clone().unwrap_or_else(|| "*No description*".into());
    apply_guild_color(info_embed(&format!("🏷️ {}", shown_name(prefix, &tag.name)), Some(&description), None), settings)
        .field("Usage", clamp_text(&usage_lines.join("\n"), 1024), false)
        .field("Status", if tag.enabled { "✅ Enabled" } else { "⏸️ Disabled" }, true)
        .field("Uses", fmt_int(i64::from(tag.uses)), true)
        .field("Created by", format!("<@{}>", tag.created_by), true)
        .field("Cooldown", cooldown_label(tag.cooldown_scope, i64::from(tag.cooldown_seconds)), true)
        .field("Delete usage", if tag.delete_invocation { "Yes (prefix only)" } else { "No" }, true)
        .field("Access", clamp_text(&if access.is_empty() { "Everyone, everywhere".to_string() } else { access.join(" · ") }, 1024), false)
        .field("Actions", if actions_text.is_empty() { "—".to_string() } else { actions_text }, false)
        .field("Created", time_tag(&tag.created_at), true)
        .field("Updated", time_tag(&tag.updated_at), true)
        .footer(CreateEmbedFooter::new(format!("ID: {}", tag.id)))
}

async fn info(ctx: &CommandCtx, settings: &Settings) -> BotResult {
    let guild_id = gid(ctx)?;
    let raw_name = required_string(ctx, "name")?;
    let tag = svc::find_tag(&ctx.app, &guild_id.to_string(), &raw_name).await?;
    let Some(tag) = tag.filter(|t| !hidden_from_member(ctx, t, settings)) else {
        let n = normalize_tag_name(&raw_name);
        return not_found(ctx, if n.is_empty() { &raw_name } else { &n }, false).await;
    };
    svc::warn_unknown_actions_format(&ctx.app, &tag);
    let prefix = effective_prefix(Some(&settings.custom_prefix));
    let guild = cached_guild(&ctx.app, guild_id);
    respond(ctx, info_embed_for(&tag, prefix.as_deref(), guild.as_ref(), Some(settings))).await
}

// ── raw ───────────────────────────────────────────────────────────────────────

/// The `/tag raw` answer: the message text and the attached files `(name, bytes)`.
pub fn raw_answer(tag: &CustomTag) -> (String, Vec<(String, Vec<u8>)>) {
    let text = &tag.content;
    let actions = normalize_actions(tag.actions.as_ref());
    // Room for the extra line when the actions JSON is attached too (the message limit is 2000).
    let inline_max = if actions.is_some() { 1840 } else { 1900 };
    let n = bot_util::js::utf16_len(text);
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut content = if n <= inline_max && !text.contains("```") {
        // No escaping: Discord shows single backticks literally inside a code block.
        format!("**Raw text of `{}`:**\n```\n{text}\n```", tag.name)
    } else {
        files.push((format!("{}.txt", tag.name), text.as_bytes().to_vec()));
        format!("**Raw text of `{}`** is attached ({n} characters).", tag.name)
    };
    if let Some(a) = actions {
        let pretty = serde_json::to_string_pretty(&actions_to_json(&a)).unwrap_or_default();
        files.push((format!("{}-actions.json", tag.name), pretty.into_bytes()));
        content.push_str("\nEvery action and reply is in the attached JSON.");
    }
    (content, files)
}

async fn raw(ctx: &CommandCtx, settings: &Settings) -> BotResult {
    let guild_id = gid(ctx)?;
    let raw_name = required_string(ctx, "name")?;
    let tag = svc::find_tag(&ctx.app, &guild_id.to_string(), &raw_name).await?;
    let Some(tag) = tag.filter(|t| !hidden_from_member(ctx, t, settings)) else {
        let n = normalize_tag_name(&raw_name);
        return not_found(ctx, if n.is_empty() { &raw_name } else { &n }, false).await;
    };
    let (content, files) = raw_answer(&tag);
    let mut payload = Payload::text(content).no_mentions().ephemeral(true);
    for (name, bytes) in files {
        payload = payload.file(CreateAttachment::bytes(bytes, name));
    }
    safe_reply(&ctx.responder, payload).await;
    Ok(())
}

// ── list ──────────────────────────────────────────────────────────────────────

/// One line of `/tag list`.
pub fn list_line(r: &CustomTag, prefix: Option<&str>) -> String {
    let mut flags: Vec<String> = Vec::new();
    if !r.enabled {
        flags.push("⏸️".into());
    }
    if r.use_embed {
        flags.push("🖼️".into());
    }
    if r.cooldown_scope != CooldownScope::None {
        flags.push("⏳".into());
    }
    if !(r.allow_role_ids.is_empty() && r.deny_role_ids.is_empty() && r.allow_channel_ids.is_empty() && r.deny_channel_ids.is_empty()) {
        flags.push("🔒".into());
    }
    let n = get_effective_actions(r).len();
    if n > 1 {
        flags.push(format!("⚙️×{n}"));
    }
    let uses = i64::from(r.uses);
    let what = clamp_text(&one_line(r.description.as_deref().unwrap_or(&r.content)), 60);
    let flag_text = if flags.is_empty() { String::new() } else { format!(" {}", flags.join(" ")) };
    clamp_text(
        &format!("`{}{}` — {what} · {} use{}{flag_text}", prefix.unwrap_or(""), r.name, fmt_int(uses), if uses == 1 { "" } else { "s" }),
        150,
    )
}

/// The rows `/tag list` shows (uses desc, then name).
pub async fn list_rows(app: &App, guild_id: &str, only_enabled: bool) -> anyhow::Result<Vec<CustomTag>> {
    let mut q = bot_db::SelectBuilder::new(&bot_db::CUSTOM_TAG_INFO).filter("guildId", json!(guild_id))?;
    if only_enabled {
        q = q.filter("enabled", json!(true))?;
    }
    Ok(q.order_by("uses", false)?.order_by("name", true)?.build()?.fetch_all(app.db()).await?)
}

async fn list(ctx: &CommandCtx, settings: &Settings) -> BotResult {
    let g = gid(ctx)?.to_string();
    let admin = is_admin(ctx, settings);
    let found = list_rows(&ctx.app, &g, !admin).await?;
    let roles = member_roles(&ctx.app, ctx.guild_id(), ctx.member());
    let rows: Vec<CustomTag> = if admin { found } else { found.into_iter().filter(|r| !member_cannot_see(r, roles.as_deref())).collect() };
    if rows.is_empty() {
        return respond(
            ctx,
            info_embed("No Custom Commands", Some(if admin { "Create one with `/tag create`." } else { "This server has no custom commands yet." }), None),
        )
        .await;
    }
    let prefix = effective_prefix(Some(&settings.custom_prefix));
    let lines: Vec<String> = rows.iter().map(|r| list_line(r, prefix.as_deref())).collect();
    let footer = match &prefix {
        None => "Prefix commands are off · /tag use name:<name>".to_string(),
        Some(p) => format!("Prefix: {p} · /tag use name:<name>"),
    };
    let pages: Vec<CreateEmbed> = lines
        .chunks(LIST_PAGE_SIZE)
        .map(|chunk| apply_guild_color(info_embed(&format!("🏷️ Custom Commands ({})", rows.len()), Some(&chunk.join("\n")), Some(&footer)), Some(settings)))
        .collect();
    if pages.len() == 1 {
        let page = pages.into_iter().next().unwrap_or_default();
        ctx.edit_reply(Payload::embed(page).no_mentions().to_edit()).await?;
        return Ok(());
    }
    paginate(ctx, &pages, PaginateOptions { ephemeral: true, ..Default::default() }).await?;
    Ok(())
}

// ── placeholders ──────────────────────────────────────────────────────────────

/// The `/tag placeholders` embed.
pub fn placeholders_embed(prefix: Option<&str>, settings: Option<&GuildSettings>) -> CreateEmbed {
    let args = [
        "`{1}` first argument (required: missing → usage reply)",
        "`{1:text}` first argument, or \"text\" when missing",
        "`{2...}` argument 2 to the end · `{...3}` arguments 1–3",
        "`{2...4}` arguments 2–4 · `{...}` all arguments",
    ]
    .join("\n");
    let footer = match prefix {
        None => "Run with /tag use name:<name> args:arg1 arg2 (prefix commands are off)".to_string(),
        Some(p) => format!("Run with {p}name arg1 arg2 or /tag use name:<name> args:arg1 arg2"),
    };
    apply_guild_color(info_embed("Custom Command Variables", Some(&clamp_text(&placeholder_help(&["user", "server", "channel"]), 4096)), None), settings)
        .field("Arguments", clamp_text(&args, 1024), false)
        .field("Random", "`{random}` 0–10 · `{random:1:6}` a number from 1 to 6", false)
        .footer(CreateEmbedFooter::new(footer))
}

async fn placeholders(ctx: &CommandCtx, settings: &Settings) -> BotResult {
    let prefix = effective_prefix(Some(&settings.custom_prefix));
    safe_reply(&ctx.responder, Payload::embed(placeholders_embed(prefix.as_deref(), Some(settings))).no_mentions().ephemeral(true)).await;
    Ok(())
}

// ── trigger group: helpers ────────────────────────────────────────────────────

const TRIGGER_PAGE_SIZE: usize = 10;

/// The note when the module is off.
pub const MODULE_OFF_NOTE: &str =
    "⚠️ The **Custom Commands** module is off, so triggers do not fire yet. Turn it on with `/tag trigger enable` (or `/module enable customcommands`).";
/// The note when triggers are switched off.
pub const TRIGGERS_OFF_NOTE: &str = "⚠️ Triggers are switched off, so this trigger does not fire yet. Turn them on with /tag trigger enable.";

/// `offNote(settings)`: the warning to append when a saved trigger would not fire yet.
pub fn off_note(settings: &GuildSettings) -> Option<&'static str> {
    if !is_module_enabled(Some(settings), Some(ModuleKey::Customcommands)) {
        return Some(MODULE_OFF_NOTE);
    }
    if !settings.auto_triggers_enabled {
        return Some(TRIGGERS_OFF_NOTE);
    }
    None
}

/// `neutraliseFences(s)`: fences inside a code block would end it.
pub fn neutralise_fences(s: &str) -> String {
    s.replace("```", "`\u{200b}`\u{200b}`")
}

fn yes_no(b: bool) -> &'static str {
    if b { "Yes" } else { "No" }
}

/// `matchLabel(row)`.
pub fn row_match_label(row: &AutoTrigger) -> String {
    let t = row.match_type.as_str();
    format!("{}{}", match_label(t).unwrap_or(t), if row.case_sensitive { " (case-sensitive)" } else { "" })
}

/// `cooldownText(n)`.
pub fn cooldown_text(n: i64) -> String {
    if n != 0 { format!("{n}s per user") } else { "None".into() }
}

/// `triggerChannelProblem(channel)`: a problem with posting in a trigger channel (forums need thread permissions).
/// The JS had the option's resolved channel: an uncached one is fetched over REST first.
async fn trigger_channel_problem(ctx: &CommandCtx, guild_id: GuildId, channel: ChannelId) -> Option<String> {
    let resolved = resolve_channel(&ctx.app, guild_id, channel).await;
    let forum = resolved
        .as_ref()
        .map(|c| u8::from(c.kind) == channel_type::GUILD_FORUM)
        .or_else(|| ctx.get_channel("channel").map(|c| u8::from(c.kind) == channel_type::GUILD_FORUM))
        .unwrap_or(false);
    if forum {
        let missing = match &resolved {
            Some(c) => missing_from(bot_permissions_for(&ctx.app, guild_id, c), &["ViewChannel", "SendMessagesInThreads"]),
            None => missing_bot_permissions(&ctx.app, PermTarget::Channel(guild_id, channel), &["ViewChannel", "SendMessagesInThreads"]),
        };
        if missing.is_empty() {
            return None;
        }
        return Some(format!("I need **{}** in <#{channel}>.", bot_core::permissions::permission_names(&missing).join(", ")));
    }
    let usable = match &resolved {
        Some(c) => check_channel_usable_for(&ctx.app, guild_id, c, &["ViewChannel", "SendMessages"]),
        None => check_channel_usable(&ctx.app, guild_id, Some(channel), &["ViewChannel", "SendMessages"]),
    };
    if usable.ok { None } else { Some(usable.reason.unwrap_or_else(|| format!("I cannot post in <#{channel}>."))) }
}

fn channels_text(row: &AutoTrigger, guild: Option<&Guild>) -> String {
    format_id_list(&row.channel_ids, IdKind::Channel, guild, "All channels", 1024)
}

fn exempt_text(row: &AutoTrigger, guild: Option<&Guild>) -> String {
    format_id_list(&row.exempt_role_ids, IdKind::Role, guild, "None", 1024)
}

/// `isUnsafe(row)`: a REGEX row whose pattern no longer validates.
pub fn is_unsafe(row: &AutoTrigger) -> bool {
    row.match_type.as_str() == "REGEX" && validate_trigger_pattern(&row.trigger, "REGEX", row.case_sensitive).is_err()
}

async fn trigger_reply(ctx: &CommandCtx, embed: CreateEmbed) -> BotResult {
    ctx.edit_reply(Payload::embed(embed).no_mentions().to_edit()).await?;
    Ok(())
}

// ── trigger group: execute ────────────────────────────────────────────────────

/// Config commands: public replies, and they work while the module is off.
async fn execute_trigger(ctx: CommandCtx, sub: &str) -> BotResult {
    ctx.defer_reply(false).await?; // public, like every admin config command
    let g = gid(&ctx)?.to_string();
    let settings = ctx.app.settings().get_settings(&g).await?;
    let outcome = match sub {
        "enable" => trigger_enable(&ctx, &settings).await,
        "disable" => trigger_disable(&ctx).await,
        "add" => trigger_add(&ctx, &settings).await,
        "edit" => trigger_edit(&ctx, &settings).await,
        "remove" => trigger_remove(&ctx).await,
        "toggle" => trigger_toggle(&ctx, &settings).await,
        "info" => trigger_info(&ctx, &settings).await,
        "list" => trigger_list(&ctx, &settings).await,
        _ => Ok(()),
    };
    match outcome {
        Err(e) if as_tag_error(&e).is_some() => {
            let err = as_tag_error(&e).cloned().unwrap_or_else(|| TagError::new("ERROR", "Error", ""));
            trigger_reply(&ctx, error_embed(&err.title, Some(&err.message), None)).await
        }
        other => other,
    }
}

async fn trigger_enable(ctx: &CommandCtx, settings: &Settings) -> BotResult {
    let g = gid(ctx)?.to_string();
    ctx.app.settings().update_settings(&g, SettingsPatch::new().set("customCommandsEnabled", true).set("autoTriggersEnabled", true)).await?;
    // Prefix commands answered already when the module was on, so the text is about triggers; it names the module
    // only when this turned it on.
    let text = if settings.custom_commands_enabled {
        "Triggers now respond to matching messages."
    } else {
        "Triggers now respond to matching messages. The **Custom Commands** module was off and is now on."
    };
    trigger_reply(ctx, success_embed("Triggers On", Some(text), None)).await
}

async fn trigger_disable(ctx: &CommandCtx) -> BotResult {
    let g = gid(ctx)?.to_string();
    // Only the trigger switch: /tag and prefix commands keep working (turn the module off with /module).
    ctx.app.settings().update_settings(&g, SettingsPatch::new().set("autoTriggersEnabled", false)).await?;
    trigger_reply(
        ctx,
        success_embed(
            "Triggers Off",
            Some("Triggers stop responding. /tag and prefix commands keep working; turn the whole module off with /module disable customcommands."),
            None,
        ),
    )
    .await
}

async fn trigger_add(ctx: &CommandCtx, settings: &Settings) -> BotResult {
    let guild_id = gid(ctx)?;
    let g = guild_id.to_string();
    let pattern = required_string(ctx, "pattern")?;
    let response = required_string(ctx, "response")?;
    let match_type = ctx.get_string("match_type").unwrap_or_else(|| "CONTAINS".into());
    let cooldown = ctx.get_integer("cooldown").unwrap_or(0);
    let delete_message = ctx.get_boolean("delete_message").unwrap_or(false);
    let case_sensitive = ctx.get_boolean("case_sensitive").unwrap_or(false);
    let channel = ctx.get_channel_id("channel");

    if let Err(reason) = validate_trigger_pattern(&pattern, &match_type, case_sensitive) {
        return trigger_reply(ctx, error_embed("Invalid Pattern", Some(&reason), None)).await;
    }
    if bot_util::js::trim(&response).is_empty() {
        return trigger_reply(ctx, error_embed("Empty", Some("The reply text cannot be empty."), None)).await;
    }
    if let Some(c) = channel
        && let Some(problem) = trigger_channel_problem(ctx, guild_id, c).await
    {
        return trigger_reply(ctx, error_embed("Missing Permissions", Some(&problem), None)).await;
    }

    ctx.app.settings().ensure_guild_settings(&g).await?;
    let row = svc::create_trigger_row(
        &ctx.app,
        NewTrigger {
            guild_id: g.clone(),
            trigger: pattern.clone(),
            response: response.clone(),
            match_type,
            cooldown,
            delete_message,
            case_sensitive,
            channel_ids: channel.map(|c| vec![c.to_string()]).unwrap_or_default(),
            exempt_role_ids: Vec::new(),
            created_by: ctx.user().id.to_string(),
        },
    )
    .await?;
    svc::invalidate_trigger_cache(&ctx.app, &g).await;
    let n = svc::trigger_number(&ctx.app, &g, &row).await?;

    let mut lines = vec![
        format!("**#{n}** · ID `{}`", row.id),
        format!("**Pattern:** `{}`", code_safe(&clamp_text(&pattern, 200))),
        format!("**Match:** {}", row_match_label(&row)),
        format!("**Cooldown:** {}", cooldown_text(cooldown)),
        format!("**Delete message:** {}", yes_no(delete_message)),
        format!("**Channels:** {}", channel.map(|c| format!("<#{c}>")).unwrap_or_else(|| "All channels".into())),
    ];
    let unknown = render_placeholders_detailed(&response, &interaction_render_ctx(ctx, None), None).unknown_tokens;
    if let Some(u) = unknown_line(&unknown) {
        lines.push(u);
    }
    if delete_message && !missing_bot_permissions(&ctx.app, PermTarget::Guild(guild_id), &["ManageMessages"]).is_empty() {
        lines.push("⚠️ I need **Manage Messages** to delete matching messages.".into());
    }
    if let Some(note) = off_note(settings) {
        lines.push(note.into());
    }
    trigger_reply(ctx, success_embed("Trigger Added", Some(&lines.join("\n")), None)).await
}

/// The options of `/tag trigger edit`.
#[derive(Debug, Clone, Default)]
pub struct TriggerEdit {
    pub pattern: Option<String>,
    pub response: Option<String>,
    pub match_type: Option<String>,
    pub cooldown: Option<i64>,
    pub delete_message: Option<bool>,
    pub case_sensitive: Option<bool>,
    pub channel: Option<String>,
    pub exempt_role: Option<String>,
    pub reset_scope: bool,
}

/// What `/tag trigger edit` writes and says, or the refusal `(title, text)`. `channel_problem` checks a channel that
/// is being added (removing never needs the check).
pub fn trigger_edit_plan(
    row: &AutoTrigger,
    e: &TriggerEdit,
    guild_id: &str,
    channel_problem: impl Fn(&str) -> Option<String>,
) -> Result<(Patch, Vec<String>), (&'static str, String)> {
    let nothing = e.pattern.is_none()
        && e.response.is_none()
        && e.match_type.is_none()
        && e.cooldown.is_none()
        && e.delete_message.is_none()
        && e.case_sensitive.is_none()
        && e.channel.is_none()
        && e.exempt_role.is_none()
        && !e.reset_scope;
    if nothing {
        return Err(("Nothing to Change", "Give at least one option to change.".into()));
    }
    if e.pattern.is_some() || e.match_type.is_some() || e.case_sensitive.is_some() {
        let pattern = e.pattern.clone().unwrap_or_else(|| row.trigger.clone());
        let mt = e.match_type.clone().unwrap_or_else(|| row.match_type.as_str().to_string());
        if let Err(reason) = validate_trigger_pattern(&pattern, &mt, e.case_sensitive.unwrap_or(row.case_sensitive)) {
            return Err(("Invalid Pattern", reason));
        }
    }
    if e.response.as_deref().is_some_and(|r| bot_util::js::trim(r).is_empty()) {
        return Err(("Empty", "The reply text cannot be empty.".into()));
    }
    // Scope: channels and exempt roles are toggled (added when absent, removed when present).
    let mut channel_ids = if e.reset_scope { Vec::new() } else { row.channel_ids.clone() };
    let mut exempt_role_ids = if e.reset_scope { Vec::new() } else { row.exempt_role_ids.clone() };
    if let Some(c) = &e.channel {
        if channel_ids.contains(c) {
            channel_ids.retain(|x| x != c);
        } else {
            if let Some(problem) = channel_problem(c) {
                return Err(("Missing Permissions", problem));
            }
            channel_ids.push(c.clone());
        }
    }
    if let Some(r) = &e.exempt_role {
        if r == guild_id {
            return Err(("Not Allowed", "@everyone cannot be exempt; pause the trigger with /tag trigger toggle instead.".into()));
        }
        if exempt_role_ids.contains(r) {
            exempt_role_ids.retain(|x| x != r);
        } else {
            exempt_role_ids.push(r.clone());
        }
    }
    if channel_ids.len() > svc::TRIGGER_SCOPE_MAX || exempt_role_ids.len() > svc::TRIGGER_SCOPE_MAX {
        return Err((
            "Limit Reached",
            format!("A trigger can list at most {} channels and {} exempt roles.", svc::TRIGGER_SCOPE_MAX, svc::TRIGGER_SCOPE_MAX),
        ));
    }
    let mut data: Patch = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    if let Some(p) = &e.pattern {
        data.push(("trigger", json!(p)));
        lines.push(format!("**Pattern:** `{}`", code_safe(&clamp_text(p, 200))));
    }
    if let Some(m) = &e.match_type {
        data.push(("matchType", json!(m)));
        lines.push(format!("**Match:** {}", match_label(m).unwrap_or("undefined")));
    }
    if let Some(c) = e.case_sensitive {
        data.push(("caseSensitive", json!(c)));
        lines.push(format!("**Case-sensitive:** {}", yes_no(c)));
    }
    if let Some(r) = &e.response {
        data.push(("response", json!(r)));
        lines.push("**Response** updated.".into());
    }
    if let Some(c) = e.cooldown {
        data.push(("cooldown", json!(c)));
        lines.push(format!("**Cooldown:** {}", cooldown_text(c)));
    }
    if let Some(d) = e.delete_message {
        data.push(("deleteMessage", json!(d)));
        lines.push(format!("**Delete message:** {}", yes_no(d)));
    }
    if e.reset_scope || e.channel.is_some() {
        data.push(("channelIds", json!(channel_ids)));
    }
    if e.reset_scope || e.exempt_role.is_some() {
        data.push(("exemptRoleIds", json!(exempt_role_ids)));
    }
    if data.is_empty() {
        return Err(("Nothing to Change", "Give at least one option to change.".into()));
    }
    Ok((data, lines))
}

async fn trigger_edit(ctx: &CommandCtx, settings: &Settings) -> BotResult {
    let guild_id = gid(ctx)?;
    let g = guild_id.to_string();
    let row = svc::resolve_trigger(&ctx.app, &g, &required_string(ctx, "trigger")?).await?;
    let e = TriggerEdit {
        pattern: ctx.get_string("pattern"),
        response: ctx.get_string("response"),
        match_type: ctx.get_string("match_type"),
        cooldown: ctx.get_integer("cooldown"),
        delete_message: ctx.get_boolean("delete_message"),
        case_sensitive: ctx.get_boolean("case_sensitive"),
        channel: ctx.get_channel_id("channel").map(|c| c.to_string()),
        exempt_role: ctx.get_role_id("exempt_role").map(|r| r.to_string()),
        reset_scope: ctx.get_boolean("reset_scope") == Some(true),
    };
    // The plan consults this only when the channel is being added (removing never needs the check).
    let channel_problem = match e.channel.as_deref().and_then(|c| c.parse::<u64>().ok()).filter(|v| *v != 0) {
        Some(v) => trigger_channel_problem(ctx, guild_id, ChannelId::new(v)).await,
        None => None,
    };
    let plan = trigger_edit_plan(&row, &e, &g, |_| channel_problem.clone());
    let (data, mut lines) = match plan {
        Ok(p) => p,
        Err((title, text)) => return trigger_reply(ctx, error_embed(title, Some(&text), None)).await,
    };
    let updated = svc::write_trigger(&ctx.app, &g, &row, data).await?;
    svc::invalidate_trigger_cache(&ctx.app, &g).await;
    let guild = cached_guild(&ctx.app, guild_id);
    lines.push(format!("**Channels:** {}", channels_text(&updated, guild.as_ref())));
    lines.push(format!("**Exempt roles:** {}", exempt_text(&updated, guild.as_ref())));
    if let Some(note) = off_note(settings) {
        lines.push(note.into());
    }
    let n = svc::trigger_number(&ctx.app, &g, &row).await?;
    trigger_reply(ctx, success_embed("Trigger Updated", Some(&format!("**#{n}** · ID `{}`\n{}", row.id, lines.join("\n"))), None)).await
}

async fn trigger_remove(ctx: &CommandCtx) -> BotResult {
    let g = gid(ctx)?.to_string();
    let row = svc::resolve_trigger(&ctx.app, &g, &required_string(ctx, "trigger")?).await?;
    let n = svc::trigger_number(&ctx.app, &g, &row).await?;
    if !svc::delete_trigger_row(&ctx.app, &row.id).await? {
        return Err(TagError::new("NOT_FOUND", "Not Found", "That trigger was already removed.").into());
    }
    svc::invalidate_trigger_cache(&ctx.app, &g).await;
    trigger_reply(
        ctx,
        success_embed("Trigger Removed", Some(&format!("#{n} `{}` was deleted ({} fires).", code_safe(&clamp_text(&row.trigger, 100)), row.uses)), None),
    )
    .await
}

async fn trigger_toggle(ctx: &CommandCtx, settings: &Settings) -> BotResult {
    let g = gid(ctx)?.to_string();
    let row = svc::resolve_trigger(&ctx.app, &g, &required_string(ctx, "trigger")?).await?;
    let n = svc::trigger_number(&ctx.app, &g, &row).await?;
    let enabled = !row.enabled;
    svc::write_trigger(&ctx.app, &g, &row, vec![("enabled", json!(enabled))]).await?;
    svc::invalidate_trigger_cache(&ctx.app, &g).await;
    let note = if enabled { off_note(settings) } else { None };
    let text = format!("#{n} `{}` is now **{}**.", code_safe(&clamp_text(&row.trigger, 100)), if enabled { "active" } else { "paused" });
    let body = match note {
        Some(n) => format!("{text}\n{n}"),
        None => text,
    };
    trigger_reply(ctx, success_embed(if enabled { "Trigger Resumed" } else { "Trigger Paused" }, Some(&body), None)).await
}

/// The `/tag trigger info` embed; `warning` is the `⚠️ Never fires` text, `creator` the creator's tag (or id).
pub fn trigger_info_embed(row: &AutoTrigger, n: i64, creator: &str, warning: Option<String>, guild: Option<&Guild>, settings: Option<&GuildSettings>) -> CreateEmbed {
    let response = clamp_text(&row.response, 1024);
    let mut embed = apply_guild_color(info_embed(&format!("Trigger #{n}"), None, None), settings)
        .field("Match type", row_match_label(row), true)
        .field("Status", if row.enabled { "✅ Active" } else { "⏸️ Paused" }, true)
        .field("Uses", fmt_int(i64::from(row.uses)), true)
        .field("Cooldown", cooldown_text(i64::from(row.cooldown)), true)
        .field("Delete message", yes_no(row.delete_message), true)
        .field("Created by", format!("<@{}>", row.created_by), true)
        .field("Channels", channels_text(row, guild), false)
        .field("Exempt roles", exempt_text(row, guild), false)
        .field("Pattern", format!("```\n{}\n```", neutralise_fences(&clamp_text(&row.trigger, 900))), false)
        .field("Response", if response.is_empty() { "—".to_string() } else { response }, false);
    if let Some(w) = warning {
        embed = embed.field("⚠️ Never fires", w, false);
    }
    embed.footer(CreateEmbedFooter::new(clamp_text(&format!("ID: {} · Created by {creator}", row.id), 2048))).timestamp(serenity::all::Timestamp::from(row.created_at))
}

async fn trigger_info(ctx: &CommandCtx, settings: &Settings) -> BotResult {
    let guild_id = gid(ctx)?;
    let g = guild_id.to_string();
    let row = svc::resolve_trigger(&ctx.app, &g, &required_string(ctx, "trigger")?).await?;
    let n = svc::trigger_number(&ctx.app, &g, &row).await?;
    let creator = creator_tag(&ctx.app, &row.created_by).await;
    let mut warning = None;
    if row.match_type.as_str() == "REGEX" {
        match validate_trigger_pattern(&row.trigger, "REGEX", row.case_sensitive) {
            Err(reason) => {
                warning = Some(clamp_text(&format!("This regex is unsafe or invalid ({reason}) and is skipped. Fix it with /tag trigger edit."), 1024));
            }
            Ok(()) if svc::trigger_quarantined(&ctx.app, &row) => {
                warning = Some("This regex timed out recently and is paused for up to 10 minutes.".into());
            }
            Ok(()) => {}
        }
    }
    let guild = cached_guild(&ctx.app, guild_id);
    trigger_reply(ctx, trigger_info_embed(&row, n, &creator, warning, guild.as_ref(), Some(settings))).await
}

/// `client.users.fetch(id).then((u) => u.tag).catch(() => id)`.
async fn creator_tag(app: &App, id: &str) -> String {
    let Some(uid) = id.parse::<u64>().ok().filter(|v| *v != 0).map(serenity::all::UserId::new) else { return id.to_string() };
    if let Some(d) = app.discord()
        && let Some(u) = d.cache.user(uid)
    {
        return bot_core::adapters::user_tag(&u);
    }
    match app.http() {
        Some(http) => http.get_user(uid).await.map(|u| bot_core::adapters::user_tag(&u)).unwrap_or_else(|_| id.to_string()),
        None => id.to_string(),
    }
}

/// One entry of `/tag trigger list`.
pub fn trigger_list_entry(i: usize, row: &AutoTrigger) -> String {
    let t = row.match_type.as_str();
    format!(
        "`#{}` {} **[{}]** `{}`{}\n↳ {} · {} fires · ID `{}`",
        i + 1,
        if row.enabled { "✅" } else { "⏸️" },
        match_label(t).unwrap_or(t),
        code_safe(&clamp_text(&one_line(&row.trigger), 40)),
        if is_unsafe(row) { " ⚠️ never fires" } else { "" },
        clamp_text(&one_line(&row.response), 60),
        row.uses,
        row.id
    )
}

async fn trigger_list(ctx: &CommandCtx, settings: &Settings) -> BotResult {
    let g = gid(ctx)?.to_string();
    let rows = svc::list_triggers(&ctx.app, &g).await?;
    if rows.is_empty() {
        return trigger_reply(ctx, info_embed("No Triggers", Some("No triggers yet. Add one with `/tag trigger add`."), None)).await;
    }
    let entries: Vec<String> = rows.iter().enumerate().map(|(i, r)| trigger_list_entry(i, r)).collect();
    let pages: Vec<CreateEmbed> = entries
        .chunks(TRIGGER_PAGE_SIZE)
        .map(|chunk| {
            apply_guild_color(
                info_embed(&format!("⚡ Triggers ({})", rows.len()), Some(&chunk.join("\n\n")), Some("Use #number or ID in /tag trigger edit|remove|toggle|info")),
                Some(settings),
            )
        })
        .collect();
    if pages.len() == 1 {
        return trigger_reply(ctx, pages.into_iter().next().unwrap_or_default()).await;
    }
    paginate(ctx, &pages, PaginateOptions::default()).await?;
    Ok(())
}

// ── Autocomplete ──────────────────────────────────────────────────────────────

/// `contains` with `mode: 'insensitive'` as Prisma runs it on PostgreSQL: `ILIKE '%q%'`, with the text passed
/// through unescaped (so `%` and `_` typed by a user are wildcards, exactly as in the JS bot).
pub fn ilike_contains(q: &str) -> String {
    format!("%{q}%")
}

/// `startsWith` as Prisma runs it: `LIKE 'q%'`, unescaped.
pub fn like_starts_with(q: &str) -> String {
    format!("{q}%")
}

/// The trigger choices of `/tag trigger edit | remove | toggle | info`: filtered in the database BEFORE the 25 cap,
/// so old triggers are found too.
pub async fn trigger_choices(app: &App, guild_id: &str, typed: &str) -> anyhow::Result<Vec<Choice>> {
    let q = bot_util::js::trim(typed);
    let rows: Vec<(String, String, bot_db::TriggerMatchType, bool)> = if q.is_empty() {
        sqlx::query_as(r#"SELECT "id", "trigger", "matchType", "enabled" FROM "AutoTrigger" WHERE "guildId" = $1 ORDER BY "createdAt" ASC, "id" ASC LIMIT 25"#)
            .bind(guild_id)
            .fetch_all(app.db())
            .await?
    } else {
        sqlx::query_as(
            r#"SELECT "id", "trigger", "matchType", "enabled" FROM "AutoTrigger" WHERE "guildId" = $1 AND ("trigger" ILIKE $2 OR "response" ILIKE $2 OR "id" LIKE $3) ORDER BY "createdAt" ASC, "id" ASC LIMIT 25"#,
        )
        .bind(guild_id)
        .bind(ilike_contains(q))
        .bind(like_starts_with(q))
        .fetch_all(app.db())
        .await?
    };
    Ok(rows
        .into_iter()
        .map(|(id, trigger, mt, enabled)| {
            let t = mt.as_str();
            Choice::new(format!("{} [{}] {}", if enabled { "✅" } else { "⏸️" }, match_label(t).unwrap_or(t), clamp_text(&one_line(&trigger), 70)), id)
        })
        .collect())
}

/// The name rows of the name autocomplete (`name`, `description`, `enabled` and the role lists).
pub async fn name_rows(app: &App, guild_id: &str, q: &str, only_enabled: bool, take: i64) -> anyhow::Result<Vec<CustomTag>> {
    let mut sql = String::from(r#"SELECT * FROM "CustomTag" WHERE "guildId" = $1"#);
    if !q.is_empty() {
        sql.push_str(r#" AND "name" ILIKE $2"#);
    }
    if only_enabled {
        sql.push_str(r#" AND "enabled" = true"#);
    }
    sql.push_str(&format!(r#" ORDER BY "uses" DESC, "name" ASC LIMIT {take}"#));
    let mut query = sqlx::query_as::<_, CustomTag>(sqlx::AssertSqlSafe(sql)).bind(guild_id);
    if !q.is_empty() {
        query = query.bind(ilike_contains(q));
    }
    Ok(query.fetch_all(app.db()).await?)
}

/// A non-focused option of an autocomplete interaction.
fn ac_option<'a>(ctx: &'a AutocompleteCtx, name: &str) -> Option<&'a serenity::all::CommandDataOptionValue> {
    bot_core::interaction::leaf_options(&ctx.interaction.data.options).iter().find(|o| o.name == name).map(|o| &o.value)
}

fn ac_integer(ctx: &AutocompleteCtx, name: &str) -> Option<i64> {
    match ac_option(ctx, name)? {
        serenity::all::CommandDataOptionValue::Integer(n) => Some(*n),
        serenity::all::CommandDataOptionValue::Autocomplete { value, .. } => value.trim().parse().ok(),
        _ => None,
    }
}

fn ac_string(ctx: &AutocompleteCtx, name: &str) -> Option<String> {
    match ac_option(ctx, name)? {
        serenity::all::CommandDataOptionValue::String(s) => Some(s.clone()),
        serenity::all::CommandDataOptionValue::Autocomplete { value, .. } => Some(value.clone()),
        _ => None,
    }
}

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    if ctx.subcommand_group().as_deref() == Some("trigger") {
        let g = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
        match trigger_choices(&ctx.app, &g, &ctx.focused_value()).await {
            Ok(choices) => respond_autocomplete(&ctx.responder, &choices).await,
            Err(e) => {
                bot_util::log_debug!(format!("/tag trigger autocomplete failed: {e}"));
                respond_autocomplete(&ctx.responder, &[]).await;
            }
        }
        return Ok(());
    }
    match name_autocomplete(&ctx).await {
        Ok(choices) => respond_autocomplete(&ctx.responder, &choices).await,
        Err(e) => {
            bot_util::log_debug!(format!("/tag autocomplete failed: {e}"));
            respond_autocomplete(&ctx.responder, &[]).await;
        }
    }
    Ok(())
}

async fn name_autocomplete(ctx: &AutocompleteCtx) -> anyhow::Result<Vec<Choice>> {
    let Some(guild_id) = ctx.guild_id() else { return Ok(Vec::new()) };
    let g = guild_id.to_string();
    // The router does not warm the memo (not gated), so the load is bounded.
    let settings = settings_in_budget(&ctx.app, Some(guild_id), bot_core::interaction::ack_budget_ms(ctx.interaction.id)).await;
    let Some(settings) = settings.filter(|s| is_module_enabled(Some(s), Some(ModuleKey::Customcommands))) else { return Ok(Vec::new()) };
    let group = ctx.subcommand_group();
    let sub = ctx.subcommand();
    let Some((focused, typed)) = ctx.focused() else { return Ok(Vec::new()) };
    let member = ctx.interaction.member.as_deref();

    if focused == "name" {
        let q = normalize_tag_name(&typed);
        // Members never see disabled or role-restricted commands.
        let member_view = sub.as_deref() == Some("use")
            || (group.is_none() && matches!(sub.as_deref(), Some("info" | "raw")))
            || (group.as_deref() == Some("action") && sub.as_deref() == Some("list"));
        let member_hidden = member_view && level_of(&ctx.app, Some(guild_id), member, &settings) < PermissionLevel::Admin;
        let only_enabled = sub.as_deref() == Some("use") || member_hidden;
        let take = if member_hidden { limits::CUSTOM_TAGS as i64 } else { 25 };
        let found = name_rows(&ctx.app, &g, &q, only_enabled, take).await?;
        let roles = member_roles(&ctx.app, Some(guild_id), member);
        let rows: Vec<CustomTag> =
            if member_hidden { found.into_iter().filter(|r| check_tag_roles(AccessLists::of(r), roles.as_deref()).is_ok()).collect() } else { found };
        return Ok(rows
            .into_iter()
            .take(25)
            .map(|r| {
                let desc = r.description.as_deref().filter(|d| !d.is_empty()).map(|d| format!(" — {d}")).unwrap_or_default();
                Choice::new(format!("{}{}{desc}", if r.enabled { "" } else { "⏸️ " }, r.name), r.name)
            })
            .collect());
    }

    if focused == "action" || focused == "reply" {
        let name = normalize_tag_name(&ac_string(ctx, "name").unwrap_or_default());
        let Some(tag) = svc::find_tag(&ctx.app, &g, &name).await? else { return Ok(Vec::new()) };
        let actions = get_effective_actions(&tag);
        let guild = cached_guild(&ctx.app, guild_id);
        let names: &dyn svc::NameLookup = match &guild {
            Some(g) => g,
            None => &svc::NoNames,
        };
        if focused == "action" {
            let messages_only = group.as_deref() == Some("response") && sub.as_deref() == Some("add");
            return Ok(actions
                .iter()
                .enumerate()
                .filter(|(_, a)| !messages_only || a.is_message())
                .map(|(i, a)| Choice::new(format!("{}. {}", i + 1, action_label(a, names)), json!(i + 1)))
                .collect());
        }
        let n = ac_integer(ctx, "action").filter(|n| *n != 0);
        let a = n.and_then(|n| usize::try_from(n - 1).ok()).and_then(|i| actions.get(i));
        return Ok(a
            .map(|a| a.responses.iter().enumerate().map(|(k, r)| Choice::new(format!("{}. {}", k + 1, clamp_text(&one_line(r), 90)), json!(k + 1))).collect())
            .unwrap_or_default());
    }
    Ok(Vec::new())
}
