//! Port of `src/commands/community/bumps.js`: `/bumps status | leaderboard | setup | reset`.
//!
//! The bot watches for a successful Disboard /bump, reminds the server when it can bump again and keeps a
//! leaderboard. Members run `status` and `leaderboard`; admins run `setup` and `reset`. The command is not gated, so
//! admins can set the channel and role up before turning the module on; the member subcommands check the module
//! themselves and refuse with the router's wording while it is off. It is `/bumps`, not `/bump`, so it never shadows
//! Disboard's own /bump.

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::embed_builder::guild_embed;
use bot_core::interaction_helpers::{ConfirmOptions, Payload, await_confirm, cached_channel, cached_guild, check_channel_usable, pingable_role_ids, safe_defer, safe_reply};
use bot_core::prelude::*;
use chrono::{DateTime, Utc};
use serenity::builder::CreateEmbed;

use crate::bump_service::{BUMP_SITES, LEADERBOARD_SIZE, bump_count_of, get_bump_states, module_off_embed, reset_counts, top_bumpers};

const MEDALS: [&str; 3] = ["🥇", "🥈", "🥉"];

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("bumps", "Bump reminders and the bump leaderboard.")
        .option(Opt::sub_command("status", "When the server can be bumped again."))
        .option(Opt::sub_command("leaderboard", "Who has bumped the server the most."))
        .option(
            Opt::sub_command("setup", "Set where and how to remind the server (no options: show them).")
                .option(Opt::channel("channel", "Where reminders are posted").channel_types(&[channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT]))
                .option(Opt::role("role", "Role to ping when the server can be bumped"))
                .option(Opt::boolean("thanks", "Thank members right after they bump"))
                .option(Opt::integer("coins", "Coins for each bump (0 = none)").min_value(0.0).max_value(100_000.0)),
        )
        .option(Opt::sub_command("reset", "Clear bump counts.").option(Opt::user("user", "Only this member (empty = everyone)")))
}

/// The command (`module 'bumps'`, `gated = false`, MEMBER; setup and reset ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("community", data(), Some(ModuleKey::Bumps), execute)
        .gated(false)
        .permission_level(PermissionLevel::Member)
        .subcommand_permission("setup", PermissionLevel::Admin)
        .subcommand_permission("reset", PermissionLevel::Admin)
}

fn settings_unavailable() -> CreateEmbed {
    error_embed("Settings Unavailable", Some("Server settings could not be loaded. Please try again in a moment."), None)
}

fn db_error() -> CreateEmbed {
    error_embed("Something Went Wrong", Some("That could not be done right now. Please try again in a moment."), None)
}

/// `plural(n, word)`: `1 bump`, `1,234 bumps`.
pub fn plural(n: i64, word: &str) -> String {
    format!("{} {word}{}", bot_util::text::format_number(n as f64), if n == 1 { "" } else { "s" })
}

fn yes_no(v: bool) -> &'static str {
    if v { "Yes" } else { "No" }
}

/// `stamp(date, style)`: `<t:unix:style>`.
pub fn stamp(date: &DateTime<Utc>, style: &str) -> String {
    format!("<t:{}:{style}>", date.timestamp_millis().div_euclid(1000))
}

async fn finish(ctx: &CommandCtx, embed: CreateEmbed) -> BotResult {
    safe_reply(&ctx.responder, Payload::embed(embed).no_mentions()).await;
    Ok(())
}

async fn execute(ctx: CommandCtx) -> BotResult {
    match ctx.subcommand().as_deref() {
        Some("status") => status(ctx).await,
        Some("leaderboard") => leaderboard(ctx).await,
        Some("setup") => setup(ctx).await,
        Some("reset") => reset(ctx).await,
        _ => Ok(()),
    }
}

// ── Gate for the member subcommands ───────────────────────────────────────────

