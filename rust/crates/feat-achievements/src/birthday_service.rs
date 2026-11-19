//! Port of `src/services/birthdayService.js`: pure date helpers, all database access for `Birthday`, the hourly
//! `birthday.tick` job (announce at the guild-local hour, grant and remove the birthday role), the role transfer and
//! the plugin hook. Commands only read options and format replies.
//!
//! Redis (cache only): `birthday:announced:{guildId}:{userId}` is a tombstone holding the guild-local year of the last
//! completed celebration. Fail-open: a miss or an unreachable Redis means "no carry-over".
//!
//! Discord access of the tick, the role transfer and the removal goes through [`BirthdayDiscord`]: production uses
//! [`SerenityBirthdays`] (the serenity cache and REST), tests a fake.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bot_core::App;
use bot_core::interaction_helpers::{check_assignable_role_cached, check_channel_usable};
use bot_core::scheduler::{CancelJobs, JobOutcome, ScheduleJob};
use bot_core::settings::is_module_enabled;
use bot_db::sqlx::{self, Row};
use bot_db::{BIRTHDAY_INFO, Birthday, GuildSettings, InsertBuilder, JobStatus, SQL_NOW};
use bot_util::placeholder_parser::{PlaceholderChannel, PlaceholderGuild, PlaceholderMember, PlaceholderUser, RenderContext, VarValue, render_placeholders};
use bot_util::text::clamp_text;
use bot_util::time::{ZonedParts, days_in_month, is_valid_time_zone, local_date_key, zoned_parts, zoned_to_utc};
use chrono::{DateTime, Duration, Utc};
use serde_json::json;
use serenity::all::{ChannelId, GuildId, Member, RoleId, UserId};
use serenity::builder::{CreateAllowedMentions, CreateMessage};

// ── Constants ─────────────────────────────────────────────────────────────────

pub const DEFAULT_BIRTHDAY_MESSAGE: &str = "Happy birthday {user}! 🎂";
pub const DEFAULT_BIRTHDAY_MESSAGE_AGE: &str = "Happy birthday {user}! They are now {age}! 🎂";
/// `TICK_JOB_TYPE`.
pub const TICK_JOB_TYPE: &str = "birthday.tick";
pub const DEFAULT_HOUR: i64 = 9;
/// Celebrations claimed per run.
pub const MAX_EVENTS_PER_TICK: usize = 50;
pub const MAX_MARKERS_PER_TICK: i64 = 500;
/// `allowed_mentions.users` hard cap is 100.
pub const MAX_USERS_PER_MESSAGE: usize = 50;
pub const MAX_MESSAGE_CHARS: usize = 2000;
/// Stop starting Discord operations; the scheduler's job timeout is 60 s.
pub const TICK_DEADLINE_MS: u64 = 40_000;
/// Continuation run when work remains.
pub const CONTINUE_MS: i64 = 60_000;
pub const ERROR_RETRY_MS: i64 = 5 * 60_000;
/// Config changes.
pub const SOON_MS: i64 = 5_000;
/// Run just after the local hour boundary.
pub const BOUNDARY_SLACK_MS: i64 = 2_000;
pub const MARKER_GIVE_UP_MS: i64 = 7 * 24 * 60 * 60_000;
pub const NEXT_LIMIT: usize = 10;
pub const NEXT_CANDIDATES: i64 = 30;
pub const MEMBER_FETCH_TIMEOUT_MS: u64 = 10_000;
/// Gateway `request_guild_members` accepts at most 100 user ids.
pub const MEMBER_FETCH_CHUNK: usize = 100;
pub const MONTH_NAMES: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

const HOUR_MS: i64 = 3_600_000;
const ANNOUNCE_NEEDS: [&str; 2] = ["ViewChannel", "SendMessages"];

/// A configured id: JS truthiness, so an empty string is unset.
pub fn set_id(v: &Option<String>) -> Option<&str> {
    v.as_deref().filter(|s| !s.is_empty())
}

/// `tickKey(guildId)`: the dedupe key of the guild's recurring job.
pub fn tick_key(guild_id: &str) -> String {
    format!("birthday.tick:{guild_id}")
}

/// `tombstoneKey(guildId, userId)`.
pub fn tombstone_key(guild_id: &str, user_id: &str) -> String {
    format!("birthday:announced:{guild_id}:{user_id}")
}

// ── In-process state ──────────────────────────────────────────────────────────

/// The JS module-level state (`running`, `guildMutex`, `warnThrottle`), one per app.
pub struct BdState {
    running: Mutex<HashSet<String>>,
    guild_mutex: bot_core::sync::KeyedMutex,
    warn_throttle: bot_util::async_util::Throttle,
}

impl Default for BdState {
    fn default() -> Self {
        Self { running: Mutex::new(HashSet::new()), guild_mutex: bot_core::sync::KeyedMutex::new(), warn_throttle: bot_util::async_util::create_throttle(6 * HOUR_MS as u64) }
    }
}

fn state(app: &App) -> Arc<BdState> {
    app.ext::<BdState>()
}

fn warn_once(app: &App, guild_id: &str, reason: &str, message: &str) {
    if state(app).warn_throttle.allow(&format!("{guild_id}:{reason}")) {
        bot_util::log_warn!(message.to_string());
    }
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

/// A guild-local calendar date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LocalDate {
    pub year: i64,
    pub month: u32,
    pub day: u32,
}

impl From<ZonedParts> for LocalDate {
    fn from(p: ZonedParts) -> Self {
        Self { year: p.year, month: p.month, day: p.day }
    }
}

/// `clampHour(h)`: an integer 0-23, or the default (9) for anything else.
pub fn clamp_hour(h: Option<f64>) -> i64 {
    match h {
        Some(h) if h.fract() == 0.0 && (0.0..=23.0).contains(&h) => h as i64,
        _ => DEFAULT_HOUR,
    }
}

fn hour_of(settings: Option<&GuildSettings>) -> i64 {
    clamp_hour(settings.map(|s| f64::from(s.birthday_hour)))
}

/// `resolveTimeZone(settings)`: the guild's IANA zone, or `UTC` when it is empty or invalid.
pub fn resolve_time_zone(settings: Option<&GuildSettings>) -> String {
    match settings {
        Some(s) if is_valid_time_zone(&s.timezone) => s.timezone.clone(),
        _ => "UTC".into(),
    }
}

/// `formatHour(h)`: `HH:00`.
pub fn format_hour(h: Option<f64>) -> String {
    bot_util::time::format_time_of_day(bot_util::time::TimeOfDay { hour: clamp_hour(h) as u32, minute: 0 })
}

/// `isLeapYear(y)`.
pub fn is_leap_year(y: i64) -> bool {
    days_in_month(y, 2) == 29
}

/// `effectiveDay(month, day, year)`: Feb 29 is celebrated on Feb 28 in non-leap years.
pub fn effective_day(month: u32, day: u32, year: i64) -> u32 {
    if month == 2 && day == 29 && !is_leap_year(year) { 28 } else { day }
}

/// `isBirthdayOn(row, local)`.
pub fn is_birthday_on(month: u32, day: u32, local: LocalDate) -> bool {
    month == local.month && effective_day(month, day, local.year) == local.day
}

