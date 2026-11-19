//! Port of `src/commands/admin/stats.js`: `/stats create | edit | delete | list | refresh | category`, the
//! configuration of the Statistics Channels (MEE6 configures this on its dashboard only). Validation, replies and calls
//! into [`crate::stats_service`]. Replies are public (the admin convention); configuration works while the module is
//! off (`gated = false`).

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::embed_builder::apply_guild_color;
use bot_core::interaction_helpers::{Choice, respond_autocomplete};
use bot_core::new_targets::{NewOptions, new_channel_option, resolve_new_options};
use bot_core::permissions::permission_names;
use bot_core::prelude::*;
use bot_db::{StatChannel, StatKind};
use bot_util::text::{chunk_lines, clamp_text, format_number};
use serenity::all::Permissions;
use serenity::builder::{CreateEmbed, EditInteractionResponse};
use serde_json::Value;

use crate::stats_io::{StatsChannel, StatsGuild, stats_io};
use crate::stats_service::{self as stats, ChannelCheck, RefreshNow, RefreshResult, StatsError, kind_info};

/// The command's data (`new SlashCommandBuilder()...`).
pub fn data() -> SlashCommand {
    let mut kind = Opt::string("kind", "What to count").required(true);
    for k in stats::STAT_KINDS {
        kind = kind.choice(kind_info(*k).0, k.as_str());
    }
    SlashCommand::new("stats", "Counter channels with live server stats (MEE6 Statistics Channels).")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(
            Opt::sub_command("create", "Create a counter channel.")
                .option(kind)
                .option(Opt::string("template", "Channel name with {count}, e.g. \"Members: {count}\"").min_length(1).max_length(100))
                .option(Opt::role("role", "Role to count (Role members only)"))
                .option(Opt::channel("category", "Category for this channel").channel_types(&[channel_type::GUILD_CATEGORY]))
                .option(new_channel_option("category", "category")),
        )
        .option(
            Opt::sub_command("edit", "Change a counter's name template.")
                .option(Opt::string("channel", "Counter channel").required(true).autocomplete(true).max_length(100))
                .option(Opt::string("template", "New name with {count}").required(true).min_length(1).max_length(100)),
        )
        .option(
            Opt::sub_command("delete", "Remove a counter and its channel.")
                .option(Opt::string("channel", "Counter channel").required(true).autocomplete(true).max_length(100))
                .option(Opt::boolean("keep_channel", "Keep the channel, just stop updating it")),
        )
        .option(Opt::sub_command("list", "List counters."))
        .option(Opt::sub_command("refresh", "Update all counters now."))
        .option(
            Opt::sub_command("category", "Set the default category for new counters.")
                .option(Opt::channel("category", "Category (omit to clear)").channel_types(&[channel_type::GUILD_CATEGORY]))
                .option(new_channel_option("category", "category")),
        )
}

/// The command (`module 'stats'`, `gated = false`, ADMIN; every subcommand is ADMIN and guild-only).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Stats), execute).gated(false).permission_level(PermissionLevel::Admin).autocomplete(autocomplete)
}

/// The note appended while the module is off.
pub const MODULE_OFF_NOTE: &str =
    "\n\n\u{26a0}\u{fe0f} The Statistics Channels module is off, so counters don't update. Turn it on with `/module enable stats`.";

fn module_note(settings: Option<&Settings>) -> &'static str {
    match settings {
        Some(s) if !is_module_enabled(Some(s), Some(ModuleKey::Stats)) => MODULE_OFF_NOTE,
        _ => "",
    }
}

fn generic_error() -> CreateEmbed {
    error_embed("Something Went Wrong", Some("I could not complete that. Please try again in a moment."), None)
}

fn not_found() -> CreateEmbed {
    error_embed("Not Found", Some("That isn't a counter channel. Pick one from the list, or see `/stats list`."), None)
}

/// `shownTemplate(template, max)`: backticks become quotes, clamped.
pub fn shown_template(template: &str, max: usize) -> String {
    clamp_text(&template.replace('`', "'"), max)
}

/// `labelOf(kind, roleId)`.
pub fn label_of(kind: StatKind, role_id: Option<&str>) -> String {
    match role_id.filter(|r| !r.is_empty()) {
        Some(r) => format!("{} (<@&{r}>)", kind_info(kind).0),
        None => kind_info(kind).0.to_string(),
    }
}

