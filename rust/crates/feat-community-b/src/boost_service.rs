//! Port of `src/services/boostService.js`: Server Boosts. A thank-you when a member boosts the server, and an optional
//! role of their own for boosters (`/boost role`). This file holds the rules, the `BoosterRole` table access, the
//! Discord role calls, the hook bodies and the `boosts.sweep` job; the hooks are declared in
//! [`crate::plugins::boosts`] and the command is [`crate::commands::boost`].
//!
//! - Boost start / end come from `guildMemberUpdate` (old vs new `premiumSince`); a partial old member is never diffed.
//! - A booster role is created (permissions 0, not hoisted, not mentionable), moved just below the anchor role (if set),
//!   else just above the server's Nitro Booster role, else left at the bottom, and given to the member. Any step
//!   failing after the create deletes the role again.
//! - `boosts.sweep` is the repair path: it deletes roles whose grace period is over or whose owner left, and fixes rows
//!   whose member stopped or resumed boosting while the bot was not looking. It keeps cleaning while the module is off.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::Engine;
use bot_config::{JobType, ModuleKey, limits};
use bot_core::App;
use bot_core::embed_builder::{error_embed, warn_embed};
use bot_core::hooks::HookCtx;
use bot_core::hooks::events::{ChannelDelete, GuildMemberRemove, GuildMemberUpdate, GuildReady, RoleDelete};
use bot_core::interaction_helpers::{cached_guild, check_channel_usable};
use bot_core::permissions::{self, guild_permissions, highest_position, js_has, role_position};
use bot_core::scheduler::{Job, JobCtx, JobOutcome, OnConflict, ScheduleJob};
use bot_core::settings::{SettingsPatch, is_module_enabled};
use bot_db::{BOOSTER_ROLE_INFO, BoosterRole, GuildSettings, InsertBuilder, PgPool, SelectBuilder, SqlValue, UpdateBuilder};
use bot_util::jsre::LazyRe;
use bot_util::lazy_re;
use bot_util::placeholder_parser::{PlaceholderGuild, PlaceholderMember, RenderContext, VarValue, render_placeholders};
use bot_util::text::clamp_text_with;
use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use serenity::all::{ChannelId, Guild, GuildId, Member, Permissions, Role, RoleId, UserId};
use serenity::builder::{CreateAllowedMentions, CreateEmbed, CreateMessage};

/// `BOOST`.
pub mod consts {
    pub const JOB_TYPE: &str = "boosts.sweep";
    pub const DAY_MS: i64 = 86_400_000;
    pub const GRACE_MAX_DAYS: i64 = 30;
    pub const SWEEP_EVERY_MS: i64 = 3_600_000;
    pub const SWEEP_MORE_MS: i64 = 60_000;
    pub const SWEEP_RETRY_MS: i64 = 5 * 60_000;
    pub const STARTUP_DELAY_MS: i64 = 60_000;
    pub const STARTUP_JITTER_MS: i64 = 120_000;
    pub const LOOKUPS_PER_RUN: usize = 50;
    pub const DELETES_PER_RUN: usize = 25;
    pub const LOOKUP_TIMEOUT_MS: u64 = 8_000;
    pub const SWEEP_BUDGET_MS: u64 = 35_000;
    pub const ROLES_PER_GUILD: usize = 250;
    pub const EDIT_COOLDOWN_MS: i64 = 60_000;
    pub const COOLDOWN_MAX: usize = 5_000;
    pub const ICON_TIMEOUT_MS: u64 = 8_000;
    pub const EMOJI_MAX: usize = 32;
    pub const MESSAGE_MAX: usize = 2000;
    pub const REASON_MAX: usize = 400;
}

/// The thank-you used while `boostMessage` is null (`DEFAULT_TEXTS.boostMessage`).
pub const DEFAULT_BOOST_MESSAGE: &str = "💎 {user} just boosted **{server}**! Thank you! We now have {boosts} boosts.";

const LABEL: &str = "Server Boosts";

/// The icon download (url -> bytes), replaceable in tests (`__setIconFetch`).
pub type IconFetch = Arc<dyn Fn(String) -> futures::future::BoxFuture<'static, Result<Vec<u8>, IconFetchError>> + Send + Sync>;

/// Why an icon download failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IconFetchError {
    /// An `IconProblem` with this text.
    Problem(String),
    /// `TOO_LARGE`.
    TooLarge,
    /// Anything else.
    Other,
}

/// Per-app state: the member lock, the edit cooldown, the sweep offsets, the warn throttles and the icon fetch.
pub struct BoostState {
    lock: bot_core::sync::KeyedMutex,
    last_edit: Mutex<IndexMap<String, i64>>,
    lookup_offset: Mutex<HashMap<String, usize>>,
    warn_blocked: bot_util::async_util::Throttle,
    warn_channel: bot_util::async_util::Throttle,
    icon_fetch: Mutex<Option<IconFetch>>,
}

impl Default for BoostState {
    fn default() -> Self {
        Self {
            lock: bot_core::sync::KeyedMutex::new(),
            last_edit: Mutex::new(IndexMap::new()),
            lookup_offset: Mutex::new(HashMap::new()),
            warn_blocked: bot_util::async_util::create_throttle(3_600_000),
            warn_channel: bot_util::async_util::create_throttle(3_600_000),
            icon_fetch: Mutex::new(None),
        }
    }
}

/// `runLocked(guildId, userId, fn)`: one member's create / edit / delete / end / leave run one at a time.
pub fn run_locked<T, F, Fut>(app: &App, guild_id: &str, user_id: &str, f: F) -> impl std::future::Future<Output = T> + Send + 'static + use<T, F, Fut>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    app.ext::<BoostState>().lock.run(&format!("{guild_id}:{user_id}"), f)
}

/// `reason(text)`: an audit log reason.
pub fn reason(text: &str) -> String {
    clamp_text_with(&format!("{LABEL}: {text}"), consts::REASON_MAX, "")
}

// ── Pure rules ────────────────────────────────────────────────────────────────

/// `isBoosting(member)`.
pub fn is_boosting(member: Option<&Member>) -> bool {
    member.and_then(|m| m.premium_since).is_some_and(|t| t.unix_timestamp() * 1000 + i64::from(t.timestamp_subsec_millis()) > 0)
}

/// A change of a member's boost state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    Start,
    End,
}

/// `boostTransition(oldMember, newMember)`: 'start', 'end' or none.
pub fn boost_transition(before: bool, after: bool) -> Option<Transition> {
    match (before, after) {
        (false, true) => Some(Transition::Start),
        (true, false) => Some(Transition::End),
        _ => None,
    }
}

/// `graceMs(settings)`: milliseconds a member keeps the role after the boost ended (0..30 days).
pub fn grace_ms(settings: Option<&GuildSettings>) -> i64 {
    let d = settings.map(|s| i64::from(s.boost_role_grace_days)).unwrap_or(0);
    if d <= 0 { 0 } else { d.min(consts::GRACE_MAX_DAYS) * consts::DAY_MS }
}

/// `tierNumber(guild)`: the server's boost tier, 0..3.
pub fn tier_number(guild: Option<&Guild>) -> u8 {
    guild.map(|g| u8::from(g.premium_tier).min(3)).unwrap_or(0)
}

/// `messageTemplate(settings)`: the configured text, or the default when it is null or blank.
pub fn message_template(settings: Option<&GuildSettings>) -> String {
    match settings.and_then(|s| s.boost_message.as_deref()) {
        Some(own) if !bot_util::js::trim(own).is_empty() => own.to_string(),
        _ => DEFAULT_BOOST_MESSAGE.to_string(),
    }
}

/// What [`render_thank_you`] renders with.
#[derive(Debug, Clone, Default)]
pub struct ThankYouSource {
    pub member: Option<PlaceholderMember>,
    pub guild: Option<PlaceholderGuild>,
    /// `guild.premiumSubscriptionCount`.
    pub boosts: u64,
    pub tier: u8,
}

impl ThankYouSource {
    /// From the cached guild and a member.
    pub fn of(guild: Option<&Guild>, member: Option<&Member>) -> Self {
        Self {
            member: member.map(|m| bot_core::adapters::placeholder_member(m, guild)),
            guild: guild.map(bot_core::adapters::placeholder_guild),
            boosts: guild.and_then(|g| g.premium_subscription_count).unwrap_or(0),
            tier: tier_number(guild),
        }
    }
}

/// `renderThankYou(settings, member, guild, { maxLength, preview })`: the thank-you, placeholders rendered (`{boosts}`
/// = boost count, `{tier}` = 0..3). A preview counts one boost more than the server has.
pub fn render_thank_you(settings: Option<&GuildSettings>, src: &ThankYouSource, max_length: usize, preview: bool) -> String {
    let render = |tpl: &str| {
        let ctx = RenderContext {
            member: src.member.clone(),
            guild: src.guild.clone(),
            vars: Some(vec![
                ("boosts".into(), VarValue::Number(src.boosts as f64 + if preview { 1.0 } else { 0.0 })),
                ("tier".into(), VarValue::Number(f64::from(src.tier))),
            ]),
            ..Default::default()
        };
        render_placeholders(tpl, &ctx, Some(max_length))
    };
    let text = render(&message_template(settings));
    let trimmed = bot_util::js::trim(&text);
    if trimmed.is_empty() { render(DEFAULT_BOOST_MESSAGE) } else { trimmed.to_string() }
}

