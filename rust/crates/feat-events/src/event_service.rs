//! Port of `src/services/eventService.js`: community events (module `events`).
//!
//! `/event create` posts an announcement with Going / Maybe / Can't go buttons, optionally mirrors it as a Discord
//! Scheduled Event (one-way), gives a role to the members who are going, pings them shortly before the start
//! (`event.remind`) and closes the event at its end (`event.end`), which also takes the role back. An event whose voice
//! channel still has people in it at the end stays open (members can still answer and get in) until the last one
//! leaves. Everything durable is in PostgreSQL (`GuildEvent`, `EventRsvp`); the render throttle, the per-member RSVP mutex and the list of voice
//! channels waiting for their cleanup live in the app's [`EventState`] and may vanish on a restart.
//!
//! Status: SCHEDULED and ACTIVE both count as open. Nothing writes ACTIVE: an open event whose start has passed is
//! shown as "happening now".
//!
//! Event channels (`eventChannelsEnabled`): a text chat when the event is created and a voice channel shortly before
//! the start (`event.voice`), private to a temporary role held by the members going or maybe and the host.
//! `event.cleanup` deletes all three, then the announcement and its reminders, once the event is closed and no human
//! is left in the voice channel.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bot_config::{PermissionLevel, colors};
use bot_core::App;
use bot_core::deletion_tracker::mark_bot_deletion;
use bot_core::embed_builder::{apply_guild_color, error_embed, guild_embed, success_embed, warn_embed};
use bot_core::error::BoxFuture;
use bot_core::interaction::{ComponentCtx, ModalCtx};
use bot_core::interaction_helpers::{
    Payload, PermTarget, check_assignable_role_cached, check_channel_usable_for, is_interaction_gone, missing_bot_permissions, reply_error,
    safe_defer, safe_reply,
};
use bot_core::permission_checker::{MemberAccess, resolve_permission_level};
use bot_core::scheduler::{CancelJobs, Job, JobCtx, JobOutcome, ScheduleJob};
use bot_core::settings::{SettingsPatch, is_module_enabled};
use bot_core::sync::KeyedMutex;
use bot_db::{DbError, EVENT_RSVP_INFO, GUILD_EVENT_INFO, GuildEvent, GuildEventStatus, GuildSettings, InsertBuilder, RsvpStatus, SqlValue, UpdateBuilder};
use bot_util::calendar::CalendarEntry;
use bot_util::text::{clamp_text, sanitize_name};
use bot_util::timezones::ResolvedTimezone;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use serenity::all::{
    ButtonStyle, ChannelId, ChannelType, GuildChannel, GuildId, Member, MessageId, Permissions, ReactionType, RoleId, ScheduledEventId,
    ScheduledEventStatus, UserId,
};
use serenity::builder::{
    CreateActionRow, CreateAllowedMentions, CreateAttachment, CreateButton, CreateEmbed, CreateEmbedFooter, CreateInputText, CreateModal,
    EditInteractionResponse,
};

// ── Constants ─────────────────────────────────────────────────────────────────

/// `EVENT`: the limits and timings of the module.
pub mod consts {
    pub const ACTIVE_MAX: usize = bot_config::limits::EVENTS_ACTIVE;
    pub const HOST_MAX: usize = bot_config::limits::EVENTS_PER_HOST;
    /// Discord's own limits for a scheduled event.
    pub const TITLE_MAX: usize = 100;
    pub const DESCRIPTION_MAX: usize = 1000;
    pub const LOCATION_MAX: usize = 100;
    pub const CAPACITY_MAX: i64 = 10_000;
    pub const REMINDER_MAX: i64 = 1440;
    pub const DURATION_MIN_MS: i64 = 5 * 60_000;
    pub const DURATION_MAX_MS: i64 = 14 * 86_400_000;
    pub const HORIZON_MS: i64 = 365 * 86_400_000;
    /// When to end an event with no duration.
    pub const DEFAULT_END_MS: i64 = 2 * 3_600_000;
    /// The Discord event's end when there is no duration (external events need one).
    pub const DISCORD_END_MS: i64 = 3_600_000;
    pub const OVERDUE_GRACE_MS: i64 = 10 * 60_000;
    pub const GOING_SHOWN: usize = 15;
    pub const LIST_SHOWN: usize = 25;
    pub const ATTENDEES_MAX: i64 = 1000;
    pub const RENDER_INTERVAL_MS: i64 = 5_000;
    pub const NAME_LOOKUP_MS: u64 = 4_000;
    pub const REMIND_USERS_MAX: i64 = 500;
    pub const REMIND_CHUNK_USERS: usize = 80;
    pub const REMIND_CHUNK_CHARS: usize = 1900;
    pub const STRIP_BATCH: usize = 100;
    pub const STRIP_CONCURRENCY: usize = 3;
    pub const STRIP_RUNS_MAX: i64 = 50;
    pub const STRIP_RESUME_MS: i64 = 15_000;
    pub const VOICE_LEAD_DEFAULT: i64 = 30;
    pub const VOICE_LEAD_MAX: i64 = 1440;
    pub const CATEGORY_NAME: &str = "Events";
    pub const RENAME_MS: u64 = 2_000;
    /// From the last leave to the delete: a dropped connection can come back.
    pub const EMPTY_GRACE_MS: i64 = 60_000;
    /// How often `event.cleanup` and `event.end` look at a voice channel that still has people in it.
    pub const CLEANUP_RECHECK_MS: i64 = 10 * 60_000;
}
use consts as EVENT;

pub const REMIND_JOB_TYPE: &str = "event.remind";
pub const END_JOB_TYPE: &str = "event.end";
pub const VOICE_JOB_TYPE: &str = "event.voice";
pub const CLEANUP_JOB_TYPE: &str = "event.cleanup";

/// `remindJobKey(eventId)`.
pub fn remind_job_key(event_id: &str) -> String {
    format!("event.remind:{event_id}")
}
/// `endJobKey(eventId)`.
pub fn end_job_key(event_id: &str) -> String {
    format!("event.end:{event_id}")
}
/// `voiceJobKey(eventId)`.
pub fn voice_job_key(event_id: &str) -> String {
    format!("event.voice:{event_id}")
}
/// `cleanupJobKey(eventId)`.
pub fn cleanup_job_key(event_id: &str) -> String {
    format!("event.cleanup:{event_id}")
}

/// `OPEN_STATUSES`.
pub const OPEN_STATUSES: [GuildEventStatus; 2] = [GuildEventStatus::Scheduled, GuildEventStatus::Active];
/// The SQL form of `status: { in: OPEN_STATUSES }`.
const OPEN_SQL: &str = r#""status" IN ('SCHEDULED', 'ACTIVE')"#;

/// `STATUS_BY_ACTION`: the button action of each RSVP.
pub fn status_by_action(action: &str) -> Option<RsvpStatus> {
    match action {
        "going" => Some(RsvpStatus::Going),
        "maybe" => Some(RsvpStatus::Maybe),
        "declined" => Some(RsvpStatus::Declined),
        _ => None,
    }
}

/// `RSVP_LABELS`.
pub fn rsvp_label(s: RsvpStatus) -> &'static str {
    match s {
        RsvpStatus::Going => "Going",
        RsvpStatus::Maybe => "Maybe",
        RsvpStatus::Declined => "Can't go",
    }
}

/// `RSVP_EMOJI`.
pub fn rsvp_emoji(s: RsvpStatus) -> &'static str {
    match s {
        RsvpStatus::Going => "✅",
        RsvpStatus::Maybe => "❔",
        RsvpStatus::Declined => "❌",
    }
}

const WARN_EVERY_MS: i64 = 10 * 60_000;
const RENDER_SWEEP_AT: usize = 500;
const RENDER_IDLE_MS: i64 = 10 * 60_000;
/// `Date.UTC(2020, 0, 1)`.
const SEQUENCE_EPOCH: i64 = 1_577_836_800_000;
/// What the access role, and the bot, may do in an event channel.
pub const TEXT_ALLOW: [&str; 3] = ["ViewChannel", "SendMessages", "ReadMessageHistory"];
pub const VOICE_ALLOW: [&str; 3] = ["ViewChannel", "Connect", "Speak"];
pub const CALENDAR_HINT: &str =
    "Add it to your calendar with the buttons below, or open the attached file in Apple Calendar or any other calendar app.";

// Discord API error codes we react to.
const UNKNOWN_CHANNEL: u64 = 10003;
const UNKNOWN_ROLE: u64 = 10011;
const UNKNOWN_MESSAGE: u64 = 10008;
const UNKNOWN_MEMBER: u64 = 10007;
const UNKNOWN_SCHEDULED_EVENT: u64 = 10070;
/// Missing access / permissions / archived thread.
const NO_ACCESS_CODES: [u64; 3] = [50001, 50013, 50083];

// ── Raw SQL ───────────────────────────────────────────────────────────────────

/// `createLockSql(guildId)`: one creator at a time per guild (the key is `event-create:{guildId}`).
pub const CREATE_LOCK_SQL: &str = bot_db::tx::ADVISORY_XACT_LOCK_SQL;
/// `eventLockSql(eventId)`: the event row locked FOR UPDATE.
pub const EVENT_LOCK_SQL: &str = r#"SELECT id, "guildId", status::text AS status, capacity FROM "GuildEvent" WHERE id = $1 FOR UPDATE"#;

/// One entry per raw statement, with sample parameters (`RAW_SQL`).
pub fn raw_sql() -> Vec<(&'static str, &'static str, Vec<String>)> {
    vec![("events.createLock", CREATE_LOCK_SQL, vec!["event-create:0".into()]), ("events.rowLock", EVENT_LOCK_SQL, vec!["0".into()])]
}

// ── Small helpers ─────────────────────────────────────────────────────────────

fn ms(d: &DateTime<Utc>) -> i64 {
    d.timestamp_millis()
}

fn at(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).unwrap_or_default()
}

fn ts(ms: i64, style: &str) -> String {
    bot_util::time::discord_timestamp_ms(ms as f64, style)
}

fn tsd(d: &DateTime<Utc>, style: &str) -> String {
    ts(ms(d), style)
}

fn iso(d: &DateTime<Utc>) -> String {
    bot_db::serde_js::format_js_date(d)
}

fn parse_id(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|v| *v != 0)
}

fn gid(s: &str) -> Option<GuildId> {
    parse_id(s).map(GuildId::new)
}

fn cid(s: &str) -> Option<ChannelId> {
    parse_id(s).map(ChannelId::new)
}

fn rid(s: &str) -> Option<RoleId> {
    parse_id(s).map(RoleId::new)
}

fn uid(s: &str) -> Option<UserId> {
    parse_id(s).map(UserId::new)
}

fn no_mentions() -> CreateAllowedMentions {
    CreateAllowedMentions::new()
}

/// The Discord error code of an error, when it carries one (a serenity error inside an `anyhow::Error`).
pub fn code_of(e: &anyhow::Error) -> Option<u64> {
    e.downcast_ref::<serenity::Error>().and_then(bot_core::discord::error_code)
}

/// `reasonOf(err)`: the error's message, clamped to 120.
fn reason_of(e: &anyhow::Error) -> String {
    let msg = match e.downcast_ref::<serenity::Error>() {
        Some(s) => bot_core::discord::error_message(s),
        None => e.to_string(),
    };
    clamp_text(&msg, 120)
}

fn serenity_reason(e: &serenity::Error) -> String {
    clamp_text(&bot_core::discord::error_message(e), 120)
}

fn log_err(e: &anyhow::Error) -> bot_util::logger::LogArg {
    bot_core::error::log_arg(e)
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

/// `isOpen(event)`.
pub fn is_open(event: Option<&GuildEvent>) -> bool {
    event.is_some_and(|e| OPEN_STATUSES.contains(&e.status))
}

/// `callKeepsOpen(event, humans)`: whether the event's call keeps it open past its end: an open event whose voice channel
/// has people in it.
pub fn call_keeps_open(event: &GuildEvent, humans: usize) -> bool {
    is_open(Some(event)) && event.voice_channel_id.as_deref().is_some_and(|v| !v.is_empty()) && humans > 0
}

/// `eventEnd(event)`: its end, else start + 2 h (the end job's time).
pub fn event_end(event: &GuildEvent) -> DateTime<Utc> {
    end_of(&event.starts_at, event.ends_at.as_ref())
}

fn end_of(starts_at: &DateTime<Utc>, ends_at: Option<&DateTime<Utc>>) -> DateTime<Utc> {
    match ends_at {
        Some(e) => *e,
        None => at(ms(starts_at) + EVENT::DEFAULT_END_MS),
    }
}

/// `remindAt(event)`: when the reminder is due, or `None` for no reminder.
pub fn remind_at(event: &GuildEvent) -> Option<DateTime<Utc>> {
    remind_at_of(&event.starts_at, i64::from(event.reminder_minutes))
}

/// [`remind_at`] from the fields.
pub fn remind_at_of(starts_at: &DateTime<Utc>, reminder_minutes: i64) -> Option<DateTime<Utc>> {
    (reminder_minutes > 0).then(|| at(ms(starts_at) - reminder_minutes * 60_000))
}

/// `voiceLead(settings)`: the guild's minutes between the voice channel opening and the start.
pub fn voice_lead(settings: Option<&GuildSettings>) -> i64 {
    settings.map(|s| i64::from(s.event_voice_lead_minutes)).unwrap_or(EVENT::VOICE_LEAD_DEFAULT)
}

/// `voiceLeadText(minutes)`.
pub fn voice_lead_text(minutes: i64) -> String {
    if minutes > 0 { format!("{minutes} minute{} before the start", if minutes == 1 { "" } else { "s" }) } else { "at the start".into() }
}

/// `voiceAt(event, lead, now)`: `lead` minutes before the start, or now when that has passed.
pub fn voice_at(starts_at: &DateTime<Utc>, lead: i64, now_ms: i64) -> DateTime<Utc> {
    at((ms(starts_at) - lead * 60_000).max(now_ms))
}

/// `textChannelName(title)`.
pub fn text_channel_name(title: &str) -> String {
    sanitize_name(title, 90, "event")
}

/// One permission overwrite as Discord takes it (`{ id, type, allow, deny }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventOverwrite {
    pub id: String,
    /// 0 = role, 1 = member.
    pub kind: u8,
    pub allow: Permissions,
    pub deny: Permissions,
}

impl EventOverwrite {
    pub fn to_json(&self) -> Value {
        json!({ "id": self.id, "type": self.kind, "allow": self.allow.bits().to_string(), "deny": self.deny.bits().to_string() })
    }
}

fn bits_of(keys: &[&str]) -> Permissions {
    keys.iter().filter_map(|k| bot_core::permissions::flag(k)).fold(Permissions::empty(), |a, b| a | b)
}

/// `eventOverwrites(guild, roleId, allow)`: the access role and the bot see the channel; @everyone is denied, so it
/// does not follow the category.
pub fn event_overwrites(guild_id: &str, role_id: &str, bot_id: &str, allow: &[&str]) -> Vec<EventOverwrite> {
    let mut bot_allow: Vec<&str> = allow.to_vec();
    bot_allow.push("ManageChannels");
    vec![
        EventOverwrite { id: guild_id.into(), kind: 0, allow: Permissions::empty(), deny: bits_of(&["ViewChannel"]) },
        EventOverwrite { id: role_id.into(), kind: 0, allow: bits_of(allow), deny: Permissions::empty() },
        EventOverwrite { id: bot_id.into(), kind: 1, allow: bits_of(&bot_allow), deny: Permissions::empty() },
    ]
}

/// Why a duration was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationError {
    Invalid,
    TooShort,
    TooLong,
}

impl DurationError {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::TooShort => "too-short",
            Self::TooLong => "too-long",
        }
    }

    /// `DURATION_ERRORS[code]`.
    pub fn text(self) -> &'static str {
        match self {
            Self::Invalid => "Use a duration like `90m`, `2h` or `1d`.",
            Self::TooShort => "An event must run for at least 5 minutes.",
            Self::TooLong => "An event can run for at most 14 days.",
        }
    }
}

/// `parseDurationInput(raw)`: `Ok(None)` for blank (no duration).
pub fn parse_duration_input(raw: Option<&str>) -> Result<Option<i64>, DurationError> {
    let Some(raw) = raw else { return Ok(None) };
    let t = bot_util::js::trim(raw);
    if t.is_empty() {
        return Ok(None);
    }
    let ms = bot_util::parse_duration::parse_duration(t).ok_or(DurationError::Invalid)?;
    if ms < EVENT::DURATION_MIN_MS {
        return Err(DurationError::TooShort);
    }
    if ms > EVENT::DURATION_MAX_MS {
        return Err(DurationError::TooLong);
    }
    Ok(Some(ms))
}

/// Why a start time is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartProblem {
    Past,
    Far,
}

/// `startProblem(date, now)` (`None` for an invalid date is `Past`, as `NaN` is).
pub fn start_problem(date_ms: Option<i64>, now_ms: i64) -> Option<StartProblem> {
    match date_ms {
        None => Some(StartProblem::Past),
        Some(m) if m <= now_ms => Some(StartProblem::Past),
        Some(m) if m > now_ms + EVENT::HORIZON_MS => Some(StartProblem::Far),
        _ => None,
    }
}

/// `formatStartForEdit(date, zone)`: 'YYYY-MM-DD HH:MM Region/City', what the edit form shows and `parseWhen`
/// reads back exactly.
pub fn format_start_for_edit(date: &DateTime<Utc>, zone: &str) -> String {
    let p = bot_util::time::zoned_parts(date, zone);
    format!("{}-{:02}-{:02} {:02}:{:02} {zone}", p.year, p.month, p.day, p.hour, p.minute)
}

/// `formatStartFields(date, zone)`: the Day and Time boxes of the Date & time form.
pub fn format_start_fields(date: &DateTime<Utc>, zone: &str) -> (String, String) {
    let text = format_start_for_edit(date, zone);
    let mut parts = text.split(' ');
    (parts.next().unwrap_or_default().to_string(), parts.next().unwrap_or_default().to_string())
}

/// `jumpUrl(event)`: the announcement's link, or '' without one.
pub fn jump_url(event: &GuildEvent) -> String {
    match &event.message_id {
        Some(m) if !m.is_empty() => format!("https://discord.com/channels/{}/{}/{m}", event.guild_id, event.channel_id),
        _ => String::new(),
    }
}

/// `{ going, maybe, declined, goingIds }`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tally {
    pub going: i64,
    pub maybe: i64,
    pub declined: i64,
    pub going_ids: Vec<String>,
}

/// `tallyFromGroups(groups, goingIds)`: `groups` are `(status, count)`.
pub fn tally_from_groups(groups: &[(String, i64)], going_ids: Vec<String>) -> Tally {
    let mut t = Tally { going_ids, ..Default::default() };
    for (status, n) in groups {
        match status.as_str() {
            "GOING" => t.going = *n,
            "MAYBE" => t.maybe = *n,
            "DECLINED" => t.declined = *n,
            _ => {}
        }
    }
    t
}

/// `emptyTally()`.
pub fn empty_tally() -> Tally {
    Tally::default()
}