fn category_permission_error(app: &App, guild_id: &str, category: &StatsChannel) -> Option<CreateEmbed> {
    let missing = stats_io(app).missing_bot_permissions(app, guild_id, Some(&category.id), &["ViewChannel", "ManageChannels"]);
    if missing.is_empty() {
        return None;
    }
    Some(error_embed(
        "Missing Permissions",
        Some(&format!("I need **{}** in the **{}** category.", permission_names(&missing).join(", "), clamp_text(&category.name, 100))),
        None,
    ))
}

fn guild_of(app: &App, guild_id: &str) -> StatsGuild {
    stats_io(app).guild(app, guild_id).unwrap_or_else(|| StatsGuild { id: guild_id.to_string(), ..Default::default() })
}

/// A channel option through the guild cache first, else the option itself.
fn option_channel(ctx: &CommandCtx, guild: &StatsGuild, name: &str) -> Option<StatsChannel> {
    let id = ctx.get_channel_id(name)?.to_string();
    if let Some(c) = guild.channel(&id) {
        return Some(c.clone());
    }
    let partial = ctx.get_channel(name);
    Some(StatsChannel {
        id,
        name: partial.as_ref().and_then(|p| p.name.clone()).unwrap_or_default(),
        kind: partial.as_ref().map(|p| u8::from(p.kind)).unwrap_or(4),
        is_thread: false,
        bot_permissions: None,
    })
}

// ── create ────────────────────────────────────────────────────────────────────