/// A colour that is not `#rrggbb` (`parseColor` answered `{ ok: false }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadColor;

/// `parseColor(raw)`: `#rrggbb` (the # is optional) to an integer; `Ok(None)` for null/blank, `Err` when invalid.
pub fn parse_color(raw: Option<&str>) -> Result<Option<u32>, BadColor> {
    let Some(s) = raw.map(bot_util::js::trim).filter(|s| !s.is_empty()) else { return Ok(None) };
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        u32::from_str_radix(hex, 16).map(Some).map_err(|_| BadColor)
    } else {
        Err(BadColor)
    }
}

const PICTO: &str = r"\p{Extended_Pictographic}(?:\ufe0f|\p{Emoji_Modifier}|[\u{E0020}-\u{E007F}])*";
static EMOJI_RE: LazyRe = std::sync::LazyLock::new(|| {
    bot_util::jsre::JsRegex::new(&format!(r"^(?:{PICTO}(?:\u200d{PICTO})*|\p{{Regional_Indicator}}{{2}}|[0-9#*]\ufe0f?\u20e3)$"), "u")
});

/// `parseEmoji(raw)`: one standard emoji, or `None`.
pub fn parse_emoji(raw: Option<&str>) -> Option<String> {
    let s = bot_util::js::trim(raw.unwrap_or(""));
    if s.is_empty() || bot_util::js::utf16_len(s) > consts::EMOJI_MAX || !EMOJI_RE.test(s) {
        return None;
    }
    Some(s.to_string())
}

static INVISIBLE_RE: LazyRe = lazy_re!(r"[\u0000-\u001f\u007f-\u009f\u00ad\u200b-\u200f\u2028-\u202e\u2060-\u206f\ufeff]", "g");
static AT_RE: LazyRe = lazy_re!(r"[@\uff20\ufe6b]", "");
static LINK_RE: LazyRe = lazy_re!(
    r"(?:https?:\/\/|www\.|\bdiscord(?:app)?\.(?:gg|com)\b|\b[a-z0-9-]+\.(?:com|net|org|gg|io|me|tv|xyz|co|app|dev|ly|to|gl|link|info|biz|us|uk|de|ru)\b)",
    "i"
);
static SPACES_RE: LazyRe = lazy_re!(r"\s+", "g");
static NON_WORD_RE: LazyRe = lazy_re!(r"[^\p{L}\p{N}]+", "gu");

fn fold(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    bot_util::js::lower(&s.nfkc().collect::<String>())
}

fn skeleton(s: &str) -> String {
    NON_WORD_RE.replace_all(&fold(s), "")
}

/// Why a booster role name was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameReason {
    Empty,
    TooLong,
    At,
    Link,
    Taken,
}

impl NameReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too-long",
            Self::At => "at",
            Self::Link => "link",
            Self::Taken => "taken",
        }
    }

    /// `NAME_REASONS[reason]`.
    pub fn text(self) -> String {
        match self {
            Self::Empty => "Give the role a name.".into(),
            Self::TooLong => format!("Role names are at most {} characters.", limits::BOOSTER_ROLE_NAME),
            Self::At => "A role name cannot contain `@`.".into(),
            Self::Link => "A role name cannot contain a link or an invite.".into(),
            Self::Taken => "A role with that name already exists. Pick a different name.".into(),
        }
    }
}

/// `validateRoleName(raw, guild, { exceptRoleId })`: trimmed (invisible characters dropped, spaces collapsed),
/// 1..BOOSTER_ROLE_NAME characters, no @, no links or invites, and not the name of an existing role (case-insensitive,
/// also when only punctuation, spaces or look-alike width forms differ). `roles` are `(id, name)` of the guild.
pub fn validate_role_name<'a>(raw: Option<&str>, roles: impl IntoIterator<Item = (&'a str, &'a str)>, except_role_id: Option<&str>) -> Result<String, NameReason> {
    let cleaned = INVISIBLE_RE.replace_all(raw.unwrap_or(""), "");
    let collapsed = SPACES_RE.replace_all(&cleaned, " ");
    let name = bot_util::js::trim(&collapsed).to_string();
    if name.is_empty() {
        return Err(NameReason::Empty);
    }
    if bot_util::js::utf16_len(&name) > limits::BOOSTER_ROLE_NAME {
        return Err(NameReason::TooLong);
    }
    if AT_RE.test(&name) {
        return Err(NameReason::At);
    }
    if LINK_RE.test(&name) {
        return Err(NameReason::Link);
    }
    let folded = fold(&name);
    let skel = skeleton(&name);
    for (id, other) in roles {
        if Some(id) == except_role_id {
            continue;
        }
        if fold(other) == folded || (!skel.is_empty() && skeleton(other) == skel) {
            return Err(NameReason::Taken);
        }
    }
    Ok(name)
}

/// What the sweep knows about a row's member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberState {
    /// Could not be looked up (`undefined`): never act on it.
    Unknown,
    /// Left the server (`null`).
    Left,
    /// In the server.
    Present { boosting: bool },
}

/// What the sweep does with one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    None,
    Skip,
    Delete(&'static str),
    Ended,
    Resumed,
}

impl Plan {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Skip => "skip",
            Self::Delete(_) => "delete",
            Self::Ended => "ended",
            Self::Resumed => "resumed",
        }
    }
}

/// `planSweep(row, member, settings, now)`.
pub fn plan_sweep(boost_ended_at_ms: Option<i64>, member: MemberState, settings: Option<&GuildSettings>, now_ms: i64) -> Plan {
    let boosting = match member {
        MemberState::Unknown => return Plan::Skip,
        MemberState::Left => return Plan::Delete("the member left the server"),
        MemberState::Present { boosting } => boosting,
    };
    if boosting {
        return if boost_ended_at_ms.is_none() { Plan::None } else { Plan::Resumed };
    }
    let grace = grace_ms(settings);
    match boost_ended_at_ms {
        None => {
            if grace == 0 {
                Plan::Delete("the boost ended")
            } else {
                Plan::Ended
            }
        }
        Some(ended) => {
            if ended + grace <= now_ms {
                Plan::Delete("the grace period after the boost is over")
            } else {
                Plan::None
            }
        }
    }
}

// ── Table ─────────────────────────────────────────────────────────────────────

/// `countRows`.
pub const COUNT_ROWS_SQL: &str = r#"SELECT COUNT(*)::bigint AS n FROM "BoosterRole" WHERE "guildId" = $1"#;
/// `clearEnded` (`updateMany` with `boostEndedAt: { not: null }`).
pub const CLEAR_ENDED_SQL: &str = r#"UPDATE "BoosterRole" SET "boostEndedAt" = NULL, "updatedAt" = $3 WHERE "guildId" = $1 AND "userId" = $2 AND "boostEndedAt" IS NOT NULL"#;
/// The raw statements of this module (run in a rolled-back transaction by the tests).
pub const RAW_SQL: [(&str, &str); 2] = [("countRows", COUNT_ROWS_SQL), ("clearEnded", CLEAR_ENDED_SQL)];

/// `loadRow(guildId, userId)`.
pub async fn load_row(pool: &PgPool, guild_id: &str, user_id: &str) -> anyhow::Result<Option<BoosterRole>> {
    let q = SelectBuilder::new(&BOOSTER_ROLE_INFO).filter("guildId", json!(guild_id))?.filter("userId", json!(user_id))?.build()?;
    Ok(q.fetch_optional(pool).await?)
}

/// `listRows(guildId)`: every row of a guild, oldest first.
pub async fn list_rows(pool: &PgPool, guild_id: &str) -> anyhow::Result<Vec<BoosterRole>> {
    let q = SelectBuilder::new(&BOOSTER_ROLE_INFO).filter("guildId", json!(guild_id))?.order_by("createdAt", true)?.limit(500).build()?;
    Ok(q.fetch_all(pool).await?)
}

/// `countRows(guildId)`.
pub async fn count_rows(pool: &PgPool, guild_id: &str) -> anyhow::Result<i64> {
    let (n,): (i64,) = bot_db::sqlx::query_as(COUNT_ROWS_SQL).bind(guild_id).fetch_one(pool).await.map_err(bot_db::DbError::from)?;
    Ok(n)
}

/// `deleteRow(guildId, userId)`: how many went.
pub async fn delete_row(pool: &PgPool, guild_id: &str, user_id: &str) -> anyhow::Result<u64> {
    Ok(bot_db::delete_where(&BOOSTER_ROLE_INFO, [("guildId", json!(guild_id)), ("userId", json!(user_id))])?.execute(pool).await?)
}

/// `deleteRowsByRole(guildId, roleId)`.
pub async fn delete_rows_by_role(pool: &PgPool, guild_id: &str, role_id: &str) -> anyhow::Result<u64> {
    Ok(bot_db::delete_where(&BOOSTER_ROLE_INFO, [("guildId", json!(guild_id)), ("roleId", json!(role_id))])?.execute(pool).await?)
}

