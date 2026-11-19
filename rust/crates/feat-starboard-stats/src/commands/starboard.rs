//! Port of `src/commands/admin/starboard.js`: `/starboard create | edit | delete | list`. An admin config command:
//! replies are public (the admin convention) except the delete prompt, and it keeps working while the Starboards
//! module is off (`gated = false`).

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::embed_builder::apply_guild_color;
use bot_core::interaction_helpers::{Choice, ConfirmOptions, Payload, await_confirm, respond_autocomplete, safe_defer, safe_reply};
use bot_core::new_targets::{NewOptions, new_channel_option, resolve_new_options};
use bot_core::permissions::permission_names;
use bot_core::prelude::*;
use bot_db::Starboard;
use bot_util::text::clamp_text;
use serenity::all::Permissions;
use serenity::builder::CreateEmbed;
use serde_json::Value;

use crate::starboard_io::{SbChannel, starboard_io};
use crate::starboard_service::{self as sb, BoardError, BoardLite, BoardPatch};

const BOARD_CHANNEL_TYPES: [u8; 2] = [channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT];
/// Text, announcement, voice, stage, forum, media, category.
const SOURCE_CHANNEL_TYPES: [u8; 7] = [0, 5, 2, 13, 15, 16, 4];

fn add_setting_options(s: Opt) -> Opt {
    s.option(Opt::string("emoji", "Emojis that count, space-separated (default \u{2b50}; \"any\" = every emoji)").max_length(200))
        .option(Opt::integer("threshold", "Reactions needed to repost (default 3)").min_value(1).max_value(500))
        .option(Opt::boolean("self_star", "Count the author's own reaction (default False)"))
        .option(Opt::boolean("remove_self_stars", "Remove the author's own reaction (default False)"))
        .option(Opt::boolean("ignore_bots", "Ignore bot messages and bot reactions (default True)"))
        .option(Opt::boolean("remove_below", "Delete the repost if it falls below the threshold (default True)"))
        .option(Opt::boolean("auto_react", "React to the repost with the first emoji (default False)"))
        .option(Opt::integer("max_age_days", "Ignore messages older than this many days (0 = no limit)").min_value(0).max_value(3650))
        .option(Opt::boolean("allow_nsfw", "Repost from age-restricted channels (default False)"))
}

/// The command's data (`new SlashCommandBuilder()...`).
pub fn data() -> SlashCommand {
    SlashCommand::new("starboard", "Starboards: repost popular messages to a channel (MEE6 Starboards).")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(add_setting_options(
            Opt::sub_command("create", "Create a starboard (up to 10).")
                .option(Opt::string("name", "Board name").required(true).min_length(1).max_length(32))
                .option(Opt::channel("channel", "Channel to repost to").channel_types(&BOARD_CHANNEL_TYPES))
                .option(new_channel_option("channel", "channel")),
        ))
        .option(
            add_setting_options(
                Opt::sub_command("edit", "Change a starboard, including its repost channel.")
                    .option(Opt::string("name", "Board to edit").required(true).autocomplete(true).max_length(32))
                    .option(Opt::channel("channel", "New repost channel").channel_types(&BOARD_CHANNEL_TYPES))
                    .option(new_channel_option("channel", "channel")),
            )
            .option(Opt::channel("ignore_channel", "Add a channel to the ignore list").channel_types(&SOURCE_CHANNEL_TYPES))
            .option(Opt::channel("unignore_channel", "Remove a channel from the ignore list").channel_types(&SOURCE_CHANNEL_TYPES))
            .option(Opt::boolean("enabled", "Turn this board on or off"))
            .option(Opt::string("rename", "New board name").min_length(1).max_length(32)),
        )
        .option(
            Opt::sub_command("delete", "Delete a starboard (existing reposts stay).")
                .option(Opt::string("name", "Board to delete").required(true).autocomplete(true).max_length(32)),
        )
        .option(Opt::sub_command("list", "List starboards and their settings."))
}

/// The command (`module 'starboard'`, `gated = false`, ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Starboard), execute)
        .gated(false)
        .permission_level(PermissionLevel::Admin)
        .autocomplete(autocomplete)
}

// ── Shared pieces ─────────────────────────────────────────────────────────────