/// `buildEventEmbed(event, tally, { settings, now, names })`: the card shown in the channel. `names`: id -> name of
/// the members going, printed under their mentions.
pub fn build_event_embed(
    event: &GuildEvent,
    tally: &Tally,
    settings: Option<&GuildSettings>,
    now_ms: i64,
    names: Option<&HashMap<String, String>>,
) -> CreateEmbed {
    let start = ms(&event.starts_at);
    let title = clamp_text(&event.title, 200);
    let description = clamp_text(event.description.as_deref().unwrap_or(""), EVENT::DESCRIPTION_MAX);
    let mut embed = guild_embed(settings, Some(&format!("📅 {title}")), Some(&description).filter(|d| !d.is_empty()).map(|s| s.as_str()), None);
    let started = start <= now_ms;
    let open = is_open(Some(event));
    match event.status {
        GuildEventStatus::Cancelled => embed = embed.color(colors::ERROR).title(format!("🚫 Cancelled: {title}")),
        GuildEventStatus::Ended => embed = embed.color(colors::NEUTRAL).title(format!("🏁 Ended: {title}")),
        _ => embed = apply_guild_color(embed, settings),
    }
    let mut when = vec![ts(start, "F"), if started && open { "**Happening now**".to_string() } else { ts(start, "R") }];
    if let Some(e) = &event.ends_at {
        when.push(format!("Until {}", tsd(e, "f")));
    }
    let mut fields: Vec<(String, String, bool)> = vec![("When".into(), when.join("\n"), false)];
    if let Some(l) = event.location.as_deref().filter(|l| !l.is_empty()) {
        fields.push(("Where".into(), clamp_text(l, EVENT::LOCATION_MAX), true));
    }
    fields.push(("Host".into(), format!("<@{}>", event.host_id), true));
    if let Some(r) = event.role_id.as_deref().filter(|r| !r.is_empty()) {
        fields.push(("Role".into(), format!("<@&{r}>"), true));
    }
    if open && let Some(t) = event.text_channel_id.as_deref().filter(|t| !t.is_empty()) {
        fields.push(("Chat".into(), format!("<#{t}>"), true));
    }
    if open && let Some(v) = event.voice_channel_id.as_deref().filter(|v| !v.is_empty()) {
        fields.push(("Voice".into(), format!("<#{v}>"), true));
    }
    let cap = match event.capacity {
        Some(c) if c != 0 => format!(" / {c}"),
        _ => String::new(),
    };
    let shown: Vec<&String> = tally.going_ids.iter().take(EVENT::GOING_SHOWN).collect();
    let hidden = (tally.going - shown.len() as i64).max(0);
    let shown_names: Vec<String> = shown
        .iter()
        .filter_map(|id| names.and_then(|n| n.get(*id)))
        .filter(|n| !n.is_empty())
        .map(|n| bot_core::markdown::escape_markdown(&clamp_text(n, 32)))
        .collect();
    let going_text = if shown.is_empty() {
        "No one yet".to_string()
    } else {
        let mentions = shown.iter().map(|id| format!("<@{id}>")).collect::<Vec<_>>().join(" ");
        let more = if hidden > 0 { format!(" +{hidden} more") } else { String::new() };
        let sub = if shown_names.is_empty() { String::new() } else { format!("\n-# {}", shown_names.join(" · ")) };
        clamp_text(&format!("{mentions}{more}{sub}"), 1024)
    };
    fields.push((format!("✅ Going ({}{cap})", tally.going), going_text, false));
    fields.push(("❔ Maybe".into(), tally.maybe.to_string(), true));
    for (n, v, i) in fields {
        embed = embed.field(n, v, i);
    }
    let footer = match event.status {
        GuildEventStatus::Cancelled => "This event was cancelled.",
        GuildEventStatus::Ended => "This event has ended.",
        _ if event.text_channel_id.as_deref().is_some_and(|t| !t.is_empty()) => "Going or Maybe gets you into the event chat.",
        _ => "RSVP with the buttons below.",
    };
    embed.footer(CreateEmbedFooter::new(footer))
}

fn cid_of(prefix: &str, params: &[&str]) -> String {
    bot_config::build_custom_id(prefix, params).unwrap_or_else(|_| format!("{prefix}:{}", params.join(":")))
}

/// `buildEventComponents(event, tally, { disabled })`: Going / Maybe / Can't go / Add to calendar
/// (`event:{eventId}:going|maybe|declined|calendar`). `disabled` defaults to "not open".
pub fn build_event_components(event: &GuildEvent, tally: &Tally, disabled: Option<bool>) -> Vec<CreateActionRow> {
    let disabled = disabled.unwrap_or(!is_open(Some(event)));
    let full = event.capacity.is_some_and(|c| c != 0 && tally.going >= i64::from(c));
    let button = |action: &str, label: &str, emoji: &str, style: ButtonStyle, off: bool| {
        CreateButton::new(cid_of("event", &[&event.id, action]))
            .label(label)
            .emoji(ReactionType::Unicode(emoji.into()))
            .style(style)
            .disabled(off)
    };
    vec![CreateActionRow::Buttons(vec![
        button("going", if full { "Going (full)" } else { "Going" }, "✅", ButtonStyle::Success, disabled || full),
        button("maybe", "Maybe", "❔", ButtonStyle::Secondary, disabled),
        button("declined", "Can't go", "❌", ButtonStyle::Danger, disabled),
        button("calendar", "Add to calendar", "📅", ButtonStyle::Secondary, disabled),
    ])]
}

/// `buildEventPayload(event, tally, { settings, now, names })`.
pub fn build_event_payload(
    event: &GuildEvent,
    tally: &Tally,
    settings: Option<&GuildSettings>,
    now_ms: i64,
    names: Option<&HashMap<String, String>>,
) -> Payload {
    Payload::new()
        .embeds(vec![build_event_embed(event, tally, settings, now_ms, names)])
        .components(build_event_components(event, tally, None))
        .no_mentions()
}

/// One reminder message: its text and the users it may ping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReminderMessage {
    pub content: String,
    pub user_ids: Vec<String>,
}

/// `reminderMessages(event, userIds, now)`: users only, each message under 2000 characters and 100 ids; the first
/// carries the heading.
pub fn reminder_messages(event: &GuildEvent, user_ids: &[String], now_ms: i64) -> Vec<ReminderMessage> {
    let start = ms(&event.starts_at);
    let when = if start > now_ms { format!("starts {}", ts(start, "R")) } else { "is starting".into() };
    let link = jump_url(event);
    let voice = match event.voice_channel_id.as_deref().filter(|v| !v.is_empty()) {
        Some(v) => format!(" Voice: <#{v}>"),
        None => String::new(),
    };
    let head = format!("⏰ **{}** {when}.{}{voice}", clamp_text(&event.title, 100), if link.is_empty() { String::new() } else { format!(" {link}") });
    let len = bot_util::js::utf16_len;
    let mut out: Vec<ReminderMessage> = Vec::new();
    let mut cur: Option<ReminderMessage> = None;
    for id in user_ids {
        let mention = format!("<@{id}>");
        if let Some(c) = &cur
            && (c.user_ids.len() >= EVENT::REMIND_CHUNK_USERS || len(&c.content) + 1 + len(&mention) > EVENT::REMIND_CHUNK_CHARS)
        {
            out.extend(cur.take());
        }
        let c = cur.get_or_insert_with(|| ReminderMessage { content: if out.is_empty() { head.clone() } else { String::new() }, user_ids: Vec::new() });
        let sep = if c.content.is_empty() {
            ""
        } else if c.user_ids.is_empty() {
            "\n"
        } else {
            " "
        };
        c.content.push_str(sep);
        c.content.push_str(&mention);
        c.user_ids.push(id.clone());
    }
    out.extend(cur);
    out
}

/// `listLine(event, going)`: one line of /event list.
pub fn list_line(event: &GuildEvent, going: i64) -> String {
    let url = jump_url(event);
    let cap = match event.capacity {
        Some(c) if c != 0 => format!("/{c}"),
        _ => String::new(),
    };
    format!(
        "**{}** · {} ({})\n✅ {going}{cap} · <#{}>{} · `{}`",
        clamp_text(&event.title, 60),
        tsd(&event.starts_at, "f"),
        tsd(&event.starts_at, "R"),
        event.channel_id,
        if url.is_empty() { String::new() } else { format!(" · [post]({url})") },
        event.id
    )
}

/// `attendeeLines(rsvps)`: one member per line, grouped Going, Maybe, Can't go. `rsvps` are `(userId, status)`.
pub fn attendee_lines(rsvps: &[(String, RsvpStatus)]) -> Vec<String> {
    let mut lines = Vec::new();
    for status in [RsvpStatus::Going, RsvpStatus::Maybe, RsvpStatus::Declined] {
        for (user, s) in rsvps {
            if *s == status {
                lines.push(format!("{} <@{user}>", rsvp_emoji(status)));
            }
        }
    }
    lines
}

/// What an RSVP did (`applyRsvp`'s `result`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RsvpResult {
    Gone,
    Closed,
    Full,
    Same,
    Set,
}

impl RsvpResult {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gone => "gone",
            Self::Closed => "closed",
            Self::Full => "full",
            Self::Same => "same",
            Self::Set => "set",
        }
    }
}

/// `{ result, previous, note }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RsvpOut {
    pub result: RsvpResult,
    pub previous: Option<RsvpStatus>,
    pub note: Option<String>,
}

impl RsvpOut {
    pub fn of(result: RsvpResult) -> Self {
        Self { result, previous: None, note: None }
    }
}

/// `rsvpReplyText(out, event, status)`: the wording for an RSVP.
pub fn rsvp_reply_text(out: &RsvpOut, event: Option<&GuildEvent>, status: RsvpStatus) -> String {
    let title = clamp_text(event.map(|e| e.title.as_str()).unwrap_or(""), 100);
    match out.result {
        RsvpResult::Gone => return "This event no longer exists.".into(),
        RsvpResult::Closed => return "This event is no longer open.".into(),
        RsvpResult::Full => {
            let cap = event.and_then(|e| e.capacity).map(|c| c.to_string()).unwrap_or_else(|| "null".into());
            return format!("**{title}** is full ({cap} going). Your RSVP was not changed.");
        }
        _ => {}
    }
    let base = if out.result == RsvpResult::Same {
        format!("You are already marked as **{}** for **{title}**.", rsvp_label(status))
    } else {
        format!("{} You are marked as **{}** for **{title}**.", rsvp_emoji(status), rsvp_label(status))
    };
    let chat = match event.and_then(|e| e.text_channel_id.as_deref()).filter(|t| !t.is_empty()) {
        Some(t) if status != RsvpStatus::Declined => format!("\n💬 Event chat: <#{t}>"),
        _ => String::new(),
    };
    match out.note.as_deref().filter(|n| !n.is_empty()) {
        Some(n) => format!("{base}{chat}\n⚠️ {n}"),
        None => format!("{base}{chat}"),
    }
}

// ── Add to calendar ───────────────────────────────────────────────────────────

/// `shownEnd(event)`: its end, else start + 1 h (the Discord event's end too).
pub fn shown_end(event: &GuildEvent) -> DateTime<Utc> {
    match &event.ends_at {
        Some(e) => *e,
        None => at(ms(&event.starts_at) + EVENT::DISCORD_END_MS),
    }
}

/// Where [`plain_discord_text`] reads names from (the guild's caches; no fetches).
pub trait NameSource {
    /// A member's display name, else the cached user's name.
    fn user_name(&self, id: &str) -> Option<String>;
    fn role_name(&self, id: &str) -> Option<String>;
    fn channel_name(&self, id: &str) -> Option<String>;
}

/// No guild: every mention falls back to its generic word.
pub struct NoNames;

impl NameSource for NoNames {
    fn user_name(&self, _: &str) -> Option<String> {
        None
    }
    fn role_name(&self, _: &str) -> Option<String> {
        None
    }
    fn channel_name(&self, _: &str) -> Option<String> {
        None
    }
}

/// Names from plain maps (a snapshot of the guild's caches).
#[derive(Debug, Clone, Default)]
pub struct NameMaps {
    pub members: HashMap<String, String>,
    pub users: HashMap<String, String>,
    pub roles: HashMap<String, String>,
    pub channels: HashMap<String, String>,
}

impl NameSource for NameMaps {
    fn user_name(&self, id: &str) -> Option<String> {
        self.members.get(id).or_else(|| self.users.get(id)).cloned()
    }
    fn role_name(&self, id: &str) -> Option<String> {
        self.roles.get(id).cloned()
    }
    fn channel_name(&self, id: &str) -> Option<String> {
        self.channels.get(id).cloned()
    }
}

/// The cached guild and the user cache, read under the cache lock.
pub struct GuildNames<'a> {
    pub guild: &'a serenity::all::Guild,
    pub cache: &'a serenity::cache::Cache,
}

impl NameSource for GuildNames<'_> {
    fn user_name(&self, id: &str) -> Option<String> {
        let u = uid(id)?;
        if let Some(m) = self.guild.members.get(&u) {
            return Some(m.display_name().to_string());
        }
        self.cache.user(u).map(|x| x.name.clone())
    }
    fn role_name(&self, id: &str) -> Option<String> {
        self.guild.roles.get(&rid(id)?).map(|r| r.name.clone())
    }
    fn channel_name(&self, id: &str) -> Option<String> {
        let c = cid(id)?;
        self.guild.channels.get(&c).map(|x| x.name.clone()).or_else(|| self.guild.threads.iter().find(|t| t.id == c).map(|t| t.name.clone()))
    }
}