async fn run_create(ctx: &CommandCtx, guild_id: &str, settings: Option<&Settings>) -> anyhow::Result<CreateEmbed> {
    let app = &ctx.app;
    let guild = guild_of(app, guild_id);
    let kind_raw = ctx.get_string("kind").unwrap_or_default();
    let role = ctx.get_role_id("role").map(|id| {
        let id = id.to_string();
        let name = ctx.get_role("role").map(|r| r.name).or_else(|| guild.role_name(&id).map(str::to_string)).unwrap_or_default();
        (id, name)
    });
    let Some(kind) = StatKind::parse(&kind_raw) else { return Ok(generic_error()) };
    let label = kind_info(kind).0;

    if kind == StatKind::RoleMembers && role.is_none() {
        return Ok(error_embed("Role Required", Some("Pick the role to count with the `role` option when using **Role members**."), None));
    }
    if kind != StatKind::RoleMembers && role.is_some() {
        return Ok(error_embed("Role Not Used", Some("The `role` option only applies to the **Role members** counter."), None));
    }
    // Role counters do not use checkAssignableRole: the role is counted, never assigned.
    if role.as_ref().is_some_and(|(id, _)| id == guild_id) {
        return Ok(error_embed("Invalid Role", Some("Use the **Members** counter to count everyone."), None));
    }
    if stats::is_list_kind(kind) && guild.member_count > stats::MEMBER_LIST_MAX_MEMBERS {
        return Ok(error_embed(
            "Server Too Large",
            Some(&format!(
                "**{label}** counters need the full member list, which I only load for servers with up to {} members. Use **Members** instead.",
                format_number(stats::MEMBER_LIST_MAX_MEMBERS as f64)
            )),
            None,
        ));
    }

    let template_input = ctx.get_string("template").unwrap_or_else(|| kind_info(kind).1.to_string());
    let template = match stats::validate_stat_template(&template_input, kind, role.as_ref().map(|(_, n)| n.as_str()), guild.placeholder.as_ref()) {
        Ok(t) => t,
        Err(msg) => return Ok(error_embed("Invalid Template", Some(&msg), None)),
    };

    // Category: the option wins; else the saved default. A saved category missing from the cache is cleared only after
    // Discord confirms it is gone; an unconfirmed lookup asks the admin to retry instead of guessing.
    let mut stale_category = false;
    let mut category = option_channel(ctx, &guild, "category");
    if category.is_none()
        && let Some(saved) = settings.and_then(|s| s.stats_category_id.clone()).filter(|s| !s.is_empty())
    {
        match stats::confirm_channel(app, &guild, &saved).await {
            ChannelCheck::Unknown(_) => {
                return Ok(error_embed(
                    "Try Again",
                    Some("I couldn't check the saved stats category right now. Please try again in a moment, or pick one with the `category` option."),
                    None,
                ));
            }
            ChannelCheck::Present(c) if c.kind == channel_type::GUILD_CATEGORY => category = Some(c),
            _ => {
                stale_category = true;
                let (a, g) = (app.clone(), guild_id.to_string());
                tokio::spawn(async move {
                    if let Err(e) = a.settings().update_settings(&g, SettingsPatch::new().set("statsCategoryId", Value::Null)).await {
                        bot_util::log_error!("stats: could not clear the stats category", e.to_string());
                    }
                });
            }
        }
    }

    // Connect is needed because Discord lets a bot deny only permissions it has.
    let missing = stats_io(app).missing_bot_permissions(app, guild_id, None, &["ViewChannel", "ManageChannels", "Connect"]);
    if !missing.is_empty() {
        return Ok(error_embed(
            "Missing Permissions",
            Some(&format!("I need **{}** in this server to create counter channels.", permission_names(&missing).join(", "))),
            None,
        ));
    }
    if let Some(c) = &category
        && let Some(problem) = category_permission_error(app, guild_id, c)
    {
        return Ok(problem);
    }

    let actor = bot_core::adapters::user_tag(ctx.user());
    let role_id = role.as_ref().map(|(id, _)| id.clone());
    let made = match stats::create_stat_channel(app, guild_id, kind, &template, role, category.map(|c| c.id), &actor).await {
        Ok(m) => m,
        Err(e) => {
            let Some(se) = e.downcast_ref::<StatsError>() else { return Err(e) };
            let for_role = role_id.as_ref().map(|r| format!(" for <@&{r}>")).unwrap_or_default();
            let (title, text) = match se.code {
                "LIMIT" => (
                    "Limit Reached",
                    format!(
                        "A server can have at most {} counter channels. Remove one with `/stats delete` first.",
                        bot_config::limits::STAT_CHANNELS
                    ),
                ),
                "ROLE_LIMIT" => (
                    "Limit Reached",
                    format!(
                        "A server can have at most {} role counters. Remove one with `/stats delete` first.",
                        bot_config::limits::ROLE_STAT_CHANNELS
                    ),
                ),
                "DUPLICATE" => (
                    "Already Exists",
                    format!(
                        "<#{}> already counts **{label}**{for_role}. Change its name with `/stats edit` instead.",
                        se.channel_id.clone().unwrap_or_default()
                    ),
                ),
                "MISSING_PERMISSIONS" => (
                    "Missing Permissions",
                    "Discord refused to create the channel. Check that I have **View Channel, Manage Channels** and **Connect** here and in the category."
                        .to_string(),
                ),
                "CHANNEL_LIMIT" => ("Channel Limit Reached", "This server has reached Discord's limit of 500 channels.".to_string()),
                "DISCORD_REJECTED" => ("Channel Not Created", "Discord rejected the new channel. The category may be full (50 channels max).".to_string()),
                _ => return Err(e),
            };
            return Ok(error_embed(title, Some(&text), None));
        }
    };

    let for_role = role_id.as_ref().map(|r| format!(" for <@&{r}>")).unwrap_or_default();
    let mut desc = format!(
        "<#{}> now shows **{label}**{for_role}.\n**Template:** `{}`\nIt updates about every 10 minutes; Discord allows only 2 renames per channel every 10 minutes.",
        made.channel.id,
        shown_template(&template, 100)
    );
    if made.value.is_none() {
        desc.push_str("\n\nThe count is still loading and will appear on the next refresh.");
    }
    if stale_category {
        desc.push_str("\n\nThe saved stats category no longer exists, so the channel was created at the top level.");
    }
    desc.push_str(module_note(settings));
    Ok(success_embed("Counter Created", Some(&desc), None))
}

// ── edit ──────────────────────────────────────────────────────────────────────

