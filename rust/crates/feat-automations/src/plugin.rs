//! Port of `src/plugins/automations.js` and the hook bodies and filters of `automationService.js`.
//!
//! Every hook id is `automations`; every hook and the button are gated by the module (none is `always`). No jobs and
//! no reward providers.

use std::sync::Arc;

use bot_core::interaction_helpers::cached_channel;
use bot_core::prelude::*;
use futures::FutureExt;
use serenity::all::{Member, UserId};

use crate::automation_service::{Base, ReactionInfo, SettingsFn, dispatch, on_automation_button, peek_has_trigger, role_diff, strip_fe0f, voice_triggers};

fn settings_fn<E: HookEvent>(ctx: &Arc<HookCtx<E>>) -> SettingsFn {
    let c = ctx.clone();
    Arc::new(move || {
        let c = c.clone();
        async move { c.get_settings().await }.boxed()
    })
}

fn base_of<E: HookEvent>(ctx: &Arc<HookCtx<E>>, trigger: &'static str, guild_id: serenity::all::GuildId) -> Base {
    let mut b = Base::new(trigger, guild_id);
    b.get_settings = Some(settings_fn(ctx));
    b
}

fn bot_id(app: &App) -> Option<UserId> {
    app.discord().map(|d| d.cache.current_user().id)
}

// ── Filters ──────────────────────────────────────────────────────────────────

/// `filterMessageCreate`.
pub fn filter_message_create(ctx: &HookCtx<MessageCreate>) -> bool {
    peek_has_trigger(&ctx.app, &ctx.data.guild_id.to_string(), "MESSAGE_SENT")
}

/// `filterMessageUpdate`.
pub fn filter_message_update(ctx: &HookCtx<MessageUpdate>) -> bool {
    peek_has_trigger(&ctx.app, &ctx.data.guild_id.to_string(), "MESSAGE_EDITED")
}

/// `filterMessageDelete`: a cached member message (no bots, webhooks or system messages).
pub fn filter_message_delete(ctx: &HookCtx<MessageDelete>) -> bool {
    let d = &ctx.data;
    let Some(m) = d.message.as_ref().filter(|_| !d.partial) else { return false };
    !m.author.bot && m.webhook_id.is_none() && !crate::automation_service::is_system_message(m) && peek_has_trigger(&ctx.app, &d.guild_id.to_string(), "MESSAGE_DELETED")
}

/// `filterReactionAdd`.
pub fn filter_reaction_add(ctx: &HookCtx<MessageReactionAdd>) -> bool {
    peek_has_trigger(&ctx.app, &ctx.data.0.guild_id.to_string(), "REACTION_ADDED")
}

/// `filterVoiceState`.
pub fn filter_voice_state(ctx: &HookCtx<VoiceStateUpdate>) -> bool {
    let g = ctx.data.guild_id.to_string();
    ctx.data.change != VoiceChange::State && (peek_has_trigger(&ctx.app, &g, "VOICE_JOINED") || peek_has_trigger(&ctx.app, &g, "VOICE_LEFT"))
}

/// `filterMemberAdd`.
pub fn filter_member_add(ctx: &HookCtx<GuildMemberAdd>) -> bool {
    !ctx.data.member.user.bot && peek_has_trigger(&ctx.app, &ctx.data.guild_id.to_string(), "MEMBER_JOINED")
}

/// `filterMemberUpdate`.
pub fn filter_member_update(ctx: &HookCtx<GuildMemberUpdate>) -> bool {
    let g = ctx.data.guild_id.to_string();
    ctx.data.old_known && !ctx.data.member.user.bot && (peek_has_trigger(&ctx.app, &g, "ROLE_GAINED") || peek_has_trigger(&ctx.app, &g, "ROLE_LOST"))
}

/// `filterThreadCreate`: threads the bot itself creates never trigger anything (the owner check comes first).
pub fn filter_thread_create(ctx: &HookCtx<ThreadCreate>) -> bool {
    let Some(owner) = ctx.data.thread.owner_id else { return false };
    if Some(owner) == bot_id(&ctx.app) {
        return false;
    }
    peek_has_trigger(&ctx.app, &ctx.data.guild_id.to_string(), "THREAD_CREATED")
}

// ── Hook bodies ──────────────────────────────────────────────────────────────

