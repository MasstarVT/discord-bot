//! Port of `src/plugins/starboard.js`: the starboard plugin declaration. Declaration only: the hook bodies, filters
//! and the sync engine live in [`crate::starboard_service`].
//!
//! Every hook id is exactly `starboard`. The reaction and edit hooks are gated by the module through the runner. The
//! delete hooks are `always`: while the module is off, a deleted source still removes its repost (privacy), a purged
//! repost is still forgotten, and a deleted board channel still disables its board. The jump button is a Link button
//! with no customId, and there are no jobs.

use std::sync::Arc;

use bot_core::prelude::*;

use crate::starboard_service::{self as sb, ReactionEventKind, ReactionInput};

/// The message's author when known without a fetch (discord.js: `ctx.message.author` of a non-partial message).
/// serenity sends `message_author_id` on add events only; a message the cache holds covers removals too (an author
/// never changes, unlike the cached reactions).
pub fn known_author(app: &App, e: &ReactionEvent) -> Option<String> {
    e.reaction.message_author_id.map(|u| u.to_string()).or_else(|| {
        let d = app.discord()?;
        d.cache.message(e.channel_id, e.message_id).map(|m| m.author.id.to_string())
    })
}

pub fn reaction_input(app: &App, kind: ReactionEventKind, e: &ReactionEvent) -> ReactionInput {
    ReactionInput {
        kind,
        guild_id: e.guild_id.to_string(),
        channel_id: e.channel_id.to_string(),
        message_id: e.message_id.to_string(),
        emoji_key: Some(e.emoji_key.clone()),
        emoji: Some(e.reaction.emoji.clone()),
        user_id: Some(e.user.id.to_string()),
        message_author_id: known_author(app, e),
    }
}

async fn on_add(ctx: Arc<HookCtx<MessageReactionAdd>>) -> BotResult {
    sb::on_reaction_event(&ctx.app, reaction_input(&ctx.app, ReactionEventKind::Add, &ctx.data.0));
    Ok(())
}

async fn on_remove(ctx: Arc<HookCtx<MessageReactionRemove>>) -> BotResult {
    sb::on_reaction_event(&ctx.app, reaction_input(&ctx.app, ReactionEventKind::Remove, &ctx.data.0));
    Ok(())
}

async fn on_remove_all(ctx: Arc<HookCtx<MessageReactionRemoveAll>>) -> BotResult {
    let d = &ctx.data;
    sb::on_reaction_event(
        &ctx.app,
        ReactionInput {
            kind: ReactionEventKind::RemoveAll,
            guild_id: d.guild_id.to_string(),
            channel_id: d.channel_id.to_string(),
            message_id: d.message_id.to_string(),
            emoji_key: None,
            emoji: None,
            user_id: None,
            message_author_id: None,
        },
    );
    Ok(())
}

async fn on_remove_emoji(ctx: Arc<HookCtx<MessageReactionRemoveEmoji>>) -> BotResult {
    let d = &ctx.data;
    sb::on_reaction_event(
        &ctx.app,
        ReactionInput {
            kind: ReactionEventKind::RemoveEmoji,
            guild_id: d.guild_id.to_string(),
            channel_id: d.channel_id.to_string(),
            message_id: d.message_id.to_string(),
            emoji_key: Some(d.emoji_key.clone()),
            emoji: Some(d.reaction.emoji.clone()),
            user_id: None,
            message_author_id: None,
        },
    );
    Ok(())
}

async fn on_update(ctx: Arc<HookCtx<MessageUpdate>>) -> BotResult {
    sb::on_message_update(&ctx.app, &ctx.data.guild_id.to_string(), ctx.data.message.clone());
    Ok(())
}

async fn on_delete(ctx: Arc<HookCtx<MessageDelete>>) -> BotResult {
    let d = &ctx.data;
    sb::on_message_delete(&ctx.app, &d.guild_id.to_string(), &d.channel_id.to_string(), &d.message_id.to_string());
    Ok(())
}

async fn on_delete_bulk(ctx: Arc<HookCtx<MessageDeleteBulk>>) -> BotResult {
    let d = &ctx.data;
    sb::on_message_delete_bulk(&ctx.app, &d.guild_id.to_string(), &d.channel_id.to_string(), d.message_ids.iter().map(|m| m.to_string()).collect());
    Ok(())
}

async fn on_channel_delete(ctx: Arc<HookCtx<ChannelDelete>>) -> BotResult {
    sb::on_channel_delete(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.channel.id.to_string()).await;
    Ok(())
}

/// The plugin (`src/plugins/starboard.js`).
pub fn plugin() -> Plugin {
    Plugin::new("starboard", Some(ModuleKey::Starboard))
        .hook(
            Hook::<MessageReactionAdd>::new("starboard", on_add)
                .filter(|ctx| sb::reaction_filter(&ctx.app, &ctx.data.0.guild_id.to_string(), Some(&ctx.data.0.emoji_key))),
        )
        .hook(
            Hook::<MessageReactionRemove>::new("starboard", on_remove)
                .filter(|ctx| sb::reaction_filter(&ctx.app, &ctx.data.0.guild_id.to_string(), Some(&ctx.data.0.emoji_key))),
        )
        .hook(
            Hook::<MessageReactionRemoveAll>::new("starboard", on_remove_all)
                .filter(|ctx| sb::reaction_filter(&ctx.app, &ctx.data.guild_id.to_string(), None)),
        )
        .hook(
            Hook::<MessageReactionRemoveEmoji>::new("starboard", on_remove_emoji)
                .filter(|ctx| sb::reaction_filter(&ctx.app, &ctx.data.guild_id.to_string(), Some(&ctx.data.emoji_key))),
        )
        .hook(Hook::<MessageUpdate>::new("starboard", on_update).filter(|ctx| sb::guild_may_have_boards(&ctx.app, &ctx.data.guild_id.to_string())))
        .hook(
            Hook::<MessageDelete>::new("starboard", on_delete)
                .always()
                .filter(|ctx| sb::guild_may_have_boards(&ctx.app, &ctx.data.guild_id.to_string())),
        )
        .hook(
            Hook::<MessageDeleteBulk>::new("starboard", on_delete_bulk)
                .always()
                .filter(|ctx| sb::guild_may_have_boards(&ctx.app, &ctx.data.guild_id.to_string())),
        )
        .hook(
            Hook::<ChannelDelete>::new("starboard", on_channel_delete)
                .always()
                .filter(|ctx| sb::guild_may_have_boards(&ctx.app, &ctx.data.guild_id.to_string())),
        )
}
