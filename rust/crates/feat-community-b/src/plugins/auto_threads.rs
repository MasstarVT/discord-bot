//! Port of `src/plugins/auto-threads.js`: every member message in a configured text or announcement channel gets a
//! thread (optionally with a Close button), and new posts in the announcement channels of
//! `GuildSettings.autoPublishChannelIds` are published to the servers that follow them. The lists come from the
//! settings memo, so an ordinary message costs no query. The `autothread:close:{starterUserId}` button closes a
//! thread. Rules and state: [`crate::auto_thread_service`].

use std::sync::Arc;

use bot_config::{ModuleKey, PermissionLevel};
use bot_core::adapters;
use bot_core::interaction_helpers::{PermTarget, bot_permissions, cached_channel, is_interaction_gone, reply_error};
use bot_core::permission_checker::{MemberAccess, resolve_permission_level};
use bot_core::permissions::{self, js_has};
use bot_core::prelude::*;
use serenity::all::{ChannelType, GuildChannel, GuildId, Permissions};
use serenity::builder::{CreateAllowedMentions, CreateInteractionResponseFollowup, CreateInteractionResponseMessage};

use crate::auto_thread_service::{
    NameSource, PublishInfo, ThreadSource, channel_index, create_thread, detach, drop_channel, listed_channel, plain_text, publish_message,
};

fn threadable(kind: ChannelType) -> bool {
    matches!(kind, ChannelType::Text | ChannelType::News)
}

// ── Hook filters (sync: they run on every message) ────────────────────────────

/// `mayHandle(ctx)`: a member message in a text or announcement channel (not a thread or forum) that may need a
/// thread or a crosspost. Without a settings memo it passes and the handler loads the settings.
pub fn may_handle(app: &App, guild_id: &str, channel: Option<&GuildChannel>) -> bool {
    let Some(channel) = channel else { return false };
    if permissions::is_thread(channel) || !threadable(channel.kind) {
        return false;
    }
    listed_channel(app.settings().peek_settings(guild_id).as_deref(), &channel.id.to_string()) != Some(false)
}

/// `mayPublish(ctx)`: a post by another bot or a webhook in an announcement channel that may be published. Edits are
/// skipped, except the one that turns a deferred reply's "thinking" placeholder into the answer.
pub fn may_publish(app: &App, guild_id: &str, edited: bool, was_loading: Option<bool>, channel: Option<&GuildChannel>) -> bool {
    if edited && was_loading != Some(true) {
        return false;
    }
    let Some(channel) = channel.filter(|c| c.kind == ChannelType::News) else { return false };
    match app.settings().peek_settings(guild_id) {
        None => true,
        Some(s) => is_module_enabled(Some(&s), Some(ModuleKey::Autothreads)) && channel_index(Some(&s)).publish.contains(&channel.id.to_string()),
    }
}

/// `mayBeListed(ctx)`: a deleted channel that may be in a list.
pub fn may_be_listed(app: &App, guild_id: &str, channel_id: &str) -> bool {
    match app.settings().peek_settings(guild_id) {
        None => true,
        Some(s) => {
            let index = channel_index(Some(&s));
            index.threads.contains_key(channel_id) || index.publish.contains(channel_id)
        }
    }
}

fn channel_of(app: &App, guild_id: GuildId, channel: &Option<GuildChannel>, channel_id: serenity::all::ChannelId) -> Option<GuildChannel> {
    channel.clone().or_else(|| cached_channel(app, guild_id, channel_id))
}

// ── Hook handlers ─────────────────────────────────────────────────────────────

/// `message.cleanContent`. discord.js puts the users and members a message mentions into its caches when it builds
/// the message, so a mention of a member the cache did not know still becomes `@displayName`; serenity's cache does
/// not, so those mentions are named from the message's own `mentions` (the partial member's nickname, else the user's
/// display name) before the rest goes through bot-core's `clean_content`.
pub fn clean_content_of(app: &App, message: &serenity::all::Message) -> String {
    let cache = app.discord().map(|d| d.cache.clone());
    let in_cache = |id: serenity::all::UserId| {
        cache.as_ref().zip(message.guild_id).is_some_and(|(c, g)| c.guild(g).is_some_and(|g| g.members.contains_key(&id)))
    };
    let mut content = message.content.clone();
    for user in message.mentions.iter().filter(|u| message.guild_id.is_some() && !in_cache(u.id)) {
        let name = user.member.as_ref().and_then(|m| m.nick.clone()).unwrap_or_else(|| adapters::user_display_name(user));
        let at = format!("@{name}");
        content = content.replace(&format!("<@{}>", user.id), &at).replace(&format!("<@!{}>", user.id), &at);
    }
    adapters::clean_content_in(app, message.guild_id, &content)
}

