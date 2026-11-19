//! Port of `src/plugins/sticky.js`: keeps a message at the bottom of a channel. A member message or one of the bot's own
//! (its command answers, level-ups ...) in a sticky channel schedules a re-send (it runs once the channel has been quiet
//! for a few seconds); deleting the sticky message or its channel cleans up even while the module is off. Also the
//! `sticky-modal` text form of `/sticky set`. Rules and state: [`crate::sticky_service`].

use std::sync::Arc;

use bot_core::interaction_helpers::{Payload, reply_error, safe_defer, safe_reply};
use bot_core::prelude::*;
use serde_json::{Value, json};
use serenity::all::{ChannelId, GuildId, MessageFlags};
use serenity::builder::{CreateActionRow, CreateEmbed, CreateInputText, CreateModal};

use crate::sticky_service::{
    ResendStatus, SetArgs, SetResult, StickyError, clear_last_message, drop_channel, get_channels, note_message, peek_channels, set_sticky,
};

/// The text field of the `sticky-modal` form (a `field` customId: no parameters).
pub const STICKY_FIELD: &str = "sticky-content";

// ── Hook filters (sync: they run on every message) ────────────────────────────

/// `mayStick(ctx)`: a member message that may need a re-send: the module is not known to be off and the memoised
/// sticky channels (when there are any) include a running one. Without a memo it passes and the handler loads the list.
pub fn may_stick(app: &App, guild_id: &str, channel_id: &str) -> bool {
    if let Some(s) = app.settings().peek_settings(guild_id)
        && !is_module_enabled(Some(&s), Some(ModuleKey::Sticky))
    {
        return false;
    }
    match peek_channels(app, guild_id) {
        None => true,
        Some(ch) => ch.get(channel_id) == Some(&true),
    }
}

/// `mayStickOwn(ctx)`: one of the bot's own messages, as [`may_stick`], minus ephemeral answers (nobody else sees them).
pub fn may_stick_own(app: &App, guild_id: &str, message: &serenity::all::Message) -> bool {
    if message.flags.is_some_and(|f| f.contains(MessageFlags::EPHEMERAL)) {
        return false;
    }
    may_stick(app, guild_id, &message.channel_id.to_string())
}

fn has_sticky_in(app: &App, guild_id: &str, channel_id: &str) -> bool {
    match peek_channels(app, guild_id) {
        None => true,
        Some(ch) => ch.contains_key(channel_id),
    }
}

/// `mayBeSticky(ctx)`: messageDelete of a message of the bot (or an uncached one) in a channel that may have a sticky.
pub fn may_be_sticky(app: &App, data: &MessageDelete) -> bool {
    let bot = app.discord().map(|d| d.cache.current_user().id);
    let own_or_unknown = match &data.message {
        None => true,
        Some(m) => data.partial || Some(m.author.id) == bot,
    };
    own_or_unknown && has_sticky_in(app, &data.guild_id.to_string(), &data.channel_id.to_string())
}

/// `mayHaveSticky(ctx)`: channelDelete of a channel that may have a sticky.
pub fn may_have_sticky(app: &App, guild_id: &str, channel_id: &str) -> bool {
    has_sticky_in(app, guild_id, channel_id)
}

// ── Hook handlers ─────────────────────────────────────────────────────────────

/// `onMessage(ctx)`: schedules the re-send for a message in a sticky channel; returns at once (the timer waits).
pub async fn on_message_in(app: &App, guild_id: GuildId, channel_id: ChannelId) -> BotResult {
    let (g, c) = (guild_id.to_string(), channel_id.to_string());
    if let Some(channels) = get_channels(app, &g).await
        && channels.get(&c) == Some(&true)
    {
        note_message(app, &g, &c);
    }
    Ok(())
}

async fn on_message(ctx: Arc<HookCtx<MessageCreate>>) -> BotResult {
    on_message_in(&ctx.app, ctx.data.guild_id, ctx.data.channel_id).await
}

async fn on_own_message(ctx: Arc<HookCtx<OwnMessage>>) -> BotResult {
    on_message_in(&ctx.app, ctx.data.guild_id, ctx.data.channel_id).await
}

/// `onMessageDelete(ctx)`: the sticky message was deleted by someone else: forget its id (the next re-send just posts).
async fn on_message_delete(ctx: Arc<HookCtx<MessageDelete>>) -> BotResult {
    clear_last_message(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.channel_id.to_string(), &ctx.data.message_id.to_string()).await?;
    Ok(())
}