/// `onMessageCreate`.
pub async fn on_message_create(ctx: Arc<HookCtx<MessageCreate>>) -> BotResult {
    let d = &ctx.data;
    if ctx.state.automod_action().is_some_and(|a| a.deleted) || bot_core::deletion_tracker::was_deleted_by_bot(&ctx.app, &d.message.id.to_string()).is_some() {
        return Ok(());
    }
    let mut b = base_of(&ctx, "MESSAGE_SENT", d.guild_id);
    b.member = d.member.clone();
    b.user = Some(d.message.author.clone());
    b.channel = d.channel.clone().or_else(|| cached_channel(&ctx.app, d.guild_id, d.channel_id));
    b.message = Some(d.message.clone());
    dispatch(&ctx.app, "MESSAGE_SENT", b, false).await;
    Ok(())
}

/// `onMessageUpdate`.
pub async fn on_message_update(ctx: Arc<HookCtx<MessageUpdate>>) -> BotResult {
    let d = &ctx.data;
    if bot_core::deletion_tracker::was_deleted_by_bot(&ctx.app, &d.message.id.to_string()).is_some() {
        return Ok(());
    }
    let mut b = base_of(&ctx, "MESSAGE_EDITED", d.guild_id);
    b.member = d.member.clone();
    b.user = Some(d.message.author.clone());
    b.channel = d.channel.clone().or_else(|| cached_channel(&ctx.app, d.guild_id, d.channel_id));
    b.message = Some(d.message.clone());
    b.old_content = if d.old_known { d.old_message.as_ref().map(|m| m.content.clone()) } else { None };
    dispatch(&ctx.app, "MESSAGE_EDITED", b, false).await;
    Ok(())
}

fn cached_member(app: &App, guild_id: serenity::all::GuildId, user: UserId) -> Option<Member> {
    bot_core::interaction_helpers::with_cached_guild(app, guild_id, |g| g.members.get(&user).cloned()).flatten()
}

/// `onMessageDelete`: deletions made by the bot (automod, purge, automations ...) are not member deletions.
pub async fn on_message_delete(ctx: Arc<HookCtx<MessageDelete>>) -> BotResult {
    let d = &ctx.data;
    let Some(message) = d.message.clone() else { return Ok(()) };
    if bot_core::deletion_tracker::was_deleted_by_bot(&ctx.app, &message.id.to_string()).is_some() {
        return Ok(());
    }
    let mut b = base_of(&ctx, "MESSAGE_DELETED", d.guild_id);
    b.member = cached_member(&ctx.app, d.guild_id, message.author.id);
    b.user = Some(message.author.clone());
    b.channel = d.channel.clone().or_else(|| cached_channel(&ctx.app, d.guild_id, d.channel_id));
    b.message = Some(message);
    b.message_gone = true;
    dispatch(&ctx.app, "MESSAGE_DELETED", b, false).await;
    Ok(())
}

/// `onReactionAdd`: the message may be partial; the dispatcher fetches it lazily.
pub async fn on_reaction_add(ctx: Arc<HookCtx<MessageReactionAdd>>) -> BotResult {
    let ev = &ctx.data.0;
    let Some(member) = ev.get_member(&ctx.app).await else { return Ok(()) };
    let mut b = base_of(&ctx, "REACTION_ADDED", ev.guild_id);
    b.member = Some(member);
    b.user = Some(ev.user.clone());
    b.channel = cached_channel(&ctx.app, ev.guild_id, ev.channel_id);
    b.reaction = Some(ReactionInfo { key: strip_fe0f(&ev.emoji_key), display: ev.reaction.emoji.to_string() });
    b.reaction_event = Some(Arc::new(ev.clone()));
    dispatch(&ctx.app, "REACTION_ADDED", b, false).await;
    Ok(())
}

/// `onVoiceState`: a move fires `VOICE_LEFT` (old channel) then `VOICE_JOINED` (new channel).
pub async fn on_voice_state(ctx: Arc<HookCtx<VoiceStateUpdate>>) -> BotResult {
    let d = &ctx.data;
    for trigger in voice_triggers(d.change.as_str()) {
        let channel_id = if *trigger == "VOICE_LEFT" { d.old_state.as_ref().and_then(|s| s.channel_id) } else { d.new_state.channel_id };
        let Some(channel) = channel_id.and_then(|c| cached_channel(&ctx.app, d.guild_id, c)) else { continue };
        let mut b = base_of(&ctx, trigger, d.guild_id);
        b.member = Some(d.member.clone());
        b.user = Some(d.member.user.clone());
        b.voice_channel = Some(channel.clone());
        b.channel = Some(channel);
        dispatch(&ctx.app, trigger, b, false).await;
    }
    Ok(())
}

