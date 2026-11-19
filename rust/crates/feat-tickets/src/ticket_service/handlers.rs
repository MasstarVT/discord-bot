//! The persistent component handlers (§6.7.1, §6.8.6, §6.11): the Open button and the dropdown, the reason form, the
//! template form of `/ticket-panel message`, and the lifecycle buttons under the intro and the close notice.

use bot_config::PermissionLevel;
use bot_core::adapters::{placeholder_channel, placeholder_guild, placeholder_member, placeholder_user};
use bot_core::custom_embed::{RenderOptions, normalize_template, parse_template_modal, render_template};
use bot_core::embed_builder::{error_embed, success_embed, warn_embed};
use bot_core::interaction::{ComponentCtx, ComponentKind, InteractionError, ModalCtx, Responder};
use bot_core::interaction_helpers::{Payload, cached_guild, is_interaction_gone, pingable_role_ids, safe_defer, safe_reply};
use bot_core::settings::{Settings, is_module_enabled};
use bot_core::App;
use bot_config::ModuleKey;
use bot_db::sqlx::{self, AssertSqlSafe};
use bot_db::{SQL_NOW, TicketPanelStyle};
use bot_util::placeholder_parser::{RenderContext, VarValue, render_placeholders_detailed};
use bot_util::text::clamp_text;
use serde_json::{Value, json};
use serenity::all::{GuildId, Message, RoleId};
use serenity::builder::{CreateAllowedMentions, EditInteractionResponse};

use super::components::{build_reason_modal, disable_buttons_json, message_rows_json, rows_from_json};
use super::db::{get_panel_for_open, get_ticket_by_id, panel_by_id};
use super::io::{guild_available, invalidate_panel, peek_panel, sf};
use super::lifecycle::{ActionCtx, Actor, claim_ticket, close_ticket, delete_ticket, load_settings, reopen_ticket, transcript_for_actor};
use super::messages::{e, error_payload, error_payload_code, live_managers, opened_embed, panel_error_payload, refresh_note};
use super::open::{Opener, list_live_tickets, open_ticket};
use super::panels::{default_color, refresh_panel_message};
use super::rules::{
    Action, Fail, MODAL_MAX_DESCRIPTION, Outcome, Source, default_intro_template, default_panel_template, is_empty_template, template_to_column,
    template_uses_token,
};

fn q(s: String) -> AssertSqlSafe<String> {
    AssertSqlSafe(s)
}

const ONLY_IN_SERVER: &str = "This only works in a server.";

/// `replyEphemeral(interaction, payload)`.
async fn reply_ephemeral(r: &Responder, payload: Payload) {
    safe_reply(r, payload.ephemeral(true)).await;
}

/// The guild of an interaction when it is cached (`interaction.inCachedGuild()`).
fn cached_guild_id(app: &App, guild_id: Option<GuildId>) -> Option<GuildId> {
    guild_id.filter(|g| guild_available(app, *g))
}

fn log_unless_gone(e: &InteractionError, what: &str) {
    if !is_interaction_gone(e) {
        bot_util::log_warn!(format!("tickets: {what}: {e}"));
    }
}

fn same_components(message: &Message) -> Payload {
    Payload::new().components(rows_from_json(&message_rows_json(message)))
}

/// `ackAndReply(interaction, payload)`: errors before the open flow acknowledges. A dropdown is reset (update) so the
/// same topic can be picked again.
async fn ack_and_reply(ctx: &ComponentCtx, payload: Payload) {
    let r = if ctx.kind == ComponentKind::Select {
        match ctx.update(same_components(&ctx.interaction.message).to_update()).await {
            Ok(()) => ctx.follow_up(payload.ephemeral(true).to_follow_up()).await.map(|_| ()),
            Err(e) => Err(e),
        }
    } else {
        ctx.reply(payload.ephemeral(true).to_reply()).await
    };
    if let Err(e) = r
        && !is_interaction_gone(&e)
    {
        bot_util::log_debug!(format!("tickets: ackAndReply failed: {e}"));
    }
}

