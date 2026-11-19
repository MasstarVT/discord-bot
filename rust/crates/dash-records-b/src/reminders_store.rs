//! Port of `src/dashboard/collections/reminders.store.js`: the Reminder rows /reminder writes, validated by the same
//! validate_reminder_input, scheduled by the same sync_reminder_job (`reminder.fire`, dedupe `reminder:{id}`, in the
//! server's time zone) and removed by delete_reminder. The cap and unique name are guarded by a per-guild advisory lock.

use async_trait::async_trait;
use bot_config::tables::limits;
use bot_core::App;
use bot_core::custom_embed::{RenderOptions, normalize_template, render_template};
use bot_core::scheduler::CancelJobs;
use bot_db::{GuildSettings, InsertBuilder, REMINDER_INFO, Reminder, SelectBuilder, UpdateBuilder};
use chrono::{DateTime, Utc};
use dash_collections::channel_access::{ChannelCheck, SEND_NEEDS, require_channel_access};
use dash_collections::collections::embeds::{
    DiscordOp, FailureInfo, SLUG_MESSAGE, discord_failure_of, member_for, ping_role_problem, render_context_for, rest_allowed_mentions,
    to_rest_message,
};
use dash_collections::collections::reminders::{reminder_item, schedule_input};
use dash_collections::{
    ActionAnswer, DiscordResources, ListAnswer, ListOpts, ReadCtx, RecordError, RecordErrorCode, RecordStore, Saved, StoreCtx, StoreError,
};
use feat_feeds::reminder_service as rs;
use serde_json::{Map, Value, json};
use std::sync::Arc;

use crate::common::{OrInternal, R, actor_id, is_unique, load_resources, lock, now, rec_at, row_json, truthy};

/// Raw statements (test/db.test.js runs each one): (name, SQL, sample bind).
pub const RAW_SQL: [(&str, &str, &str); 1] = [("dashboard.reminders.lockGuild", crate::common::LOCK_SQL, "reminder-create:0")];

fn not_found() -> RecordError {
    RecordError::not_found("That reminder no longer exists.")
}

async fn settings_of(app: &App, guild_id: &str) -> R<Arc<GuildSettings>> {
    app.settings()
        .get_settings_safe(guild_id)
        .await
        .ok_or_else(|| RecordError::unavailable("The server settings could not be loaded. Try again in a moment.").into())
}

async fn resources_of(ctx: &StoreCtx, guild_id: &str) -> R<Arc<DiscordResources>> {
    load_resources(
        ctx,
        guild_id,
        "The server cannot be read from the dashboard right now.",
        "The server could not be loaded from Discord. Try again in a moment.",
        &[],
    )
    .await
}

/// `OPTION_LABELS`: validateReminderInput speaks in slash-option names; the form shows labels.
fn option_label(name: &str) -> Option<&'static str> {
    Some(match name {
        "time" => "time of day",
        "at" => "date and time",
        "interval" => "interval",
        "mode" => "repeat mode",
        "weekdays" => "days",
        "window_start" => "daily start",
        "window_end" => "daily stop",
        _ => return None,
    })
}

/// `ERROR_PATHS`: first match wins ("With a daily window the interval must be under 24 hours" is about the interval).
fn error_path(message: &str) -> Option<&'static str> {
    let m = message.to_lowercase();
    let any = |words: &[&str]| words.iter().any(|w| m.contains(w));
    if any(&["interval"]) {
        Some("intervalMinutes")
    } else if any(&["window"]) {
        Some("windowEnd")
    } else if any(&["time of day", "time mode"]) {
        Some("timeOfDay")
    } else if any(&["days", "weekdays", "never fires"]) {
        Some("weekdays")
    } else if any(&["that time", "once mode", "has passed", "within one year"]) {
        Some("runAt")
    } else {
        None
    }
}

