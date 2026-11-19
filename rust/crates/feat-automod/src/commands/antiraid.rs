//! Port of `src/commands/admin/antiraid.js`: `/antiraid status | lockdown start | lockdown end | config`. An admin
//! command: replies are public (the admin convention) and it keeps working while the Anti-Raid module is off
//! (`gated = false`), so a lockdown can be started or ended and the module set up first. Detection and the actions
//! on joiners are [`crate::plugins::antiraid`].

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::interaction_helpers::{Payload, check_channel_usable, safe_defer, safe_reply};
use bot_core::prelude::*;
use bot_util::text::clamp_text;
use serde_json::{Value, json};
use serenity::all::{GuildId, Permissions};
use serenity::builder::CreateEmbed;

use crate::anti_raid_service::{
    EndAnnounce, Started, StartInput, announce_end, announce_start, build_end_embed, build_start_embed, config_lines, end_lockdown, is_active,
    load_state, permission_problems, raid, start_lockdown,
};

fn action_choices(o: Opt) -> Opt {
    o.choice("Alert only", "ALERT").choice("Kick", "KICK").choice("Ban", "BAN")
}

/// The command's data (`new SlashCommandBuilder()...`).
pub fn data() -> SlashCommand {
    SlashCommand::new("antiraid", "Raid protection: join floods, lockdowns and brand-new accounts.")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(Opt::sub_command("status", "Show the lockdown, settings and problems."))
        .option(
            Opt::sub_command_group("lockdown", "Start or end a lockdown by hand.")
                .option(
                    Opt::sub_command("start", "Start a lockdown now.")
                        .option(Opt::integer("minutes", "Minutes (default: the setting).").min_value(raid::MINUTES.min).max_value(raid::MINUTES.max))
                        .option(Opt::string("reason", "Why (shown in the alert).").max_length(raid::REASON_MAX as u16)),
                )
                .option(Opt::sub_command("end", "End it and undo what it changed.")),
        )
        .option(
            Opt::sub_command("config", "Change the settings (no options: show them).")
                .option(Opt::boolean("enabled", "Detection on or off."))
                .option(Opt::integer("joins", "Joins that count as a raid.").min_value(raid::THRESHOLD.min).max_value(raid::THRESHOLD.max))
                .option(Opt::integer("seconds", "Window the joins are counted in.").min_value(raid::SECONDS.min).max_value(raid::SECONDS.max))
                .option(action_choices(Opt::string("action", "Members who join during a raid.")))
                .option(Opt::boolean("pause_invites", "Pause invites during a lockdown."))
                .option(Opt::boolean("lock_chat", "Take Send Messages from @everyone."))
                .option(Opt::integer("minutes", "How long a lockdown lasts.").min_value(raid::MINUTES.min).max_value(raid::MINUTES.max))
                .option(
                    Opt::integer("min_account_age", "Days; younger accounts get young_action (0 = off).")
                        .min_value(raid::AGE_DAYS.min)
                        .max_value(raid::AGE_DAYS.max),
                )
                .option(action_choices(Opt::string("young_action", "Action for too-young accounts.")))
                .option(
                    Opt::channel("alert_channel", "Alert channel (default: the mod log).")
                        .channel_types(&[channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT]),
                ),
        )
}

/// The command (`module 'antiraid'`, `gated = false`, ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Antiraid), execute).gated(false).permission_level(PermissionLevel::Admin)
}

fn settings_unavailable() -> CreateEmbed {
    error_embed("Settings Unavailable", Some("Server settings could not be loaded. Please try again in a moment."), None)
}

fn could_not_save() -> CreateEmbed {
    error_embed("Could Not Save", Some("The change could not be saved. Please try again in a moment."), None)
}

fn could_not_end() -> CreateEmbed {
    error_embed("Could Not End Lockdown", Some("The lockdown could not be ended. Please try again in a moment."), None)
}

fn cannot_use(reason: &str) -> CreateEmbed {
    error_embed("Cannot Use That Channel", Some(&format!("{reason} Pick another channel, or fix my permissions there.")), None)
}

/// The note shown while the module is off.
pub const OFF_NOTE: &str = "⚠️ The **Anti-Raid** module is off: raids are not detected and members who join are not actioned. Turn it on with `/antiraid config enabled:true` or `/module enable antiraid`.";

/// `finish(interaction, embed)`: the final answer (overwrites the deferred reply); never pings.
async fn finish(ctx: &CommandCtx, embed: CreateEmbed) -> BotResult {
    safe_reply(&ctx.responder, Payload::embed(embed).no_mentions()).await;
    Ok(())
}

async fn execute(ctx: CommandCtx) -> BotResult {
    let group = ctx.subcommand_group();
    let sub = ctx.subcommand();
    if group.as_deref() == Some("lockdown") {
        return if sub.as_deref() == Some("start") { run_start(ctx).await } else { run_end(ctx).await };
    }
    match sub.as_deref() {
        Some("status") => run_status(ctx).await,
        Some("config") => run_config(ctx).await,
        _ => Ok(()),
    }
}

