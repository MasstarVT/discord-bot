//! Port of `src/commands/community/boost.js`: boosters make a role of their own (`role create | edit | delete`);
//! admins run `config`, `test`, `list` and `remove` (`subcommandPermissions`). The command is not gated, so admins can
//! set it up before turning the module on; every member subcommand checks the module itself. The rules live in
//! [`crate::boost_service`]; the hooks are [`crate::plugins::boosts`].

use bot_config::limits;
use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::interaction_helpers::{PermTarget, Payload, cached_guild, check_channel_usable, missing_bot_permissions, safe_defer, safe_reply};
use bot_core::permissions::{highest_position, permission_names, role_position};
use bot_core::prelude::*;
use bot_db::GuildSettings;
use bot_util::text::join_with_limit;
use serenity::all::{ChannelId, Guild, GuildId, RoleId};
use serenity::builder::{CreateEmbed, CreateEmbedFooter};

use crate::boost_service::{
    self as svc, IconAttachment, LookInput, Problem, ThankYou, ThankYouSource, consts as BOOST, ensure_sweep_job, grace_ms, is_boosting, list_rows,
    member_problem, nitro_booster_role, placement_for, post_thank_you, problem_embed, render_thank_you,
};

fn looks(s: CommandOption) -> CommandOption {
    s.option(Opt::string("color", "Hex colour, like #ff8800").max_length(7))
        .option(Opt::string("icon_emoji", "One standard emoji as the role icon").max_length(BOOST::EMOJI_MAX as u16))
        .option(Opt::attachment("icon_image", "PNG or JPEG role icon"))
}

/// The command's data.
pub fn data() -> SlashCommand {
    let name_max = limits::BOOSTER_ROLE_NAME as u16;
    SlashCommand::new("boost", "Boosters can make a role of their own. Admins set it up.")
        .option(
            Opt::sub_command_group("role", "Your own booster role.")
                .option(looks(
                    Opt::sub_command("create", "Make your booster role.").option(Opt::string("name", "Role name").required(true).max_length(name_max)),
                ))
                .option(looks(Opt::sub_command("edit", "Change your booster role.").option(Opt::string("name", "New role name").max_length(name_max))))
                .option(Opt::sub_command("delete", "Delete your booster role.")),
        )
        .option(
            Opt::sub_command("config", "Show or change the boost settings.")
                .option(Opt::channel("channel", "Where the thank-you is posted").channel_types(&[channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT]))
                .option(Opt::boolean("custom_roles", "Let boosters make their own role"))
                .option(Opt::role("anchor", "Booster roles go just below this role (unset: just above the Nitro Booster role)"))
                .option(
                    Opt::integer("grace_days", "Days a role stays after the boost ends (0 = removed at once)").min_value(0).max_value(BOOST::GRACE_MAX_DAYS as f64),
                ),
        )
        .option(Opt::sub_command("test", "Post a preview of the thank-you."))
        .option(Opt::sub_command("list", "List the booster roles."))
        .option(Opt::sub_command("remove", "Delete a member's booster role.").option(Opt::user("user", "The member").required(true)))
}

/// The command (`module 'boosts'`, `gated = false`, MEMBER with admin subcommands).
pub fn command() -> CommandDef {
    CommandDef::new("community", data(), Some(ModuleKey::Boosts), execute)
        .gated(false)
        .permission_level(PermissionLevel::Member)
        .subcommand_permission("config", PermissionLevel::Admin)
        .subcommand_permission("test", PermissionLevel::Admin)
        .subcommand_permission("list", PermissionLevel::Admin)
        .subcommand_permission("remove", PermissionLevel::Admin)
}

fn yes_no(v: bool) -> &'static str {
    if v { "✅ On" } else { "❌ Off" }
}

async fn finish(ctx: &CommandCtx, embed: CreateEmbed) -> BotResult {
    safe_reply(&ctx.responder, Payload::embed(embed).no_mentions()).await;
    Ok(())
}

fn guild_of(ctx: &CommandCtx) -> anyhow::Result<GuildId> {
    ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/boost outside a guild"))
}

async fn execute(ctx: CommandCtx) -> BotResult {
    let group = ctx.subcommand_group();
    let sub = ctx.subcommand().unwrap_or_default();
    if group.as_deref() == Some("role") {
        return role_command(ctx, &sub).await;
    }
    match sub.as_str() {
        "config" => config(ctx).await,
        "test" => preview(ctx).await,
        "list" => list(ctx).await,
        "remove" => remove(ctx).await,
        _ => Ok(()),
    }
}

// ── role create | edit | delete (members) ─────────────────────────────────────

