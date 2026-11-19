//! Port of `src/commands/admin/reminder.js`: `/reminder create | edit | list | pause | resume | delete | test | next`,
//! with the name autocomplete. Thin: validation, next-run maths, payloads and the job live in
//! [`crate::reminder_service`]; the message editor submit is in [`crate::plugins::reminders`].

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::custom_embed::{build_template_modal, normalize_template, validate_template};
use bot_core::embed_builder::guild_embed;
use bot_core::interaction_helpers::{
    Choice, ConfirmOptions, Payload, await_confirm, cached_channel, cached_guild, check_channel_usable, check_channel_usable_for, pingable_role_ids, reply_error, respond_autocomplete, safe_reply,
};
use bot_core::prelude::*;
use bot_db::{Reminder, ReminderMode};
use bot_util::text::clamp_text;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use serenity::all::{ChannelId, GuildChannel, GuildId, Permissions, RoleId};
use serenity::builder::{CreateEmbed, CreateModal};

use crate::reminder_service::{
    self as svc, NewReminder, ReminderUpdate, ReminderUserError, Schedule, ScheduleInput, compute_next_run, default_template, describe_schedule, job_key, mode_label,
    module_off_line, reminder_warnings, resolve_time_zone, summary_lines, sync_reminder_job,
};

const MODE_CHOICES: [(&str, &str); 3] =
    [("Time of day on chosen days (MEE6 Single)", "time"), ("Every interval (MEE6 Recurring)", "interval"), ("Once (date/time or delay)", "once")];

fn schedule_options(mut s: Opt, for_edit: bool) -> Opt {
    let mut mode = Opt::string("mode", "How it repeats").required(!for_edit);
    for (n, v) in MODE_CHOICES {
        mode = mode.choice(n, v);
    }
    s = s
        .option(Opt::channel("channel", "Channel to post in").required(!for_edit).channel_types(&[channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT]))
        .option(mode)
        .option(Opt::string("time", "Time mode: e.g. 09:00 or 9:30pm (server timezone)").max_length(10))
        .option(Opt::string("weekdays", "mon,wed,fri · weekdays · weekends · all (default)").max_length(60))
        .option(Opt::string("interval", "Interval mode: e.g. 30m, 2h, 1d (5m to 7d)").max_length(20))
        .option(Opt::string("window_start", "Interval mode: daily start HH:MM, or none").max_length(10))
        .option(Opt::string("window_end", "Interval mode: daily stop HH:MM, or none").max_length(10))
        .option(Opt::string("at", "Once mode: 18:00, 2026-12-24 09:00, or a delay like 2h").max_length(40))
        .option(Opt::boolean("require_activity", "Skip if nobody chatted since the last one (MEE6 anti-spam)"))
        .option(Opt::role("ping_role", "Role to ping"));
    if for_edit {
        s = s.option(Opt::boolean("clear_ping_role", "Remove the ping role"));
    }
    s.option(Opt::string("message", "Plain text; skips the editor").max_length(2000))
}

fn name_opt(desc: &str, ac: bool) -> Opt {
    let o = Opt::string("name", desc).required(true).max_length(32);
    if ac { o.autocomplete(true) } else { o.min_length(1) }
}

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("reminder", "Scheduled channel messages (MEE6 Reminders/Timers)")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(schedule_options(Opt::sub_command("create", "Create a reminder, then write its message").option(name_opt("Unique name, e.g. daily-standup", false)), false))
        .option(schedule_options(Opt::sub_command("edit", "Change a reminder; name only = open the message editor").option(name_opt("Reminder", true)), true))
        .option(Opt::sub_command("list", "List this server's reminders"))
        .option(Opt::sub_command("pause", "Pause a reminder").option(name_opt("Reminder", true)))
        .option(Opt::sub_command("resume", "Resume a paused reminder").option(name_opt("Reminder", true)))
        .option(Opt::sub_command("delete", "Delete a reminder").option(name_opt("Reminder", true)))
        .option(Opt::sub_command("test", "Post a reminder here now as a preview (no pings)").option(name_opt("Reminder", true)))
        .option(Opt::sub_command("next", "Show when a reminder fires next").option(name_opt("Reminder", true)))
}

/// The command (`module 'reminders'`, `gated = false`, ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Reminders), execute).gated(false).permission_level(PermissionLevel::Admin).autocomplete(autocomplete)
}

// ── Shared helpers ─────────────────────────────────────────────────────────────