/// `markEnded(guildId, userId, when)`: sets `boostEndedAt` (only when it is not set yet).
pub async fn mark_ended(pool: &PgPool, guild_id: &str, user_id: &str, when: DateTime<Utc>) -> anyhow::Result<u64> {
    let q = UpdateBuilder::new(&BOOSTER_ROLE_INFO)
        .set_typed("boostEndedAt", SqlValue::timestamp(when))?
        .filter("guildId", json!(guild_id))?
        .filter("userId", json!(user_id))?
        .filter("boostEndedAt", Value::Null)?
        .without_returning()
        .build()?;
    Ok(q.execute(pool).await?)
}

/// `clearEnded(guildId, userId)`: clears `boostEndedAt` (the member boosts again; the role was kept).
pub async fn clear_ended(pool: &PgPool, guild_id: &str, user_id: &str) -> anyhow::Result<u64> {
    let res = bot_db::sqlx::query(CLEAR_ENDED_SQL)
        .bind(guild_id)
        .bind(user_id)
        .bind(bot_db::now_utc().naive_utc())
        .execute(pool)
        .await
        .map_err(bot_db::DbError::from)?;
    Ok(res.rows_affected())
}

/// `prisma.boosterRole.create({ data: { guildId, userId, roleId } })`.
pub async fn insert_row(pool: &PgPool, guild_id: &str, user_id: &str, role_id: &str) -> Result<BoosterRole, bot_db::DbError> {
    InsertBuilder::new(&BOOSTER_ROLE_INFO)
        .values([("guildId", json!(guild_id)), ("userId", json!(user_id)), ("roleId", json!(role_id))])?
        .build()?
        .fetch_one(pool)
        .await
}

// ── Scheduling ────────────────────────────────────────────────────────────────

/// `sweepKey(guildId)`: the sweep job's dedupe key.
pub fn sweep_key(guild_id: &str) -> String {
    format!("boosts.sweep:{guild_id}")
}

/// `ensureSweepJob(guildId, runAt)`: makes sure the recurring sweep exists. A live job is left alone ('keep'); a
/// missing, DONE or FAILED one is created again. `None` runs it in an hour. Never fails.
pub async fn ensure_sweep_job(app: &App, guild_id: &str, run_at: Option<DateTime<Utc>>) -> bool {
    let run_at = run_at.unwrap_or_else(|| app.clock().now() + chrono::Duration::milliseconds(consts::SWEEP_EVERY_MS));
    let job = ScheduleJob::new(guild_id, JobType::BoostsSweep, run_at).payload(json!({})).dedupe_key(sweep_key(guild_id)).on_conflict(OnConflict::Keep);
    match app.scheduler().schedule_job(job).await {
        Ok(_) => true,
        Err(e) => {
            bot_util::log_error!(format!("boosts: could not schedule the sweep of guild {guild_id}"), bot_core::error::log_arg(&e));
            false
        }
    }
}

/// `sweepJob(guildId)`: the guild's sweep job row.
pub async fn sweep_job(app: &App, guild_id: &str) -> Option<bot_db::ScheduledJob> {
    app.scheduler().get_job(&sweep_key(guild_id)).await
}

// ── Discord calls ─────────────────────────────────────────────────────────────

/// What deleting a role came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveOutcome {
    Deleted,
    Gone,
    Forbidden,
    Error,
}

fn parse_id(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|v| *v != 0)
}

/// `removeRole(guild, roleId, why)`: deletes a role by id. Never fails; @everyone is never touched.
pub async fn remove_role(app: &App, guild_id: GuildId, role_id: &str, why: &str) -> RemoveOutcome {
    let Some(rid) = parse_id(role_id).filter(|r| *r != guild_id.get()) else { return RemoveOutcome::Gone };
    let Some(http) = app.http() else { return RemoveOutcome::Error };
    match http.delete_role(guild_id, RoleId::new(rid), Some(why)).await {
        Ok(()) => RemoveOutcome::Deleted,
        Err(e) => match bot_core::discord::error_code(&e) {
            Some(10011) => RemoveOutcome::Gone,
            Some(50013) => RemoveOutcome::Forbidden,
            _ => {
                bot_util::log_warn!(format!("[boosts] {guild_id}: could not delete role {role_id}: {}", bot_core::discord::error_message(&e)));
                RemoveOutcome::Error
            }
        },
    }
}

/// `removeBoosterRole(guild, row, why)`: deletes a booster's role and then their row. A role that is already gone
/// counts as deleted; when the role cannot be deleted the row is kept, so the sweep tries again. Never fails.
pub async fn remove_booster_role(app: &App, guild_id: GuildId, row: &BoosterRole, why: &str) -> RemoveOutcome {
    let res = remove_role(app, guild_id, &row.role_id, why).await;
    if res != RemoveOutcome::Deleted && res != RemoveOutcome::Gone {
        if res == RemoveOutcome::Forbidden && app.ext::<BoostState>().warn_blocked.allow(&guild_id.to_string()) {
            bot_util::log_warn!(format!("[boosts] {guild_id}: cannot delete booster roles, the bot lacks Manage Roles or they are above its highest role"));
        }
        return res;
    }
    if let Err(e) = delete_row(app.db(), &guild_id.to_string(), &row.user_id).await {
        bot_util::log_error!(format!("[boosts] {guild_id}: could not delete the row of {}", row.user_id), bot_core::error::log_arg(&e));
        return RemoveOutcome::Error;
    }
    res
}

/// `nitroBoosterRole(guild)`: the server's Nitro Booster role (tagged `premiumSubscriberRole`).
pub fn nitro_booster_role(guild: &Guild) -> Option<&Role> {
    let mut tagged: Vec<&Role> = guild.roles.values().filter(|r| r.tags.premium_subscriber).collect();
    tagged.sort_by_key(|r| r.id);
    tagged.into_iter().next()
}

/// Where a new booster role goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementKind {
    Anchor,
    Nitro,
    Bottom,
}

/// `placementFor`'s answer: the kind, the role it is placed by, and the discord.js position to move to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub kind: PlacementKind,
    pub base: Option<RoleId>,
    pub target: usize,
}

/// `placementFor(guild, settings)`: just below the anchor role when one is set; otherwise just ABOVE the Nitro
/// Booster role; with neither, at the bottom (position 1). Never at or above the bot's highest role. Needs Manage
/// Roles and a highest role of position 2 or more. `Err` is `BOT_PERMS`, `BOT_LOW` or `ANCHOR_HIGH`.
pub fn placement_for(guild: &Guild, bot_id: UserId, settings: Option<&GuildSettings>) -> Result<Placement, &'static str> {
    let Some(me) = permissions::me(guild, bot_id) else { return Err("BOT_PERMS") };
    if !js_has(guild_permissions(guild, bot_id, &me.roles), Permissions::MANAGE_ROLES) {
        return Err("BOT_PERMS");
    }
    let top = highest_position(guild, &me.roles);
    if top < 2 {
        return Err("BOT_LOW");
    }
    let anchor = settings
        .and_then(|s| s.boost_role_anchor_id.as_deref())
        .and_then(parse_id)
        .filter(|id| *id != guild.id.get())
        .map(RoleId::new)
        .filter(|id| guild.roles.contains_key(id));
    if let Some(a) = anchor {
        let pos = role_position(guild, a).unwrap_or(0);
        if pos > top {
            return Err("ANCHOR_HIGH");
        }
        return Ok(Placement { kind: PlacementKind::Anchor, base: Some(a), target: pos.saturating_sub(1) });
    }
    if let Some(n) = nitro_booster_role(guild) {
        let pos = role_position(guild, n.id).unwrap_or(0);
        return Ok(Placement { kind: PlacementKind::Nitro, base: Some(n.id), target: pos.min(top - 1) });
    }
    Ok(Placement { kind: PlacementKind::Bottom, base: None, target: 1 })
}

/// discord.js `Role#editable`: not managed, the bot has Manage Roles and its highest role is above this one.
pub fn role_editable(guild: &Guild, bot_id: UserId, role_id: RoleId) -> bool {
    let Some(role) = guild.roles.get(&role_id) else { return false };
    if role.managed {
        return false;
    }
    let Some(me) = permissions::me(guild, bot_id) else { return false };
    if !js_has(guild_permissions(guild, bot_id, &me.roles), Permissions::MANAGE_ROLES) {
        return false;
    }
    highest_position(guild, &me.roles) > role_position(guild, role_id).unwrap_or(usize::MAX)
}

fn bot_id(app: &App) -> Option<UserId> {
    app.discord().map(|d| d.bot_user_id())
}

