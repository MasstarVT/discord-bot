//! The economy commands (`src/commands/economy/*.js` and `src/commands/admin/economy-admin.js`) and what they share.

pub mod balance;
pub mod daily;
pub mod economy_admin;
pub mod inventory;
pub mod pay;
pub mod pity;
pub mod richest;
pub mod shop;
#[path = "use.rs"]
pub mod use_;
pub mod work;

use bot_core::interaction_helpers::Payload;
use bot_core::prelude::*;
use bot_db::GuildSettings;
use bot_util::placeholder_parser::{PlaceholderChannel, RenderContext, VarValue};
use serenity::all::{GuildId, Member, Role, RoleId, User};
use serenity::builder::CreateEmbed;

/// The guild id of a guild-only command (the router refuses DMs before `execute`).
pub(crate) fn guild_of(ctx: &CommandCtx) -> anyhow::Result<GuildId> {
    ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/{} outside a guild", ctx.command_name()))
}

/// `getSettings(guildId)` (throws on a database failure).
pub(crate) async fn settings_of(ctx: &CommandCtx, guild_id: GuildId) -> anyhow::Result<Settings> {
    Ok(ctx.app.settings().get_settings(&guild_id.to_string()).await?)
}

/// `Some(&row)` for the service calls.
pub(crate) fn opt(s: &Settings) -> Option<&GuildSettings> {
    Some(s.as_ref())
}

/// `interaction.editReply({ embeds: [embed] })`.
pub(crate) async fn edit_embed(ctx: &CommandCtx, embed: CreateEmbed) -> BotResult {
    ctx.edit_reply(Payload::embed(embed).to_edit()).await?;
    Ok(())
}

/// `interaction.reply({ embeds: [errorEmbed(title, description)], ephemeral: true })`.
pub(crate) async fn private_error(ctx: &CommandCtx, title: &str, description: &str) -> BotResult {
    ctx.reply_embed(error_embed(title, Some(description), None), true).await?;
    Ok(())
}

/// The "Not Allowed" answer for a bot target.
pub(crate) const BOTS_CANT: &str = "Bots can't hold coins or items.";

/// `member.displayName` (nickname, then global name, then username).
pub(crate) fn member_name(m: &Member) -> String {
    bot_core::adapters::member_display_name(m)
}

/// `user.displayName` (global name, then username).
pub(crate) fn user_name(u: &User) -> String {
    bot_core::adapters::user_display_name(u)
}

/// `clampText(escapeMarkdown((self ? member.displayName : getMember(option)?.displayName) ?? target.displayName ?? target.username), 100)`.
pub(crate) fn target_label(ctx: &CommandCtx, option: &str, target: &User, is_self: bool) -> String {
    let member_display = if is_self { ctx.member().map(member_name) } else { ctx.get_member(option).as_ref().map(member_name) };
    let raw = member_display.unwrap_or_else(|| user_name(target));
    bot_util::text::clamp_text(&bot_core::markdown::escape_markdown(&raw), 100)
}

/// `[...interaction.member?.roles?.cache?.keys() ?? []]`: discord.js's role cache of a member holds the @everyone
/// role (the guild id) first, then the member's roles that still exist in the guild. Without a cached guild the
/// payload roles stand in for the second part.
pub(crate) fn member_role_ids(ctx: &CommandCtx) -> Vec<String> {
    let (Some(member), Some(guild_id)) = (ctx.member(), ctx.guild_id()) else { return Vec::new() };
    let ids = bot_core::interaction_helpers::with_cached_guild(&ctx.app, guild_id, |g| bot_core::interaction_helpers::member_role_ids(g, member))
        .unwrap_or_else(|| {
            let everyone = RoleId::new(guild_id.get());
            std::iter::once(everyone).chain(member.roles.iter().copied().filter(|r| *r != everyone)).collect()
        });
    ids.iter().map(|r| r.to_string()).collect()
}

/// `${n} day${n === 1 ? '' : 's'}`.
pub(crate) fn plural(n: i64, one: &str, many: &str) -> String {
    if n == 1 { one.to_string() } else { many.to_string() }
}

/// `guild.roles.cache.get(id)` from the serenity cache.
pub(crate) fn cached_role(app: &App, guild_id: GuildId, role_id: &str) -> Option<Role> {
    let id = role_id.parse::<u64>().ok().filter(|v| *v != 0).map(RoleId::new)?;
    let d = app.discord()?;
    let guild = d.cache.guild(guild_id)?;
    guild.roles.get(&id).cloned()
}

/// `renderPlaceholders(template, { member, user, guild, channel: interaction.channel ?? { id: interaction.channelId },
/// vars }, { maxLength })`.
pub(crate) fn render_message(ctx: &CommandCtx, member: Option<&Member>, template: &str, vars: Vec<(String, String)>, max_length: usize) -> String {
    let mut rc: RenderContext = bot_core::adapters::render_context(&ctx.app, ctx.guild_id(), ctx.user(), member, Some(ctx.channel_id()));
    if rc.channel.is_none() {
        rc.channel = Some(PlaceholderChannel { id: ctx.channel_id().to_string(), name: None, kind: None });
    }
    rc.vars = Some(vars.into_iter().map(|(k, v)| (k, VarValue::Text(v))).collect());
    bot_util::placeholder_parser::render_placeholders(template, &rc, Some(max_length))
}

/// `respondAutocomplete(interaction, choices)`.
pub(crate) async fn respond(ctx: &AutocompleteCtx, choices: Vec<bot_core::interaction_helpers::Choice>) {
    bot_core::interaction_helpers::respond_autocomplete(&ctx.responder, &choices).await;
}

/// `withTimeout(promise, ackBudgetMs(interaction), label)`.
pub(crate) async fn within_budget<T, F>(ctx: &AutocompleteCtx, label: &str, fut: F) -> anyhow::Result<T>
where
    F: std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
    T: Send + 'static,
{
    let budget = bot_core::interaction::ack_budget_ms(ctx.interaction.id);
    bot_util::async_util::with_timeout(async move { fut.await.map_err(|e| e.to_string()) }, budget, label).await.map_err(|e| anyhow::anyhow!("{e}"))
}