async fn role_command(ctx: CommandCtx, sub: &str) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let guild_id = guild_of(&ctx)?;
    let settings = ctx.app.settings().get_settings_safe(&guild_id.to_string()).await;
    let member = ctx.member().cloned().ok_or_else(|| anyhow::anyhow!("/boost role without a member"))?;
    // Deleting is allowed after the boost ended (the role is still there during the grace period).
    if let Some(blocked) = member_problem(settings.as_deref(), is_boosting(Some(&member)), sub != "delete") {
        return finish(&ctx, problem_embed(&blocked)).await;
    }
    if sub == "delete" {
        let embed = match svc::delete_own_role(&ctx.app, guild_id, member.user.id).await {
            Ok(()) => success_embed("Booster Role Deleted", Some("Your booster role was deleted."), None),
            Err(p) => problem_embed(&p),
        };
        return finish(&ctx, embed).await;
    }
    let input = LookInput {
        name: ctx.get_string("name"),
        color: ctx.get_string("color"),
        icon_emoji: ctx.get_string("icon_emoji"),
        icon_image: ctx.get_attachment("icon_image").as_ref().map(IconAttachment::of),
    };
    if sub == "create" {
        let res = svc::create_booster_role(&ctx.app, guild_id, &member, settings.as_deref().cloned(), input).await;
        return finish(&ctx, create_answer(&res)).await;
    }
    let embed = match svc::edit_booster_role(&ctx.app, guild_id, &member, input).await {
        Ok(e) => success_embed("Booster Role Updated", Some(&format!("Changed the {} of <@&{}>.", e.changed.join(" and "), e.role_id)), None),
        Err(p) => problem_embed(&p),
    };
    finish(&ctx, embed).await
}

/// The answer to `/boost role create`.
pub fn create_answer(res: &Result<svc::Created, Problem>) -> CreateEmbed {
    match res {
        Err(p) => problem_embed(p),
        Ok(c) => {
            let mut lines = vec![format!("Your role <@&{}> is ready and you have it now. Change it any time with `/boost role edit`.", c.role_id)];
            if !c.moved {
                lines.push("⚠️ I could not move it to its spot in the role list, so another role may hide its colour. Ask an admin to check my role position.".into());
            }
            success_embed("Booster Role Created", Some(&lines.join("\n")), None)
        }
    }
}

// ── config (admin) ────────────────────────────────────────────────────────────

/// `configEmbed(guild, settings, title, rowCount)`.
pub fn config_embed(app: &App, guild: Option<&Guild>, guild_id: GuildId, settings: &GuildSettings, title: &str, row_count: Option<usize>) -> CreateEmbed {
    let channel_id = settings.boost_channel_id.clone().filter(|c| !c.is_empty());
    let anchor_id = settings.boost_role_anchor_id.clone().filter(|c| !c.is_empty());
    let grace = ((grace_ms(Some(settings)) as f64) / BOOST::DAY_MS as f64).round() as i64;
    let nitro = guild.and_then(nitro_booster_role).map(|r| r.id);
    let placed = match (&anchor_id, nitro) {
        (Some(a), _) => format!("just below <@&{a}>"),
        (None, Some(n)) => format!("just above <@&{n}> (the Nitro Booster role)"),
        _ => "at the bottom of the role list".to_string(),
    };
    let module = if is_module_enabled(Some(settings), Some(ModuleKey::Boosts)) { "✅ On".to_string() } else { "❌ Off, turn it on with `/module enable boosts`".to_string() };
    let lines = [
        format!("Module: {module}"),
        format!("Thank-you channel: {}", channel_id.as_ref().map(|c| format!("<#{c}>")).unwrap_or_else(|| "Not set (no thank-yous)".into())),
        format!("Booster roles: {}{}", yes_no(settings.boost_custom_roles), row_count.map(|n| format!(" ({n} made)")).unwrap_or_default()),
        format!("Booster roles go: {placed}"),
        format!("Kept after the boost ends: {}", if grace > 0 { format!("{grace} day{}", if grace == 1 { "" } else { "s" }) } else { "removed at once".into() }),
    ];
    let mut embed = guild_embed(Some(settings), Some(title), Some(&lines.join("\n")), None);
    let mut warnings: Vec<String> = Vec::new();
    if let Some(c) = &channel_id {
        let usable = check_channel_usable(app, guild_id, c.parse::<u64>().ok().filter(|v| *v != 0).map(ChannelId::new), &["ViewChannel", "SendMessages"]);
        if !usable.ok {
            warnings.push(format!("⚠️ {}", usable.reason.unwrap_or_else(|| "I cannot post in the thank-you channel.".into())));
        }
    }
    if settings.boost_custom_roles {
        let spot = match (guild, app.discord().map(|d| d.bot_user_id())) {
            (Some(g), Some(bot)) => placement_for(g, bot, Some(settings)).err(),
            _ => Some("BOT_PERMS"),
        };
        match spot {
            Some("BOT_PERMS") => {
                let names = permission_names(&missing_bot_permissions(app, PermTarget::Guild(guild_id), &["ManageRoles"])).join(", ");
                warnings.push(format!("⚠️ I need **{}** to make booster roles.", if names.is_empty() { "Manage Roles".to_string() } else { names }));
            }
            Some("BOT_LOW") => warnings.push("⚠️ My highest role is too low to put a role below it. Move my role up.".into()),
            Some("ANCHOR_HIGH") => warnings.push("⚠️ The role booster roles go below is above my highest role. Pick a lower one.".into()),
            _ => {}
        }
    }
    if !warnings.is_empty() {
        embed = embed.field("Problems", bot_util::js::utf16_prefix(&warnings.join("\n"), 1024).to_string(), false);
    }
    let icons = guild.is_some_and(|g| g.features.iter().any(|f| f == "ROLE_ICONS"));
    embed.footer(CreateEmbedFooter::new(if icons { "Role icons are unlocked." } else { "Role icons need Boost level 2. The message text is set on the dashboard." }))
}