/// `moveToSpot(guild, role, settings, why)`: moves a freshly created role to its spot, read from fresh positions.
/// False when it could not be moved. Like discord.js `role.setPosition(target, { reason })`, it PATCHes the whole
/// re-ordered role list (`Util.setPosition`).
async fn move_to_spot(app: &App, guild_id: GuildId, role: &Role, settings: Option<&GuildSettings>, why: &str) -> bool {
    let Some(http) = app.http() else { return false };
    let Some(mut guild) = cached_guild(app, guild_id) else { return false };
    match http.get_guild_roles(guild_id).await {
        Ok(roles) => guild.roles = roles.into_iter().map(|r| (r.id, r)).collect(),
        Err(e) => bot_util::log_debug!(format!("[boosts] {guild_id}: could not refresh the roles before moving: {}", bot_core::discord::error_message(&e))),
    }
    // discord.js added the created role to its cache (`roles._add`); the gateway may not have told serenity yet.
    guild.roles.entry(role.id).or_insert_with(|| role.clone());
    let Some(bot) = bot_id(app) else { return false };
    let Ok(spot) = placement_for(&guild, bot, settings) else { return false };
    if spot.target <= 1 {
        return true;
    }
    let raw: Vec<(RoleId, u16)> = guild.roles.values().map(|r| (r.id, r.position)).collect();
    let body: Vec<(RoleId, u16)> = role_positions_body(&raw, role.id, spot.target).into_iter().map(|(id, i)| (id, u16::try_from(i).unwrap_or(u16::MAX))).collect();
    match bot_core::discord::set_role_positions(&http, guild_id, &body, Some(why)).await {
        Ok(_) => true,
        Err(e) => {
            bot_util::log_warn!(format!("[boosts] {guild_id}: could not move booster role {}: {}", role.id, bot_core::discord::error_message(&e)));
            false
        }
    }
}

/// `Util.setPosition`'s body: the roles sorted as discord.js sorts them (raw position, then the newer role lower),
/// `role_id` moved to `target`, every role with its new index.
pub fn role_positions_body(roles: &[(RoleId, u16)], role_id: RoleId, target: usize) -> Vec<(RoleId, usize)> {
    let mut sorted: Vec<(RoleId, u16)> = roles.to_vec();
    sorted.sort_by(|a, b| a.1.cmp(&b.1).then(b.0.get().cmp(&a.0.get())));
    let mut ids: Vec<RoleId> = sorted.into_iter().map(|(id, _)| id).collect();
    if let Some(index) = ids.iter().position(|r| *r == role_id)
        && target < ids.len()
    {
        let r = ids.remove(index);
        ids.insert(target, r);
    }
    ids.into_iter().enumerate().map(|(i, id)| (id, i)).collect()
}

// ── Cooldown ──────────────────────────────────────────────────────────────────

/// `cooldownLeftMs(guildId, userId)`: milliseconds until the member may create or edit again (0 = now).
pub fn cooldown_left_ms(app: &App, guild_id: &str, user_id: &str) -> i64 {
    let now = app.clock().now_ms();
    let map = app.ext::<BoostState>();
    let g = map.last_edit.lock().unwrap_or_else(|e| e.into_inner());
    g.get(&format!("{guild_id}:{user_id}")).map(|at| (at + consts::EDIT_COOLDOWN_MS - now).max(0)).unwrap_or(0)
}

/// `noteEdit(guildId, userId)`: starts the member's cooldown (when a Discord call is about to be made).
pub fn note_edit(app: &App, guild_id: &str, user_id: &str) {
    let now = app.clock().now_ms();
    let state = app.ext::<BoostState>();
    let mut g = state.last_edit.lock().unwrap_or_else(|e| e.into_inner());
    if g.len() >= consts::COOLDOWN_MAX {
        g.retain(|_, t| now - *t < consts::EDIT_COOLDOWN_MS);
        if g.len() >= consts::COOLDOWN_MAX {
            g.shift_remove_index(0);
        }
    }
    g.insert(format!("{guild_id}:{user_id}"), now);
}

/// The number of cooldown entries (tests).
pub fn cooldown_entries(app: &App) -> usize {
    app.ext::<BoostState>().last_edit.lock().unwrap_or_else(|e| e.into_inner()).len()
}

/// `resetCooldowns()` (tests).
pub fn reset_cooldowns(app: &App) {
    app.ext::<BoostState>().last_edit.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

// ── Icons ─────────────────────────────────────────────────────────────────────

fn is_png(b: &[u8]) -> bool {
    b.len() > 8 && b[0] == 0x89 && b[1] == 0x50 && b[2] == 0x4e && b[3] == 0x47
}

fn is_jpeg(b: &[u8]) -> bool {
    b.len() > 3 && b[0] == 0xff && b[1] == 0xd8 && b[2] == 0xff
}

fn icon_kb() -> usize {
    limits::BOOSTER_ROLE_ICON_BYTES / 1000
}

fn bad_icon_text() -> String {
    format!("The icon must be a PNG or JPEG image of at most {} KB.", icon_kb())
}

fn too_big_text() -> String {
    format!("The icon is too big: at most {} KB.", icon_kb())
}

/// `__setIconFetch(fn)`: replaces the download; `None` restores the real one (tests).
pub fn set_icon_fetch(app: &App, f: Option<IconFetch>) {
    *app.ext::<BoostState>().icon_fetch.lock().unwrap_or_else(|e| e.into_inner()) = f;
}

async fn default_icon_fetch(url: String) -> Result<Vec<u8>, IconFetchError> {
    let opts = bot_util::safe_fetch::FetchOptions {
        headers: vec![("accept".into(), "image/png,image/jpeg".into())],
        timeout_ms: consts::ICON_TIMEOUT_MS,
        max_bytes: limits::BOOSTER_ROLE_ICON_BYTES as u64,
        ..Default::default()
    };
    match bot_util::safe_fetch::safe_fetch(&url, &opts).await {
        Ok(r) if r.ok => Ok(r.buffer),
        Ok(_) => Err(IconFetchError::Problem("The image could not be downloaded.".into())),
        Err(e) if e.code == bot_util::safe_fetch::SafeFetchCode::TooLarge => Err(IconFetchError::TooLarge),
        Err(_) => Err(IconFetchError::Other),
    }
}

/// An icon attachment (`options.getAttachment('icon_image')`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconAttachment {
    pub url: String,
    pub content_type: Option<String>,
    pub size: u64,
}

impl IconAttachment {
    pub fn of(a: &serenity::all::Attachment) -> Self {
        Self { url: a.url.clone(), content_type: a.content_type.clone(), size: u64::from(a.size) }
    }
}

/// `iconAttachmentProblem(att)`: checked before the download (type, size).
pub fn icon_attachment_problem(att: &IconAttachment) -> Option<String> {
    let ty = att.content_type.as_deref().unwrap_or("").split(';').next().map(bot_util::js::trim).unwrap_or("").to_lowercase();
    if att.url.is_empty() || !["image/png", "image/jpeg", "image/jpg"].contains(&ty.as_str()) {
        return Some(bad_icon_text());
    }
    if att.size as usize > limits::BOOSTER_ROLE_ICON_BYTES {
        return Some(too_big_text());
    }
    None
}

/// `fetchIcon(att)`: the bytes, checked (PNG or JPEG signature, size), or the problem text.
async fn fetch_icon(app: &App, att: &IconAttachment) -> Result<Vec<u8>, String> {
    let custom = app.ext::<BoostState>().icon_fetch.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let res = match custom {
        Some(f) => f(att.url.clone()).await,
        None => default_icon_fetch(att.url.clone()).await,
    };
    let buf = res.map_err(|e| match e {
        IconFetchError::Problem(t) => t,
        IconFetchError::TooLarge => too_big_text(),
        IconFetchError::Other => "The image could not be downloaded.".into(),
    })?;
    if buf.is_empty() || buf.len() > limits::BOOSTER_ROLE_ICON_BYTES || !(is_png(&buf) || is_jpeg(&buf)) {
        return Err(bad_icon_text());
    }
    Ok(buf)
}

// ── Messages ──────────────────────────────────────────────────────────────────

/// `moduleOffEmbed()`: "The module is off" in the router's wording.
pub fn module_off_embed() -> CreateEmbed {
    warn_embed(
        "Module Disabled",
        Some(&format!("The **{LABEL}** module is turned off on this server. An admin can enable it with `/module enable boosts`.")),
        None,
    )
}

/// `discordText(err)`.
pub fn discord_text(code: Option<u64>) -> String {
    match code {
        Some(50013) => "I am missing permissions: I need **Manage Roles** and my highest role must be above the booster role spot.".into(),
        Some(30005) => format!("This server has reached Discord's limit of {} roles.", consts::ROLES_PER_GUILD),
        Some(50035) => "Discord rejected the role details (for example the icon image).".into(),
        _ => "Discord refused the change. Please try again in a moment.".into(),
    }
}

/// The detail of a refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detail {
    Ms(i64),
    Name(NameReason),
    Text(String),
}

/// A `{ ok: false, code, detail }` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub code: &'static str,
    pub detail: Option<Detail>,
}

impl Problem {
    pub fn new(code: &'static str) -> Self {
        Self { code, detail: None }
    }

    pub fn with(code: &'static str, detail: Detail) -> Self {
        Self { code, detail: Some(detail) }
    }
}

