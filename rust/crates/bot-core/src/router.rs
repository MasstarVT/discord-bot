//! Port of `src/handlers/interactionRouter.js`: routes every interaction.
//!
//! Slash commands and autocompletes pass one gate ([`check_command_access`]): guild guard (fails closed unless
//! `allowInDMs`) -> module toggle (gated commands of disabled modules refused; unreadable settings answer "Temporarily
//! Unavailable") -> permission level (`subcommandPermissions['group sub']` beats `['sub']` beats `permissionLevel`).
//! Autocomplete is gated exactly like execute and answers an empty list on any refusal. Components route by prefix to
//! the handler maps (first registration wins); a click on a `collector` prefix goes to the app's
//! [`crate::collectors::CollectorHub`] and gets "This menu has expired." after 2.5 s if nothing acknowledged it.
//! Every handler is guarded: an error (or panic) is logged and answered with an error embed, never propagated.

use std::time::Duration;

use bot_config::{CustomIdKind, PermissionLevel, parse_custom_id, prefix_info};
use serde_json::json;
use serenity::all::{CommandInteraction, ComponentInteraction, ComponentInteractionDataKind, Interaction, ModalInteraction};
use serenity::builder::{CreateEmbed, CreateInteractionResponseFollowup, CreateInteractionResponseMessage, EditInteractionResponse};

use crate::command::CommandDef;
use crate::context::App;
use crate::discord::GuildState;
use crate::embed_builder::{error_embed, warn_embed};
use crate::error::{log_arg, start_guarded};
use crate::interaction::{AutocompleteCtx, CommandCtx, ComponentCtx, ComponentKind, ModalCtx, Responder, member_access, subcommand_path};
use crate::permission_checker::resolve_permission_level;
use crate::settings::{Settings, is_module_enabled};

/// How long the router may wait for a cold settings load before a slash command.
pub const ROUTER_SETTINGS_TIMEOUT_MS: u64 = 1500;
/// The tighter budget before an autocomplete.
pub const AUTOCOMPLETE_SETTINGS_TIMEOUT_MS: u64 = 1000;
/// How long a live collector gets to answer a click on a collector prefix.
pub const COLLECTOR_GRACE_MS: u64 = 2500;

/// The no-handler texts (`NO_HANDLER_TEXT`).
pub fn no_handler_text(kind: ComponentKind) -> &'static str {
    match kind {
        ComponentKind::Button => "This button is no longer active.",
        ComponentKind::Select => "This menu is no longer active.",
        ComponentKind::Modal => "This form has expired. Please run the command again.",
    }
}

/// The handler error embeds (`ERROR_EMBED`).
pub fn error_embed_for(kind: ComponentKind) -> CreateEmbed {
    let (title, description) = match kind {
        ComponentKind::Button => ("Button Error", "Something went wrong processing this button."),
        ComponentKind::Select => ("Menu Error", "Something went wrong processing this selection."),
        ComponentKind::Modal => ("Modal Error", "Something went wrong processing your submission."),
    };
    error_embed(title, Some(description), None)
}

/// The router's own `safeReply(interaction, { embeds: [embed] })` (interactionRouter.js, silent on errors; the
/// helper of the same name for features is [`crate::interaction_helpers::safe_reply`]): works whether or not the interaction was acknowledged. Only a
/// `deferReply` placeholder (deferred, not replied, a boolean `ephemeral`) is edited; after `deferUpdate`/`update`
/// the clicked panel, and after a reply that reply, are never overwritten: a follow-up is sent instead. Errors (an
/// expired interaction) are ignored.
pub async fn safe_reply(responder: &Responder, embed: CreateEmbed) {
    let s = responder.state();
    let _ = if !s.replied && !s.deferred {
        responder.reply(CreateInteractionResponseMessage::new().embed(embed).ephemeral(true)).await
    } else if s.deferred && !s.replied && s.ephemeral.is_some() {
        responder.edit_reply(EditInteractionResponse::new().embed(embed)).await.map(|_| ())
    } else {
        responder.follow_up(CreateInteractionResponseFollowup::new().embed(embed).ephemeral(true)).await.map(|_| ())
    };
}

/// A refused command: the embed a slash command answers with (autocomplete answers `[]`).
#[derive(Debug, Clone)]
pub struct AccessDenied {
    pub embed: Box<CreateEmbed>,
}