/// `scheduleError(message)`: `name` in backticks becomes the field label, and the field is highlighted.
pub fn schedule_error(message: &str) -> StoreError {
    let mut text = String::new();
    let mut rest = message;
    while let Some(start) = rest.find('`') {
        let after = &rest[start + 1..];
        match after.find('`') {
            Some(end) if end > 0 => {
                let inner = &after[..end];
                text.push_str(&rest[..start]);
                text.push_str(option_label(inner).unwrap_or(inner));
                rest = &after[end + 1..];
            }
            _ => {
                text.push_str(&rest[..=start]);
                rest = after;
            }
        }
    }
    text.push_str(rest);
    let err = RecordError::invalid(text);
    match error_path(message) {
        Some(p) => err.with_path(p).into(),
        None => err.into(),
    }
}

/// The form schedule (`scheduleInput(s)`) as the raw /reminder options.
fn to_input(input: &Map<String, Value>) -> rs::ScheduleInput {
    let text = |k: &str| match input.get(k) {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => Some(dash_schema::js::to_js_string(Some(other))),
    };
    rs::ScheduleInput {
        mode: text("mode"),
        time: text("time"),
        weekdays: text("weekdays"),
        interval: text("interval"),
        window_start: text("windowStart"),
        window_end: text("windowEnd"),
        at: text("at"),
        require_activity: input.get("requireActivity").filter(|v| !v.is_null()).map(|v| truthy(Some(v))),
    }
}

/// `validateSchedule(s, { tz, now, base })`: the schedule columns.
fn validate_schedule(s: &Map<String, Value>, tz: &str, now: DateTime<Utc>, base: Option<&Reminder>) -> R<rs::Schedule> {
    rs::validate_reminder_input(&to_input(&schedule_input(s)), tz, now, base).map_err(|e| schedule_error(&e))
}

/// The `fields` of validateReminderInput as Prisma data.
fn schedule_fields(s: &rs::Schedule) -> Vec<(&'static str, Value)> {
    vec![
        ("mode", json!(s.mode)),
        ("timeOfDay", json!(s.time_of_day)),
        ("weekdays", json!(s.weekdays)),
        ("intervalMinutes", json!(s.interval_minutes)),
        ("windowStart", json!(s.window_start)),
        ("windowEnd", json!(s.window_end)),
        ("runAt", s.run_at.as_ref().map_or(Value::Null, |d| json!(bot_db::format_js_date(d)))),
        ("requireActivity", json!(s.require_activity)),
    ]
}

async fn check_ping_role(guild_id: &str, role_id: &str, ctx: &StoreCtx) -> R<()> {
    let resources = resources_of(ctx, guild_id).await?;
    let res: &DiscordResources = &resources;
    if let Some(problem) = ping_role_problem(Some(res), role_id, member_for(ctx, Some(res)).as_ref()) {
        return Err(rec_at(RecordErrorCode::Invalid, problem, "pingRoleId"));
    }
    Ok(())
}

fn tz_of(settings: Option<&GuildSettings>) -> String {
    rs::resolve_time_zone(settings.map(|s| s.timezone.as_str()))
}

/// `toItem(row, settings, now)`.
fn to_item(row: &Reminder, settings: Option<&GuildSettings>, now: DateTime<Utc>) -> Value {
    let tz = tz_of(settings);
    let next = if row.enabled { rs::compute_next_run(&rs::Schedule::from(row), &tz, now) } else { None };
    let next = next.map(|d| json!(bot_db::format_js_date(&d)));
    reminder_item(&row_json(row), &tz, next.as_ref())
}

/// `JSON.stringify(x)` (key order counts).
fn stringify(m: &Map<String, Value>) -> String {
    serde_json::to_string(m).unwrap_or_default()
}

/// The `reminders` store.
pub struct RemindersStore {
    app: App,
}

impl RemindersStore {
    pub fn new(app: App) -> Self {
        Self { app }
    }

    async fn find(&self, guild_id: &str, id: &str) -> R<Option<Reminder>> {
        SelectBuilder::new(&REMINDER_INFO)
            .filter("id", json!(id))
            .oi()?
            .filter("guildId", json!(guild_id))
            .oi()?
            .limit(1)
            .build()
            .oi()?
            .fetch_optional::<Reminder, _>(self.app.db())
            .await
            .oi()
    }

