//! Port of `src/services/reminderService.js`: scheduled channel messages on the shared scheduler. Three modes:
//! `TIME_OF_DAY` (MEE6 "Single"), `INTERVAL` (MEE6 "Recurring") and `ONE_SHOT`. Constants, the pure helpers
//! (validation, next-run maths, the activity gate, payloads, descriptions), the database helpers and the
//! `reminder.fire` job handler. State lives only in PostgreSQL (`Reminder` and `ScheduledJob` rows): no Redis keys.

use bot_core::custom_embed::{RenderOptions, normalize_template, render_template};
use bot_core::interaction_helpers::{cached_channel, cached_guild, channel_usable_verdict, pingable_role_ids};
use bot_core::prelude::*;
use bot_db::{GuildSettings, InsertBuilder, REMINDER_INFO, Reminder, ReminderMode, SqlValue, UpdateBuilder};
use bot_util::jsre::LazyRe;
use bot_util::lazy_re;
use bot_util::text::clamp_text;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use serenity::all::{ChannelId, Guild, GuildChannel, GuildId, MessageId, Permissions};
use serenity::builder::CreateAllowedMentions;
use sha1::Digest;

use crate::net::{self, MessageLite, RestError};

// ── Constants ──────────────────────────────────────────────────────────────────

/// `DEFAULT_CONTENT`.
pub const DEFAULT_CONTENT: &str = "Hey, I'm a reminder!";
/// `LATE_TOLERANCE_MS`.
pub const LATE_TOLERANCE_MS: i64 = 15 * 60_000;
/// `ONE_SHOT_LATE_MS`.
pub const ONE_SHOT_LATE_MS: i64 = 60 * 60_000;
/// `REMINDER_CHANNEL_TYPES`: GuildText, GuildAnnouncement.
pub const REMINDER_CHANNEL_TYPES: [u8; 2] = [0, 5];
/// `MODULE_OFF_LINE`.
pub const MODULE_OFF_LINE: &str = "⚠️ The **Reminders** module is off, so nothing is sent until an admin runs `/module enable reminders`.";
const USER_TOKEN_WARNING: &str = "⚠️ Reminders have no member, so user variables like `{user}` are posted as written. Server and channel variables work.";
const MINUTE_MS: i64 = 60_000;
const DAY_MS: i64 = 86_400_000;
const SEND_TIMEOUT_MS: u64 = 20_000;

/// `DEFAULT_TEMPLATE`.
pub fn default_template() -> Value {
    json!({ "content": DEFAULT_CONTENT, "embed": null, "buttons": [] })
}

/// `MODE_FROM_CHOICE[choice]`.
pub fn mode_from_choice(choice: &str) -> Option<ReminderMode> {
    match choice {
        "time" => Some(ReminderMode::TimeOfDay),
        "interval" => Some(ReminderMode::Interval),
        "once" => Some(ReminderMode::OneShot),
        _ => None,
    }
}

/// `MODE_LABELS[mode]`.
pub fn mode_label(m: ReminderMode) -> &'static str {
    match m {
        ReminderMode::TimeOfDay => "Time of day",
        ReminderMode::Interval => "Interval",
        ReminderMode::OneShot => "Once",
    }
}

/// `ReminderUserError`: shown to the admin as it is (cap reached, name taken).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReminderUserError {
    pub title: String,
    pub message: String,
}

impl std::fmt::Display for ReminderUserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ReminderUserError {}

// ── Small pure helpers ─────────────────────────────────────────────────────────

/// `resolveTimeZone(tz)`: the zone, or `UTC` when it is not a valid IANA name.
pub fn resolve_time_zone(tz: Option<&str>) -> String {
    match tz {
        Some(t) if bot_util::time::is_valid_time_zone(t) => t.to_string(),
        _ => "UTC".into(),
    }
}

/// `jobKey(id)`: the scheduler dedupe key of a reminder.
pub fn job_key(id: &str) -> String {
    format!("reminder:{id}")
}

/// `moduleOffLine(settings)`: the line shown while the module is off (none when on or unknown).
pub fn module_off_line(settings: Option<&GuildSettings>) -> Option<&'static str> {
    match settings {
        Some(s) if !is_module_enabled(Some(s), Some(ModuleKey::Reminders)) => Some(MODULE_OFF_LINE),
        _ => None,
    }
}

/// `pausedLine(name)`.
pub fn paused_line(name: &str) -> String {
    format!("⏸️ Paused — run `/reminder resume name:{name}` to schedule it.")
}

// ── Schedules ──────────────────────────────────────────────────────────────────

/// The schedule columns of a reminder (and of a candidate being validated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    pub mode: ReminderMode,
    pub time_of_day: Option<String>,
    pub weekdays: Vec<i32>,
    pub interval_minutes: Option<i32>,
    pub window_start: Option<String>,
    pub window_end: Option<String>,
    pub run_at: Option<DateTime<Utc>>,
    pub require_activity: bool,
    /// The INTERVAL grid anchor (`createdAt`).
    pub created_at: Option<DateTime<Utc>>,
}

impl From<&Reminder> for Schedule {
    fn from(r: &Reminder) -> Self {
        Self {
            mode: r.mode,
            time_of_day: r.time_of_day.clone(),
            weekdays: r.weekdays.clone(),
            interval_minutes: r.interval_minutes,
            window_start: r.window_start.clone(),
            window_end: r.window_end.clone(),
            run_at: r.run_at,
            require_activity: r.require_activity,
            created_at: Some(r.created_at),
        }
    }
}

/// The raw `/reminder` schedule options (`None` = not provided).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScheduleInput {
    pub mode: Option<String>,
    pub time: Option<String>,
    pub weekdays: Option<String>,
    pub interval: Option<String>,
    pub window_start: Option<String>,
    pub window_end: Option<String>,
    pub at: Option<String>,
    pub require_activity: Option<bool>,
}

impl ScheduleInput {
    pub fn any(&self) -> bool {
        self.mode.is_some()
            || self.time.is_some()
            || self.weekdays.is_some()
            || self.interval.is_some()
            || self.window_start.is_some()
            || self.window_end.is_some()
            || self.at.is_some()
            || self.require_activity.is_some()
    }
}

const NONE_WORDS: [&str; 4] = ["none", "off", "clear", "-"];

/// `parseWindowBound(v)`: `Some(None)` for none, `Some(Some("HH:MM"))`, or `None` when unreadable.
fn parse_window_bound(v: &str) -> Option<Option<String>> {
    if NONE_WORDS.contains(&bot_util::js::lower(bot_util::js::trim(v)).as_str()) {
        return Some(None);
    }
    bot_util::time::parse_time_of_day(v).map(|t| Some(bot_util::time::format_time_of_day(t)))
}

static PLAIN_MINUTES: LazyRe = lazy_re!(r"^\d{1,5}$", "");