/// `todayMatchers(local)`: the (month, day) pairs that are a birthday on the local date.
pub fn today_matchers(local: LocalDate) -> Vec<(u32, u32)> {
    let mut out = vec![(local.month, local.day)];
    if !is_leap_year(local.year) && local.month == 2 && local.day == 28 {
        out.push((2, 29));
    }
    out
}

/// `nextOccurrence(row, local)`: the next date (today included) on which the row is celebrated.
pub fn next_occurrence(month: u32, day: u32, local: LocalDate) -> LocalDate {
    let d = effective_day(month, day, local.year);
    if month > local.month || (month == local.month && d >= local.day) {
        return LocalDate { year: local.year, month, day: d };
    }
    LocalDate { year: local.year + 1, month, day: effective_day(month, day, local.year + 1) }
}

/// `ageOn(row, celebrationYear)`: the age being celebrated, or `None` when the year is unknown or the age is below 1.
pub fn age_on(year: Option<i64>, celebration_year: i64) -> Option<i64> {
    let age = celebration_year - year?;
    (age >= 1).then_some(age)
}

/// `isEventEligible(row, local, hour)`.
pub fn is_event_eligible(month: u32, day: u32, last_announced_year: Option<i64>, local: LocalDate, local_hour: i64, hour: Option<f64>) -> bool {
    local_hour >= clamp_hour(hour) && is_birthday_on(month, day, local) && last_announced_year.is_none_or(|y| y < local.year)
}

/// A role marker (a Birthday row with `roleGrantedAt`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    pub id: String,
    pub user_id: String,
    pub month: u32,
    pub day: u32,
    pub role_granted_at: DateTime<Utc>,
}

/// `partitionMarkers(markers, { enabled, roleId, local, tz })`: (keep, expired). Pure.
pub fn partition_markers(markers: &[Marker], enabled: bool, role_id: Option<&str>, local: LocalDate, tz: &str) -> (Vec<Marker>, Vec<Marker>) {
    let today = format!("{:04}-{:02}-{:02}", local.year, local.month, local.day);
    let (mut keep, mut expired) = (Vec::new(), Vec::new());
    for m in markers {
        let due = enabled && role_id.is_some_and(|r| !r.is_empty()) && is_birthday_on(m.month, m.day, local) && local_date_key(&m.role_granted_at, tz) == today;
        if due { keep.push(m.clone()) } else { expired.push(m.clone()) }
    }
    (keep, expired)
}

/// `pickTemplate(settings, age)`: the age message when the age is known, otherwise the plain one.
pub fn pick_template(settings: Option<&GuildSettings>, age: Option<i64>) -> String {
    let pick = |v: Option<&String>, d: &str| v.filter(|s| !s.is_empty()).cloned().unwrap_or_else(|| d.to_string());
    match age {
        None => pick(settings.and_then(|s| s.birthday_message.as_ref()), DEFAULT_BIRTHDAY_MESSAGE),
        Some(_) => pick(settings.and_then(|s| s.birthday_message_age.as_ref()), DEFAULT_BIRTHDAY_MESSAGE_AGE),
    }
}

/// `nextTickAt(now, tz)`: the first run strictly after `now` just after a local hour boundary of `tz`.
pub fn next_tick_at(now: &DateTime<Utc>, tz: &str) -> DateTime<Utc> {
    let p = zoned_parts(now, tz);
    let start = zoned_to_utc(p.year, i64::from(p.month), i64::from(p.day), i64::from(p.hour), 0, tz).map_or(now.timestamp_millis(), |d| d.timestamp_millis());
    let mut t = start + HOUR_MS + BOUNDARY_SLACK_MS;
    // A DST overlap makes zonedToUtc return the earlier instant: the loop catches up.
    while t <= now.timestamp_millis() {
        t += HOUR_MS;
    }
    bot_util::time::from_ms(t as f64).unwrap_or(*now)
}

/// `tombstoneTtlSeconds(now, tz)`: seconds until Jan 1 of the next local year, plus one day. Never below 60.
pub fn tombstone_ttl_seconds(now: &DateTime<Utc>, tz: &str) -> i64 {
    let p = zoned_parts(now, tz);
    let end = zoned_to_utc(p.year + 1, 1, 1, 0, 0, tz).map_or(now.timestamp_millis(), |d| d.timestamp_millis());
    let secs = ((end - now.timestamp_millis()) as f64 / 1000.0).ceil() as i64;
    (secs + 86_400).max(60)
}

/// One announcement line before chunking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnounceEntry {
    pub id: String,
    pub user_id: String,
    pub text: String,
}

/// One message of the announcement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub content: String,
    pub user_ids: Vec<String>,
    pub ids: Vec<String>,
}

/// `chunkAnnouncements(entries, { maxChars, maxUsers })`: packs lines into messages, greedily and in order.
pub fn chunk_announcements(entries: &[AnnounceEntry], max_chars: usize, max_users: usize) -> Vec<Chunk> {
    let mut chunks: Vec<Chunk> = Vec::new();
    let len = bot_util::js::utf16_len;
    for e in entries {
        let text = clamp_text(&e.text, max_chars);
        let fits = chunks.last().is_some_and(|c| c.ids.len() < max_users && len(&c.content) + 1 + len(&text) <= max_chars);
        if fits {
            if let Some(c) = chunks.last_mut() {
                c.content.push('\n');
                c.content.push_str(&text);
            }
        } else {
            chunks.push(Chunk { content: text, user_ids: Vec::new(), ids: Vec::new() });
        }
        if let Some(c) = chunks.last_mut() {
            c.ids.push(e.id.clone());
            if !c.user_ids.contains(&e.user_id) {
                c.user_ids.push(e.user_id.clone());
            }
        }
    }
    chunks
}

/// `formatBirthday({ month, day, year }, { short, withYear })`: `March 14`, `Mar 14`, `March 14, 1998`.
pub fn format_birthday(month: u32, day: u32, year: Option<i64>, short: bool, with_year: bool) -> String {
    let name = MONTH_NAMES.get((month as usize).wrapping_sub(1)).copied().unwrap_or("?");
    let name = if short { name.chars().take(3).collect::<String>() } else { name.to_string() };
    let mut out = format!("{name} {day}");
    if let (true, Some(y)) = (with_year, year.filter(|y| *y != 0)) {
        out.push_str(&format!(", {y}"));
    }
    out
}

/// `codeBlock(text, max)`: a fenced code block user text cannot break out of (a zero-width space follows every
/// backtick).
pub fn code_block(text: &str, max: usize) -> String {
    format!("```\n{}\n```", clamp_text(&text.replace('`', "`\u{200b}"), max))
}

// ── Date parsing ──────────────────────────────────────────────────────────────

const FORMAT_MESSAGE: &str = "Use **MM-DD** (like `03-14`) or **YYYY-MM-DD** (like `1998-03-14`).";

/// A parsed birthday.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BirthdayDate {
    pub month: u32,
    pub day: u32,
    pub year: Option<i64>,
}

