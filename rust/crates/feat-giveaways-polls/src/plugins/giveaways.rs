//! Port of `src/plugins/giveaways.js`: the `messageDelete` / `messageDeleteBulk` hooks that draft a giveaway whose
//! message was deleted, the persistent Participate button and the `giveaway.end` job. Declaration only: the logic
//! lives in [`crate::giveaway_service`].

use std::sync::Arc;

use bot_core::prelude::*;

use crate::giveaway_service::{handle_end_job, handle_giveaway_bulk_delete, handle_giveaway_message_delete, handle_join_button};
use crate::guild_view::bot_id;

/// The `messageDelete` filter: giveaway messages are bot-authored; a partial (uncached) message has no author and is
/// looked up by id.
pub fn is_own_or_unknown(app: &App, message: Option<&serenity::all::Message>) -> bool {
    match message {
        None => true,
        Some(m) => bot_id(app).is_some_and(|b| b == m.author.id),
    }
}

async fn on_delete(ctx: Arc<HookCtx<MessageDelete>>) -> BotResult {
    handle_giveaway_message_delete(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.message_id.to_string()).await
}

async fn on_bulk_delete(ctx: Arc<HookCtx<MessageDeleteBulk>>) -> BotResult {
    let ids: Vec<String> = ctx.data.message_ids.iter().map(|m| m.to_string()).collect();
    handle_giveaway_bulk_delete(&ctx.app, &ctx.data.guild_id.to_string(), &ids).await
}

/// The plugin (`src/plugins/giveaways.js`).
pub fn plugin() -> Plugin {
    Plugin::new("giveaways", Some(ModuleKey::Giveaways))
        .hook(
            // Drafts are tracked even while the module is off.
            Hook::<MessageDelete>::new("giveaways", on_delete)
                .always()
                .filter(|ctx| is_own_or_unknown(&ctx.app, ctx.data.message.as_ref()))
                .timeout_ms(10_000),
        )
        .hook(
            // A purge fires no messageDelete: one batched lookup drafts every giveaway among the deleted messages.
            Hook::<MessageDeleteBulk>::new("giveaways", on_bulk_delete).always().timeout_ms(10_000),
        )
        .button("giveaway", handle_join_button)
        .job(JobType::GiveawayEnd, job_handler(handle_end_job))
}