const REMINDER_NAME_MAX: usize = 32;
const UNAVAILABLE_TITLE: &str = "Temporarily Unavailable";

async fn err(ctx: &CommandCtx, title: &str, text: &str) {
    reply_error(&ctx.responder, title, Some(text), true).await;
}

async fn edit(ctx: &CommandCtx, embed: CreateEmbed) {
    safe_reply(&ctx.responder, Payload::embed(embed)).await;
}

fn gid_of(ctx: &CommandCtx) -> anyhow::Result<GuildId> {
    ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/reminder outside a guild"))
}

async fn load_settings(ctx: &CommandCtx) -> Option<Settings> {
    let g = ctx.guild_id()?.to_string();
    if let Some(s) = ctx.app.settings().peek_settings(&g) {
        return Some(s);
    }
    let app = ctx.app.clone();
    let fut = async move { Ok::<_, std::convert::Infallible>(app.settings().get_settings_safe(&g).await) };
    match bot_util::async_util::with_timeout(fut, ctx.ack_budget_ms(), "reminder settings").await {
        Ok(Some(s)) => Some(s),
        _ => {
            err(ctx, UNAVAILABLE_TITLE, "Server settings could not be loaded. Please try again in a moment.").await;
            None
        }
    }
}

async fn lookup_reminder(ctx: &CommandCtx) -> anyhow::Result<Option<Reminder>> {
    let raw = ctx.get_string("name").unwrap_or_default();
    let (pool, g, r) = (ctx.app.db().clone(), gid_of(ctx)?.to_string(), raw.clone());
    match bot_util::async_util::with_timeout(async move { svc::find_reminder_by_name(&pool, &g, &r).await }, ctx.ack_budget_ms(), "reminder lookup").await {
        Err(bot_util::async_util::WithTimeoutError::Timeout(_)) => {
            err(ctx, UNAVAILABLE_TITLE, "That took too long. Please try again.").await;
            Ok(None)
        }
        Err(bot_util::async_util::WithTimeoutError::Inner(e)) => Err(e),
        Ok(None) => {
            err(ctx, "Reminder Not Found", &format!("No reminder named **{}**. See `/reminder list`.", clamp_text(&raw, REMINDER_NAME_MAX))).await;
            Ok(None)
        }
        Ok(Some(r)) => Ok(Some(r)),
    }
}

/// `readScheduleInput(options)`.
fn read_schedule_input(ctx: &CommandCtx) -> ScheduleInput {
    ScheduleInput {
        mode: ctx.get_string("mode"),
        time: ctx.get_string("time"),
        weekdays: ctx.get_string("weekdays"),
        interval: ctx.get_string("interval"),
        window_start: ctx.get_string("window_start"),
        window_end: ctx.get_string("window_end"),
        at: ctx.get_string("at"),
        require_activity: ctx.get_boolean("require_activity"),
    }
}

/// `channelCheck(interaction, channel, { embed })`: answers Missing Permissions and returns false when the bot cannot
/// post there.
async fn channel_check(ctx: &CommandCtx, gid: GuildId, channel: &GuildChannel, embed: bool) -> bool {
    let mut needs = vec!["ViewChannel", "SendMessages"];
    if embed {
        needs.push("EmbedLinks");
    }
    // The channel in hand (it may have just been fetched over REST, like the JS `checkChannelUsable(channel)`).
    let r = check_channel_usable_for(&ctx.app, gid, channel, &needs);
    if r.ok {
        return true;
    }
    let detail = if r.missing.is_empty() {
        r.reason.unwrap_or_else(|| "I can't post in that channel.".into())
    } else {
        format!("I can't post in <#{}>: I need **{}** there.", channel.id, bot_core::permissions::permission_names(&r.missing).join(", "))
    };
    err(ctx, "Missing Permissions", &detail).await;
    false
}

/// `checkPingRole(interaction, role, channel)`: not @everyone, and pingable by the member who sets it.
async fn check_ping_role(ctx: &CommandCtx, gid: GuildId, role: RoleId, channel: &GuildChannel) -> bool {
    if role.get() == gid.get() {
        err(ctx, "Invalid Role", "@everyone can't be used as a ping role.").await;
        return false;
    }
    let refused = match cached_guild(&ctx.app, gid) {
        Some(g) => {
            let roles: Vec<RoleId> = ctx.member().map(|m| m.roles.clone()).unwrap_or_default();
            !pingable_role_ids(&g, channel, Some((ctx.user().id, roles.as_slice())), &[role.to_string()]).refused.is_empty()
        }
        None => true,
    };
    if refused {
        let text = format!("<@&{role}> is not mentionable and you lack **Mention @everyone, @here and All Roles** in <#{}>.", channel.id);
        err(ctx, "Cannot Ping Role", &text).await;
        return false;
    }
    true
}

