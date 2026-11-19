//! Port of `src/commands/community/giveaway.js`: MEE6 Giveaways with a persistent Participate button, role-based
//! odds, required and denied roles, XP/coin prizes through the rewards bus and up to 90 winners. Thin: option parsing,
//! validation and replies; every flow lives in [`crate::giveaway_service`]. Every reply is ephemeral; the giveaway
//! message and the announcements are the public output.

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::custom_embed::parse_color_str;
use bot_core::embed_builder::guild_embed;
use bot_core::interaction_helpers::{Choice, ConfirmOptions, Payload, await_confirm, cached_channel, respond_autocomplete, safe_defer};
use bot_core::paginator::{PaginateOptions, paginate};
use bot_core::permissions::permission_names;
use bot_core::prelude::*;
use bot_config::limits;
use bot_db::{Giveaway, GiveawayStatus};
use bot_util::text::{clamp_text, default_more, format_number, join_with_limit};
use bot_util::time::discord_timestamp;
use chrono::{DateTime, Utc};
use serenity::all::{ChannelType, GuildChannel, Permissions};
use serenity::builder::CreateEmbed;

use crate::giveaway_service::{
    self as svc, DEFAULT_BONUS_WEIGHT, ENTRANTS_SHOWN, EndSource, GiveawayError, LIST_SHOWN, ListFilter, MAX_PRIZE_AMOUNT, MAX_ROLE_WEIGHTS,
    MIN_DURATION_MS, OddsOp, POST_NEEDS, RoleWeight, StartGiveaway, normalize_role_weights, status_emoji, status_text, status_text_short,
};
use crate::guild_view::{self, channel_usable};

// ── Builder ───────────────────────────────────────────────────────────────────

fn gw() -> Opt {
    Opt::string("giveaway", "Pick a giveaway, or paste its ID or message link").required(true).autocomplete(true).max_length(100)
}

const TEXT_TYPES: [u8; 2] = [channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT];

/// The command's data (`new SlashCommandBuilder()...`).
pub fn data() -> SlashCommand {
    let winners_max = limits::GIVEAWAY_WINNERS as i64;
    SlashCommand::new("giveaway", "Run giveaways with a Participate button (MEE6 Giveaways, /reroll)")
        .default_member_permissions(Permissions::MODERATE_MEMBERS)
        .option(
            Opt::sub_command("start", "Start a giveaway")
                .option(Opt::string("prize", "What the winners get").required(true).max_length(256))
                .option(Opt::string("duration", "Length or end time: 30m, 2d, 1w, or 2026-12-24 18:00 (server timezone)").required(true).max_length(40))
                .option(Opt::integer("winners", "Number of winners (1-90, default 1)").min_value(1).max_value(winners_max))
                .option(Opt::channel("channel", "Channel to post in (default: this one)").channel_types(&TEXT_TYPES))
                .option(Opt::role("required_role", "Only members with this role can enter"))
                .option(Opt::role("denied_role", "Members with this role cannot enter"))
                .option(Opt::role("bonus_role", "Members with this role get better odds"))
                .option(Opt::integer("bonus_weight", "Entries each bonus_role member counts as (2-10, default 2)").min_value(2).max_value(10))
                .option(Opt::integer("xp", "XP for each winner (needs the Levels module)").min_value(1).max_value(MAX_PRIZE_AMOUNT))
                .option(Opt::integer("coins", "Coins for each winner (needs the Economy module)").min_value(1).max_value(MAX_PRIZE_AMOUNT))
                .option(Opt::role("ping_role", "Role to ping when the giveaway starts"))
                .option(Opt::string("description", "Extra text shown in the giveaway").max_length(1000))
                .option(Opt::string("color", "Embed colour, e.g. #5865F2 or blurple").max_length(16)),
        )
        .option(Opt::sub_command("end", "End a giveaway now and draw its winners").option(gw()))
        .option(
            Opt::sub_command("reroll", "Draw new winners for an ended giveaway (MEE6 /reroll)")
                .option(gw())
                .option(Opt::integer("winners", "How many new winners (default 1)").min_value(1).max_value(winners_max)),
        )
        .option(Opt::sub_command("cancel", "Cancel a giveaway without drawing winners").option(gw()))
        .option(
            Opt::sub_command("restart", "Republish a draft giveaway (its message was deleted)")
                .option(gw())
                .option(Opt::string("duration", "New length or end time (default: keep the old end time)").max_length(40))
                .option(Opt::channel("channel", "Channel to post in (default: the original one)").channel_types(&TEXT_TYPES)),
        )
        .option(
            Opt::sub_command("list", "List this server's giveaways").option(
                Opt::string("status", "Which giveaways to show (default: open)")
                    .choice("Open (active and draft)", "open")
                    .choice("Ended", "ended")
                    .choice("Cancelled", "cancelled")
                    .choice("All", "all"),
            ),
        )
        .option(Opt::sub_command("entrants", "Show who entered a giveaway").option(gw()))
        .option(
            Opt::sub_command_group("odds", "Adjust winning odds by role")
                .option(
                    Opt::sub_command("add", "Give a role better odds in a giveaway")
                        .option(gw())
                        .option(Opt::role("role", "Role that gets better odds").required(true))
                        .option(Opt::integer("weight", "Entries each member of the role counts as (2-10)").required(true).min_value(2).max_value(10)),
                )
                .option(
                    Opt::sub_command("remove", "Remove a role's bonus odds from a giveaway")
                        .option(gw())
                        .option(Opt::role("role", "Role to remove").required(true)),
                ),
        )
}