fn guild_of(ctx: &CommandCtx) -> anyhow::Result<GuildId> {
    ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/antiraid outside a guild"))
}

fn module_on(settings: &bot_db::GuildSettings) -> bool {
    is_module_enabled(Some(settings), Some(ModuleKey::Antiraid))
}

// ── /antiraid status ──────────────────────────────────────────────────────────

async fn run_status(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = guild_of(&ctx)?;
    let g = guild_id.to_string();
    let Some(settings) = ctx.app.settings().get_settings_safe(&g).await else { return finish(&ctx, settings_unavailable()).await };

    let (row, loaded) = match load_state(ctx.app.db(), &g).await {
        Ok(r) => (r, true),
        Err(e) => {
            bot_util::log_warn!(format!("[antiraid] {g}: could not load the lockdown: {e}"));
            (None, false)
        }
    };
    let now = ctx.app.clock().now_ms();
    let lockdown = if !loaded {
        "Unavailable".to_string()
    } else if is_active(row.as_ref(), now) {
        let r = row.as_ref();
        let until = r.and_then(|r| r.locked_until).map(|u| bot_util::time::discord_timestamp(&u, "R")).unwrap_or_default();
        let by = match r.and_then(|r| r.locked_by_id.as_deref()) {
            Some(id) => format!("started by <@{id}>"),
            None => "started by a detected raid".into(),
        };
        let reason = r.and_then(|r| r.reason.as_deref()).filter(|x| !x.is_empty()).map(|x| format!(": {}", clamp_text(x, raid::REASON_MAX))).unwrap_or_default();
        format!("🔒 On until {until}, {by}{reason}")
    } else if row.as_ref().is_some_and(|r| r.locked_at.is_some()) {
        "🔒 Ran out and is being ended".into()
    } else {
        "🔓 Off".into()
    };

    let mut embed = guild_embed(Some(&settings), Some("🚨 Anti-Raid"), None, None)
        .field("Detection", if module_on(&settings) { "✅ On" } else { "❌ Off: `/antiraid config enabled:true`" }, true)
        .field("Lockdown", lockdown, true)
        .field("Settings", config_lines(&settings).join("\n"), false);
    let mut problems = permission_problems(&ctx.app, guild_id, Some(&settings));
    let alert = settings.anti_raid_alert_channel_id.clone().or_else(|| settings.mod_log_channel_id.clone()).filter(|a| !a.is_empty());
    match alert {
        None => problems.push("No alert channel: set one with `/antiraid config alert_channel:`, or a moderation log channel.".into()),
        Some(id) => {
            let cid = id.parse::<u64>().ok().filter(|v| *v != 0).map(serenity::all::ChannelId::new);
            if !check_channel_usable(&ctx.app, guild_id, cid, &["ViewChannel", "SendMessages", "EmbedLinks"]).ok {
                problems.push(format!("I cannot post alerts in <#{id}>."));
            }
        }
    }
    if loaded && row.as_ref().is_some_and(|r| r.locked_at.is_none() && r.prior_everyone_send.is_some()) {
        problems.push("@everyone still lacks some send permissions from an earlier lockdown: run `/antiraid lockdown end` to give them back.".into());
    }
    if !problems.is_empty() {
        embed = embed.field("Needs attention", clamp_text(&problems.iter().map(|p| format!("• {p}")).collect::<Vec<_>>().join("\n"), 1024), false);
    }
    finish(&ctx, embed).await
}

// ── /antiraid lockdown start | end ────────────────────────────────────────────

async fn run_start(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = guild_of(&ctx)?;
    let g = guild_id.to_string();
    let Some(settings) = ctx.app.settings().get_settings_safe(&g).await else { return finish(&ctx, settings_unavailable()).await };
    let user = ctx.user().id.to_string();
    let res = start_lockdown(
        &ctx.app,
        guild_id,
        StartInput {
            guild_id: Some(guild_id),
            settings: Some(settings.clone()),
            started_by_id: Some(user.clone()),
            reason: ctx.get_string("reason"),
            minutes: ctx.get_integer("minutes"),
            trigger: None,
        },
    )
    .await;
    let (state, effects) = match res {
        Started::Failed { error } => return finish(&ctx, error_embed("Could Not Start Lockdown", Some(&error), None)).await,
        Started::Ok { started: false, state, .. } => {
            let until = state.locked_until.map(|u| bot_util::time::discord_timestamp(&u, "R")).unwrap_or_default();
            return finish(
                &ctx,
                warn_embed("Already Locked Down", Some(&format!("A lockdown is already on until {until}. End it with `/antiraid lockdown end`.")), None),
            )
            .await;
        }
        Started::Ok { state, effects, .. } => (state, effects),
    };
    bot_util::log_info!(format!("[antiraid] {g}: lockdown started by {user}"));

    let posted = announce_start(&ctx.app, guild_id, Some(&settings), &state, effects.as_ref(), None).await;
    if posted.as_ref().is_some_and(|(c, _)| *c == ctx.channel_id()) {
        return finish(&ctx, success_embed("Lockdown Started", Some("The alert above has the details and the button to end it."), None)).await;
    }
    let problems = permission_problems(&ctx.app, guild_id, Some(&settings));
    let embed = build_start_embed(&state, Some(&settings), effects.as_ref(), None, &problems);
    let mut notes = Vec::new();
    if posted.is_none() {
        notes.push("No alert was posted: I have no usable alert channel (`/antiraid config alert_channel:`).".to_string());
    }
    if !module_on(&settings) {
        notes.push(OFF_NOTE.to_string());
    }
    let mut payload = Payload::embed(embed).no_mentions();
    if !notes.is_empty() {
        payload = payload.content(notes.join("\n"));
    }
    safe_reply(&ctx.responder, payload).await;
    Ok(())
}

