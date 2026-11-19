//! Port of `src/commands/community/afk.js`: `/afk set | clear` for members, `/afk list | remove` for moderators.
//! Gated by the AFK module. Replies are ephemeral and never ping; the public side (announcing AFK members, welcoming
//! them back) is the `messageCreate` hook in [`crate::plugins::afk`].

use bot_core::command::CommandOption as Opt;
use bot_core::interaction_helpers::{Payload, safe_defer, safe_reply};
use bot_core::prelude::*;
use bot_util::text::clamp_text;
use serenity::all::{GuildId, Member, UserId};
use serenity::builder::CreateEmbed;

use crate::afk_service::{
    NickProblem, add_nickname_prefix, afk, afk_length, clean_reason, clear_afk, list_statuses, plain_text, plain_text_default, remove_nickname_prefix,
    set_afk,
};
use crate::ops;

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("afk", "Go AFK: others who mention you are told, and it ends when you write.")
        .option(
            Opt::sub_command("set", "Mark yourself AFK.")
                .option(Opt::string("reason", "Why you are away").max_length(bot_config::limits::AFK_REASON_LENGTH as u16)),
        )
        .option(Opt::sub_command("clear", "End your AFK."))
        .option(Opt::sub_command("remove", "Moderators: end a member's AFK.").option(Opt::user("user", "The member").required(true)))
        .option(Opt::sub_command("list", "Moderators: list the AFK members."))
}

/// The command (`module 'afk'`, gated, MEMBER; `remove` and `list` MODERATOR).
pub fn command() -> CommandDef {
    CommandDef::new("community", data(), Some(ModuleKey::Afk), execute)
        .permission_level(PermissionLevel::Member)
        .subcommand_permission("remove", PermissionLevel::Moderator)
        .subcommand_permission("list", PermissionLevel::Moderator)
}

/// `nickProblem(problem)`.
pub fn nick_problem(problem: Option<NickProblem>) -> Option<&'static str> {
    match problem? {
        NickProblem::Cannot => Some("I couldn't add [AFK] to your nickname: I need Manage Nicknames and a role above yours."),
        NickProblem::Failed => Some("I couldn't change your nickname just now."),
    }
}

/// `memberFor(interaction, userId)`: the invoker, else the cached member, else a fetch.
async fn member_for(ctx: &CommandCtx, guild_id: GuildId, user_id: UserId) -> Option<Member> {
    if let Some(m) = ctx.member().filter(|m| m.user.id == user_id) {
        return Some(m.clone());
    }
    if let Some(m) = ops::view(&ctx.app, guild_id, |g| g.members.get(&user_id).cloned()).flatten() {
        return Some(m);
    }
    ops::ops(&ctx.app).fetch_member(&ctx.app, guild_id, user_id).await
}

async fn run_set(ctx: &CommandCtx, guild_id: GuildId, settings: Option<&bot_db::GuildSettings>) -> anyhow::Result<CreateEmbed> {
    let reason = clean_reason(ctx.get_string("reason").as_deref());
    let member = ctx.member().cloned();
    let nick = add_nickname_prefix(&ctx.app, guild_id, member.as_ref(), settings).await;
    let g = guild_id.to_string();
    if let Err(e) = set_afk(&ctx.app, &g, &ctx.user().id.to_string(), reason.as_deref(), nick.changed, nick.previous_nick.as_deref()).await {
        // No row means nothing would ever take the prefix off again: undo it before the error reply.
        if nick.changed
            && let Some(mut m) = member
        {
            // The member now carries the prefixed nickname (discord.js patches the cached member on setNickname).
            m.nick = crate::afk_service::prefixed_nickname(&bot_core::adapters::member_display_name(&m));
            remove_nickname_prefix(&ctx.app, guild_id, Some(&m), nick.previous_nick.as_deref()).await;
        }
        return Err(e);
    }
    let mut lines = vec![
        match &reason {
            Some(r) => format!("You are now AFK: {}", plain_text_default(r)),
            None => "You are now AFK.".to_string(),
        },
        "Anyone who mentions you is told. Your next message ends it.".to_string(),
    ];
    if let Some(p) = nick_problem(nick.problem) {
        lines.push(format!("-# {p}"));
    }
    Ok(success_embed(&format!("{} AFK", afk::EMOJI), Some(&lines.join("\n")), None))
}