    async fn find_row(&self, guild_id: &str, id: &str) -> R<Reminder> {
        self.find(guild_id, id).await?.ok_or_else(|| not_found().into())
    }

    /// `buildSendPayload(row, { guild, channel, settings, ping: false })` with the render context of the dashboard
    /// (renderContextFor: the guild and channel from getGuildResources).
    fn preview_payload(&self, row: &Reminder, resources: &DiscordResources, channel_id: &str, settings: Option<&GuildSettings>) -> Value {
        let ctx = render_context_for(Some(resources), Some(channel_id), None, None);
        let color = settings.and_then(|s| s.embed_color).map(|c| c as u32).unwrap_or(bot_config::colors::INFO);
        let opts = || RenderOptions { default_color: Some(color), allowed_mentions: None, extra_components: Vec::new() };
        let tpl = normalize_template(&row.template);
        let rendered = render_template(&tpl, &ctx, opts()).or_else(|| render_template(&rs::default_template(), &ctx, opts()));
        let mut json = rendered.map_or_else(|| json!({ "embeds": [], "components": [] }), |r| r.to_json());
        if let Some(role) = row.ping_role_id.as_deref().filter(|r| !r.is_empty()) {
            let content = json.get("content").and_then(Value::as_str).unwrap_or("").to_string();
            let text = format!("<@&{role}> {content}");
            json["content"] = json!(bot_util::text::clamp_text(bot_util::js::trim_end(&text), 2000));
        }
        to_rest_message(Some(&json), false, Some(rest_allowed_mentions(false, &[])))
    }
}

#[async_trait]
impl RecordStore for RemindersStore {
    async fn list(&self, guild_id: &str, _opts: &ListOpts, _ctx: &ReadCtx) -> R<ListAnswer> {
        let rows = SelectBuilder::new(&REMINDER_INFO)
            .filter("guildId", json!(guild_id))
            .oi()?
            .order_by("name", true)
            .oi()?
            .limit(limits::REMINDERS as i64)
            .build()
            .oi()?
            .fetch_all::<Reminder, _>(self.app.db())
            .await
            .oi()?;
        let settings = self.app.settings().get_settings_safe(guild_id).await;
        let now = now(&self.app);
        Ok(ListAnswer::items(rows.iter().map(|r| to_item(r, settings.as_deref(), now)).collect()))
    }

    async fn get(&self, guild_id: &str, id: &str, _ctx: &ReadCtx) -> R<Option<Value>> {
        let Some(row) = self.find(guild_id, id).await? else { return Ok(None) };
        let settings = self.app.settings().get_settings_safe(guild_id).await;
        Ok(Some(to_item(&row, settings.as_deref(), now(&self.app))))
    }