async fn template_check(ctx: &CommandCtx, tpl: &Value) -> bool {
    let v = validate_template(tpl, true);
    if v.ok {
        return true;
    }
    err(ctx, "Invalid Message", &bot_util::text::join_with_limit(&v.errors, "\n", 1000, bot_util::text::default_more)).await;
    false
}

/// `modalFor(r, name, template)`: the message editor (`reminder-modal:<id>`).
pub fn modal_for(id: &str, name: &str, template: &Value) -> anyhow::Result<CreateModal> {
    let custom_id = bot_config::build_custom_id("reminder-modal", &[id]).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    build_template_modal(&custom_id, Some(&format!("Reminder: {name}")), template, false).map_err(|e| anyhow::anyhow!("{e:?}"))
}

fn summary_text(r: &Reminder, settings: Option<&bot_db::GuildSettings>, next: Option<DateTime<Utc>>, warnings: &[String]) -> String {
    clamp_text(&summary_lines(r, settings, next, warnings).join("\n"), 4096)
}

fn channel_of(ctx: &CommandCtx, gid: GuildId, id: &str) -> Option<GuildChannel> {
    id.parse::<u64>().ok().filter(|v| *v != 0).and_then(|c| cached_channel(&ctx.app, gid, ChannelId::new(c)))
}

// ── create ─────────────────────────────────────────────────────────────────────

async fn create_cmd(ctx: &CommandCtx) -> anyhow::Result<()> {
    let gid = gid_of(ctx)?;
    let name = bot_util::text::sanitize_name(&ctx.get_string("name").unwrap_or_default(), REMINDER_NAME_MAX, "");
    if name.is_empty() {
        { err(ctx, "Invalid Name", "Use letters, numbers, `-` or `_` in the name.").await; return Ok(()); }
    }
    let Some(settings) = load_settings(ctx).await else { return Ok(()) };
    let tz = resolve_time_zone(Some(&settings.timezone));
    let now = ctx.app.clock().now();
    let fields = match svc::validate_reminder_input(&read_schedule_input(ctx), &tz, now, None) {
        Ok(f) => f,
        Err(e) => { err(ctx, "Invalid Schedule", &e).await; return Ok(()); },
    };
    let Some(channel) = ctx.get_channel_id("channel").and_then(|c| cached_channel(&ctx.app, gid, c)) else {
        { err(ctx, "Channel Not Found", "I can't see that channel.").await; return Ok(()); }
    };
    if !channel_check(ctx, gid, &channel, false).await {
        return Ok(());
    }
    let role = ctx.get_role_id("ping_role");
    if let Some(r) = role
        && !check_ping_role(ctx, gid, r, &channel).await
    {
        return Ok(());
    }
    let message = ctx.get_string("message");
    let mut template = default_template();
    if let Some(m) = &message {
        template = normalize_template(&json!({ "content": m }));
        if !template_check(ctx, &template).await {
            return Ok(());
        }
    }
    let args = NewReminder {
        guild_id: gid.to_string(),
        name: name.clone(),
        channel_id: channel.id.to_string(),
        template: template.clone(),
        fields,
        ping_role_id: role.map(|r| r.to_string()),
        created_by: ctx.user().id.to_string(),
        now,
    };

    if message.is_none() {
        // The form path: the reminder is saved first (already active with the default text); the form then edits it.
        let (tx, rx) = tokio::sync::oneshot::channel();
        let app = ctx.app.clone();
        let s2 = settings.clone();
        tokio::spawn(async move {
            match svc::create_reminder_row(&app, args).await {
                Ok(row) => {
                    let _ = tx.send(Ok(row.clone()));
                    // Scheduling runs whether or not the form opens.
                    if let Err(e) = sync_reminder_job(&app, &row, Some(&s2), app.clock().now()).await {
                        bot_util::log_error!("reminder create: scheduling failed", bot_core::error::log_arg(&e));
                    }
                }
                Err(e) => {
                    if e.downcast_ref::<ReminderUserError>().is_none() {
                        bot_util::log_error!("reminder create: scheduling failed", bot_core::error::log_arg(&e));
                    }
                    let _ = tx.send(Err(e));
                }
            }
        });
        let row = match tokio::time::timeout(std::time::Duration::from_millis(ctx.ack_budget_ms()), rx).await {
            Ok(Ok(Ok(row))) => row,
            Ok(Ok(Err(e))) => {
                if let Some(u) = e.downcast_ref::<ReminderUserError>() {
                    { err(ctx, &u.title, &u.message).await; return Ok(()); }
                }
                return Err(e);
            }
            Ok(Err(_)) => anyhow::bail!("reminder create task ended"),
            Err(_) => {
                let text = format!(
                    "Saving took too long, so I could not open the message editor. Check `/reminder list` in a moment, then run `/reminder edit name:{name}` to write the message."
                );
                { err(ctx, "Still Saving", &text).await; return Ok(()); }
            }
        };
        ctx.show_modal(modal_for(&row.id, &name, &default_template())?).await?;
        return Ok(());
    }

    if ctx.defer_reply(false).await.is_err() {
        return Ok(());
    }
    let row = match svc::create_reminder_row(&ctx.app, args).await {
        Ok(r) => r,
        Err(e) => {
            if let Some(u) = e.downcast_ref::<ReminderUserError>() {
                edit(ctx, error_embed(&u.title, Some(&u.message), None)).await;
                return Ok(());
            }
            return Err(e);
        }
    };
    let next = sync_reminder_job(&ctx.app, &row, Some(&settings), ctx.app.clock().now()).await?;
    let warnings = reminder_warnings(&ctx.app, &row, &template);
    edit(ctx, success_embed("Reminder Created", Some(&summary_text(&row, Some(&settings), next, &warnings)), None)).await;
    Ok(())
}