/// Why [`parse_birthday_input`] refused: `code` is `FORMAT`, `YEAR_RANGE`, `INVALID_DATE`, `YEAR_REQUIRED` or
/// `FUTURE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseFail {
    pub code: &'static str,
    pub message: String,
}

fn bd_fail(code: &'static str, message: impl Into<String>) -> ParseFail {
    ParseFail { code, message: message.into() }
}

/// `parseBirthdayInput(raw, { requireYear, now, tz })`.
pub fn parse_birthday_input(raw: &str, require_year: bool, now: &DateTime<Utc>, tz: &str) -> Result<BirthdayDate, ParseFail> {
    let s = bot_util::js::trim(raw);
    let parts: Vec<&str> = s.split('-').collect();
    let digits = |p: &str, lo: usize, hi: usize| (lo..=hi).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit());
    let (y, m, d) = match parts.as_slice() {
        [m, d] if digits(m, 1, 2) && digits(d, 1, 2) => (None, *m, *d),
        [y, m, d] if digits(y, 4, 4) && digits(m, 1, 2) && digits(d, 1, 2) => (Some(*y), *m, *d),
        _ => return Err(bd_fail("FORMAT", FORMAT_MESSAGE)),
    };
    let year: Option<i64> = y.and_then(|y| y.parse().ok());
    let month: u32 = m.parse().unwrap_or(0);
    let day: u32 = d.parse().unwrap_or(0);
    let local = zoned_parts(now, tz);
    if let Some(yr) = year
        && (yr < 1900 || yr > local.year)
    {
        return Err(bd_fail("YEAR_RANGE", format!("The year must be between 1900 and {}.", local.year)));
    }
    if bot_util::time::parse_birthday(&format!("{month:02}-{day:02}"), false, now).is_none() {
        return Err(bd_fail("INVALID_DATE", format!("**{s}** is not a real date.")));
    }
    if let Some(yr) = year
        && day > days_in_month(yr, i64::from(month))
    {
        return Err(bd_fail("INVALID_DATE", format!("**{s}** is not a real date: {yr} is not a leap year.")));
    }
    if require_year && year.is_none() {
        return Err(bd_fail("YEAR_REQUIRED", "This server asks for your birth year too: use **YYYY-MM-DD** (like `1998-03-14`)."));
    }
    if year == Some(local.year) && (month > local.month || (month == local.month && day > local.day)) {
        return Err(bd_fail("FUTURE", "That date is in the future."));
    }
    Ok(BirthdayDate { month, day, year })
}

// ── Message rendering ─────────────────────────────────────────────────────────

/// `renderBirthdayLine({ template, member, guild, channel, age })`: one announcement line. `age` is empty when unknown.
/// An empty result falls back to the default template.
pub fn render_birthday_line(template: &str, ctx: RenderContext, age: Option<i64>) -> String {
    let mut ctx = ctx;
    ctx.vars = Some(vec![("age".into(), VarValue::Text(age.map(|a| a.to_string()).unwrap_or_default()))]);
    let out = render_placeholders(template, &ctx, Some(MAX_MESSAGE_CHARS));
    if !bot_util::js::trim(&out).is_empty() {
        return out;
    }
    render_placeholders(&pick_template(None, age), &ctx, Some(MAX_MESSAGE_CHARS))
}

/// The render context of a birthday line.
pub fn line_context(member: Option<&BdMember>, guild: Option<PlaceholderGuild>, channel: Option<PlaceholderChannel>) -> RenderContext {
    RenderContext {
        member: member.and_then(|m| m.ctx_member.clone()),
        user: member.and_then(|m| m.ctx_user.clone()),
        guild,
        channel,
        vars: None,
        args: None,
    }
}

// ── Discord seam ──────────────────────────────────────────────────────────────

/// A guild member as the birthday code needs it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BdMember {
    pub id: String,
    pub bot: bool,
    /// Membership screening not passed yet.
    pub pending: bool,
    pub roles: Vec<String>,
    pub ctx_member: Option<PlaceholderMember>,
    pub ctx_user: Option<PlaceholderUser>,
}

/// A Discord error: its JSON code when there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordFail {
    pub code: Option<u64>,
    pub message: String,
}

/// What the tick, the role transfer and the removal do on Discord (one guild).
#[async_trait::async_trait]
pub trait BirthdayDiscord: Send + Sync {
    /// `guild.available`.
    fn available(&self) -> bool;
    fn guild(&self) -> Option<PlaceholderGuild>;
    /// A cached channel (`guild.channels.cache.get(id)`).
    fn channel(&self, id: &str) -> Option<PlaceholderChannel>;
    /// `checkChannelUsable(channel, { needs }).ok`.
    fn channel_usable(&self, id: &str, needs: &[&str]) -> bool;
    /// A cached role.
    fn role_exists(&self, id: &str) -> bool;
    /// `checkAssignableRole(guild, role, { allowDangerous }).ok`.
    fn role_assignable(&self, id: &str, allow_dangerous: bool) -> bool;
    /// `fetchMembers(guild, ids)`: an id missing from the result has left the guild; an error propagates.
    async fn members(&self, ids: &[String]) -> Result<HashMap<String, BdMember>, DiscordFail>;
    async fn send(&self, channel_id: &str, content: &str, user_ids: &[String]) -> Result<(), DiscordFail>;
    async fn add_role(&self, user_id: &str, role_id: &str, reason: &str) -> Result<(), DiscordFail>;
    async fn remove_role(&self, user_id: &str, role_id: &str, reason: &str) -> Result<(), DiscordFail>;
}

/// The production [`BirthdayDiscord`]: the serenity cache, then REST.
pub struct SerenityBirthdays {
    pub app: App,
    pub guild_id: GuildId,
}

fn snowflake(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|v| *v != 0)
}

fn serenity_fail(e: &serenity::Error) -> DiscordFail {
    DiscordFail { code: bot_core::discord::error_code(e), message: bot_core::discord::error_message(e) }
}

fn offline() -> DiscordFail {
    DiscordFail { code: None, message: "Discord is not connected".into() }
}

/// A serenity member as [`BdMember`].
pub fn bd_member(m: &Member, guild: Option<&serenity::all::Guild>) -> BdMember {
    BdMember {
        id: m.user.id.to_string(),
        bot: m.user.bot,
        pending: m.pending,
        roles: m.roles.iter().map(|r| r.to_string()).collect(),
        ctx_member: Some(bot_core::adapters::placeholder_member(m, guild)),
        ctx_user: Some(bot_core::adapters::placeholder_user(&m.user)),
    }
}

impl SerenityBirthdays {
    pub fn new(app: &App, guild_id: GuildId) -> Self {
        Self { app: app.clone(), guild_id }
    }

    fn cached(&self) -> Option<serenity::all::Guild> {
        bot_core::interaction_helpers::cached_guild(&self.app, self.guild_id)
    }
}

#[async_trait::async_trait]
impl BirthdayDiscord for SerenityBirthdays {
    fn available(&self) -> bool {
        self.cached().is_some_and(|g| !g.unavailable)
    }

    fn guild(&self) -> Option<PlaceholderGuild> {
        self.cached().map(|g| bot_core::adapters::placeholder_guild(&g))
    }