fn denied(embed: CreateEmbed) -> Result<Option<Settings>, AccessDenied> {
    Err(AccessDenied { embed: Box::new(embed) })
}

/// `checkCommandAccess(interaction, command, settingsTimeoutMs)`: the one gate every command invocation passes. Sends
/// nothing itself. `Ok(settings)` when it may run (the settings it loaded, if it needed them).
pub async fn check_command_access(app: &App, command: &CommandDef, interaction: &CommandInteraction, settings_timeout_ms: u64) -> Result<Option<Settings>, AccessDenied> {
    let guild_id = interaction.guild_id;
    let in_guild = guild_id.is_some();
    let in_cached_guild = guild_id.is_some_and(|g| app.guild_directory().guild_state(&g.to_string()) != GuildState::Missing);

    // Guild guard. Fails closed: a permission level can only be resolved from a cached guild member.
    if !command.allow_in_dms {
        if !in_guild {
            return denied(error_embed("Server Only", Some("This command can only be used in a server."), None));
        }
        if !in_cached_guild {
            return denied(error_embed(
                "Bot Not In Server",
                Some("I need to be a member of this server to run commands here. Ask an admin to invite me with the bot scope."),
                None,
            ));
        }
    }

    let (group, sub) = subcommand_path(&interaction.data.options);
    let required = command.required_level(group.as_deref(), sub.as_deref());

    if required > PermissionLevel::Member && !in_cached_guild {
        return denied(error_embed("Server Only", Some("This command can only be used in a server where I am a member."), None));
    }

    let gated = command.module.is_some() && command.gated;
    let mut settings = None;
    if let Some(g) = guild_id
        && (required > PermissionLevel::Member || gated)
    {
        let gid = g.to_string();
        settings = app.settings().peek_settings(&gid);
        if settings.is_none() {
            let a = app.clone();
            let load = async move { Ok::<_, std::convert::Infallible>(a.settings().get_settings_safe(&gid).await) };
            settings = bot_util::async_util::with_timeout(load, settings_timeout_ms, "router settings").await.ok().flatten();
        }
    }

    if gated && guild_id.is_some() {
        let Some(s) = &settings else {
            return denied(error_embed("Temporarily Unavailable", Some("Server settings could not be loaded. Please try again in a moment."), None));
        };
        if !is_module_enabled(Some(s), command.module) {
            let module = command.module.map(|m| m.as_str()).unwrap_or_default();
            let label = command.module.map(|m| m.info().label).unwrap_or(module);
            return denied(warn_embed(
                "Module Disabled",
                Some(&format!("The **{label}** module is turned off on this server. An admin can enable it with `/module enable {module}`.")),
                None,
            ));
        }
    }

    if required > PermissionLevel::Member {
        // Without settings a moderator role cannot be proven, but Discord permissions still count.
        let access = member_access(app, guild_id, interaction.member.as_deref());
        let level = resolve_permission_level(access.as_ref(), settings.as_deref(), app.config().bot_owner_id.as_deref());
        if level < required {
            return denied(error_embed("Insufficient Permissions", Some("You don't have permission to use this command."), None));
        }
    }

    Ok(settings)
}

/// `routeInteraction(interaction, client)`: routes any interaction. Never fails.
pub async fn route_interaction(app: &App, interaction: Interaction) {
    match interaction {
        Interaction::Command(ci) => route_command(app, ci).await,
        Interaction::Autocomplete(ci) => route_autocomplete(app, ci).await,
        Interaction::Component(ci) => {
            let kind = match ci.data.kind {
                ComponentInteractionDataKind::Button => ComponentKind::Button,
                _ => ComponentKind::Select,
            };
            route_component(app, ci, kind).await
        }
        Interaction::Modal(mi) => route_modal(app, mi).await,
        _ => {}
    }
}

fn responder_for(app: &App, id: serenity::all::InteractionId, token: &str) -> Responder {
    Responder::new(app.interaction_backend(), id, token)
}

