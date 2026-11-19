//! Port of `src/commands/community/event.js`: `/event create | edit | cancel | list | info | attendees`. Thin: option
//! parsing, validation and replies; posting, RSVPs, the jobs, editing and cancelling live in
//! [`crate::event_service`]. Every reply is private. Any member may create an event; its host may edit and cancel
//! it, and staff (Moderator level and up) may do so for every event. Members below Moderator may host at most
//! `EVENT.HOST_MAX` open events at once.

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::embed_builder::guild_embed;
use bot_core::interaction_helpers::{Choice, Payload, check_assignable_role_cached, check_channel_usable, respond_autocomplete, safe_defer, safe_reply};
use bot_core::paginator::{PaginateOptions, paginate};
use bot_core::prelude::*;
use bot_db::{GuildEvent, GuildEventStatus, GuildSettings, RsvpStatus};
use bot_util::text::{chunk_lines, clamp_text};
use serenity::all::{ChannelId, GuildId, Permissions, RoleId};
use serenity::builder::CreateEmbed;

use crate::event_service::{
    self as svc, CloseOutcome, CloseReason, CreateFailure, EventFields, NOT_YOURS_TEXT, NewEvent, attendee_lines, build_edit_panel, can_manage_event,
    cancel_event, consts as EVENT, create_event, find_events_for_autocomplete, get_tally, is_event_staff, is_open, jump_url, list_line, list_open_events,
    load_zone, parse_event_fields, remind_at, resolve_event_ref, voice_lead, voice_lead_text,
};

fn event_option() -> Opt {
    Opt::string("event", "Event").required(true).autocomplete(true).max_length(100)
}

/// The command's data. The payload budget is tight: do not lengthen any description.
pub fn data() -> SlashCommand {
    let text_types = [channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT];
    SlashCommand::new("event", "Run events with RSVP buttons, an optional Discord event and a reminder")
        .option(
            Opt::sub_command("create", "Post an event with RSVP buttons")
                .option(Opt::string("title", "Event name").required(true).max_length(EVENT::TITLE_MAX as u16))
                .option(Opt::string("start", "When: fri 7pm, oct 30 18:00, in 3d").required(true).max_length(100))
                .option(Opt::string("duration", "How long: 2h, 1d (max 14d)").max_length(40))
                .option(Opt::string("description", "What it is about").max_length(EVENT::DESCRIPTION_MAX as u16))
                .option(Opt::string("location", "Where (needed for a Discord event)").max_length(EVENT::LOCATION_MAX as u16))
                .option(Opt::channel("channel", "Where to post it").channel_types(&text_types))
                .option(Opt::role("role", "Role for members who are going"))
                .option(Opt::integer("capacity", "Most members who can go").min_value(1).max_value(EVENT::CAPACITY_MAX))
                .option(Opt::integer("reminder", "Minutes before the start to ping (0 = none)").min_value(0).max_value(EVENT::REMINDER_MAX)),
        )
        .option(Opt::sub_command("edit", "Edit an event").option(event_option()))
        .option(Opt::sub_command("cancel", "Cancel an event").option(event_option()))
        .option(Opt::sub_command("list", "List the open events"))
        .option(Opt::sub_command("info", "Show an event").option(event_option()))
        .option(Opt::sub_command("attendees", "List who is going").option(event_option()))
}

/// The command (`module 'events'`, gated, MEMBER: edit and cancel check the host or staff per event).
pub fn command() -> CommandDef {
    CommandDef::new("community", data(), Some(ModuleKey::Events), execute)
        .permission_level(PermissionLevel::Member)
        .autocomplete(|ctx| async move { autocomplete(ctx).await })
}

fn not_found() -> CreateEmbed {
    error_embed("Event Not Found", Some("Pick an event from the list."), None)
}
fn closed() -> CreateEmbed {
    warn_embed("Event Closed", Some("That event has ended or was cancelled."), None)
}
fn not_yours() -> CreateEmbed {
    error_embed("Not Your Event", Some(NOT_YOURS_TEXT), None)
}

/// `STATUS_EMOJI`.
pub fn status_emoji(s: GuildEventStatus) -> &'static str {
    match s {
        GuildEventStatus::Scheduled | GuildEventStatus::Active => "🟢",
        GuildEventStatus::Ended => "🏁",
        GuildEventStatus::Cancelled => "🚫",
    }
}