/// `problemEmbed(result)`: the error embed for a refusal.
pub fn problem_embed(p: &Problem) -> CreateEmbed {
    let e = |t: &str, d: &str| error_embed(t, Some(d), None);
    match p.code {
        "SETTINGS" => e("Settings Unavailable", "Server settings could not be loaded. Please try again in a moment."),
        "MODULE_OFF" => module_off_embed(),
        "ROLES_OFF" => e("Booster Roles Are Off", "An admin has not turned on booster roles on this server yet."),
        "NOT_BOOSTING" => e("Boosters Only", "You need to be boosting this server to do that."),
        "ALREADY" => e("You Already Have A Role", "Change it with `/boost role edit`, or remove it with `/boost role delete`."),
        "NO_ROLE" => e("No Booster Role", "You do not have a booster role. Make one with `/boost role create`."),
        "COOLDOWN" => {
            let ms = match p.detail {
                Some(Detail::Ms(ms)) => ms,
                _ => 0,
            };
            let secs = ((ms as f64) / 1000.0).ceil().max(1.0) as i64;
            e("Slow Down", &format!("You can change your role again in {secs} seconds."))
        }
        "BAD_NAME" => {
            let r = match p.detail {
                Some(Detail::Name(r)) => r,
                _ => NameReason::Empty,
            };
            e("Invalid Name", &r.text())
        }
        "BAD_COLOR" => e("Invalid Colour", "Use a hex colour such as `#ff8800`."),
        "BAD_EMOJI" => e("Invalid Icon", "Use one standard emoji. Custom emojis cannot be role icons; upload an image instead."),
        "BAD_ICON" => {
            let t = match &p.detail {
                Some(Detail::Text(t)) => t.clone(),
                _ => "The icon could not be used.".into(),
            };
            e("Invalid Icon Image", &t)
        }
        "ICON_BOTH" => e("Pick One Icon", "Use either `icon_emoji` or `icon_image`, not both."),
        "NO_ROLE_ICONS" => e("Role Icons Locked", "This server has not unlocked role icons yet (they need Boost level 2)."),
        "NOTHING" => e("Nothing To Change", "Give at least one of `name`, `color`, `icon_emoji` or `icon_image`."),
        "BOT_PERMS" => e("Missing Permissions", "I need the **Manage Roles** permission to make booster roles. Ask an admin."),
        "BOT_LOW" => e("Cannot Place The Role", "My highest role is too low to put a role below it. An admin must move my role up."),
        "ANCHOR_HIGH" => e("Cannot Place The Role", "The role booster roles go below is above my highest role. An admin must fix `/boost config`."),
        "ROLE_LIMIT" => e("Role Limit Reached", &format!("This server has reached Discord's limit of {} roles.", consts::ROLES_PER_GUILD)),
        "BOT_ROLE" => e("Cannot Edit The Role", "Your booster role is above my highest role, so I cannot change it. Ask an admin."),
        "DISCORD" => {
            let t = match &p.detail {
                Some(Detail::Text(t)) => t.clone(),
                _ => "Discord refused the change.".into(),
            };
            e("Discord Refused", &t)
        }
        _ => e("Could Not Save", "Something went wrong. Please try again in a moment."),
    }
}

/// `memberProblem(settings, member, { needBoosting })`: why a member may not use `/boost role ...` right now.
pub fn member_problem(settings: Option<&GuildSettings>, boosting: bool, need_boosting: bool) -> Option<Problem> {
    let Some(s) = settings else { return Some(Problem::new("SETTINGS")) };
    if !is_module_enabled(Some(s), Some(ModuleKey::Boosts)) {
        return Some(Problem::new("MODULE_OFF"));
    }
    if !s.boost_custom_roles {
        return Some(Problem::new("ROLES_OFF"));
    }
    if need_boosting && !boosting {
        return Some(Problem::new("NOT_BOOSTING"));
    }
    None
}

// ── Looks (name, colour, icon) ────────────────────────────────────────────────

/// What a member asked their role to look like (checked).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Look {
    pub name: Option<String>,
    pub color: Option<u32>,
    pub unicode_emoji: Option<String>,
    pub icon_attachment: Option<IconAttachment>,
}

/// What a member typed (`name`, `color`, `icon_emoji`, `icon_image`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LookInput {
    pub name: Option<String>,
    pub color: Option<String>,
    pub icon_emoji: Option<String>,
    pub icon_image: Option<IconAttachment>,
}

/// `prepareLook(...)`: checks what the member asked for. Nothing here talks to Discord or spends the cooldown.
pub fn prepare_look(guild: &Guild, input: &LookInput, except_role_id: Option<&str>, require_name: bool) -> Result<Look, Problem> {
    let mut look = Look::default();
    if let Some(name) = &input.name {
        let ids: Vec<(String, String)> = guild.roles.values().map(|r| (r.id.to_string(), r.name.clone())).collect();
        let checked = validate_role_name(Some(name), ids.iter().map(|(a, b)| (a.as_str(), b.as_str())), except_role_id)
            .map_err(|r| Problem::with("BAD_NAME", Detail::Name(r)))?;
        look.name = Some(checked);
    } else if require_name {
        return Err(Problem::with("BAD_NAME", Detail::Name(NameReason::Empty)));
    }
    look.color = parse_color(input.color.as_deref()).map_err(|_| Problem::new("BAD_COLOR"))?;
    let wants_emoji = input.icon_emoji.as_deref().is_some_and(|e| !bot_util::js::trim(e).is_empty());
    let wants_image = input.icon_image.is_some();
    if wants_emoji && wants_image {
        return Err(Problem::new("ICON_BOTH"));
    }
    if wants_emoji || wants_image {
        if !guild.features.iter().any(|f| f == "ROLE_ICONS") {
            return Err(Problem::new("NO_ROLE_ICONS"));
        }
        if wants_emoji {
            look.unicode_emoji = Some(parse_emoji(input.icon_emoji.as_deref()).ok_or_else(|| Problem::new("BAD_EMOJI"))?);
        } else if let Some(att) = &input.icon_image {
            if let Some(problem) = icon_attachment_problem(att) {
                return Err(Problem::with("BAD_ICON", Detail::Text(problem)));
            }
            look.icon_attachment = Some(att.clone());
        }
    }
    Ok(look)
}

async fn resolve_icon(app: &App, look: &Look) -> Result<Option<Vec<u8>>, Problem> {
    let Some(att) = &look.icon_attachment else { return Ok(None) };
    fetch_icon(app, att).await.map(Some).map_err(|t| Problem::with("BAD_ICON", Detail::Text(t)))
}

fn icon_data_uri(icon: &[u8]) -> String {
    format!("data:image/jpg;base64,{}", base64::engine::general_purpose::STANDARD.encode(icon))
}

/// The body discord.js posts for `guild.roles.create(...)` (undefined keys left out).
pub fn create_role_body(look: &Look, icon: Option<&[u8]>) -> Value {
    let mut m = Map::new();
    if let Some(n) = &look.name {
        m.insert("name".into(), json!(n));
    }
    if let Some(c) = look.color {
        m.insert("colors".into(), json!({ "primary_color": c }));
    }
    m.insert("hoist".into(), json!(false));
    m.insert("permissions".into(), json!("0"));
    m.insert("mentionable".into(), json!(false));
    if let Some(i) = icon {
        m.insert("icon".into(), json!(icon_data_uri(i)));
    }
    if let Some(e) = &look.unicode_emoji {
        m.insert("unicode_emoji".into(), json!(e));
    }
    Value::Object(m)
}

/// The body discord.js sends for `role.edit(...)` (undefined keys left out).
pub fn edit_role_body(look: &Look, icon: Option<&[u8]>) -> Value {
    let mut m = Map::new();
    if let Some(n) = &look.name {
        m.insert("name".into(), json!(n));
    }
    if let Some(c) = look.color {
        m.insert("colors".into(), json!({ "primary_color": c }));
    }
    if let Some(i) = icon {
        m.insert("icon".into(), json!(icon_data_uri(i)));
    }
    if let Some(e) = &look.unicode_emoji {
        m.insert("unicode_emoji".into(), json!(e));
    }
    Value::Object(m)
}

// ── Create / edit / delete (member) ───────────────────────────────────────────

/// A created booster role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub role_id: RoleId,
    pub moved: bool,
}

/// `createBoosterRole(...)`: the caller has run [`member_problem`]. Checks, then: create the role (no permissions,
/// not hoisted, not mentionable), move it to its spot, give it to the member, save the row. A failure after the
/// create deletes the role.
pub async fn create_booster_role(app: &App, guild_id: GuildId, member: &Member, settings: Option<GuildSettings>, input: LookInput) -> Result<Created, Problem> {
    let (a, uid) = (app.clone(), member.user.id);
    run_locked(app, &guild_id.to_string(), &uid.to_string(), move || async move { create_locked(&a, guild_id, uid, settings.as_ref(), &input).await }).await
}