/// The note appended while the module is off.
pub const MODULE_OFF_NOTE: &str = "\n\n\u{26a0}\u{fe0f} The Starboards module is off, so nothing is reposted. Turn it on with `/module enable starboard`.";
const SELF_STAR_CONFLICT: &str = "Self-stars are counted on this board, so they cannot also be removed.";

fn invalid_name_embed() -> CreateEmbed {
    error_embed("Invalid Name", Some("Board names may use letters a\u{2013}z, digits, - and _ (max 32)."), None)
}

fn not_found_embed(name: &str) -> CreateEmbed {
    error_embed("Not Found", Some(&format!("No starboard named **{}**. See `/starboard list`.", clamp_text(name, 40))), None)
}

fn name_taken_embed(name: &str) -> CreateEmbed {
    error_embed("Name Taken", Some(&format!("A starboard named **{name}** already exists.")), None)
}

fn nsfw_embed() -> CreateEmbed {
    error_embed(
        "Channel Not Age-Restricted",
        Some("To repost from NSFW channels, the starboard channel must be age-restricted too. Mark it as age-restricted first."),
        None,
    )
}

fn failed_embed() -> CreateEmbed {
    error_embed("Something Went Wrong", Some("I could not save that starboard. Please try again in a moment."), None)
}

fn limit_ignored_embed() -> CreateEmbed {
    error_embed("Limit Reached", Some(&format!("A starboard can ignore at most {} channels.", sb::MAX_IGNORED_CHANNELS)), None)
}

/// `readSettings(options)`: the setting options the invoker actually gave.
fn read_settings(ctx: &CommandCtx) -> BoardPatch {
    BoardPatch {
        threshold: ctx.get_integer("threshold"),
        self_star: ctx.get_boolean("self_star"),
        remove_self_stars: ctx.get_boolean("remove_self_stars"),
        ignore_bots: ctx.get_boolean("ignore_bots"),
        remove_below_threshold: ctx.get_boolean("remove_below"),
        auto_react: ctx.get_boolean("auto_react"),
        max_message_age_days: ctx.get_integer("max_age_days"),
        allow_nsfw: ctx.get_boolean("allow_nsfw"),
        enabled: ctx.get_boolean("enabled"),
        ..Default::default()
    }
}

/// `resolveChannel(interaction, option)`: a channel option through the guild cache first, else the option itself.
fn resolve_channel(ctx: &CommandCtx, guild_id: &str, option: &str) -> Option<SbChannel> {
    let id = ctx.get_channel_id(option)?.to_string();
    if let Some(c) = starboard_io(&ctx.app).guild(&ctx.app, guild_id).and_then(|g| g.channel(&id).cloned()) {
        return Some(c);
    }
    let partial = ctx.get_channel(option);
    Some(SbChannel {
        id: id.clone(),
        guild_id: Some(guild_id.to_string()),
        name: partial.as_ref().and_then(|p| p.name.clone()).unwrap_or_default(),
        kind: partial.as_ref().map(|p| u8::from(p.kind)).unwrap_or(0),
        parent_id: partial.as_ref().and_then(|p| p.parent_id).map(|p| p.to_string()),
        is_thread: partial.as_ref().is_some_and(|p| matches!(u8::from(p.kind), 10..=12)),
        ..Default::default()
    })
}

async fn load_module_state(app: &App, guild_id: &str) -> (Option<Settings>, bool) {
    let settings = app.settings().get_settings_safe(guild_id).await;
    let off = settings.as_ref().is_some_and(|s| !is_module_enabled(Some(s), Some(ModuleKey::Starboard)));
    (settings, off)
}

/// `boardChannelProblem(channel, { autoReact })`: an error embed when the bot cannot post (and react) there.
fn board_channel_problem(app: &App, guild_id: &str, channel_id: &str, auto_react: bool) -> Option<CreateEmbed> {
    let mut needs = vec!["ViewChannel", "SendMessages", "EmbedLinks"];
    if auto_react {
        needs.push("AddReactions");
        needs.push("ReadMessageHistory");
    }
    let usable = starboard_io(app).channel_usable(app, guild_id, channel_id, &needs);
    if usable.ok {
        return None;
    }
    let detail = if usable.missing.is_empty() {
        usable.reason.unwrap_or_default()
    } else {
        format!("I need **{}** in <#{channel_id}> to post starboard reposts.", permission_names(&usable.missing).join(", "))
    };
    Some(error_embed("Missing Permissions", Some(&detail), None))
}