async fn config(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = guild_of(&ctx)?;
    let g = guild_id.to_string();
    let Some(settings) = ctx.app.settings().get_settings_safe(&g).await else {
        return finish(&ctx, problem_embed(&Problem::new("SETTINGS"))).await;
    };
    let guild = cached_guild(&ctx.app, guild_id);
    let mut update = SettingsPatch::new();
    if let Some(picked) = ctx.get_channel_id("channel") {
        let usable = check_channel_usable(&ctx.app, guild_id, Some(picked), &["ViewChannel", "SendMessages"]);
        if !usable.ok {
            let text = format!("{} Pick another channel, or fix my permissions there.", usable.reason.unwrap_or_default());
            return finish(&ctx, error_embed("Cannot Use That Channel", Some(&text), None)).await;
        }
        update.insert("boostChannelId", picked.to_string());
    }
    if let Some(custom) = ctx.get_boolean("custom_roles") {
        update.insert("boostCustomRoles", custom);
    }
    if let Some(anchor) = ctx.get_role_id("anchor") {
        if anchor.get() == guild_id.get() {
            return finish(&ctx, error_embed("Invalid Role", Some("@everyone cannot be used here. Pick another role."), None)).await;
        }
        if let (Some(guild), Some(bot)) = (guild.as_ref(), ctx.app.discord().map(|d| d.bot_user_id()))
            && let Some(me) = bot_core::permissions::me(guild, bot)
            && role_position(guild, anchor).unwrap_or(0) > highest_position(guild, &me.roles)
        {
            return finish(
                &ctx,
                error_embed("Role Too High", Some("That role is above my highest role, so I could not put roles below it. Pick a lower role, or move my role up."), None),
            )
            .await;
        }
        update.insert("boostRoleAnchorId", anchor.to_string());
    }
    if let Some(days) = ctx.get_integer("grace_days") {
        update.insert("boostRoleGraceDays", days.clamp(0, BOOST::GRACE_MAX_DAYS));
    }

    let rows = match list_rows(ctx.app.db(), &g).await {
        Ok(r) => Some(r),
        Err(e) => {
            bot_util::log_warn!(format!("[boosts] {g}: could not count the booster roles: {e}"));
            None
        }
    };
    if rows.as_ref().is_some_and(|r| !r.is_empty()) {
        ensure_sweep_job(&ctx.app, &g, None).await;
    }
    let count = rows.as_ref().map(Vec::len);
    if update.is_empty() {
        return finish(&ctx, config_embed(&ctx.app, guild.as_ref(), guild_id, &settings, "💎 Server Boosts", count)).await;
    }
    let keys: Vec<String> = update.iter().map(|(k, _)| k.clone()).collect();
    let saved = match ctx.app.settings().update_settings(&g, update).await {
        Ok(s) => s,
        Err(e) => {
            bot_util::log_error!(format!("[boosts] {g}: config failed"), bot_core::error::log_arg(&anyhow::Error::from(e)));
            return finish(&ctx, error_embed("Could Not Save", Some("The change could not be saved. Please try again in a moment."), None)).await;
        }
    };
    bot_util::log_info!(format!("[boosts] {g}: config changed {} by {}", keys.join(", "), ctx.user().id));
    finish(&ctx, config_embed(&ctx.app, guild.as_ref(), guild_id, &saved, "✅ Server Boosts Updated", count)).await
}