async fn run_edit(ctx: &CommandCtx, guild_id: &str, settings: Option<&Settings>) -> anyhow::Result<CreateEmbed> {
    let app = &ctx.app;
    let guild = guild_of(app, guild_id);
    let Some(row) = stats::find_row_by_ref(app, guild_id, &ctx.get_string("channel").unwrap_or_default()).await else { return Ok(not_found()) };
    let role_name = if row.kind == StatKind::RoleMembers {
        Some(row.role_id.as_deref().and_then(|r| guild.role_name(r)).unwrap_or("role").to_string())
    } else {
        None
    };
    let template = match stats::validate_stat_template(&ctx.get_string("template").unwrap_or_default(), row.kind, role_name.as_deref(), guild.placeholder.as_ref())
    {
        Ok(t) => t,
        Err(msg) => return Ok(error_embed("Invalid Template", Some(&msg), None)),
    };
    if let Err(e) = stats::edit_template(app, guild_id, &row, &template).await {
        if e.downcast_ref::<StatsError>().is_some_and(|s| s.code == "NOT_FOUND") {
            return Ok(not_found());
        }
        return Err(e);
    }
    let preview = stats::render_stat_name(&template, row.last_value.map(i64::from), role_name.as_deref(), guild.placeholder.as_ref());
    Ok(success_embed(
        "Counter Updated",
        Some(&format!(
            "<#{}> will be renamed to **{preview}** on the next refresh (usually within a minute, or up to 5 minutes if it was renamed recently).{}",
            row.channel_id,
            module_note(settings)
        )),
        None,
    ))
}

// ── delete ────────────────────────────────────────────────────────────────────

async fn run_delete(ctx: &CommandCtx, guild_id: &str, settings: Option<&Settings>) -> anyhow::Result<CreateEmbed> {
    let app = &ctx.app;
    let Some(row) = stats::find_row_by_ref(app, guild_id, &ctx.get_string("channel").unwrap_or_default()).await else { return Ok(not_found()) };
    let keep = ctx.get_boolean("keep_channel").unwrap_or(false);
    let actor = bot_core::adapters::user_tag(ctx.user());
    if let Err(e) = stats::delete_stat_channel(app, guild_id, &row, keep, &actor).await {
        if e.downcast_ref::<StatsError>().is_some_and(|s| s.code == "MISSING_PERMISSIONS") {
            return Ok(error_embed(
                "Missing Permissions",
                Some(&format!(
                    "I couldn't delete <#{}>: I need **View Channel, Manage Channels** there. Fix my permissions, or run this again with `keep_channel:True` to only stop updating it.",
                    row.channel_id
                )),
                None,
            ));
        }
        return Err(e);
    }
    let label = label_of(row.kind, row.role_id.as_deref());
    let desc = if keep {
        format!("The **{label}** counter was removed. <#{}> was kept and no longer updates.", row.channel_id)
    } else {
        format!("The **{label}** counter was removed and its channel deleted.")
    };
    Ok(success_embed("Counter Removed", Some(&format!("{desc}{}", module_note(settings))), None))
}

// ── list ──────────────────────────────────────────────────────────────────────

/// The embed total is 6000; leaves room for the title and the description.
const FIELDS_BUDGET: usize = 5400;

/// `rowLine(guild, row)`: one counter of `/stats list`.
pub fn row_line(app: &App, guild: &StatsGuild, row: &StatChannel) -> String {
    let peak = match row.peak_value {
        Some(p) if row.kind == StatKind::PeakOnline => format!(" \u{b7} peak {}", format_number(f64::from(p))),
        _ => String::new(),
    };
    let renamed = match &row.last_renamed_at {
        Some(t) => format!("renamed {}", bot_util::time::discord_timestamp(t, "R")),
        None => "not renamed yet".to_string(),
    };
    let showing = row.last_value.map(|v| format_number(f64::from(v))).unwrap_or_else(|| "\u{2014}".to_string());
    let mut line = clamp_text(
        &format!(
            "**{}** \u{b7} <#{}> \u{b7} `{}` \u{b7} showing **{showing}**{peak} \u{b7} {renamed}",
            label_of(row.kind, row.role_id.as_deref()),
            row.channel_id,
            shown_template(&row.template, 40)
        ),
        240,
    );
    if let Some(problem) = stats::inspect_row(app, guild, row) {
        line.push_str(&format!("\n\u{26a0}\u{fe0f} {}", stats::describe_problem(problem)));
    }
    line
}