    async fn create(&self, guild_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> R<Saved> {
        let name = match data.get("name") {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        if !data.get("name").is_some_and(Value::is_string) || bot_util::text::sanitize_name(&name, 32, "") != name {
            return Err(rec_at(RecordErrorCode::Invalid, SLUG_MESSAGE, "name"));
        }
        self.app.settings().ensure_guild_settings(guild_id).await.oi()?;
        let settings = settings_of(&self.app, guild_id).await?;
        let tz = tz_of(Some(&settings));
        let now = now(&self.app);
        let fields = validate_schedule(&data, &tz, now, None)?;
        if let Some(role) = data.get("pingRoleId").filter(|v| truthy(Some(v))).and_then(Value::as_str) {
            check_ping_role(guild_id, role, ctx).await?;
        }
        let template = normalize_template(data.get("template").unwrap_or(&Value::Null));

        let mut values: Vec<(&'static str, Value)> =
            vec![("guildId", json!(guild_id)), ("name", json!(name)), ("channelId", data.get("channelId").cloned().unwrap_or(Value::Null))];
        values.push(("template", template));
        values.extend(schedule_fields(&fields));
        values.push(("pingRoleId", data.get("pingRoleId").cloned().unwrap_or(Value::Null)));
        values.push(("createdBy", json!(actor_id(ctx))));
        values.push(("enabled", json!(data.get("enabled") != Some(&Value::Bool(false)))));
        // createdAt is set explicitly: it anchors the INTERVAL grid (createReminderRow does the same).
        values.push(("createdAt", json!(bot_db::format_js_date(&now))));
        let q = InsertBuilder::new(&REMINDER_INFO).values(values).oi()?.build().oi()?;
        let key = format!("reminder-create:{guild_id}");
        let gid = guild_id.to_string();
        let result = bot_db::transaction(self.app.db(), bot_db::TxOptions::default(), move |tx| {
            Box::pin(async move {
                lock(&mut *tx, &key).await?;
                let (n,): (i64,) =
                    sqlx::query_as(r#"SELECT COUNT(*)::bigint FROM "Reminder" WHERE "guildId" = $1"#).bind(&gid).fetch_one(&mut *tx).await?;
                if n >= limits::REMINDERS as i64 {
                    return Err(anyhow::Error::from(RecordError::new(
                        RecordErrorCode::Limit,
                        format!("This server already has the maximum of {} reminders. Delete one first.", limits::REMINDERS),
                    )));
                }
                Ok::<Reminder, anyhow::Error>(q.fetch_one(&mut *tx).await?)
            })
        })
        .await;
        let row = match result {
            Ok(r) => r,
            Err(err) => {
                if let Some(e) = err.downcast_ref::<RecordError>() {
                    return Err(e.clone().into());
                }
                if is_unique(&err) {
                    return Err(rec_at(RecordErrorCode::Conflict, format!("A reminder named \"{name}\" already exists."), "name"));
                }
                return Err(err.into());
            }
        };
        rs::sync_reminder_job(&self.app, &row, Some(&settings), now).await.oi()?;
        Ok(Saved::item(to_item(&row, Some(&settings), now)))
    }

    async fn update(&self, guild_id: &str, id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> R<Saved> {
        let row = self.find_row(guild_id, id).await?;
        let settings = settings_of(&self.app, guild_id).await?;
        let tz = tz_of(Some(&settings));
        let now = now(&self.app);
        let before = match reminder_item(&row_json(&row), &tz, None) {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        let mut merged = before.clone();
        for (k, v) in &data {
            merged.insert(k.clone(), v.clone());
        }
        let mut patch: Vec<(&'static str, Value)> = Vec::new();
        let mut schedule: Option<rs::Schedule> = None;

        // The schedule is validated only when it changed (compared as the options of its mode, so the order of the days
        // or the hidden fields of other modes do not count), so a finished one-off can still get a new channel or text.
        if stringify(&schedule_input(&merged)) != stringify(&schedule_input(&before)) {
            let s = validate_schedule(&merged, &tz, now, Some(&row))?;
            patch.extend(schedule_fields(&s));
            schedule = Some(s);
        }
        let mut channel_id = row.channel_id.clone();
        if let Some(c) = data.get("channelId")
            && c.as_str() != Some(row.channel_id.as_str())
        {
            patch.push(("channelId", c.clone()));
            patch.push(("lastMessageId", Value::Null)); // the activity gate's baseline belongs to the old channel
            channel_id = c.as_str().unwrap_or_default().to_string();
        }
        if data.contains_key("template") {
            patch.push(("template", normalize_template(data.get("template").unwrap_or(&Value::Null))));
        }
        if let Some(p) = data.get("pingRoleId")
            && *p != json!(row.ping_role_id)
        {
            if let Some(role) = p.as_str().filter(|r| !r.is_empty()) {
                check_ping_role(guild_id, role, ctx).await?;
            }
            patch.push(("pingRoleId", p.clone()));
        }
        if let Some(e) = data.get("enabled")
            && *e != json!(row.enabled)
        {
            if truthy(Some(e)) {
                // /reminder resume: a one-off whose time passed, a schedule that never fires or a deleted channel stay paused.
                let mut next = schedule.clone().unwrap_or_else(|| rs::Schedule::from(&row));
                next.created_at = Some(row.created_at);
                if next.mode == bot_db::ReminderMode::OneShot && next.run_at.is_none_or(|r| r.timestamp_millis() <= now.timestamp_millis() + 60_000) {
                    return Err(rec_at(
                        RecordErrorCode::Invalid,
                        "That one-time reminder's time has passed. Pick a new date and time, then turn it on.",
                        "runAt",
                    ));
                }
                if rs::compute_next_run(&next, &tz, now).is_none() {
                    return Err(rec_at(RecordErrorCode::Invalid, "This schedule never fires. Check the days and the daily window.", "weekdays"));
                }
                let resources = resources_of(ctx, guild_id).await?;
                if resources.channel(&channel_id).is_none() {
                    return Err(rec_at(RecordErrorCode::Invalid, "Its channel no longer exists. Pick a new channel first.", "channelId"));
                }
            }
            patch.push(("enabled", e.clone()));
        }

        if !patch.is_empty() {
            let count = UpdateBuilder::new(&REMINDER_INFO)
                .set_all(patch)
                .oi()?
                .filter("id", json!(id))
                .oi()?
                .filter("guildId", json!(guild_id))
                .oi()?
                .without_returning()
                .build()
                .oi()?
                .execute(self.app.db())
                .await
                .oi()?;
            if count == 0 {
                return Err(not_found().into());
            }
        }
        let updated = self.find_row(guild_id, id).await?;
        // Make the job match the row, as every /reminder write does (pause cancels it).
        if updated.enabled {
            rs::sync_reminder_job(&self.app, &updated, Some(&settings), now).await.oi()?;
        } else {
            self.app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(rs::job_key(&updated.id)), ..Default::default() }).await;
        }
        Ok(Saved::item(to_item(&updated, Some(&settings), now)))
    }

    async fn remove(&self, guild_id: &str, id: &str, _ctx: &StoreCtx) -> R<()> {
        let row = self.find_row(guild_id, id).await?;
        rs::delete_reminder(&self.app, &row).await.oi() // guild-scoped delete + cancels `reminder:{id}`
    }

    fn action_ids(&self) -> &[&'static str] {
        &["test"]
    }

    async fn action(&self, guild_id: &str, id: &str, action_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> R<ActionAnswer> {
        if action_id != "test" {
            return Err(RecordError::not_found("Unknown action.").into());
        }
        // /reminder test: posts a preview now (no pings, the schedule, lastSentAt and the job are untouched).
        let Some(discord) = crate::common::discord_with(ctx, dash_collections::DiscordMethod::SendMessage).cloned() else {
            return Err(RecordError::unavailable("Posting messages from the dashboard is not available right now.").into());
        };
        let row = self.find_row(guild_id, id).await?;
        let resources = resources_of(ctx, guild_id).await?;
        let channel_id = data.get("channelId").filter(|v| !v.is_null()).and_then(Value::as_str).map_or_else(|| row.channel_id.clone(), str::to_string);
        let Some(channel) = resources.channel(&channel_id).cloned() else {
            return Err(rec_at(RecordErrorCode::Invalid, "That channel no longer exists. Pick another one.", "channelId"));
        };
        // The bot posts now, in any channel the form names: the user needs to be able to post there themselves.
        require_channel_access(ctx, &[ChannelCheck::new(channel_id.clone()).needs(&SEND_NEEDS).path("channelId")], "").await?;
        let settings = self.app.settings().get_settings_safe(guild_id).await;
        let body = self.preview_payload(&row, &resources, &channel_id, settings.as_deref());
        let has_embeds = body.get("embeds").and_then(Value::as_array).is_some_and(|e| !e.is_empty());
        if let Err(err) = discord.send_message(&channel_id, body).await {
            let info = FailureInfo { op: DiscordOp::Send, channel_name: Some(&channel.name), channel_id: Some(&channel.id), embeds: has_embeds };
            return Err(discord_failure_of(&err, &info).into());
        }
        Ok(ActionAnswer::new(
            to_item(&row, settings.as_deref(), now(&self.app)),
            format!("Posted a test of \"{}\" in #{}. Roles were not pinged.", row.name, channel.name),
        ))
    }
}