/// `validateReminderInput(input, { tz, now, base })`: the schedule columns, or the first error. `base` is the row
/// being edited (options not provided carry over).
pub fn validate_reminder_input(inp: &ScheduleInput, tz: &str, now: DateTime<Utc>, base: Option<&Reminder>) -> Result<Schedule, String> {
    // 1. Mode.
    let Some(mode) = inp.mode.as_deref().and_then(mode_from_choice).or(base.map(|b| b.mode)) else {
        return Err("Pick a `mode`.".into());
    };
    let one = mode == ReminderMode::OneShot;

    // 2. Options that do not apply to the mode (named as the slash options).
    let mut bad: Vec<&str> = Vec::new();
    let checks: [(bool, bool, &str); 6] = [
        (inp.time.is_some(), mode == ReminderMode::TimeOfDay, "time"),
        (inp.interval.is_some(), mode == ReminderMode::Interval, "interval"),
        (inp.window_start.is_some(), mode == ReminderMode::Interval, "window_start"),
        (inp.window_end.is_some(), mode == ReminderMode::Interval, "window_end"),
        (inp.at.is_some(), one, "at"),
        (inp.weekdays.is_some(), !one, "weekdays"),
    ];
    for (given, applies, name) in checks {
        if given && !applies {
            bad.push(name);
        }
    }
    if inp.require_activity == Some(true) && one {
        bad.push("require_activity");
    }
    if !bad.is_empty() {
        let list: Vec<String> = bad.iter().map(|n| format!("`{n}`")).collect();
        return Err(format!("These options don't apply to **{}** mode: {}.", mode_label(mode), list.join(", ")));
    }

    // 3. Carry-over.
    let mut weekdays: Vec<i32> = Vec::new();
    let mut time_of_day = None;
    let mut interval_minutes: Option<i64> = None;
    let mut window_start = None;
    let mut window_end = None;
    let mut run_at = None;
    let mut require_activity = false;
    if let Some(b) = base {
        if b.mode != ReminderMode::OneShot && !one {
            weekdays = b.weekdays.clone();
        }
        if b.mode == ReminderMode::TimeOfDay && mode == ReminderMode::TimeOfDay {
            time_of_day = b.time_of_day.clone();
        }
        if b.mode == ReminderMode::Interval && mode == ReminderMode::Interval {
            interval_minutes = b.interval_minutes.map(i64::from);
            window_start = b.window_start.clone();
            window_end = b.window_end.clone();
        }
        if b.mode == ReminderMode::OneShot && one {
            run_at = b.run_at;
        }
        if !one {
            require_activity = b.require_activity;
        }
    }

    // 4. Parse the provided values, in this order.
    if let Some(t) = &inp.time {
        let Some(t) = bot_util::time::parse_time_of_day(t) else {
            return Err("I couldn't read that time of day. Try `09:00`, `9:30pm` or `21:30`.".into());
        };
        time_of_day = Some(bot_util::time::format_time_of_day(t));
    }
    if let Some(w) = &inp.weekdays {
        match bot_util::time::parse_weekdays(w) {
            Some(days) if !days.is_empty() => weekdays = if days.len() == 7 { Vec::new() } else { days.iter().map(|d| *d as i32).collect() },
            _ => return Err("I couldn't read those days. Use names like `mon,wed,fri`, or `weekdays`, `weekends`, `all`.".into()),
        }
    }
    if let Some(i) = &inp.interval {
        let raw = bot_util::js::trim(i);
        if PLAIN_MINUTES.test(raw) {
            interval_minutes = raw.parse::<i64>().ok();
        } else {
            let Some(ms) = bot_util::parse_duration::parse_duration(raw) else {
                return Err("I couldn't read that interval. Try `30m`, `2h` or `1d`.".into());
            };
            interval_minutes = Some(bot_util::text::js_round(ms as f64 / MINUTE_MS as f64) as i64);
        }
    }
    for (raw, slot) in [(&inp.window_start, &mut window_start), (&inp.window_end, &mut window_end)] {
        let Some(v) = raw else { continue };
        match parse_window_bound(v) {
            Some(b) => *slot = b,
            None => return Err("Window times use HH:MM, e.g. `09:00`, or `none`.".into()),
        }
    }
    if let Some(a) = &inp.at {
        match bot_util::time::parse_date_input(a, tz, &now) {
            Some(d) => run_at = Some(d),
            None => return Err("I couldn't read that time. Try `18:00`, `2026-12-24 09:00` or a delay like `2h30m`.".into()),
        }
    }
    if let Some(r) = inp.require_activity
        && !one
    {
        require_activity = r;
    }

    // 5. Required fields by mode.
    if mode == ReminderMode::TimeOfDay && time_of_day.is_none() {
        return Err("Time mode needs `time`, e.g. `09:00`.".into());
    }
    if mode == ReminderMode::Interval && interval_minutes.is_none() {
        return Err("Interval mode needs `interval`, e.g. `2h`.".into());
    }
    if one && run_at.is_none() {
        return Err("Once mode needs `at`, e.g. `18:00` or `2h`.".into());
    }

    // 6. INTERVAL limits.
    if mode == ReminderMode::Interval {
        let minutes = interval_minutes.unwrap_or(0);
        if minutes < 5 {
            return Err("The shortest interval is 5 minutes.".into());
        }
        if minutes > 10_080 {
            return Err("The longest interval is 7 days.".into());
        }
        if (window_start.is_some() || window_end.is_some()) && minutes >= 1440 {
            return Err("With a daily window the interval must be under 24 hours. For daily or weekly reminders use the Time of day mode.".into());
        }
        if window_start.is_some() && window_start == window_end {
            return Err("The window start and end can't be the same time.".into());
        }
        if !weekdays.is_empty() && minutes > 1440 {
            return Err("Weekdays can only be used with intervals of 1 day or less.".into());
        }
    }

    // 7. ONE_SHOT limits (also for a carried-over runAt).
    if one && let Some(at) = run_at {
        let at = at.timestamp_millis();
        let n = now.timestamp_millis();
        if at <= n + MINUTE_MS {
            return Err("That time has passed or is less than a minute away. Pick a later `at`.".into());
        }
        if at > n + 366 * DAY_MS {
            return Err("Pick a time within one year.".into());
        }
    }

    // 8. Never-fires check.
    let fields = Schedule {
        mode,
        time_of_day,
        weekdays,
        interval_minutes: interval_minutes.map(|m| m.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32),
        window_start,
        window_end,
        run_at,
        require_activity,
        created_at: Some(base.map(|b| b.created_at).unwrap_or(now)),
    };
    if compute_next_run(&fields, tz, now).is_none() {
        return Err("This schedule never fires. Check the weekdays and the time window.".into());
    }
    Ok(fields)
}

// ── Next-run maths ─────────────────────────────────────────────────────────────

/// `computeNextRun(r, tz, after)`: the next occurrence strictly after `after`. INTERVAL grids are anchored on
/// `createdAt`; a window (either bound) ignores the anchor.
pub fn compute_next_run(r: &Schedule, tz: &str, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    match r.mode {
        ReminderMode::TimeOfDay => {
            let t = bot_util::time::parse_time_of_day(r.time_of_day.as_deref().unwrap_or(""))?;
            let wd: Vec<u32> = r.weekdays.iter().filter(|d| **d >= 0).map(|d| *d as u32).collect();
            bot_util::time::next_time_of_day(tz, t.hour, t.minute, &wd, &after)
        }
        ReminderMode::Interval => {
            let im = r.interval_minutes.filter(|m| *m >= 5)?;
            let o = bot_util::time::IntervalOptions {
                tz,
                interval_minutes: f64::from(im),
                weekdays: r.weekdays.iter().map(|d| f64::from(*d)).collect(),
                window_start: r.window_start.as_deref(),
                window_end: r.window_end.as_deref(),
                after_ms: after.timestamp_millis() as f64,
                anchor_ms: r.created_at.map(|c| c.timestamp_millis() as f64),
            };
            bot_util::time::next_interval_run_ms(&o).and_then(|ms| DateTime::from_timestamp_millis(ms as i64))
        }
        ReminderMode::OneShot => r.run_at.filter(|a| a.timestamp_millis() > after.timestamp_millis()),
    }
}