async fn reply_open_result(r: &Responder, res: Outcome<super::open::Opened>) {
    match res {
        Ok(o) => {
            safe_reply(r, Payload::embed(opened_embed(&o.channel.id.to_string(), &o.notes)).no_mentions().ephemeral(true)).await;
        }
        Err(f) => {
            safe_reply(r, error_payload(&f).ephemeral(true)).await;
        }
    }
}

fn opener_of(interaction_id: String, user: &serenity::all::User, member: Option<&serenity::all::Member>) -> Opener {
    Opener { interaction_id, user: user.clone(), member: member.cloned() }
}

/// `handleOpenClick(interaction, client, panelId, optionId)`: the Open button or a dropdown pick: checks, then the
/// optional reason form, otherwise the open flow.
pub async fn handle_open_click(ctx: &ComponentCtx, panel_id: &str, option_id: &str) {
    let app = &ctx.app;
    let Some(guild_id) = cached_guild_id(app, ctx.guild_id()) else {
        reply_ephemeral(ctx, Payload::text(ONLY_IN_SERVER)).await;
        return;
    };
    let g = guild_id.to_string();
    let is_select = ctx.kind == ComponentKind::Select;

    // Everything before the acknowledgement fits the 3 s window: memo first, then a bounded lookup.
    let budget = ctx.ack_budget_ms();
    let mut panel = peek_panel(app, &g, panel_id);
    let mut live = Vec::new();
    let mut lookup_failed = false;
    if budget > 0 {
        let (a, gg, p, u, memo) = (app.clone(), g.clone(), panel_id.to_string(), ctx.user().id.to_string(), panel.clone());
        let fut = async move {
            let panel = match memo {
                Some(p) => Some(p),
                None => get_panel_for_open(&a, &gg, &p).await?,
            };
            let live = list_live_tickets(&a, guild_id, &p, &u).await?;
            Ok::<_, anyhow::Error>((panel, live))
        };
        match bot_util::async_util::with_timeout(fut, budget, "ticket open lookup").await {
            Ok((p, l)) => {
                panel = p;
                live = l;
            }
            Err(bot_util::async_util::WithTimeoutError::Timeout(_)) => lookup_failed = true,
            Err(bot_util::async_util::WithTimeoutError::Inner(e)) => {
                lookup_failed = true;
                bot_util::log_warn!("tickets: open lookup failed", bot_core::error::log_arg(&e));
            }
        }
    }
    let Some(panel) = panel else {
        ack_and_reply(ctx, error_payload_code(if lookup_failed || budget == 0 { "SLOW" } else { "PANEL_GONE" })).await;
        return;
    };
    let Some(option) = panel.options.iter().find(|o| o.id == option_id).cloned() else {
        ack_and_reply(ctx, error_payload_code("OPTION_GONE")).await;
        return;
    };
    if live.len() as i64 >= i64::from(panel.panel.max_open_per_user) {
        let f = Fail::with("OPEN_LIMIT", json!({ "ids": live.iter().map(|r| r.channel_id.clone()).collect::<Vec<_>>(), "max": panel.panel.max_open_per_user }));
        ack_and_reply(ctx, error_payload(&f)).await;
        return;
    }
    if panel.panel.ask_reason {
        // showModal is the first response; nothing was acknowledged before it.
        if let Err(e) = ctx.show_modal(build_reason_modal(&panel.panel.id, &option.id, &option.label)).await {
            log_unless_gone(&e, "showModal failed");
        }
        return;
    }
    let ack = if is_select { ctx.update(same_components(&ctx.interaction.message).to_update()).await } else { ctx.defer_reply(true).await };
    if let Err(e) = ack {
        log_unless_gone(&e, "acknowledging the open click failed");
        return;
    }
    let settings = load_settings(app, &g).await;
    let opener = opener_of(ctx.interaction.id.to_string(), ctx.user(), ctx.member());
    let r = open_ticket(app, guild_id, &opener, &panel, Some(&option), None, settings.as_deref()).await;
    reply_open_result(ctx, r).await;
}