/// `selfStarPermissionNote(guild, board)`.
fn self_star_permission_note(app: &App, guild_id: &str, board: &Starboard) -> &'static str {
    if board.remove_self_stars && !starboard_io(app).guild_missing(app, guild_id, &["ManageMessages"]).is_empty() {
        "\n\n\u{26a0}\u{fe0f} Removing self-stars needs **Manage Messages** in the watched channels, which I do not have server-wide."
    } else {
        ""
    }
}

/// `boardProblem(guild, board)`: why a board cannot post right now; `None` when it can.
fn board_problem(app: &App, guild_id: &str, board: &Starboard) -> Option<String> {
    let io = starboard_io(app);
    if io.guild(app, guild_id).and_then(|g| g.channel(&board.channel_id).cloned()).is_none() {
        return Some("Channel deleted \u{2014} set a new one with /starboard edit".into());
    }
    let usable = io.channel_usable(app, guild_id, &board.channel_id, &["ViewChannel", "SendMessages", "EmbedLinks"]);
    if usable.ok {
        return None;
    }
    let what = if usable.missing.is_empty() { usable.reason.unwrap_or_default() } else { permission_names(&usable.missing).join(", ") };
    Some(format!("I can't post there (missing {what})"))
}

fn summary(app: &App, board: &Starboard, reposts: Option<i64>) -> String {
    let io = starboard_io(app);
    sb::settings_summary(&BoardLite::from(board), &|id| io.custom_emoji(app, id), reposts)
}

async fn reply(ctx: &CommandCtx, embed: CreateEmbed) {
    safe_reply(&ctx.responder, Payload::embed(embed)).await;
}

async fn execute(ctx: CommandCtx) -> BotResult {
    match ctx.subcommand().as_deref() {
        Some("create") => run_create(ctx).await,
        Some("edit") => run_edit(ctx).await,
        Some("delete") => run_delete(ctx).await,
        Some("list") => run_list(ctx).await,
        _ => Ok(()),
    }
}

// ── create ────────────────────────────────────────────────────────────────────

async fn run_create(mut ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    if !resolve_new_options(&mut ctx, NewOptions { required: &["channel"], private: &[] }).await {
        return Ok(());
    }
    if let Err(e) = create_inner(&ctx).await {
        bot_util::log_error!("starboard create failed", bot_core::error::log_arg(&e));
        reply(&ctx, failed_embed()).await;
    }
    Ok(())
}