/// `onChannelDelete(ctx)`: the channel is gone: so is its sticky.
async fn on_channel_delete(ctx: Arc<HookCtx<ChannelDelete>>) -> BotResult {
    drop_channel(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.channel.id.to_string()).await?;
    Ok(())
}

// ── /sticky set: the text form and the answer ─────────────────────────────────

/// `buildStickyModal(channelId, content)`: the form `/sticky set` opens when no text is given: one paragraph prefilled
/// with the current text.
pub fn build_sticky_modal(channel_id: &str, content: Option<&str>) -> CreateModal {
    let max = bot_core::custom_embed::limits::CONTENT;
    let mut input = CreateInputText::new(serenity::all::InputTextStyle::Paragraph, "Sticky message text", STICKY_FIELD)
        .max_length(max as u16)
        .required(false)
        .placeholder("Rules, links, a reminder … {server} and {channel} work.");
    if let Some(c) = content.filter(|c| !c.is_empty()) {
        input = input.value(bot_util::js::utf16_prefix(c, max).to_string());
    }
    let id = bot_config::custom_id!("sticky-modal", channel_id).unwrap_or_else(|_| format!("sticky-modal:{channel_id}"));
    CreateModal::new(id, "Sticky message").components(vec![CreateActionRow::InputText(input)])
}

/// The status line of a saved `/sticky set` (`STATUS_NOTES[status] ?? NOT_POSTED`).
pub fn status_note(status: ResendStatus) -> &'static str {
    match status {
        ResendStatus::Sent => "It is posted now and moves back down once chat has been quiet for a few seconds.",
        ResendStatus::Current => "It is already the last message there.",
        ResendStatus::Off => "⚠️ The **Sticky Messages** module is off, so nothing is posted yet. Turn it on with `/module enable sticky`.",
        ResendStatus::Perms => "⚠️ I can't post there right now. Check my permissions in the channel.",
        _ => "It is saved but could not be posted just now; it is tried again after the next message.",
    }
}

const USER_TOKEN_WARNING: &str =
    "\n⚠️ A sticky message has no member, so user variables like `{user}` are posted as written. Server and channel variables work.";

/// `USER_TOKEN_RE` (`/\{user(\.[a-z_]+)?\}/i`).
pub fn has_user_token(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    let b = lower.as_bytes();
    let mut i = 0;
    while let Some(pos) = lower[i..].find("{user") {
        let mut j = i + pos + 5;
        if b.get(j) == Some(&b'}') {
            return true;
        }
        if b.get(j) == Some(&b'.') {
            j += 1;
            let start = j;
            while b.get(j).is_some_and(|c| c.is_ascii_lowercase() || *c == b'_') {
                j += 1;
            }
            if j > start && b.get(j) == Some(&b'}') {
                return true;
            }
        }
        i += pos + 1;
    }
    false
}

/// `variableNotes({ content, embed }, renderCtx)`: warning lines for variables a sticky cannot fill in: unknown ones, and
/// `{user…}` (nobody is "the user"). `""` when there is nothing to warn about.
pub fn variable_notes(content: Option<&str>, embed: Option<&Value>, ctx: &bot_util::placeholder_parser::RenderContext) -> String {
    let tpl = json!({
        "content": content,
        "embed": embed.map(|e| e["embed"].clone()).unwrap_or(Value::Null),
        "buttons": embed.map(|e| e["buttons"].clone()).filter(Value::is_array).unwrap_or_else(|| json!([])),
    });
    let mut notes = feat_embeds::embed_service::unknown_variables_note(&tpl, ctx);
    if has_user_token(&serde_json::to_string(&tpl).unwrap_or_default()) {
        notes.push_str(USER_TOKEN_WARNING);
    }
    notes
}

/// `setResultEmbed(channelId, { created, hasEmbed, status }, notes)`: the embed that answers a saved `/sticky set`.
pub fn set_result_embed(channel_id: &str, r: &SetResult, notes: &str) -> CreateEmbed {
    let head = format!(
        "The sticky message in <#{channel_id}> is {}{}.",
        if r.created { "set" } else { "updated" },
        if r.has_embed { " (with an embed)" } else { "" }
    );
    success_embed(
        if r.created { "Sticky Message Set" } else { "Sticky Message Updated" },
        Some(&format!("{head}\n{}{notes}", status_note(r.status))),
        None,
    )
}