/// `jobRunAt(r, tz, now)`: when the job should run; a one-shot whose time passed under an hour ago runs at once.
pub fn job_run_at(r: &Schedule, tz: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if r.mode == ReminderMode::OneShot {
        let at = r.run_at?;
        return (now.timestamp_millis() - at.timestamp_millis() <= ONE_SHOT_LATE_MS).then_some(at);
    }
    compute_next_run(r, tz, now)
}

/// `lateToleranceMs(r)`: how late an occurrence may still be sent.
pub fn late_tolerance_ms(r: &Schedule) -> i64 {
    match r.mode {
        ReminderMode::OneShot => ONE_SHOT_LATE_MS,
        ReminderMode::Interval => LATE_TOLERANCE_MS.min(i64::from(r.interval_minutes.unwrap_or(0)) * MINUTE_MS / 2),
        ReminderMode::TimeOfDay => LATE_TOLERANCE_MS,
    }
}

/// `dueSlot(r, tz, now)`: the occurrence of the current configuration that is due now (within the tolerance).
pub fn due_slot(r: &Schedule, tz: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let n = now.timestamp_millis();
    if r.mode == ReminderMode::OneShot {
        let at = r.run_at?;
        let late = n - at.timestamp_millis();
        return (0..=ONE_SHOT_LATE_MS).contains(&late).then_some(at);
    }
    let from = DateTime::from_timestamp_millis(n - late_tolerance_ms(r))?;
    compute_next_run(r, tz, from).filter(|s| s.timestamp_millis() <= n)
}

// ── Templates and payloads ─────────────────────────────────────────────────────

fn non_blank(v: &Value) -> bool {
    v.as_str().is_some_and(|s| !bot_util::js::trim(s).is_empty())
}

/// `isEmptyTemplate(t)`: no text, embed text, fields or images (a colour alone is not content).
pub fn is_empty_template(t: &Value) -> bool {
    if non_blank(&t["content"]) {
        return false;
    }
    let e = &t["embed"];
    if e.is_null() || !e.is_object() && !e.as_bool().unwrap_or(true) {
        return true;
    }
    if !e.is_object() {
        return true;
    }
    !(non_blank(&e["title"])
        || non_blank(&e["description"])
        || e["fields"].as_array().is_some_and(|f| !f.is_empty())
        || non_blank(&e["image"])
        || non_blank(&e["thumbnail"])
        || non_blank(&e["author"]["name"])
        || non_blank(&e["footer"]["text"]))
}

static USER_TOKEN: LazyRe = lazy_re!(r"\{user(\.[a-z_]+)?\}", "i");

/// `hasUserTokens(tpl)`: the template mentions a user variable, which a reminder cannot fill in.
pub fn has_user_tokens(tpl: &Value) -> bool {
    let t = normalize_template(tpl);
    [&t["content"], &t["embed"]["title"], &t["embed"]["description"], &t["embed"]["footer"]["text"]]
        .iter()
        .any(|s| s.as_str().is_some_and(|s| USER_TOKEN.test(s)))
}

/// A reminder message.
#[derive(Debug, Clone, PartialEq)]
pub struct SendPayload {
    pub content: Option<String>,
    pub embeds: Vec<Value>,
    pub components: Vec<Value>,
    /// `{ parse: ['users'], roles }` when pinging, `{ parse: [] }` for a preview.
    pub allowed_mentions: Value,
}

impl SendPayload {
    /// The create-message JSON (`nonce` + `enforce_nonce` when given).
    pub fn to_json(&self, nonce: Option<&str>) -> Value {
        let mut v = json!({ "embeds": self.embeds, "components": self.components, "allowed_mentions": self.allowed_mentions });
        if let Some(c) = &self.content {
            v["content"] = json!(c);
        }
        if let Some(n) = nonce {
            v["nonce"] = json!(n);
            v["enforce_nonce"] = json!(true);
        }
        v
    }

    /// The serenity mention rules of the payload.
    pub fn mentions(&self) -> CreateAllowedMentions {
        let mut m = CreateAllowedMentions::new();
        if self.allowed_mentions["parse"].as_array().is_some_and(|p| p.iter().any(|x| x == "users")) {
            m = m.all_users(true);
        }
        let roles: Vec<u64> = self.allowed_mentions["roles"].as_array().into_iter().flatten().filter_map(|r| r.as_str()?.parse().ok()).collect();
        m.roles(roles)
    }
}

/// `buildSendPayload(r, { guild, channel, settings, ping })`: placeholders see the guild and channel only; `@everyone`
/// never pings; the ping role only through `allowedMentions.roles` and only while the bot may ping it.
pub fn build_send_payload(app: &App, r: &Reminder, guild: Option<&Guild>, channel: Option<&GuildChannel>, settings: Option<&GuildSettings>, ping: bool) -> SendPayload {
    let tpl = normalize_template(&r.template);
    let ping_role = r.ping_role_id.clone().filter(|p| !p.is_empty());
    let roles: Vec<String> = match (ping, &ping_role, guild, channel) {
        (true, Some(role), Some(g), Some(ch)) => {
            let me = crate::feed_service::bot_member(app, g);
            pingable_role_ids(g, ch, me.as_ref().map(|(id, roles)| (*id, roles.as_slice())), std::slice::from_ref(role)).allowed
        }
        _ => Vec::new(),
    };
    let allowed = if ping { json!({ "parse": ["users"], "roles": roles }) } else { json!({ "parse": [] }) };
    let ctx = bot_util::placeholder_parser::RenderContext {
        guild: guild.map(bot_core::adapters::placeholder_guild),
        channel: channel.map(bot_core::adapters::placeholder_channel),
        ..Default::default()
    };
    let color = settings.and_then(|s| s.embed_color).map(|c| c as u32).unwrap_or(bot_config::colors::INFO);
    let opts = || RenderOptions { default_color: Some(color), allowed_mentions: None, extra_components: Vec::new() };
    let rendered = render_template(&tpl, &ctx, opts()).or_else(|| render_template(&default_template(), &ctx, opts()));
    let (mut content, embeds, components) = match rendered {
        Some(r) => (r.content, r.embeds, r.link_rows),
        None => (None, Vec::new(), Vec::new()),
    };
    if let Some(role) = ping_role {
        content = Some(clamp_text(bot_util::js::trim_end(&format!("<@&{role}> {}", content.unwrap_or_default())), 2000));
    }
    SendPayload { content, embeds, components, allowed_mentions: allowed }
}

// ── Descriptions ───────────────────────────────────────────────────────────────