/// The `/stats list` header and fields: `(description, [(name, value)])`.
pub fn list_view(app: &App, guild: &StatsGuild, rows: &[StatChannel], settings: Option<&Settings>) -> (String, Vec<(String, String)>) {
    let role_count = rows.iter().filter(|r| r.kind == StatKind::RoleMembers).count();
    let target = match settings.and_then(|s| s.stats_category_id.clone()).filter(|s| !s.is_empty()) {
        Some(c) => format!("<#{c}>"),
        None => "top level".to_string(),
    };
    let header = format!(
        "{}/{} counters \u{b7} {role_count}/{} role counters\nNew counters go to: {target}\nNames refresh about every 10 minutes.{}",
        rows.len(),
        bot_config::limits::STAT_CHANNELS,
        bot_config::limits::ROLE_STAT_CHANNELS,
        module_note(settings)
    );
    if rows.is_empty() {
        return (format!("{header}\n\nNo counters yet. Create one with `/stats create`."), Vec::new());
    }
    let mut lines: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut shown = 0usize;
    for row in rows {
        let line = row_line(app, guild, row);
        let len = bot_util::js::utf16_len(&line);
        if used + len + 1 > FIELDS_BUDGET {
            break;
        }
        lines.push(line);
        used += len + 1;
        shown += 1;
    }
    if shown < rows.len() {
        lines.push(format!("\u{2026} and {} more", rows.len() - shown));
    }
    let fields = chunk_lines(&lines, 1024).into_iter().enumerate().map(|(i, v)| (if i == 0 { "Counters".to_string() } else { "\u{200b}".to_string() }, v)).collect();
    (header, fields)
}

async fn run_list(ctx: &CommandCtx, guild_id: &str, settings: Option<&Settings>) -> anyhow::Result<CreateEmbed> {
    let app = &ctx.app;
    let rows = stats::list_rows(app, guild_id).await?;
    let guild = guild_of(app, guild_id);
    let (description, fields) = list_view(app, &guild, &rows, settings);
    let mut embed = apply_guild_color(info_embed("Statistics Channels", Some(&description), None), settings.map(|s| s.as_ref()));
    for (name, value) in fields {
        embed = embed.field(name, value, false);
    }
    Ok(embed)
}

// ── refresh ───────────────────────────────────────────────────────────────────

/// The lines of the `/stats refresh` answer.
pub fn refresh_lines(r: &RefreshResult) -> Vec<String> {
    let mut lines = vec![format!("\u{2705} Renamed: {}", r.renamed)];
    if r.unchanged > 0 {
        lines.push(format!("\u{2796} Already up to date: {}", r.unchanged));
    }
    if let Some(next) = r.waiting.iter().map(|(_, t)| *t).min() {
        lines.push(format!(
            "\u{23f3} Waiting for Discord's rename limit: {} (next {})",
            r.waiting.len(),
            bot_util::time::discord_timestamp_ms(next as f64, "R")
        ));
    }
    if r.removed > 0 {
        lines.push(format!("\u{1f5d1}\u{fe0f} Removed (channel was deleted): {}", r.removed));
    }
    if r.deferred > 0 {
        lines.push(format!("\u{23f8}\u{fe0f} Not started (time budget): {}", r.deferred));
    }
    for (channel, problem) in r.skipped.iter().take(10) {
        lines.push(format!("\u{26a0}\u{fe0f} <#{channel}>: {}", stats::describe_problem(problem)));
    }
    if r.skipped.len() > 10 {
        lines.push(format!("\u{2026} and {} more", r.skipped.len() - 10));
    }
    lines
}

async fn run_refresh(ctx: &CommandCtx, guild_id: &str, settings: Option<&Settings>) -> anyhow::Result<CreateEmbed> {
    let Some(s) = settings else {
        return Ok(error_embed("Temporarily Unavailable", Some("Server settings could not be loaded. Please try again in a moment."), None));
    };
    if !is_module_enabled(Some(s), Some(ModuleKey::Stats)) {
        return Ok(warn_embed(
            "Module Disabled",
            Some("The **Statistics Channels** module is turned off, so counters are not updated. Turn it on with `/module enable stats`."),
            None,
        ));
    }
    Ok(match stats::refresh_now(&ctx.app, guild_id).await? {
        RefreshNow::Empty => apply_guild_color(info_embed("Statistics Channels", Some("No counters yet. Create one with `/stats create`."), None), Some(s)),
        RefreshNow::Busy => apply_guild_color(info_embed("Refresh Running", Some("Counters are being updated right now. Check `/stats list` in a minute."), None), Some(s)),
        RefreshNow::Done(r) => success_embed("Counters Refreshed", Some(&refresh_lines(&r).join("\n")), None),
    })
}