/// A slash command.
pub async fn route_command(app: &App, interaction: CommandInteraction) {
    let responder = responder_for(app, interaction.id, &interaction.token);
    let Some(command) = app.commands().get(&interaction.data.name) else {
        safe_reply(&responder, error_embed("Unknown Command", Some("This command no longer exists. Try `/` to see available commands."), None)).await;
        return;
    };
    let settings = match check_command_access(app, &command, &interaction, ROUTER_SETTINGS_TIMEOUT_MS).await {
        Ok(s) => s,
        Err(d) => {
            safe_reply(&responder, *d.embed).await;
            return;
        }
    };
    let name = interaction.data.name.clone();
    let guild = interaction.guild_id.map(|g| g.to_string());
    let user = interaction.user.id.to_string();
    let ctx = CommandCtx::new(app.clone(), interaction, responder.clone(), settings);
    if let Err(e) = start_guarded(|| (command.execute)(ctx)).await {
        bot_util::log_error!(format!("Error in command \"{name}\""), log_arg(&e), json!({ "guildId": guild, "userId": user }));
        safe_reply(&responder, error_embed("Command Error", Some("Something went wrong while running this command."), None)).await;
    }
}

/// An autocomplete: gated like execute; anything short of a clean pass answers no suggestions.
pub async fn route_autocomplete(app: &App, interaction: CommandInteraction) {
    let responder = responder_for(app, interaction.id, &interaction.token);
    let Some(command) = app.commands().get(&interaction.data.name) else { return };
    let Some(autocomplete) = command.autocomplete.clone() else { return };
    if check_command_access(app, &command, &interaction, AUTOCOMPLETE_SETTINGS_TIMEOUT_MS).await.is_err() {
        let _ = responder.respond(Vec::new()).await;
        return;
    }
    let name = interaction.data.name.clone();
    let ctx = AutocompleteCtx { app: app.clone(), interaction, responder };
    if let Err(e) = start_guarded(|| autocomplete(ctx)).await {
        bot_util::log_error!(format!("Autocomplete error for \"{name}\""), log_arg(&e));
    }
}

fn capitalised(kind: ComponentKind) -> &'static str {
    match kind {
        ComponentKind::Button => "Button",
        ComponentKind::Select => "Select",
        ComponentKind::Modal => "Modal",
    }
}

/// A button or select menu (every kind).
pub async fn route_component(app: &App, interaction: ComponentInteraction, kind: ComponentKind) {
    let responder = responder_for(app, interaction.id, &interaction.token);
    let prefix = parse_custom_id(&interaction.data.custom_id).prefix.to_string();
    let handler = app.components().message_map(kind).and_then(|m| m.get(&prefix));
    let ctx = ComponentCtx { app: app.clone(), interaction, responder: responder.clone(), kind };

    let Some(handler) = handler else {
        if prefix_info(&prefix).is_some_and(|i| i.kind == CustomIdKind::Collector) {
            // The live collector gets the first chance; only a click nobody answered gets the expiry notice.
            app.collectors().offer(ctx);
            let r = responder.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(COLLECTOR_GRACE_MS)).await;
                if !r.is_acknowledged() {
                    let _ = r.reply(CreateInteractionResponseMessage::new().content("This menu has expired.").ephemeral(true)).await;
                }
            });
            return;
        }
        let _ = responder.reply(CreateInteractionResponseMessage::new().content(no_handler_text(kind)).ephemeral(true)).await;
        return;
    };

    if let Err(e) = start_guarded(|| handler(ctx)).await {
        bot_util::log_error!(format!("{} handler error for prefix \"{prefix}\"", capitalised(kind)), log_arg(&e));
        safe_reply(&responder, error_embed_for(kind)).await;
    }
}

/// A modal submit.
pub async fn route_modal(app: &App, interaction: ModalInteraction) {
    let responder = responder_for(app, interaction.id, &interaction.token);
    let prefix = parse_custom_id(&interaction.data.custom_id).prefix.to_string();
    let Some(handler) = app.components().modals.get(&prefix) else {
        let _ = responder.reply(CreateInteractionResponseMessage::new().content(no_handler_text(ComponentKind::Modal)).ephemeral(true)).await;
        return;
    };
    let ctx = ModalCtx { app: app.clone(), interaction, responder: responder.clone() };
    if let Err(e) = start_guarded(|| handler(ctx)).await {
        bot_util::log_error!(format!("{} handler error for prefix \"{prefix}\"", capitalised(ComponentKind::Modal)), log_arg(&e));
        safe_reply(&responder, error_embed_for(ComponentKind::Modal)).await;
    }
}