async fn run_clear(ctx: &CommandCtx, guild_id: GuildId) -> anyhow::Result<CreateEmbed> {
    let Some(row) = clear_afk(&ctx.app, &guild_id.to_string(), &ctx.user().id.to_string()).await? else {
        return Ok(info_embed("Not AFK", Some("You are not AFK."), None));
    };
    if row.nick_changed {
        remove_nickname_prefix(&ctx.app, guild_id, ctx.member(), row.previous_nick.as_deref()).await;
    }
    let length = afk_length(ctx.app.clock().now_ms() - row.since.timestamp_millis());
    Ok(success_embed("Welcome back", Some(&format!("You were AFK for **{length}**.")), None))
}

async fn run_remove(ctx: &CommandCtx, guild_id: GuildId) -> anyhow::Result<CreateEmbed> {
    let target_id = ctx.get_user_id("user").ok_or_else(|| anyhow::anyhow!("Required option \"user\" not found."))?;
    // Discord sends the resolved user; without it, the member's user.
    let target = match ctx.get_user("user") {
        Some(u) => u,
        None => member_for(ctx, guild_id, target_id).await.map(|m| m.user).ok_or_else(|| anyhow::anyhow!("Required option \"user\" not found."))?,
    };
    let Some(row) = clear_afk(&ctx.app, &guild_id.to_string(), &target.id.to_string()).await? else {
        let name = target.global_name.clone().unwrap_or_else(|| target.name.clone());
        return Ok(info_embed("Not AFK", Some(&format!("{} is not AFK.", plain_text(&name, 40))), None));
    };
    if row.nick_changed {
        let member = member_for(ctx, guild_id, target.id).await;
        remove_nickname_prefix(&ctx.app, guild_id, member.as_ref(), row.previous_nick.as_deref()).await;
    }
    Ok(success_embed("AFK removed", Some(&format!("<@{}> is no longer AFK.", target.id)), None))
}

/// The `/afk list` lines.
pub fn list_lines(rows: &[bot_db::AfkStatus]) -> Vec<String> {
    rows.iter()
        .map(|r| {
            let reason = r.reason.as_deref().filter(|s| !s.is_empty()).map(|s| format!(": {}", plain_text(s, 120))).unwrap_or_default();
            format!("<@{}>{reason} (since {})", r.user_id, bot_util::time::discord_timestamp(&r.since, "R"))
        })
        .collect()
}

async fn run_list(ctx: &CommandCtx, guild_id: GuildId) -> anyhow::Result<CreateEmbed> {
    let (rows, total) = list_statuses(ctx.app.db(), &guild_id.to_string()).await?;
    if rows.is_empty() {
        return Ok(info_embed(&format!("{} AFK members", afk::EMOJI), Some("Nobody is AFK."), None));
    }
    let more = if total > rows.len() as i64 { format!("\n-# … and {} more", total - rows.len() as i64) } else { String::new() };
    let text = format!("{}{more}", clamp_text(&list_lines(&rows).join("\n"), 3800));
    Ok(info_embed(&format!("{} AFK members ({total})", afk::EMOJI), Some(&text), None))
}

async fn execute(ctx: CommandCtx) -> BotResult {
    let sub = ctx.subcommand().unwrap_or_default();
    let result: anyhow::Result<()> = async {
        if !safe_defer(&ctx.responder, true).await {
            return Ok(());
        }
        let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/afk outside a guild"))?;
        let g = guild_id.to_string();
        // Memo first: the router already loaded the settings, so this normally costs no I/O.
        let settings = match ctx.app.settings().peek_settings(&g) {
            Some(s) => Some(s),
            None => ctx.app.settings().get_settings_safe(&g).await,
        };
        let embed = match sub.as_str() {
            "set" => run_set(&ctx, guild_id, settings.as_deref()).await?,
            "clear" => run_clear(&ctx, guild_id).await?,
            "remove" => run_remove(&ctx, guild_id).await?,
            _ => run_list(&ctx, guild_id).await?,
        };
        safe_reply(&ctx.responder, Payload::embed(embed).no_mentions()).await;
        Ok(())
    }
    .await;
    if let Err(e) = result {
        bot_util::log_error!(format!("afk {sub} failed"), bot_core::error::log_arg(&e));
        safe_reply(&ctx.responder, Payload::embed(warn_embed("Something went wrong", Some("Please try again in a moment."), None)).no_mentions()).await;
    }
    Ok(())
}