// ── edit ───────────────────────────────────────────────────────────────────────

async fn edit_cmd(ctx: &CommandCtx) -> anyhow::Result<()> {
    let gid = gid_of(ctx)?;
    let Some(r) = lookup_reminder(ctx).await? else { return Ok(()) };
    let provided = ctx.options().iter().filter(|o| o.name != "name").count();
    if provided == 0 {
        // Name only: open the message editor (the modal is the first response).
        ctx.show_modal(modal_for(&r.id, &r.name, &normalize_template(&r.template))?).await?;
        return Ok(());
    }
    let Some(settings) = load_settings(ctx).await else { return Ok(()) };
    let tz = resolve_time_zone(Some(&settings.timezone));
    let now = ctx.app.clock().now();

    let input = read_schedule_input(ctx);
    let mut fields: Option<Schedule> = None;
    if input.any() {
        match svc::validate_reminder_input(&input, &tz, now, Some(&r)) {
            Ok(f) => fields = Some(f),
            Err(e) => { err(ctx, "Invalid Schedule", &e).await; return Ok(()); },
        }
    }
    let current_tpl = normalize_template(&r.template);
    let mut channel = None;
    if let Some(cid) = ctx.get_channel_id("channel") {
        let Some(c) = cached_channel(&ctx.app, gid, cid) else {
            { err(ctx, "Channel Not Found", "I can't see that channel.").await; return Ok(()); }
        };
        if !channel_check(ctx, gid, &c, current_tpl["embed"].is_object()).await {
            return Ok(());
        }
        channel = Some(c);
    }
    let role = ctx.get_role_id("ping_role");
    let clear_role = ctx.get_boolean("clear_ping_role") == Some(true);
    if role.is_some() && clear_role {
        { err(ctx, "Conflicting Options", "Use either `ping_role` or `clear_ping_role`, not both.").await; return Ok(()); }
    }
    if let Some(ro) = role {
        let effective = channel.clone().or_else(|| channel_of(ctx, gid, &r.channel_id));
        let Some(eff) = effective else {
            let text = format!("Its channel no longer exists. Set a new one with `/reminder edit name:{} channel:<#channel>`.", r.name);
            { err(ctx, "Channel Missing", &text).await; return Ok(()); }
        };
        if !check_ping_role(ctx, gid, ro, &eff).await {
            return Ok(());
        }
    }
    let mut template = None;
    if let Some(m) = ctx.get_string("message") {
        let mut t = current_tpl.clone();
        t["content"] = json!(m);
        let t = normalize_template(&t);
        if !template_check(ctx, &t).await {
            return Ok(());
        }
        template = Some(t);
    }

    if ctx.defer_reply(false).await.is_err() {
        return Ok(());
    }
    let mut update = ReminderUpdate { fields, template: template.clone(), ..Default::default() };
    if let Some(c) = &channel {
        update.channel_id = Some(c.id.to_string());
        if c.id.to_string() != r.channel_id {
            update.last_message_id = Some(None);
        }
    }
    if clear_role {
        update.ping_role_id = Some(None);
    } else if let Some(ro) = role {
        update.ping_role_id = Some(Some(ro.to_string()));
    }
    let updated = svc::update_reminder(&ctx.app, &r.id, &update).await?;
    let next = if updated.enabled { sync_reminder_job(&ctx.app, &updated, Some(&settings), ctx.app.clock().now()).await? } else { None };
    let warnings = reminder_warnings(&ctx.app, &updated, template.as_ref().unwrap_or(&current_tpl));
    edit(ctx, success_embed("Reminder Updated", Some(&summary_text(&updated, Some(&settings), next, &warnings)), None)).await;
    Ok(())
}