/// The command (`module 'giveaways'`, gated, MODERATOR, servers only).
pub fn command() -> CommandDef {
    CommandDef::new("community", data(), Some(ModuleKey::Giveaways), execute)
        .permission_level(PermissionLevel::Moderator)
        .autocomplete(autocomplete)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub const GENERIC_ERROR: &str = "Discord or the database did not respond. Please try again in a moment.";
pub const NOT_FOUND_TEXT: &str = "I couldn't find that giveaway. Pick one from the list, or paste its ID, message ID or message link.";
const NOT_OPEN_TEXT: &str = "Odds can only be changed while a giveaway is running or in drafts.";
const RESOLVE_FAILED_TEXT: &str = "I couldn't check every entrant right now (Discord did not answer in time). Please try again in a minute.";

/// `fail(interaction, title, text, extra)`: an error embed as the reply; never fails. `clear` adds `components: []`.
async fn fail_with(ctx: &CommandCtx, title: &str, text: &str, clear: bool) {
    let mut p = Payload::embed(error_embed(title, Some(text), None));
    if clear {
        p = p.components(Vec::new());
    }
    if let Err(e) = ctx.edit_reply(p.to_edit()).await {
        bot_util::log_debug!(format!("giveaway: could not send the error reply: {e}"));
    }
}

async fn fail(ctx: &CommandCtx, title: &str, text: &str) -> BotResult {
    fail_with(ctx, title, text, false).await;
    Ok(())
}

/// `interaction.editReply({ embeds: [embed] })`.
async fn edit(ctx: &CommandCtx, embed: CreateEmbed, clear: bool) -> BotResult {
    let mut p = Payload::embed(embed);
    if clear {
        p = p.components(Vec::new());
    }
    ctx.edit_reply(p.to_edit()).await?;
    Ok(())
}

/// `mentionsOf(ids)`.
pub fn mentions_of(ids: &[String]) -> String {
    join_with_limit(&ids.iter().map(|id| format!("<@{id}>")).collect::<Vec<_>>(), ", ", 3500, default_more)
}

async fn resolve_ref(ctx: &CommandCtx) -> anyhow::Result<Option<Giveaway>> {
    let guild_id = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
    let input = ctx.get_string("giveaway").ok_or_else(|| anyhow::anyhow!("Required option \"giveaway\" not found."))?;
    let g = svc::resolve_giveaway_ref(ctx.app.db(), &guild_id, &input).await?;
    if g.is_none() {
        fail_with(ctx, "Giveaway Not Found", NOT_FOUND_TEXT, false).await;
    }
    Ok(g)
}

fn is_text_type(ch: &GuildChannel) -> bool {
    matches!(ch.kind, ChannelType::Text | ChannelType::News)
}

/// Start steps 1-3, shared with restart. `false` when it already replied.
async fn check_post_channel(ctx: &CommandCtx, channel: Option<&GuildChannel>) -> bool {
    let Some(channel) = channel.filter(|c| is_text_type(c)) else {
        fail_with(
            ctx,
            "Unsupported Channel",
            "Giveaways can be posted in text or announcement channels only. Run this in one, or pick one with `channel`.",
            false,
        )
        .await;
        return false;
    };
    let can = ctx
        .member()
        .is_some_and(|m| guild_view::member_has_in(&ctx.app, channel.guild_id, channel, m, Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES));
    if !can {
        fail_with(ctx, "No Access", &format!("You can't send messages in <#{}>.", channel.id), false).await;
        return false;
    }
    let u = channel_usable(&ctx.app, channel.guild_id, channel, &POST_NEEDS);
    if !u.ok {
        let text = if !u.missing.is_empty() {
            format!("I need **{}** in <#{}> to run a giveaway there.", permission_names(&u.missing).join(", "), channel.id)
        } else {
            u.reason.unwrap_or_default()
        };
        fail_with(ctx, "Missing Permissions", &text, false).await;
        return false;
    }
    true
}

/// `optionChannel(interaction)`: the picked channel from the guild cache, else from Discord (discord.js builds it from
/// the interaction's resolved data, which has no permission overwrites to check; Discord's answer has them). `None`
/// when nothing was picked, or the channel cannot be read (then the checks answer "Unsupported Channel").
async fn option_channel(ctx: &CommandCtx) -> Option<GuildChannel> {
    let picked = ctx.get_channel_id("channel")?;
    let guild_id = ctx.guild_id()?;
    if let Some(c) = cached_channel(&ctx.app, guild_id, picked) {
        return Some(c);
    }
    match guild_view::fetch_guild_channel(&ctx.app, &guild_id.to_string(), &picked.to_string()).await {
        Ok(c) => c,
        Err(e) => {
            bot_util::log_debug!(format!("giveaway: could not fetch the picked channel {picked}: {e}"));
            None
        }
    }
}

/// Parses `duration` in the server timezone and validates it; `None` when it already replied.
async fn parse_ends_at(ctx: &CommandCtx, settings: &Settings, text: &str) -> Option<DateTime<Utc>> {
    let now = svc::now_of(&ctx.app);
    let ends_at = bot_util::time::parse_date_input(text, &settings.timezone, &now);
    match svc::validate_ends_at(ends_at, now, &settings.timezone) {
        Ok(()) => ends_at,
        Err((title, message)) => {
            fail_with(ctx, &title, &message, false).await;
            None
        }
    }
}

async fn fail_start_or_restart(ctx: &CommandCtx, err: &GiveawayError, channel: &GuildChannel) -> BotResult {
    match err {
        GiveawayError::Limit => {
            fail(
                ctx,
                "Giveaway Limit Reached",
                &format!("This server already has {} running or draft giveaways. End or cancel one first.", limits::ACTIVE_GIVEAWAYS),
            )
            .await
        }
        GiveawayError::SendFailed(_) => {
            fail(ctx, "Could Not Post", &format!("I couldn't post the giveaway in <#{}>. Check my permissions there.", channel.id)).await
        }
        _ => fail(ctx, "Something Went Wrong", GENERIC_ERROR).await,
    }
}

// ── Subcommands ───────────────────────────────────────────────────────────────

async fn start(ctx: &CommandCtx, settings: Settings) -> BotResult {
    let Some(guild_id) = ctx.guild_id() else { return Ok(()) };
    let g = guild_id.to_string();

    // 1-3. Channel, executor access, bot permissions.
    // A picked channel that cannot be read is refused below; it never falls back to this channel.
    let channel = if ctx.get_channel_id("channel").is_some() { option_channel(ctx).await } else { cached_channel(&ctx.app, guild_id, ctx.channel_id()) };
    if !check_post_channel(ctx, channel.as_ref()).await {
        return Ok(());
    }
    let Some(channel) = channel else { return Ok(()) };

    // 4. Duration.
    let duration = ctx.get_string("duration").unwrap_or_default();
    let Some(ends_at) = parse_ends_at(ctx, &settings, &duration).await else { return Ok(()) };

    // 5. Winners.
    let winner_count = ctx.get_integer("winners").unwrap_or(1);

    // 6. Required and denied roles. Managed roles are allowed: nothing is assigned.
    let required = ctx.get_role_id("required_role").map(|r| r.to_string());
    if required.as_deref() == Some(g.as_str()) {
        return fail(ctx, "Invalid Role", "Choose a specific role for `required_role`, not @everyone.").await;
    }
    let denied = ctx.get_role_id("denied_role").map(|r| r.to_string());
    if denied.as_deref() == Some(g.as_str()) {
        return fail(ctx, "Invalid Role", "Choose a specific role for `denied_role`, not @everyone.").await;
    }
    if required.is_some() && required == denied {
        return fail(ctx, "Invalid Role", "The same role cannot be both required and denied.").await;
    }

    // 7. Bonus odds.
    let bonus = ctx.get_role_id("bonus_role").map(|r| r.to_string());
    let bonus_weight = ctx.get_integer("bonus_weight");
    if bonus_weight.is_some() && bonus.is_none() {
        return fail(ctx, "Missing Role", "Pick a `bonus_role` to use `bonus_weight`.").await;
    }
    if bonus.as_deref() == Some(g.as_str()) {
        return fail(ctx, "Invalid Role", "Bonus odds for @everyone would change nothing. Choose a specific role.").await;
    }
    let role_weights: Vec<RoleWeight> =
        bonus.iter().map(|r| RoleWeight { role_id: r.clone(), weight: bonus_weight.unwrap_or(DEFAULT_BONUS_WEIGHT) }).collect();

    // 8. XP and coin prizes.
    let xp_prize = ctx.get_integer("xp");
    let coin_prize = ctx.get_integer("coins");
    for (kind, amount) in [(RewardKind::Xp, xp_prize), (RewardKind::Coins, coin_prize)] {
        if amount.is_none() {
            continue;
        }
        if let Some(err) = svc::reward_unavailable_message(&ctx.app, kind, Some(&settings)) {
            return fail(ctx, "Prize Unavailable", &err).await;
        }
    }

    // 9. Ping role (checked for the bot and for the executor).
    let ping = ctx.get_role_id("ping_role").map(|r| r.to_string());
    if let Some(p) = &ping {
        if *p == g {
            return fail(ctx, "Invalid Role", "I never ping @everyone. Choose a role for `ping_role`.").await;
        }
        if !guild_view::pingable_by_bot(&ctx.app, guild_id, &channel, std::slice::from_ref(p)).refused.is_empty() {
            return fail(
                ctx,
                "Cannot Ping Role",
                &format!(
                    "I can't ping <@&{p}> in <#{}>. Make the role mentionable, or give me **Mention @everyone, @here and All Roles** there.",
                    channel.id
                ),
            )
            .await;
        }
        let exec = ctx.member().map(|m| (m.user.id, m.roles.clone()));
        if !guild_view::pingable(&ctx.app, guild_id, &channel, exec, std::slice::from_ref(p)).refused.is_empty() {
            return fail(ctx, "Cannot Ping Role", &format!("You don't have permission to ping <@&{p}>.")).await;
        }
    }

    // 10. Colour.
    let color_str = ctx.get_string("color").filter(|c| !c.is_empty());
    let embed_color = match &color_str {
        Some(c) => match parse_color_str(c) {
            Some(v) => Some(v),
            None => return fail(ctx, "Invalid Colour", "Use a hex colour like `#5865F2` or a name like `blurple`.").await,
        },
        None => None,
    };

    // 11. Text.
    let prize = bot_util::js::trim(&ctx.get_string("prize").unwrap_or_default()).to_string();
    if prize.is_empty() {
        return fail(ctx, "Invalid Prize", "The prize cannot be empty.").await;
    }
    let description = ctx.get_string("description").map(|d| bot_util::js::trim(&d).to_string()).filter(|d| !d.is_empty());

    // 12-13. Start and reply.
    let res = svc::start_giveaway(
        &ctx.app,
        StartGiveaway {
            guild_id: g,
            channel: channel.clone(),
            host_id: ctx.user().id.to_string(),
            prize: prize.clone(),
            description,
            winner_count,
            ends_at,
            required_role_ids: required.into_iter().collect(),
            denied_role_ids: denied.into_iter().collect(),
            role_weights,
            xp_prize,
            coin_prize,
            ping_role_id: ping,
            embed_color,
            settings: Some(settings),
        },
    )
    .await;
    match res {
        Err(e) => fail_start_or_restart(ctx, &e, &channel).await,
        Ok(started) => {
            edit(
                ctx,
                success_embed(
                    "Giveaway Started",
                    Some(&format!(
                        "**{}** is live in <#{}> and ends {}.\n[Jump to the giveaway]({}) · ID `{}`",
                        clamp_text(&prize, 200),
                        channel.id,
                        discord_timestamp(&ends_at, "R"),
                        started.message.url(),
                        started.giveaway.id
                    )),
                    None,
                ),
                false,
            )
            .await
        }
    }
}

async fn end(ctx: &CommandCtx) -> BotResult {
    let Some(g) = resolve_ref(ctx).await? else { return Ok(()) };
    if g.status != GiveawayStatus::Active {
        if g.status == GiveawayStatus::Draft {
            return fail(ctx, "Not Running", "That giveaway is a draft (its message was deleted). Use `/giveaway restart` to republish it first.").await;
        }
        return fail(ctx, "Not Running", &format!("That giveaway is {}.", status_text(g.status))).await;
    }
    let res = svc::end_giveaway(&ctx.app, &g.guild_id, &g.id, EndSource::Command, 1).await?;
    let prize = clamp_text(&g.prize, 200);
    match res {
        Ok(r) => {
            let mut text = if r.winner_ids.is_empty() {
                format!("**{prize}** ended with no valid entrants.")
            } else {
                format!("Winners of **{prize}**: {}", mentions_of(&r.winner_ids))
            };
            if !r.pay_warnings.is_empty() {
                text.push_str(&format!("\n\n{}", r.pay_warnings.join("\n")));
            }
            if !r.announced {
                text.push_str("\n\nI could not post the announcement in the giveaway channel.");
            }
            edit(ctx, success_embed("Giveaway Ended", Some(&text), None), false).await
        }
        Err(GiveawayError::NotActive | GiveawayError::NotFound) => {
            edit(ctx, warn_embed("Already Ended", Some("That giveaway was already ended."), None), false).await
        }
        Err(GiveawayError::Drafted) => {
            edit(
                ctx,
                warn_embed(
                    "Message Missing",
                    Some("The giveaway message or its channel was deleted, so the giveaway was moved to drafts. Use `/giveaway restart` to republish it."),
                    None,
                ),
                false,
            )
            .await
        }
        Err(GiveawayError::NoAccess { missing, reason }) => {
            let text = if !missing.is_empty() {
                format!("I need **{}** in <#{}> to end this giveaway.", permission_names(&missing).join(", "), g.channel_id)
            } else {
                reason.unwrap_or_else(|| format!("I can't use <#{}> to end this giveaway.", g.channel_id))
            };
            fail(ctx, "Missing Permissions", &text).await
        }
        Err(GiveawayError::ResolveFailed) => fail(ctx, "Try Again", RESOLVE_FAILED_TEXT).await,
        Err(_) => fail(ctx, "Something Went Wrong", GENERIC_ERROR).await,
    }
}

async fn reroll(ctx: &CommandCtx) -> BotResult {
    let Some(g) = resolve_ref(ctx).await? else { return Ok(()) };
    if g.status != GiveawayStatus::Ended {
        if g.status == GiveawayStatus::Active {
            return fail(ctx, "Still Running", "That giveaway is still running. Use `/giveaway end` to end it now.").await;
        }
        return fail(ctx, "Cannot Reroll", "Only ended giveaways can be rerolled.").await;
    }
    let count = ctx.get_integer("winners").unwrap_or(1);
    match svc::reroll_giveaway(&ctx.app, &g.guild_id, &g.id, count).await? {
        Ok(r) => {
            let mut text = format!(
                "New winner{} of **{}**: {}",
                if r.new_winner_ids.len() == 1 { "" } else { "s" },
                clamp_text(&g.prize, 200),
                mentions_of(&r.new_winner_ids)
            );
            if g.xp_prize.unwrap_or(0) != 0 || g.coin_prize.unwrap_or(0) != 0 {
                text.push_str("\nThey receive the prize too; earlier winners keep theirs.");
            }
            if !r.pay_warnings.is_empty() {
                text.push_str(&format!("\n\n{}", r.pay_warnings.join("\n")));
            }
            if !r.announced {
                text.push_str("\n\nI could not post the announcement in the giveaway channel.");
            }
            edit(ctx, success_embed("Giveaway Rerolled", Some(&text), None), false).await
        }
        Err(GiveawayError::NoEntrants) => {
            edit(ctx, warn_embed("No One Left", Some("There are no other eligible entrants to draw from."), None), false).await
        }
        Err(GiveawayError::NotEnded | GiveawayError::NotFound) => fail(ctx, "Cannot Reroll", "Only ended giveaways can be rerolled.").await,
        Err(GiveawayError::Conflict) => fail(ctx, "Try Again", "That giveaway changed while I was drawing. Please try again.").await,
        Err(GiveawayError::ResolveFailed) => fail(ctx, "Try Again", RESOLVE_FAILED_TEXT).await,
        Err(_) => fail(ctx, "Something Went Wrong", GENERIC_ERROR).await,
    }
}

async fn cancel(ctx: &CommandCtx) -> BotResult {
    let Some(g) = resolve_ref(ctx).await? else { return Ok(()) };
    if !matches!(g.status, GiveawayStatus::Active | GiveawayStatus::Draft) {
        return fail(ctx, "Cannot Cancel", &format!("That giveaway is {}.", status_text(g.status))).await;
    }
    let prize = clamp_text(&g.prize, 200);
    let ok = await_confirm(
        ctx,
        ConfirmOptions {
            title: Some("Cancel this giveaway?".into()),
            description: Some(format!("**{prize}** will be cancelled and no winners will be drawn. This cannot be undone.")),
            confirm_label: "Cancel giveaway".into(),
            cancel_label: "Keep it".into(),
            danger: true,
            ephemeral: true,
            ..Default::default()
        },
    )
    .await;
    if !ok {
        return edit(ctx, info_embed("Kept", Some("The giveaway was not cancelled."), None), true).await;
    }
    match svc::cancel_giveaway(&ctx.app, &g.guild_id, &g.id).await? {
        Ok(()) => edit(ctx, success_embed("Giveaway Cancelled", Some(&format!("**{prize}** was cancelled.")), None), true).await,
        Err(_) => {
            fail_with(ctx, "Cannot Cancel", "That giveaway already ended or was cancelled.", true).await;
            Ok(())
        }
    }
}

async fn restart(ctx: &CommandCtx, settings: Settings) -> BotResult {
    let Some(g) = resolve_ref(ctx).await? else { return Ok(()) };
    if g.status != GiveawayStatus::Draft {
        return fail(ctx, "Not a Draft", "Only draft giveaways can be restarted. A giveaway becomes a draft when its message is deleted.").await;
    }
    let channel = if ctx.get_channel_id("channel").is_some() {
        let picked = option_channel(ctx).await;
        if picked.is_none() {
            check_post_channel(ctx, None).await; // "Unsupported Channel"
            return Ok(());
        }
        picked
    } else {
        svc::fetch_channel(&ctx.app, &g.guild_id, &g.channel_id).await?
    };
    let Some(channel) = channel else {
        return fail(ctx, "Channel Missing", "The original channel no longer exists. Pick one with `channel`.").await;
    };
    if !check_post_channel(ctx, Some(&channel)).await {
        return Ok(());
    }
    let ends_at = match ctx.get_string("duration").filter(|d| !d.is_empty()) {
        Some(text) => match parse_ends_at(ctx, &settings, &text).await {
            Some(e) => e,
            None => return Ok(()),
        },
        None if g.ends_at.timestamp_millis() - ctx.app.clock().now_ms() >= MIN_DURATION_MS => g.ends_at,
        None => return fail(ctx, "Duration Needed", "This giveaway's end time has passed. Give a new `duration`.").await,
    };
    match svc::restart_giveaway(&ctx.app, &g.guild_id, &g.id, channel.clone(), ends_at, Some(settings)).await {
        Ok(message) => {
            edit(
                ctx,
                success_embed(
                    "Giveaway Restarted",
                    Some(&format!(
                        "**{}** is live again in <#{}> and ends {}. Existing entries were kept.\n[Jump to the giveaway]({})",
                        clamp_text(&g.prize, 200),
                        channel.id,
                        discord_timestamp(&ends_at, "R"),
                        message.url()
                    )),
                    None,
                ),
                false,
            )
            .await
        }
        Err(GiveawayError::NotDraft | GiveawayError::Conflict) => {
            fail(ctx, "Try Again", "That giveaway changed meanwhile. Check `/giveaway list` and try again.").await
        }
        Err(e) => fail_start_or_restart(ctx, &e, &channel).await,
    }
}

/// The pages of `/giveaway list` (10 lines a page).
pub fn list_pages(rows: &[svc::ListRow], settings: Option<&bot_db::GuildSettings>) -> Vec<CreateEmbed> {
    let lines: Vec<String> = rows.iter().map(svc::format_list_line).collect();
    lines
        .chunks(10)
        .enumerate()
        .map(|(i, chunk)| {
            let mut text = chunk.join("\n\n");
            if i == 0 && rows.len() as i64 == LIST_SHOWN {
                text = format!("_Showing the {LIST_SHOWN} most recent._\n\n{text}");
            }
            guild_embed(settings, Some("🎉 Giveaways"), Some(&text), None)
        })
        .collect()
}

async fn list(ctx: &CommandCtx, settings: Settings) -> BotResult {
    let filter = ListFilter::parse(ctx.get_string("status").as_deref());
    let guild_id = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
    let rows = svc::list_giveaways(ctx.app.db(), &guild_id, filter).await?;
    if rows.is_empty() {
        let text = if filter == ListFilter::All {
            "There are no giveaways yet. Start one with `/giveaway start`.".to_string()
        } else {
            format!(
                "There are no {}giveaways. Use `/giveaway list status:all` to see every status, or start one with `/giveaway start`.",
                filter.name()
            )
        };
        return edit(ctx, info_embed("No Giveaways", Some(&text), None), false).await;
    }
    paginate(ctx, &list_pages(&rows, Some(&settings)), PaginateOptions { ephemeral: true, ..Default::default() }).await?;
    Ok(())
}

/// The pages of `/giveaway entrants` (20 lines a page).
pub fn entrant_pages(g: &Giveaway, total: i64, rows: &[(String, DateTime<Utc>)], settings: Option<&bot_db::GuildSettings>) -> Vec<CreateEmbed> {
    let prize = clamp_text(&g.prize, 200);
    let weights = normalize_role_weights(g.role_weights.as_ref().unwrap_or(&serde_json::Value::Null));
    let mut header = format!("**Status:** {}", status_text_short(g.status));
    if !weights.is_empty() {
        header.push_str(&format!(
            " · **Bonus odds:** {}",
            weights.iter().map(|w| format!("<@&{}> ×{}", w.role_id, w.weight)).collect::<Vec<_>>().join(", ")
        ));
    }
    let footer = format!(
        "{} entrant{}{}",
        format_number(total as f64),
        if total == 1 { "" } else { "s" },
        if total > ENTRANTS_SHOWN { " · showing the first 1,000" } else { "" }
    );
    let lines: Vec<String> = rows
        .iter()
        .enumerate()
        .map(|(i, (uid, at))| {
            format!("{}. <@{uid}> · {}{}", i + 1, discord_timestamp(at, "R"), if g.winner_ids.contains(uid) { " 🏆" } else { "" })
        })
        .collect();
    lines
        .chunks(20)
        .map(|c| guild_embed(settings, Some(&format!("🎉 Entrants: {prize}")), Some(&format!("{header}\n\n{}", c.join("\n"))), Some(&footer)))
        .collect()
}

async fn entrants(ctx: &CommandCtx, settings: Settings) -> BotResult {
    let Some(g) = resolve_ref(ctx).await? else { return Ok(()) };
    let total = svc::entry_count(ctx.app.db(), &g.id).await?;
    let rows = svc::list_entrants(ctx.app.db(), &g.id).await?;
    if rows.is_empty() {
        return edit(ctx, info_embed("No Entrants", Some(&format!("Nobody has entered **{}** yet.", clamp_text(&g.prize, 200))), None), false).await;
    }
    paginate(ctx, &entrant_pages(&g, total, &rows, Some(&settings)), PaginateOptions { ephemeral: true, ..Default::default() }).await?;
    Ok(())
}

async fn odds(ctx: &CommandCtx, settings: Settings, add: bool) -> BotResult {
    let Some(g) = resolve_ref(ctx).await? else { return Ok(()) };
    if !matches!(g.status, GiveawayStatus::Active | GiveawayStatus::Draft) {
        return fail(ctx, "Not Open", NOT_OPEN_TEXT).await;
    }
    let role = ctx.get_role_id("role").map(|r| r.to_string()).ok_or_else(|| anyhow::anyhow!("Required option \"role\" not found."))?;
    if role == g.guild_id {
        return fail(ctx, "Invalid Role", "Choose a specific role, not @everyone.").await;
    }
    let op = if add {
        OddsOp::Add { weight: ctx.get_integer("weight").ok_or_else(|| anyhow::anyhow!("Required option \"weight\" not found."))? }
    } else {
        OddsOp::Remove
    };
    let prize = clamp_text(&g.prize, 200);
    match svc::update_odds(&ctx.app, &g.guild_id, &g.id, op, &role, Some(settings)).await? {
        Ok(list) => {
            let text = match op {
                OddsOp::Add { weight } => format!(
                    "Members with <@&{role}> now count as **{weight}** entries in **{prize}**.\n\n**Bonus odds:** {}",
                    list.iter().map(|w| format!("<@&{}> ×{}", w.role_id, w.weight)).collect::<Vec<_>>().join(", ")
                ),
                OddsOp::Remove => format!("<@&{role}> no longer has bonus odds in **{prize}**."),
            };
            edit(ctx, success_embed("Odds Updated", Some(&text), None), false).await
        }
        Err(GiveawayError::Limit) => {
            fail(ctx, "Too Many Roles", &format!("A giveaway can have at most {MAX_ROLE_WEIGHTS} bonus-odds roles.")).await
        }
        Err(GiveawayError::NotSet) => fail(ctx, "Not Set", &format!("<@&{role}> has no bonus odds in this giveaway.")).await,
        Err(GiveawayError::NotOpen) => fail(ctx, "Not Open", NOT_OPEN_TEXT).await,
        Err(_) => fail(ctx, "Giveaway Not Found", NOT_FOUND_TEXT).await,
    }
}

// ── Execute ───────────────────────────────────────────────────────────────────

async fn execute(ctx: CommandCtx) -> BotResult {
    // Every reply is ephemeral; acknowledge before anything else.
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let group = ctx.subcommand_group();
    let sub = ctx.subcommand().unwrap_or_default();
    let key = match &group {
        Some(g) => format!("{g} {sub}"),
        None => sub.clone(),
    };
    const KNOWN: [&str; 9] = ["start", "end", "reroll", "cancel", "restart", "list", "entrants", "odds add", "odds remove"];
    if !KNOWN.contains(&key.as_str()) {
        fail_with(&ctx, "Unknown Subcommand", "This subcommand no longer exists.", false).await;
        return Ok(());
    }
    let run: BotResult = async {
        let guild_id = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
        let settings = ctx.app.settings().get_settings(&guild_id).await?;
        match key.as_str() {
            "start" => start(&ctx, settings).await,
            "end" => end(&ctx).await,
            "reroll" => reroll(&ctx).await,
            "cancel" => cancel(&ctx).await,
            "restart" => restart(&ctx, settings).await,
            "list" => list(&ctx, settings).await,
            "entrants" => entrants(&ctx, settings).await,
            "odds add" => odds(&ctx, settings, true).await,
            _ => odds(&ctx, settings, false).await,
        }
    }
    .await;
    if let Err(e) = run {
        bot_util::log_error!(format!("giveaway {key} failed"), bot_core::error::log_arg(&e));
        fail_with(&ctx, "Something Went Wrong", GENERIC_ERROR, true).await;
    }
    Ok(())
}

// ── Autocomplete ──────────────────────────────────────────────────────────────

/// `AUTOCOMPLETE_STATUSES`.
pub fn autocomplete_statuses(key: &str) -> Option<&'static [&'static str]> {
    Some(match key {
        "end" => &["ACTIVE"],
        "reroll" => &["ENDED"],
        "cancel" => &["ACTIVE", "DRAFT"],
        "restart" => &["DRAFT"],
        "entrants" => &["ACTIVE", "DRAFT", "ENDED", "CANCELLED"],
        "odds add" | "odds remove" => &["ACTIVE", "DRAFT"],
        _ => return None,
    })
}