/// `onMemberAdd`.
pub async fn on_member_add(ctx: Arc<HookCtx<GuildMemberAdd>>) -> BotResult {
    let d = &ctx.data;
    let mut b = base_of(&ctx, "MEMBER_JOINED", d.guild_id);
    b.member = Some(d.member.clone());
    b.user = Some(d.member.user.clone());
    b.captcha_pending = ctx.state.get::<bool>("captchaPending") == Some(true);
    dispatch(&ctx.app, "MEMBER_JOINED", b, false).await;
    Ok(())
}

/// `onMemberUpdate`: without the old member there is no reliable role diff.
pub async fn on_member_update(ctx: Arc<HookCtx<GuildMemberUpdate>>) -> BotResult {
    let d = &ctx.data;
    let Some(old) = d.old_member.as_ref().filter(|_| d.old_known) else { return Ok(()) };
    let g = d.guild_id.to_string();
    let old_roles: Vec<String> = old.roles.iter().map(|r| r.to_string()).collect();
    let new_roles: Vec<String> = d.member.roles.iter().map(|r| r.to_string()).collect();
    let (gained, lost) = role_diff(&old_roles, &new_roles, &g);
    if gained.is_empty() && lost.is_empty() {
        return Ok(());
    }
    let depth = {
        let st = crate::automation_service::store::state(&ctx.app);
        let causes = crate::automation_service::store::lock(&st.causes);
        causes.peek(&g, &d.member.user.id.to_string(), ctx.app.clock().now_ms())
    };
    let make = |trigger: &'static str, roles: Vec<String>| {
        let mut b = base_of(&ctx, trigger, d.guild_id);
        b.member = Some(d.member.clone());
        b.user = Some(d.member.user.clone());
        b.depth = depth;
        b.role_ids = Some(roles);
        b
    };
    if !gained.is_empty() {
        dispatch(&ctx.app, "ROLE_GAINED", make("ROLE_GAINED", gained), false).await;
    }
    if !lost.is_empty() {
        dispatch(&ctx.app, "ROLE_LOST", make("ROLE_LOST", lost), false).await;
    }
    Ok(())
}

/// `onThreadCreate`: the owner is resolved (cache, then REST); bots are ignored.
pub async fn on_thread_create(ctx: Arc<HookCtx<ThreadCreate>>) -> BotResult {
    let d = &ctx.data;
    let Some(owner) = d.thread.owner_id else { return Ok(()) };
    let member = match cached_member(&ctx.app, d.guild_id, owner) {
        Some(m) => Some(m),
        None => match ctx.app.http() {
            Some(http) => http.get_member(d.guild_id, owner).await.ok(),
            None => None,
        },
    };
    let Some(member) = member.filter(|m| !m.user.bot) else { return Ok(()) };
    let mut b = base_of(&ctx, "THREAD_CREATED", d.guild_id);
    b.user = Some(member.user.clone());
    b.member = Some(member);
    b.channel = Some(d.thread.clone());
    b.thread = Some(d.thread.clone());
    dispatch(&ctx.app, "THREAD_CREATED", b, false).await;
    Ok(())
}

/// The plugin (`src/plugins/automations.js`).
pub fn plugin() -> Plugin {
    Plugin::new("automations", Some(ModuleKey::Automations))
        .hook(Hook::<MessageCreate>::new("automations", on_message_create).filter(filter_message_create))
        .hook(Hook::<MessageUpdate>::new("automations", on_message_update).filter(filter_message_update))
        .hook(Hook::<MessageDelete>::new("automations", on_message_delete).filter(filter_message_delete))
        .hook(Hook::<MessageReactionAdd>::new("automations", on_reaction_add).filter(filter_reaction_add))
        .hook(Hook::<VoiceStateUpdate>::new("automations", on_voice_state).filter(filter_voice_state))
        .hook(Hook::<GuildMemberAdd>::new("automations", on_member_add).filter(filter_member_add))
        .hook(Hook::<GuildMemberUpdate>::new("automations", on_member_update).filter(filter_member_update))
        .hook(Hook::<ThreadCreate>::new("automations", on_thread_create).filter(filter_thread_create))
        .button("automation", on_automation_button)
}