async fn handle_reason_submit(ctx: &ModalCtx, params: &[String]) {
    let app = &ctx.app;
    let Some(guild_id) = cached_guild_id(app, ctx.guild_id()) else {
        reply_ephemeral(ctx, Payload::text(ONLY_IN_SERVER)).await;
        return;
    };
    let g = guild_id.to_string();
    // This prefix is not wrapped by the module gate (the template form must work while the module is off).
    let mut settings = app.settings().peek_settings(&g);
    if settings.is_none() {
        let budget = ctx.ack_budget_ms();
        if budget > 0 {
            let (a, gg) = (app.clone(), g.clone());
            settings = bot_util::async_util::with_timeout(async move { Ok::<_, String>(a.settings().get_settings_safe(&gg).await) }, budget, "ticket modal settings")
                .await
                .ok()
                .flatten();
        }
    }
    let Some(settings) = settings else {
        reply_ephemeral(ctx, Payload::text("Server settings are temporarily unavailable. Please try again in a moment.")).await;
        return;
    };
    if !is_module_enabled(Some(&settings), Some(ModuleKey::Tickets)) {
        reply_ephemeral(ctx, error_payload_code("MODULE_OFF")).await;
        return;
    }
    let raw = ctx.value("ticket-reason").unwrap_or_default();
    let reason = bot_util::js::utf16_prefix(bot_util::js::trim(&raw), 1000).to_string();
    let reason = (!reason.is_empty()).then_some(reason);
    let ack = match &ctx.interaction.message {
        Some(m) => ctx.update(same_components(m).to_update()).await,
        None => ctx.defer_reply(true).await,
    };
    if let Err(e) = ack {
        log_unless_gone(&e, "acknowledging the reason form failed");
        return;
    }
    let panel_id = params.first().cloned().unwrap_or_default();
    let panel = match get_panel_for_open(app, &g, &panel_id).await {
        Ok(p) => p,
        Err(e) => {
            bot_util::log_warn!("tickets: panel lookup after the reason form failed", bot_core::error::log_arg(&anyhow::Error::from(e)));
            safe_reply(ctx, error_payload_code("SLOW").ephemeral(true)).await;
            return;
        }
    };
    let Some(panel) = panel else {
        safe_reply(ctx, error_payload_code("PANEL_GONE").ephemeral(true)).await;
        return;
    };
    let option_id = params.get(1).cloned().unwrap_or_default();
    let Some(option) = panel.options.iter().find(|o| o.id == option_id).cloned() else {
        safe_reply(ctx, error_payload_code("OPTION_GONE").ephemeral(true)).await;
        return;
    };
    let opener = opener_of(ctx.interaction.id.to_string(), ctx.user(), ctx.member());
    let r = open_ticket(app, guild_id, &opener, &panel, Some(&option), reason, Some(&settings)).await;
    reply_open_result(ctx, r).await;
}

