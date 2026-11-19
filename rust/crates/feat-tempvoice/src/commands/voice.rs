//! Port of `src/commands/community/voice.js`: `/voice` (the MEE6 `/voice-ban`, `-claim`, `-clean`, `-hide`, `-kick`,
//! `-limit`, `-lock`, `-rename`, `-reveal`, `-transfer`, `-unban` and `-unlock` commands as subcommands). Thin: it
//! defers (ephemeral), finds the temporary channel the member is connected to and calls the service, where the owner /
//! moderator rules, bot permission checks and rate limits live.

use bot_core::command::CommandOption as Opt;
use bot_core::interaction_helpers::{Payload, safe_defer, safe_reply};
use bot_core::prelude::*;
use serenity::builder::CreateEmbed;

use crate::temp_voice_service::{self as tv, ActionSource, ActionValue, Msg, VoiceActionReq};

fn member_opt(desc: &str) -> Opt {
    Opt::user("member", desc).required(true)
}

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("voice", "Control your temporary voice channel (MEE6 /voice-* commands).")
        .option(Opt::sub_command("ban", "Ban a member from your temporary voice channel (MEE6 /voice-ban).").option(member_opt("Member to ban")))
        .option(Opt::sub_command("claim", "Claim this temporary voice channel after its owner left (MEE6 /voice-claim)."))
        .option(Opt::sub_command("clean", "Moderators: delete every empty temporary voice channel (MEE6 /voice-clean)."))
        .option(Opt::sub_command("hide", "Hide your temporary voice channel (MEE6 /voice-hide)."))
        .option(Opt::sub_command("kick", "Disconnect a member from your temporary voice channel (MEE6 /voice-kick).").option(member_opt("Member to disconnect")))
        .option(
            Opt::sub_command("limit", "Change the member limit of your temporary voice channel (MEE6 /voice-limit).")
                .option(Opt::integer("member_limit", "Maximum members (0 = no limit)").required(true).min_value(0).max_value(99)),
        )
        .option(Opt::sub_command("lock", "Lock your temporary voice channel (MEE6 /voice-lock)."))
        .option(
            Opt::sub_command("rename", "Rename your temporary voice channel (MEE6 /voice-rename).")
                .option(Opt::string("name", "New channel name").required(true).min_length(1).max_length(100)),
        )
        .option(Opt::sub_command("reveal", "Unhide your temporary voice channel (MEE6 /voice-reveal)."))
        .option(
            Opt::sub_command("transfer", "Give ownership to a member in the channel (MEE6 /voice-transfer).")
                .option(member_opt("New owner (must be in the channel)")),
        )
        .option(Opt::sub_command("unban", "Unban a member from your temporary voice channel (MEE6 /voice-unban).").option(member_opt("Member to unban")))
        .option(Opt::sub_command("unlock", "Unlock your temporary voice channel (MEE6 /voice-unlock)."))
}

/// The command (`module 'tempvoice'`, gated, MEMBER: owner / moderator rules are enforced in the service).
pub fn command() -> CommandDef {
    CommandDef::new("community", data(), Some(ModuleKey::Tempvoice), execute).permission_level(PermissionLevel::Member)
}

async fn reply(ctx: &CommandCtx, embed: CreateEmbed) {
    safe_reply(&ctx.responder, Payload::embed(embed)).await;
}

async fn execute(ctx: CommandCtx) -> BotResult {
    let sub = ctx.subcommand().unwrap_or_default();
    if let Err(e) = run(&ctx, &sub).await {
        bot_util::log_error!(format!("voice {sub} failed"), bot_core::error::log_arg(&e));
        reply(&ctx, Msg::Internal.plain()).await;
    }
    Ok(())
}

async fn run(ctx: &CommandCtx, sub: &str) -> anyhow::Result<()> {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/voice outside a guild"))?;
    let member = ctx.member().cloned().ok_or_else(|| anyhow::anyhow!("/voice without a member"))?;
    let g = guild_id.to_string();
    // Memo first: the router already loaded the settings, so this normally costs no I/O.
    let settings = match ctx.app.settings().peek_settings(&g) {
        Some(s) => Some(s),
        None => ctx.app.settings().get_settings_safe(&g).await,
    };

    if sub == "clean" {
        let embed = tv::clean_empty_channels(&ctx.app, guild_id, &member, settings.as_deref()).await?;
        reply(ctx, embed).await;
        return Ok(());
    }

    // /voice acts on the temporary channel the member is connected to.
    let channel_id = tv::with_guild(&ctx.app, guild_id, |gd| gd.voice_states.get(&member.user.id).and_then(|v| v.channel_id)).flatten();
    let Some(channel_id) = channel_id else {
        reply(ctx, Msg::NotInTemp.plain()).await;
        return Ok(());
    };
    let target_id = ctx.get_user_id("member").map(|u| u.to_string());
    let value = match sub {
        "limit" => ctx.get_integer("member_limit").map(ActionValue::Int).unwrap_or_default(),
        "rename" => ctx.get_string("name").map(ActionValue::Text).unwrap_or_default(),
        _ => ActionValue::None,
    };
    let r = tv::run_voice_action(
        &ctx.app,
        VoiceActionReq {
            guild_id,
            actor: member,
            channel_id: channel_id.to_string(),
            action: sub.to_string(),
            target_id,
            value,
            settings,
            source: ActionSource::Command,
        },
    )
    .await;
    reply(ctx, r.embed).await;
    Ok(())
}