async fn create_inner(ctx: &CommandCtx) -> anyhow::Result<()> {
    let app = &ctx.app;
    let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/starboard outside a guild"))?.to_string();
    let (_, module_off) = load_module_state(app, &guild_id).await;
    let raw = ctx.get_string("name").ok_or_else(|| anyhow::anyhow!("Required option \"name\" not found."))?;
    let Some(name) = sb::normalize_board_name(&raw) else {
        reply(ctx, invalid_name_embed()).await;
        return Ok(());
    };
    let io = starboard_io(app);
    let mut emojis = vec!["\u{2b50}".to_string()];
    if let Some(opt) = ctx.get_string("emoji") {
        let guild_emojis = io.guild(app, &guild_id).map(|g| g.emojis).unwrap_or_default();
        match sb::parse_emoji_list(&opt, &guild_emojis) {
            Ok(list) => emojis = list,
            Err(msg) => {
                reply(ctx, error_embed("Invalid Emoji", Some(&msg), None)).await;
                return Ok(());
            }
        }
    }
    let channel = resolve_channel(ctx, &guild_id, "channel").ok_or_else(|| anyhow::anyhow!("Required option \"channel\" not found."))?;
    let settings = read_settings(ctx);
    if settings.remove_self_stars == Some(true) && settings.self_star == Some(true) {
        reply(ctx, error_embed("Invalid Options", Some(SELF_STAR_CONFLICT), None)).await;
        return Ok(());
    }
    if let Some(problem) = board_channel_problem(app, &guild_id, &channel.id, settings.auto_react == Some(true)) {
        reply(ctx, problem).await;
        return Ok(());
    }
    if settings.allow_nsfw == Some(true) && !channel.nsfw {
        reply(ctx, nsfw_embed()).await;
        return Ok(());
    }

    let data = BoardPatch { name: Some(name.clone()), channel_id: Some(channel.id.clone()), emojis: Some(emojis), ..settings };
    let board = match sb::create_board(app, &guild_id, data).await {
        Ok(b) => b,
        Err(BoardError::Limit) => {
            let msg = format!(
                "A server can have at most {} starboards. Delete one with `/starboard delete` first.",
                bot_config::limits::STARBOARDS
            );
            reply(ctx, error_embed("Limit Reached", Some(&msg), None)).await;
            return Ok(());
        }
        Err(e) if e.is_unique_violation() => {
            reply(ctx, name_taken_embed(&name)).await;
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    let emoji_list = sb::format_emoji_list_app(app, &board.emojis);
    let description = format!(
        "**{}** reposts messages with **{}+** {} reactions to <#{}>.\n{}{}{}",
        board.name,
        board.threshold,
        emoji_list,
        channel.id,
        summary(app, &board, None),
        self_star_permission_note(app, &guild_id, &board),
        if module_off { MODULE_OFF_NOTE } else { "" }
    );
    reply(ctx, success_embed("Starboard Created", Some(&description), None)).await;
    Ok(())
}

// ── edit ──────────────────────────────────────────────────────────────────────

fn on_off(v: bool) -> String {
    if v { "on".into() } else { "off".into() }
}

/// `describeChanges(before, after, client)`: the "old → new" lines of `/starboard edit`.
pub fn describe_changes(app: &App, before: &Starboard, after: &Starboard) -> Vec<String> {
    let mut lines = Vec::new();
    let mut diff = |label: &str, changed: bool, old: String, new: String| {
        if changed {
            lines.push(format!("\u{2022} **{label}:** {old} \u{2192} {new}"));
        }
    };
    let emojis = |v: &[String]| sb::format_emoji_list_app(app, v);
    let age = |v: i32| if v > 0 { format!("{v} days") } else { "no limit".into() };
    diff("Name", before.name != after.name, before.name.clone(), after.name.clone());
    diff("Channel", before.channel_id != after.channel_id, format!("<#{}>", before.channel_id), format!("<#{}>", after.channel_id));
    diff("Emojis", before.emojis != after.emojis, emojis(&before.emojis), emojis(&after.emojis));
    diff("Threshold", before.threshold != after.threshold, before.threshold.to_string(), after.threshold.to_string());
    diff("Self-star", before.self_star != after.self_star, on_off(before.self_star), on_off(after.self_star));
    diff("Remove self-stars", before.remove_self_stars != after.remove_self_stars, on_off(before.remove_self_stars), on_off(after.remove_self_stars));
    let bots = |v: bool| if v { "ignored".to_string() } else { "counted".to_string() };
    diff("Bots", before.ignore_bots != after.ignore_bots, bots(before.ignore_bots), bots(after.ignore_bots));
    let below = |v: bool| if v { "remove".to_string() } else { "keep".to_string() };
    diff(
        "Below threshold",
        before.remove_below_threshold != after.remove_below_threshold,
        below(before.remove_below_threshold),
        below(after.remove_below_threshold),
    );
    diff("Auto-react", before.auto_react != after.auto_react, on_off(before.auto_react), on_off(after.auto_react));
    diff("Max age", before.max_message_age_days != after.max_message_age_days, age(before.max_message_age_days), age(after.max_message_age_days));
    let nsfw = |v: bool| if v { "allowed".to_string() } else { "blocked".to_string() };
    diff("NSFW", before.allow_nsfw != after.allow_nsfw, nsfw(before.allow_nsfw), nsfw(after.allow_nsfw));
    diff("Enabled", before.enabled != after.enabled, on_off(before.enabled), on_off(after.enabled));
    lines
}

async fn run_edit(mut ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    if !resolve_new_options(&mut ctx, NewOptions::default()).await {
        return Ok(());
    }
    let mut patch_name: Option<String> = None;
    if let Err(e) = edit_inner(&ctx, &mut patch_name).await {
        bot_util::log_error!("starboard edit failed", bot_core::error::log_arg(&e));
        reply(&ctx, failed_embed()).await;
    }
    Ok(())
}

async fn edit_inner(ctx: &CommandCtx, patch_name: &mut Option<String>) -> anyhow::Result<()> {
    let app = &ctx.app;
    let io = starboard_io(app);
    let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/starboard outside a guild"))?.to_string();
    let (_, module_off) = load_module_state(app, &guild_id).await;
    let raw = ctx.get_string("name").ok_or_else(|| anyhow::anyhow!("Required option \"name\" not found."))?;
    let normalized = sb::normalize_board_name(&raw);
    let Some(board) = sb::get_board_by_name(app, &guild_id, normalized.as_deref()).await? else {
        reply(ctx, not_found_embed(&raw)).await;
        return Ok(());
    };

    if !ctx.options().iter().any(|o| o.name != "name") {
        reply(ctx, error_embed("Nothing To Change", Some("Pass at least one option to change."), None)).await;
        return Ok(());
    }

    let mut patch = read_settings(ctx);
    if let Some(rename) = ctx.get_string("rename") {
        let Some(n) = sb::normalize_board_name(&rename) else {
            reply(ctx, invalid_name_embed()).await;
            return Ok(());
        };
        *patch_name = Some(n.clone());
        patch.name = Some(n);
    }
    if let Some(opt) = ctx.get_string("emoji") {
        let guild_emojis = io.guild(app, &guild_id).map(|g| g.emojis).unwrap_or_default();
        match sb::parse_emoji_list(&opt, &guild_emojis) {
            Ok(list) => patch.emojis = Some(list),
            Err(msg) => {
                reply(ctx, error_embed("Invalid Emoji", Some(&msg), None)).await;
                return Ok(());
            }
        }
    }
    let new_channel = resolve_channel(ctx, &guild_id, "channel");
    if let Some(c) = &new_channel {
        patch.channel_id = Some(c.id.clone());
    }

    let eff_self = patch.self_star.unwrap_or(board.self_star);
    let eff_remove = patch.remove_self_stars.unwrap_or(board.remove_self_stars);
    if eff_self && eff_remove {
        reply(ctx, error_embed("Invalid Options", Some(SELF_STAR_CONFLICT), None)).await;
        return Ok(());
    }
    let eff_auto = patch.auto_react.unwrap_or(board.auto_react);
    let eff_nsfw = patch.allow_nsfw.unwrap_or(board.allow_nsfw);

    // The channel the reposts will go to. A board channel missing from the cache is not treated as deleted.
    let target = new_channel.clone().or_else(|| io.guild(app, &guild_id).and_then(|g| g.channel(&board.channel_id).cloned()));
    if let Some(t) = &target
        && (new_channel.is_some() || patch.auto_react == Some(true))
        && let Some(problem) = board_channel_problem(app, &guild_id, &t.id, eff_auto)
    {
        reply(ctx, problem).await;
        return Ok(());
    }
    if target.as_ref().is_some_and(|t| eff_nsfw && !t.nsfw) {
        reply(ctx, nsfw_embed()).await;
        return Ok(());
    }

    let ignore = resolve_channel(ctx, &guild_id, "ignore_channel");
    let unignore = resolve_channel(ctx, &guild_id, "unignore_channel");
    if let (Some(i), Some(u)) = (&ignore, &unignore)
        && i.id == u.id
    {
        reply(ctx, error_embed("Invalid Options", Some("Choose different channels for ignore_channel and unignore_channel."), None)).await;
        return Ok(());
    }
    if let Some(i) = &ignore
        && !board.ignored_channel_ids.contains(&i.id)
        && board.ignored_channel_ids.len() >= sb::MAX_IGNORED_CHANNELS
    {
        reply(ctx, limit_ignored_embed()).await;
        return Ok(());
    }
    patch.add_ignored = ignore.as_ref().map(|c| c.id.clone());
    patch.remove_ignored = unignore.as_ref().map(|c| c.id.clone());

    let (before, after) = match sb::update_board(app, &guild_id, &board.id, patch).await {
        Ok(r) => r,
        Err(BoardError::NotFound) => {
            reply(ctx, not_found_embed(&raw)).await;
            return Ok(());
        }
        Err(BoardError::IgnoreLimit) => {
            reply(ctx, limit_ignored_embed()).await;
            return Ok(());
        }
        Err(BoardError::SettingsConflict) => {
            reply(ctx, error_embed("Invalid Options", Some(SELF_STAR_CONFLICT), None)).await;
            return Ok(());
        }
        Err(e) if e.is_unique_violation() => {
            reply(ctx, name_taken_embed(patch_name.as_deref().unwrap_or(&raw))).await;
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    let mut lines = describe_changes(app, &before, &after);
    if let Some(i) = &ignore {
        lines.push(if before.ignored_channel_ids.contains(&i.id) { format!("<#{}> was already ignored", i.id) } else { format!("+ <#{}> ignored", i.id) });
    }
    if let Some(u) = &unignore {
        lines.push(if before.ignored_channel_ids.contains(&u.id) {
            format!("\u{2212} <#{}> no longer ignored", u.id)
        } else {
            format!("<#{}> was not ignored", u.id)
        });
    }
    let mut description = format!("**{}**\n{}", after.name, if lines.is_empty() { "No settings changed.".to_string() } else { lines.join("\n") });
    if before.channel_id != after.channel_id {
        description.push_str(&format!("\nExisting reposts stay in <#{}>; new reposts go to <#{}>.", before.channel_id, after.channel_id));
    }
    description.push_str(&format!("\n\n{}{}", summary(app, &after, None), self_star_permission_note(app, &guild_id, &after)));
    if !after.enabled {
        description.push_str(&format!("\n\nThis board is **off**. Turn it back on with `/starboard edit name:{} enabled:True`.", after.name));
    }
    if module_off {
        description.push_str(MODULE_OFF_NOTE);
    }
    reply(ctx, success_embed("Starboard Updated", Some(&description), None)).await;
    Ok(())
}

// ── delete ────────────────────────────────────────────────────────────────────

async fn run_delete(ctx: CommandCtx) -> BotResult {
    // Deferred first: a slow database cannot miss Discord's 3-second window, and awaitConfirm edits the deferred reply.
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    if let Err(e) = delete_inner(&ctx).await {
        bot_util::log_error!("starboard delete failed", bot_core::error::log_arg(&e));
        let embed = error_embed("Something Went Wrong", Some("I could not delete that starboard. Please try again in a moment."), None);
        safe_reply(&ctx.responder, Payload::embed(embed).ephemeral(true)).await;
    }
    Ok(())
}

async fn delete_inner(ctx: &CommandCtx) -> anyhow::Result<()> {
    let app = &ctx.app;
    let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/starboard outside a guild"))?.to_string();
    let raw = ctx.get_string("name").ok_or_else(|| anyhow::anyhow!("Required option \"name\" not found."))?;
    let normalized = sb::normalize_board_name(&raw);
    let Some(board) = sb::get_board_by_name(app, &guild_id, normalized.as_deref()).await? else {
        reply(ctx, not_found_embed(&raw)).await;
        return Ok(());
    };
    let confirmed = await_confirm(
        ctx,
        ConfirmOptions {
            title: Some("Delete starboard?".into()),
            description: Some(format!("Delete **{}**? Reposts already in <#{}> stay, but they stop updating.", board.name, board.channel_id)),
            confirm_label: "Delete".into(),
            ..Default::default()
        },
    )
    .await;
    if !confirmed {
        return Ok(());
    }
    let deleted = sb::delete_board(app, &guild_id, &board.id).await?;
    if deleted == 0 {
        safe_reply(&ctx.responder, Payload::embed(error_embed("Not Found", Some("That starboard was already deleted."), None)).ephemeral(true)).await;
        return Ok(());
    }
    let text = format!("**{}** was deleted. Its reposts in <#{}> were kept.", board.name, board.channel_id);
    reply(ctx, success_embed("Starboard Deleted", Some(&text), None)).await;
    Ok(())
}

// ── list ──────────────────────────────────────────────────────────────────────

fn list_field_value(app: &App, guild_id: &str, b: &sb::BoardWithCount, ignored_max: usize) -> String {
    let mut lines = vec![summary(app, &b.board, Some(b.reposts))];
    if !b.board.ignored_channel_ids.is_empty() {
        let items: Vec<String> = b.board.ignored_channel_ids.iter().map(|id| format!("<#{id}>")).collect();
        lines.push(format!("Ignored: {}", bot_util::text::join_with_limit(&items, ", ", ignored_max, bot_util::text::default_more)));
    }
    if let Some(p) = board_problem(app, guild_id, &b.board) {
        lines.push(format!("\u{26a0}\u{fe0f} {p}"));
    }
    lines.join("\n")
}

/// The `/starboard list` fields: `(name, value)`, sharing [`sb::LIST_EMBED_BUDGET`].
pub fn list_fields(app: &App, guild_id: &str, boards: &[sb::BoardWithCount]) -> Vec<(String, String)> {
    let names: Vec<String> =
        boards.iter().map(|b| clamp_text(&format!("\u{1f31f} {}{}", b.board.name, if b.board.enabled { "" } else { " (off)" }), 256)).collect();
    let used: usize = names.iter().map(|n| bot_util::js::utf16_len(n)).sum();
    let per_field = 1024usize.min(sb::LIST_EMBED_BUDGET.saturating_sub(used) / boards.len().max(1));
    boards
        .iter()
        .zip(names)
        .map(|(b, name)| {
            let mut value = list_field_value(app, guild_id, b, 300);
            if bot_util::js::utf16_len(&value) > per_field {
                value = list_field_value(app, guild_id, b, 60);
            }
            (name, clamp_text(&value, per_field))
        })
        .collect()
}

async fn run_list(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    if let Err(e) = list_inner(&ctx).await {
        bot_util::log_error!("starboard list failed", bot_core::error::log_arg(&e));
        reply(&ctx, error_embed("Something Went Wrong", Some("I could not load the starboards. Please try again in a moment."), None)).await;
    }
    Ok(())
}

async fn list_inner(ctx: &CommandCtx) -> anyhow::Result<()> {
    let app = &ctx.app;
    let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/starboard outside a guild"))?.to_string();
    let (settings, module_off) = load_module_state(app, &guild_id).await;
    let note = if module_off { MODULE_OFF_NOTE } else { "" };
    let boards = sb::list_boards_with_counts(app, &guild_id).await?;
    if boards.is_empty() {
        let embed = info_embed("Starboards", Some(&format!("No starboards yet. Create one with `/starboard create`.{note}")), None);
        reply(ctx, apply_guild_color(embed, settings.as_deref())).await;
        return Ok(());
    }
    let mut embed = apply_guild_color(
        info_embed("Starboards", Some(&format!("{}/{} boards{note}", boards.len(), bot_config::limits::STARBOARDS)), None),
        settings.as_deref(),
    );
    for (name, value) in list_fields(app, &guild_id, &boards) {
        embed = embed.field(name, value, false);
    }
    reply(ctx, embed).await;
    Ok(())
}

// ── autocomplete ──────────────────────────────────────────────────────────────

/// The autocomplete choice names (`<name> → #<channel>` and ` (off)`).
pub fn autocomplete_choices(app: &App, guild_id: &str, rows: &[sb::BoardSearchRow]) -> Vec<Choice> {
    let guild = starboard_io(app).guild(app, guild_id);
    rows.iter()
        .map(|b| {
            let channel = guild.as_ref().and_then(|g| g.channel(&b.channel_id)).map(|c| c.name.clone()).unwrap_or_else(|| "deleted-channel".into());
            Choice::new(format!("{} \u{2192} #{channel}{}", b.name, if b.enabled { "" } else { " (off)" }), Value::from(b.name.clone()))
        })
        .collect()
}

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let focused = ctx.focused_value();
    let focused = bot_util::js::utf16_prefix(&bot_util::js::lower(&focused), 32).to_string();
    let Some(guild_id) = ctx.guild_id().map(|g| g.to_string()) else {
        respond_autocomplete(&ctx.responder, &[]).await;
        return Ok(());
    };
    // Bounded by the 3 s answer window; a slow database just gives an empty list.
    let budget = bot_core::interaction::ack_budget_ms(ctx.interaction.id);
    let (a, g, f) = (ctx.app.clone(), guild_id.clone(), focused.clone());
    let rows = tokio::time::timeout(std::time::Duration::from_millis(budget), async move { sb::search_boards(&a, &g, &f).await }).await.unwrap_or_default();
    let choices = autocomplete_choices(&ctx.app, &guild_id, &rows);
    respond_autocomplete(&ctx.responder, &choices).await;
    Ok(())
}