fn ping_refusal(refused: &[String], channel_id: &str) -> String {
    refused
        .iter()
        .map(|id| format!("<@&{id}> is not mentionable and you lack **Mention @everyone, @here and All Roles** in <#{channel_id}>."))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The template form (`ticket-modal:tpl:{panelId}:{panel|intro}[:{optionId}]`). Modal submits bypass the router's level
/// check, so the Admin level is checked here as well.
async fn handle_template_submit(ctx: &ModalCtx, params: &[String]) {
    let app = &ctx.app;
    let Some(guild_id) = cached_guild_id(app, ctx.guild_id()) else {
        reply_ephemeral(ctx, Payload::text(ONLY_IN_SERVER)).await;
        return;
    };
    let g = guild_id.to_string();
    let mut settings: Option<Settings> = app.settings().peek_settings(&g);
    if settings.is_none() {
        let (a, gg) = (app.clone(), g.clone());
        settings = bot_util::async_util::with_timeout(async move { Ok::<_, String>(a.settings().get_settings_safe(&gg).await) }, ctx.ack_budget_ms(), "ticket form settings")
            .await
            .ok()
            .flatten();
    }
    // ADMIN follows from Discord permissions, so unavailable settings cannot wrongly grant it.
    let access = ctx.member_access();
    let level = bot_core::permission_checker::resolve_permission_level(access.as_ref(), settings.as_deref(), app.config().bot_owner_id.as_deref());
    if level < PermissionLevel::Admin {
        reply_ephemeral(ctx, panel_error_payload("NOT_ADMIN", &Value::Null)).await;
        return;
    }
    if !safe_defer(ctx, false).await {
        return;
    }
    let reply = |p: Payload| async move {
        safe_reply(ctx, p).await;
    };

    let panel_id = params.get(1).cloned().unwrap_or_default();
    let target = params.get(2).cloned().unwrap_or_default();
    let option_id = params.get(3).cloned().filter(|o| !o.is_empty());
    if target != "panel" && target != "intro" {
        return reply(panel_error_payload("FORM_EXPIRED", &Value::Null)).await;
    }
    let panel = match panel_by_id(app.db(), &g, &panel_id).await {
        Ok(Some(p)) => p,
        Ok(None) => return reply(panel_error_payload("PANEL_DELETED", &Value::Null)).await,
        Err(e) => {
            bot_util::log_error!("tickets: template form failed", bot_core::error::log_arg(&anyhow::Error::from(e)));
            return reply(panel_error_payload("SLOW", &Value::Null)).await;
        }
    };
    let mut option = None;
    if let Some(oid) = &option_id {
        option = panel.options.iter().find(|o| &o.id == oid).cloned();
        if option.is_none() {
            return reply(panel_error_payload("OPTION_NOT_FOUND", &Value::Null)).await;
        }
    }
    let is_panel = target == "panel";
    let base = if is_panel {
        match &panel.panel.template {
            Some(t) if !t.is_null() => normalize_template(t),
            _ => default_panel_template(panel.panel.style == TicketPanelStyle::Select),
        }
    } else {
        let src = option
            .as_ref()
            .and_then(|o| o.intro_template.clone())
            .filter(|v| !v.is_null())
            .or_else(|| panel.panel.intro_template.clone().filter(|v| !v.is_null()))
            .unwrap_or_else(default_intro_template);
        normalize_template(&src)
    };
    let (template, mut errors) = parse_template_modal(|id| ctx.value(id), &base, true);
    let what = if is_panel {
        "panel message".to_string()
    } else {
        format!("ticket intro{}", option.as_ref().map(|o| format!(" for **{}**", clamp_text(&o.label, 80))).unwrap_or_default())
    };

    let save = |value: Value| {
        let (a, gg, pid, oid) = (app.clone(), g.clone(), panel.panel.id.clone(), option.as_ref().map(|o| o.id.clone()));
        async move {
            let value = if value.is_null() { None } else { Some(value) };
            let r = if is_panel {
                sqlx::query(q(format!(r#"UPDATE "TicketPanel" SET "template" = $1, "updatedAt" = {SQL_NOW} WHERE "id" = $2 AND "guildId" = $3"#)))
                    .bind(value)
                    .bind(&pid)
                    .bind(&gg)
                    .execute(a.db())
                    .await
            } else if let Some(oid) = oid {
                sqlx::query(r#"UPDATE "TicketPanelOption" SET "introTemplate" = $1 WHERE "id" = $2 AND "panelId" = $3"#)
                    .bind(value)
                    .bind(&oid)
                    .bind(&pid)
                    .execute(a.db())
                    .await
            } else {
                sqlx::query(q(format!(r#"UPDATE "TicketPanel" SET "introTemplate" = $1, "updatedAt" = {SQL_NOW} WHERE "id" = $2 AND "guildId" = $3"#)))
                    .bind(value)
                    .bind(&pid)
                    .bind(&gg)
                    .execute(a.db())
                    .await
            };
            invalidate_panel(&a, &pid);
            r.map(|_| ())
        }
    };

    // An empty form resets the message to its default.
    if is_empty_template(&template) {
        if let Err(e) = save(Value::Null).await {
            bot_util::log_error!("tickets: saving a template failed", bot_core::error::log_arg(&anyhow::Error::from(e)));
            return reply(panel_error_payload("UNKNOWN", &Value::Null)).await;
        }
        let refresh = if is_panel { Some(refresh_panel_message(app, guild_id, &panel.panel.id).await) } else { None };
        return reply(e(success_embed("Message Reset", Some(&format!("Restored the default message.{}", refresh_note(refresh.as_ref()))), None))).await;
    }
    let desc_len = template["embed"].get("description").and_then(Value::as_str).map(bot_util::js::utf16_len).unwrap_or(0);
    if desc_len > MODAL_MAX_DESCRIPTION {
        errors.push(format!("Description is {desc_len} characters (max {MODAL_MAX_DESCRIPTION})."));
    }
    if !errors.is_empty() {
        return reply(panel_error_payload("INVALID_MESSAGE", &json!({ "errors": errors }))).await;
    }

    // Configuration time: a {managers} ping must be something the admin could send themselves.
    let managers = live_managers(app, guild_id, &panel.panel);
    let guild = cached_guild(app, guild_id);
    // `guild.channels.cache.get(interaction.channelId) ?? interaction.channel`.
    let channel = match guild.as_ref().and_then(|gd| gd.channels.get(&ctx.interaction.channel_id).cloned()) {
        Some(c) => Some(c),
        None => bot_core::interaction_helpers::resolve_channel(app, guild_id, ctx.interaction.channel_id).await,
    };
    if !is_panel
        && template_uses_token(&template, "managers")
        && !managers.is_empty()
        && let (Some(gd), Some(ch)) = (&guild, &channel)
    {
        let roles: Vec<RoleId> = ctx.member().map(|m| m.roles.clone()).unwrap_or_default();
        let refused = pingable_role_ids(gd, ch, Some((ctx.user().id, &roles)), &managers).refused;
        if !refused.is_empty() {
            return reply(e(error_embed("Role Not Pingable", Some(&ping_refusal(&refused, &ch.id.to_string())), None))).await;
        }
    }

    let mut stored = template.clone();
    if is_panel {
        stored["buttons"] = Value::Array(Vec::new());
    }
    if let Err(err) = save(template_to_column(&stored)).await {
        bot_util::log_error!("tickets: saving a template failed", bot_core::error::log_arg(&anyhow::Error::from(err)));
        return reply(panel_error_payload("UNKNOWN", &Value::Null)).await;
    }
    let refresh = if is_panel { Some(refresh_panel_message(app, guild_id, &panel.panel.id).await) } else { None };

    let vars = if is_panel {
        Vec::new()
    } else {
        vec![
            ("number".to_string(), VarValue::Text("0001".into())),
            ("reason".to_string(), VarValue::Text("Example reason".into())),
            ("topic".to_string(), VarValue::Text(option.as_ref().map(|o| o.label.clone()).unwrap_or_else(|| "Example topic".into()))),
            ("panel".to_string(), VarValue::Text(panel.panel.name.clone())),
            ("managers".to_string(), VarValue::Text(managers.iter().map(|id| format!("<@&{id}>")).collect::<Vec<_>>().join(" "))),
        ]
    };
    let rctx = RenderContext {
        member: ctx.member().map(|m| placeholder_member(m, guild.as_ref())),
        user: Some(placeholder_user(ctx.user())),
        guild: guild.as_ref().map(placeholder_guild),
        channel: channel.as_ref().map(placeholder_channel),
        vars: Some(vars),
        args: None,
    };
    let mut unknown: Vec<String> = Vec::new();
    let emb = &template["embed"];
    for text in [template.get("content"), emb.get("title"), emb.get("description"), emb.get("footer").and_then(|f| f.get("text"))]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        for token in render_placeholders_detailed(text, &rctx, None).unknown_tokens {
            if !unknown.contains(&token) {
                unknown.push(token);
            }
        }
    }
    let mut lines = vec![format!("Saved the {what} of **{}**.{}", clamp_text(&panel.panel.name, 80), refresh_note(refresh.as_ref()))];
    if !unknown.is_empty() {
        lines.push(format!("Unknown variables (left as typed): {}", clamp_text(&unknown.join(", "), 500)));
    }
    lines.push(if is_panel {
        "Variables: {server} {server.member_count} {channel}".into()
    } else {
        "Variables: {user} {user.display_name} {number} {reason} {topic} {panel} {managers} {server}".into()
    });
    reply(e(success_embed("Message Saved", Some(&lines.join("\n")), None))).await;

    let settings_ref = settings.as_deref();
    let preview = render_template(
        &template,
        &rctx,
        RenderOptions { default_color: Some(default_color(settings_ref)), allowed_mentions: Some(CreateAllowedMentions::new()), extra_components: Vec::new() },
    );
    if let Some(p) = preview {
        let mut payload = p.to_payload().no_mentions();
        let content = clamp_text(&format!("**Preview** (nothing is pinged){}", p.content.as_ref().map(|c| format!("\n{c}")).unwrap_or_default()), 2000);
        payload = payload.content(content);
        if let Err(err) = ctx.follow_up(payload.to_follow_up()).await {
            bot_util::log_debug!(format!("tickets: preview failed: {err}"));
        }
    }
}

/// `handleTicketModal(interaction)`: the reason form, or the template form when `params[0] === 'tpl'`.
pub async fn handle_ticket_modal(ctx: ModalCtx) -> bot_core::BotResult {
    let (_, params) = ctx.parsed();
    if params.first().map(String::as_str) == Some("tpl") {
        handle_template_submit(&ctx, &params).await;
    } else {
        handle_reason_submit(&ctx, &params).await;
    }
    Ok(())
}

/// `handleTicketSelect(interaction)`: the panel dropdown.
pub async fn handle_ticket_select(ctx: ComponentCtx) -> bot_core::BotResult {
    let (_, params) = ctx.parsed();
    let Some(option_id) = ctx.values().into_iter().next() else {
        ack_and_reply(&ctx, error_payload_code("OPTION_GONE")).await;
        return Ok(());
    };
    handle_open_click(&ctx, params.first().map(String::as_str).unwrap_or(""), &option_id).await;
    Ok(())
}

async fn button_error(ctx: &ComponentCtx, payload: Payload) {
    let _ = ctx.follow_up(payload.ephemeral(true).to_follow_up()).await;
}

async fn handle_lifecycle_button(ctx: &ComponentCtx, action: Action, ticket_id: &str) {
    let app = &ctx.app;
    let Some(guild_id) = cached_guild_id(app, ctx.guild_id()) else {
        reply_ephemeral(ctx, Payload::text(ONLY_IN_SERVER)).await;
        return;
    };
    let is_transcript = action == Action::Transcript;
    // deferUpdate keeps the clicked message; every error then goes out as an ephemeral follow-up. Only the transcript
    // button uses deferReply.
    let ack = if is_transcript { ctx.defer_reply(true).await } else { ctx.defer_update().await };
    if let Err(e) = ack {
        log_unless_gone(&e, "acknowledging a ticket button failed");
        return;
    }
    let fail = |p: Payload| async move {
        if is_transcript {
            safe_reply(ctx, p.ephemeral(true)).await;
        } else {
            button_error(ctx, p).await;
        }
    };
    let g = guild_id.to_string();
    let settings = load_settings(app, &g).await;
    let ticket = get_ticket_by_id(app.db(), &g, ticket_id).await.ok().flatten();
    let Some(ticket) = ticket.filter(|t| t.ticket.channel_id == ctx.interaction.channel_id.to_string()) else {
        return fail(error_payload_code("TICKET_NOT_FOUND")).await;
    };
    let Some(member) = ctx.member() else { return fail(error_payload_code("TICKET_NOT_FOUND")).await };
    let actor = Actor::of(app, guild_id, member);
    let actx = ActionCtx { guild_id, ticket_id: ticket.ticket.id.clone(), actor: actor.clone(), settings: settings.clone(), source: Source::Button };

    if is_transcript {
        // `interaction.channel`: the cache, else a REST fetch (which caches it, as discord.js does).
        let Some(channel) = bot_core::interaction_helpers::resolve_channel(app, guild_id, ctx.interaction.channel_id).await else {
            return fail(error_payload_code("TICKET_NOT_FOUND")).await;
        };
        match transcript_for_actor(app, guild_id, &ticket, &channel, settings.as_deref(), &actor).await {
            Ok(r) => {
                safe_reply(ctx, r.payload.ephemeral(true)).await;
            }
            Err(f) => fail(error_payload(&f)).await,
        }
        return;
    }
    if action == Action::Delete {
        let noop: super::lifecycle::OnSaved = Box::new(|_text: String| Box::pin(async {}));
        if let Err(f) = delete_ticket(app, actx, Some(noop)).await {
            fail(error_payload(&f)).await;
        }
        return;
    }
    let r = match action {
        Action::Claim => claim_ticket(app, actx).await,
        Action::Close => close_ticket(app, actx, None).await,
        _ => reopen_ticket(app, actx).await,
    };
    let done = match r {
        Ok(d) => d,
        Err(f) => return fail(error_payload(&f)).await,
    };
    let rows = message_rows_json(&ctx.interaction.message);
    let claim_only = action == Action::Claim;
    let predicate = move |c: &Value| !claim_only || c.get("custom_id").and_then(Value::as_str).is_some_and(|id| id.starts_with("ticket:claim:"));
    let edited = rows_from_json(&disable_buttons_json(&rows, &predicate));
    if let Err(err) = ctx.edit_reply(EditInteractionResponse::new().components(edited)).await {
        bot_util::log_debug!(format!("tickets: could not update the clicked message: {err}"));
    }
    if !done.notes.is_empty() {
        button_error(ctx, e(warn_embed("Done, with a note", Some(&done.notes.join("\n")), None))).await;
    }
}

/// `handleTicketButton(interaction)`: every button with the `ticket` prefix (the gate wrapper has already checked the
/// module).
pub async fn handle_ticket_button(ctx: ComponentCtx) -> bot_core::BotResult {
    let (_, params) = ctx.parsed();
    let action = params.first().map(String::as_str).unwrap_or("");
    if action == "open" {
        handle_open_click(&ctx, params.get(1).map(String::as_str).unwrap_or(""), params.get(2).map(String::as_str).unwrap_or("")).await;
        return Ok(());
    }
    match Action::parse(action).filter(|a| matches!(a, Action::Close | Action::Claim | Action::Reopen | Action::Delete | Action::Transcript)) {
        Some(a) => handle_lifecycle_button(&ctx, a, params.get(1).map(String::as_str).unwrap_or("")).await,
        None => reply_ephemeral(&ctx, Payload::text("This button is no longer active.")).await,
    }
    Ok(())
}

/// The guild id of a snowflake string.
pub fn guild_of(id: &str) -> Option<GuildId> {
    sf(id).map(GuildId::new)
}