    fn channel(&self, id: &str) -> Option<PlaceholderChannel> {
        bot_core::interaction_helpers::cached_channel(&self.app, self.guild_id, ChannelId::new(snowflake(id)?)).map(|c| bot_core::adapters::placeholder_channel(&c))
    }

    fn channel_usable(&self, id: &str, needs: &[&str]) -> bool {
        snowflake(id).is_some_and(|c| check_channel_usable(&self.app, self.guild_id, Some(ChannelId::new(c)), needs).ok)
    }

    fn role_exists(&self, id: &str) -> bool {
        snowflake(id).is_some_and(|r| self.cached().is_some_and(|g| g.roles.contains_key(&RoleId::new(r))))
    }

    fn role_assignable(&self, id: &str, allow_dangerous: bool) -> bool {
        snowflake(id).is_some_and(|r| check_assignable_role_cached(&self.app, self.guild_id, RoleId::new(r), None, allow_dangerous).ok)
    }

    async fn members(&self, ids: &[String]) -> Result<HashMap<String, BdMember>, DiscordFail> {
        let mut out = HashMap::new();
        let guild = self.cached();
        let mut missing = Vec::new();
        let mut seen = HashSet::new();
        for id in ids {
            if !seen.insert(id.clone()) {
                continue;
            }
            let Some(u) = snowflake(id) else { continue };
            match guild.as_ref().and_then(|g| g.members.get(&UserId::new(u))) {
                Some(m) => {
                    out.insert(id.clone(), bd_member(m, guild.as_ref()));
                }
                None => missing.push(u),
            }
        }
        if missing.is_empty() {
            return Ok(out);
        }
        if self.app.discord().is_none() {
            return Err(offline());
        }
        // `guild.members.fetch({ user: ids, time: 10 s })`: one gateway request per 100 ids; a timeout rejects (the
        // tick fails before it changes anything); an id Discord does not return has left the guild.
        let ids: Vec<UserId> = missing.into_iter().map(UserId::new).collect();
        let found = bot_core::guild_members::fetch_members_by_ids(&self.app, self.guild_id, &ids, MEMBER_FETCH_TIMEOUT_MS)
            .await
            .map_err(|e| DiscordFail { code: None, message: e.to_string() })?;
        for (u, m) in found {
            out.insert(u.to_string(), bd_member(&m, guild.as_ref()));
        }
        Ok(out)
    }

    async fn send(&self, channel_id: &str, content: &str, user_ids: &[String]) -> Result<(), DiscordFail> {
        let Some(http) = self.app.http() else { return Err(offline()) };
        let Some(c) = snowflake(channel_id) else { return Err(DiscordFail { code: Some(10003), message: "Unknown Channel".into() }) };
        let users: Vec<UserId> = user_ids.iter().filter_map(|u| snowflake(u)).map(UserId::new).collect();
        let msg = CreateMessage::new().content(content).allowed_mentions(CreateAllowedMentions::new().users(users));
        ChannelId::new(c).send_message(http.as_ref(), msg).await.map(|_| ()).map_err(|e| serenity_fail(&e))
    }

    async fn add_role(&self, user_id: &str, role_id: &str, reason: &str) -> Result<(), DiscordFail> {
        let Some(http) = self.app.http() else { return Err(offline()) };
        let (Some(u), Some(r)) = (snowflake(user_id), snowflake(role_id)) else { return Err(DiscordFail { code: Some(10011), message: "Unknown Role".into() }) };
        http.add_member_role(self.guild_id, UserId::new(u), RoleId::new(r), Some(reason)).await.map_err(|e| serenity_fail(&e))
    }

    async fn remove_role(&self, user_id: &str, role_id: &str, reason: &str) -> Result<(), DiscordFail> {
        let Some(http) = self.app.http() else { return Err(offline()) };
        let (Some(u), Some(r)) = (snowflake(user_id), snowflake(role_id)) else { return Err(DiscordFail { code: Some(10011), message: "Unknown Role".into() }) };
        http.remove_member_role(self.guild_id, UserId::new(u), RoleId::new(r), Some(reason)).await.map_err(|e| serenity_fail(&e))
    }
}

// ── Database access ───────────────────────────────────────────────────────────

/// `getBirthday(guildId, userId)`.
pub async fn get_birthday(pool: &bot_db::PgPool, guild_id: &str, user_id: &str) -> Result<Option<Birthday>, sqlx::Error> {
    sqlx::query_as(r#"SELECT * FROM "Birthday" WHERE "guildId" = $1 AND "userId" = $2"#).bind(guild_id).bind(user_id).fetch_optional(pool).await
}

/// `countBirthdays(guildId)`: (total, withoutYear).
pub async fn count_birthdays(pool: &bot_db::PgPool, guild_id: &str) -> Result<(i64, i64), sqlx::Error> {
    let row = sqlx::query(r#"SELECT COUNT(*) AS "total", COUNT(*) FILTER (WHERE "year" IS NULL) AS "without" FROM "Birthday" WHERE "guildId" = $1"#)
        .bind(guild_id)
        .fetch_one(pool)
        .await?;
    Ok((row.try_get("total")?, row.try_get("without")?))
}

/// `hasMarkers(guildId)`: any member of the guild still carries a role marker.
pub async fn has_markers(pool: &bot_db::PgPool, guild_id: &str) -> Result<bool, sqlx::Error> {
    let row = sqlx::query(r#"SELECT "id" FROM "Birthday" WHERE "guildId" = $1 AND "roleGrantedAt" IS NOT NULL LIMIT 1"#).bind(guild_id).fetch_optional(pool).await?;
    Ok(row.is_some())
}

/// What [`save_birthday`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct SaveResult {
    pub created: bool,
    pub date_changed: bool,
    pub late: bool,
    pub already_celebrated: bool,
    pub row: Birthday,
}

/// `saveBirthday({ guild, userId, date, settings, now })`: creates the Birthday row of a member, or changes its date.
/// One celebration per member per guild per local year.
pub async fn save_birthday(app: &App, guild_id: &str, user_id: &str, date: BirthdayDate, settings: Option<&GuildSettings>, now: &DateTime<Utc>) -> anyhow::Result<SaveResult> {
    app.settings().ensure_guild_settings(guild_id).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let tz = resolve_time_zone(settings);
    let parts = zoned_parts(now, &tz);
    let local = LocalDate::from(parts);
    let hour = hour_of(settings);
    let existing = get_birthday(app.db(), guild_id, user_id).await?;
    let date_changed = existing.as_ref().is_none_or(|e| e.month as u32 != date.month || e.day as u32 != date.day);
    let mut last: Option<i64> = existing.as_ref().and_then(|e| e.last_announced_year).map(i64::from);
    if existing.is_none() {
        let t = app.cache().get(&tombstone_key(guild_id, user_id)).await;
        let year = t.as_ref().and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok())));
        if year == Some(local.year as f64) {
            last = Some(local.year);
        }
    }
    let late = date_changed && is_birthday_on(date.month, date.day, local) && i64::from(parts.hour) >= hour && last.is_none_or(|l| l < local.year);
    if late {
        last = Some(local.year);
    }
    let already_celebrated = !late && date_changed && last == Some(local.year);
    let fields = [
        ("month", json!(date.month)),
        ("day", json!(date.day)),
        ("year", json!(date.year)),
        ("lastAnnouncedYear", json!(last)),
    ];
    let q = InsertBuilder::new(&BIRTHDAY_INFO)
        .values([("guildId", json!(guild_id)), ("userId", json!(user_id))])?
        .values(fields.clone())?
        .on_conflict_update(&["guildId", "userId"], fields)?
        .build()?;
    let row: Birthday = q.fetch_one(app.db()).await?;
    Ok(SaveResult { created: existing.is_none(), date_changed, late, already_celebrated, row })
}

