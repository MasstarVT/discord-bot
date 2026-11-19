//! Port of `src/commands/utility/owner.js`: `/owner` shows who owns the bot (`BOT_OWNER_ID`). A core command with no
//! subcommands.

use bot_core::interaction_helpers::Payload;
use bot_core::prelude::*;
use serenity::all::{CreateAllowedMentions, UserId};

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("owner", "Show who owns this bot.")
}

/// The command (core, MEMBER).
pub fn command() -> CommandDef {
    CommandDef::new("utility", data(), None, execute).permission_level(PermissionLevel::Member)
}

/// The owner id when `BOT_OWNER_ID` (trimmed) is a snowflake (`/^\d{17,20}$/`).
pub fn owner_id_of(raw: Option<&str>) -> Option<String> {
    let id = bot_util::js::trim(raw.unwrap_or(""));
    ((17..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())).then(|| id.to_string())
}

/// The public answer: `👑 <@id> is the owner of this bot.`
pub fn owner_line(id: &str) -> String {
    format!("👑 <@{id}> is the owner of this bot.")
}

async fn execute(ctx: CommandCtx) -> BotResult {
    let Some(owner) = owner_id_of(ctx.app.config().bot_owner_id.as_deref()) else {
        let embed = error_embed("No Owner Set", Some("This bot has no owner configured (`BOT_OWNER_ID`)."), None);
        ctx.reply(Payload::embed(embed).ephemeral(true).to_reply()).await?;
        return Ok(());
    };
    // Mentions inside embeds never ping, so the owner goes in plain content and only that user may be pinged.
    // An id Discord could never issue (0, or beyond 64 bits) pings nobody.
    let users: Vec<UserId> = owner.parse::<u64>().ok().filter(|n| *n != 0).map(UserId::new).into_iter().collect();
    let payload = Payload::text(owner_line(&owner)).allowed_mentions(CreateAllowedMentions::new().users(users));
    ctx.reply(payload.to_reply()).await?;
    Ok(())
}
