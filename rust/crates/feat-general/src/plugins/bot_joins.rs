//! Port of `src/plugins/bot-joins.js`: records who added the bot to each server, which the web dashboard shows the
//! bot owner. Declaration only; the work is in [`crate::bot_join_service`].

use bot_core::prelude::*;

use crate::bot_join_service::record_bot_join;

/// The plugin (`id 'bot-joins'`, `module: null`): one `guildReady` hook with a 15 s timeout.
pub fn plugin() -> Plugin {
    Plugin::new("bot-joins", None).hook(
        Hook::<GuildReady>::new("bot-joins", |ctx| async move {
            record_bot_join(&ctx.app, ctx.data.guild_id, ctx.data.reason).await;
            Ok(())
        })
        .timeout_ms(15_000),
    )
}
