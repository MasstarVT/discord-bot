//! Port of `src/commands/admin/counting.js`: `/counting set | reset | status`. An admin config command: replies are
//! public (the admin convention) and it keeps working while the Counting module is off (`gated = false`). The game
//! itself is [`crate::plugin`].

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::embed_builder::guild_embed;
use bot_core::interaction_helpers::{Payload, check_channel_usable, safe_defer, safe_reply};
use bot_core::new_targets::{NewOptions, new_channel_option, resolve_new_options};
use bot_core::prelude::*;
use bot_core::settings::is_module_enabled;
use serenity::all::{ChannelId, GuildId, Permissions};
use serenity::builder::CreateEmbed;

use crate::counting_service::{COUNT_MAX, NewState, current_state, load_state, run_locked, save_state};
use crate::plugin::post_note;

/// What the bot needs in the counting channel: read counts, react to them and post notes.
pub const CHANNEL_NEEDS: [&str; 5] = ["ViewChannel", "SendMessages", "EmbedLinks", "AddReactions", "ReadMessageHistory"];

/// The rules, as `/counting set` and the channel note show them.
pub const RULES: &str = "Count up from **1**, one number per message. Maths works too: `2+2` counts as **4** (`+ - * / % ^` and brackets). Nobody counts twice in a row, and a wrong number starts the count again from **1**.";

/// The note appended while the module is off.
pub const OFF_NOTE: &str = "\n\n⚠️ The **Counting** module is off. Turn it on with `/counting set` or `/module enable counting`.";

/// The command's data (`new SlashCommandBuilder()...`).
pub fn data() -> SlashCommand {
    SlashCommand::new("counting", "A channel where members count up from 1, one number each.")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(
            Opt::sub_command("set", "Pick the counting channel and turn counting on.")
                .option(Opt::channel("channel", "The counting channel.").channel_types(&[channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT]))
                .option(new_channel_option("channel", "channel")),
        )
        .option(
            Opt::sub_command("reset", "Set the count back to 0, or to the last number to carry on from.").option(
                Opt::integer("number", "The last number counted (default 0: start again from 1).").min_value(0).max_value(COUNT_MAX as f64),
            ),
        )
        .option(Opt::sub_command("status", "Show the counting channel, the next number and the best run."))
}

/// The command (`module 'counting'`, `gated = false`, ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Counting), execute).gated(false).permission_level(PermissionLevel::Admin)
}

fn settings_unavailable() -> CreateEmbed {
    error_embed("Settings Unavailable", Some("Server settings could not be loaded. Please try again in a moment."), None)
}

fn could_not_save() -> CreateEmbed {
    error_embed("Could Not Save", Some("The change could not be saved. Please try again in a moment."), None)
}

fn no_channel() -> CreateEmbed {
    error_embed("No Counting Channel", Some("Pick one first with `/counting set`."), None)
}

fn cannot_use(reason: &str) -> CreateEmbed {
    error_embed("Cannot Use That Channel", Some(&format!("{reason} Pick another channel, or fix my permissions there.")), None)
}

/// `finish(interaction, embed)`: the final answer (overwrites the deferred reply); never pings.
async fn finish(ctx: &CommandCtx, embed: CreateEmbed) -> BotResult {
    safe_reply(&ctx.responder, Payload::embed(embed).no_mentions()).await;
    Ok(())
}

async fn execute(ctx: CommandCtx) -> BotResult {
    match ctx.subcommand().as_deref() {
        Some("set") => run_set(ctx).await,
        Some("reset") => run_reset(ctx).await,
        Some("status") => run_status(ctx).await,
        _ => Ok(()),
    }
}

// ── /counting set ─────────────────────────────────────────────────────────────

async fn run_set(mut ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    if !resolve_new_options(&mut ctx, NewOptions { required: &["channel"], private: &[] }).await {
        return Ok(());
    }
    let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/counting outside a guild"))?;
    let option = ctx.get_channel_id("channel").ok_or_else(|| anyhow::anyhow!("Required option \"channel\" not found."))?;
    let usable = check_channel_usable(&ctx.app, guild_id, Some(option), &CHANNEL_NEEDS);
    if !usable.ok {
        return finish(&ctx, cannot_use(usable.reason.as_deref().unwrap_or_default())).await;
    }
    let g = guild_id.to_string();
    let Some(settings) = ctx.app.settings().get_settings_safe(&g).await else {
        return finish(&ctx, settings_unavailable()).await;
    };
    let channel = option.to_string();
    let moved = settings.counting_channel_id.as_deref() != Some(channel.as_str());

    let (app, gid, ch) = (ctx.app.clone(), g.clone(), channel.clone());
    let saved: anyhow::Result<()> = run_locked(&ctx.app, &g, move || async move {
        // A new channel starts from 1; picking the current channel again keeps its count.
        if moved {
            let best = load_state(app.db(), &gid).await?.map(|r| i64::from(r.best)).unwrap_or(0);
            save_state(&app, &gid, &NewState { channel_id: ch.clone(), count: 0, last_user_id: None, last_message_id: None, best }).await?;
        }
        app.settings().update_settings(&gid, SettingsPatch::new().set("countingChannelId", ch.clone()).set("countingEnabled", true)).await?;
        Ok(())
    })
    .await;
    if let Err(e) = saved {
        bot_util::log_error!(format!("[counting] {g}: set failed"), bot_core::error::log_arg(&e));
        return finish(&ctx, could_not_save()).await;
    }
    let before = settings.counting_channel_id.clone().unwrap_or_else(|| "null".into());
    bot_util::log_info!(format!("[counting] {g}: channel {before} → {channel} by {}", ctx.user().id));

    if moved {
        post_note(&ctx.app, guild_id, option, info_embed("Counting", Some(RULES), None)).await;
    }
    let head = if moved {
        format!("Counting is on in <#{channel}>. The first number is **1**.")
    } else {
        format!("Counting is on in <#{channel}>.")
    };
    finish(&ctx, success_embed("Counting Channel Set", Some(&format!("{head}\n{RULES}")), None)).await
}