// ── list ───────────────────────────────────────────────────────────────────────

/// `isDone(r)`: a one-time reminder that was sent.
pub fn is_done(r: &Reminder) -> bool {
    !r.enabled && r.mode == ReminderMode::OneShot && matches!((r.last_sent_at, r.run_at), (Some(s), Some(a)) if s >= a)
}

async fn list_cmd(ctx: &CommandCtx) -> anyhow::Result<()> {
    let gid = gid_of(ctx)?;
    let Some(settings) = load_settings(ctx).await else { return Ok(()) };
    let tz = resolve_time_zone(Some(&settings.timezone));
    if ctx.defer_reply(false).await.is_err() {
        return Ok(());
    }
    let rows = svc::list_reminders(ctx.app.db(), &gid.to_string(), 200).await?;
    if rows.is_empty() {
        edit(ctx, info_embed("Reminders", Some("No reminders yet. Create one with `/reminder create`."), None)).await;
        return Ok(());
    }
    let now = ctx.app.clock().now();
    let entries: Vec<String> = rows
        .iter()
        .map(|r| {
            let done = is_done(r);
            let usable = r
                .channel_id
                .parse::<u64>()
                .ok()
                .filter(|v| *v != 0)
                .is_some_and(|c| check_channel_usable(&ctx.app, gid, Some(ChannelId::new(c)), &["ViewChannel", "SendMessages"]).ok);
            let icon = if done {
                "✅"
            } else if !r.enabled {
                "⏸️"
            } else if !usable {
                "⚠️"
            } else {
                "▶️"
            };
            let mut next_text = if r.enabled {
                match compute_next_run(&Schedule::from(r), &tz, now) {
                    Some(n) => format!("next {}", bot_util::time::discord_timestamp(&n, "R")),
                    None => "next —".into(),
                }
            } else if done {
                format!("sent {}", r.last_sent_at.map(|s| bot_util::time::discord_timestamp(&s, "R")).unwrap_or_default())
            } else {
                "paused".into()
            };
            if let Some(s) = r.last_sent_at.filter(|_| !done) {
                next_text.push_str(&format!(" · last sent {}", bot_util::time::discord_timestamp(&s, "R")));
            }
            let head = format!(
                "{icon} **{}** · <#{}> · {}{}{}",
                r.name,
                r.channel_id,
                describe_schedule(&Schedule::from(r)),
                if r.require_activity { " · 💬" } else { "" },
                r.ping_role_id.as_deref().filter(|p| !p.is_empty()).map(|p| format!(" · <@&{p}>")).unwrap_or_default()
            );
            format!("{head}\n↳ {next_text}")
        })
        .collect();
    let off = module_off_line(Some(&settings));
    let footer = format!("{}/{} reminders · Timezone: {tz}", rows.len(), bot_config::limits::REMINDERS);
    let pages: Vec<CreateEmbed> = bot_util::text::chunk_lines(&entries, 3500)
        .into_iter()
        .map(|chunk| {
            let desc = match off {
                Some(o) => format!("{o}\n\n{chunk}"),
                None => chunk,
            };
            guild_embed(Some(&settings), Some("⏰ Reminders"), Some(&desc), Some(&footer))
        })
        .collect();
    if pages.len() == 1 {
        safe_reply(&ctx.responder, Payload::new().embeds(pages)).await;
        return Ok(());
    }
    bot_core::paginator::paginate(ctx, &pages, bot_core::paginator::PaginateOptions::default()).await?;
    Ok(())
}

// ── pause / resume / delete ────────────────────────────────────────────────────