fn is_digits(s: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

fn is_word(s: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Replaces every `<...>` token (no nested `<`) for which `f` returns a replacement, scanning left to right like a
/// global regex replace.
fn replace_tokens(text: &str, f: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let replaced = tail[1..].find(['>', '<']).and_then(|j| {
            if tail.as_bytes()[1 + j] != b'>' {
                return None;
            }
            let inner = &tail[1..1 + j];
            f(inner).map(|r| (r, j + 2))
        });
        match replaced {
            Some((r, used)) => {
                out.push_str(&r);
                rest = &tail[used..];
            }
            None => {
                out.push('<');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// `plainDiscordText(text, guild)`: Discord markup as plain text for a calendar. Mentions by name from the guild's
/// caches, custom emoji as `:name:`, command mentions as `/name`, timestamps as UTC times.
pub fn plain_discord_text(text: Option<&str>, names: &dyn NameSource) -> String {
    let s = text.unwrap_or("");
    // 1. custom emoji <a?:name:id>
    let s = replace_tokens(s, |t| {
        let t = t.strip_prefix('a').unwrap_or(t);
        let t = t.strip_prefix(':')?;
        let (name, id) = t.split_once(':')?;
        (is_word(name, 1, 32) && is_digits(id, 17, 20)).then(|| format!(":{name}:"))
    });
    // 2. users <@id> <@!id>
    let s = replace_tokens(&s, |t| {
        let t = t.strip_prefix('@')?;
        let t = t.strip_prefix('!').unwrap_or(t);
        is_digits(t, 17, 20).then(|| format!("@{}", names.user_name(t).unwrap_or_else(|| "someone".into())))
    });
    // 3. roles
    let s = replace_tokens(&s, |t| {
        let t = t.strip_prefix("@&")?;
        is_digits(t, 17, 20).then(|| format!("@{}", names.role_name(t).unwrap_or_else(|| "role".into())))
    });
    // 4. channels
    let s = replace_tokens(&s, |t| {
        let t = t.strip_prefix('#')?;
        is_digits(t, 17, 20).then(|| format!("#{}", names.channel_name(t).unwrap_or_else(|| "channel".into())))
    });
    // 5. command mentions </name:id>
    let s = replace_tokens(&s, |t| {
        let t = t.strip_prefix('/')?;
        let (name, id) = t.rsplit_once(':')?;
        let n16 = bot_util::js::utf16_len(name);
        ((1..=96).contains(&n16) && !name.contains([':', '>', '\n']) && is_digits(id, 17, 20)).then(|| format!("/{name}"))
    });
    // 6. timestamps <t:secs[:style]>
    replace_tokens(&s, |t| {
        let t = t.strip_prefix("t:")?;
        let (num, style) = match t.split_once(':') {
            Some((n, st)) => (n, Some(st)),
            None => (t, None),
        };
        if let Some(st) = style
            && !(st.len() == 1 && "tTdDfFR".contains(st))
        {
            return None;
        }
        let digits = num.strip_prefix('-').unwrap_or(num);
        if !is_digits(digits, 1, 13) {
            return None;
        }
        let secs: f64 = num.parse().ok()?;
        // An invalid date keeps the whole token.
        bot_util::js::to_iso_string(secs * 1000.0).map(|iso| {
            let head: String = iso.chars().take(16).collect();
            format!("{} UTC", head.replacen('T', " ", 1))
        })
    })
}

/// `calendarEntry(event, guild)`: the event as a calendar entry.
pub fn calendar_entry(event: &GuildEvent, names: &dyn NameSource) -> CalendarEntry {
    let updated = ms(&event.updated_at);
    CalendarEntry {
        uid: format!("{}@events.discord-bot", event.id),
        title: clamp_text(&event.title, EVENT::TITLE_MAX),
        description: Some(clamp_text(&plain_discord_text(event.description.as_deref(), names), EVENT::DESCRIPTION_MAX)),
        location: Some(event.location.as_deref().filter(|l| !l.is_empty()).map(|l| clamp_text(l, EVENT::LOCATION_MAX)).unwrap_or_default()),
        start: event.starts_at,
        end: shown_end(event),
        url: Some(jump_url(event)),
        // Seconds since 2020: grows with every change to the row and stays well inside a 32-bit SEQUENCE.
        sequence: Some(((updated - SEQUENCE_EPOCH).div_euclid(1000)).max(0) as f64),
    }
}

/// The calendar file's name (`{sanitized title}.ics`).
pub fn calendar_file_name(title: &str) -> String {
    format!("{}.ics", sanitize_name(title, 50, "event"))
}

/// `buildCalendarPayload(event, { guild, now, content })`: Google Calendar and Outlook.com link buttons and an .ics
/// file, no mentions.
pub fn build_calendar_payload(event: &GuildEvent, names: &dyn NameSource, now_ms: i64, content: Option<String>) -> Payload {
    let entry = calendar_entry(event, names);
    let content = content.unwrap_or_else(|| format!("📅 **{}**\n{CALENDAR_HINT}", clamp_text(&event.title, 100)));
    let row = CreateActionRow::Buttons(vec![
        CreateButton::new_link(crate::calendar::google_url(&entry)).label("Google Calendar"),
        CreateButton::new_link(crate::calendar::outlook_url(&entry)).label("Outlook"),
    ]);
    let ics = crate::calendar::ics(&entry, at(now_ms));
    Payload::text(content)
        .components(vec![row])
        .file(CreateAttachment::bytes(ics.into_bytes(), calendar_file_name(&event.title)))
        .no_mentions()
}

// ── Time zone ─────────────────────────────────────────────────────────────────

/// What [`load_zone`] found.
#[derive(Debug, Clone)]
pub struct LoadedZone {
    pub resolved: ResolvedTimezone,
    pub settings: Option<bot_core::Settings>,
    /// The member's own zone could not be read in time (another source was used).
    pub personal_unknown: bool,
}

/// `loadZone(interaction, { budgetMs })`: the caller's zone (their own, else their timezone role, else the
/// server's). Lookups are bounded; a slow or failed one falls back to the next source. Never fails.
pub async fn load_zone(app: &App, guild_id: Option<GuildId>, user_id: UserId, member: Option<&Member>, budget_ms: u64) -> LoadedZone {
    let settings_fut = async {
        match guild_id {
            Some(g) => app.settings().peek_or_load(&g.to_string(), budget_ms).await,
            None => None,
        }
    };
    let (a, u) = (app.clone(), user_id.to_string());
    let personal_fut = async move {
        let load = async move { Ok::<_, std::convert::Infallible>(feat_translation::user_timezone_service::get_user_timezone(&a, &u, false).await) };
        bot_util::async_util::with_timeout(load, budget_ms, "event user timezone").await.ok().flatten()
    };
    let (settings, personal) = tokio::join!(settings_fut, personal_fut);
    let roles = feat_translation::user_timezone_service::member_role_ids(member);
    let personal_zone = personal.clone().flatten();
    let resolve = |g: Option<&serenity::all::Guild>| {
        feat_translation::user_timezone_service::resolve_member_timezone(&roles, g, settings.as_deref(), personal_zone.as_deref())
    };
    let resolved = match guild_id.and_then(|g| bot_core::interaction_helpers::with_cached_guild(app, g, |guild| resolve(Some(guild)))) {
        Some(r) => r,
        None => resolve(None),
    };
    LoadedZone { resolved, settings, personal_unknown: personal.is_none() }
}

// ── Per-app state ─────────────────────────────────────────────────────────────

/// The render hook: replaces the Discord render in tests (`__setRenderImpl`).
pub type RenderFn = Arc<dyn Fn(App, String) -> BoxFuture<anyhow::Result<RenderOutcome>> + Send + Sync>;

#[derive(Default)]
struct RenderSlot {
    last_at: i64,
    timer: Option<tokio::task::JoinHandle<()>>,
}

/// What the JS keeps in module-level maps.
#[derive(Default)]
pub struct EventState {
    render: Mutex<HashMap<String, RenderSlot>>,
    warned_at: Mutex<HashMap<String, i64>>,
    /// Voice channels of events with people still in them: id -> (eventId, guildId, open). `open`: the event is past its
    /// end and stays open while the call has people (`event.end` fills it; the last leave queues `event.end`); otherwise
    /// the event is closed and waits for `event.cleanup` (which fills it; the last leave queues the cleanup). Both jobs
    /// fill it again every CLEANUP_RECHECK_MS, so a restart only delays the close or the cleanup (by up to 10 minutes),
    /// and voice leaves elsewhere cost no database query.
    waiting_voice: Mutex<HashMap<String, (String, String, bool)>>,
    render_impl: Mutex<Option<RenderFn>>,
    render_mutex: KeyedMutex,
    user_mutex: KeyedMutex,
    category_mutex: KeyedMutex,
}

fn state(app: &App) -> Arc<EventState> {
    app.ext::<EventState>()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// `__setRenderImpl(fn)`: replaces the Discord render (tests). `None` restores it.
pub fn set_render_impl(app: &App, f: Option<RenderFn>) {
    *lock(&state(app).render_impl) = f;
}

/// Whether a voice channel is waiting for its cleanup (`waitingVoice.has`).
pub fn is_waiting_voice(app: &App, channel_id: &str) -> bool {
    lock(&state(app).waiting_voice).contains_key(channel_id)
}

/// `warnThrottled(key, message, err)`: at most one warning per key every 10 minutes.
fn warn_throttled(app: &App, key: &str, message: &str, err: Option<bot_util::logger::LogArg>) {
    let now = app.clock().now_ms();
    {
        let st = state(app);
        let mut w = lock(&st.warned_at);
        if now - w.get(key).copied().unwrap_or(0) < WARN_EVERY_MS {
            return;
        }
        if w.len() > 200 {
            w.retain(|_, t| now - *t < WARN_EVERY_MS);
        }
        w.insert(key.to_string(), now);
    }
    match err {
        Some(e) => bot_util::log_warn!(message.to_string(), e),
        None => bot_util::log_warn!(message.to_string()),
    }
}

/// `guildFor(client, guildId)`: the cached, available guild's id.
fn guild_for(app: &App, guild_id: &str) -> Option<GuildId> {
    let g = gid(guild_id)?;
    let d = app.discord()?;
    d.cache.guild(g).map(|_| g)
}

fn bot_id(app: &App) -> Option<UserId> {
    app.discord().map(|d| d.bot_user_id())
}

// ── Database ──────────────────────────────────────────────────────────────────

/// `findUnique({ where: { id } })`.
pub async fn find_event(app: &App, event_id: &str) -> anyhow::Result<Option<GuildEvent>> {
    Ok(sqlx::query_as::<_, GuildEvent>(r#"SELECT * FROM "GuildEvent" WHERE "id" = $1"#).bind(event_id).fetch_optional(app.db()).await.map_err(DbError::from)?)
}

/// `findFirst({ where: { id, guildId } })`.
pub async fn find_guild_event(app: &App, event_id: &str, guild_id: &str) -> anyhow::Result<Option<GuildEvent>> {
    Ok(sqlx::query_as::<_, GuildEvent>(r#"SELECT * FROM "GuildEvent" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#)
        .bind(event_id)
        .bind(guild_id)
        .fetch_optional(app.db())
        .await
        .map_err(DbError::from)?)
}

/// `updateMany({ where: { id }, data })` with plain equality filters (`updatedAt` set, as Prisma does).
async fn update_events(app: &App, filters: &[(&str, Value)], set: Vec<(&str, Value)>) -> anyhow::Result<u64> {
    let mut b = UpdateBuilder::new(&GUILD_EVENT_INFO).set_all(set)?;
    for (k, v) in filters {
        b = b.filter(k, v.clone())?;
    }
    Ok(b.without_returning().build()?.execute(app.db()).await?)
}

/// The same, limited to open events.
async fn update_open_events(app: &App, filters: &[(&str, Value)], set: Vec<(&str, SqlValueOrJson)>) -> anyhow::Result<u64> {
    let mut b = UpdateBuilder::new(&GUILD_EVENT_INFO);
    for (k, v) in set {
        b = match v {
            SqlValueOrJson::Json(j) => b.set(k, j)?,
            SqlValueOrJson::Sql(s) => b.set_typed(k, s)?,
        };
    }
    for (k, v) in filters {
        b = b.filter(k, v.clone())?;
    }
    let mut q = b.without_returning().build()?;
    q.sql.push_str(&format!(" AND {OPEN_SQL}"));
    Ok(q.execute(app.db()).await?)
}

enum SqlValueOrJson {
    Json(Value),
    Sql(SqlValue),
}

fn ts_value(d: Option<&DateTime<Utc>>) -> SqlValueOrJson {
    SqlValueOrJson::Sql(match d {
        Some(d) => SqlValue::timestamp(*d),
        None => SqlValue::Timestamp(None),
    })
}

/// `getTally(eventId)`.
pub async fn get_tally(app: &App, event_id: &str) -> anyhow::Result<Tally> {
    let groups: Vec<(String, i64)> =
        sqlx::query_as(r#"SELECT "status"::text AS status, COUNT(*)::bigint AS n FROM "EventRsvp" WHERE "eventId" = $1 GROUP BY "status""#)
            .bind(event_id)
            .fetch_all(app.db())
            .await
            .map_err(DbError::from)?;
    let going: Vec<(String,)> = sqlx::query_as(
        r#"SELECT "userId" FROM "EventRsvp" WHERE "eventId" = $1 AND "status" = 'GOING' ORDER BY "updatedAt" ASC LIMIT $2"#,
    )
    .bind(event_id)
    .bind(EVENT::GOING_SHOWN as i64)
    .fetch_all(app.db())
    .await
    .map_err(DbError::from)?;
    Ok(tally_from_groups(&groups, going.into_iter().map(|r| r.0).collect()))
}

/// The ids of a pasted link to an announcement: `/channels/{guild}/{channel}/{message}` at the end of the text.
fn parse_post_link(text: &str) -> Option<(String, String, String)> {
    let t = text.strip_suffix('/').unwrap_or(text);
    let mut parts = t.rsplitn(5, '/');
    let m = parts.next()?;
    let c = parts.next()?;
    let g = parts.next()?;
    let word = parts.next()?;
    if word != "channels" || parts.next().is_none() {
        return None;
    }
    (is_digits(g, 17, 20) && is_digits(c, 17, 20) && is_digits(m, 17, 20)).then(|| (g.to_string(), c.to_string(), m.to_string()))
}

/// `resolveEventRef(guildId, raw)`: an event of the guild by id, or by a pasted link to its announcement.
pub async fn resolve_event_ref(app: &App, guild_id: &str, raw: &str) -> anyhow::Result<Option<GuildEvent>> {
    let text = bot_util::js::trim(raw);
    if text.is_empty() {
        return Ok(None);
    }
    if let Some((g, _, m)) = parse_post_link(text) {
        if g != guild_id {
            return Ok(None);
        }
        return Ok(sqlx::query_as::<_, GuildEvent>(r#"SELECT * FROM "GuildEvent" WHERE "guildId" = $1 AND "messageId" = $2 LIMIT 1"#)
            .bind(guild_id)
            .bind(m)
            .fetch_optional(app.db())
            .await
            .map_err(DbError::from)?);
    }
    find_guild_event(app, bot_util::js::utf16_prefix(text, 40), guild_id).await
}

/// `listOpenEvents(guildId)`: open events (soonest first) with the number going.
pub async fn list_open_events(app: &App, guild_id: &str) -> anyhow::Result<(Vec<GuildEvent>, HashMap<String, i64>)> {
    let events: Vec<GuildEvent> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(r#"SELECT * FROM "GuildEvent" WHERE "guildId" = $1 AND {OPEN_SQL} ORDER BY "startsAt" ASC LIMIT $2"#)))
            .bind(guild_id)
            .bind(EVENT::ACTIVE_MAX as i64)
            .fetch_all(app.db())
            .await
            .map_err(DbError::from)?;
    let mut going = HashMap::new();
    if !events.is_empty() {
        let ids: Vec<String> = events.iter().map(|e| e.id.clone()).collect();
        let rows: Vec<(String, i64)> = sqlx::query_as(
            r#"SELECT "eventId", COUNT(*)::bigint FROM "EventRsvp" WHERE "eventId" = ANY($1) AND "status" = 'GOING' GROUP BY "eventId""#,
        )
        .bind(&ids)
        .fetch_all(app.db())
        .await
        .map_err(DbError::from)?;
        going.extend(rows);
    }
    Ok((events, going))
}

/// One autocomplete row (`select: { id, title, startsAt, status }`).
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct EventChoice {
    pub id: String,
    pub title: String,
    #[sqlx(rename = "startsAt")]
    pub starts_at: chrono::NaiveDateTime,
    pub status: String,
}

/// `findEventsForAutocomplete({ guildId, sub, q, hostId })`: edit/cancel offer open events (with `host_id`, only that
/// member's), info/attendees also the latest closed ones.
pub async fn find_events_for_autocomplete(app: &App, guild_id: &str, sub: &str, q: &str, host_id: Option<&str>) -> anyhow::Result<Vec<EventChoice>> {
    let open = sub == "edit" || sub == "cancel";
    let query = bot_util::js::utf16_prefix(bot_util::js::trim(q), 100).to_string();
    let mut sql = r#"SELECT "id", "title", "startsAt", "status"::text AS status FROM "GuildEvent" WHERE "guildId" = $1"#.to_string();
    let mut binds: Vec<String> = vec![guild_id.to_string()];
    if open {
        sql.push_str(&format!(" AND {OPEN_SQL}"));
        if let Some(h) = host_id.filter(|h| !h.is_empty()) {
            binds.push(h.to_string());
            sql.push_str(&format!(r#" AND "hostId" = ${}"#, binds.len()));
        }
    }
    if !query.is_empty() {
        // Prisma's `contains` with `mode: 'insensitive'`: ILIKE '%q%' (wildcards in q are not escaped).
        binds.push(format!("%{query}%"));
        sql.push_str(&format!(r#" AND "title"::text ILIKE ${}"#, binds.len()));
    }
    sql.push_str(if open { r#" ORDER BY "startsAt" ASC LIMIT 25"# } else { r#" ORDER BY "startsAt" DESC LIMIT 25"# });
    let mut qy = sqlx::query_as::<_, EventChoice>(sqlx::AssertSqlSafe(sql));
    for b in binds {
        qy = qy.bind(b);
    }
    Ok(qy.fetch_all(app.db()).await.map_err(DbError::from)?)
}

/// The data of a new event (`createEvent`'s `data`).
#[derive(Debug, Clone, PartialEq)]
pub struct NewEvent {
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub starts_at: DateTime<Utc>,
    pub ends_at: Option<DateTime<Utc>>,
    pub host_id: String,
    pub role_id: Option<String>,
    pub capacity: Option<i32>,
    pub reminder_minutes: i32,
}

/// `createEventRow`'s outcome.
#[derive(Debug, Clone, PartialEq)]
pub enum CreateRow {
    Created(Box<GuildEvent>),
    Limit,
    HostLimit,
}

/// `createEventRow(guildId, data, { hostMax })`: inserts under the guild's open-event cap and, with `host_max`, the
/// host's own cap (staff pass `None`). One creator at a time per guild (advisory lock).
pub async fn create_event_row(app: &App, guild_id: &str, data: &NewEvent, channel_id: &str, host_max: Option<usize>) -> anyhow::Result<CreateRow> {
    app.settings().ensure_guild_settings(guild_id).await?;
    let (g, d, ch) = (guild_id.to_string(), data.clone(), channel_id.to_string());
    bot_db::transaction(app.db(), bot_db::TxOptions::TEN_SECONDS, move |tx| {
        Box::pin(async move {
            bot_db::advisory_xact_lock(tx, &format!("event-create:{g}")).await?;
            let (open,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(format!(r#"SELECT COUNT(*)::bigint FROM "GuildEvent" WHERE "guildId" = $1 AND {OPEN_SQL}"#)))
                .bind(&g)
                .fetch_one(&mut *tx)
                .await
                .map_err(DbError::from)?;
            if open >= EVENT::ACTIVE_MAX as i64 {
                return Ok(CreateRow::Limit);
            }
            if let Some(max) = host_max {
                let (own,): (i64,) =
                    sqlx::query_as(sqlx::AssertSqlSafe(format!(r#"SELECT COUNT(*)::bigint FROM "GuildEvent" WHERE "guildId" = $1 AND "hostId" = $2 AND {OPEN_SQL}"#)))
                        .bind(&g)
                        .bind(&d.host_id)
                        .fetch_one(&mut *tx)
                        .await
                        .map_err(DbError::from)?;
                if own >= max as i64 {
                    return Ok(CreateRow::HostLimit);
                }
            }
            let q = InsertBuilder::new(&GUILD_EVENT_INFO)
                .values([
                    ("guildId", json!(g)),
                    ("title", json!(d.title)),
                    ("description", json!(d.description)),
                    ("location", json!(d.location)),
                    ("hostId", json!(d.host_id)),
                    ("channelId", json!(ch)),
                    ("roleId", json!(d.role_id)),
                    ("capacity", json!(d.capacity)),
                    ("reminderMinutes", json!(d.reminder_minutes)),
                ])?
                .typed("startsAt", SqlValue::timestamp(d.starts_at))?
                .typed("endsAt", d.ends_at.map(SqlValue::timestamp).unwrap_or(SqlValue::Timestamp(None)))?
                .build()?;
            let row: GuildEvent = q.fetch_one(&mut *tx).await?;
            Ok::<_, anyhow::Error>(CreateRow::Created(Box::new(row)))
        })
    })
    .await
}

/// `prisma.guildEvent.update({ where: { id }, data })`: fails when the row is gone.
async fn update_event_row(app: &App, event_id: &str, set: Vec<(&str, Value)>) -> anyhow::Result<GuildEvent> {
    let q = UpdateBuilder::new(&GUILD_EVENT_INFO).set_all(set)?.filter("id", json!(event_id))?.build()?;
    Ok(q.fetch_one(app.db()).await?)
}

async fn delete_event_row(app: &App, event_id: &str) -> anyhow::Result<()> {
    bot_db::query::delete_where(&GUILD_EVENT_INFO, [("id", json!(event_id))])?.execute(app.db()).await?;
    Ok(())
}

// ── Discord: small wrappers ───────────────────────────────────────────────────

/// What [`resolve_channel`] found.
#[derive(Debug, Clone)]
pub enum ChannelLookup {
    Found(Box<GuildChannel>),
    /// Deleted (10003).
    Gone,
    /// The bot cannot see it.
    NoAccess,
}

/// `resolveChannel(client, channelId)`: the cached channel, else a REST fetch. Errors other than "gone" and "no
/// access" fail.
pub async fn resolve_channel(app: &App, guild_id: &str, channel_id: &str) -> anyhow::Result<ChannelLookup> {
    let (Some(g), Some(c)) = (gid(guild_id), cid(channel_id)) else { return Ok(ChannelLookup::Gone) };
    if let Some(ch) = bot_core::interaction_helpers::cached_channel(app, g, c) {
        return Ok(ChannelLookup::Found(Box::new(ch)));
    }
    let http = app.http().ok_or_else(|| anyhow::anyhow!("Discord is not connected"))?;
    match http.get_channel(c).await {
        Ok(serenity::all::Channel::Guild(gc)) => {
            bot_core::discord::cache_created_channel(app, &gc);
            Ok(ChannelLookup::Found(Box::new(gc)))
        }
        Ok(_) => Ok(ChannelLookup::NoAccess),
        Err(e) => match bot_core::discord::error_code(&e) {
            Some(UNKNOWN_CHANNEL) => Ok(ChannelLookup::Gone),
            Some(code) if NO_ACCESS_CODES.contains(&code) => Ok(ChannelLookup::NoAccess),
            _ => Err(e.into()),
        },
    }
}

/// `guild.members.fetch(id)`: the cached member, else REST.
#[allow(clippy::result_large_err)] // serenity::Error, as serenity returns it
pub async fn fetch_member(app: &App, guild_id: GuildId, user_id: UserId) -> Result<Member, serenity::Error> {
    if let Some(m) = app.discord().and_then(|d| d.cache.guild(guild_id).and_then(|g| g.members.get(&user_id).cloned())) {
        return Ok(m);
    }
    let http = app.http().ok_or(serenity::Error::Other("Discord is not connected"))?;
    http.get_member(guild_id, user_id).await
}

async fn send_in(app: &App, channel_id: ChannelId, payload: &Payload) -> anyhow::Result<serenity::all::Message> {
    let http = app.http().ok_or_else(|| anyhow::anyhow!("Discord is not connected"))?;
    Ok(channel_id.send_message(http.as_ref(), payload.to_message()).await?)
}

/// `tellChat(channel, content)`: a message in the event chat. Best effort.
async fn tell_chat(app: &App, channel_id: Option<ChannelId>, content: &str) {
    let Some(c) = channel_id else { return };
    if let Err(e) = send_in(app, c, &Payload::text(content).no_mentions()).await {
        bot_util::log_warn!(format!("events: could not post in the event chat {c}"), log_err(&e));
    }
}

// ── Discord scheduled event (one-way) ─────────────────────────────────────────

/// The options of an external Discord event (`discordEventOptions(event)`), as Discord's JSON body.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscordEventOptions {
    pub name: String,
    pub description: Option<String>,
    pub scheduled_start_time: DateTime<Utc>,
    pub scheduled_end_time: DateTime<Utc>,
    /// `GuildScheduledEventPrivacyLevel.GuildOnly`.
    pub privacy_level: u8,
    /// `GuildScheduledEventEntityType.External`.
    pub entity_type: u8,
    pub location: String,
}

/// `discordEventOptions(event)`: what Discord needs for an external event (a location and an end).
pub fn discord_event_options(event: &GuildEvent) -> DiscordEventOptions {
    let description = clamp_text(event.description.as_deref().unwrap_or(""), EVENT::DESCRIPTION_MAX);
    DiscordEventOptions {
        name: clamp_text(&event.title, EVENT::TITLE_MAX),
        description: (!description.is_empty()).then_some(description),
        scheduled_start_time: event.starts_at,
        scheduled_end_time: shown_end(event),
        privacy_level: 2,
        entity_type: 3,
        location: clamp_text(event.location.as_deref().unwrap_or(""), EVENT::LOCATION_MAX),
    }
}

fn discord_iso(d: &DateTime<Utc>) -> String {
    iso(d)
}

/// Creates the Discord event when the setting is on, the event has a location and the bot may
/// (`createDiscordEvent`). `None` when the setting is off; `Some(Ok(id))` or `Some(Err(note))`.
pub async fn create_discord_event(app: &App, guild_id: GuildId, event: &GuildEvent, settings: Option<&GuildSettings>) -> Option<Result<String, String>> {
    if !settings.map(|s| s.event_create_discord_event).unwrap_or(true) {
        return None;
    }
    if event.location.as_deref().is_none_or(str::is_empty) {
        return Some(Err("No Discord event was created: it needs a location.".into()));
    }
    if !missing_bot_permissions(app, PermTarget::Guild(guild_id), &["ManageEvents"]).is_empty() {
        return Some(Err("No Discord event was created: I need the **Manage Events** permission.".into()));
    }
    let o = discord_event_options(event);
    let mut body = json!({
        "name": o.name,
        "privacy_level": o.privacy_level,
        "scheduled_start_time": discord_iso(&o.scheduled_start_time),
        "scheduled_end_time": discord_iso(&o.scheduled_end_time),
        "entity_type": o.entity_type,
        "entity_metadata": { "location": o.location },
    });
    if let Some(d) = &o.description {
        body["description"] = json!(d);
    }
    let Some(http) = app.http() else { return Some(Err("I could not create the Discord event (Discord is not connected).".into())) };
    match http.create_scheduled_event(guild_id, &body, Some("Event created with /event")).await {
        Ok(created) => Some(Ok(created.id.to_string())),
        Err(e) => {
            bot_util::log_warn!(format!("events: could not create the Discord event in {guild_id}"), bot_core::discord::error_message(&e));
            Some(Err(format!("I could not create the Discord event ({}).", serenity_reason(&e))))
        }
    }
}

/// `updateDiscordEvent(guild, event, { startChanged, endChanged })`: edits the linked Discord event. A note when it
/// failed.
async fn update_discord_event(app: &App, guild_id: GuildId, event: &GuildEvent, start_changed: bool, end_changed: bool) -> Option<String> {
    let de = event.discord_event_id.as_deref().filter(|d| !d.is_empty())?;
    let o = discord_event_options(event);
    let mut body = json!({
        "name": o.name,
        "description": o.description,
        "entity_metadata": { "location": o.location },
    });
    // A started event cannot get a new start (Discord rejects the whole edit): the start goes only when it changed.
    if start_changed {
        body["scheduled_start_time"] = json!(discord_iso(&o.scheduled_start_time));
    }
    if start_changed || end_changed {
        body["scheduled_end_time"] = json!(discord_iso(&o.scheduled_end_time));
    }
    let result: anyhow::Result<()> = async {
        let http = app.http().ok_or_else(|| anyhow::anyhow!("Discord is not connected"))?;
        let id = ScheduledEventId::new(parse_id(de).ok_or_else(|| anyhow::anyhow!("bad Discord event id"))?);
        http.edit_scheduled_event(guild_id, id, &body, Some("Event edited with /event")).await?;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => None,
        Err(e) if code_of(&e) == Some(UNKNOWN_SCHEDULED_EVENT) => {
            let _ = update_events(app, &[("id", json!(event.id))], vec![("discordEventId", Value::Null)]).await;
            Some("The linked Discord event no longer exists.".into())
        }
        Err(e) => {
            bot_util::log_warn!(format!("events: could not update the Discord event of {}", event.id), log_err(&e));
            Some(format!("I could not update the Discord event ({}).", reason_of(&e)))
        }
    }
}

/// `discordEventCancelled(client, event)`: whether the linked Discord event was deleted or cancelled in Discord. A
/// failed lookup counts as still there.
async fn discord_event_cancelled(app: &App, event: &GuildEvent) -> bool {
    let Some(de) = event.discord_event_id.as_deref().and_then(parse_id) else { return false };
    let Some(g) = guild_for(app, &event.guild_id) else { return false };
    let Some(http) = app.http() else { return false };
    match http.get_scheduled_event(g, ScheduledEventId::new(de), false).await {
        Ok(found) => found.status == ScheduledEventStatus::Canceled,
        Err(e) => bot_core::discord::error_code(&e) == Some(UNKNOWN_SCHEDULED_EVENT),
    }
}

/// `deleteDiscordEvent(guild, event)`: false when it failed (a missing one counts as gone).
async fn delete_discord_event(app: &App, guild_id: GuildId, event: &GuildEvent) -> bool {
    let Some(de) = event.discord_event_id.as_deref().filter(|d| !d.is_empty()) else { return true };
    let res: anyhow::Result<()> = async {
        let http = app.http().ok_or_else(|| anyhow::anyhow!("Discord is not connected"))?;
        let id = ScheduledEventId::new(parse_id(de).ok_or_else(|| anyhow::anyhow!("bad Discord event id"))?);
        http.delete_scheduled_event(guild_id, id).await?;
        Ok(())
    }
    .await;
    if let Err(e) = res
        && code_of(&e) != Some(UNKNOWN_SCHEDULED_EVENT)
    {
        bot_util::log_warn!(format!("events: could not delete the Discord event of {}", event.id), log_err(&e));
        return false;
    }
    let _ = update_events(app, &[("id", json!(event.id))], vec![("discordEventId", Value::Null)]).await;
    true
}

// ── Event channels ────────────────────────────────────────────────────────────

/// `resolveCategory(guild, settings)`: the configured category, else one called Events, else a new one, which the
/// settings remember. `Err(note)` when the bot may not make one; a refused create fails.
async fn resolve_category(app: &App, guild_id: GuildId, settings: Option<&GuildSettings>) -> anyhow::Result<Result<ChannelId, String>> {
    let configured = settings.and_then(|s| s.event_category_id.clone());
    let (a, c) = (app.clone(), configured);
    state(app)
        .category_mutex
        .run(&guild_id.to_string(), move || async move {
            let app = a;
            let found = bot_core::interaction_helpers::with_cached_guild(&app, guild_id, |g| {
                let is_cat = |ch: &GuildChannel| ch.kind == ChannelType::Category;
                if let Some(id) = c.as_deref().and_then(cid)
                    && g.channels.get(&id).is_some_and(is_cat)
                {
                    return (Some(id), true);
                }
                let named = g
                    .channels
                    .values()
                    .filter(|ch| is_cat(ch) && ch.name.to_lowercase() == EVENT::CATEGORY_NAME.to_lowercase())
                    .min_by_key(|ch| (ch.position, ch.id))
                    .map(|ch| ch.id);
                (named, false)
            });
            let (named, configured_ok) = found.unwrap_or((None, false));
            if configured_ok {
                return Ok(Ok(named.unwrap_or_default()));
            }
            if named.is_none() && !missing_bot_permissions(&app, PermTarget::Guild(guild_id), &["ManageChannels"]).is_empty() {
                return Ok(Err("No event channels were made: I need the **Manage Channels** permission.".to_string()));
            }
            let category = match named {
                Some(c) => c,
                None => {
                    let http = app.http().ok_or_else(|| anyhow::anyhow!("Discord is not connected"))?;
                    let ch = http
                        .create_channel(guild_id, &json!({ "name": EVENT::CATEGORY_NAME, "type": 4 }), Some("Category for event channels"))
                        .await?;
                    bot_core::discord::cache_created_channel(&app, &ch);
                    ch.id
                }
            };
            // Unsaved is harmless: the next event finds the category by name.
            if let Err(e) =
                app.settings().update_settings(&guild_id.to_string(), SettingsPatch::new().set("eventCategoryId", category.to_string())).await
            {
                bot_util::log_warn!("events: could not save the category".to_string(), e.to_string());
            }
            Ok::<_, anyhow::Error>(Ok(category))
        })
        .await
}

/// The event chat and its access role (`createEventChat`'s success).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventChat {
    pub channel_id: ChannelId,
    pub role_id: RoleId,
}

fn overwrites_json(guild_id: GuildId, role_id: RoleId, bot: UserId, allow: &[&str]) -> Value {
    Value::Array(event_overwrites(&guild_id.to_string(), &role_id.to_string(), &bot.to_string(), allow).iter().map(EventOverwrite::to_json).collect())
}

/// `createEventChat(guild, event, settings)`: the event's access role and private text chat when the setting is on.
/// `None` when off; `Some(Err(note))` when nothing was made.
pub async fn create_event_chat(app: &App, guild_id: GuildId, event: &GuildEvent, settings: Option<&GuildSettings>) -> Option<Result<EventChat, String>> {
    if !settings.is_some_and(|s| s.event_channels_enabled) {
        return None;
    }
    let mut role: Option<RoleId> = None;
    let res: anyhow::Result<Result<EventChat, String>> = async {
        let category = match resolve_category(app, guild_id, settings).await? {
            Ok(c) => c,
            Err(note) => return Ok(Err(note)),
        };
        let mut needs: Vec<&str> = TEXT_ALLOW.to_vec();
        needs.extend(["ManageChannels", "ManageRoles"]);
        if !missing_bot_permissions(app, PermTarget::Channel(guild_id, category), &needs).is_empty() {
            return Ok(Err(format!(
                "No event channels were made: I need **Manage Channels**, **Manage Roles**, **Send Messages** and **Read Message History** in <#{category}>."
            )));
        }
        let http = app.http().ok_or_else(|| anyhow::anyhow!("Discord is not connected"))?;
        let bot = bot_id(app).ok_or_else(|| anyhow::anyhow!("Discord is not connected"))?;
        let created = http
            .create_role(
                guild_id,
                &json!({ "name": clamp_text(&format!("Event: {}", event.title), 100), "permissions": "0", "mentionable": false, "hoist": false }),
                Some(&format!("Access to the event chat of {}", clamp_text(&event.title, 60))),
            )
            .await?;
        bot_core::discord::cache_created_role(app, guild_id, &created);
        role = Some(created.id);
        let channel = http
            .create_channel(
                guild_id,
                &json!({
                    "name": text_channel_name(&event.title),
                    "type": 0,
                    "parent_id": category.to_string(),
                    "permission_overwrites": overwrites_json(guild_id, created.id, bot, &TEXT_ALLOW),
                }),
                Some(&format!("Event chat: {}", clamp_text(&event.title, 60))),
            )
            .await?;
        bot_core::discord::cache_created_channel(app, &channel);
        Ok(Ok(EventChat { channel_id: channel.id, role_id: created.id }))
    }
    .await;
    match res {
        Ok(r) => Some(r),
        Err(e) => {
            if let (Some(r), Some(http)) = (role, app.http()) {
                let _ = http.delete_role(guild_id, r, Some("Event chat could not be made")).await;
            }
            bot_util::log_warn!(format!("events: could not create the event chat in {guild_id}"), log_err(&e));
            Some(Err(format!("I could not create the event chat ({}).", reason_of(&e))))
        }
    }
}

/// `removeEventChat(chat)`: takes back the chat when the event itself could not be made.
async fn remove_event_chat(app: &App, guild_id: GuildId, chat: Option<&EventChat>) {
    let (Some(chat), Some(http)) = (chat, app.http()) else { return };
    let _ = http.delete_channel(chat.channel_id, Some("Event could not be created")).await;
    let _ = http.delete_role(guild_id, chat.role_id, Some("Event could not be created")).await;
}

/// `syncAccessRole({ member, event, status })`: shows the event's channels to a member going or maybe (and the host,
/// whatever they answered) and hides them from one who cannot go. A note when the role could not be changed.
pub async fn sync_access_role(app: &App, guild_id: GuildId, member: Option<&Member>, event: &GuildEvent, status: RsvpStatus) -> Option<String> {
    let role = event.access_role_id.as_deref().and_then(rid)?;
    let member = member?;
    let want = status != RsvpStatus::Declined || member.user.id.to_string() == event.host_id;
    if want == member.roles.contains(&role) {
        return None;
    }
    let res: anyhow::Result<()> = async {
        let http = app.http().ok_or_else(|| anyhow::anyhow!("Discord is not connected"))?;
        if want {
            http.add_member_role(guild_id, member.user.id, role, Some(&format!("Event RSVP: {}", clamp_text(&event.title, 60)))).await?;
        } else {
            http.remove_member_role(guild_id, member.user.id, role, Some(&format!("Event RSVP: not going to {}", clamp_text(&event.title, 60))))
                .await?;
        }
        Ok(())
    }
    .await;
    match res {
        Ok(()) => None,
        Err(e) => {
            bot_util::log_warn!(format!("events: could not change the event chat access of {} for {}", member.user.id, event.id), log_err(&e));
            Some(format!("I could not change your access to the event chat ({}).", reason_of(&e)))
        }
    }
}

/// `renameEventChannels(guild, event)`: renames the event's channels after a title edit (bounded).
async fn rename_event_channels(app: &App, guild_id: GuildId, event: &GuildEvent) {
    let names = [
        (event.text_channel_id.clone(), text_channel_name(&event.title)),
        (event.voice_channel_id.clone(), clamp_text(&event.title, EVENT::TITLE_MAX)),
    ];
    let mut tasks = Vec::new();
    for (id, name) in names {
        let Some(c) = id.as_deref().and_then(cid) else { continue };
        if bot_core::interaction_helpers::cached_channel(app, guild_id, c).is_none() {
            continue;
        }
        let Some(http) = app.http() else { continue };
        tasks.push(async move {
            let fut = async move { http.edit_channel(c, &json!({ "name": name }), Some("Event renamed")).await.map(|_| ()) };
            if let Err(e) = bot_util::async_util::with_timeout(fut, EVENT::RENAME_MS, "event channel rename").await {
                bot_util::log_warn!(format!("events: could not rename {c}"), e.to_string());
            }
        });
    }
    futures_join_all(tasks).await;
}

async fn futures_join_all<F: std::future::Future<Output = ()>>(tasks: Vec<F>) {
    let mut set = Vec::new();
    for t in tasks {
        set.push(t);
    }
    // Two renames at most: run them together.
    let mut it = set.into_iter();
    match (it.next(), it.next()) {
        (Some(a), Some(b)) => {
            tokio::join!(a, b);
        }
        (Some(a), None) => a.await,
        _ => {}
    }
}

/// `queueCleanup({ eventId, guildId }, delayMs)`.
async fn queue_cleanup(app: &App, event_id: &str, guild_id: &str, delay_ms: i64) -> anyhow::Result<()> {
    let run_at = at(app.clock().now_ms() + delay_ms);
    app.scheduler()
        .schedule_job(
            ScheduleJob::new(guild_id, bot_config::JobType::EventCleanup, run_at).payload(json!({ "eventId": event_id })).dedupe_key(cleanup_job_key(event_id)),
        )
        .await?;
    Ok(())
}

/// `queueEnd({ eventId, guildId }, delayMs)`.
async fn queue_end(app: &App, event_id: &str, guild_id: &str, delay_ms: i64) -> anyhow::Result<()> {
    let run_at = at(app.clock().now_ms() + delay_ms);
    app.scheduler()
        .schedule_job(ScheduleJob::new(guild_id, bot_config::JobType::EventEnd, run_at).payload(json!({ "eventId": event_id })).dedupe_key(end_job_key(event_id)))
        .await?;
    Ok(())
}

/// `callHumans(client, event)`: people in the event's voice channel: 0 when it has none, or the channel is gone or out of
/// the bot's sight (so the event can still close; `event.cleanup` is the careful one). Never fails.
async fn call_humans(app: &App, event: &GuildEvent) -> usize {
    let Some(voice) = event.voice_channel_id.as_deref().filter(|v| !v.is_empty()) else { return 0 };
    call_humans_in(app, &event.guild_id, voice).await
}

async fn call_humans_in(app: &App, guild_id: &str, voice: &str) -> usize {
    match resolve_channel(app, guild_id, voice).await {
        Ok(ChannelLookup::Found(c)) => feat_tempvoice::temp_voice_service::count_humans(app, c.guild_id, c.id),
        _ => 0,
    }
}

/// `deleteEventChannel(client, event, column)`: whether the channel is gone. One that is already gone counts as
/// deleted; one the bot may not delete stays linked; anything else fails, so the job retries.
async fn delete_event_channel(app: &App, event: &GuildEvent, column: &str) -> anyhow::Result<bool> {
    let id = if column == "voiceChannelId" { event.voice_channel_id.clone() } else { event.text_channel_id.clone() };
    let Some(id) = id.filter(|i| !i.is_empty()) else { return Ok(true) };
    let res: anyhow::Result<bool> = async {
        match resolve_channel(app, &event.guild_id, &id).await? {
            ChannelLookup::NoAccess => {
                warn_throttled(app, &format!("cleanup:{id}"), &format!("event {}: cannot reach {id} to delete it", event.id), None);
                return Ok(false);
            }
            ChannelLookup::Gone => {}
            ChannelLookup::Found(ch) => {
                mark_bot_deletion(app, &[id.as_str()], Some("events:cleanup"), None);
                let http = app.http().ok_or_else(|| anyhow::anyhow!("Discord is not connected"))?;
                http.delete_channel(ch.id, Some(&format!("Event is over: {}", clamp_text(&event.title, 60)))).await?;
            }
        }
        Ok(true)
    }
    .await;
    match res {
        Ok(false) => return Ok(false),
        Ok(true) => {}
        Err(e) => match code_of(&e) {
            Some(c) if NO_ACCESS_CODES.contains(&c) => {
                warn_throttled(app, &format!("cleanup:{id}"), &format!("event {}: cannot delete {id}", event.id), Some(log_err(&e)));
                return Ok(false);
            }
            Some(UNKNOWN_CHANNEL) => {}
            _ => return Err(e),
        },
    }
    update_events(app, &[("id", json!(event.id))], vec![(column, Value::Null)]).await?;
    Ok(true)
}

/// `deleteAccessRole(client, event)`: deletes the access role, which takes it from everyone.
async fn delete_access_role(app: &App, event: &GuildEvent) -> anyhow::Result<()> {
    let Some(g) = guild_for(app, &event.guild_id) else { return Ok(()) };
    let Some(role) = event.access_role_id.as_deref().filter(|r| !r.is_empty()) else { return Ok(()) };
    let res: anyhow::Result<()> = async {
        let http = app.http().ok_or_else(|| anyhow::anyhow!("Discord is not connected"))?;
        let r = rid(role).ok_or_else(|| anyhow::anyhow!("bad role id"))?;
        http.delete_role(g, r, Some(&format!("Event is over: {}", clamp_text(&event.title, 60)))).await?;
        Ok(())
    }
    .await;
    if let Err(e) = res {
        match code_of(&e) {
            Some(c) if NO_ACCESS_CODES.contains(&c) => {
                warn_throttled(app, &format!("cleanup:{role}"), &format!("event {}: cannot delete its access role", event.id), Some(log_err(&e)));
                return Ok(());
            }
            Some(UNKNOWN_ROLE) => {}
            _ => return Err(e),
        }
    }
    update_events(app, &[("id", json!(event.id))], vec![("accessRoleId", Value::Null)]).await?;
    Ok(())
}

/// `deleteEventMessages(client, event)`: deletes the announcement and its reminders and forgets them.
async fn delete_event_messages(app: &App, event: &GuildEvent) -> anyhow::Result<()> {
    let ids: Vec<String> = event.message_id.iter().chain(event.reminder_message_ids.iter()).filter(|i| !i.is_empty()).cloned().collect();
    if ids.is_empty() {
        return Ok(());
    }
    let channel = match resolve_channel(app, &event.guild_id, &event.channel_id).await? {
        ChannelLookup::NoAccess => {
            warn_throttled(
                app,
                &format!("cleanup:{}", event.id),
                &format!("event {}: cannot reach <#{}> to delete its post", event.id, event.channel_id),
                None,
            );
            return Ok(());
        }
        ChannelLookup::Gone => None,
        ChannelLookup::Found(c) => Some(c.id),
    };
    if let Some(c) = channel {
        let http = app.http().ok_or_else(|| anyhow::anyhow!("Discord is not connected"))?;
        for id in &ids {
            mark_bot_deletion(app, &[id.as_str()], Some("events:cleanup"), None);
            let Some(m) = parse_id(id).map(MessageId::new) else { continue };
            if let Err(e) = http.delete_message(c, m, None).await {
                match bot_core::discord::error_code(&e) {
                    Some(code) if NO_ACCESS_CODES.contains(&code) => {
                        warn_throttled(
                            app,
                            &format!("cleanup:{}", event.id),
                            &format!("event {}: cannot delete its post or reminders", event.id),
                            Some(bot_core::discord::error_message(&e).into()),
                        );
                        return Ok(());
                    }
                    Some(UNKNOWN_MESSAGE) | Some(UNKNOWN_CHANNEL) => {}
                    _ => return Err(e.into()),
                }
            }
        }
    }
    update_events(app, &[("id", json!(event.id))], vec![("messageId", Value::Null), ("reminderMessageIds", json!([]))]).await?;
    Ok(())
}

// ── Jobs ──────────────────────────────────────────────────────────────────────

/// `scheduleEventJobs(event, now)`: `event.end`, `event.remind` while it is still ahead (cancelled otherwise), and
/// `event.voice` for an event with a chat and no voice channel yet. Returns `failed`: a job could not be written.
pub async fn schedule_event_jobs(app: &App, event: &GuildEvent, now_ms: i64) -> bool {
    let mut failed = false;
    let job = |t: bot_config::JobType, run_at: DateTime<Utc>, key: String| {
        ScheduleJob::new(event.guild_id.clone(), t, run_at).payload(json!({ "eventId": event.id })).dedupe_key(key)
    };
    if let Err(e) = app.scheduler().schedule_job(job(bot_config::JobType::EventEnd, event_end(event), end_job_key(&event.id))).await {
        failed = true;
        bot_util::log_error!(format!("events: could not schedule event.end for {}", event.id), log_err(&e));
    }
    match remind_at(event) {
        Some(r) if event.reminder_sent_at.is_none() && ms(&r) > now_ms => {
            if let Err(e) = app.scheduler().schedule_job(job(bot_config::JobType::EventRemind, r, remind_job_key(&event.id))).await {
                failed = true;
                bot_util::log_error!(format!("events: could not schedule event.remind for {}", event.id), log_err(&e));
            }
        }
        _ => {
            app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(remind_job_key(&event.id)), ..Default::default() }).await;
        }
    }
    if event.text_channel_id.as_deref().is_some_and(|t| !t.is_empty()) && event.voice_channel_id.as_deref().is_none_or(str::is_empty) {
        let settings = app.settings().get_settings_safe(&event.guild_id).await;
        let run_at = voice_at(&event.starts_at, voice_lead(settings.as_deref()), now_ms);
        if let Err(e) = app.scheduler().schedule_job(job(bot_config::JobType::EventVoice, run_at, voice_job_key(&event.id))).await {
            failed = true;
            bot_util::log_error!(format!("events: could not schedule event.voice for {}", event.id), log_err(&e));
        }
    }
    failed
}

// ── Posting and rendering ─────────────────────────────────────────────────────

/// `memberNames(client, guild, ids)`: display names for the card (the member's name in the server, else the account
/// name). Best effort, bounded by 4 s.
pub async fn member_names(app: &App, guild_id: Option<GuildId>, ids: &[String]) -> HashMap<String, String> {
    let Some(g) = guild_id else { return HashMap::new() };
    let names: Arc<Mutex<HashMap<String, String>>> = Arc::default();
    let (a, n) = (app.clone(), names.clone());
    let mut wanted: Vec<String> = Vec::new();
    for id in ids {
        if !id.is_empty() && !wanted.contains(id) {
            wanted.push(id.clone());
        }
    }
    let lookup = async move {
        let cached: HashMap<String, String> = bot_core::interaction_helpers::with_cached_guild(&a, g, |guild| {
            wanted.iter().filter_map(|id| uid(id).and_then(|u| guild.members.get(&u)).map(|m| (id.clone(), m.display_name().to_string()))).collect()
        })
        .unwrap_or_default();
        let missing: Vec<UserId> = wanted.iter().filter(|id| !cached.contains_key(*id)).filter_map(|id| uid(id)).collect();
        let found = if missing.is_empty() {
            HashMap::new()
        } else {
            bot_core::guild_members::fetch_members_by_ids(&a, g, &missing, EVENT::NAME_LOOKUP_MS).await.unwrap_or_default()
        };
        let mut left = Vec::new();
        for id in &wanted {
            if let Some(name) = cached.get(id) {
                lock(&n).insert(id.clone(), name.clone());
            } else if let Some(m) = uid(id).and_then(|u| found.get(&u)) {
                lock(&n).insert(id.clone(), m.display_name().to_string());
            } else {
                left.push(id.clone());
            }
        }
        let (a2, n2) = (a.clone(), n.clone());
        bot_util::async_util::map_limit(left, 3, move |id, _| {
            let (a, n) = (a2.clone(), n2.clone());
            async move {
                let Some(u) = uid(&id) else { return Ok::<_, String>(()) };
                let cached = a.discord().and_then(|d| d.cache.user(u).map(|x| x.display_name().to_string()));
                let name = match cached {
                    Some(n) => Some(n),
                    None => match a.http() {
                        Some(h) => h.get_user(u).await.ok().map(|x| x.display_name().to_string()),
                        None => None,
                    },
                };
                if let Some(name) = name {
                    lock(&n).insert(id, name);
                }
                Ok(())
            }
        })
        .await;
        Ok::<_, std::convert::Infallible>(())
    };
    let _ = bot_util::async_util::with_timeout(lookup, EVENT::NAME_LOOKUP_MS, "event names").await;
    let mut guard = lock(&names);
    std::mem::take(&mut *guard)
}

/// What a render did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderOutcome {
    Ok,
    Skip,
    /// The message was deleted (its id is cleared).
    Gone,
}

/// `editPost(client, event, { settings, tally })`: edits the announcement to the event's current state.
async fn edit_post(app: &App, event: &GuildEvent, settings: Option<Option<bot_core::Settings>>, tally: Option<Tally>) -> anyhow::Result<RenderOutcome> {
    let Some(message_id) = event.message_id.clone().filter(|m| !m.is_empty()) else { return Ok(RenderOutcome::Skip) };
    let Some(g) = guild_for(app, &event.guild_id) else { return Ok(RenderOutcome::Skip) };
    let res: anyhow::Result<RenderOutcome> = async {
        let channel = match resolve_channel(app, &event.guild_id, &event.channel_id).await? {
            // The channelDelete hook cancels its events.
            ChannelLookup::Gone => return Ok(RenderOutcome::Skip),
            ChannelLookup::NoAccess => {
                warn_throttled(app, &format!("render:{}", event.id), &format!("event {}: cannot reach its channel to update the post", event.id), None);
                return Ok(RenderOutcome::Skip);
            }
            ChannelLookup::Found(c) => c.id,
        };
        let t = match tally {
            Some(t) => t,
            None => get_tally(app, &event.id).await?,
        };
        let s = match settings {
            Some(s) => s,
            None => app.settings().get_settings_safe(&event.guild_id).await,
        };
        let names = member_names(app, Some(g), &t.going_ids).await;
        let payload = build_event_payload(event, &t, s.as_deref(), app.clock().now_ms(), Some(&names));
        let http = app.http().ok_or_else(|| anyhow::anyhow!("Discord is not connected"))?;
        let m = MessageId::new(parse_id(&message_id).ok_or_else(|| anyhow::anyhow!("bad message id"))?);
        channel.edit_message(http.as_ref(), m, payload.to_message_edit()).await?;
        Ok(RenderOutcome::Ok)
    }
    .await;
    match res {
        Ok(r) => Ok(r),
        Err(e) => match code_of(&e) {
            Some(UNKNOWN_MESSAGE) => {
                let _ = update_events(app, &[("id", json!(event.id)), ("messageId", json!(message_id))], vec![("messageId", Value::Null)]).await;
                Ok(RenderOutcome::Gone)
            }
            Some(UNKNOWN_CHANNEL) => Ok(RenderOutcome::Skip),
            Some(c) if NO_ACCESS_CODES.contains(&c) => {
                warn_throttled(app, &format!("render:{}", event.id), &format!("event {}: cannot edit its post (Discord {c})", event.id), None);
                Ok(RenderOutcome::Skip)
            }
            _ => Err(e),
        },
    }
}

/// `renderEventMessage(client, eventId)`: the live render (open events only; the final edit belongs to close).
pub async fn render_event_message(app: &App, event_id: &str) -> anyhow::Result<RenderOutcome> {
    let Some(event) = find_event(app, event_id).await? else { return Ok(RenderOutcome::Skip) };
    if event.message_id.as_deref().is_none_or(str::is_empty) || !is_open(Some(&event)) {
        return Ok(RenderOutcome::Skip);
    }
    edit_post(app, &event, None, None).await
}

fn render_with(app: &App, event_id: &str) -> impl std::future::Future<Output = anyhow::Result<RenderOutcome>> + Send + 'static + use<> {
    let custom = lock(&state(app).render_impl).clone();
    let (a, id) = (app.clone(), event_id.to_string());
    state(app).render_mutex.run(event_id, move || async move {
        match custom {
            Some(f) => f(a, id).await,
            None => render_event_message(&a, &id).await,
        }
    })
}

/// `requestRender(client, eventId)`: at most one edit per event per 5 s. The first request after a quiet period
/// renders at once; requests inside the window collapse into one trailing render that reads the database when it
/// runs.
pub fn request_render(app: &App, event_id: &str) {
    let st = state(app);
    let now = app.clock().now_ms();
    let mut map = lock(&st.render);
    if map.get(event_id).is_some_and(|s| s.timer.as_ref().is_some_and(|t| !t.is_finished())) {
        return;
    }
    if map.len() > RENDER_SWEEP_AT {
        map.retain(|id, s| id == event_id || s.timer.as_ref().is_some_and(|t| !t.is_finished()) || now - s.last_at <= RENDER_IDLE_MS);
    }
    let slot = map.entry(event_id.to_string()).or_default();
    let wait = (slot.last_at + EVENT::RENDER_INTERVAL_MS - now).max(0);
    let (a, id) = (app.clone(), event_id.to_string());
    slot.timer = Some(tokio::spawn(async move {
        if wait > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(wait as u64)).await;
        }
        {
            let st = state(&a);
            let mut map = lock(&st.render);
            if let Some(s) = map.get_mut(&id) {
                s.last_at = a.clock().now_ms();
                s.timer = None;
            }
        }
        if let Err(e) = render_with(&a, &id).await {
            bot_util::log_error!(format!("event render failed ({id})"), log_err(&e));
        }
    }));
}

/// `cancelRender(eventId)`: drops a queued render (the event closed).
pub fn cancel_render(app: &App, event_id: &str) {
    let st = state(app);
    if let Some(s) = lock(&st.render).remove(event_id)
        && let Some(t) = s.timer
    {
        t.abort();
    }
}

/// `renderNow(client, eventId)`: renders now (an edit), under the same mutex as the live and final renders.
pub async fn render_now(app: &App, event_id: &str) -> anyhow::Result<RenderOutcome> {
    cancel_render(app, event_id);
    render_with(app, event_id).await
}

// ── Creating ──────────────────────────────────────────────────────────────────

/// `reconcileOverdue(client, guildId)`: ends the open events whose end passed more than 10 minutes ago.
pub async fn reconcile_overdue(app: &App, guild_id: &str) -> anyhow::Result<usize> {
    let rows: Vec<(String, DateTime<Utc>, Option<DateTime<Utc>>, Option<String>)> =
        sqlx::query_as::<_, (String, chrono::NaiveDateTime, Option<chrono::NaiveDateTime>, Option<String>)>(sqlx::AssertSqlSafe(format!(
            r#"SELECT "id", "startsAt", "endsAt", "voiceChannelId" FROM "GuildEvent" WHERE "guildId" = $1 AND {OPEN_SQL}"#
        )))
        .bind(guild_id)
        .fetch_all(app.db())
        .await
        .map_err(DbError::from)?
        .into_iter()
        .map(|(id, s, e, v)| (id, s.and_utc(), e.map(|x| x.and_utc()), v))
        .collect();
    let mut ended = 0;
    for (id, s, e, voice) in rows {
        if ms(&end_of(&s, e.as_ref())) + EVENT::OVERDUE_GRACE_MS > app.clock().now_ms() {
            continue;
        }
        // An event with people still in its voice channel stays open (the rows are open: OPEN_SQL).
        if let Some(voice) = voice.filter(|v| !v.is_empty()) {
            let humans = call_humans_in(app, guild_id, &voice).await;
            if humans > 0 {
                continue;
            }
        }
        if let CloseOutcome::Closed { event, .. } = close_event(app, &id, GuildEventStatus::Ended, CloseReason::Expired, Some(guild_id)).await? {
            ended += 1;
            strip_after_close(app, event.as_deref(), CloseReason::Expired).await;
        }
    }
    Ok(ended)
}

/// Why `createEvent` failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateFailure {
    Limit,
    HostLimit,
    Send,
    Save,
}

/// `createEvent({ client, guild, channel, settings, data, hostMax })`: creates the event, posts the announcement,
/// stores its message id, makes the Discord event and schedules the jobs. A failed post or save removes the row again;
/// a failed Discord event or job is reported in `notes`.
pub async fn create_event(
    app: &App,
    guild_id: GuildId,
    channel_id: ChannelId,
    settings: Option<&GuildSettings>,
    data: &NewEvent,
    host_max: Option<usize>,
) -> anyhow::Result<Result<(GuildEvent, Vec<String>), CreateFailure>> {
    let g = guild_id.to_string();
    let ch = channel_id.to_string();
    let mut created = create_event_row(app, &g, data, &ch, host_max).await?;
    if !matches!(created, CreateRow::Created(_)) {
        // Overdue events still count against both caps until their end job runs: end them, then try once more.
        // As in the JS, the sweep and the second try share one try/catch: a failure there is only warned about, and
        // the answer is the first try's LIMIT / HOST_LIMIT.
        let retried: anyhow::Result<Option<CreateRow>> = async {
            if reconcile_overdue(app, &g).await? > 0 { Ok(Some(create_event_row(app, &g, data, &ch, host_max).await?)) } else { Ok(None) }
        }
        .await;
        match retried {
            Ok(Some(r)) => created = r,
            Ok(None) => {}
            Err(e) => bot_util::log_warn!("events: reconciling overdue events failed".to_string(), log_err(&e)),
        }
    }
    let mut event = match created {
        CreateRow::Created(e) => *e,
        CreateRow::Limit => return Ok(Err(CreateFailure::Limit)),
        CreateRow::HostLimit => return Ok(Err(CreateFailure::HostLimit)),
    };
    let mut notes: Vec<String> = Vec::new();

    // Before the post, so nobody can answer an event whose access role does not exist yet.
    let mut chat: Option<EventChat> = None;
    match create_event_chat(app, guild_id, &event, settings).await {
        Some(Err(note)) => notes.push(note),
        Some(Ok(c)) => {
            match update_event_row(app, &event.id, vec![("textChannelId", json!(c.channel_id.to_string())), ("accessRoleId", json!(c.role_id.to_string()))]).await
            {
                Ok(e) => {
                    event = e;
                    chat = Some(c);
                }
                Err(e) => {
                    bot_util::log_error!("events: could not store the event chat".to_string(), log_err(&e));
                    remove_event_chat(app, guild_id, Some(&c)).await;
                    notes.push("The event chat could not be linked, so it was removed.".into());
                }
            }
        }
        None => {}
    }

    let now = app.clock().now_ms();
    let msg = match send_in(app, channel_id, &build_event_payload(&event, &empty_tally(), settings, now, None)).await {
        Ok(m) => m,
        Err(e) => {
            bot_util::log_warn!(format!("events: could not post the announcement in {guild_id}"), log_err(&e));
            remove_event_chat(app, guild_id, chat.as_ref()).await;
            let _ = delete_event_row(app, &event.id).await;
            return Ok(Err(CreateFailure::Send));
        }
    };
    match update_event_row(app, &event.id, vec![("messageId", json!(msg.id.to_string()))]).await {
        Ok(e) => event = e,
        Err(e) => {
            bot_util::log_error!("events: could not store the announcement id".to_string(), log_err(&e));
            if let Some(http) = app.http() {
                let _ = http.delete_message(channel_id, msg.id, None).await;
            }
            remove_event_chat(app, guild_id, chat.as_ref()).await;
            let _ = delete_event_row(app, &event.id).await;
            return Ok(Err(CreateFailure::Save));
        }
    }

    if let Some(c) = &chat {
        let host = match uid(&event.host_id) {
            Some(u) => fetch_member(app, guild_id, u).await.ok(),
            None => None,
        };
        if let Some(note) = sync_access_role(app, guild_id, host.as_ref(), &event, RsvpStatus::Going).await {
            notes.push(note);
        }
        let link = jump_url(&event);
        tell_chat(
            app,
            Some(c.channel_id),
            &format!(
                "💬 This is the chat for **{}**.{}\nOnly members who answered Going or Maybe can see it. A voice channel opens {}. When the event is over and the voice channel is empty, both are deleted, along with the event post and its reminders.",
                clamp_text(&event.title, 100),
                if link.is_empty() { String::new() } else { format!(" {link}") },
                voice_lead_text(voice_lead(settings))
            ),
        )
        .await;
    }

    match create_discord_event(app, guild_id, &event, settings).await {
        Some(Ok(id)) => match update_event_row(app, &event.id, vec![("discordEventId", json!(id))]).await {
            Ok(e) => event = e,
            Err(e) => {
                bot_util::log_error!("events: could not store the Discord event id".to_string(), log_err(&e));
                // Unlinked first: should the id have been stored after all, the delete would otherwise cancel this event.
                let _ = update_events(app, &[("id", json!(event.id))], vec![("discordEventId", Value::Null)]).await;
                if let (Some(http), Some(de)) = (app.http(), parse_id(&id)) {
                    let _ = http.delete_scheduled_event(guild_id, ScheduledEventId::new(de)).await;
                }
                notes.push("The Discord event could not be linked, so it was removed.".into());
            }
        },
        Some(Err(note)) => notes.push(note),
        None => {}
    }

    if schedule_event_jobs(app, &event, app.clock().now_ms()).await {
        notes.push("I could not schedule the reminder or the end of the event: edit the event to try again.".into());
    }
    Ok(Ok((event, notes)))
}

// ── RSVP ──────────────────────────────────────────────────────────────────────

/// `applyRsvp({ eventId, guildId, userId, status })`: records one RSVP with the event row locked FOR UPDATE, so the
/// capacity check and a concurrent cancel or end are serialised.
pub async fn apply_rsvp(app: &App, event_id: &str, guild_id: &str, user_id: &str, status: RsvpStatus) -> anyhow::Result<RsvpOut> {
    let (e, g, u) = (event_id.to_string(), guild_id.to_string(), user_id.to_string());
    bot_db::transaction(app.db(), bot_db::TxOptions::TEN_SECONDS, move |tx| {
        Box::pin(async move {
            let row: Option<(String, String, String, Option<i32>)> =
                sqlx::query_as(EVENT_LOCK_SQL).bind(&e).fetch_optional(&mut *tx).await.map_err(DbError::from)?;
            let Some((_, row_guild, row_status, capacity)) = row else { return Ok(RsvpOut::of(RsvpResult::Gone)) };
            if row_guild != g {
                return Ok(RsvpOut::of(RsvpResult::Gone));
            }
            if row_status != "SCHEDULED" && row_status != "ACTIVE" {
                return Ok(RsvpOut::of(RsvpResult::Closed));
            }
            let existing: Option<(String,)> = sqlx::query_as(r#"SELECT "status"::text FROM "EventRsvp" WHERE "eventId" = $1 AND "userId" = $2"#)
                .bind(&e)
                .bind(&u)
                .fetch_optional(&mut *tx)
                .await
                .map_err(DbError::from)?;
            let previous = existing.and_then(|(s,)| RsvpStatus::ALL.iter().copied().find(|x| x.as_str() == s));
            if previous == Some(status) {
                return Ok(RsvpOut { result: RsvpResult::Same, previous, note: None });
            }
            if status == RsvpStatus::Going
                && let Some(cap) = capacity
            {
                // The caller is not going yet.
                let (going,): (i64,) = sqlx::query_as(r#"SELECT COUNT(*)::bigint FROM "EventRsvp" WHERE "eventId" = $1 AND "status" = 'GOING'"#)
                    .bind(&e)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(DbError::from)?;
                if going >= i64::from(cap) {
                    return Ok(RsvpOut { result: RsvpResult::Full, previous, note: None });
                }
            }
            InsertBuilder::new(&EVENT_RSVP_INFO)
                .values([("eventId", json!(e)), ("userId", json!(u)), ("status", json!(status.as_str()))])?
                .on_conflict_update(&["eventId", "userId"], [("status", json!(status.as_str()))])?
                .without_returning()
                .build()?
                .execute(&mut *tx)
                .await?;
            Ok::<_, anyhow::Error>(RsvpOut { result: RsvpResult::Set, previous, note: None })
        })
    })
    .await
}

/// `markRoleGranted(eventId, userId, roleGranted)`.
pub async fn mark_role_granted(app: &App, event_id: &str, user_id: &str, granted: bool) -> anyhow::Result<u64> {
    Ok(UpdateBuilder::new(&EVENT_RSVP_INFO)
        .set("roleGranted", json!(granted))?
        .filter("eventId", json!(event_id))?
        .filter("userId", json!(user_id))?
        .without_returning()
        .build()?
        .execute(app.db())
        .await?)
}

/// `syncRsvpRole({ guild, member, event, status })`: gives the event role to a member who is now going, takes it back
/// from one who stopped going (only from a member the bot gave it to). A note when the role could not be changed.
pub async fn sync_rsvp_role(app: &App, guild_id: GuildId, member: Option<&Member>, event: &GuildEvent, status: RsvpStatus) -> Option<String> {
    let role = event.role_id.as_deref().and_then(rid)?;
    let member = member?;
    let has = member.roles.contains(&role);
    let user = member.user.id;
    let res: anyhow::Result<Option<String>> = async {
        let http = app.http().ok_or_else(|| anyhow::anyhow!("Discord is not connected"))?;
        if status == RsvpStatus::Going {
            if has {
                return Ok(None);
            }
            let check = check_assignable_role_cached(app, guild_id, role, None, false);
            if !check.ok {
                return Ok(Some(format!("I could not give you the event role: {}", check.reason.unwrap_or_default())));
            }
            http.add_member_role(guild_id, user, role, Some(&format!("Event RSVP: going to {}", clamp_text(&event.title, 60)))).await?;
            // Flag first, then look at the status: an end or cancel that commits after the flag is written finds this
            // row in its strip, one that committed before is seen by the read below.
            let mut open: Option<bool> = None;
            let flagged: anyhow::Result<bool> = async {
                mark_role_granted(app, &event.id, &user.to_string(), true).await?;
                let fresh = find_event(app, &event.id).await?;
                Ok(is_open(fresh.as_ref()))
            }
            .await;
            match flagged {
                Ok(o) => open = Some(o),
                Err(e) => bot_util::log_warn!(format!("events: could not record the role of {user} for {}", event.id), log_err(&e)),
            }
            if open == Some(true) {
                return Ok(None);
            }
            http.remove_member_role(guild_id, user, role, Some(&format!("Event RSVP: {} is over", clamp_text(&event.title, 60)))).await?;
            let _ = mark_role_granted(app, &event.id, &user.to_string(), false).await;
            return Ok(Some(if open == Some(false) {
                "This event just ended, so the event role was not kept.".into()
            } else {
                "I could not save your event role, so it was taken back. Please try again.".into()
            }));
        }
        let granted: Option<(bool,)> = sqlx::query_as(r#"SELECT "roleGranted" FROM "EventRsvp" WHERE "eventId" = $1 AND "userId" = $2"#)
            .bind(&event.id)
            .bind(user.to_string())
            .fetch_optional(app.db())
            .await
            .map_err(DbError::from)?;
        if granted.is_some_and(|(g,)| g) {
            if has {
                http.remove_member_role(guild_id, user, role, Some(&format!("Event RSVP: no longer going to {}", clamp_text(&event.title, 60)))).await?;
            }
            mark_role_granted(app, &event.id, &user.to_string(), false).await?;
        }
        Ok(None)
    }
    .await;
    match res {
        Ok(n) => n,
        Err(e) => {
            bot_util::log_warn!(format!("events: could not change the role of {user} for {}", event.id), log_err(&e));
            Some(format!("I could not change your event role ({}).", reason_of(&e)))
        }
    }
}

/// Every run of 17 to 20 digits in `text` (the ids a mention can name).
fn snowflakes_in(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for run in text.split(|c: char| !c.is_ascii_digit()) {
        if (17..=20).contains(&run.len()) && !out.iter().any(|x| x == run) {
            out.push(run.to_string());
        }
    }
    out
}

/// A snapshot of the guild's names for [`plain_discord_text`] (`interaction.guild`'s caches), limited to the ids
/// `text` mentions.
pub fn guild_name_maps(app: &App, guild_id: Option<GuildId>, text: &str) -> Option<NameMaps> {
    let g = guild_id?;
    let d = app.discord()?;
    let ids = snowflakes_in(text);
    let mut maps = NameMaps::default();
    {
        let guild = d.cache.guild(g)?;
        let names = GuildNames { guild: &guild, cache: &d.cache };
        for id in &ids {
            if let Some(n) = names.user_name(id) {
                maps.members.insert(id.clone(), n);
            }
            if let Some(n) = names.role_name(id) {
                maps.roles.insert(id.clone(), n);
            }
            if let Some(n) = names.channel_name(id) {
                maps.channels.insert(id.clone(), n);
            }
        }
    }
    Some(maps)
}

fn names_for(app: &App, guild_id: Option<GuildId>, event: &GuildEvent) -> Box<dyn NameSource + Send> {
    match guild_name_maps(app, guild_id, event.description.as_deref().unwrap_or("")) {
        Some(m) => Box::new(m),
        None => Box::new(NoNames),
    }
}

/// `event:{eventId}:calendar`: the calendar links and file, privately.
async fn handle_calendar_button(ctx: &ComponentCtx, event_id: &str) {
    if !safe_defer(ctx, true).await {
        return;
    }
    let reply = |content: String| safe_reply(ctx, Payload::text(content).no_mentions());
    let guild = ctx.guild_id();
    let res: anyhow::Result<()> = async {
        let event = find_guild_event(&ctx.app, event_id, &guild.map(|g| g.to_string()).unwrap_or_default()).await?;
        if !is_open(event.as_ref()) {
            let out = RsvpOut::of(if event.is_some() { RsvpResult::Closed } else { RsvpResult::Gone });
            reply(rsvp_reply_text(&out, event.as_ref(), RsvpStatus::Going)).await;
            return Ok(());
        }
        let Some(event) = event else { return Ok(()) };
        let names = names_for(&ctx.app, guild, &event);
        safe_reply(ctx, build_calendar_payload(&event, names.as_ref(), ctx.app.clock().now_ms(), None)).await;
        Ok(())
    }
    .await;
    if let Err(e) = res {
        bot_util::log_error!("event calendar failed".to_string(), log_err(&e));
        reply("Something went wrong making the calendar entry. Please try again.".into()).await;
    }
}

/// `handleRsvpButton(interaction, client)`: `event:{eventId}:going|maybe|declined|calendar`. Defers first; a first
/// Going or Maybe also gets the calendar links and file.
pub async fn handle_rsvp_button(ctx: ComponentCtx) -> bot_core::BotResult {
    let (_, params) = ctx.parsed();
    let event_id = params.first().cloned().unwrap_or_default();
    let action = params.get(1).cloned().unwrap_or_default();
    if !event_id.is_empty() && action == "calendar" {
        handle_calendar_button(&ctx, &event_id).await;
        return Ok(());
    }
    let status = status_by_action(&action);
    let Some(status) = status.filter(|_| !event_id.is_empty()) else {
        safe_reply(&ctx, Payload::text("This button is no longer active.").ephemeral(true)).await;
        return Ok(());
    };
    if !safe_defer(&ctx, true).await {
        return Ok(());
    }
    let reply = |content: String| safe_reply(&ctx, Payload::text(content).no_mentions());
    let res: anyhow::Result<()> = async {
        let app = ctx.app.clone();
        let guild = ctx.guild_id();
        let g = guild.map(|g| g.to_string()).unwrap_or_default();
        let user = ctx.user().id;
        let event = find_guild_event(&app, &event_id, &g).await?;
        let Some(event) = event.filter(|e| is_open(Some(e))) else {
            let gone = find_guild_event(&app, &event_id, &g).await?;
            let out = RsvpOut::of(if gone.is_some() { RsvpResult::Closed } else { RsvpResult::Gone });
            reply(rsvp_reply_text(&out, gone.as_ref(), status)).await;
            return Ok(());
        };
        let member = ctx.member().cloned();
        let (a, e2, g2, ev) = (app.clone(), event_id.clone(), g.clone(), event.clone());
        let out: anyhow::Result<RsvpOut> = state(&app)
            .user_mutex
            .run(&format!("{event_id}:{user}"), move || async move {
                let res = apply_rsvp(&a, &e2, &g2, &user.to_string(), status).await?;
                if res.result != RsvpResult::Set && res.result != RsvpResult::Same {
                    return Ok(res);
                }
                let member = match (member, guild) {
                    (Some(m), _) => Some(m),
                    (None, Some(g)) => fetch_member(&a, g, user).await.ok(),
                    _ => None,
                };
                let gid = guild.unwrap_or(GuildId::new(1));
                // `same` on Going also repairs a role that an earlier click failed to give.
                let note = if res.result == RsvpResult::Set || status == RsvpStatus::Going {
                    sync_rsvp_role(&a, gid, member.as_ref(), &ev, status).await
                } else {
                    None
                };
                let access = sync_access_role(&a, gid, member.as_ref(), &ev, status).await;
                let joined: Vec<String> = [note, access].into_iter().flatten().filter(|n| !n.is_empty()).collect();
                Ok(RsvpOut { note: (!joined.is_empty()).then(|| joined.join("\n⚠️ ")), ..res })
            })
            .await;
        let out = out?;
        let text = rsvp_reply_text(&out, Some(&event), status);
        // Only on joining: moving between Going and Maybe would send the file again every time.
        let joined = out.result == RsvpResult::Set
            && status != RsvpStatus::Declined
            && out.previous != Some(RsvpStatus::Going)
            && out.previous != Some(RsvpStatus::Maybe);
        let mut sent = false;
        if joined {
            let names = names_for(&app, guild, &event);
            let payload = build_calendar_payload(&event, names.as_ref(), app.clock().now_ms(), Some(format!("{text}\n{CALENDAR_HINT}")));
            // A refused calendar reply must not cost the member the confirmation of an RSVP that was saved.
            sent = safe_reply(&ctx, payload).await.is_some();
        }
        if !sent {
            reply(text).await;
        }
        if out.result == RsvpResult::Set {
            request_render(&app, &event_id);
        }
        Ok(())
    }
    .await;
    if let Err(e) = res {
        bot_util::log_error!("event rsvp failed".to_string(), log_err(&e));
        reply("Something went wrong saving your RSVP. Please try again.".into()).await;
    }
    Ok(())
}

// ── Closing: end, cancel ──────────────────────────────────────────────────────

/// Why an event closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    Timer,
    Manual,
    Expired,
    ChannelDeleted,
    DiscordCancelled,
}

/// `closeEvent`'s outcome.
#[derive(Debug, Clone, PartialEq)]
pub enum CloseOutcome {
    Closed { event: Option<Box<GuildEvent>>, discord_failed: bool },
    NotFound,
    /// Already closed (the row as it is).
    AlreadyClosed(Box<GuildEvent>),
}

/// `closeEvent(client, eventId, { status, reason, guildId })`: flips an open event to ENDED or CANCELLED (one winner),
/// cancels its jobs, edits the announcement with disabled buttons and, for a cancel, deletes the Discord event. Fails
/// only when the status UPDATE fails.
pub async fn close_event(app: &App, event_id: &str, status: GuildEventStatus, reason: CloseReason, guild_id: Option<&str>) -> anyhow::Result<CloseOutcome> {
    let mut filters: Vec<(&str, Value)> = vec![("id", json!(event_id))];
    if let Some(g) = guild_id.filter(|g| !g.is_empty()) {
        filters.push(("guildId", json!(g)));
    }
    let count = update_open_events(app, &filters, vec![("status", SqlValueOrJson::Json(json!(status.as_str())))]).await?;
    if count == 0 {
        let row = match guild_id.filter(|g| !g.is_empty()) {
            Some(g) => find_guild_event(app, event_id, g).await?,
            None => find_event(app, event_id).await?,
        };
        return Ok(match row {
            Some(r) => CloseOutcome::AlreadyClosed(Box::new(r)),
            None => CloseOutcome::NotFound,
        });
    }

    let mut event: Option<GuildEvent> = None;
    let mut discord_failed = false;
    let res: anyhow::Result<()> = async {
        cancel_render(app, event_id);
        app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(remind_job_key(event_id)), ..Default::default() }).await;
        if reason != CloseReason::Timer {
            app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(end_job_key(event_id)), ..Default::default() }).await;
        }
        event = find_event(app, event_id).await?;
        let Some(ev) = event.clone() else { return Ok(()) };
        if [&ev.text_channel_id, &ev.voice_channel_id, &ev.access_role_id].iter().any(|x| x.as_deref().is_some_and(|s| !s.is_empty())) {
            app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(voice_job_key(event_id)), ..Default::default() }).await;
            if let Err(e) = queue_cleanup(app, event_id, &ev.guild_id, 0).await {
                bot_util::log_error!(format!("event {event_id}: could not schedule the cleanup of its channels"), log_err(&e));
            }
        }
        let Some(g) = guild_for(app, &ev.guild_id) else {
            // An unavailable guild is never judged: the post keeps its buttons (they answer "no longer open").
            bot_util::log_warn!(format!("event {event_id} closed, but its guild is unavailable: the post was not updated"));
            return Ok(());
        };
        if status == GuildEventStatus::Cancelled {
            discord_failed = !delete_discord_event(app, g, &ev).await;
        }
        if reason != CloseReason::ChannelDeleted {
            let edited: anyhow::Result<()> = async {
                let tally = get_tally(app, event_id).await?;
                let settings = app.settings().get_settings_safe(&ev.guild_id).await;
                let (a, e2) = (app.clone(), ev.clone());
                state(app).render_mutex.run(event_id, move || async move { edit_post(&a, &e2, Some(settings), Some(tally)).await }).await?;
                Ok(())
            }
            .await;
            if let Err(e) = edited {
                bot_util::log_warn!(format!("event {event_id}: could not edit the final post"), log_err(&e));
            }
        }
        Ok(())
    }
    .await;
    if let Err(e) = res {
        bot_util::log_error!(format!("event {event_id}: finishing the close failed"), log_err(&e));
    }
    Ok(CloseOutcome::Closed { event: event.map(Box::new), discord_failed })
}

/// `stripRoles(client, event, { after, run })`: takes the event role back from up to 100 members the bot gave it to,
/// after the cursor. Returns the cursor for the next batch, `None` when done.
pub async fn strip_roles(app: &App, event: &GuildEvent, after: Option<&str>, run: i64) -> anyhow::Result<Option<String>> {
    let Some(role) = event.role_id.as_deref().and_then(rid) else { return Ok(None) };
    let Some(g) = guild_for(app, &event.guild_id) else { return Ok(None) };
    if !missing_bot_permissions(app, PermTarget::Guild(g), &["ManageRoles"]).is_empty() {
        bot_util::log_warn!(format!("event {}: cannot take the event role back, Manage Roles is missing", event.id));
        return Ok(None);
    }
    // Only members the bot gave the role to. A flag stays set when the removal failed, and for a member who is not in
    // the server: that is Role Persistence's marker that this member's saved copy of the role came from this event.
    let rows: Vec<(String,)> = match after {
        Some(a) => sqlx::query_as(
            r#"SELECT "userId" FROM "EventRsvp" WHERE "eventId" = $1 AND "roleGranted" = true AND "userId" > $2 ORDER BY "userId" ASC LIMIT $3"#,
        )
        .bind(&event.id)
        .bind(a)
        .bind((EVENT::STRIP_BATCH + 1) as i64)
        .fetch_all(app.db())
        .await
        .map_err(DbError::from)?,
        None => sqlx::query_as(r#"SELECT "userId" FROM "EventRsvp" WHERE "eventId" = $1 AND "roleGranted" = true ORDER BY "userId" ASC LIMIT $2"#)
            .bind(&event.id)
            .bind((EVENT::STRIP_BATCH + 1) as i64)
            .fetch_all(app.db())
            .await
            .map_err(DbError::from)?,
    };
    let batch: Vec<String> = rows.iter().take(EVENT::STRIP_BATCH).map(|r| r.0.clone()).collect();
    let (a, ev) = (app.clone(), event.clone());
    bot_util::async_util::map_limit(batch.clone(), EVENT::STRIP_CONCURRENCY, move |user, _| {
        let (app, event) = (a.clone(), ev.clone());
        async move {
            let res: anyhow::Result<()> = async {
                let Some(u) = uid(&user) else { return Ok(()) };
                match fetch_member(&app, g, u).await {
                    Ok(m) => {
                        if m.roles.contains(&role) {
                            let http = app.http().ok_or_else(|| anyhow::anyhow!("Discord is not connected"))?;
                            http.remove_member_role(g, u, role, Some("Event is over")).await?;
                        }
                    }
                    Err(e) => {
                        if bot_core::discord::error_code(&e) == Some(UNKNOWN_MEMBER) {
                            // A member who left holds nothing; the flag stays as the marker.
                            return Ok(());
                        }
                        return Err(e.into());
                    }
                }
                mark_role_granted(&app, &event.id, &user, false).await?;
                Ok(())
            }
            .await;
            if let Err(e) = res {
                warn_throttled(&app, &format!("strip:{}", event.id), &format!("event {}: could not take the role from {user}", event.id), Some(log_err(&e)));
            }
            Ok::<_, std::convert::Infallible>(())
        }
    })
    .await;
    let more = (rows.len() > EVENT::STRIP_BATCH && run + 1 < EVENT::STRIP_RUNS_MAX).then(|| batch.last().cloned()).flatten();
    Ok(more)
}

/// `stripAfterClose(client, event, { reason })`: strips the first batch and, when members remain, schedules the job
/// that continues.
async fn strip_after_close(app: &App, event: Option<&GuildEvent>, reason: CloseReason) {
    let Some(event) = event.filter(|e| e.role_id.as_deref().is_some_and(|r| !r.is_empty())) else { return };
    let res: anyhow::Result<()> = async {
        if let Some(more) = strip_roles(app, event, None, 0).await?
            && reason != CloseReason::Timer
        {
            app.scheduler()
                .schedule_job(
                    ScheduleJob::new(event.guild_id.clone(), bot_config::JobType::EventEnd, at(app.clock().now_ms() + EVENT::STRIP_RESUME_MS))
                        .payload(json!({ "eventId": event.id, "after": more, "run": 1 }))
                        .dedupe_key(end_job_key(&event.id)),
                )
                .await?;
        }
        Ok(())
    }
    .await;
    if let Err(e) = res {
        bot_util::log_warn!(format!("event {}: taking the role back failed", event.id), log_err(&e));
    }
}

/// `cancelEvent(client, eventId, { guildId, reason })`: cancels an open event (status, jobs, post, Discord event, role).
pub async fn cancel_event(app: &App, event_id: &str, guild_id: Option<&str>, reason: CloseReason) -> anyhow::Result<CloseOutcome> {
    let res = close_event(app, event_id, GuildEventStatus::Cancelled, reason, guild_id).await?;
    if let CloseOutcome::Closed { event, .. } = &res {
        strip_after_close(app, event.as_deref(), reason).await;
    }
    Ok(res)
}

// ── Editing ───────────────────────────────────────────────────────────────────

/// Changes to an event (`undefined` = unchanged = `None`; `Some(None)` clears a nullable field).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EventChanges {
    pub title: Option<String>,
    pub description: Option<Option<String>>,
    pub location: Option<Option<String>>,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<Option<DateTime<Utc>>>,
    pub capacity: Option<Option<i32>>,
    pub reminder_minutes: Option<i32>,
}

/// `editEvent({ client, guild, event, changes })`: applies validated changes to an open event, reschedules its jobs,
/// updates the Discord event and the post. `Ok(None)` when the event closed meanwhile.
pub async fn edit_event(app: &App, guild_id: GuildId, event: &GuildEvent, changes: &EventChanges) -> anyhow::Result<Option<(GuildEvent, Vec<String>)>> {
    let start_changed = changes.starts_at.is_some_and(|s| ms(&s) != ms(&event.starts_at));
    let end_changed = changes.ends_at.is_some_and(|e| e.map(|d| ms(&d)) != event.ends_at.map(|d| ms(&d)));
    let reminder_changed = changes.reminder_minutes.is_some_and(|r| r != event.reminder_minutes);
    let title_changed = changes.title.as_ref().is_some_and(|t| *t != event.title);
    let mut set: Vec<(&str, SqlValueOrJson)> = Vec::new();
    if let Some(t) = &changes.title {
        set.push(("title", SqlValueOrJson::Json(json!(t))));
    }
    if let Some(d) = &changes.description {
        set.push(("description", SqlValueOrJson::Json(json!(d))));
    }
    if let Some(l) = &changes.location {
        set.push(("location", SqlValueOrJson::Json(json!(l))));
    }
    if let Some(s) = &changes.starts_at {
        set.push(("startsAt", ts_value(Some(s))));
    }
    if let Some(e) = &changes.ends_at {
        set.push(("endsAt", ts_value(e.as_ref())));
    }
    if let Some(c) = &changes.capacity {
        set.push(("capacity", SqlValueOrJson::Json(json!(c))));
    }
    if let Some(r) = &changes.reminder_minutes {
        set.push(("reminderMinutes", SqlValueOrJson::Json(json!(r))));
    }
    // A moved event reminds again.
    if start_changed || reminder_changed {
        set.push(("reminderSentAt", ts_value(None)));
    }
    let count = update_open_events(app, &[("id", json!(event.id)), ("guildId", json!(event.guild_id))], set).await?;
    if count == 0 {
        return Ok(None);
    }
    let Some(updated) = find_event(app, &event.id).await? else { return Ok(None) };
    let mut notes = Vec::new();
    if (start_changed || end_changed || reminder_changed) && schedule_event_jobs(app, &updated, app.clock().now_ms()).await {
        notes.push("I could not reschedule the reminder or the end of the event.".to_string());
    }
    if let Some(n) = update_discord_event(app, guild_id, &updated, start_changed, end_changed).await {
        notes.push(n);
    }
    if title_changed {
        rename_event_channels(app, guild_id, &updated).await;
    }
    if let Some(Some(cap)) = changes.capacity {
        let tally = get_tally(app, &event.id).await?;
        if tally.going > i64::from(cap) {
            notes.push(format!("{} members are already going: they keep their place.", tally.going));
        }
    }
    if let Err(e) = render_now(app, &event.id).await {
        bot_util::log_warn!(format!("event {}: could not update the post after an edit", event.id), log_err(&e));
        notes.push("I could not update the announcement.".into());
    }
    Ok(Some((updated, notes)))
}

/// `EDIT_FORMS`: the forms of the /event edit panel.
pub const EDIT_FORMS: [&str; 3] = ["details", "when", "limits"];

/// `buildEditPanel(event, { settings })`: what the event is set to now and a button per form
/// (`event-edit:{eventId}:{form}`).
pub fn build_edit_panel(event: &GuildEvent, settings: Option<&GuildSettings>) -> Payload {
    let length = event.ends_at.map(|e| bot_util::parse_duration::format_duration((ms(&e) - ms(&event.starts_at)) as f64));
    let lines = [
        format!(
            "**When:** {} ({}){}",
            tsd(&event.starts_at, "F"),
            tsd(&event.starts_at, "R"),
            length.map(|l| format!(", for {l}")).unwrap_or_default()
        ),
        format!("**Where:** {}", event.location.as_deref().filter(|l| !l.is_empty()).map(|l| clamp_text(l, 100)).unwrap_or_else(|| "not set".into())),
        format!("**Description:** {}", event.description.as_deref().filter(|d| !d.is_empty()).map(|d| clamp_text(d, 300)).unwrap_or_else(|| "none".into())),
        format!("**Capacity:** {}", match event.capacity {
            Some(c) if c != 0 => format!("{c} members"),
            _ => "no limit".into(),
        }),
        format!(
            "**Reminder:** {}",
            if event.reminder_minutes > 0 {
                format!("{} min before{}", event.reminder_minutes, if event.reminder_sent_at.is_some() { " (sent)" } else { "" })
            } else {
                "none".into()
            }
        ),
    ];
    let embed = guild_embed(
        settings,
        Some(&format!("✏️ {}", clamp_text(&event.title, 200))),
        Some(&lines.join("\n")),
        Some("Pick what to change. The post, the reminder and the Discord event follow."),
    );
    let button = |form: &str, label: &str, emoji: &str| {
        CreateButton::new(cid_of("event-edit", &[&event.id, form])).label(label).emoji(ReactionType::Unicode(emoji.into())).style(ButtonStyle::Primary)
    };
    let mut row = vec![button("details", "Details", "📝"), button("when", "Date & time", "🗓️"), button("limits", "Capacity & reminder", "👥")];
    let url = jump_url(event);
    if !url.is_empty() {
        row.push(CreateButton::new_link(url).label("Open post"));
    }
    Payload::new().embeds(vec![embed]).components(vec![CreateActionRow::Buttons(row)])
}

fn modal_field(id: &str, label: &str, style: serenity::all::InputTextStyle, value: &str, required: bool, max: u16, placeholder: Option<&str>) -> CreateActionRow {
    let mut input = CreateInputText::new(style, label, id).required(required).max_length(max);
    if !value.is_empty() {
        input = input.value(clamp_text(value, max as usize));
    }
    if let Some(p) = placeholder.filter(|p| !p.is_empty()) {
        input = input.placeholder(p);
    }
    CreateActionRow::InputText(input)
}

/// `buildEditModal(event, form, zone)`: one form of the edit panel, prefilled (`event-modal:{eventId}:{form}`).
pub fn build_edit_modal(event: &GuildEvent, form: &str, zone: &str) -> CreateModal {
    use serenity::all::InputTextStyle::{Paragraph, Short};
    let id = cid_of("event-modal", &[&event.id, form]);
    if form == "details" {
        return CreateModal::new(id, "Edit details").components(vec![
            modal_field("event-title", "Title", Short, &event.title, true, EVENT::TITLE_MAX as u16, None),
            modal_field("event-description", "Description", Paragraph, event.description.as_deref().unwrap_or(""), false, EVENT::DESCRIPTION_MAX as u16, None),
            modal_field("event-location", "Location", Short, event.location.as_deref().unwrap_or(""), false, EVENT::LOCATION_MAX as u16, None),
        ]);
    }
    if form == "when" {
        let (day, time) = format_start_fields(&event.starts_at, zone);
        let end = event.ends_at.map(|e| ms(&e) - ms(&event.starts_at)).unwrap_or(0);
        let duration = if end > 0 { bot_util::parse_duration::format_duration(end as f64) } else { String::new() };
        return CreateModal::new(id, "Edit date and time").components(vec![
            modal_field("event-day", "Day", Short, &day, true, 40, Some("fri, tomorrow, nov 20 or 2026-11-20")),
            modal_field("event-time", "Time", Short, &time, true, 40, Some("7pm or 18:30")),
            modal_field("event-zone", "Time zone (empty = yours)", Short, zone, false, 64, Some("Europe/London, or CST")),
            modal_field("event-duration", "Duration (empty = none)", Short, &duration, false, 40, Some("2h")),
        ]);
    }
    let cap = match event.capacity {
        Some(c) if c != 0 => c.to_string(),
        _ => String::new(),
    };
    CreateModal::new(id, "Edit capacity and reminder").components(vec![
        modal_field("event-capacity", "Most members who can go (empty = no limit)", Short, &cap, false, 5, Some(&format!("1 to {}", EVENT::CAPACITY_MAX))),
        modal_field(
            "event-reminder",
            "Minutes before the start to ping (0 = none)",
            Short,
            &event.reminder_minutes.to_string(),
            true,
            4,
            Some(&format!("0 to {}", EVENT::REMINDER_MAX)),
        ),
    ])
}

// ── Reading the fields (create options and the edit forms) ────────────────────

/// A refused field: an error embed's title and text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    pub title: String,
    pub text: String,
}

fn fail<T>(title: &str, text: impl Into<String>) -> Result<T, FieldError> {
    Err(FieldError { title: title.into(), text: text.into() })
}

/// `shown(raw)`.
fn shown(raw: &str) -> String {
    let cleaned: String = raw.chars().filter(|c| *c != '`' && *c != '\\').collect();
    let s = clamp_text(bot_util::js::trim(&cleaned), 64);
    if s.is_empty() { "(empty)".into() } else { s }
}

pub const CREATE_EXAMPLES: &str = "Try `fri 7pm`, `tomorrow 18:30`, `oct 30 7:30pm`, `2026-10-30 19:00`, `in 3d` or `7pm CST`.";
pub const FORM_EXAMPLES: &str = "Type a day like `fri`, `tomorrow`, `nov 20` or `2026-11-20` and a time like `7pm` or `18:30`.";

/// The text fields of a new or edited event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventFields {
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start: String,
    pub duration: Option<String>,
}

/// The event being edited, as the parsers need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentEvent {
    pub starts_at: DateTime<Utc>,
    pub discord_event_id: Option<String>,
}

impl CurrentEvent {
    pub fn of(e: &GuildEvent) -> Self {
        Self { starts_at: e.starts_at, discord_event_id: e.discord_event_id.clone() }
    }
}

/// `parseEventDetails`' values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Details {
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
}

/// `parseEventWhen`'s values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct When {
    pub starts_at: DateTime<Utc>,
    pub ends_at: Option<DateTime<Utc>>,
    pub zone: String,
    pub zone_from_input: bool,
}

/// `parseEventFields(fields, ctx)`: parseEventDetails, then parseEventWhen.
pub fn parse_event_fields(fields: &EventFields, resolved: &ResolvedTimezone, now_ms: i64) -> Result<(Details, When), FieldError> {
    let d = parse_event_details(fields, None)?;
    let w = parse_event_when(&fields.start, fields.duration.as_deref(), resolved, None, now_ms, None, CREATE_EXAMPLES)?;
    Ok((d, w))
}

/// `parseEventDetails(fields, { current })`: title, description and location. A linked Discord event keeps its
/// location.
pub fn parse_event_details(fields: &EventFields, current: Option<&CurrentEvent>) -> Result<Details, FieldError> {
    let len = bot_util::js::utf16_len;
    let title = bot_util::js::trim(&fields.title).to_string();
    if title.is_empty() {
        return fail("Missing Title", "Give the event a title.");
    }
    if len(&title) > EVENT::TITLE_MAX {
        return fail("Title Too Long", format!("The title can have at most {} characters.", EVENT::TITLE_MAX));
    }
    let description = Some(bot_util::js::trim(fields.description.as_deref().unwrap_or("")).to_string()).filter(|s| !s.is_empty());
    if description.as_deref().is_some_and(|d| len(d) > EVENT::DESCRIPTION_MAX) {
        return fail("Description Too Long", format!("The description can have at most {} characters.", EVENT::DESCRIPTION_MAX));
    }
    let location = Some(bot_util::js::trim(fields.location.as_deref().unwrap_or("")).to_string()).filter(|s| !s.is_empty());
    if location.as_deref().is_some_and(|l| len(l) > EVENT::LOCATION_MAX) {
        return fail("Location Too Long", format!("The location can have at most {} characters.", EVENT::LOCATION_MAX));
    }
    if location.is_none() && current.is_some_and(|c| c.discord_event_id.as_deref().is_some_and(|d| !d.is_empty())) {
        return fail("Location Needed", "This event is linked to a Discord event, which needs a location.");
    }
    Ok(Details { title, description, location })
}

/// `parseEventWhen(fields, { resolved, zone, now, current, examples })`: start and duration. The start is read in
/// `zone` (the caller's by default) unless the text names one; an edited event's own start stays valid when it
/// already passed.
pub fn parse_event_when(
    start: &str,
    duration: Option<&str>,
    resolved: &ResolvedTimezone,
    zone: Option<&str>,
    now_ms: i64,
    current: Option<&CurrentEvent>,
    examples: &str,
) -> Result<When, FieldError> {
    let zone = zone.unwrap_or(&resolved.zone);
    let Some(parsed) = bot_util::time::parse_when_ms(start, zone, now_ms as f64) else {
        let label = bot_util::timezones::zone_label_ms(zone, now_ms as f64);
        let read_in = if zone == resolved.zone {
            format!("Times are read in **{label}**, {}.", bot_util::timezones::zone_source_text(resolved.source.as_str(), resolved.role_id.as_deref(), false))
        } else {
            format!("Times are read in **{label}**.")
        };
        return fail("Could Not Read That Time", [format!("I couldn't read `{}` as a time.", shown(start)), examples.to_string(), read_in].join("\n"));
    };
    // Whole minutes only: an untouched edit form must not look changed.
    let typed = (parsed.date_ms / 60_000.0).floor() as i64 * 60_000;
    let unchanged = current.is_some_and(|c| typed == ms(&c.starts_at).div_euclid(60_000) * 60_000);
    let starts_at = match current {
        Some(c) if unchanged => c.starts_at,
        _ => at(typed),
    };
    let problem = if unchanged { None } else { start_problem(Some(ms(&starts_at)), now_ms) };
    match problem {
        Some(StartProblem::Past) => return fail("Start In The Past", format!("That is {}, which has already passed.", tsd(&starts_at, "f"))),
        Some(StartProblem::Far) => return fail("Start Too Far Away", "An event can start at most a year from now."),
        None => {}
    }
    let d = match parse_duration_input(duration) {
        Ok(d) => d,
        Err(e) => return fail("Invalid Duration", e.text()),
    };
    let ends_at = d.map(|d| at(ms(&starts_at) + d));
    Ok(When { starts_at, ends_at, zone: parsed.zone, zone_from_input: parsed.zone_from_input })
}

/// `parseEventLimits(fields)`: the Capacity & reminder form. Empty or 0 capacity is no limit; empty reminder is none.
pub fn parse_event_limits(capacity: &str, reminder: &str) -> Result<(Option<i32>, i32), FieldError> {
    let cap = bot_util::js::trim(capacity);
    let rem = bot_util::js::trim(reminder);
    let digits = |s: &str, max_len: usize| (1..=max_len).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit());
    if !cap.is_empty() && (!digits(cap, 5) || cap.parse::<i64>().unwrap_or(i64::MAX) > EVENT::CAPACITY_MAX) {
        return fail("Invalid Capacity", format!("Type how many members can go, from 1 to {}, or leave it empty for no limit.", EVENT::CAPACITY_MAX));
    }
    if !rem.is_empty() && (!digits(rem, 4) || rem.parse::<i64>().unwrap_or(i64::MAX) > EVENT::REMINDER_MAX) {
        return fail("Invalid Reminder", format!("Type the minutes before the start, from 0 to {}. 0 means no reminder.", EVENT::REMINDER_MAX));
    }
    let c = cap.parse::<i32>().unwrap_or(0);
    Ok(((c > 0).then_some(c), rem.parse::<i32>().unwrap_or(0)))
}

/// `isEventStaff(member, settings)`: the Moderator level and up manage every event.
pub fn is_event_staff(app: &App, member: Option<&MemberAccess>, settings: Option<&GuildSettings>) -> bool {
    resolve_permission_level(member, settings, app.config().bot_owner_id.as_deref()) >= PermissionLevel::Moderator
}

/// `canManageEvent(member, settings, event)`: its host always may, staff may for any event.
pub fn can_manage_event(app: &App, member: Option<&MemberAccess>, settings: Option<&GuildSettings>, event: Option<&GuildEvent>) -> bool {
    let (Some(m), Some(e)) = (member, event) else { return false };
    m.user_id == e.host_id || is_event_staff(app, Some(m), settings)
}

pub const NOT_YOURS_TEXT: &str = "Only the host of this event or a moderator can change it.";

fn in_cached_guild(app: &App, guild_id: Option<GuildId>) -> bool {
    guild_id.is_some_and(|g| app.discord().is_some_and(|d| d.cache.guild(g).is_some()))
}

/// `handleEditButton(interaction)`: `event-edit:{eventId}:{form}` opens that form, prefilled from the event as it is
/// now. Nothing is acknowledged before the modal, so the lookups share the ack budget.
pub async fn handle_edit_button(ctx: ComponentCtx) -> bot_core::BotResult {
    let (_, params) = ctx.parsed();
    let event_id = params.first().cloned().unwrap_or_default();
    let form = params.get(1).cloned().unwrap_or_default();
    if !in_cached_guild(&ctx.app, ctx.guild_id()) || event_id.is_empty() || !EDIT_FORMS.contains(&form.as_str()) {
        reply_error(&ctx, "Panel Expired", Some("This panel has expired. Please run `/event edit` again."), true).await;
        return Ok(());
    }
    let budget = ctx.ack_budget_ms();
    let guild = ctx.guild_id();
    let (app, e2, g2, user, member) = (ctx.app.clone(), event_id.clone(), guild.map(|g| g.to_string()).unwrap_or_default(), ctx.user().id, ctx.member().cloned());
    let lookup = async move {
        let (event, zone) = tokio::join!(find_guild_event(&app, &e2, &g2), load_zone(&app, guild, user, member.as_ref(), budget));
        Ok::<_, anyhow::Error>((event?, zone))
    };
    let (event, zone) = match bot_util::async_util::with_timeout(lookup, budget, "event edit form").await {
        Ok(v) => v,
        Err(bot_util::async_util::WithTimeoutError::Timeout(_)) => {
            reply_error(&ctx, "Temporarily Unavailable", Some("That took too long. Please try again."), true).await;
            return Ok(());
        }
        Err(e) => return Err(anyhow::anyhow!(e.to_string())),
    };
    let Some(event) = event else {
        reply_error(&ctx, "Event Not Found", Some("This event no longer exists."), true).await;
        return Ok(());
    };
    if !can_manage_event(&ctx.app, ctx.member_access().as_ref(), zone.settings.as_deref(), Some(&event)) {
        reply_error(&ctx, "Not Your Event", Some(NOT_YOURS_TEXT), true).await;
        return Ok(());
    }
    if !is_open(Some(&event)) {
        safe_reply(&ctx, Payload::embed(warn_embed("Event Closed", Some("This event has ended or was cancelled, so it cannot be edited."), None)).ephemeral(true))
            .await;
        return Ok(());
    }
    if let Err(e) = ctx.show_modal(build_edit_modal(&event, &form, &zone.resolved.zone)).await
        && !is_interaction_gone(&e)
    {
        bot_util::log_error!("event edit: showModal failed".to_string(), e.to_string());
    }
    Ok(())
}

/// One submitted form, read.
struct ReadForm {
    changes: EventChanges,
    zone_note: Option<String>,
}

async fn read_edit_form(ctx: &ModalCtx, form: &str, event: &GuildEvent) -> Result<ReadForm, FieldError> {
    let text = |id: &str| ctx.value(id).unwrap_or_default();
    if form == "details" {
        let f = EventFields { title: text("event-title"), description: Some(text("event-description")), location: Some(text("event-location")), ..Default::default() };
        let d = parse_event_details(&f, Some(&CurrentEvent::of(event)))?;
        return Ok(ReadForm {
            changes: EventChanges { title: Some(d.title), description: Some(d.description), location: Some(d.location), ..Default::default() },
            zone_note: None,
        });
    }
    if form == "limits" {
        let (capacity, reminder) = parse_event_limits(&text("event-capacity"), &text("event-reminder"))?;
        return Ok(ReadForm { changes: EventChanges { capacity: Some(capacity), reminder_minutes: Some(reminder), ..Default::default() }, zone_note: None });
    }
    let loaded = load_zone(&ctx.app, ctx.guild_id(), ctx.user().id, ctx.member(), 3000).await;
    let resolved = loaded.resolved;
    let zone_raw = bot_util::js::trim(&text("event-zone")).to_string();
    let mut zone = resolved.zone.clone();
    if !zone_raw.is_empty() {
        match bot_util::timezones::parse_zone_input(&zone_raw) {
            Ok(z) => zone = z.zone,
            Err(e) => {
                let msg = if e.code == bot_util::timezones::ZoneErrorCode::Ambiguous {
                    format!(
                        "`{}` can mean several time zones: {}. Type the one you mean.",
                        shown(&zone_raw),
                        e.options.unwrap_or_default().iter().map(|o| format!("`{o}`")).collect::<Vec<_>>().join(", ")
                    )
                } else {
                    format!(
                        "`{}` is not a time zone I know. Type a name like `Europe/London` or `America/Chicago`, an abbreviation like `CST`, or leave it empty for yours.",
                        shown(&zone_raw)
                    )
                };
                return fail("Unknown Time Zone", msg);
            }
        }
    }
    let start = bot_util::js::trim(&format!("{} {}", bot_util::js::trim(&text("event-day")), bot_util::js::trim(&text("event-time")))).to_string();
    let now = ctx.app.clock().now_ms();
    let w = parse_event_when(&start, Some(&text("event-duration")), &resolved, Some(&zone), now, Some(&CurrentEvent::of(event)), FORM_EXAMPLES)?;
    let zone_note = (zone_raw.is_empty() && !w.zone_from_input).then(|| {
        format!(
            "The start was read in {}, {}.",
            bot_util::timezones::zone_label_ms(&zone, now as f64),
            bot_util::timezones::zone_source_text(resolved.source.as_str(), resolved.role_id.as_deref(), false)
        )
    });
    Ok(ReadForm { changes: EventChanges { starts_at: Some(w.starts_at), ends_at: Some(w.ends_at), ..Default::default() }, zone_note })
}

/// `handleEditModal(interaction, client)`: `event-modal:{eventId}:{details|when|limits}`. Only that form's fields
/// change; the panel is updated in place, with the result on top.
pub async fn handle_edit_modal(ctx: ModalCtx) -> bot_core::BotResult {
    let (_, params) = ctx.parsed();
    let event_id = params.first().cloned().unwrap_or_default();
    let form = params.get(1).cloned().unwrap_or_default();
    if !in_cached_guild(&ctx.app, ctx.guild_id()) || event_id.is_empty() || !EDIT_FORMS.contains(&form.as_str()) {
        reply_error(&ctx, "Form Expired", Some("This form has expired. Please run `/event edit` again."), true).await;
        return Ok(());
    }
    let guild = ctx.guild_id().unwrap_or(GuildId::new(1));
    let g = guild.to_string();
    let settings = ctx.app.settings().peek_or_load(&g, ctx.ack_budget_ms()).await;
    let from_panel = ctx.interaction.message.is_some();
    if from_panel {
        if let Err(e) = ctx.defer_update().await {
            if !is_interaction_gone(&e) {
                bot_util::log_warn!("event edit: deferUpdate failed".to_string(), e.to_string());
            }
            return Ok(());
        }
    } else if !safe_defer(&ctx, true).await {
        return Ok(());
    }
    let note = |embed: CreateEmbed| safe_reply(&ctx, Payload::embed(embed).ephemeral(true).no_mentions());
    let panel = |embed: CreateEmbed, event: Option<GuildEvent>| {
        let s = settings.clone();
        let ctx = &ctx;
        async move {
            if !from_panel {
                note(embed).await;
                return;
            }
            let current = match event.filter(|e| is_open(Some(e))) {
                Some(e) => build_edit_panel(&e, s.as_deref()),
                None => Payload::new().embeds(vec![]).components(vec![]),
            };
            let mut embeds = vec![embed.clone()];
            embeds.extend(current.embeds.unwrap_or_default());
            let edit = EditInteractionResponse::new()
                .embeds(embeds)
                .components(current.components.unwrap_or_default())
                .allowed_mentions(no_mentions());
            if let Err(e) = ctx.edit_reply(edit).await {
                if is_interaction_gone(&e) {
                    return;
                }
                bot_util::log_warn!("event edit: could not update the panel".to_string(), e.to_string());
                note(embed).await;
            }
        }
    };
    let res: anyhow::Result<()> = async {
        let event = find_guild_event(&ctx.app, &event_id, &g).await?;
        let Some(event) = event else {
            panel(error_embed("Event Not Found", Some("This event no longer exists."), None), None).await;
            return Ok(());
        };
        if !can_manage_event(&ctx.app, ctx.member_access().as_ref(), settings.as_deref(), Some(&event)) {
            panel(error_embed("Not Your Event", Some(&format!("{NOT_YOURS_TEXT}\nNothing was saved.")), None), None).await;
            return Ok(());
        }
        if !is_open(Some(&event)) {
            panel(warn_embed("Event Closed", Some("This event has ended or was cancelled, so it cannot be edited."), None), None).await;
            return Ok(());
        }
        let read = match read_edit_form(&ctx, &form, &event).await {
            Ok(r) => r,
            Err(f) => {
                panel(error_embed(&f.title, Some(&format!("{}\nNothing was saved.", f.text)), None), Some(event)).await;
                return Ok(());
            }
        };
        let Some((updated, notes)) = edit_event(&ctx.app, guild, &event, &read.changes).await? else {
            panel(warn_embed("Event Closed", Some("This event ended or was cancelled before the form was saved."), None), None).await;
            return Ok(());
        };
        let mut lines = vec![format!(
            "**{}** starts {} ({}).",
            clamp_text(&updated.title, 100),
            tsd(&updated.starts_at, "F"),
            tsd(&updated.starts_at, "R")
        )];
        lines.extend(read.zone_note);
        for n in notes {
            lines.push(format!("⚠️ {n}"));
        }
        panel(success_embed("Event Updated", Some(&lines.join("\n")), None), Some(updated)).await;
        Ok(())
    }
    .await;
    if let Err(e) = res {
        bot_util::log_error!("event edit failed".to_string(), log_err(&e));
        note(error_embed("Could Not Save", Some("Something went wrong saving the event. Please try again."), None)).await;
    }
    Ok(())
}

// ── Hooks and jobs ────────────────────────────────────────────────────────────

/// messageDelete: the announcement is gone; the event stays (and its reminders still run) without a post.
pub async fn on_message_delete(app: &App, guild_id: &str, message_id: &str) -> anyhow::Result<()> {
    if message_id.is_empty() || guild_id.is_empty() {
        return Ok(());
    }
    update_events(app, &[("guildId", json!(guild_id)), ("messageId", json!(message_id))], vec![("messageId", Value::Null)]).await?;
    Ok(())
}

/// channelDelete: every open event posted in the channel is cancelled; an event's own chat or voice channel is only
/// forgotten.
pub async fn on_channel_delete(app: &App, guild_id: &str, channel_id: &str) -> anyhow::Result<()> {
    if channel_id.is_empty() || guild_id.is_empty() {
        return Ok(());
    }
    lock(&state(app).waiting_voice).remove(channel_id);
    update_events(app, &[("guildId", json!(guild_id)), ("textChannelId", json!(channel_id))], vec![("textChannelId", Value::Null)]).await?;
    update_events(app, &[("guildId", json!(guild_id)), ("voiceChannelId", json!(channel_id))], vec![("voiceChannelId", Value::Null)]).await?;
    let rows: Vec<(String,)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(r#"SELECT "id" FROM "GuildEvent" WHERE "guildId" = $1 AND "channelId" = $2 AND {OPEN_SQL}"#)))
        .bind(guild_id)
        .bind(channel_id)
        .fetch_all(app.db())
        .await
        .map_err(DbError::from)?;
    for (id,) in rows {
        if let Err(e) = cancel_event(app, &id, Some(guild_id), CloseReason::ChannelDeleted).await {
            bot_util::log_error!(format!("event {id}: cancelling after a channel delete failed"), log_err(&e));
        }
    }
    Ok(())
}

/// guildScheduledEventDelete, and guildScheduledEventUpdate to Canceled: the linked Discord event was cancelled in
/// Discord, so the event is cancelled too (only before the start).
pub async fn on_discord_event_cancelled(app: &App, guild_id: &str, discord_event_id: &str) -> anyhow::Result<()> {
    if discord_event_id.is_empty() || guild_id.is_empty() {
        return Ok(());
    }
    let rows: Vec<(String,)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        r#"SELECT "id" FROM "GuildEvent" WHERE "guildId" = $1 AND "discordEventId" = $2 AND {OPEN_SQL} AND "startsAt" > $3"#
    )))
    .bind(guild_id)
    .bind(discord_event_id)
    .bind(app.clock().now().naive_utc())
    .fetch_all(app.db())
    .await
    .map_err(DbError::from)?;
    for (id,) in rows {
        if let Err(e) = cancel_event(app, &id, Some(guild_id), CloseReason::DiscordCancelled).await {
            bot_util::log_error!(format!("event {id}: cancelling after its Discord event was cancelled failed"), log_err(&e));
        }
    }
    Ok(())
}

/// voiceStateUpdate: the last human left the voice channel of an event that is waiting on it, so after the grace
/// `event.end` runs (an event kept open past its end by its call) or `event.cleanup` (a closed one). Each job looks at
/// the call again.
pub async fn on_waiting_voice_left(app: &App, guild_id: GuildId, voice_id: ChannelId) -> anyhow::Result<()> {
    let key = voice_id.to_string();
    let Some(waiting) = lock(&state(app).waiting_voice).get(&key).cloned() else { return Ok(()) };
    if feat_tempvoice::temp_voice_service::count_humans(app, guild_id, voice_id) > 0 {
        return Ok(());
    }
    lock(&state(app).waiting_voice).remove(&key);
    if waiting.2 {
        queue_end(app, &waiting.0, &waiting.1, EVENT::EMPTY_GRACE_MS).await
    } else {
        queue_cleanup(app, &waiting.0, &waiting.1, EVENT::EMPTY_GRACE_MS).await
    }
}

/// roleDelete: the event role or access role is gone, so events stop giving it.
pub async fn on_role_delete(app: &App, guild_id: &str, role_id: &str) -> anyhow::Result<()> {
    if role_id.is_empty() || guild_id.is_empty() {
        return Ok(());
    }
    update_events(app, &[("guildId", json!(guild_id)), ("roleId", json!(role_id))], vec![("roleId", Value::Null)]).await?;
    update_events(app, &[("guildId", json!(guild_id)), ("accessRoleId", json!(role_id))], vec![("accessRoleId", Value::Null)]).await?;
    Ok(())
}

fn job_event_id(job: &Job, kind: &str) -> anyhow::Result<String> {
    match job.payload.get("eventId").and_then(Value::as_str) {
        Some(id) if !id.is_empty() => Ok(id.to_string()),
        _ => Err(PermanentJobErr::new(format!("{kind}: missing eventId")).into()),
    }
}

use bot_core::error::PermanentJobError as PermanentJobErr;

/// `handleRemindJob(job, ctx)`: one message pinging the members going (claimed first, so a retry never pings twice).
pub async fn handle_remind_job(job: Job, ctx: JobCtx) -> bot_core::BotResult<JobOutcome> {
    let app = &ctx.app;
    let event_id = job_event_id(&job, "event.remind")?;
    let Some(event) = find_guild_event(app, &event_id, &job.guild_id).await? else { return Ok(JobOutcome::Done) };
    if !is_open(Some(&event)) || event.reminder_sent_at.is_some() {
        return Ok(JobOutcome::Done);
    }
    // Too late to be a reminder.
    if ms(&event.starts_at) <= app.clock().now_ms() {
        return Ok(JobOutcome::Done);
    }
    let settings = app.settings().get_settings_safe(&job.guild_id).await;
    if let Some(s) = &settings
        && !is_module_enabled(Some(s), Some(bot_config::ModuleKey::Events))
    {
        return Ok(JobOutcome::Done);
    }
    // A Discord event cancelled while the bot was not listening cancels the event instead of pinging.
    if discord_event_cancelled(app, &event).await {
        cancel_event(app, &event_id, Some(&job.guild_id), CloseReason::DiscordCancelled).await?;
        return Ok(JobOutcome::Done);
    }
    let count = update_open_events(
        app,
        &[("id", json!(event_id)), ("reminderSentAt", Value::Null)],
        vec![("reminderSentAt", ts_value(Some(&app.clock().now())))],
    )
    .await?;
    if count == 0 {
        return Ok(JobOutcome::Done);
    }
    let going: Vec<(String,)> =
        sqlx::query_as(r#"SELECT "userId" FROM "EventRsvp" WHERE "eventId" = $1 AND "status" = 'GOING' ORDER BY "updatedAt" ASC LIMIT $2"#)
            .bind(&event_id)
            .bind(EVENT::REMIND_USERS_MAX)
            .fetch_all(app.db())
            .await
            .map_err(DbError::from)?;
    if going.is_empty() {
        return Ok(JobOutcome::Done);
    }
    let found = resolve_channel(app, &event.guild_id, &event.channel_id).await.unwrap_or(ChannelLookup::NoAccess);
    let channel = match found {
        ChannelLookup::Found(c) => Some(c),
        _ => None,
    };
    let g = gid(&event.guild_id);
    let usable = match (&channel, g) {
        (Some(c), Some(g)) => check_channel_usable_for(app, g, c, &["ViewChannel", "SendMessages"]).ok,
        _ => false,
    };
    let (Some(channel), true) = (channel, usable) else {
        bot_util::log_warn!(format!("event {event_id}: cannot post the reminder in <#{}>", event.channel_id));
        return Ok(JobOutcome::Done);
    };
    let ids: Vec<String> = going.into_iter().map(|r| r.0).collect();
    let mut sent_ids: Vec<String> = Vec::new();
    for m in reminder_messages(&event, &ids, app.clock().now_ms()) {
        let users: Vec<UserId> = m.user_ids.iter().filter_map(|u| uid(u)).collect();
        let payload = Payload::text(m.content).allowed_mentions(CreateAllowedMentions::new().users(users));
        match send_in(app, channel.id, &payload).await {
            Ok(sent) => sent_ids.push(sent.id.to_string()),
            Err(e) => {
                bot_util::log_warn!(format!("event {event_id}: could not post the reminder"), log_err(&e));
                break;
            }
        }
    }
    if !sent_ids.is_empty() {
        // Added to the older ones: a moved event reminds again, and `event.cleanup` removes them all.
        let mut all = event.reminder_message_ids.clone();
        all.extend(sent_ids);
        update_events(app, &[("id", json!(event_id))], vec![("reminderMessageIds", json!(all))]).await?;
    }
    Ok(JobOutcome::Done)
}

/// `handleEndJob(job, ctx)`: ends the event, then takes the role back in batches; a payload with `after` continues an
/// earlier batch. An open event whose end is still ahead (moved by an edit) waits for it; one with people in its voice
/// channel stays open, so members can still answer while the event is going on: the last one out queues this job again
/// after EMPTY_GRACE_MS, and until then it looks again every CLEANUP_RECHECK_MS (after a restart the voice hook has
/// nothing to go on).
pub async fn handle_end_job(job: Job, ctx: JobCtx) -> bot_core::BotResult<JobOutcome> {
    let app = &ctx.app;
    let event_id = job_event_id(&job, "event.end")?;
    let after = job.payload.get("after").and_then(Value::as_str).map(str::to_string);
    let run = job.payload.get("run").and_then(|v| v.as_f64()).filter(|f| f.fract() == 0.0 && f.is_finite()).map(|f| f as i64).unwrap_or(0);
    let event = match &after {
        None => {
            if let Some(current) = find_guild_event(app, &event_id, &job.guild_id).await?.filter(|c| is_open(Some(c))) {
                let end = event_end(&current);
                if ms(&end) > app.clock().now_ms() {
                    return Ok(JobOutcome::Reschedule { next_run_at: end, payload: None });
                }
                if call_keeps_open(&current, call_humans(app, &current).await) {
                    let voice = current.voice_channel_id.clone().unwrap_or_default();
                    lock(&state(app).waiting_voice).insert(voice, (event_id.clone(), current.guild_id.clone(), true));
                    return Ok(JobOutcome::Reschedule { next_run_at: at(app.clock().now_ms() + EVENT::CLEANUP_RECHECK_MS), payload: None });
                }
                if let Some(voice) = current.voice_channel_id.as_deref().filter(|v| !v.is_empty()) {
                    lock(&state(app).waiting_voice).remove(voice);
                }
            }
            match close_event(app, &event_id, GuildEventStatus::Ended, CloseReason::Timer, Some(&job.guild_id)).await? {
                CloseOutcome::Closed { event, .. } => event.map(|e| *e),
                CloseOutcome::AlreadyClosed(e) => Some(*e),
                CloseOutcome::NotFound => None,
            }
        }
        Some(_) => find_guild_event(app, &event_id, &job.guild_id).await?,
    };
    let Some(event) = event.filter(|e| !is_open(Some(e))) else { return Ok(JobOutcome::Done) };
    match strip_roles(app, &event, after.as_deref(), run).await? {
        Some(more) => Ok(JobOutcome::Reschedule {
            next_run_at: at(app.clock().now_ms() + EVENT::STRIP_RESUME_MS),
            payload: Some(json!({ "eventId": event_id, "after": more, "run": run + 1 })),
        }),
        None => Ok(JobOutcome::Done),
    }
}

/// `handleVoiceJob(job, ctx)`: opens the voice channel next to the chat, private to the same role.
pub async fn handle_voice_job(job: Job, ctx: JobCtx) -> bot_core::BotResult<JobOutcome> {
    let app = &ctx.app;
    let event_id = job_event_id(&job, "event.voice")?;
    let Some(event) = find_guild_event(app, &event_id, &job.guild_id).await? else { return Ok(JobOutcome::Done) };
    let has = |s: &Option<String>| s.as_deref().is_some_and(|x| !x.is_empty());
    if !is_open(Some(&event)) || !has(&event.text_channel_id) || !has(&event.access_role_id) || has(&event.voice_channel_id) {
        return Ok(JobOutcome::Done);
    }
    // A late job: the end job closes the event.
    if ms(&event_end(&event)) <= app.clock().now_ms() {
        return Ok(JobOutcome::Done);
    }
    let settings = app.settings().get_settings_safe(&job.guild_id).await;
    if let Some(s) = &settings
        && (!is_module_enabled(Some(s), Some(bot_config::ModuleKey::Events)) || !s.event_channels_enabled)
    {
        return Ok(JobOutcome::Done);
    }
    if discord_event_cancelled(app, &event).await {
        cancel_event(app, &event_id, Some(&job.guild_id), CloseReason::DiscordCancelled).await?;
        return Ok(JobOutcome::Done);
    }
    let Some(g) = guild_for(app, &event.guild_id) else { return Ok(JobOutcome::Done) };
    let text_id = event.text_channel_id.clone().unwrap_or_default();
    let chat = resolve_channel(app, &event.guild_id, &text_id).await?;
    let chat_channel = match &chat {
        ChannelLookup::Gone => {
            update_events(app, &[("id", json!(event_id)), ("textChannelId", json!(text_id))], vec![("textChannelId", Value::Null)]).await?;
            return Ok(JobOutcome::Done);
        }
        ChannelLookup::Found(c) => Some(c.id),
        ChannelLookup::NoAccess => None,
    };
    let parent_id = match &chat {
        ChannelLookup::Found(c) => c.parent_id.map(|p| p.to_string()),
        _ => None,
    }
    .or_else(|| settings.as_ref().and_then(|s| s.event_category_id.clone()))
    .filter(|p| !p.is_empty());
    let parent = parent_id.as_deref().and_then(cid).filter(|p| bot_core::interaction_helpers::cached_channel(app, g, *p).is_some());
    let mut needs: Vec<&str> = VOICE_ALLOW.to_vec();
    needs.extend(["ManageChannels", "ManageRoles"]);
    let target = match parent {
        Some(p) => PermTarget::Channel(g, p),
        None => PermTarget::Guild(g),
    };
    if !missing_bot_permissions(app, target, &needs).is_empty() {
        bot_util::log_warn!(format!("event {event_id}: no permission to open its voice channel"));
        tell_chat(
            app,
            chat_channel,
            "⚠️ I could not open the voice channel: I need **Manage Channels**, **Manage Roles**, **Connect** and **Speak** in this category.",
        )
        .await;
        return Ok(JobOutcome::Done);
    }
    let http = app.http().ok_or_else(|| anyhow::anyhow!("Discord is not connected"))?;
    let bot = bot_id(app).ok_or_else(|| anyhow::anyhow!("Discord is not connected"))?;
    let role = event.access_role_id.as_deref().and_then(rid).ok_or_else(|| anyhow::anyhow!("bad access role id"))?;
    let mut body = json!({
        "name": clamp_text(&event.title, EVENT::TITLE_MAX),
        "type": 2,
        "permission_overwrites": overwrites_json(g, role, bot, &VOICE_ALLOW),
    });
    if let Some(p) = &parent_id {
        body["parent_id"] = json!(p);
    }
    let voice = match http.create_channel(g, &body, Some(&format!("Event voice: {}", clamp_text(&event.title, 60)))).await {
        Ok(v) => v,
        Err(e) => {
            let status = bot_core::discord::error_status(&e);
            if !status.is_some_and(|s| (400..500).contains(&s)) {
                return Err(e.into());
            }
            bot_util::log_warn!(format!("event {event_id}: Discord refused its voice channel"), bot_core::discord::error_message(&e));
            tell_chat(app, chat_channel, &format!("⚠️ I could not open the voice channel ({}).", serenity_reason(&e))).await;
            return Ok(JobOutcome::Done);
        }
    };
    bot_core::discord::cache_created_channel(app, &voice);
    let saved = update_open_events(
        app,
        &[("id", json!(event_id)), ("voiceChannelId", Value::Null)],
        vec![("voiceChannelId", SqlValueOrJson::Json(json!(voice.id.to_string())))],
    )
    .await;
    let saved = match saved {
        Ok(n) => n,
        Err(e) => {
            let _ = http.delete_channel(voice.id, Some("Event voice channel could not be linked")).await;
            return Err(e);
        }
    };
    if saved == 0 {
        let _ = http.delete_channel(voice.id, Some("Event closed while its voice channel opened")).await;
        return Ok(JobOutcome::Done);
    }
    tell_chat(app, chat_channel, &format!("🔊 The voice channel is open: <#{}>", voice.id)).await;
    request_render(app, &event_id);
    Ok(JobOutcome::Done)
}

/// `handleCleanupJob(job, ctx)`: the only thing that deletes event channels. It runs even while the module is off, and
/// again every 10 minutes while people are in the voice channel. The post and reminders go last, once both channels
/// are gone.
pub async fn handle_cleanup_job(job: Job, ctx: JobCtx) -> bot_core::BotResult<JobOutcome> {
    let app = &ctx.app;
    let event_id = job_event_id(&job, "event.cleanup")?;
    let Some(event) = find_guild_event(app, &event_id, &job.guild_id).await? else { return Ok(JobOutcome::Done) };
    if is_open(Some(&event)) {
        return Ok(JobOutcome::Done);
    }
    if let Some(voice) = event.voice_channel_id.clone().filter(|v| !v.is_empty()) {
        let found = resolve_channel(app, &event.guild_id, &voice).await?;
        // A channel the bot cannot see counts as occupied: what the deletes below take cannot be brought back.
        let occupied = match &found {
            ChannelLookup::NoAccess => true,
            ChannelLookup::Found(c) => feat_tempvoice::temp_voice_service::count_humans(app, c.guild_id, c.id) > 0,
            ChannelLookup::Gone => false,
        };
        if occupied {
            lock(&state(app).waiting_voice).insert(voice.clone(), (event_id.clone(), event.guild_id.clone(), false));
            return Ok(JobOutcome::Reschedule { next_run_at: at(app.clock().now_ms() + EVENT::CLEANUP_RECHECK_MS), payload: None });
        }
        lock(&state(app).waiting_voice).remove(&voice);
    }
    let voice_gone = delete_event_channel(app, &event, "voiceChannelId").await?;
    let text_gone = delete_event_channel(app, &event, "textChannelId").await?;
    delete_access_role(app, &event).await?;
    if voice_gone && text_gone {
        delete_event_messages(app, &event).await?;
    }
    Ok(JobOutcome::Done)
}