/// What the thread name reads from a member message.
fn name_source(app: &App, data: &MessageCreate, channel: Option<&GuildChannel>) -> NameSource {
    // Read under the cache guard: a busy channel must not clone the whole guild (members included) per message.
    let (guild, member) = match app.discord().and_then(|d| d.cache.guild(data.guild_id)) {
        Some(g) => (Some(adapters::placeholder_guild(&g)), data.member.as_ref().map(|m| adapters::placeholder_member(m, Some(&g)))),
        None => (None, data.member.as_ref().map(|m| adapters::placeholder_member(m, None))),
    };
    // `message.cleanContent`: user mentions become the member's display name, as discord.js does.
    let content = clean_content_of(app, &data.message);
    NameSource {
        member: member.map(|mut pm| {
            if pm.user.is_none() {
                pm.user = Some(adapters::placeholder_user(&data.message.author));
            }
            pm
        }),
        user: Some(adapters::placeholder_user(&data.message.author)),
        guild,
        channel: channel.map(adapters::placeholder_channel),
        content,
        created_at_ms: Some(data.message.timestamp.unix_timestamp() as f64 * 1000.0 + f64::from(data.message.timestamp.timestamp_subsec_millis())),
        now_ms: app.clock().now_ms() as f64,
    }
}

/// `onMessage(ctx)`: publishes the post (in the background) and starts its thread. Never fails.
pub async fn on_message(ctx: Arc<HookCtx<MessageCreate>>) -> BotResult {
    let app = &ctx.app;
    let Some(settings) = ctx.get_settings().await else { return Ok(()) };
    if !is_module_enabled(Some(&settings), Some(ModuleKey::Autothreads)) {
        return Ok(());
    }
    let index = channel_index(Some(&settings));
    let channel_id = ctx.data.channel_id;
    let cid = channel_id.to_string();
    let entry = index.threads.get(&cid).cloned();
    let g = ctx.data.guild_id;
    if index.publish.contains(&cid) {
        // The crosspost can sit in the REST queue; the hooks after this one (sticky) must not wait for it.
        let (a, m, info) = (app.clone(), ctx.data.message.id, PublishInfo::of(&ctx.data.message));
        detach(app, async move {
            publish_message(&a, g, channel_id, m, &info).await;
        });
    }
    if let Some(entry) = entry {
        let channel = channel_of(app, g, &ctx.data.channel, channel_id);
        let src = ThreadSource {
            guild_id: g,
            channel_id,
            message_id: ctx.data.message.id,
            author_id: ctx.data.message.author.id.to_string(),
            name: name_source(app, &ctx.data, channel.as_ref()),
            in_thread: channel.as_ref().is_none_or(permissions::is_thread),
        };
        create_thread(app, &src, &entry).await;
    }
    Ok(())
}

/// `onBotMessage(ctx)`: publishes the post of a bot or webhook in the background.
pub async fn on_bot_message(ctx: Arc<HookCtx<BotMessage>>) -> BotResult {
    let app = &ctx.app;
    let Some(settings) = ctx.get_settings().await else { return Ok(()) };
    if !is_module_enabled(Some(&settings), Some(ModuleKey::Autothreads)) || !channel_index(Some(&settings)).publish.contains(&ctx.data.channel_id.to_string()) {
        return Ok(());
    }
    let (a, g, c, m, info) = (app.clone(), ctx.data.guild_id, ctx.data.channel_id, ctx.data.message.id, PublishInfo::of(&ctx.data.message));
    detach(app, async move {
        publish_message(&a, g, c, m, &info).await;
    });
    Ok(())
}

/// `onChannelDelete(ctx)`: the channel is gone, so it leaves both lists (also while the module is off).
pub async fn on_channel_delete(ctx: Arc<HookCtx<ChannelDelete>>) -> BotResult {
    let channel_id = ctx.data.channel.id.to_string();
    if let Some(settings) = ctx.get_settings().await {
        let index = channel_index(Some(&settings));
        if !index.threads.contains_key(&channel_id) && !index.publish.contains(&channel_id) {
            return Ok(());
        }
    }
    let g = ctx.data.guild_id.to_string();
    if drop_channel(&ctx.app, &g, &channel_id).await? {
        bot_util::log_info!(format!("[auto-threads] {g}: {channel_id} removed (the channel was deleted)"));
    }
    Ok(())
}

// ── The Close button ──────────────────────────────────────────────────────────

async fn notice(ctx: &ComponentCtx, title: &str, text: &str) {
    reply_error(&ctx.responder, title, Some(text), true).await;
}