/// `removeBirthday({ guild, userId, settings })`: deletes a member's birthday, taking the birthday role off first. The
/// tombstone stays. Returns whether a row was removed.
pub async fn remove_birthday(app: &App, d: &dyn BirthdayDiscord, guild_id: &str, user_id: &str, settings: Option<&GuildSettings>) -> anyhow::Result<bool> {
    let Some(row) = get_birthday(app.db(), guild_id, user_id).await? else { return Ok(false) };
    if let (Some(_), Some(role_id)) = (row.role_granted_at, settings.and_then(|s| set_id(&s.birthday_role_id))) {
        let member = d.members(&[user_id.to_string()]).await.ok().and_then(|mut m| m.remove(user_id));
        if member.is_some_and(|m| m.roles.iter().any(|r| r == role_id))
            && let Err(e) = d.remove_role(user_id, role_id, "Birthday removed").await
        {
            bot_util::log_warn!(format!("birthdays: could not remove the birthday role from {user_id} in guild {guild_id}: {}", e.message));
        }
    }
    sqlx::query(r#"DELETE FROM "Birthday" WHERE "guildId" = $1 AND "userId" = $2"#).bind(guild_id).bind(user_id).execute(app.db()).await?;
    Ok(true)
}

/// One `/birthday next` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upcoming {
    pub user_id: String,
    pub month: u32,
    pub day: u32,
    pub occ: LocalDate,
    pub at: DateTime<Utc>,
    pub is_today: bool,
}

/// `getUpcomingBirthdays(guild, settings, { limit, now })`: members whose birthday comes next, soonest first (today
/// included). Departed members and bots are left out; a failed member fetch keeps every row.
pub async fn get_upcoming_birthdays(app: &App, d: &dyn BirthdayDiscord, guild_id: &str, settings: Option<&GuildSettings>, limit: usize, now: &DateTime<Utc>) -> anyhow::Result<Vec<Upcoming>> {
    let tz = resolve_time_zone(settings);
    let local = LocalDate::from(zoned_parts(now, &tz));
    let (m, dd) = (local.month as i32, local.day as i32);
    let read = |rows: Vec<bot_db::sqlx::postgres::PgRow>| -> Result<Vec<(String, i32, i32)>, sqlx::Error> {
        rows.iter().map(|r| Ok((r.try_get("userId")?, r.try_get("month")?, r.try_get("day")?))).collect()
    };
    let rows = sqlx::query(
        r#"SELECT "userId", "month", "day" FROM "Birthday" WHERE "guildId" = $1 AND ("month" > $2 OR ("month" = $2 AND "day" >= $3)) ORDER BY "month" ASC, "day" ASC, "userId" ASC LIMIT $4"#,
    )
    .bind(guild_id)
    .bind(m)
    .bind(dd)
    .bind(NEXT_CANDIDATES)
    .fetch_all(app.db())
    .await?;
    let mut rows = read(rows)?;
    if (rows.len() as i64) < NEXT_CANDIDATES {
        let wrapped = sqlx::query(
            r#"SELECT "userId", "month", "day" FROM "Birthday" WHERE "guildId" = $1 AND ("month" < $2 OR ("month" = $2 AND "day" < $3)) ORDER BY "month" ASC, "day" ASC, "userId" ASC LIMIT $4"#,
        )
        .bind(guild_id)
        .bind(m)
        .bind(dd)
        .bind(NEXT_CANDIDATES - rows.len() as i64)
        .fetch_all(app.db())
        .await?;
        rows.extend(read(wrapped)?);
    }
    let mut entries: Vec<Upcoming> = rows
        .into_iter()
        .map(|(user_id, month, day)| {
            let occ = next_occurrence(month as u32, day as u32, local);
            let at = zoned_to_utc(occ.year, i64::from(occ.month), i64::from(occ.day), 0, 0, &tz).unwrap_or(*now);
            Upcoming { user_id, month: month as u32, day: day as u32, occ, at, is_today: occ == local }
        })
        .collect();
    entries.sort_by(|a, b| a.occ.cmp(&b.occ).then_with(|| a.user_id.cmp(&b.user_id)));
    let ids: Vec<String> = entries.iter().map(|e| e.user_id.clone()).collect();
    if let Ok(members) = d.members(&ids).await {
        entries.retain(|e| members.get(&e.user_id).is_some_and(|m| !m.bot));
    }
    entries.truncate(limit);
    Ok(entries)
}

// ── Raw SQL ───────────────────────────────────────────────────────────────────

/// The tick's at-most-once guard: marks the rows as celebrated this local year and, with `set_marker`, sets the role
/// marker. `$1` year, `$2` guildId, `$3` ids.
pub fn claim_sql(set_marker: bool) -> String {
    let marker = if set_marker { format!(r#", "roleGrantedAt" = {SQL_NOW}"#) } else { String::new() };
    format!(
        r#"UPDATE "Birthday"
  SET "lastAnnouncedYear" = $1, "updatedAt" = {SQL_NOW}
      {marker}
  WHERE "guildId" = $2
    AND id = ANY($3::text[])
    AND ("lastAnnouncedYear" IS NULL OR "lastAnnouncedYear" < $1)
  RETURNING id, "userId", "year""#
    )
}

/// `RAW_SQL`: every raw statement by name.
pub fn raw_sql() -> Vec<(&'static str, String)> {
    vec![("birthdays.claim", claim_sql(false)), ("birthdays.claimWithMarker", claim_sql(true))]
}

/// A claimed row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claimed {
    pub id: String,
    pub user_id: String,
    pub year: Option<i64>,
}

/// `claimBirthdays({ guildId, ids, year, setMarker })`.
pub async fn claim_birthdays(pool: &bot_db::PgPool, guild_id: &str, ids: &[String], year: i64, set_marker: bool) -> Result<Vec<Claimed>, sqlx::Error> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(claim_sql(set_marker))).bind(year as i32).bind(guild_id).bind(ids).fetch_all(pool).await?;
    rows.iter()
        .map(|r| Ok(Claimed { id: r.try_get("id")?, user_id: r.try_get("userId")?, year: r.try_get::<Option<i32>, _>("year")?.map(i64::from) }))
        .collect()
}