fn status_emoji_str(s: &str) -> &'static str {
    match s {
        "SCHEDULED" | "ACTIVE" => "🟢",
        "ENDED" => "🏁",
        "CANCELLED" => "🚫",
        _ => "",
    }
}

/// `STATUS_TEXT`.
pub fn status_text(s: GuildEventStatus) -> &'static str {
    match s {
        GuildEventStatus::Scheduled => "Scheduled",
        GuildEventStatus::Active => "Active",
        GuildEventStatus::Ended => "Ended",
        GuildEventStatus::Cancelled => "Cancelled",
    }
}

async fn reply_embed(ctx: &CommandCtx, embed: CreateEmbed) -> BotResult {
    safe_reply(&ctx.responder, Payload::embed(embed).no_mentions()).await;
    Ok(())
}

fn guild_of(ctx: &CommandCtx) -> anyhow::Result<GuildId> {
    ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/event outside a guild"))
}

fn event_ref(ctx: &CommandCtx) -> anyhow::Result<String> {
    ctx.get_string("event").ok_or_else(|| anyhow::anyhow!("Required option \"event\" not found."))
}

async fn execute(ctx: CommandCtx) -> BotResult {
    match ctx.subcommand().as_deref() {
        Some("create") => create(ctx).await,
        Some("edit") => edit(ctx).await,
        Some("cancel") => cancel(ctx).await,
        Some("list") => list(ctx).await,
        Some("info") => info(ctx).await,
        Some("attendees") => attendees(ctx).await,
        _ => Ok(()),
    }
}

// ── create ────────────────────────────────────────────────────────────────────

/// `memberCanPost(channel, member)`: can the member post in the channel (so nobody makes the bot post where they
/// could not post themselves)?
fn member_can_post(ctx: &CommandCtx, guild_id: GuildId, channel_id: ChannelId) -> bool {
    let Some(member) = ctx.member() else { return false };
    bot_core::interaction_helpers::with_cached_guild(&ctx.app, guild_id, |g| {
        let Some(ch) = bot_core::permissions::find_channel(g, channel_id) else { return false };
        let need = if bot_core::permissions::is_thread(ch) {
            Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES_IN_THREADS
        } else {
            Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES
        };
        bot_core::permissions::member_permissions_in(g, ch, member.user.id, &member.roles, true).is_some_and(|p| bot_core::permissions::js_has(p, need))
    })
    .unwrap_or(false)
}