// ── test | list | remove (admin) ──────────────────────────────────────────────

/// The note added while the module is off.
pub const PREVIEW_OFF: &str = "\n\n⚠️ The **Server Boosts** module is off, so real thank-yous are not posted. Turn it on with `/module enable boosts`.";

async fn preview(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = guild_of(&ctx)?;
    let Some(settings) = ctx.app.settings().get_settings_safe(&guild_id.to_string()).await else {
        return finish(&ctx, problem_embed(&Problem::new("SETTINGS"))).await;
    };
    let off = if is_module_enabled(Some(&settings), Some(ModuleKey::Boosts)) { "" } else { PREVIEW_OFF };
    let member = ctx.member().cloned().ok_or_else(|| anyhow::anyhow!("/boost test without a member"))?;
    let embed = match post_thank_you(&ctx.app, guild_id, &member, Some(&settings), true).await {
        ThankYou::Posted { channel_id } => success_embed("Preview Posted", Some(&format!("Posted in <#{channel_id}>.{off}")), None),
        ThankYou::NoChannel => {
            let guild = cached_guild(&ctx.app, guild_id);
            let text = render_thank_you(Some(&settings), &ThankYouSource::of(guild.as_ref(), Some(&member)), 1500, true);
            info_embed("No Thank-you Channel", Some(&format!("Set one with `/boost config channel:`. This is what members will see:\n\n{text}{off}")), None)
        }
        ThankYou::Unusable(t) | ThankYou::Failed(t) => {
            let t = if t.is_empty() { "The preview could not be posted.".to_string() } else { t };
            error_embed("Could Not Post", Some(&format!("{t}{off}")), None)
        }
    };
    finish(&ctx, embed).await
}

/// The `/boost list` lines.
pub fn list_lines(rows: &[bot_db::BoosterRole], guild: Option<&Guild>) -> Vec<String> {
    rows.iter()
        .map(|r| {
            let exists = guild.is_some_and(|g| r.role_id.parse::<u64>().ok().filter(|v| *v != 0).is_some_and(|id| g.roles.contains_key(&RoleId::new(id))));
            let role = if exists { format!("<@&{}>", r.role_id) } else { format!("deleted role `{}`", r.role_id) };
            let state = match r.boost_ended_at {
                Some(d) => format!("boost ended <t:{}:R>", d.timestamp_millis().div_euclid(1000)),
                None => "boosting".into(),
            };
            format!("{role} · <@{}> · {state}", r.user_id)
        })
        .collect()
}

async fn list(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = guild_of(&ctx)?;
    let g = guild_id.to_string();
    let Some(settings) = ctx.app.settings().get_settings_safe(&g).await else {
        return finish(&ctx, problem_embed(&Problem::new("SETTINGS"))).await;
    };
    let rows = match list_rows(ctx.app.db(), &g).await {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_warn!(format!("[boosts] {g}: could not list the booster roles: {e}"));
            return finish(&ctx, error_embed("Lookup Failed", Some("The booster roles could not be read. Please try again in a moment."), None)).await;
        }
    };
    if rows.is_empty() {
        return finish(&ctx, info_embed("No Booster Roles", Some("Nobody has made a booster role yet."), None)).await;
    }
    ensure_sweep_job(&ctx.app, &g, None).await;
    let guild = cached_guild(&ctx.app, guild_id);
    let lines = list_lines(&rows, guild.as_ref());
    let text = join_with_limit(&lines, "\n", 4000, bot_util::text::default_more);
    finish(&ctx, guild_embed(Some(&settings), Some(&format!("💎 Booster Roles ({})", rows.len())), Some(&text), None)).await
}

async fn remove(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = guild_of(&ctx)?;
    let user = ctx.get_user_id("user").ok_or_else(|| anyhow::anyhow!("Required option \"user\" not found."))?.to_string();
    let actor = ctx.user().id.to_string();
    let embed = match svc::remove_member_role(&ctx.app, guild_id, &user, &actor).await {
        Ok(_) => {
            bot_util::log_info!(format!("[boosts] {guild_id}: booster role of {user} removed by {actor}"));
            success_embed("Booster Role Removed", Some(&format!("Deleted the booster role of <@{user}>.")), None)
        }
        Err(p) if p.code == "NO_ROLE" => info_embed("Nothing To Remove", Some(&format!("<@{user}> has no booster role.")), None),
        Err(p) if p.code == "BOT_ROLE" => {
            error_embed("Could Not Delete", Some("I could not delete that role: I need **Manage Roles** and my highest role must be above it."), None)
        }
        Err(p) => problem_embed(&p),
    };
    finish(&ctx, embed).await
}