async fn pause_cmd(ctx: &CommandCtx) -> anyhow::Result<()> {
    let Some(r) = lookup_reminder(ctx).await? else { return Ok(()) };
    if is_done(&r) {
        let text = format!("**{}** is a one-time reminder and has already been sent, so there is nothing to pause.", r.name);
        { err(ctx, "Already Sent", &text).await; return Ok(()); }
    }
    if !r.enabled {
        { err(ctx, "Already Paused", &format!("**{}** is already paused.", r.name)).await; return Ok(()); }
    }
    if ctx.defer_reply(false).await.is_err() {
        return Ok(());
    }
    svc::pause_reminder(&ctx.app, &r).await?;
    edit(ctx, success_embed("Reminder Paused", Some(&format!("**{}** will not send until you run `/reminder resume`.", r.name)), None)).await;
    Ok(())
}

async fn resume_cmd(ctx: &CommandCtx) -> anyhow::Result<()> {
    let gid = gid_of(ctx)?;
    let Some(r) = lookup_reminder(ctx).await? else { return Ok(()) };
    let Some(settings) = load_settings(ctx).await else { return Ok(()) };
    let tz = resolve_time_zone(Some(&settings.timezone));
    let now = ctx.app.clock().now();
    let tpl = normalize_template(&r.template);
    if r.mode == ReminderMode::OneShot && r.run_at.is_none_or(|a| a.timestamp_millis() <= now.timestamp_millis() + 60_000) {
        let text = format!("That one-time reminder's time has passed. Set a new time with `/reminder edit name:{} at:<when>`, then resume it.", r.name);
        { err(ctx, "Time Has Passed", &text).await; return Ok(()); }
    }
    let mut channel = channel_of(ctx, gid, &r.channel_id);
    if channel.is_none()
        && let Some(cid) = r.channel_id.parse::<u64>().ok().filter(|v| *v != 0).map(ChannelId::new)
    {
        let app = ctx.app.clone();
        let fut = async move { crate::net::fetch_guild_channel(&app, gid, cid).await };
        match bot_util::async_util::with_timeout(fut, ctx.ack_budget_ms(), "reminder channel fetch").await {
            Ok(c) => channel = Some(c),
            Err(bot_util::async_util::WithTimeoutError::Inner(e)) if matches!(e.code, Some(10003) | Some(50001)) => channel = None,
            Err(_) => { err(ctx, UNAVAILABLE_TITLE, "That took too long. Please try again.").await; return Ok(()); },
        }
    }
    let Some(channel) = channel else {
        let text = format!("Its channel no longer exists. Set a new one with `/reminder edit name:{} channel:<#channel>`.", r.name);
        { err(ctx, "Channel Missing", &text).await; return Ok(()); }
    };
    if !channel_check(ctx, gid, &channel, tpl["embed"].is_object()).await {
        return Ok(());
    }
    if compute_next_run(&Schedule::from(&r), &tz, now).is_none() {
        { err(ctx, "Never Fires", "This schedule never fires. Check the weekdays and the time window with `/reminder edit`.").await; return Ok(()); }
    }
    if ctx.defer_reply(false).await.is_err() {
        return Ok(());
    }
    // On an enabled reminder this is a refresh: it re-syncs the job (after /settings timezone, for example).
    let refresh = r.enabled;
    let updated = if refresh { r.clone() } else { svc::update_reminder(&ctx.app, &r.id, &ReminderUpdate { enabled: Some(true), ..Default::default() }).await? };
    let next = sync_reminder_job(&ctx.app, &updated, Some(&settings), ctx.app.clock().now()).await?;
    let warnings = reminder_warnings(&ctx.app, &updated, &tpl);
    edit(ctx, success_embed(if refresh { "Schedule Refreshed" } else { "Reminder Resumed" }, Some(&summary_text(&updated, Some(&settings), next, &warnings)), None)).await;
    Ok(())
}

async fn delete_cmd(ctx: &CommandCtx) -> anyhow::Result<()> {
    let Some(r) = lookup_reminder(ctx).await? else { return Ok(()) };
    let ok = await_confirm(
        ctx,
        ConfirmOptions {
            title: Some("Delete reminder?".into()),
            description: Some(format!("**{}** in <#{}> will be deleted. This can't be undone.", r.name, r.channel_id)),
            confirm_label: "Delete".into(),
            ephemeral: true,
            ..Default::default()
        },
    )
    .await;
    if !ok {
        return Ok(());
    }
    svc::delete_reminder(&ctx.app, &r).await?;
    let _ = ctx
        .follow_up(serenity::builder::CreateInteractionResponseFollowup::new().embed(success_embed("Reminder Deleted", Some(&format!("**{}** was deleted.", r.name)), None)))
        .await;
    Ok(())
}