async fn create_locked(app: &App, guild_id: GuildId, user_id: UserId, settings: Option<&GuildSettings>, input: &LookInput) -> Result<Created, Problem> {
    let (g, u) = (guild_id.to_string(), user_id.to_string());
    let left = cooldown_left_ms(app, &g, &u);
    if left > 0 {
        return Err(Problem::with("COOLDOWN", Detail::Ms(left)));
    }
    let fail = |e: anyhow::Error| {
        bot_util::log_error!(format!("[boosts] {g}: create failed for {u}"), bot_core::error::log_arg(&e));
        Problem::new("FAILED")
    };
    let guild = cached_guild(app, guild_id).ok_or_else(|| Problem::new("FAILED"))?;
    if let Some(existing) = load_row(app.db(), &g, &u).await.map_err(fail)? {
        if parse_id(&existing.role_id).is_some_and(|r| guild.roles.contains_key(&RoleId::new(r))) {
            return Err(Problem::new("ALREADY"));
        }
        delete_row(app.db(), &g, &u).await.map_err(fail)?; // the role was deleted behind our back: start over
    }
    let look = prepare_look(&guild, input, None, true)?;
    let bot = bot_id(app).ok_or_else(|| Problem::new("BOT_PERMS"))?;
    placement_for(&guild, bot, settings).map_err(Problem::new)?;
    if guild.roles.len() >= consts::ROLES_PER_GUILD {
        return Err(Problem::new("ROLE_LIMIT"));
    }
    let icon = resolve_icon(app, &look).await?;

    note_edit(app, &g, &u);
    let http = app.http().ok_or_else(|| Problem::new("FAILED"))?;
    let body = create_role_body(&look, icon.as_deref());
    let role = match http.create_role(guild_id, &body, Some(&reason(&format!("booster role made by {u}")))).await {
        Ok(r) => {
            bot_core::discord::cache_created_role(app, guild_id, &r);
            r
        }
        Err(e) => {
            bot_util::log_warn!(format!("[boosts] {g}: could not create a role for {u}: {}", bot_core::discord::error_message(&e)));
            return Err(Problem::with("DISCORD", Detail::Text(discord_text(bot_core::discord::error_code(&e)))));
        }
    };

    enum Step {
        Discord(serenity::Error),
        Db(anyhow::Error),
    }
    let finish = async {
        let moved = move_to_spot(app, guild_id, &role, settings, &reason("placing a booster role")).await;
        http.add_member_role(guild_id, user_id, role.id, Some(&reason("booster role of the member"))).await.map_err(Step::Discord)?;
        app.settings().ensure_guild_settings(&g).await.map_err(|e| Step::Db(e.into()))?;
        insert_row(app.db(), &g, &u, &role.id.to_string()).await.map_err(|e| Step::Db(e.into()))?;
        Ok::<bool, Step>(moved)
    };
    let moved = match finish.await {
        Ok(m) => m,
        Err(step) => {
            remove_role(app, guild_id, &role.id.to_string(), &reason("cleaning up a booster role that could not be saved")).await;
            return Err(match step {
                Step::Db(e) => {
                    if e.downcast_ref::<bot_db::DbError>().is_some_and(|d| d.is_unique_violation()) {
                        return Err(Problem::new("ALREADY"));
                    }
                    bot_util::log_error!(format!("[boosts] {g}: could not finish the booster role of {u}"), bot_core::error::log_arg(&e));
                    Problem::new("FAILED")
                }
                Step::Discord(e) => {
                    bot_util::log_error!(format!("[boosts] {g}: could not finish the booster role of {u}: {}", bot_core::discord::error_message(&e)));
                    // `err?.code && Number.isInteger(err.code)`: an error without a Discord code (a 5xx, a network
                    // failure) is FAILED, not DISCORD.
                    match bot_core::discord::error_code(&e).filter(|c| *c != 0) {
                        Some(c) => Problem::with("DISCORD", Detail::Text(discord_text(Some(c)))),
                        None => Problem::new("FAILED"),
                    }
                }
            });
        }
    };
    ensure_sweep_job(app, &g, None).await;
    bot_util::log_info!(format!("[boosts] {g}: {u} made booster role {}", role.id));
    Ok(Created { role_id: role.id, moved })
}

/// An edited booster role and what changed (`name`, `colour`, `icon`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edited {
    pub role_id: RoleId,
    pub changed: Vec<&'static str>,
}

/// Whether anything was asked (`[name, color, iconEmoji, iconImage].some(...)`).
pub fn asked_anything(input: &LookInput) -> bool {
    let s = |v: &Option<String>| v.as_deref().is_some_and(|x| !bot_util::js::trim(x).is_empty());
    s(&input.name) || s(&input.color) || s(&input.icon_emoji) || input.icon_image.is_some()
}

/// `editBoosterRole(...)`: name, colour or icon of the member's own role. A role the member lost is given back.
pub async fn edit_booster_role(app: &App, guild_id: GuildId, member: &Member, input: LookInput) -> Result<Edited, Problem> {
    let (a, uid, has) = (app.clone(), member.user.id, member.roles.clone());
    run_locked(app, &guild_id.to_string(), &uid.to_string(), move || async move { edit_locked(&a, guild_id, uid, &has, &input).await }).await
}

async fn edit_locked(app: &App, guild_id: GuildId, user_id: UserId, member_roles: &[RoleId], input: &LookInput) -> Result<Edited, Problem> {
    let (g, u) = (guild_id.to_string(), user_id.to_string());
    let left = cooldown_left_ms(app, &g, &u);
    if left > 0 {
        return Err(Problem::with("COOLDOWN", Detail::Ms(left)));
    }
    let fail = |e: anyhow::Error| {
        bot_util::log_error!(format!("[boosts] {g}: edit failed for {u}"), bot_core::error::log_arg(&e));
        Problem::new("FAILED")
    };
    let row = load_row(app.db(), &g, &u).await.map_err(fail)?.ok_or_else(|| Problem::new("NO_ROLE"))?;
    let guild = cached_guild(app, guild_id).ok_or_else(|| Problem::new("FAILED"))?;
    let Some(role_id) = parse_id(&row.role_id).map(RoleId::new).filter(|r| guild.roles.contains_key(r)) else {
        delete_row(app.db(), &g, &u).await.map_err(fail)?;
        return Err(Problem::new("NO_ROLE"));
    };
    if !asked_anything(input) {
        return Err(Problem::new("NOTHING"));
    }
    let look = prepare_look(&guild, input, Some(&row.role_id), false)?;
    if bot_id(app).is_some_and(|b| !role_editable(&guild, b, role_id)) {
        return Err(Problem::new("BOT_ROLE"));
    }
    let icon = resolve_icon(app, &look).await?;

    note_edit(app, &g, &u);
    let http = app.http().ok_or_else(|| Problem::new("FAILED"))?;
    if let Err(e) = http.edit_role(guild_id, role_id, &edit_role_body(&look, icon.as_deref()), Some(&reason(&format!("booster role edited by {u}")))).await {
        bot_util::log_warn!(format!("[boosts] {g}: could not edit role {role_id}: {}", bot_core::discord::error_message(&e)));
        return Err(Problem::with("DISCORD", Detail::Text(discord_text(bot_core::discord::error_code(&e)))));
    }
    if !member_roles.contains(&role_id)
        && let Err(e) = http.add_member_role(guild_id, user_id, role_id, Some(&reason("booster role of the member"))).await
    {
        bot_util::log_warn!(format!("[boosts] {g}: could not give {role_id} back to {u}: {}", bot_core::discord::error_message(&e)));
    }
    let mut changed = Vec::new();
    if look.name.is_some() {
        changed.push("name");
    }
    if look.color.is_some() {
        changed.push("colour");
    }
    if look.unicode_emoji.is_some() || icon.is_some() {
        changed.push("icon");
    }
    Ok(Edited { role_id, changed })
}

/// `deleteOwnRole({ guild, member })`: the member removes their own role (also during the grace period).
pub async fn delete_own_role(app: &App, guild_id: GuildId, user_id: UserId) -> Result<(), Problem> {
    let a = app.clone();
    run_locked(app, &guild_id.to_string(), &user_id.to_string(), move || async move {
        let (g, u) = (guild_id.to_string(), user_id.to_string());
        let row = match load_row(a.db(), &g, &u).await {
            Ok(Some(r)) => r,
            Ok(None) => return Err(Problem::new("NO_ROLE")),
            Err(e) => {
                bot_util::log_error!(format!("[boosts] {g}: delete failed for {u}"), bot_core::error::log_arg(&e));
                return Err(Problem::new("FAILED"));
            }
        };
        match remove_booster_role(&a, guild_id, &row, &reason(&format!("booster role deleted by {u}"))).await {
            RemoveOutcome::Forbidden => Err(Problem::new("BOT_ROLE")),
            RemoveOutcome::Error => Err(Problem::new("FAILED")),
            _ => Ok(()),
        }
    })
    .await
}

/// `removeMemberRole({ guild, userId, actorId })` (`/boost remove`): the role id that went.
pub async fn remove_member_role(app: &App, guild_id: GuildId, user_id: &str, actor_id: &str) -> Result<String, Problem> {
    let (a, u, actor) = (app.clone(), user_id.to_string(), actor_id.to_string());
    run_locked(app, &guild_id.to_string(), user_id, move || async move {
        let g = guild_id.to_string();
        let row = match load_row(a.db(), &g, &u).await {
            Ok(Some(r)) => r,
            Ok(None) => return Err(Problem::new("NO_ROLE")),
            Err(e) => {
                bot_util::log_error!(format!("[boosts] {g}: remove failed for {u}"), bot_core::error::log_arg(&e));
                return Err(Problem::new("FAILED"));
            }
        };
        match remove_booster_role(&a, guild_id, &row, &reason(&format!("booster role removed by {actor}"))).await {
            RemoveOutcome::Forbidden => Err(Problem::new("BOT_ROLE")),
            RemoveOutcome::Error => Err(Problem::new("FAILED")),
            _ => Ok(row.role_id),
        }
    })
    .await
}

// ── Thank-you ─────────────────────────────────────────────────────────────────

/// What [`post_thank_you`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThankYou {
    Posted { channel_id: String },
    NoChannel,
    Unusable(String),
    Failed(String),
}

