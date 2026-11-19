//! Port of `src/plugins/tickets.js`: the channelDelete cleanup hook and the persistent panel components. The logic lives
//! in [`crate::ticket_service`].

use std::sync::Arc;

use bot_core::prelude::*;
use serenity::all::ChannelType;

use crate::ticket_service::{handle_ticket_button, handle_ticket_modal, handle_ticket_select, manager_sync_job, on_channel_delete};

/// `RELEVANT`: text, announcement and category channels.
pub fn relevant(kind: ChannelType) -> bool {
    matches!(kind, ChannelType::Text | ChannelType::News | ChannelType::Category)
}

async fn channel_deleted(ctx: Arc<HookCtx<ChannelDelete>>) -> BotResult {
    let ch = &ctx.data.channel;
    on_channel_delete(&ctx.app, &ctx.data.guild_id.to_string(), &ch.id.to_string(), ch.kind == ChannelType::Category).await;
    Ok(())
}

/// The plugin (`src/plugins/tickets.js`).
pub fn plugin() -> Plugin {
    Plugin::new("tickets", Some(ModuleKey::Tickets))
        // `always`: marking tickets DELETED and clearing dead references is cleanup and must happen while the module is
        // off as well, otherwise dead rows would keep counting against maxOpenPerUser.
        .hook(Hook::<ChannelDelete>::new("tickets", channel_deleted).always().filter(|ctx| relevant(ctx.data.channel.kind)))
        // Gated by the foundation wrapper: while the module is off the buttons answer that the module is turned off.
        .button("ticket", handle_ticket_button)
        .select("ticket-select", handle_ticket_select)
        // NOT wrapped: the template form must work while the module is off; the reason form checks the module itself
        // and the template form checks the Admin level itself.
        .modal("ticket-modal", handle_ticket_modal)
        .component_module(ModuleRef::Ungated)
        .job(JobType::TicketManagersync, job_handler(manager_sync_job))
}