// ── test / next ────────────────────────────────────────────────────────────────

async fn test_cmd(ctx: &CommandCtx) -> anyhow::Result<()> {
    let gid = gid_of(ctx)?;
    let Some(r) = lookup_reminder(ctx).await? else { return Ok(()) };
    if ctx.defer_reply(true).await.is_err() {
        return Ok(());
    }
    let g = gid.to_string();
    let settings = match ctx.app.settings().peek_settings(&g) {
        Some(s) => Some(s),
        None => ctx.app.settings().get_settings_safe(&g).await,
    };
    // A preview never touches lastSentAt, lastMessageId or the job, and works while the module is off.
    let guild = cached_guild(&ctx.app, gid);
    let here = cached_channel(&ctx.app, gid, ctx.channel_id());
    let target = channel_of(ctx, gid, &r.channel_id).or_else(|| here.clone());
    let payload = svc::build_send_payload(&ctx.app, &r, guild.as_ref(), target.as_ref(), settings.as_deref(), false);
    let mut needs = vec!["ViewChannel", "SendMessages"];
    if !payload.embeds.is_empty() {
        needs.push("EmbedLinks");
    }
    if check_channel_usable(&ctx.app, gid, Some(ctx.channel_id()), &needs).ok {
        match crate::net::rest(&ctx.app).send_message(ctx.app.clone(), ctx.channel_id(), payload.to_json(None)).await {
            Ok(_) => {
                let text = format!("Posted a preview of **{}** here. Scheduled sends go to <#{}>; role pings are suppressed in tests.", r.name, r.channel_id);
                edit(ctx, success_embed("Test Sent", Some(&text), None)).await;
                return Ok(());
            }
            Err(e) => bot_util::log_warn!(format!("reminder test: send failed in {}: {}", ctx.channel_id(), e.message)),
        }
    }
    let mut embeds: Vec<CreateEmbed> = payload.embeds.iter().map(bot_core::custom_embed::embed_from_json).collect();
    embeds.push(warn_embed("Preview Only", Some("I can't post in this channel, so here is a private preview."), None));
    embeds.truncate(10);
    let rows: Vec<serenity::builder::CreateActionRow> =
        payload.components.iter().filter_map(|v| serde_json::from_value::<serenity::all::ActionRow>(v.clone()).ok()).map(action_row_builder).collect();
    let mut p = Payload::new().embeds(embeds).components(rows).no_mentions();
    if let Some(c) = payload.content {
        p = p.content(c);
    }
    safe_reply(&ctx.responder, p).await;
    Ok(())
}

/// A link-button row as a serenity builder.
pub fn action_row_builder(row: serenity::all::ActionRow) -> serenity::builder::CreateActionRow {
    let buttons = row
        .components
        .into_iter()
        .filter_map(|c| match c {
            serenity::all::ActionRowComponent::Button(b) => {
                let serenity::all::ButtonKind::Link { url } = b.data else { return None };
                let mut btn = serenity::builder::CreateButton::new_link(url);
                if let Some(l) = b.label {
                    btn = btn.label(l);
                }
                if let Some(e) = b.emoji {
                    btn = btn.emoji(e);
                }
                Some(btn)
            }
            _ => None,
        })
        .collect();
    serenity::builder::CreateActionRow::Buttons(buttons)
}