async fn create(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let guild_id = guild_of(&ctx)?;
    let app = ctx.app.clone();
    let loaded = load_zone(&app, Some(guild_id), ctx.user().id, ctx.member(), 3000).await;
    let settings = loaded.settings.clone();
    let s: Option<&GuildSettings> = settings.as_deref();
    let fields = EventFields {
        title: ctx.get_string("title").unwrap_or_default(),
        start: ctx.get_string("start").unwrap_or_default(),
        duration: ctx.get_string("duration"),
        description: ctx.get_string("description"),
        location: ctx.get_string("location"),
    };
    let (details, when) = match parse_event_fields(&fields, &loaded.resolved, app.clock().now_ms()) {
        Ok(v) => v,
        Err(f) => return reply_embed(&ctx, error_embed(&f.title, Some(&f.text), None)).await,
    };

    // Where to post: the option, else the configured announcement channel, else here.
    let picked = ctx.get_channel_id("channel");
    let configured = s
        .and_then(|s| s.event_channel_id.as_deref())
        .and_then(|c| c.parse::<u64>().ok().filter(|v| *v != 0))
        .map(ChannelId::new)
        .filter(|c| bot_core::interaction_helpers::cached_channel(&app, guild_id, *c).is_some());
    let channel = picked.or(configured).unwrap_or(ctx.channel_id());
    // A picked channel is `guild.channels.cache.get(id) ?? picked` in the JS: the option always names a real channel,
    // so one the cache lacks is fetched (and cached) before it is judged, instead of answering "Channel not found".
    if let Some(p) = picked {
        let _ = bot_core::interaction_helpers::resolve_channel(&app, guild_id, p).await;
    }
    let usable = check_channel_usable(&app, guild_id, Some(channel), &bot_core::interaction_helpers::CHANNEL_POST_NEEDS);
    if !usable.ok {
        return reply_embed(&ctx, error_embed("Cannot Post There", Some(usable.reason.as_deref().unwrap_or("I cannot post in that channel.")), None)).await;
    }
    // A chosen channel is always checked. Below staff, so is the channel the command ran in; only the configured
    // announcement channel is open to everyone.
    let access = ctx.member_access();
    let staff = is_event_staff(&app, access.as_ref(), s);
    let is_configured = s.and_then(|s| s.event_channel_id.as_deref()) == Some(channel.to_string().as_str());
    let must_check = picked.is_some() || (!staff && !is_configured);
    if must_check && !member_can_post(&ctx, guild_id, channel) {
        return reply_embed(&ctx, error_embed("Cannot Post There", Some(&format!("You cannot send messages in <#{channel}>.")), None)).await;
    }

    let role_opt: Option<RoleId> = ctx.get_role_id("role");
    if let Some(role) = role_opt {
        // A caller must not hand out a role they could not give themselves (the role is added by the bot).
        let owner = bot_core::interaction_helpers::with_cached_guild(&app, guild_id, |g| g.owner_id).is_some_and(|o| o == ctx.user().id);
        let may_give = owner || access.as_ref().and_then(|a| a.permissions).is_some_and(|p| bot_core::permissions::js_has(p, Permissions::MANAGE_ROLES));
        if !may_give {
            return reply_embed(&ctx, error_embed("Invalid Role", Some("You need the **Manage Roles** permission to attach a role to an event."), None)).await;
        }
        let check = check_assignable_role_cached(&app, guild_id, role, ctx.member(), false);
        if !check.ok {
            return reply_embed(&ctx, error_embed("Invalid Role", Some(check.reason.as_deref().unwrap_or("That role cannot be given.")), None)).await;
        }
    }

    let fallback = s.map(|s| i64::from(s.event_reminder_minutes)).unwrap_or(15);
    let reminder = ctx.get_integer("reminder").unwrap_or(fallback).clamp(0, EVENT::REMINDER_MAX);
    let data = NewEvent {
        title: details.title,
        description: details.description,
        location: details.location,
        starts_at: when.starts_at,
        ends_at: when.ends_at,
        host_id: ctx.user().id.to_string(),
        role_id: role_opt.map(|r| r.to_string()),
        capacity: ctx.get_integer("capacity").map(|c| c.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32),
        reminder_minutes: reminder as i32,
    };
    let out = create_event(&app, guild_id, channel, s, &data, if staff { None } else { Some(EVENT::HOST_MAX) }).await?;
    let (event, notes) = match out {
        Ok(v) => v,
        Err(CreateFailure::Limit) => {
            let tail = if staff { "Cancel one with `/event cancel` first." } else { "Try again once one has ended." };
            return reply_embed(&ctx, error_embed("Too Many Events", Some(&format!("This server already has {} open events. {tail}", EVENT::ACTIVE_MAX)), None)).await;
        }
        Err(CreateFailure::HostLimit) => {
            let text = format!("You already host {} open events. Cancel one with `/event cancel` or wait until one ends.", EVENT::HOST_MAX);
            return reply_embed(&ctx, error_embed("Too Many Events", Some(&text), None)).await;
        }
        Err(f) => {
            let text = if f == CreateFailure::Send {
                format!("I could not post in <#{channel}>. Check that I can send messages and embeds there.")
            } else {
                "The event could not be saved. Please try again.".to_string()
            };
            return reply_embed(&ctx, error_embed("Could Not Create The Event", Some(&text), None)).await;
        }
    };

    let url = jump_url(&event);
    let ts = |style: &str| bot_util::time::discord_timestamp(&event.starts_at, style);
    let mut lines = vec![
        format!(
            "**{}** is posted in <#{}>{}",
            clamp_text(&event.title, 100),
            event.channel_id,
            if url.is_empty() { ".".to_string() } else { format!(": [jump to it]({url})") }
        ),
        format!("Starts {} ({}).", ts("F"), ts("R")),
    ];
    if !when.zone_from_input {
        let now = app.clock().now_ms() as f64;
        lines.push(format!(
            "The start was read in {}, {}.",
            bot_util::timezones::zone_label_ms(&loaded.resolved.zone, now),
            bot_util::timezones::zone_source_text(loaded.resolved.source.as_str(), loaded.resolved.role_id.as_deref(), false)
        ));
        if loaded.personal_unknown {
            lines.push("⚠️ Your saved timezone could not be loaded just now, so another one was used.".into());
        }
    }
    if event.discord_event_id.as_deref().is_some_and(|d| !d.is_empty()) {
        lines.push("A Discord event was created too.".into());
    }
    if let Some(t) = event.text_channel_id.as_deref().filter(|t| !t.is_empty()) {
        lines.push(format!("Its chat is <#{t}>, for members who answer Going or Maybe. A voice channel opens {}.", voice_lead_text(voice_lead(s))));
    }
    if event.reminder_minutes > 0 {
        let ahead = remind_at(&event).is_some_and(|r| r.timestamp_millis() > app.clock().now_ms());
        lines.push(if ahead {
            format!("Members who are going are pinged {} minutes before the start.", event.reminder_minutes)
        } else {
            "The reminder time has already passed, so no reminder will be sent.".into()
        });
    }
    for n in notes {
        lines.push(format!("⚠️ {n}"));
    }
    reply_embed(&ctx, success_embed("Event Created", Some(&lines.join("\n")), None)).await
}

