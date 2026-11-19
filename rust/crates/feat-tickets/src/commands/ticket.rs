//! Port of `src/commands/community/ticket.js`: `/ticket` (the per-ticket commands: claim, close, reopen, delete, add,
//! remove, transcript, rename). Thin: it defers, finds the ticket of the current channel and calls the service. Every
//! reply to the invoker is ephemeral; the public effects are notices the service posts itself.

use bot_core::command::CommandOption as Opt;
use bot_core::interaction_helpers::{ConfirmOptions, Payload, await_confirm, resolve_channel, safe_defer, safe_reply};
use bot_core::prelude::*;

use crate::ticket_service::{
    Action, ActionCtx, Actor, Source, add_member, can_perform, claim_ticket, close_ticket, delete_ticket, error_payload, error_payload_code,
    get_ticket_by_channel, is_ticket_staff, load_settings, notes_block, remove_member, rename_ticket, reopen_ticket, transcript_for_actor,
};

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("ticket", "Manage the ticket in this channel (MEE6 /ticket-claim, -close, -reopen, -delete).")
        .option(Opt::sub_command("claim", "Claim this ticket so the team knows you handle it (MEE6 /ticket-claim)."))
        .option(
            Opt::sub_command("close", "Close this ticket; the opener can still read it (MEE6 /ticket-close).")
                .option(Opt::string("reason", "Why it is being closed (shown in the ticket)").max_length(500)),
        )
        .option(Opt::sub_command("reopen", "Reopen this closed ticket (MEE6 /ticket-reopen)."))
        .option(Opt::sub_command("delete", "Save the transcript and delete this ticket channel (MEE6 /ticket-delete)."))
        .option(Opt::sub_command("add", "Give a member access to this ticket.").option(Opt::user("user", "Member to add").required(true)))
        .option(Opt::sub_command("remove", "Remove a member you added from this ticket.").option(Opt::user("user", "Member to remove").required(true)))
        .option(Opt::sub_command("transcript", "Get a transcript file of this ticket (only you see it)."))
        .option(
            Opt::sub_command("rename", "Rename this ticket channel (Discord allows 2 renames per 10 minutes).")
                .option(Opt::string("name", "New channel name").required(true).min_length(1).max_length(90)),
        )
}

/// The command (`module 'tickets'`, gated, MEMBER: openers must be able to close their own ticket; staff checks are in
/// the service).
pub fn command() -> CommandDef {
    CommandDef::new("community", data(), Some(ModuleKey::Tickets), execute).permission_level(PermissionLevel::Member)
}

fn success_text(sub: &str) -> &'static str {
    match sub {
        "claim" => "Ticket claimed.",
        "close" => "Ticket closed.",
        "reopen" => "Ticket reopened.",
        "add" => "Member added.",
        "remove" => "Member removed.",
        _ => "Channel renamed.",
    }
}

async fn execute(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    // Ephemeral also on follow-ups: after the confirm dialog a reply is a follow-up, and those are public unless flagged.
    let reply = |p: Payload| {
        let r = ctx.responder.clone();
        async move {
            safe_reply(&r, p.ephemeral(true)).await;
        }
    };
    let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/ticket outside a guild"))?;
    let g = guild_id.to_string();
    let sub = ctx.subcommand().unwrap_or_default();

    let Some(ticket) = get_ticket_by_channel(ctx.app.db(), &g, &ctx.channel_id().to_string()).await? else {
        reply(error_payload_code("NOT_TICKET")).await;
        return Ok(());
    };
    if ticket.status() == "DELETED" {
        reply(error_payload_code("TICKET_DELETED")).await;
        return Ok(());
    }
    let settings = load_settings(&ctx.app, &g).await;
    let Some(member) = ctx.member() else {
        reply(error_payload_code("NOT_TICKET")).await;
        return Ok(());
    };
    let actor = Actor::of(&ctx.app, guild_id, member);
    let actx = ActionCtx { guild_id, ticket_id: ticket.ticket.id.clone(), actor: actor.clone(), settings: settings.clone(), source: Source::Command };

    if sub == "transcript" {
        // `interaction.channel`: the cache, else a REST fetch (which caches it, as discord.js does).
        let Some(channel) = resolve_channel(&ctx.app, guild_id, ctx.channel_id()).await else {
            reply(error_payload_code("NOT_TICKET")).await;
            return Ok(());
        };
        match transcript_for_actor(&ctx.app, guild_id, &ticket, &channel, settings.as_deref(), &actor).await {
            Ok(r) => reply(r.payload).await,
            Err(f) => reply(error_payload(&f)).await,
        }
        return Ok(());
    }

    if sub == "delete" {
        let staff = is_ticket_staff(&ctx.app, Some(&actor), ticket.panel.as_ref(), settings.as_deref());
        if let Err(f) = can_perform(Action::Delete, ctx.user().id.to_string() == ticket.ticket.owner_id, staff) {
            reply(error_payload(&f)).await;
            return Ok(());
        }
        if ticket.status() != "CLOSED" {
            let ok = await_confirm(
                &ctx,
                ConfirmOptions {
                    title: Some("Delete Ticket?".into()),
                    description: Some("This ticket is still open. Its transcript will be saved (if transcripts are set up) and the channel deleted.".into()),
                    confirm_label: "Delete".into(),
                    ephemeral: true,
                    ..Default::default()
                },
            )
            .await;
            if !ok {
                reply(Payload::embed(info_embed("Cancelled", Some("The ticket was not deleted."), None)).components(Vec::new())).await;
                return Ok(());
            }
        }
        let r = ctx.responder.clone();
        let on_saved: crate::ticket_service::OnSaved = Box::new(move |text: String| {
            Box::pin(async move {
                safe_reply(&r, Payload::embed(success_embed("Deleting Ticket", Some(&text), None)).components(Vec::new()).ephemeral(true)).await;
            })
        });
        // On success the channel that holds this interaction is gone: touching it would only produce 10003 errors.
        if let Err(f) = delete_ticket(&ctx.app, actx, Some(on_saved)).await {
            reply(error_payload(&f).components(Vec::new())).await;
        }
        return Ok(());
    }

    let result = match sub.as_str() {
        "claim" => claim_ticket(&ctx.app, actx).await,
        "close" => {
            let reason = ctx.get_string("reason").map(|r| bot_util::js::trim(&r).to_string()).filter(|r| !r.is_empty());
            close_ticket(&ctx.app, actx, reason).await
        }
        "reopen" => reopen_ticket(&ctx.app, actx).await,
        "add" => {
            let user = ctx.get_user("user").ok_or_else(|| anyhow::anyhow!("Required option \"user\" not found."))?;
            let is_member = ctx.get_member("user").is_some();
            add_member(&ctx.app, actx, user, is_member).await
        }
        "remove" => {
            let user = ctx.get_user_id("user").ok_or_else(|| anyhow::anyhow!("Required option \"user\" not found."))?;
            remove_member(&ctx.app, actx, user).await
        }
        "rename" => {
            let name = ctx.get_string("name").ok_or_else(|| anyhow::anyhow!("Required option \"name\" not found."))?;
            rename_ticket(&ctx.app, actx, name).await
        }
        _ => return Ok(()),
    };
    match result {
        Ok(done) => reply(Payload::embed(success_embed("Done", Some(&format!("{}{}", success_text(&sub), notes_block(&done.notes))), None)).no_mentions()).await,
        Err(f) => reply(error_payload(&f)).await,
    }
    Ok(())
}