// ── category ──────────────────────────────────────────────────────────────────

async fn run_category(ctx: &CommandCtx, guild_id: &str, settings: Option<&Settings>) -> anyhow::Result<CreateEmbed> {
    let app = &ctx.app;
    let guild = guild_of(app, guild_id);
    if let Some(cat) = option_channel(ctx, &guild, "category") {
        if let Some(problem) = category_permission_error(app, guild_id, &cat) {
            return Ok(problem);
        }
        app.settings().update_settings(guild_id, SettingsPatch::new().set("statsCategoryId", cat.id.clone())).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        return Ok(success_embed(
            "Stats Category Set",
            Some(&format!(
                "New counters will be created in **{}**. Existing counters stay where they are.{}",
                clamp_text(&cat.name, 100),
                module_note(settings)
            )),
            None,
        ));
    }
    app.settings().update_settings(guild_id, SettingsPatch::new().set("statsCategoryId", Value::Null)).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(success_embed(
        "Stats Category Cleared",
        Some(&format!("New counters will be created at the top level, outside any category.{}", module_note(settings))),
        None,
    ))
}

// ── Entry points ──────────────────────────────────────────────────────────────

async fn execute(mut ctx: CommandCtx) -> BotResult {
    let sub = ctx.subcommand().unwrap_or_default();
    let outcome: anyhow::Result<()> = async {
        ctx.defer_reply(false).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        if !resolve_new_options(&mut ctx, NewOptions::default()).await {
            return Ok(());
        }
        let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/stats outside a guild"))?.to_string();
        let settings = ctx.app.settings().get_settings_safe(&guild_id).await;
        let s = settings.as_ref();
        let embed = match sub.as_str() {
            "create" => run_create(&ctx, &guild_id, s).await?,
            "edit" => run_edit(&ctx, &guild_id, s).await?,
            "delete" => run_delete(&ctx, &guild_id, s).await?,
            "list" => run_list(&ctx, &guild_id, s).await?,
            "refresh" => run_refresh(&ctx, &guild_id, s).await?,
            "category" => run_category(&ctx, &guild_id, s).await?,
            _ => generic_error(),
        };
        ctx.edit_reply(EditInteractionResponse::new().embed(embed)).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(())
    }
    .await;
    if let Err(e) = outcome {
        bot_util::log_error!(format!("stats {sub} failed"), bot_core::error::log_arg(&e));
        let _ = ctx.edit_reply(EditInteractionResponse::new().embed(generic_error())).await;
    }
    Ok(())
}

/// The autocomplete choices for the rows (`<channel> — <label> (@role)`), filtered by `focused`.
pub fn autocomplete_choices(guild: Option<&StatsGuild>, rows: &[stats::RowRef], focused: &str) -> Vec<Choice> {
    rows.iter()
        .map(|r| {
            let ch = guild.and_then(|g| g.channel(&r.channel_id)).map(|c| c.name.clone()).unwrap_or_else(|| "unavailable channel".into());
            let role = r.role_id.as_deref().and_then(|id| guild.and_then(|g| g.role_name(id))).map(|n| format!(" (@{n})")).unwrap_or_default();
            (format!("{ch} \u{2014} {}{role}", kind_info(r.kind).0), r.channel_id.clone())
        })
        .filter(|(name, _)| focused.is_empty() || bot_util::js::lower(name).contains(focused))
        .map(|(name, value)| Choice::new(name, Value::from(value)))
        .collect()
}

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let focused = bot_util::js::utf16_prefix(&bot_util::js::lower(&ctx.focused_value()), 100).to_string();
    let guild_id = ctx.guild_id().map(|g| g.to_string());
    let rows = match &guild_id {
        Some(g) => stats::list_rows_safe(&ctx.app, g, 2_000).await,
        None => Vec::new(),
    };
    let guild = guild_id.as_deref().and_then(|g| stats_io(&ctx.app).guild(&ctx.app, g));
    let choices = autocomplete_choices(guild.as_ref(), &rows, &focused);
    respond_autocomplete(&ctx.responder, &choices).await;
    Ok(())
}
