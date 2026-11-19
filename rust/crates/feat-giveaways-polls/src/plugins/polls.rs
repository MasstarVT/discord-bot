//! Port of `src/plugins/polls.js`: the delete hooks that end a poll whose message or channel vanished, the persistent
//! vote button / select and create modal, and the `poll.end` job. Declaration only: the logic lives in
//! [`crate::poll_service`].

use std::sync::Arc;

use bot_core::prelude::*;

use crate::guild_view::bot_id;
use crate::poll_service::{
    handle_create_modal, handle_end_job, handle_vote_button, handle_vote_select, on_channel_delete, on_message_delete, on_message_delete_bulk,
};

/// `isOwnOrUnknown(ctx)`: poll messages are bot-authored; a partial (uncached) message is looked up by id.
pub fn is_own_or_unknown(app: &App, partial: bool, message: Option<&serenity::all::Message>) -> bool {
    partial || message.is_some_and(|m| bot_id(app).is_some_and(|b| b == m.author.id))
}

async fn on_delete(ctx: Arc<HookCtx<MessageDelete>>) -> BotResult {
    on_message_delete(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.message_id.to_string()).await
}

async fn on_bulk(ctx: Arc<HookCtx<MessageDeleteBulk>>) -> BotResult {
    let ids: Vec<String> = ctx.data.message_ids.iter().map(|m| m.to_string()).collect();
    on_message_delete_bulk(&ctx.app, &ctx.data.guild_id.to_string(), &ids).await
}

async fn on_channel(ctx: Arc<HookCtx<ChannelDelete>>) -> BotResult {
    on_channel_delete(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.channel.id.to_string()).await
}

/// The plugin (`src/plugins/polls.js`).
pub fn plugin() -> Plugin {
    Plugin::new("polls", Some(ModuleKey::Polls))
        // `always`: ending a poll whose message or channel vanished is cleanup and must happen even while the module
        // is off, otherwise dead polls would keep holding the 50-poll cap.
        .hook(
            Hook::<MessageDelete>::new("polls", on_delete)
                .always()
                .filter(|ctx| is_own_or_unknown(&ctx.app, ctx.data.partial, ctx.data.message.as_ref())),
        )
        .hook(Hook::<MessageDeleteBulk>::new("polls", on_bulk).always())
        .hook(Hook::<ChannelDelete>::new("polls", on_channel).always())
        // Module gating is applied by the registry (plugin module 'polls').
        .button("poll", handle_vote_button)
        .select("poll-select", handle_vote_select)
        .modal("poll-modal", handle_create_modal)
        .job(JobType::PollEnd, job_handler(handle_end_job))
}