/// The preview label line.
pub const PREVIEW_HEAD: &str = "🧪 **Preview of the boost thank-you** (nobody was pinged)\n";

/// The thank-you message `postThankYou` sends (content and mentions), for a channel that is usable.
pub fn thank_you_message(settings: Option<&GuildSettings>, src: &ThankYouSource, member_id: UserId, preview: bool) -> CreateMessage {
    let head = if preview { PREVIEW_HEAD } else { "" };
    let content = format!("{head}{}", render_thank_you(settings, src, consts::MESSAGE_MAX - bot_util::js::utf16_len(head), preview));
    let mentions = if preview { CreateAllowedMentions::new() } else { CreateAllowedMentions::new().users([member_id]) };
    CreateMessage::new().content(content).allowed_mentions(mentions)
}

/// `postThankYou(guild, member, settings, { preview })`: posts the thank-you in `boostChannelId`. Pings only that
/// member (a preview pings nobody). Never fails.
pub async fn post_thank_you(app: &App, guild_id: GuildId, member: &Member, settings: Option<&GuildSettings>, preview: bool) -> ThankYou {
    let Some(channel_id) = settings.and_then(|s| s.boost_channel_id.clone()).filter(|c| !c.is_empty()) else { return ThankYou::NoChannel };
    let cid = parse_id(&channel_id).map(ChannelId::new);
    let usable = check_channel_usable(app, guild_id, cid, &["ViewChannel", "SendMessages"]);
    let warn = |text: &str| {
        if !preview && app.ext::<BoostState>().warn_channel.allow(&guild_id.to_string()) {
            bot_util::log_warn!(format!("[boosts] {guild_id}: {text}"));
        }
    };
    if !usable.ok {
        let r = usable.reason.unwrap_or_default();
        warn(&format!("cannot post the boost thank-you: {r}"));
        return ThankYou::Unusable(r);
    }
    let guild = cached_guild(app, guild_id);
    let msg = thank_you_message(settings, &ThankYouSource::of(guild.as_ref(), Some(member)), member.user.id, preview);
    let (Some(http), Some(cid)) = (app.http(), cid) else { return ThankYou::Failed("not connected".into()) };
    match cid.send_message(http.as_ref(), msg).await {
        Ok(_) => ThankYou::Posted { channel_id },
        Err(e) => {
            let text = bot_core::discord::error_message(&e);
            warn(&format!("could not post the boost thank-you: {text}"));
            ThankYou::Failed(text)
        }
    }
}

// ── Hook bodies and filters ───────────────────────────────────────────────────

/// `memberUpdateFilter(ctx)`: only a change of the boost state of a cached person; a partial old member never counts.
pub fn member_update_filter(old_known: bool, old: Option<&Member>, new: &Member) -> bool {
    if !old_known || new.user.bot {
        return false;
    }
    let Some(old) = old else { return false };
    is_boosting(Some(old)) != is_boosting(Some(new))
}

/// `memberRemoveFilter(ctx)`: people only.
pub fn member_remove_filter(user_is_bot: bool) -> bool {
    !user_is_bot
}

/// `channelDeleteFilter(ctx)`: skipped once the memoised settings show the channel is not the thank-you channel.
pub fn channel_delete_filter(app: &App, guild_id: &str, channel_id: &str) -> bool {
    match app.settings().peek_settings(guild_id) {
        None => true,
        Some(s) => s.boost_channel_id.as_deref() == Some(channel_id),
    }
}

async fn on_boost_start(app: &App, guild_id: GuildId, member: &Member, settings: &GuildSettings) {
    let (g, u) = (guild_id.to_string(), member.user.id.to_string());
    match clear_ended(app.db(), &g, &u).await {
        Ok(n) if n > 0 => bot_util::log_info!(format!("[boosts] {g}: {u} boosts again, keeping their booster role")),
        Ok(_) => {}
        Err(e) => bot_util::log_warn!(format!("[boosts] {g}: could not clear the end of the boost of {u}: {e}")),
    }
    post_thank_you(app, guild_id, member, Some(settings), false).await;
}

async fn on_boost_end(app: &App, guild_id: GuildId, user_id: UserId, settings: GuildSettings) -> anyhow::Result<()> {
    let a = app.clone();
    run_locked(app, &guild_id.to_string(), &user_id.to_string(), move || async move {
        let (g, u) = (guild_id.to_string(), user_id.to_string());
        let Some(row) = load_row(a.db(), &g, &u).await? else { return Ok(()) };
        if row.boost_ended_at.is_none() {
            mark_ended(a.db(), &g, &u, a.clock().now()).await?;
        }
        if grace_ms(Some(&settings)) == 0 {
            remove_booster_role(&a, guild_id, &row, &reason("the boost ended")).await;
        } else {
            ensure_sweep_job(&a, &g, None).await; // the grace period ends later; the sweep removes the role then
        }
        Ok(())
    })
    .await
}

/// `onMemberUpdate(ctx)`: a boost started (thank-you, a returning booster keeps their role) or ended (grace starts).
pub async fn on_member_update(ctx: Arc<HookCtx<GuildMemberUpdate>>) -> bot_core::BotResult {
    let before = is_boosting(ctx.data.old_member.as_ref());
    let Some(kind) = boost_transition(before, is_boosting(Some(&ctx.data.member))) else { return Ok(()) };
    let Some(settings) = ctx.get_settings().await else { return Ok(()) };
    let g = ctx.data.guild_id;
    let res = match kind {
        Transition::Start => {
            on_boost_start(&ctx.app, g, &ctx.data.member, &settings).await;
            Ok(())
        }
        Transition::End => on_boost_end(&ctx.app, g, ctx.data.member.user.id, (*settings).clone()).await,
    };
    if let Err(e) = res {
        let k = if kind == Transition::Start { "start" } else { "end" };
        bot_util::log_error!(format!("[boosts] {g}: boost {k} handling failed for {}", ctx.data.member.user.id), bot_core::error::log_arg(&e));
    }
    Ok(())
}

/// `onMemberRemove(ctx)`: the member's booster role and row go.
pub async fn on_member_remove(ctx: Arc<HookCtx<GuildMemberRemove>>) -> bot_core::BotResult {
    let (g, uid) = (ctx.data.guild_id, ctx.data.user.id);
    let a = ctx.app.clone();
    let res: anyhow::Result<()> = run_locked(&ctx.app, &g.to_string(), &uid.to_string(), move || async move {
        if let Some(row) = load_row(a.db(), &g.to_string(), &uid.to_string()).await? {
            remove_booster_role(&a, g, &row, &reason("the member left the server")).await;
        }
        Ok(())
    })
    .await;
    if let Err(e) = res {
        bot_util::log_error!(format!("[boosts] {g}: could not remove the booster role of {uid} who left"), bot_core::error::log_arg(&e));
    }
    Ok(())
}

/// `onRoleDelete(ctx)` (always): the row of a deleted booster role is dropped; a deleted anchor role is forgotten.
pub async fn on_role_delete(ctx: Arc<HookCtx<RoleDelete>>) -> bot_core::BotResult {
    let (g, role) = (ctx.data.guild_id.to_string(), ctx.data.role_id.to_string());
    if let Err(e) = delete_rows_by_role(ctx.app.db(), &g, &role).await {
        bot_util::log_warn!(format!("[boosts] {g}: could not drop the row of deleted role {role}: {e}"));
    }
    let settings = ctx.get_settings().await;
    if settings.as_ref().and_then(|s| s.boost_role_anchor_id.as_deref()) == Some(role.as_str())
        && let Err(e) = ctx.app.settings().update_settings(&g, SettingsPatch::new().set("boostRoleAnchorId", Value::Null)).await
    {
        bot_util::log_warn!(format!("[boosts] {g}: could not clear the deleted anchor role: {e}"));
    }
    Ok(())
}

/// `onChannelDelete(ctx)` (always): a deleted thank-you channel is forgotten.
pub async fn on_channel_delete(ctx: Arc<HookCtx<ChannelDelete>>) -> bot_core::BotResult {
    let Some(settings) = ctx.get_settings().await else { return Ok(()) };
    let id = ctx.data.channel.id.to_string();
    if settings.boost_channel_id.as_deref() != Some(id.as_str()) {
        return Ok(());
    }
    let g = ctx.data.guild_id.to_string();
    if let Err(e) = ctx.app.settings().update_settings(&g, SettingsPatch::new().set("boostChannelId", Value::Null)).await {
        bot_util::log_warn!(format!("[boosts] {g}: could not clear the deleted thank-you channel: {e}"));
    }
    Ok(())
}

/// `onGuildReady(ctx)`: a guild with booster roles gets its sweep back (a live job is kept).
pub async fn on_guild_ready(ctx: Arc<HookCtx<GuildReady>>) -> bot_core::BotResult {
    let g = ctx.data.guild_id.to_string();
    let n = match count_rows(ctx.app.db(), &g).await {
        Ok(n) => n,
        Err(e) => {
            bot_util::log_error!(format!("[boosts] {g}: could not count the booster roles on startup"), bot_core::error::log_arg(&e));
            return Ok(());
        }
    };
    if n > 0 {
        let jitter = (rand_unit() * consts::STARTUP_JITTER_MS as f64).floor() as i64;
        let at = ctx.app.clock().now() + chrono::Duration::milliseconds(consts::STARTUP_DELAY_MS + jitter);
        ensure_sweep_job(&ctx.app, &g, Some(at)).await;
    }
    Ok(())
}