/// `handleClose(interaction)`: `autothread:close:{starterUserId}` archives (and locks, when the bot may) the thread.
/// Allowed for the member who started the thread, a member with Manage Threads there and the bot's Moderators.
pub async fn handle_close(ctx: ComponentCtx) -> BotResult {
    let app = &ctx.app;
    let (_, params) = ctx.parsed();
    let action = params.first().map(String::as_str).unwrap_or("");
    let starter = params.get(1).cloned().unwrap_or_default();
    let guild_id = ctx.guild_id().filter(|g| app.discord().is_some_and(|d| d.cache.guild(*g).is_some()));
    // `interaction.channel`: discord.js (Partials.Channel) builds it from the interaction when the cache has none (an
    // archived thread); here the thread the interaction names is fetched (and cached) instead.
    let thread = match guild_id {
        Some(g) => match cached_channel(app, g, ctx.interaction.channel_id) {
            Some(c) => Some(c),
            None if ctx
                .interaction
                .channel
                .as_ref()
                .is_some_and(|c| matches!(c.kind, ChannelType::PublicThread | ChannelType::PrivateThread | ChannelType::NewsThread)) =>
            {
                bot_core::interaction_helpers::resolve_channel(app, g, ctx.interaction.channel_id).await
            }
            None => None,
        },
        None => None,
    }
    .filter(permissions::is_thread);
    let (Some(guild_id), Some(thread)) = (guild_id, thread) else {
        notice(&ctx, "Button Expired", "This button is no longer active.").await;
        return Ok(());
    };
    if action != "close" || starter.is_empty() {
        notice(&ctx, "Button Expired", "This button is no longer active.").await;
        return Ok(());
    }
    let user = ctx.user().clone();
    let member_perms = ctx.interaction.member.as_ref().and_then(|m| m.permissions);
    let level = match ctx.member() {
        Some(m) => {
            let access = MemberAccess::from_cache(app.discord().map(|d| d.cache.as_ref()), guild_id, m);
            let peek = app.settings().peek_settings(&guild_id.to_string());
            resolve_permission_level(Some(&access), peek.as_deref(), app.config().bot_owner_id.as_deref())
        }
        None => PermissionLevel::Member,
    };
    let allowed = user.id.to_string() == starter || member_perms.is_some_and(|p| js_has(p, Permissions::MANAGE_THREADS)) || level >= PermissionLevel::Moderator;
    if !allowed {
        notice(&ctx, "Not Allowed", "Only the person who started this thread, or a moderator, can close it.").await;
        return Ok(());
    }
    if thread.thread_metadata.is_some_and(|m| m.archived) {
        notice(&ctx, "Already Closed", "This thread is already closed.").await;
        return Ok(());
    }
    let can_lock = bot_permissions(app, PermTarget::Channel(guild_id, thread.id)).is_some_and(|p| js_has(p, Permissions::MANAGE_THREADS));
    let bot = app.discord().map(|d| d.bot_user_id());
    if !can_lock && (thread.owner_id.is_none() || thread.owner_id != bot) {
        notice(&ctx, "Cannot Close", "I need **Manage Threads** here to close this thread.").await;
        return Ok(());
    }

    let display = ctx.member().map(adapters::member_display_name).unwrap_or_else(|| user.name.clone());
    let who = {
        let p = plain_text(&display);
        if p.is_empty() { "a member".to_string() } else { p }
    };
    let update = CreateInteractionResponseMessage::new()
        .content(format!("🔒 Thread closed by {who}."))
        .components(vec![])
        .allowed_mentions(CreateAllowedMentions::new());
    if let Err(e) = ctx.update(update).await {
        if is_interaction_gone(&e) {
            return Ok(());
        }
        bot_util::log_warn!(format!("[auto-threads] {guild_id}: could not update the close message: {e}"));
    }
    let mut body = serde_json::json!({ "archived": true });
    if can_lock {
        body["locked"] = serde_json::json!(true);
    }
    let edited = match app.http() {
        Some(http) => http.edit_thread(thread.id, &body, Some(&format!("Closed by {}", user.id))).await.map(|_| ()).map_err(|e| bot_core::discord::error_message(&e)),
        None => Err("not connected".to_string()),
    };
    if let Err(e) = edited {
        bot_util::log_warn!(format!("[auto-threads] {guild_id}: could not close {}: {e}", thread.id));
        let _ = ctx.follow_up(CreateInteractionResponseFollowup::new().content("I could not close this thread. Check my permissions here.").ephemeral(true)).await;
    }
    Ok(())
}

/// The plugin (`src/plugins/auto-threads.js`).
pub fn plugin() -> Plugin {
    Plugin::new("auto-threads", Some(ModuleKey::Autothreads))
        .hook(
            // HOOK_ORDER.messageCreate: after automod has removed what it removes, before sticky.
            Hook::<MessageCreate>::new("auto-threads", on_message).filter(|ctx| {
                let channel = channel_of(&ctx.app, ctx.data.guild_id, &ctx.data.channel, ctx.data.channel_id);
                may_handle(&ctx.app, &ctx.data.guild_id.to_string(), channel.as_ref())
            }),
        )
        .hook(Hook::<BotMessage>::new("auto-threads", on_bot_message).filter(|ctx| {
            let channel = channel_of(&ctx.app, ctx.data.guild_id, &ctx.data.channel, ctx.data.channel_id);
            may_publish(&ctx.app, &ctx.data.guild_id.to_string(), ctx.data.edited, ctx.data.was_loading, channel.as_ref())
        }))
        .hook(
            // Cleanup runs while the module is off too: a list must not keep a channel that is gone.
            Hook::<ChannelDelete>::new("auto-threads", on_channel_delete)
                .always()
                .filter(|ctx| may_be_listed(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.channel.id.to_string())),
        )
        // Gated by the foundation wrapper (plugin.module): while the module is off the button answers "turned off".
        .button("autothread", handle_close)
}