/// `settingsInBudget(interaction)`: the memo, else a lookup bounded by what is left of the 3 s window.
pub async fn settings_in_budget(app: &App, guild_id: &str, budget_ms: u64) -> Option<Settings> {
    settings_in_budget_labeled(app, guild_id, budget_ms, "bumps settings").await
}

/// [`settings_in_budget`] with the timeout's log label (`'suggestion settings'`, `'confession settings'`).
pub async fn settings_in_budget_labeled(app: &App, guild_id: &str, budget_ms: u64, label: &str) -> Option<Settings> {
    if let Some(hit) = app.settings().peek_settings(guild_id) {
        return Some(hit);
    }
    if budget_ms == 0 {
        return None;
    }
    let (a, g) = (app.clone(), guild_id.to_string());
    bot_util::async_util::with_timeout(async move { Ok::<_, String>(a.settings().get_settings_safe(&g).await) }, budget_ms, label).await.ok().flatten()
}

/// `memberGate(interaction)`: the settings, or `None` after replying (ephemerally) that the module is off or the
/// settings are missing.
async fn member_gate(ctx: &CommandCtx) -> Option<Settings> {
    let g = ctx.guild_id()?.to_string();
    let Some(settings) = settings_in_budget(&ctx.app, &g, ctx.ack_budget_ms()).await else {
        safe_reply(&ctx.responder, Payload::embed(settings_unavailable()).ephemeral(true)).await;
        return None;
    };
    if !is_module_enabled(Some(&settings), Some(ModuleKey::Bumps)) {
        safe_reply(&ctx.responder, Payload::embed(module_off_embed()).ephemeral(true)).await;
        return None;
    }
    Some(settings)
}

// ── status ────────────────────────────────────────────────────────────────────

/// The `/bumps status` fields: `(name, value)` per site.
pub fn status_fields(states: &[bot_db::BumpState], now_ms: i64) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for site in BUMP_SITES.iter() {
        let Some(s) = states.iter().find(|s| s.site == site.key) else {
            out.push((site.name.to_string(), format!("No bump seen yet. Use `/bump` with {}; I'll count it and remind you in time.", site.name)));
            continue;
        };
        let ready = s.next_bump_at.timestamp_millis() <= now_ms;
        let by = s.last_bumper_id.as_deref().map(|b| format!(" by <@{b}>")).unwrap_or_default();
        let next = if ready { "**now**, the server can be bumped!".to_string() } else { format!("{} ({})", stamp(&s.next_bump_at, "R"), stamp(&s.next_bump_at, "t")) };
        out.push((site.name.to_string(), format!("Last bump: {}{by}\nNext bump: {next}", stamp(&s.last_bump_at, "R"))));
    }
    out
}

async fn status(ctx: CommandCtx) -> BotResult {
    let Some(settings) = member_gate(&ctx).await else { return Ok(()) };
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let g = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
    let states = match get_bump_states(ctx.app.db(), &g).await {
        Ok(s) => s,
        Err(e) => {
            bot_util::log_error!(format!("[bumps] {g}: status failed"), bot_core::error::log_arg(&e));
            return finish(&ctx, db_error()).await;
        }
    };
    let mut embed = guild_embed(Some(&settings), Some("📣 Bump status"), None, None);
    for (name, value) in status_fields(&states, ctx.app.clock().now_ms()) {
        embed = embed.field(name, value, false);
    }
    finish(&ctx, embed).await
}

// ── leaderboard ───────────────────────────────────────────────────────────────

/// The leaderboard lines: `🥇 <@id> — 3 bumps`, then `**4.**`.
pub fn leaderboard_lines(rows: &[bot_db::BumpCount]) -> Vec<String> {
    rows.iter()
        .enumerate()
        .map(|(i, r)| {
            let rank = MEDALS.get(i).map(|m| m.to_string()).unwrap_or_else(|| format!("**{}.**", i + 1));
            format!("{rank} <@{}> — {}", r.user_id, plural(i64::from(r.count), "bump"))
        })
        .collect()
}