/// `revertClaims(guildId, ids, year)`: gives claims back after a failed send. Never fails.
pub async fn revert_claims(pool: &bot_db::PgPool, guild_id: &str, ids: &[String], year: i64) {
    if ids.is_empty() {
        return;
    }
    let sql = format!(r#"UPDATE "Birthday" SET "lastAnnouncedYear" = $1, "updatedAt" = {SQL_NOW} WHERE "guildId" = $2 AND "id" = ANY($3::text[]) AND "lastAnnouncedYear" = $4"#);
    if let Err(e) = sqlx::query(sqlx::AssertSqlSafe(sql)).bind((year - 1) as i32).bind(guild_id).bind(ids).bind(year as i32).execute(pool).await {
        bot_util::log_error!(format!("birthdays: could not revert {} claim(s) in guild {guild_id}", ids.len()), bot_core::error::log_arg(&e.into()));
    }
}

/// `clearMarker(m)`: clears one role marker, but only while it still has the value that was read.
pub async fn clear_marker(pool: &bot_db::PgPool, m: &Marker) -> Result<u64, sqlx::Error> {
    let sql = format!(r#"UPDATE "Birthday" SET "roleGrantedAt" = NULL, "updatedAt" = {SQL_NOW} WHERE "id" = $1 AND "roleGrantedAt" = $2"#);
    Ok(sqlx::query(sqlx::AssertSqlSafe(sql)).bind(&m.id).bind(m.role_granted_at.naive_utc()).execute(pool).await?.rows_affected())
}

async fn clear_all_markers(pool: &bot_db::PgPool, guild_id: &str) -> Result<u64, sqlx::Error> {
    let sql = format!(r#"UPDATE "Birthday" SET "roleGrantedAt" = NULL, "updatedAt" = {SQL_NOW} WHERE "guildId" = $1 AND "roleGrantedAt" IS NOT NULL"#);
    Ok(sqlx::query(sqlx::AssertSqlSafe(sql)).bind(guild_id).execute(pool).await?.rows_affected())
}

// ── Members and roles ─────────────────────────────────────────────────────────

async fn grant_role(app: &App, d: &dyn BirthdayDiscord, guild_id: &str, user_id: &str, role_id: &str) -> bool {
    match d.add_role(user_id, role_id, "Birthday").await {
        Ok(()) => true,
        Err(e) => {
            match e.code {
                Some(10007) => {}
                Some(50013 | 50001) => warn_once(app, guild_id, "role-perms", &format!("birthdays: missing access to grant the birthday role in guild {guild_id}")),
                _ => bot_util::log_warn!(format!("birthdays: could not grant the birthday role in guild {guild_id}: {}", e.message)),
            }
            false
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Take {
    Ok,
    Gone,
    Keep,
}

async fn take_role(app: &App, d: &dyn BirthdayDiscord, guild_id: &str, user_id: &str, role_id: &str, reason: &str) -> Take {
    match d.remove_role(user_id, role_id, reason).await {
        Ok(()) => Take::Ok,
        Err(e) => match e.code {
            Some(10007 | 10011) => Take::Gone,
            Some(50013 | 50001) => {
                warn_once(app, guild_id, "role-perms", &format!("birthdays: missing access to remove the birthday role in guild {guild_id}"));
                Take::Keep
            }
            _ => {
                bot_util::log_warn!(format!("birthdays: could not remove the birthday role in guild {guild_id}: {}", e.message));
                Take::Keep
            }
        },
    }
}

/// `transferBirthdayRole(guild, oldRoleId, newRoleId)`: moves today's celebrants off the previous birthday role, inside
/// the guild mutex. With no new role every marker is cleared. Never fails; returns (removed, failed).
pub async fn transfer_birthday_role(app: &App, d: Arc<dyn BirthdayDiscord>, guild_id: &str, old_role_id: &str, new_role_id: Option<&str>) -> (u64, u64) {
    let (a, g, old, new) = (app.clone(), guild_id.to_string(), old_role_id.to_string(), new_role_id.map(str::to_string));
    state(app)
        .guild_mutex
        .run(guild_id, move || async move {
            let (mut removed, mut failed) = (0u64, 0u64);
            let r: anyhow::Result<()> = async {
                let rows = sqlx::query(r#"SELECT "id", "userId" FROM "Birthday" WHERE "guildId" = $1 AND "roleGrantedAt" IS NOT NULL LIMIT $2"#)
                    .bind(&g)
                    .bind(MAX_MARKERS_PER_TICK)
                    .fetch_all(a.db())
                    .await?;
                let users: Vec<String> = rows.iter().map(|r| r.try_get::<String, _>("userId")).collect::<Result<_, _>>()?;
                let exists = d.role_exists(&old);
                if exists && !users.is_empty() && d.role_assignable(&old, true) {
                    let members = d.members(&users).await.unwrap_or_default();
                    for user in &users {
                        let Some(m) = members.get(user) else { continue };
                        if !m.roles.contains(&old) {
                            continue;
                        }
                        match take_role(&a, d.as_ref(), &g, user, &old, "Birthday role changed").await {
                            Take::Ok => removed += 1,
                            Take::Keep => failed += 1,
                            Take::Gone => {}
                        }
                    }
                } else if exists && !users.is_empty() {
                    failed += users.len() as u64; // the old role exists but the bot can no longer manage it
                }
                if new.is_none() {
                    clear_all_markers(a.db(), &g).await?;
                }
                Ok(())
            }
            .await;
            if let Err(e) = r {
                bot_util::log_error!(format!("birthdays: role transfer failed in guild {g}"), bot_core::error::log_arg(&e));
            }
            (removed, failed)
        })
        .await
}

// ── Scheduling ────────────────────────────────────────────────────────────────

/// `ensureTickJob(guildId, settings, { soon })`: makes sure the guild has its recurring `birthday.tick` job when
/// anything is configured, and drops it when nothing is (and no role marker is left). `soon` replaces the job with one
/// that runs in a few seconds. Never fails.
pub async fn ensure_tick_job(app: &App, guild_id: &str, settings: Option<&GuildSettings>, soon: bool) {
    let Some(s) = settings else { return }; // without settings we cannot decide: an existing job is kept
    let r: anyhow::Result<()> = async {
        let configured = set_id(&s.birthday_channel_id).is_some() || set_id(&s.birthday_role_id).is_some();
        if !configured && !has_markers(app.db(), guild_id).await? {
            app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(tick_key(guild_id)), ..Default::default() }).await;
            return Ok(());
        }
        let job = |run_at: DateTime<Utc>| {
            ScheduleJob::new(guild_id, bot_config::JobType::BirthdayTick, run_at).payload(json!({})).dedupe_key(tick_key(guild_id)).on_conflict(bot_core::scheduler::OnConflict::Replace)
        };
        if soon {
            app.scheduler().schedule_job(job(app.clock().now() + Duration::milliseconds(SOON_MS))).await?;
            return Ok(());
        }
        if let Some(j) = app.scheduler().get_job(&tick_key(guild_id)).await
            && matches!(j.status, JobStatus::Pending | JobStatus::Running)
        {
            return Ok(());
        }
        app.scheduler().schedule_job(job(next_tick_at(&app.clock().now(), &resolve_time_zone(Some(s))))).await?;
        Ok(())
    }
    .await;
    if let Err(e) = r {
        bot_util::log_error!("birthdays: could not schedule birthday.tick", bot_core::error::log_arg(&e));
    }
}

/// `onGuildReady(ctx)`: unconfigured guilds cost nothing at startup; configured ones get a missing or FAILED job back.
pub async fn on_guild_ready(app: &App, guild_id: &str, settings: Option<&GuildSettings>) {
    let Some(s) = settings else { return };
    if set_id(&s.birthday_channel_id).is_none() && set_id(&s.birthday_role_id).is_none() {
        return;
    }
    ensure_tick_job(app, guild_id, Some(s), false).await;
}

// ── The tick ──────────────────────────────────────────────────────────────────

fn later(app: &App, ms: i64) -> JobOutcome {
    JobOutcome::Reschedule { next_run_at: app.clock().now() + Duration::milliseconds(ms), payload: None }
}

/// `runBirthdayTick(job, ctx)` with the Discord side given. Never fails: an error becomes a retry in five minutes.
pub async fn run_birthday_tick_with(app: &App, guild_id: &str, d: Arc<dyn BirthdayDiscord>) -> JobOutcome {
    if state(app).running.lock().unwrap_or_else(|e| e.into_inner()).contains(guild_id) {
        return later(app, CONTINUE_MS);
    }
    if !d.available() {
        return later(app, ERROR_RETRY_MS);
    }
    state(app).running.lock().unwrap_or_else(|e| e.into_inner()).insert(guild_id.to_string());
    let (a, g) = (app.clone(), guild_id.to_string());
    let r = state(app).guild_mutex.run(guild_id, move || async move { tick_guild(&a, d.as_ref(), &g, &a.clock().now()).await }).await;
    state(app).running.lock().unwrap_or_else(|e| e.into_inner()).remove(guild_id);
    match r {
        Ok(Some(next)) => JobOutcome::Reschedule { next_run_at: next, payload: None },
        Ok(None) => JobOutcome::Done,
        Err(e) => {
            bot_util::log_error!(format!("birthday.tick failed for guild {guild_id}"), bot_core::error::log_arg(&e));
            later(app, ERROR_RETRY_MS)
        }
    }
}

/// `runBirthdayTick(job, ctx)`: the `birthday.tick` job handler.
pub async fn run_birthday_tick(app: &App, guild_id: &str) -> JobOutcome {
    let Some(g) = snowflake(guild_id) else { return JobOutcome::Done };
    run_birthday_tick_with(app, guild_id, Arc::new(SerenityBirthdays::new(app, GuildId::new(g)))).await
}

struct Candidate {
    id: String,
    user_id: String,
}

/// `tickGuild(client, guild, { now })`: one tick for one guild: remove expired roles, claim and announce today's
/// birthdays, grant roles, keep roles of today's members. `Ok(None)` deletes the job (nothing configured, no marker).
pub async fn tick_guild(app: &App, d: &dyn BirthdayDiscord, guild_id: &str, now: &DateTime<Utc>) -> anyhow::Result<Option<DateTime<Utc>>> {
    // 1. Setup.
    let settings = app.settings().get_settings(guild_id).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let s: &GuildSettings = &settings;
    let tz = resolve_time_zone(Some(s));
    let parts = zoned_parts(now, &tz);
    let local = LocalDate::from(parts);
    let hour = hour_of(Some(s));
    let enabled = is_module_enabled(Some(s), Some(bot_config::ModuleKey::Birthdays));
    let started = Instant::now();
    let out_of_time = || started.elapsed().as_millis() as u64 >= TICK_DEADLINE_MS;
    let mut more = false;
    let pool = app.db();

    // 2. Resolve the configured channel and role.
    let mut channel: Option<(String, PlaceholderChannel)> = None;
    if let Some(cid) = set_id(&s.birthday_channel_id) {
        match d.channel(cid) {
            Some(c) => channel = Some((cid.to_string(), c)),
            None => warn_once(app, guild_id, "channel-deleted", &format!("birthdays: the birthday channel of guild {guild_id} was deleted")),
        }
    }
    let mut channel_ok = false;
    if let Some((cid, _)) = &channel {
        channel_ok = d.channel_usable(cid, &ANNOUNCE_NEEDS);
        if !channel_ok {
            warn_once(app, guild_id, "channel-perms", &format!("birthdays: cannot post in the birthday channel of guild {guild_id}"));
        }
    }
    let role_id = set_id(&s.birthday_role_id).map(str::to_string);
    let mut role: Option<String> = None;
    if let Some(r) = role_id.as_deref() {
        if d.role_exists(r) {
            role = Some(r.to_string());
        } else {
            warn_once(app, guild_id, "role-deleted", &format!("birthdays: the birthday role of guild {guild_id} was deleted"));
        }
    }
    // Re-checked every tick: a role that later gained dangerous permissions is never granted again.
    let can_grant = role.as_deref().is_some_and(|r| d.role_assignable(r, false));
    let can_remove = role.as_deref().is_some_and(|r| d.role_assignable(r, true));
    if role.is_some() && !can_grant {
        warn_once(app, guild_id, "role-perms", &format!("birthdays: cannot grant the birthday role of guild {guild_id}"));
    }

    // 3. Read.
    let rows = sqlx::query(r#"SELECT "id", "userId", "month", "day", "roleGrantedAt" FROM "Birthday" WHERE "guildId" = $1 AND "roleGrantedAt" IS NOT NULL ORDER BY "roleGrantedAt" ASC LIMIT $2"#)
        .bind(guild_id)
        .bind(MAX_MARKERS_PER_TICK)
        .fetch_all(pool)
        .await?;
    let markers: Vec<Marker> = rows
        .iter()
        .map(|r| {
            Ok(Marker {
                id: r.try_get("id")?,
                user_id: r.try_get("userId")?,
                month: r.try_get::<i32, _>("month")? as u32,
                day: r.try_get::<i32, _>("day")? as u32,
                role_granted_at: r.try_get::<chrono::NaiveDateTime, _>("roleGrantedAt")?.and_utc(),
            })
        })
        .collect::<Result<_, sqlx::Error>>()?;
    let (keep, expired) = partition_markers(&markers, enabled, role.as_deref(), local, &tz);

    // 4. Members. A failure propagates before anything was changed.
    let mut members: HashMap<String, BdMember> = HashMap::new();
    let mut asked: HashSet<String> = HashSet::new();
    let marker_ids: Vec<String> = keep.iter().chain(expired.iter()).map(|m| m.user_id.clone()).collect();
    async fn load(d: &dyn BirthdayDiscord, ids: Vec<String>, asked: &mut HashSet<String>, members: &mut HashMap<String, BdMember>) -> anyhow::Result<()> {
        let mut wanted: Vec<String> = Vec::new();
        for id in ids {
            if !asked.contains(&id) && !wanted.contains(&id) {
                wanted.push(id);
            }
        }
        if wanted.is_empty() {
            return Ok(());
        }
        for id in &wanted {
            asked.insert(id.clone());
        }
        let found = d.members(&wanted).await.map_err(|e| anyhow::anyhow!("member fetch failed: {}", e.message))?;
        members.extend(found);
        Ok(())
    }

    // Today's candidates, in pages by userId: members who left (or bots) are paged past, never claimed.
    let mut candidates: Vec<Candidate> = Vec::new();
    if enabled && (channel.is_some() || role.is_some()) && i64::from(parts.hour) >= hour {
        let matchers = today_matchers(local);
        let months: Vec<i32> = matchers.iter().map(|m| m.0 as i32).collect();
        let days: Vec<i32> = matchers.iter().map(|m| m.1 as i32).collect();
        let mut cursor: Option<String> = None;
        let mut first = true;
        'scan: loop {
            let page = sqlx::query(
                r#"SELECT "id", "userId", "month", "day", "year" FROM "Birthday"
                WHERE "guildId" = $1 AND ("month", "day") IN (SELECT * FROM unnest($2::int[], $3::int[]))
                  AND ("lastAnnouncedYear" IS NULL OR "lastAnnouncedYear" < $4)
                  AND ($5::text IS NULL OR "userId" > $5)
                ORDER BY "userId" ASC LIMIT $6"#,
            )
            .bind(guild_id)
            .bind(&months)
            .bind(&days)
            .bind(local.year as i32)
            .bind(cursor.as_deref())
            .bind((MAX_EVENTS_PER_TICK + 1) as i64)
            .fetch_all(pool)
            .await?;
            if page.is_empty() {
                break;
            }
            let page: Vec<Candidate> = page.iter().map(|r| Ok(Candidate { id: r.try_get("id")?, user_id: r.try_get("userId")? })).collect::<Result<_, sqlx::Error>>()?;
            cursor = page.last().map(|c| c.user_id.clone());
            let mut ids: Vec<String> = page.iter().map(|c| c.user_id.clone()).collect();
            if first {
                ids.extend(marker_ids.iter().cloned()); // one request for the first page
            }
            load(d, ids, &mut asked, &mut members).await?;
            first = false;
            let page_len = page.len();
            for row in page {
                let Some(m) = members.get(&row.user_id) else { continue };
                if m.bot {
                    continue;
                }
                if candidates.len() >= MAX_EVENTS_PER_TICK {
                    more = true;
                    break 'scan;
                }
                candidates.push(row);
            }
            if page_len <= MAX_EVENTS_PER_TICK {
                break; // the last page
            }
            if out_of_time() {
                more = true;
                break;
            }
        }
    }
    load(d, marker_ids.clone(), &mut asked, &mut members).await?;

    // 5. Expired markers: remove the role (also while the module is off).
    for m in &expired {
        if out_of_time() {
            more = true;
            break;
        }
        let member = members.get(&m.user_id);
        let (Some(member), Some(r)) = (member, role.as_deref()) else {
            clear_marker(pool, m).await?;
            continue;
        };
        if !can_remove {
            if now.timestamp_millis() - m.role_granted_at.timestamp_millis() > MARKER_GIVE_UP_MS {
                clear_marker(pool, m).await?;
                bot_util::log_warn!(format!("birthdays: gave up removing the birthday role from {} in guild {guild_id}", m.user_id));
            }
            continue;
        }
        if !member.roles.iter().any(|x| x == r) {
            clear_marker(pool, m).await?;
            continue;
        }
        if take_role(app, d, guild_id, &m.user_id, r, "Birthday is over").await != Take::Keep {
            clear_marker(pool, m).await?;
        }
    }

    // 6. Claim today's celebrations.
    let mut claimed: Vec<Claimed> = Vec::new();
    if !candidates.is_empty() {
        let ids: Vec<String> = candidates.iter().map(|c| c.id.clone()).collect();
        claimed = claim_birthdays(pool, guild_id, &ids, local.year, role.is_some()).await?;
    }

    // 7. Announce. Without a usable channel the claims are completed silently (the role is still granted).
    let mut completed: Vec<Claimed> = claimed.clone();
    if let (false, Some((cid, chan)), true) = (claimed.is_empty(), &channel, channel_ok) {
        completed = Vec::new();
        let guild = d.guild();
        let entries: Vec<AnnounceEntry> = claimed
            .iter()
            .map(|r| {
                let age = age_on(r.year, local.year);
                let ctx = line_context(members.get(&r.user_id), guild.clone(), Some(chan.clone()));
                AnnounceEntry { id: r.id.clone(), user_id: r.user_id.clone(), text: render_birthday_line(&pick_template(Some(s), age), ctx, age) }
            })
            .collect();
        let chunks = chunk_announcements(&entries, MAX_MESSAGE_CHARS, MAX_USERS_PER_MESSAGE);
        for (i, chunk) in chunks.iter().enumerate() {
            let failure: Option<Option<DiscordFail>> = if out_of_time() {
                more = true;
                Some(None)
            } else {
                d.send(cid, &chunk.content, &chunk.user_ids).await.err().map(Some)
            };
            let Some(failure) = failure else {
                completed.extend(claimed.iter().filter(|r| chunk.ids.contains(&r.id)).cloned());
                continue;
            };
            if let Some(f) = failure {
                if matches!(f.code, Some(50013 | 50001 | 10003)) {
                    warn_once(app, guild_id, "channel-perms", &format!("birthdays: cannot post in the birthday channel of guild {guild_id} ({})", f.code.unwrap_or(0)));
                } else {
                    bot_util::log_error!(format!("birthdays: announcing in guild {guild_id} failed"), f.message);
                }
            }
            // Revert this chunk and every unsent one; the next run claims them again.
            let ids: Vec<String> = chunks[i..].iter().flat_map(|c| c.ids.clone()).collect();
            revert_claims(pool, guild_id, &ids, local.year).await;
            break;
        }
    }

    // 8. Tombstones for completed rows (fail-open).
    let ttl = tombstone_ttl_seconds(now, &tz);
    for r in &completed {
        app.cache().set(&tombstone_key(guild_id, &r.user_id), &local.year, ttl).await;
    }

    // 9 + 10. Grant the role to claimed rows (completed or reverted), then keep it on today's other celebrants.
    if can_grant && let Some(r) = role.as_deref() {
        let mut granted: HashSet<String> = HashSet::new();
        let ensure = async |user: &str, granted: &mut HashSet<String>| {
            let Some(m) = members.get(user) else { return };
            if !granted.insert(user.to_string()) {
                return;
            }
            if m.roles.iter().any(|x| x == r) || m.pending {
                return;
            }
            grant_role(app, d, guild_id, user, r).await;
        };
        for c in &claimed {
            if out_of_time() {
                more = true;
                break;
            }
            ensure(&c.user_id, &mut granted).await;
        }
        for m in &keep {
            if out_of_time() {
                more = true;
                break;
            }
            if !members.contains_key(&m.user_id) {
                clear_marker(pool, m).await?;
                continue;
            }
            ensure(&m.user_id, &mut granted).await;
        }
    }

    // 11. Next run.
    let real_now = app.clock().now();
    if more {
        return Ok(Some(real_now + Duration::milliseconds(CONTINUE_MS)));
    }
    if set_id(&s.birthday_channel_id).is_none() && set_id(&s.birthday_role_id).is_none() && !has_markers(pool, guild_id).await? {
        return Ok(None);
    }
    Ok(Some(next_tick_at(&real_now, &tz)))
}