// ── edit ──────────────────────────────────────────────────────────────────────

/// The panel: its buttons open the forms ([`crate::event_service::handle_edit_button`]).
async fn edit(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let g = guild_of(&ctx)?.to_string();
    let Some(event) = resolve_event_ref(&ctx.app, &g, &event_ref(&ctx)?).await? else { return reply_embed(&ctx, not_found()).await };
    let settings = ctx.app.settings().get_settings_safe(&g).await;
    if !can_manage_event(&ctx.app, ctx.member_access().as_ref(), settings.as_deref(), Some(&event)) {
        return reply_embed(&ctx, not_yours()).await;
    }
    if !is_open(Some(&event)) {
        return reply_embed(&ctx, closed()).await;
    }
    safe_reply(&ctx.responder, build_edit_panel(&event, settings.as_deref()).no_mentions()).await;
    Ok(())
}

// ── cancel ────────────────────────────────────────────────────────────────────

async fn cancel(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let g = guild_of(&ctx)?.to_string();
    let Some(event) = resolve_event_ref(&ctx.app, &g, &event_ref(&ctx)?).await? else { return reply_embed(&ctx, not_found()).await };
    let settings = ctx.app.settings().get_settings_safe(&g).await;
    if !can_manage_event(&ctx.app, ctx.member_access().as_ref(), settings.as_deref(), Some(&event)) {
        return reply_embed(&ctx, not_yours()).await;
    }
    let res = cancel_event(&ctx.app, &event.id, Some(&g), CloseReason::Manual).await?;
    let (closed_event, discord_failed) = match res {
        CloseOutcome::Closed { event, discord_failed } => (event, discord_failed),
        CloseOutcome::AlreadyClosed(_) => return reply_embed(&ctx, closed()).await,
        CloseOutcome::NotFound => return reply_embed(&ctx, not_found()).await,
    };
    let mut lines = vec![format!("**{}** is cancelled. Its post is updated and its reminder is removed.", clamp_text(&event.title, 100))];
    if let Some(r) = event.role_id.as_deref().filter(|r| !r.is_empty()) {
        lines.push(format!("The <@&{r}> role is being taken back from the members who were going."));
    }
    let has = |s: &Option<String>| s.as_deref().is_some_and(|x| !x.is_empty());
    if closed_event.as_ref().is_some_and(|e| has(&e.text_channel_id) || has(&e.voice_channel_id)) {
        lines.push("Its chat, voice channel, post and reminders are deleted as soon as the voice channel is empty.".into());
    }
    if discord_failed {
        lines.push("⚠️ I could not delete the linked Discord event: remove it by hand.".into());
    }
    reply_embed(&ctx, success_embed("Event Cancelled", Some(&lines.join("\n")), None)).await
}

// ── list / info / attendees ───────────────────────────────────────────────────