async fn leaderboard(ctx: CommandCtx) -> BotResult {
    let Some(settings) = member_gate(&ctx).await else { return Ok(()) };
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let g = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
    let me = ctx.user().id.to_string();
    let loaded = async {
        let rows = top_bumpers(ctx.app.db(), &g, LEADERBOARD_SIZE).await?;
        let own = if rows.iter().any(|r| r.user_id == me) { 0 } else { bump_count_of(ctx.app.db(), &g, &me).await? };
        Ok::<_, anyhow::Error>((rows, own))
    };
    let (rows, own) = match loaded.await {
        Ok(v) => v,
        Err(e) => {
            bot_util::log_error!(format!("[bumps] {g}: leaderboard failed"), bot_core::error::log_arg(&e));
            return finish(&ctx, db_error()).await;
        }
    };
    if rows.is_empty() {
        return finish(&ctx, guild_embed(Some(&settings), Some("🏆 Bump leaderboard"), Some("Nobody has bumped yet. Be the first with `/bump`!"), None)).await;
    }
    let lines = leaderboard_lines(&rows).join("\n");
    let footer = (own > 0).then(|| format!("You: {}", plural(own, "bump")));
    finish(&ctx, guild_embed(Some(&settings), Some("🏆 Bump leaderboard"), Some(&lines), footer.as_deref())).await
}

// ── setup ─────────────────────────────────────────────────────────────────────

/// The setup embed's description.
pub fn config_lines(settings: &bot_db::GuildSettings) -> String {
    let coins = settings.bump_reward_coins;
    [
        format!(
            "Module: {}",
            if is_module_enabled(Some(settings), Some(ModuleKey::Bumps)) { "✅ On" } else { "❌ Off, turn it on with `/module enable bumps`" }
        ),
        format!("Reminder channel: {}", settings.bump_channel_id.as_deref().map(|c| format!("<#{c}>")).unwrap_or_else(|| "the channel of the last bump".into())),
        format!("Role to ping: {}", settings.bump_role_id.as_deref().map(|r| format!("<@&{r}>")).unwrap_or_else(|| "none".into())),
        format!("Thank the bumper: {}", yes_no(settings.bump_thanks)),
        format!("Coins per bump: {}", if coins > 0 { bot_util::text::format_number(f64::from(coins)) } else { "none".into() }),
    ]
    .join("\n")
}

fn config_embed(settings: &bot_db::GuildSettings, title: &str) -> CreateEmbed {
    guild_embed(Some(settings), Some(title), Some(&config_lines(settings)), None)
}