async fn run_end(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = guild_of(&ctx)?;
    let g = guild_id.to_string();
    let settings = ctx.app.settings().get_settings_safe(&g).await;
    let res = match end_lockdown(&ctx.app, guild_id, false).await {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_error!(format!("[antiraid] {g}: ending the lockdown failed"), bot_core::error::log_arg(&e));
            return finish(&ctx, could_not_end()).await;
        }
    };
    if !res.was_active && !res.restored && !res.pending_restore {
        return finish(&ctx, info_embed("No Lockdown", Some("There is no lockdown to end."), None)).await;
    }
    let user = ctx.user().id.to_string();
    bot_util::log_info!(format!("[antiraid] {g}: lockdown ended by {user}"));
    announce_end(&ctx.app, guild_id, settings.as_deref(), &res, EndAnnounce { how: "manual", ended_by_id: Some(&user), except_channel_id: Some(ctx.channel_id()) }).await;
    finish(&ctx, build_end_embed("manual", Some(&user), Some(&res))).await
}

// ── /antiraid config ──────────────────────────────────────────────────────────

/// `OPTION_FIELDS`: option name, getter kind and settings field.
pub const OPTION_FIELDS: [(&str, &str, &str); 9] = [
    ("enabled", "boolean", "antiRaidEnabled"),
    ("joins", "integer", "antiRaidJoinThreshold"),
    ("seconds", "integer", "antiRaidJoinSeconds"),
    ("action", "string", "antiRaidAction"),
    ("pause_invites", "boolean", "antiRaidPauseInvites"),
    ("lock_chat", "boolean", "antiRaidLockChat"),
    ("minutes", "integer", "antiRaidLockdownMinutes"),
    ("min_account_age", "integer", "antiRaidMinAccountAgeDays"),
    ("young_action", "string", "antiRaidYoungAction"),
];

async fn run_config(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = guild_of(&ctx)?;
    let g = guild_id.to_string();
    let Some(settings) = ctx.app.settings().get_settings_safe(&g).await else { return finish(&ctx, settings_unavailable()).await };

    let mut changes = SettingsPatch::new();
    let mut names = Vec::new();
    for (option, kind, field) in OPTION_FIELDS {
        let value: Option<Value> = match kind {
            "boolean" => ctx.get_boolean(option).map(Value::from),
            "integer" => ctx.get_integer(option).map(Value::from),
            _ => ctx.get_string(option).map(Value::from),
        };
        if let Some(v) = value {
            changes.insert(field, v);
            names.push(field);
        }
    }
    if let Some(picked) = ctx.get_channel_id("alert_channel") {
        // discord.js `_add`s a picked channel to the cache from the interaction's resolved data, so the JS
        // `guild.channels.cache.get(picked.id)` always found it: an uncached pick is fetched here.
        let usable = bot_core::interaction_helpers::check_channel_usable_fetch(&ctx.app, guild_id, Some(picked), &["ViewChannel", "SendMessages", "EmbedLinks"]).await;
        if !usable.ok {
            return finish(&ctx, cannot_use(usable.reason.as_deref().unwrap_or_default())).await;
        }
        changes.insert("antiRaidAlertChannelId", json!(picked.to_string()));
        names.push("antiRaidAlertChannelId");
    }

    if changes.is_empty() {
        let text = format!("{}\n\nChange them with the options of this command, or on the dashboard.", config_lines(&settings).join("\n"));
        return finish(&ctx, guild_embed(Some(&settings), Some("🚨 Anti-Raid Settings"), Some(&text), None)).await;
    }
    let updated = match ctx.app.settings().update_settings(&g, changes).await {
        Ok(u) => u,
        Err(e) => {
            bot_util::log_error!(format!("[antiraid] {g}: config failed"), e.to_string());
            return finish(&ctx, could_not_save()).await;
        }
    };
    bot_util::log_info!(format!("[antiraid] {g}: settings {} changed by {}", names.join(", "), ctx.user().id));

    let mut parts = vec![config_lines(&updated).join("\n")];
    let problems = permission_problems(&ctx.app, guild_id, Some(&updated));
    if !problems.is_empty() {
        parts.push(problems.iter().map(|p| format!("⚠️ {p}")).collect::<Vec<_>>().join("\n"));
    }
    if !module_on(&updated) {
        parts.push(OFF_NOTE.to_string());
    }
    finish(&ctx, success_embed("Anti-Raid Updated", Some(&parts.join("\n\n")), None)).await
}