async fn next_cmd(ctx: &CommandCtx) -> anyhow::Result<()> {
    let Some(r) = lookup_reminder(ctx).await? else { return Ok(()) };
    let Some(settings) = load_settings(ctx).await else { return Ok(()) };
    let tz = resolve_time_zone(Some(&settings.timezone));
    if ctx.defer_reply(false).await.is_err() {
        return Ok(());
    }
    let schedule = Schedule::from(&r);
    let mut lines = vec![format!("**{}** → <#{}>", r.name, r.channel_id), describe_schedule(&schedule), format!("Timezone: {tz}")];
    if !r.enabled {
        lines.push(if is_done(&r) {
            format!("✅ Sent {}", r.last_sent_at.map(|s| bot_util::time::discord_timestamp(&s, "R")).unwrap_or_default())
        } else {
            "⏸️ Paused — no upcoming runs.".into()
        });
    } else {
        let job = ctx.app.scheduler().get_job(&job_key(&r.id)).await;
        let next = match job.filter(|j| matches!(j.status, bot_db::JobStatus::Pending | bot_db::JobStatus::Running)) {
            Some(j) => Some(j.run_at),
            None => {
                let n = sync_reminder_job(&ctx.app, &r, Some(&settings), ctx.app.clock().now()).await?;
                lines.push("🔧 The schedule was repaired.".into());
                n
            }
        };
        match next {
            Some(n) => {
                lines.push(format!("Next: {} ({})", bot_util::time::discord_timestamp(&n, "F"), bot_util::time::discord_timestamp(&n, "R")));
                if r.mode != ReminderMode::OneShot {
                    let mut upcoming = Vec::new();
                    let mut prev = n;
                    for _ in 0..3 {
                        let Some(p) = compute_next_run(&schedule, &tz, prev) else { break };
                        upcoming.push(bot_util::time::discord_timestamp(&p, "f"));
                        prev = p;
                    }
                    if !upcoming.is_empty() {
                        lines.push(format!("Upcoming: {}", upcoming.join(" · ")));
                    }
                }
            }
            None => lines.push("Next: —".into()),
        }
        if r.require_activity {
            lines.push("💬 Skipped when nobody has chatted since the last reminder.".into());
        }
    }
    if let Some(off) = module_off_line(Some(&settings)) {
        lines.push(off.into());
    }
    edit(ctx, guild_embed(Some(&settings), Some("⏰ Next run"), Some(&lines.join("\n")), None)).await;
    Ok(())
}

// ── Dispatch ───────────────────────────────────────────────────────────────────

async fn execute(ctx: CommandCtx) -> BotResult {
    let sub = ctx.subcommand().unwrap_or_default();
    let result = match sub.as_str() {
        "create" => create_cmd(&ctx).await,
        "edit" => edit_cmd(&ctx).await,
        "list" => list_cmd(&ctx).await,
        "pause" => pause_cmd(&ctx).await,
        "resume" => resume_cmd(&ctx).await,
        "delete" => delete_cmd(&ctx).await,
        "test" => test_cmd(&ctx).await,
        "next" => next_cmd(&ctx).await,
        _ => {
            err(&ctx, "Unknown Subcommand", "That subcommand does not exist.").await;
            Ok(())
        }
    };
    if let Err(e) = result {
        // `if (isInteractionGone(err)) return undefined;`
        if e.downcast_ref::<bot_core::interaction::InteractionError>().is_some_and(bot_core::interaction_helpers::is_interaction_gone) {
            return Ok(());
        }
        bot_util::log_error!(format!("reminder {sub} failed"), bot_core::error::log_arg(&e));
        reply_error(&ctx.responder, "Something went wrong", Some("I could not complete that. Please try again."), true).await;
    }
    Ok(())
}

// ── Autocomplete ───────────────────────────────────────────────────────────────

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let Some((name, value)) = ctx.focused() else {
        respond_autocomplete(&ctx.responder, &[]).await;
        return Ok(());
    };
    let Some(gid) = ctx.guild_id().filter(|_| name == "name") else {
        respond_autocomplete(&ctx.responder, &[]).await;
        return Ok(());
    };
    let pause = ctx.subcommand().as_deref() == Some("pause");
    let q = bot_util::text::sanitize_name(&value, REMINDER_NAME_MAX, "");
    let (pool, g) = (ctx.app.db().clone(), gid.to_string());
    let rows = bot_util::async_util::with_timeout(async move { svc::search_reminders(&pool, &g, &q, pause).await }, 2000, "reminder autocomplete").await.unwrap_or_default();
    let choices: Vec<Choice> =
        rows.iter().map(|r| Choice::new(format!("{} {} · {}", if r.enabled { "▶" } else { "⏸" }, r.name, mode_label(r.mode)), r.name.clone())).collect();
    respond_autocomplete(&ctx.responder, &choices).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn link_rows_keep_their_label_url_and_emoji() {
        let v = serde_json::json!({ "type": 1, "components": [{ "type": 2, "style": 5, "label": "Go", "url": "https://go.test/", "emoji": { "name": "👍" } }] });
        let row: serenity::all::ActionRow = serde_json::from_value(v).expect("an action row");
        let built = serde_json::to_value(super::action_row_builder(row)).unwrap_or_default();
        assert_eq!(built["components"][0]["label"], "Go");
        assert_eq!(built["components"][0]["url"], "https://go.test/");
        assert_eq!(built["components"][0]["emoji"]["name"], "👍");
    }
}