async fn setup(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let Some(guild_id) = ctx.guild_id() else { return Ok(()) };
    let g = guild_id.to_string();
    let Some(settings) = ctx.app.settings().get_settings_safe(&g).await else {
        return finish(&ctx, settings_unavailable()).await;
    };

    let mut update = SettingsPatch::new();
    if let Some(t) = ctx.get_boolean("thanks") {
        update.insert("bumpThanks", t);
    }
    if let Some(c) = ctx.get_integer("coins") {
        update.insert("bumpRewardCoins", c);
    }
    if let Some(picked) = ctx.get_channel_id("channel") {
        let usable = check_channel_usable(&ctx.app, guild_id, Some(picked), &["ViewChannel", "SendMessages"]);
        if !usable.ok {
            let reason = usable.reason.unwrap_or_default();
            return finish(&ctx, error_embed("Cannot Use That Channel", Some(&format!("{reason} Pick another channel, or fix my permissions there.")), None)).await;
        }
        update.insert("bumpChannelId", picked.to_string());
    }
    if let Some(role) = ctx.get_role_id("role") {
        if role.get() == guild_id.get() {
            return finish(&ctx, error_embed("Cannot Ping @everyone", Some("Pick a role made for bump reminders instead of @everyone."), None)).await;
        }
        update.insert("bumpRoleId", role.to_string());
    }
    if update.is_empty() {
        return finish(&ctx, config_embed(&settings, "📣 Bump Reminders")).await;
    }
    let keys: Vec<String> = update.iter().map(|(k, _)| k.clone()).collect();
    let saved = match ctx.app.settings().update_settings(&g, update).await {
        Ok(s) => s,
        Err(e) => {
            bot_util::log_error!(format!("[bumps] {g}: setup failed"), bot_core::error::log_arg(&anyhow::Error::from(e)));
            return finish(&ctx, error_embed("Could Not Save", Some("The change could not be saved. Please try again in a moment."), None)).await;
        }
    };
    bot_util::log_info!(format!("[bumps] {g}: setup changed {} by {}", keys.join(", "), ctx.user().id));

    let mut embed = config_embed(&saved, "✅ Bump Reminders Updated");
    let saved_channel = saved.bump_channel_id.as_deref().and_then(crate::io::snowflake).map(serenity::all::ChannelId::new);
    let target = saved_channel.and_then(|c| cached_channel(&ctx.app, guild_id, c)).or_else(|| cached_channel(&ctx.app, guild_id, ctx.channel_id()));
    if let (Some(role), Some(target), Some(guild)) = (saved.bump_role_id.as_deref(), target, cached_guild(&ctx.app, guild_id)) {
        let bot = crate::io::bot_user_id(&ctx.app);
        let my_roles = bot.and_then(|b| guild.members.get(&b)).map(|m| m.roles.clone()).unwrap_or_default();
        let allowed = pingable_role_ids(&guild, &target, bot.map(|b| (b, my_roles.as_slice())), &[role]).allowed;
        if allowed.is_empty() {
            embed = embed.field(
                "Role",
                format!("⚠️ I can't ping <@&{role}>: it isn't mentionable and I don't have **Mention @everyone, @here and All Roles**. Make the role mentionable, or give me that permission."),
                false,
            );
        }
    }
    if saved.bump_reward_coins > 0 && !is_module_enabled(Some(&saved), Some(ModuleKey::Economy)) {
        embed = embed.field("Coins", "⚠️ The Economy module is off, so no coins are given. Turn it on with `/module enable economy`.", false);
    }
    finish(&ctx, embed).await
}

// ── reset ─────────────────────────────────────────────────────────────────────

async fn reset(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let g = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
    let user = ctx.get_user_id("user");
    if user.is_none() {
        let yes = await_confirm(
            &ctx,
            ConfirmOptions {
                title: Some("Clear all bump counts?".into()),
                description: Some("This removes every member's bump count and empties the leaderboard. Reminders keep working. It cannot be undone.".into()),
                confirm_label: "Clear all".into(),
                ..Default::default()
            },
        )
        .await;
        if !yes {
            return Ok(());
        }
    }
    let uid = user.map(|u| u.to_string());
    let removed = match reset_counts(ctx.app.db(), &g, uid.as_deref()).await {
        Ok(n) => n,
        Err(e) => {
            bot_util::log_error!(format!("[bumps] {g}: reset failed"), bot_core::error::log_arg(&e));
            return finish(&ctx, db_error()).await;
        }
    };
    bot_util::log_info!(format!("[bumps] {g}: counts of {} cleared by {}", uid.as_deref().unwrap_or("everyone"), ctx.user().id));
    let embed = match &uid {
        Some(u) => success_embed(
            "Bump Count Cleared",
            Some(&if removed > 0 { format!("The bump count of <@{u}> was cleared.") } else { format!("<@{u}> had no bumps counted.") }),
            None,
        ),
        None => success_embed(
            "Bump Counts Cleared",
            Some(&if removed > 0 { format!("Cleared the counts of {}.", plural(removed as i64, "member")) } else { "There were no bump counts to clear.".to_string() }),
            None,
        ),
    };
    finish(&ctx, embed).await
}