async fn list(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let g = guild_of(&ctx)?.to_string();
    let settings = ctx.app.settings().get_settings_safe(&g).await;
    let (events, going) = list_open_events(&ctx.app, &g).await?;
    if events.is_empty() {
        safe_reply(&ctx.responder, Payload::embed(info_embed("No Open Events", Some("There are no open events. Post one with `/event create`."), None))).await;
        return Ok(());
    }
    let mut lines: Vec<String> = Vec::new();
    for e in &events {
        lines.push(list_line(e, going.get(&e.id).copied().unwrap_or(0)));
        lines.push(String::new());
    }
    lines.pop();
    let title = format!("📅 Open events ({}/{})", events.len(), EVENT::ACTIVE_MAX);
    let pages: Vec<CreateEmbed> = chunk_lines(&lines, 3800).iter().map(|c| guild_embed(settings.as_deref(), Some(&title), Some(c), None)).collect();
    paginate(&ctx, &pages, PaginateOptions { ephemeral: true, ..Default::default() }).await?;
    Ok(())
}

/// The lines of `/event info` (after the description).
pub fn info_lines(event: &GuildEvent, tally: &svc::Tally, guild_id: &str) -> Vec<String> {
    let url = jump_url(event);
    let ts = |style: &str| bot_util::time::discord_timestamp(&event.starts_at, style);
    let length = event.ends_at.map(|e| bot_util::parse_duration::format_duration((e.timestamp_millis() - event.starts_at.timestamp_millis()) as f64));
    let mut lines = vec![
        format!("{} **{}**", status_emoji(event.status), status_text(event.status)),
        format!("**When:** {} ({}){}", ts("F"), ts("R"), length.map(|l| format!(", for {l}")).unwrap_or_default()),
    ];
    if let Some(l) = event.location.as_deref().filter(|l| !l.is_empty()) {
        lines.push(format!("**Where:** {}", clamp_text(l, 100)));
    }
    lines.push(format!("**Host:** <@{}>", event.host_id));
    lines.push(format!("**Post:** <#{}>{}", event.channel_id, if url.is_empty() { " · the post was deleted".to_string() } else { format!(" · [jump]({url})") }));
    if let Some(r) = event.role_id.as_deref().filter(|r| !r.is_empty()) {
        lines.push(format!("**Role:** <@&{r}>"));
    }
    if let Some(t) = event.text_channel_id.as_deref().filter(|t| !t.is_empty()) {
        lines.push(format!("**Chat:** <#{t}>"));
    }
    if let Some(v) = event.voice_channel_id.as_deref().filter(|v| !v.is_empty()) {
        lines.push(format!("**Voice:** <#{v}>"));
    }
    let cap = match event.capacity {
        Some(c) if c != 0 => format!("/{c}"),
        _ => String::new(),
    };
    lines.push(format!("**Going:** {}{cap} · **Maybe:** {} · **Can't go:** {}", tally.going, tally.maybe, tally.declined));
    lines.push(format!(
        "**Reminder:** {}",
        if event.reminder_minutes > 0 {
            format!("{} min before{}", event.reminder_minutes, if event.reminder_sent_at.is_some() { " (sent)" } else { "" })
        } else {
            "none".into()
        }
    ));
    if let Some(d) = event.discord_event_id.as_deref().filter(|d| !d.is_empty()) {
        lines.push(format!("**Discord event:** https://discord.com/events/{guild_id}/{d}"));
    }
    lines.push(format!("**ID:** `{}`", event.id));
    lines
}

async fn info(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let g = guild_of(&ctx)?.to_string();
    let Some(event) = resolve_event_ref(&ctx.app, &g, &event_ref(&ctx)?).await? else { return reply_embed(&ctx, not_found()).await };
    let (tally, settings) = tokio::join!(get_tally(&ctx.app, &event.id), ctx.app.settings().get_settings_safe(&g));
    let tally = tally?;
    let mut parts = vec![clamp_text(event.description.as_deref().unwrap_or(""), 1000)];
    parts.extend(info_lines(&event, &tally, &g));
    let text = parts.into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join("\n");
    reply_embed(&ctx, guild_embed(settings.as_deref(), Some(&format!("📅 {}", clamp_text(&event.title, 200))), Some(&text), None)).await
}