/// `applySet(interaction, args)`: saves a sticky and answers the (deferred) interaction. Never fails.
pub async fn apply_set(app: &App, r: &Responder, args: SetArgs) -> BotResult {
    let (g, c, u) = (args.guild_id.clone(), args.channel_id.clone(), args.user_id.clone());
    let (content, embed) = (args.content.clone(), args.embed.clone());
    let embed_out = match set_sticky(app, args).await {
        Ok(result) => {
            bot_util::log_info!(format!("[sticky] {g}: {c} {} by {u} ({})", if result.created { "set" } else { "updated" }, result.status.as_str()));
            let ctx = match (g.parse::<u64>().ok().filter(|v| *v != 0), c.parse::<u64>().ok().filter(|v| *v != 0)) {
                (Some(gid), Some(cid)) => bot_core::interaction_helpers::with_cached_guild(app, GuildId::new(gid), |gd| {
                    crate::captcha_service::guild_render_context(Some(gd), bot_core::permissions::find_channel(gd, ChannelId::new(cid)))
                })
                .unwrap_or_default(),
                _ => Default::default(),
            };
            set_result_embed(&c, &result, &variable_notes(content.as_deref(), embed.as_ref(), &ctx))
        }
        Err(e) => match e.downcast::<StickyError>() {
            Ok(se) => error_embed(&se.title, Some(&se.message), None),
            Err(e) => {
                bot_util::log_error!(format!("[sticky] {g}: set failed"), bot_core::error::log_arg(&e));
                error_embed("Could Not Save", Some("The change could not be saved. Please try again in a moment."), None)
            }
        },
    };
    safe_reply(r, Payload::embed(embed_out).no_mentions()).await;
    Ok(())
}

/// `handleStickyModal(interaction)`: `sticky-modal:{channelId}`. Works while the module is off; the router checks no
/// level for forms, so the Admin level is checked here.
pub async fn handle_sticky_modal(ctx: ModalCtx) -> BotResult {
    let (_, params) = ctx.parsed();
    let channel_id = params.first().cloned().unwrap_or_default();
    let cached = ctx.guild_id().filter(|g| bot_core::interaction_helpers::with_cached_guild(&ctx.app, *g, |_| ()).is_some());
    let Some(guild_id) = cached.filter(|_| !channel_id.is_empty()) else {
        reply_error(&ctx.responder, "Form Expired", Some("This form has expired. Please run `/sticky set` again."), true).await;
        return Ok(());
    };
    let g = guild_id.to_string();
    let settings = ctx.app.settings().peek_settings(&g);
    let access = ctx.member_access();
    if resolve_permission_level(access.as_ref(), settings.as_deref(), ctx.app.config().bot_owner_id.as_deref()) < PermissionLevel::Admin {
        reply_error(&ctx.responder, "Insufficient Permissions", Some("You need the Admin level to change sticky messages."), true).await;
        return Ok(());
    }
    let Some(content) = ctx.value(STICKY_FIELD) else {
        reply_error(&ctx.responder, "Form Expired", Some("This form is out of date. Please run `/sticky set` again."), true).await;
        return Ok(());
    };
    let exists = channel_id
        .parse::<u64>()
        .ok()
        .filter(|v| *v != 0)
        .is_some_and(|c| bot_core::interaction_helpers::cached_channel(&ctx.app, guild_id, ChannelId::new(c)).is_some());
    if !exists {
        reply_error(&ctx.responder, "Channel Not Found", Some("That channel no longer exists."), true).await;
        return Ok(());
    }
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let args = SetArgs { guild_id: g, channel_id, user_id: ctx.user().id.to_string(), content: Some(content), embed: None, keep_embed: true };
    apply_set(&ctx.app, &ctx.responder, args).await
}

/// The plugin (`id: 'sticky'`, `module: 'sticky'`).
pub fn plugin() -> Plugin {
    Plugin::new("sticky", Some(ModuleKey::Sticky))
        // HOOK_ORDER.messageCreate: last, after automod has removed what it removes.
        .hook(Hook::<MessageCreate>::new("sticky", on_message).filter(|ctx| may_stick(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.channel_id.to_string())))
        // The bot's own messages, its command answers included (messageCreate carries members' messages only).
        .hook(Hook::<OwnMessage>::new("sticky", on_own_message).filter(|ctx| may_stick_own(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.message)))
        // Cleanup runs while the module is off too: a row must not keep pointing at a message or channel that is gone.
        .hook(Hook::<MessageDelete>::new("sticky", on_message_delete).always().filter(|ctx| may_be_sticky(&ctx.app, &ctx.data)))
        .hook(
            Hook::<ChannelDelete>::new("sticky", on_channel_delete)
                .always()
                .filter(|ctx| may_have_sticky(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.channel.id.to_string())),
        )
        // sticky-modal:{channelId}: the admin text form, so it must work while the module is off.
        .modal("sticky-modal", handle_sticky_modal)
        .component_module(ModuleRef::Ungated)
}