/// `Math.random()` without a dependency: the sub-second clock mixed through a hash.
fn rand_unit() -> f64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    (h.finish() >> 11) as f64 / (1u64 << 53) as f64
}

// ── The sweep ─────────────────────────────────────────────────────────────────

/// `resetSweepState()`: forgets the lookup offsets (tests).
pub fn reset_sweep_state(app: &App) {
    app.ext::<BoostState>().lookup_offset.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// The member of `user_id` as the sweep sees it, from the cache or a lookup (`Unknown` on any other error).
pub type MemberLookup = Arc<dyn Fn(String) -> futures::future::BoxFuture<'static, MemberState> + Send + Sync>;

async fn lookup_member(app: &App, guild_id: GuildId, user_id: &str) -> MemberState {
    let (Some(http), Some(uid)) = (app.http(), parse_id(user_id)) else { return MemberState::Unknown };
    let fetch = async move { Ok::<_, std::convert::Infallible>(http.get_member(guild_id, UserId::new(uid)).await) };
    match bot_util::async_util::with_timeout(fetch, consts::LOOKUP_TIMEOUT_MS, "boost sweep member lookup").await {
        Ok(Ok(m)) => MemberState::Present { boosting: is_boosting(Some(&m)) },
        Ok(Err(e)) if bot_core::discord::error_code(&e) == Some(10007) => MemberState::Left,
        Ok(Err(e)) => {
            bot_util::log_debug!(format!("[boosts] {guild_id}: could not look up {user_id}: {}", bot_core::discord::error_message(&e)));
            MemberState::Unknown
        }
        Err(_) => MemberState::Unknown,
    }
}

fn cached_member_state(app: &App, guild_id: GuildId, user_id: &str) -> Option<MemberState> {
    let d = app.discord()?;
    let guild = d.cache.guild(guild_id)?;
    let m = guild.members.get(&UserId::new(parse_id(user_id)?))?;
    Some(MemberState::Present { boosting: is_boosting(Some(m)) })
}

/// What one sweep pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepOut {
    pub deleted: usize,
    pub ended: usize,
    pub resumed: usize,
    pub failed: usize,
    pub more: bool,
}

/// Options of [`sweep_guild`]: `now` (epoch ms), the time budget, and a member lookup to use instead of the REST call
/// (tests).
#[derive(Clone)]
pub struct SweepOptions {
    pub now_ms: i64,
    pub budget_ms: u64,
    pub lookup: Option<MemberLookup>,
}

async fn apply_plan(app: &App, guild_id: GuildId, row: &BoosterRole, resolved: MemberState, settings: Option<GuildSettings>, now_ms: i64) -> anyhow::Result<&'static str> {
    let (a, user) = (app.clone(), row.user_id.clone());
    run_locked(app, &guild_id.to_string(), &row.user_id, move || async move {
        let g = guild_id.to_string();
        let Some(fresh) = load_row(a.db(), &g, &user).await? else { return Ok("none") };
        let member = cached_member_state(&a, guild_id, &user).unwrap_or(resolved);
        let plan = plan_sweep(fresh.boost_ended_at.map(|d| d.timestamp_millis()), member, settings.as_ref(), now_ms);
        Ok(match plan {
            Plan::Ended => {
                let when = chrono::DateTime::from_timestamp_millis(now_ms).unwrap_or_else(Utc::now);
                mark_ended(a.db(), &g, &user, when).await?;
                "ended"
            }
            Plan::Resumed => {
                clear_ended(a.db(), &g, &user).await?;
                "resumed"
            }
            Plan::Delete(why) => match remove_booster_role(&a, guild_id, &fresh, &reason(why)).await {
                RemoveOutcome::Deleted | RemoveOutcome::Gone => "deleted",
                _ => "failed",
            },
            _ => "none",
        })
    })
    .await
}

/// `sweepGuild(guild, rows, settings, { now, budgetMs })`: one pass over a guild's rows. Members come from the cache;
/// the ones that are not cached are looked up one by one, at most LOOKUPS_PER_RUN per run, and deletions are capped at
/// DELETES_PER_RUN. `more` says the next run should come soon.
pub async fn sweep_guild(app: &App, guild_id: GuildId, rows: &[BoosterRole], settings: Option<&GuildSettings>, opts: SweepOptions) -> SweepOut {
    let mut out = SweepOut::default();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(opts.budget_ms);
    let late = || tokio::time::Instant::now() >= deadline;
    let g = guild_id.to_string();
    let mut resolved: HashMap<String, MemberState> = HashMap::new();
    let mut uncached: Vec<String> = Vec::new();
    for row in rows {
        match cached_member_state(app, guild_id, &row.user_id) {
            Some(s) => {
                resolved.insert(row.user_id.clone(), s);
            }
            None => uncached.push(row.user_id.clone()),
        }
    }
    if !uncached.is_empty() {
        let state = app.ext::<BoostState>();
        let start = state.lookup_offset.lock().unwrap_or_else(|e| e.into_inner()).get(&g).copied().unwrap_or(0) % uncached.len();
        let order: Vec<String> = uncached[start..].iter().chain(uncached[..start].iter()).cloned().collect();
        let mut looked = 0usize;
        for user_id in order.iter().take(consts::LOOKUPS_PER_RUN) {
            if late() {
                break;
            }
            let member = match &opts.lookup {
                Some(f) => f(user_id.clone()).await,
                None => lookup_member(app, guild_id, user_id).await,
            };
            looked += 1;
            if member != MemberState::Unknown {
                resolved.insert(user_id.clone(), member);
            }
        }
        let mut offsets = state.lookup_offset.lock().unwrap_or_else(|e| e.into_inner());
        if order.len() > looked {
            out.more = true;
            offsets.insert(g.clone(), start + looked);
        } else {
            offsets.remove(&g);
        }
    }

    let mut deletes = 0usize;
    for row in rows {
        let member = resolved.get(&row.user_id).copied().unwrap_or(MemberState::Unknown);
        let plan = plan_sweep(row.boost_ended_at.map(|d| d.timestamp_millis()), member, settings, opts.now_ms);
        if matches!(plan, Plan::None | Plan::Skip) {
            continue;
        }
        if late() {
            out.more = true;
            break;
        }
        if matches!(plan, Plan::Delete(_)) {
            if deletes >= consts::DELETES_PER_RUN {
                out.more = true;
                continue;
            }
            deletes += 1;
        }
        match apply_plan(app, guild_id, row, member, settings.cloned(), opts.now_ms).await {
            Ok("deleted") => out.deleted += 1,
            Ok("ended") => out.ended += 1,
            Ok("resumed") => out.resumed += 1,
            Ok("failed") => out.failed += 1,
            Ok(_) => {}
            Err(e) => {
                out.failed += 1;
                bot_util::log_error!(format!("[boosts] {g}: sweep failed for {}", row.user_id), bot_core::error::log_arg(&e));
            }
        }
    }
    if out.deleted > 0 || out.ended > 0 || out.resumed > 0 {
        bot_util::log_info!(format!("[boosts] {g}: sweep removed {} role(s), {} boost(s) ended, {} resumed", out.deleted, out.ended, out.resumed));
    }
    out
}

fn after(app: &App, ms: i64) -> JobOutcome {
    JobOutcome::Reschedule { next_run_at: app.clock().now() + chrono::Duration::milliseconds(ms), payload: None }
}

/// `sweepJobHandler(job, ctx)`: the `boosts.sweep` body. Never fails, so the recurring job cannot go FAILED. It ends
/// itself when the guild has no rows. It cleans up whether or not the module is on.
pub async fn sweep_job_handler(job: Job, ctx: JobCtx) -> bot_core::BotResult<JobOutcome> {
    let app = ctx.app.clone();
    Ok(sweep_job_run(&app, &job.guild_id, None).await)
}

/// The sweep job body with an optional member lookup (tests).
pub async fn sweep_job_run(app: &App, guild_id: &str, lookup: Option<MemberLookup>) -> JobOutcome {
    let Some(gid) = parse_id(guild_id).map(GuildId::new) else { return after(app, consts::SWEEP_RETRY_MS) };
    if app.guild_directory().guild_state(guild_id) != bot_core::discord::GuildState::Available {
        return after(app, consts::SWEEP_RETRY_MS);
    }
    let rows = match list_rows(app.db(), guild_id).await {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_error!(format!("boosts.sweep failed for guild {guild_id}"), bot_core::error::log_arg(&e));
            return after(app, consts::SWEEP_RETRY_MS);
        }
    };
    if rows.is_empty() {
        return JobOutcome::Done;
    }
    let Some(settings) = app.settings().get_settings_safe(guild_id).await else { return after(app, 2 * 60_000) };
    let out = sweep_guild(app, gid, &rows, Some(&settings), SweepOptions { now_ms: app.clock().now_ms(), budget_ms: consts::SWEEP_BUDGET_MS, lookup }).await;
    after(app, if out.more { consts::SWEEP_MORE_MS } else { consts::SWEEP_EVERY_MS })
}