async fn attendees(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let g = guild_of(&ctx)?.to_string();
    let Some(event) = resolve_event_ref(&ctx.app, &g, &event_ref(&ctx)?).await? else {
        safe_reply(&ctx.responder, Payload::embed(not_found())).await;
        return Ok(());
    };
    let rsvps_fut = async {
        let rows: Vec<(String, String)> = sqlx::query_as(
            r#"SELECT "userId", "status"::text FROM "EventRsvp" WHERE "eventId" = $1 ORDER BY "updatedAt" ASC LIMIT $2"#,
        )
        .bind(&event.id)
        .bind(EVENT::ATTENDEES_MAX)
        .fetch_all(ctx.app.db())
        .await
        .map_err(bot_db::DbError::from)?;
        Ok::<_, anyhow::Error>(rows)
    };
    let (tally, settings, rsvps) = tokio::join!(get_tally(&ctx.app, &event.id), ctx.app.settings().get_settings_safe(&g), rsvps_fut);
    let (tally, rsvps) = (tally?, rsvps?);
    if rsvps.is_empty() {
        safe_reply(&ctx.responder, Payload::embed(info_embed("No RSVPs Yet", Some("No one has answered yet."), None))).await;
        return Ok(());
    }
    let typed: Vec<(String, RsvpStatus)> = rsvps
        .into_iter()
        .filter_map(|(u, s)| RsvpStatus::ALL.iter().copied().find(|x| x.as_str() == s).map(|st| (u, st)))
        .collect();
    let shown = typed.len() as i64;
    let head = format!("✅ {} going · ❔ {} maybe · ❌ {} can't go", tally.going, tally.maybe, tally.declined);
    let total = tally.going + tally.maybe + tally.declined;
    let title = format!("👥 {}", clamp_text(&event.title, 200));
    let pages: Vec<CreateEmbed> = chunk_lines(&attendee_lines(&typed), 3700)
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let more = if i == 0 && total > shown { format!("\n… and {} more", total - shown) } else { String::new() };
            guild_embed(settings.as_deref(), Some(&title), Some(&format!("**{head}**\n\n{c}{more}")), None)
        })
        .collect();
    paginate(&ctx, &pages, PaginateOptions { ephemeral: true, ..Default::default() }).await?;
    Ok(())
}

// ── autocomplete ──────────────────────────────────────────────────────────────

/// The name of an autocomplete choice.
pub fn choice_name(status: &str, title: &str, starts_at: &chrono::NaiveDateTime) -> String {
    let iso = bot_db::format_js_date(&starts_at.and_utc());
    let day: String = iso.chars().take(10).collect();
    bot_util::js::trim(&format!("{} {} · {day}", status_emoji_str(status), clamp_text(title, 70))).to_string()
}

/// Read-only. Answers within the 3 s window or with no choices.
async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let focused = ctx.focused();
    let sub = ctx.subcommand();
    let (Some((name, value)), Some(sub)) = (focused, sub) else {
        respond_autocomplete(&ctx, &[]).await;
        return Ok(());
    };
    if name != "event" {
        respond_autocomplete(&ctx, &[]).await;
        return Ok(());
    }
    let g = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
    let (app, access, user) = (ctx.app.clone(), ctx.member_access(), ctx.user().id.to_string());
    // Edit and cancel offer a member who is not staff only their own events (execute checks again).
    let lookup = async move {
        let own = if sub == "edit" || sub == "cancel" {
            let settings = app.settings().get_settings_safe(&g).await;
            !is_event_staff(&app, access.as_ref(), settings.as_deref())
        } else {
            false
        };
        find_events_for_autocomplete(&app, &g, &sub, &value, own.then_some(user.as_str())).await
    };
    match bot_util::async_util::with_timeout(lookup, ctx.ack_budget_ms(), "event autocomplete").await {
        Ok(rows) => {
            let choices: Vec<Choice> = rows.iter().map(|e| Choice::new(choice_name(&e.status, &e.title, &e.starts_at), e.id.clone())).collect();
            respond_autocomplete(&ctx, &choices).await;
        }
        Err(e) => {
            bot_util::log_warn!("event autocomplete failed".to_string(), e.to_string());
            respond_autocomplete(&ctx, &[]).await;
        }
    }
    Ok(())
}
