//! What the moderation commands share: the `reason` / `proof` options, `tagOf(user)`, the invalid-duration answer and
//! the common reply shape of the action commands.

use bot_core::interaction_helpers::Payload;
use bot_core::prelude::*;
use bot_util::text::clamp_text;
use serenity::all::User;

use crate::guild_ops::UserRef;
use crate::moderation_service::{ModSuccess, deliver_mod_reply, ModReply, DmStatus};

/// `reasonOpt(o, desc = 'Reason (audit log and DM)')`.
pub fn reason_opt(description: &str) -> CommandOption {
    CommandOption::string("reason", description).max_length(512)
}

/// The default `reason` option.
pub fn reason_default() -> CommandOption {
    reason_opt("Reason (audit log and DM)")
}

/// `proofOpt(o)`.
pub fn proof_opt() -> CommandOption {
    CommandOption::string("proof", "Proof link (http/https)").max_length(1000)
}

/// `tagOf(user)`: the user's tag with backticks replaced, at most 100 characters.
pub fn tag_of(user: &User) -> String {
    UserRef::of(user).tag_of()
}

/// The answer to a duration that cannot be read.
pub fn invalid_duration(raw: &str) -> CreateEmbedAlias {
    error_embed(
        "Invalid Duration",
        Some(&format!("Couldn't read `{}`. Use something like `12h`, `7d` or `1mo`.", clamp_text(&raw.replace('`', "'"), 32))),
        None,
    )
}

/// `serenity::builder::CreateEmbed`.
pub type CreateEmbedAlias = serenity::builder::CreateEmbed;

/// `[...(res.dm === 'failed' ? ["I couldn't DM the user."] : []), ...(res.warnings ?? [])]`.
pub fn result_warnings(res: &ModSuccess) -> Vec<String> {
    let mut w = Vec::new();
    if res.dm == DmStatus::Failed {
        w.push("I couldn't DM the user.".to_string());
    }
    w.extend(res.warnings.iter().cloned());
    w
}

/// `editReply({ embeds: [embed] })` (the commands defer first).
pub async fn edit_embed(ctx: &CommandCtx, embed: CreateEmbedAlias) -> BotResult {
    ctx.edit_reply(Payload::embed(embed).to_edit()).await?;
    Ok(())
}

/// `deliverModReply(interaction, settings, { title, lines, warnings })`.
pub async fn deliver(ctx: &CommandCtx, settings: Option<&bot_db::GuildSettings>, title: &str, lines: Vec<String>, warnings: Vec<String>) {
    deliver_mod_reply(ctx, settings, ModReply { title: title.to_string(), lines, warnings }).await;
}

/// The guild id as a string, or an error outside a guild (the router never lets that happen).
pub fn guild_of(ctx: &CommandCtx) -> anyhow::Result<String> {
    ctx.guild_id().map(|g| g.to_string()).ok_or_else(|| anyhow::anyhow!("/{} outside a guild", ctx.command_name()))
}

/// `interaction.options.getUser(name, true)`.
pub fn required_user(ctx: &CommandCtx, name: &str) -> anyhow::Result<User> {
    ctx.get_user(name).ok_or_else(|| anyhow::anyhow!("Required option \"{name}\" not found."))
}