fn weekday_label(weekdays: &[i32]) -> String {
    let mut wd: Vec<i32> = weekdays.to_vec();
    wd.sort_unstable();
    wd.dedup();
    if wd.is_empty() {
        return "Every day".into();
    }
    if wd == [1, 2, 3, 4, 5] {
        return "Weekdays".into();
    }
    if wd == [0, 6] {
        return "Weekends".into();
    }
    wd.iter()
        .map(|d| {
            let n = usize::try_from(*d).ok().and_then(|i| bot_util::time::WEEKDAY_NAMES.get(i)).copied().unwrap_or("");
            let mut c = n.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `describeSchedule(r)`: 'Weekdays at 09:00', 'Every 2 hours between 09:00 and 17:00 on weekdays', 'Once at <t:…:f>'.
pub fn describe_schedule(r: &Schedule) -> String {
    let wd = weekday_label(&r.weekdays);
    match r.mode {
        ReminderMode::TimeOfDay => format!("{wd} at {}", r.time_of_day.as_deref().unwrap_or("??:??")),
        ReminderMode::Interval => {
            let mut s = format!("Every {}", bot_util::parse_duration::format_duration_long((i64::from(r.interval_minutes.unwrap_or(0)) * MINUTE_MS) as f64));
            if r.window_start.is_some() || r.window_end.is_some() {
                s.push_str(&format!(" between {} and {}", r.window_start.as_deref().unwrap_or("00:00"), r.window_end.as_deref().unwrap_or("24:00")));
            }
            if !r.weekdays.is_empty() {
                s.push_str(&format!(" on {}", if wd == "Weekdays" || wd == "Weekends" { bot_util::js::lower(&wd) } else { wd.clone() }));
            }
            s
        }
        ReminderMode::OneShot => match r.run_at {
            Some(a) => format!("Once at {}", bot_util::time::discord_timestamp(&a, "f")),
            None => "Once".into(),
        },
    }
}

/// `summaryLines(r, settings, next, warnings)`: the confirmation lines of create / edit / resume / the form.
pub fn summary_lines(r: &Reminder, settings: Option<&GuildSettings>, next: Option<DateTime<Utc>>, warnings: &[String]) -> Vec<String> {
    let tz = resolve_time_zone(settings.map(|s| s.timezone.as_str()));
    let mut lines = vec![
        format!("**{}** → <#{}>", r.name, r.channel_id),
        describe_schedule(&Schedule::from(r)),
        match next {
            Some(n) => format!("Next: {} ({})", bot_util::time::discord_timestamp(&n, "F"), bot_util::time::discord_timestamp(&n, "R")),
            None => "Next: —".into(),
        },
    ];
    if !r.enabled {
        lines.push(paused_line(&r.name));
    }
    lines.push(format!("Timezone: {tz} (change with `/settings timezone`)"));
    if r.require_activity {
        lines.push("💬 Skipped when nobody has chatted since the last reminder.".into());
    }
    if let Some(p) = r.ping_role_id.as_deref().filter(|p| !p.is_empty()) {
        lines.push(format!("🔔 Pings <@&{p}>"));
    }
    lines.push(format!("Use `/reminder test name:{}` to preview it.", r.name));
    lines.extend(warnings.iter().cloned());
    if let Some(off) = module_off_line(settings) {
        lines.push(off.into());
    }
    lines
}

fn parse_snowflake(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|v| *v != 0)
}

/// `reminderWarnings(r, guild, tpl)`: synchronous and never fails; channel warnings are skipped when the channel is
/// not cached.
pub fn reminder_warnings(app: &App, r: &Reminder, tpl: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if r.require_activity
        && let (Some(g), Some(c)) = (parse_snowflake(&r.guild_id), parse_snowflake(&r.channel_id))
        && let Some(guild) = cached_guild(app, GuildId::new(g))
        && let Some(channel) = bot_core::permissions::find_channel(&guild, ChannelId::new(c))
        && let Some((id, roles)) = crate::feed_service::bot_member(app, &guild)
        && let Some(perms) = bot_core::permissions::member_permissions_in(&guild, channel, id, &roles, true)
        && !perms.contains(Permissions::READ_MESSAGE_HISTORY)
        && !perms.contains(Permissions::ADMINISTRATOR)
    {
        out.push(format!("⚠️ I can't read message history in <#{}>, so the activity check only sees the last cached message.", r.channel_id));
    }
    if has_user_tokens(tpl) {
        out.push(USER_TOKEN_WARNING.into());
    }
    out
}

// ── Activity gate ──────────────────────────────────────────────────────────────

/// The state [`activity_allows_send`] decides on.
#[derive(Debug, Clone, Default)]
pub struct ActivityState {
    pub require_activity: bool,
    pub last_message_id: Option<String>,
    pub channel_last_message_id: Option<String>,
    pub recent: Option<Vec<MessageLite>>,
    pub cached_last: Option<MessageLite>,
}

/// `activityAllowsSend(state)`: send only when a non-bot, non-system message came after our last reminder.
/// Fail-open when the history is unknown.
pub fn activity_allows_send(s: &ActivityState) -> bool {
    let Some(last) = s.last_message_id.as_deref().filter(|l| !l.is_empty()) else { return true };
    if !s.require_activity {
        return true;
    }
    if s.channel_last_message_id.as_deref().is_some_and(|c| !c.is_empty() && c == last) {
        return false;
    }
    let human = |m: &Option<MessageLite>| m.is_some_and(|m| !m.bot && !m.system);
    if let Some(recent) = &s.recent {
        return recent.iter().any(|m| !m.bot && !m.system) || human(&s.cached_last);
    }
    match &s.cached_last {
        Some(_) => human(&s.cached_last),
        None => true,
    }
}

// ── Database helpers ───────────────────────────────────────────────────────────

fn select_reminders(where_sql: &str) -> String {
    format!("SELECT {} FROM {} WHERE {where_sql}", REMINDER_INFO.select_list(), REMINDER_INFO.quoted_table())
}

fn text(v: &str) -> SqlValue {
    SqlValue::Text(Some(v.to_string()))
}

/// `findReminderByName(guildId, raw)`: by the name as typed (normalised with `sanitizeName`).
pub async fn find_reminder_by_name(pool: &bot_db::PgPool, guild_id: &str, raw: &str) -> anyhow::Result<Option<Reminder>> {
    let name = bot_util::text::sanitize_name(raw, 32, "");
    if name.is_empty() {
        return Ok(None);
    }
    let q = bot_db::BuiltQuery { sql: select_reminders(r#""guildId" = $1 AND "name" = $2 LIMIT 1"#), args: vec![text(guild_id), text(&name)] };
    Ok(q.fetch_optional::<Reminder, _>(pool).await?)
}

/// A reminder of a guild by id (`findFirst({ where: { id, guildId } })`).
pub async fn get_reminder(pool: &bot_db::PgPool, guild_id: &str, id: &str) -> anyhow::Result<Option<Reminder>> {
    let q = bot_db::BuiltQuery { sql: select_reminders(r#""id" = $1 AND "guildId" = $2 LIMIT 1"#), args: vec![text(id), text(guild_id)] };
    Ok(q.fetch_optional::<Reminder, _>(pool).await?)
}

/// The reminders of a guild by name (`findMany({ where: { guildId }, orderBy: { name: 'asc' }, take })`).
pub async fn list_reminders(pool: &bot_db::PgPool, guild_id: &str, take: i64) -> anyhow::Result<Vec<Reminder>> {
    let q = bot_db::BuiltQuery { sql: select_reminders(&format!(r#""guildId" = $1 ORDER BY "name" ASC LIMIT {take}"#)), args: vec![text(guild_id)] };
    Ok(q.fetch_all::<Reminder, _>(pool).await?)
}

/// Autocomplete rows: `(name, enabled, mode)` whose name contains `q` (Prisma's unescaped `ILIKE '%q%'`), only the
/// enabled ones when `enabled_only`, by name, at most 25.
pub async fn search_reminders(pool: &bot_db::PgPool, guild_id: &str, q: &str, enabled_only: bool) -> anyhow::Result<Vec<Reminder>> {
    let mut where_sql = String::from(r#""guildId" = $1"#);
    let mut args = vec![text(guild_id)];
    if !q.is_empty() {
        args.push(text(&format!("%{q}%")));
        where_sql.push_str(&format!(r#" AND "name" ILIKE ${}"#, args.len()));
    }
    if enabled_only {
        args.push(SqlValue::Bool(Some(true)));
        where_sql.push_str(&format!(r#" AND "enabled" = ${}"#, args.len()));
    }
    where_sql.push_str(r#" ORDER BY "name" ASC LIMIT 25"#);
    Ok(bot_db::BuiltQuery { sql: select_reminders(&where_sql), args }.fetch_all::<Reminder, _>(pool).await?)
}

/// Per-app state: the per-guild create lock and the warn throttle.
pub struct RemindersState {
    pub create_lock: bot_core::sync::KeyedMutex,
    /// `createThrottle(6 h)` on the app clock, built on first use.
    warn_throttle: std::sync::OnceLock<bot_util::async_util::Throttle>,
}

impl Default for RemindersState {
    fn default() -> Self {
        Self { create_lock: bot_core::sync::KeyedMutex::new(), warn_throttle: std::sync::OnceLock::new() }
    }
}

/// The arguments of [`create_reminder_row`].
#[derive(Debug, Clone)]
pub struct NewReminder {
    pub guild_id: String,
    pub name: String,
    pub channel_id: String,
    pub template: Value,
    pub fields: Schedule,
    pub ping_role_id: Option<String>,
    pub created_by: String,
    pub now: DateTime<Utc>,
}

fn ts(d: Option<DateTime<Utc>>) -> SqlValue {
    match d {
        Some(d) => SqlValue::timestamp(d),
        None => SqlValue::Timestamp(None),
    }
}

fn schedule_values(f: &Schedule) -> Vec<(&'static str, Value)> {
    vec![
        ("mode", json!(f.mode.as_str())),
        ("timeOfDay", json!(f.time_of_day)),
        ("weekdays", json!(f.weekdays)),
        ("intervalMinutes", json!(f.interval_minutes)),
        ("windowStart", json!(f.window_start)),
        ("windowEnd", json!(f.window_end)),
        ("requireActivity", json!(f.require_activity)),
    ]
}

/// `createReminderRow(...)`: creates a reminder under the per-guild lock (cap and unique name); `createdAt` is set
/// explicitly because it anchors the INTERVAL grid. A [`ReminderUserError`] for the cap or a taken name.
pub async fn create_reminder_row(app: &App, a: NewReminder) -> anyhow::Result<Reminder> {
    let app2 = app.clone();
    let guild = a.guild_id.clone();
    app.ext::<RemindersState>()
        .create_lock
        .run(&guild, move || async move {
            let app = app2;
            app.settings().ensure_guild_settings(&a.guild_id).await?;
            let row: (i64,) = bot_db::sqlx::query_as(r#"SELECT COUNT(*)::bigint FROM "Reminder" WHERE "guildId" = $1"#).bind(&a.guild_id).fetch_one(app.db()).await?;
            let limit = bot_config::limits::REMINDERS as i64;
            if row.0 >= limit {
                return Err(ReminderUserError {
                    title: "Limit Reached".into(),
                    message: format!("This server already has the maximum of {limit} reminders. Delete one with `/reminder delete` first."),
                }
                .into());
            }
            let mut values = vec![
                ("guildId", json!(a.guild_id)),
                ("name", json!(a.name)),
                ("channelId", json!(a.channel_id)),
                ("template", a.template.clone()),
                ("pingRoleId", json!(a.ping_role_id)),
                ("createdBy", json!(a.created_by)),
                ("enabled", json!(true)),
            ];
            values.extend(schedule_values(&a.fields));
            let q = InsertBuilder::new(&REMINDER_INFO).values(values)?.typed("runAt", ts(a.fields.run_at))?.typed("createdAt", SqlValue::timestamp(a.now))?.build()?;
            match q.fetch_one::<Reminder, _>(app.db()).await {
                Ok(r) => Ok(r),
                Err(e) if e.is_unique_violation() => Err(ReminderUserError {
                    title: "Name Taken".into(),
                    message: format!("A reminder named **{}** already exists. Use `/reminder edit` to change it or `/reminder delete` to remove it.", a.name),
                }
                .into()),
                Err(e) => Err(e.into()),
            }
        })
        .await
}

/// The columns `/reminder edit` and the form change (`None` = unchanged).
#[derive(Debug, Clone, Default)]
pub struct ReminderUpdate {
    pub fields: Option<Schedule>,
    pub channel_id: Option<String>,
    /// `Some(None)` clears the last message id.
    pub last_message_id: Option<Option<String>>,
    pub ping_role_id: Option<Option<String>>,
    pub template: Option<Value>,
    pub enabled: Option<bool>,
}

/// `prisma.reminder.update({ where: { id }, data })`: fails when the row is gone.
pub async fn update_reminder(app: &App, id: &str, u: &ReminderUpdate) -> anyhow::Result<Reminder> {
    let mut b = UpdateBuilder::new(&REMINDER_INFO);
    if let Some(f) = &u.fields {
        b = b.set_all(schedule_values(f))?.set_typed("runAt", ts(f.run_at))?;
    }
    if let Some(c) = &u.channel_id {
        b = b.set("channelId", json!(c))?;
    }
    if let Some(m) = &u.last_message_id {
        b = b.set("lastMessageId", json!(m))?;
    }
    if let Some(p) = &u.ping_role_id {
        b = b.set("pingRoleId", json!(p))?;
    }
    if let Some(t) = &u.template {
        b = b.set("template", t.clone())?;
    }
    if let Some(e) = u.enabled {
        b = b.set("enabled", json!(e))?;
    }
    let row = b.filter("id", json!(id))?.build()?.fetch_optional::<Reminder, _>(app.db()).await?;
    row.ok_or_else(|| anyhow::anyhow!("Record to update not found."))
}

/// `syncReminderJob(r, settings, { now })`: makes the job match the row (replace). The time it was scheduled for,
/// or `None` (nothing scheduled). Fails on a database error.
pub async fn sync_reminder_job(app: &App, r: &Reminder, settings: Option<&GuildSettings>, now: DateTime<Utc>) -> anyhow::Result<Option<DateTime<Utc>>> {
    let key = job_key(&r.id);
    let cancel = || CancelJobs { dedupe_key: Some(key.clone()), ..Default::default() };
    if !r.enabled {
        app.scheduler().cancel_jobs(cancel()).await;
        return Ok(None);
    }
    let Some(run_at) = job_run_at(&Schedule::from(r), &resolve_time_zone(settings.map(|s| s.timezone.as_str())), now) else {
        app.scheduler().cancel_jobs(cancel()).await;
        return Ok(None);
    };
    app.scheduler()
        .schedule_job(
            ScheduleJob::new(&r.guild_id, JobType::ReminderFire, run_at)
                .payload(json!({ "reminderId": r.id }))
                .dedupe_key(key)
                .max_attempts(10)
                .on_conflict(OnConflict::Replace),
        )
        .await?;
    Ok(Some(run_at))
}

/// `pauseReminder(r)`: pauses it and cancels its job.
pub async fn pause_reminder(app: &App, r: &Reminder) -> anyhow::Result<()> {
    update_reminder(app, &r.id, &ReminderUpdate { enabled: Some(false), ..Default::default() }).await?;
    app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(job_key(&r.id)), ..Default::default() }).await;
    Ok(())
}

/// `deleteReminder(r)`: deletes it (guild-scoped) and cancels its job.
pub async fn delete_reminder(app: &App, r: &Reminder) -> anyhow::Result<()> {
    bot_db::delete_where(&REMINDER_INFO, [("id", json!(r.id)), ("guildId", json!(r.guild_id))])?.execute(app.db()).await?;
    app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(job_key(&r.id)), ..Default::default() }).await;
    Ok(())
}

/// `autoDisable(r, reason)`: pauses a reminder that can no longer work and logs why, once.
pub async fn auto_disable(app: &App, r: &Reminder, reason: &str) -> anyhow::Result<()> {
    let count = UpdateBuilder::new(&REMINDER_INFO)
        .set("enabled", json!(false))?
        .filter("id", json!(r.id))?
        .filter("enabled", json!(true))?
        .without_returning()
        .build()?
        .execute(app.db())
        .await?;
    if count == 1 {
        bot_util::log_warn!(format!("Reminder \"{}\" ({}) in guild {} was paused automatically: {reason}", r.name, r.id, r.guild_id));
    }
    Ok(())
}

/// `disableRemindersForChannel(guildId, channelId)` (`channelDelete`): pauses the channel's enabled reminders and
/// cancels their jobs; the rows stay. Returns how many.
pub async fn disable_reminders_for_channel(app: &App, guild_id: &str, channel_id: &str) -> anyhow::Result<usize> {
    let q = bot_db::BuiltQuery {
        sql: select_reminders(r#""guildId" = $1 AND "channelId" = $2 AND "enabled" = $3"#),
        args: vec![text(guild_id), text(channel_id), SqlValue::Bool(Some(true))],
    };
    let rows = q.fetch_all::<Reminder, _>(app.db()).await?;
    if rows.is_empty() {
        return Ok(0);
    }
    let ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
    bot_db::BuiltQuery {
        sql: r#"UPDATE "Reminder" SET "enabled" = $1, "updatedAt" = $2 WHERE "id" = ANY($3)"#.into(),
        args: vec![SqlValue::Bool(Some(false)), SqlValue::timestamp(bot_db::now_utc()), SqlValue::TextArray(ids.clone())],
    }
    .execute(app.db())
    .await?;
    for id in &ids {
        app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(job_key(id)), ..Default::default() }).await;
    }
    bot_util::log_info!(format!("Reminders: paused {} reminder(s) in guild {guild_id}; channel {channel_id} was deleted", rows.len()));
    Ok(rows.len())
}

/// `reconcileGuildReminders(guild)` (`guildReady`): an enabled reminder without a live job gets it back; one that
/// can never fire again is paused.
pub async fn reconcile_guild_reminders(app: &App, guild_id: &str) -> anyhow::Result<()> {
    let q = bot_db::BuiltQuery { sql: select_reminders(r#""guildId" = $1 AND "enabled" = $2"#), args: vec![text(guild_id), SqlValue::Bool(Some(true))] };
    let rows = q.fetch_all::<Reminder, _>(app.db()).await?;
    if rows.is_empty() {
        return Ok(());
    }
    let Some(settings) = app.settings().get_settings_safe(guild_id).await else { return Ok(()) };
    let keys: Vec<String> = rows.iter().map(|r| job_key(&r.id)).collect();
    let Some(jobs) = app.scheduler().get_jobs_by_keys(&keys).await else { return Ok(()) };
    for r in rows {
        if let Some(job) = jobs.get(&job_key(&r.id))
            && matches!(job.status, bot_db::JobStatus::Pending | bot_db::JobStatus::Running)
        {
            continue;
        }
        let res: anyhow::Result<()> = async {
            let at = sync_reminder_job(app, &r, Some(&settings), app.clock().now()).await?;
            if at.is_none() {
                let reason = if r.mode == ReminderMode::OneShot { "it was missed while the bot was offline" } else { "its schedule never fires" };
                auto_disable(app, &r, reason).await?;
            }
            Ok(())
        }
        .await;
        if let Err(e) = res {
            bot_util::log_error!(format!("reminders reconcile {}", r.id), bot_core::error::log_arg(&e));
        }
    }
    Ok(())
}

// ── The job handler ────────────────────────────────────────────────────────────

fn warn_once(app: &App, r: &Reminder, kind: &str, detail: &str) {
    let state = app.ext::<RemindersState>();
    let throttle = state.warn_throttle.get_or_init(|| bot_core::clock::clock_throttle(app.clock(), 6 * 60 * 60_000));
    if throttle.allow(&format!("{}:{kind}", r.id)) {
        bot_util::log_warn!(format!("Reminder \"{}\" ({}) in guild {} skipped: {detail}", r.name, r.id, r.guild_id));
    }
}

const GONE_CHANNEL_CODES: [u64; 2] = [10003, 50001];
const PERMANENT_SEND_CODES: [u64; 6] = [50001, 50013, 10003, 10013, 50007, 50035];
const SCHEDULER_RETRY_BASE_MS: i64 = 30_000;
const SCHEDULER_RETRY_MAX_MS: i64 = 60 * MINUTE_MS;
const TIMEOUT_RETRY_MAX_DELAY_MS: i64 = MINUTE_MS;

/// `isTransientSendError(err)`: a timeout, 408, 429, 5xx or network error may succeed later.
pub fn is_transient_send_error(e: &RestError) -> bool {
    if e.timeout {
        return true;
    }
    if e.code.is_some_and(|c| PERMANENT_SEND_CODES.contains(&c)) {
        return false;
    }
    match e.status {
        Some(s) if (400..500).contains(&s) => s == 408 || s == 429,
        _ => true,
    }
}

/// `schedulerRetryDelayMs(attempt)`: the scheduler's wait after failed attempt `attempt` (1-based).
pub fn scheduler_retry_delay_ms(attempt: i32) -> i64 {
    let exp = (attempt - 1).clamp(0, 30) as u32;
    (SCHEDULER_RETRY_BASE_MS.saturating_mul(2i64.saturating_pow(exp))).min(SCHEDULER_RETRY_MAX_MS)
}

/// `mayRetry({ attempt, maxAttempts, lateMs, toleranceMs })`: not the final attempt, and the retry still falls inside
/// the late tolerance of the occurrence.
pub fn may_retry(attempt: Option<i32>, max_attempts: Option<i32>, late_ms: i64, tolerance_ms: i64) -> bool {
    let (Some(a), Some(m)) = (attempt, max_attempts) else { return false };
    if a >= m {
        return false;
    }
    late_ms + scheduler_retry_delay_ms(a) < tolerance_ms
}

/// `occurrenceNonce(reminderId, slot)`: the Discord nonce of one occurrence (25 characters).
pub fn occurrence_nonce(reminder_id: &str, slot: DateTime<Utc>) -> String {
    let h = hex::encode(sha1::Sha1::digest(format!("{reminder_id}:{}", slot.timestamp_millis()).as_bytes()));
    h[..25].to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Sent,
    Skipped,
    Disabled,
    Retry,
}

async fn check_activity(app: &App, r: &Reminder, guild: Option<&Guild>, channel: &GuildChannel) -> bool {
    let Some(last) = r.last_message_id.clone().filter(|l| !l.is_empty()) else { return true };
    let channel_last = channel.last_message_id.map(|m| m.to_string());
    if channel_last.as_deref() == Some(last.as_str()) {
        return false;
    }
    let cached_last = channel.last_message_id.and_then(|mid| {
        let d = app.discord()?;
        let m = d.cache.message(channel.id, mid)?;
        Some(MessageLite { bot: m.author.bot || m.webhook_id.is_some(), system: !matches!(m.kind, serenity::all::MessageType::Regular | serenity::all::MessageType::InlineReply | serenity::all::MessageType::ChatInputCommand | serenity::all::MessageType::ContextMenuCommand) })
    });
    let mut recent = None;
    let can_read = guild
        .and_then(|g| {
            let (id, roles) = crate::feed_service::bot_member(app, g)?;
            bot_core::permissions::member_permissions_in(g, channel, id, &roles, true)
        })
        .is_some_and(|p| p.contains(Permissions::READ_MESSAGE_HISTORY) || p.contains(Permissions::ADMINISTRATOR));
    if can_read && let Some(after) = parse_snowflake(&last) {
        match net::rest(app).messages_after(app.clone(), channel.id, MessageId::new(after), 50).await {
            Ok(m) => recent = Some(m),
            Err(e) => warn_once(app, r, "history", &e.message),
        }
    }
    activity_allows_send(&ActivityState { require_activity: true, last_message_id: Some(last), channel_last_message_id: channel_last, recent, cached_last })
}

struct RunCtx {
    attempt: i32,
    max_attempts: i32,
}

async fn deliver(app: &App, r: &Reminder, slot: DateTime<Utc>, settings: &GuildSettings, run: &RunCtx, gate: bool, retryable: bool) -> anyhow::Result<Outcome> {
    let gid = GuildId::new(parse_snowflake(&r.guild_id).unwrap_or(1));
    let guild = cached_guild(app, gid);
    // 1. The channel. A cache miss is not a deletion: only a REST answer of 10003 / 50001 pauses the reminder.
    let cid = parse_snowflake(&r.channel_id).map(ChannelId::new);
    let mut channel = cid.and_then(|c| cached_channel(app, gid, c));
    if channel.is_none() {
        match cid {
            None => channel = None,
            Some(c) => match net::fetch_guild_channel(app, gid, c).await {
                Ok(ch) => channel = Some(ch),
                Err(e) => {
                    if e.code.is_some_and(|c| GONE_CHANNEL_CODES.contains(&c)) {
                        auto_disable(app, r, "its channel no longer exists or I lost access").await?;
                        return Ok(Outcome::Disabled);
                    }
                    if retryable && is_transient_send_error(&e) {
                        bot_util::log_warn!(format!("Reminder \"{}\" ({}) in guild {}: could not look up its channel ({}); retrying", r.name, r.id, r.guild_id, e.message));
                        return Ok(Outcome::Retry);
                    }
                    warn_once(app, r, "fetch", &e.message);
                    return Ok(Outcome::Skipped);
                }
            },
        }
    }
    let Some(channel) = channel.filter(|c| REMINDER_CHANNEL_TYPES.contains(&u8::from(c.kind))) else {
        auto_disable(app, r, "its channel can no longer receive reminders").await?;
        return Ok(Outcome::Disabled);
    };

    // 2. Payload.
    let payload = build_send_payload(app, r, guild.as_ref(), Some(&channel), Some(settings), true);

    // 3. Permissions. Without a cached bot member every permission looks missing, so never pause on that.
    let mut needs = vec!["ViewChannel", "SendMessages"];
    if !payload.embeds.is_empty() {
        needs.push("EmbedLinks");
    }
    let me = guild.as_ref().and_then(|g| crate::feed_service::bot_member(app, g));
    let perms = match (&guild, &me) {
        (Some(g), Some((id, roles))) => bot_core::permissions::member_permissions_in(g, &channel, *id, roles, true),
        _ => None,
    };
    let u = channel_usable_verdict(Some(&channel), perms, &needs);
    if !u.ok {
        if u.missing.iter().any(|m| m == "ViewChannel") && me.is_some() {
            auto_disable(app, r, &format!("I lost access to #{} (View Channel)", channel.name)).await?;
            return Ok(Outcome::Disabled);
        }
        let detail =
            if u.missing.is_empty() { u.reason.clone().unwrap_or_else(|| "channel not usable".into()) } else { format!("missing {} in #{}", u.missing.join(", "), channel.name) };
        warn_once(app, r, "perms", &detail);
        return Ok(Outcome::Skipped);
    }

    // 4. Activity gate.
    if gate && !check_activity(app, r, guild.as_ref(), &channel).await {
        return Ok(Outcome::Skipped);
    }

    // 5. Claim the occurrence (idempotent: a job can run twice after a lock expiry).
    let claimed_at = app.clock().now();
    let claim = bot_db::BuiltQuery {
        sql: r#"UPDATE "Reminder" SET "lastSentAt" = $1, "updatedAt" = $2 WHERE ("id" = $3 AND "enabled" = $4 AND ("lastSentAt" IS NULL OR "lastSentAt" < $5))"#.into(),
        args: vec![SqlValue::timestamp(claimed_at), SqlValue::timestamp(bot_db::now_utc()), text(&r.id), SqlValue::Bool(Some(true)), SqlValue::timestamp(slot)],
    };
    if claim.execute(app.db()).await? != 1 {
        return Ok(Outcome::Skipped);
    }

    // 6. Send, with the same nonce on every attempt at this occurrence.
    let body = payload.to_json(Some(&occurrence_nonce(&r.id, slot)));
    let sent = bot_util::async_util::with_timeout(net::rest(app).send_message(app.clone(), channel.id, body), SEND_TIMEOUT_MS, &format!("reminder send {}", r.id)).await;
    let err = match sent {
        Ok(msg_id) => {
            let r2 = UpdateBuilder::new(&REMINDER_INFO).set("lastMessageId", json!(msg_id))?.filter("id", json!(r.id))?.without_returning().build()?.execute(app.db()).await;
            if let Err(e) = r2 {
                bot_util::log_warn!(format!("Reminder {}: could not store lastMessageId: {e}", r.id));
            }
            return Ok(Outcome::Sent);
        }
        Err(bot_util::async_util::WithTimeoutError::Timeout(t)) => RestError::timed_out(&t.to_string()),
        Err(bot_util::async_util::WithTimeoutError::Inner(e)) => e,
    };
    let timed_out = err.timeout;
    let will_retry = retryable && is_transient_send_error(&err) && (!timed_out || scheduler_retry_delay_ms(run.attempt) <= TIMEOUT_RETRY_MAX_DELAY_MS);
    if !timed_out || will_retry {
        let revert = bot_db::BuiltQuery {
            sql: r#"UPDATE "Reminder" SET "lastSentAt" = $1, "updatedAt" = $2 WHERE ("id" = $3 AND "lastSentAt" = $4)"#.into(),
            args: vec![ts(r.last_sent_at), SqlValue::timestamp(bot_db::now_utc()), text(&r.id), SqlValue::timestamp(claimed_at)],
        };
        let _ = revert.execute(app.db()).await;
    }
    if err.code.is_some_and(|c| GONE_CHANNEL_CODES.contains(&c)) {
        auto_disable(app, r, "its channel no longer exists or I lost access").await?;
        return Ok(Outcome::Disabled);
    }
    if err.code == Some(50035) {
        auto_disable(app, r, &format!("Discord rejected its message: {}", clamp_text(&err.message, 200))).await?;
        return Ok(Outcome::Disabled);
    }
    if will_retry {
        bot_util::log_warn!(format!("Reminder \"{}\" ({}) in guild {}: the send failed ({}); retrying", r.name, r.id, r.guild_id, err.message));
        return Ok(Outcome::Retry);
    }
    warn_once(app, r, "send", &err.message);
    let _ = run.max_attempts;
    Ok(Outcome::Skipped)
}

enum RunResult {
    Done,
    Next(DateTime<Utc>),
    Retry,
}

async fn run_recurring(app: &App, r: &Reminder, settings: &GuildSettings, tz: &str, now: DateTime<Utc>, run: &RunCtx) -> anyhow::Result<RunResult> {
    let s = Schedule::from(r);
    let Some(next) = compute_next_run(&s, tz, now) else {
        auto_disable(app, r, "its schedule never fires").await?;
        return Ok(RunResult::Done);
    };
    let Some(slot) = due_slot(&s, tz, now) else { return Ok(RunResult::Next(next)) };
    if !is_module_enabled(Some(settings), Some(ModuleKey::Reminders)) {
        return Ok(RunResult::Next(next));
    }
    let retryable = may_retry(Some(run.attempt), Some(run.max_attempts), now.timestamp_millis() - slot.timestamp_millis(), late_tolerance_ms(&s));
    match deliver(app, r, slot, settings, run, r.require_activity, retryable).await? {
        Outcome::Retry => Ok(RunResult::Retry),
        Outcome::Disabled => Ok(RunResult::Done),
        _ => Ok(RunResult::Next(next)),
    }
}

async fn run_one_shot(app: &App, r: &Reminder, settings: &GuildSettings, now: DateTime<Utc>, run: &RunCtx) -> anyhow::Result<RunResult> {
    let Some(at) = r.run_at else {
        auto_disable(app, r, "it has no run time").await?;
        return Ok(RunResult::Done);
    };
    if at.timestamp_millis() > now.timestamp_millis() {
        return Ok(RunResult::Next(at));
    }
    if now.timestamp_millis() - at.timestamp_millis() > ONE_SHOT_LATE_MS {
        auto_disable(app, r, "it was missed by more than 60 minutes (bot offline)").await?;
        return Ok(RunResult::Done);
    }
    if !is_module_enabled(Some(settings), Some(ModuleKey::Reminders)) {
        auto_disable(app, r, "the Reminders module was off at its time").await?;
        return Ok(RunResult::Done);
    }
    let retryable = may_retry(Some(run.attempt), Some(run.max_attempts), now.timestamp_millis() - at.timestamp_millis(), ONE_SHOT_LATE_MS);
    if deliver(app, r, at, settings, run, false, retryable).await? == Outcome::Retry {
        return Ok(RunResult::Retry);
    }
    UpdateBuilder::new(&REMINDER_INFO).set("enabled", json!(false))?.filter("id", json!(r.id))?.without_returning().build()?.execute(app.db()).await?;
    Ok(RunResult::Done)
}

/// `fireReminder(job, ctx)`, the `reminder.fire` handler. Fails when the first database read fails, and when a send
/// failed for a transient reason with attempts left inside the late tolerance (the scheduler retries; the claim was
/// given back). Every other failure ends in a reschedule (recurring) or a finished job.
pub async fn fire_reminder(job: Job, app: App, attempt: i32) -> BotResult<JobOutcome> {
    let now = app.clock().now();
    let Some(id) = job.payload.get("reminderId").and_then(Value::as_str) else { return Ok(JobOutcome::Done) };
    let Some(r) = get_reminder(app.db(), &job.guild_id, id).await? else { return Ok(JobOutcome::Done) };
    if !r.enabled {
        return Ok(JobOutcome::Done);
    }
    let Some(settings) = app.settings().get_settings_safe(&job.guild_id).await else {
        return Ok(JobOutcome::Reschedule { next_run_at: now + chrono::Duration::milliseconds(MINUTE_MS), payload: None });
    };
    let tz = resolve_time_zone(Some(&settings.timezone));
    let run = RunCtx { attempt, max_attempts: job.max_attempts };
    let result = if r.mode == ReminderMode::OneShot { run_one_shot(&app, &r, &settings, now, &run).await } else { run_recurring(&app, &r, &settings, &tz, now, &run).await };
    match result {
        Ok(RunResult::Done) => Ok(JobOutcome::Done),
        Ok(RunResult::Next(at)) => Ok(JobOutcome::Reschedule { next_run_at: at, payload: None }),
        Ok(RunResult::Retry) => Err(anyhow::anyhow!("reminder {}: transient send failure, retrying (attempt {attempt}/{})", r.id, job.max_attempts)),
        Err(e) => {
            bot_util::log_error!(format!("reminder.fire {} (guild {}) failed unexpectedly", r.id, r.guild_id), bot_core::error::log_arg(&e));
            let next = if r.mode == ReminderMode::OneShot { None } else { compute_next_run(&Schedule::from(&r), &tz, now) };
            Ok(JobOutcome::Reschedule { next_run_at: next.unwrap_or(now + chrono::Duration::milliseconds(15 * MINUTE_MS)), payload: None })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `warnOnce` (`createThrottle(6 h)`) runs on the app clock: one line per reminder and kind, again after 6 h.
    #[tokio::test]
    async fn warn_once_repeats_after_six_hours_of_app_time() {
        let t = bot_core::testing::TestApp::new();
        let r: Reminder = serde_json::from_value(json!({
            "id": "rem1", "guildId": "1", "name": "n", "channelId": "2", "template": {}, "mode": "INTERVAL", "weekdays": [],
            "timeOfDay": null, "intervalMinutes": 60, "windowStart": null, "windowEnd": null, "runAt": null, "requireActivity": false,
            "pingRoleId": null, "lastSentAt": null, "lastMessageId": null, "enabled": true, "createdBy": "u",
            "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-01T00:00:00.000Z"
        }))
        .expect("a reminder row");
        let count = |logs: &[(bot_util::logger::Level, String)]| logs.iter().filter(|(_, l)| l.contains("rem1")).count();
        let ((), first) = bot_util::logger::capture(async {
            warn_once(&t, &r, "send", "boom");
            warn_once(&t, &r, "send", "boom");
        })
        .await;
        assert_eq!(count(&first), 1);
        t.clock.advance(6 * 60 * 60_000 + 1);
        let ((), later) = bot_util::logger::capture(async { warn_once(&t, &r, "send", "boom") }).await;
        assert_eq!(count(&later), 1, "6 h of app time later it is logged again");
    }
}