/// The name of an autocomplete choice.
pub fn choice_name(row: &svc::AutocompleteRow, channel_name: Option<&str>, now_ms: i64) -> String {
    let tail = match row.status {
        GiveawayStatus::Active => {
            format!("ends in {}", bot_util::parse_duration::format_duration((row.ends_at.timestamp_millis() - now_ms).max(1000) as f64))
        }
        GiveawayStatus::Draft => "draft".into(),
        GiveawayStatus::Ended => "ended".into(),
        GiveawayStatus::Cancelled => "cancelled".into(),
    };
    clamp_text(&format!("{} {} · #{} · {tail}", status_emoji(row.status), row.prize, channel_name.unwrap_or("deleted-channel")), 100)
}

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let (name, value) = ctx.focused().unwrap_or_default();
    if name != "giveaway" {
        respond_autocomplete(&ctx.responder, &[]).await;
        return Ok(());
    }
    let key = match (ctx.subcommand_group(), ctx.subcommand()) {
        (Some(g), Some(s)) => format!("{g} {s}"),
        (None, Some(s)) => s,
        _ => String::new(),
    };
    let (Some(statuses), Some(guild_id)) = (autocomplete_statuses(&key), ctx.guild_id()) else {
        respond_autocomplete(&ctx.responder, &[]).await;
        return Ok(());
    };
    let budget = bot_core::interaction::ack_budget_ms(ctx.interaction.id);
    if budget == 0 {
        respond_autocomplete(&ctx.responder, &[]).await;
        return Ok(());
    }
    let q = bot_util::js::utf16_prefix(bot_util::js::trim(&value), 100).to_string();
    let (db, g) = (ctx.app.db().clone(), guild_id.to_string());
    let rows = bot_util::async_util::with_timeout(
        async move { svc::find_giveaways_for_autocomplete(&db, &g, statuses, &q).await.map_err(|e| e.to_string()) },
        budget,
        "giveaway autocomplete",
    )
    .await;
    let rows = match rows {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_debug!(format!("giveaway autocomplete failed: {e}"));
            respond_autocomplete(&ctx.responder, &[]).await;
            return Ok(());
        }
    };
    let now = ctx.app.clock().now_ms();
    let guild = bot_core::interaction_helpers::cached_guild(&ctx.app, guild_id);
    let choices: Vec<Choice> = rows
        .iter()
        .map(|r| {
            let channel_name = guild_view::channel_id_of(&r.channel_id)
                .and_then(|cid| guild.as_ref().and_then(|g| bot_core::permissions::find_channel(g, cid)).map(|c| c.name.clone()));
            Choice::new(choice_name(r, channel_name.as_deref(), now), r.id.clone())
        })
        .collect();
    respond_autocomplete(&ctx.responder, &choices).await;
    Ok(())
}