// ── /counting reset ───────────────────────────────────────────────────────────

async fn run_reset(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/counting outside a guild"))?;
    let g = guild_id.to_string();
    let Some(settings) = ctx.app.settings().get_settings_safe(&g).await else {
        return finish(&ctx, settings_unavailable()).await;
    };
    let Some(channel_id) = settings.counting_channel_id.clone().filter(|c| !c.is_empty()) else {
        return finish(&ctx, no_channel()).await;
    };
    let number = ctx.get_integer("number").unwrap_or(0);

    let (app, gid, ch) = (ctx.app.clone(), g.clone(), channel_id.clone());
    let saved: anyhow::Result<()> = run_locked(&ctx.app, &g, move || async move {
        // The best run is only ever set by counting, never by an admin picking a number.
        let row = load_state(app.db(), &gid).await?;
        let best = current_state(row.as_ref(), Some(&ch)).best;
        save_state(&app, &gid, &NewState { channel_id: ch, count: number, last_user_id: None, last_message_id: None, best }).await?;
        Ok(())
    })
    .await;
    if let Err(e) = saved {
        bot_util::log_error!(format!("[counting] {g}: reset failed"), bot_core::error::log_arg(&e));
        return finish(&ctx, could_not_save()).await;
    }
    bot_util::log_info!(format!("[counting] {g}: count set to {number} by {}", ctx.user().id));

    let fmt = |n: i64| bot_util::text::format_number(n as f64);
    let next = format!("The next number is **{}**.", fmt(number + 1));
    if let Some(cid) = channel_id.parse::<u64>().ok().filter(|v| *v != 0).map(ChannelId::new)
        && bot_core::interaction_helpers::cached_channel(&ctx.app, guild_id, cid).is_some()
    {
        let note = format!("An admin set the count to **{}**. {next}", fmt(number));
        post_note(&ctx.app, guild_id, cid, info_embed("Count Reset", Some(&note), None)).await;
    }
    let off = if is_module_enabled(Some(&settings), Some(ModuleKey::Counting)) { "" } else { OFF_NOTE };
    finish(&ctx, success_embed("Count Reset", Some(&format!("{next} (in <#{channel_id}>){off}")), None)).await
}

// ── /counting status ──────────────────────────────────────────────────────────

/// The `/counting status` embed's fields: `(name, value, inline)`.
pub fn status_fields(
    enabled: bool,
    channel_id: Option<&str>,
    channel_exists: bool,
    loaded: bool,
    state: &crate::counting_service::State,
    problem: Option<&str>,
) -> Vec<(String, String, bool)> {
    let fmt = |n: i64| bot_util::text::format_number(n as f64);
    let mut fields = vec![
        ("Status".to_string(), if enabled { "✅ On".to_string() } else { "❌ Off — `/counting set`".to_string() }, true),
        (
            "Channel".to_string(),
            match channel_id {
                None => "Not set — `/counting set`".to_string(),
                Some(c) if channel_exists => format!("<#{c}>"),
                Some(c) => format!("⚠️ Deleted (`{c}`)"),
            },
            true,
        ),
        ("Next number".to_string(), if loaded { fmt(state.count + 1) } else { "Unavailable".to_string() }, true),
        ("Last counted by".to_string(), state.last_user_id.as_ref().map(|u| format!("<@{u}>")).unwrap_or_else(|| "—".to_string()), true),
        ("Best run".to_string(), if loaded { fmt(state.best) } else { "Unavailable".to_string() }, true),
    ];
    if let Some(p) = problem {
        fields.push(("Problem".to_string(), format!("⚠️ {p}"), false));
    }
    fields
}

async fn run_status(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id: GuildId = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/counting outside a guild"))?;
    let g = guild_id.to_string();
    let Some(settings) = ctx.app.settings().get_settings_safe(&g).await else {
        return finish(&ctx, settings_unavailable()).await;
    };
    let channel_id = settings.counting_channel_id.clone().filter(|c| !c.is_empty());
    let enabled = is_module_enabled(Some(&settings), Some(ModuleKey::Counting));

    let (row, loaded) = match load_state(ctx.app.db(), &g).await {
        Ok(r) => (r, true),
        Err(e) => {
            bot_util::log_warn!(format!("[counting] {g}: could not load the count: {e}"));
            (None, false)
        }
    };
    let state = current_state(row.as_ref(), channel_id.as_deref());
    let cid = channel_id.as_deref().and_then(|c| c.parse::<u64>().ok()).filter(|v| *v != 0).map(ChannelId::new);
    let exists = cid.is_some_and(|c| bot_core::interaction_helpers::cached_channel(&ctx.app, guild_id, c).is_some());
    let problem = if exists {
        let usable = check_channel_usable(&ctx.app, guild_id, cid, &CHANNEL_NEEDS);
        if usable.ok { None } else { usable.reason }
    } else {
        None
    };
    let mut embed = guild_embed(Some(&settings), Some("🔢 Counting"), None, None);
    for (name, value, inline) in status_fields(enabled, channel_id.as_deref(), exists, loaded, &state, problem.as_deref()) {
        embed = embed.field(name, value, inline);
    }
    finish(&ctx, embed).await
}
