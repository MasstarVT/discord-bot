//! Port of `src/services/bumpService.js`: Bump Reminders (beyond MEE6).
//!
//! The bot watches for a successful `/bump` on a bump site (today Disboard), remembers when the server can be bumped
//! again, reminds the server (optionally pinging a role) when the cooldown ends, thanks the bumper and keeps a
//! leaderboard of who bumps. This file holds the rules: the site table and its pure success check, the bumper
//! lookup, the state / count writes, the `bump.remind` job and the cleanup hooks. [`crate::plugins::bumps`] declares
//! the hooks and the job; [`crate::commands::bumps`] is the command.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use bot_core::adapters::{placeholder_channel, placeholder_guild, render_context};
use bot_core::interaction_helpers::{Payload, cached_guild, check_channel_usable, pingable_role_ids};
use bot_core::prelude::*;
use bot_db::{BumpCount, BumpState, PgPool};
use bot_util::placeholder_parser::{RenderContext, VarValue, render_placeholders};
use chrono::{DateTime, TimeZone, Utc};
use serde_json::{Value, json};
use serenity::all::{ChannelId, Embed, GuildChannel, GuildId, Message, MessageInteractionMetadata, User};
use serenity::builder::{CreateAllowedMentions, CreateEmbed};

use crate::io::{IoError, io, snowflake};

// ── Constants ─────────────────────────────────────────────────────────────────

const HOUR_MS: i64 = 60 * 60_000;

/// The Int clamp of `BumpCount.count`.
pub const BUMP_COUNT_MAX: i32 = bot_config::XP_MAX;
/// Rows on `/bumps leaderboard`.
pub const LEADERBOARD_SIZE: i64 = 10;
/// A reminder job that runs this much before `nextBumpAt` (clock slack) still sends; earlier ones reschedule
/// themselves.
const EARLY_SLACK_MS: i64 = 5_000;
/// Message ids remembered so one bump reply is never processed twice.
const SEEN_MAX: usize = 500;
/// Discord errors that no retry fixes: unknown channel, missing access, missing permissions, archived thread.
pub const PERMANENT_CODES: [u64; 4] = [10003, 50001, 50013, 50083];
const MESSAGE_MAX: usize = 1900;

/// The text used while `bumpReminderMessage` is null. An exact copy of DEFAULT_TEXTS in src/dashboard/fields.js.
pub const DEFAULT_REMINDER_MESSAGE: &str = "📣 The server can be bumped again! Use `/bump` to keep us near the top of Disboard.";
/// The text used while `bumpThanksMessage` is null.
pub const DEFAULT_THANKS_MESSAGE: &str = "📣 Thanks for bumping, {user}! The next bump is possible {next}.";

// ── The bump sites ────────────────────────────────────────────────────────────

// Disboard answers `/bump` with an interaction reply: an embed whose description reads "Bump done! :thumbsup:" and
// whose image is bot-command-image-bump. The failure reply ("Please wait another N minutes until the server can be
// bumped") is never a success.
static DONE_RE: bot_util::jsre::LazyRe = bot_util::lazy_re!("bump done", "i");
static WAIT_RE: bot_util::jsre::LazyRe = bot_util::lazy_re!("please wait|wait another|until the server can be bumped", "i");
const IMAGE_MARK: &str = "bot-command-image-bump";

/// Pure: whether a Disboard message (its embeds) is the answer to a successful bump.
pub fn is_disboard_success(embeds: &[Embed]) -> bool {
    for e in embeds {
        let text = e.description.as_deref().unwrap_or("");
        if WAIT_RE.test(text) {
            return false;
        }
        let image = e.image.as_ref().map(|i| i.url.as_str()).unwrap_or("");
        if DONE_RE.test(text) || image.contains(IMAGE_MARK) {
            return true;
        }
    }
    false
}

/// One bump site: `{ botId, name, cooldownMs, isSuccess(message) }`.
#[derive(Debug, Clone, Copy)]
pub struct BumpSite {
    pub key: &'static str,
    pub bot_id: &'static str,
    pub name: &'static str,
    pub cooldown_ms: i64,
    pub is_success: fn(&[Embed]) -> bool,
}

/// `BUMP_SITES`: add a bump site here and nowhere else.
pub const BUMP_SITES: [BumpSite; 1] =
    [BumpSite { key: "disboard", bot_id: "302050872383242240", name: "Disboard", cooldown_ms: 2 * HOUR_MS, is_success: is_disboard_success }];

/// The site with this key.
pub fn site(key: &str) -> Option<&'static BumpSite> {
    BUMP_SITES.iter().find(|s| s.key == key)
}

/// `siteKeyForBot(userId)`: the site key whose bot has this user id.
pub fn site_key_for_bot(user_id: &str) -> Option<&'static str> {
    BUMP_SITES.iter().find(|s| s.bot_id == user_id).map(|s| s.key)
}

/// `bumperOf(message)`: the member who ran the bump command (interaction metadata, else the deprecated interaction);
/// `None` when unknown or a bot.
pub fn bumper_of(message: &Message) -> Option<User> {
    let from_meta = message.interaction_metadata.as_deref().and_then(|m| match m {
        MessageInteractionMetadata::Command(c) => Some(c.user.clone()),
        MessageInteractionMetadata::Component(c) => Some(c.user.clone()),
        MessageInteractionMetadata::ModalSubmit(c) => Some(c.user.clone()),
        _ => None,
    });
    #[allow(deprecated)]
    let user = from_meta.or_else(|| message.interaction.as_ref().map(|i| i.user.clone()))?;
    (!user.bot).then_some(user)
}

/// `isBumpSiteMessage(ctx)`: the synchronous hook filter; only messages by a known bump site's bot pass.
pub fn is_bump_site_message(author_id: &str) -> bool {
    site_key_for_bot(author_id).is_some()
}

// ── Raw SQL ───────────────────────────────────────────────────────────────────

/// `incrementCountSql(guildId, userId, at)`: binds `$1` guild, `$2` user, `$3` the bump time (ISO text).
pub fn increment_count_sql() -> String {
    format!(
        r#"
  INSERT INTO "BumpCount" ("guildId", "userId", "count", "lastBumpAt")
  VALUES ($1, $2, 1, {at})
  ON CONFLICT ("guildId", "userId") DO UPDATE
  SET "count" = LEAST("BumpCount"."count"::bigint + 1, {max}::bigint)::int, "lastBumpAt" = {at}
  RETURNING "count""#,
        at = bot_db::sql_utc(3),
        max = BUMP_COUNT_MAX
    )
}

/// `RAW_SQL`: every raw statement, with sample parameters `(name, sql, binds)`.
pub fn raw_sql() -> Vec<(&'static str, String, Vec<Value>)> {
    vec![("bumps.incrementCount", increment_count_sql(), vec![json!("0"), json!("0"), json!("1970-01-01T00:00:00.000Z")])]
}

// ── Small helpers ─────────────────────────────────────────────────────────────

/// `moduleOffEmbed()`: the router's wording for a member command used while the module is off.
pub fn module_off_embed() -> CreateEmbed {
    warn_embed(
        "Module Disabled",
        Some("The **Bump Reminders** module is turned off on this server. An admin can enable it with `/module enable bumps`."),
        None,
    )
}

/// `reminderDedupeKey(guildId, site)`.
pub fn reminder_dedupe_key(guild_id: &str, site: &str) -> String {
    format!("bump.remind:{guild_id}:{site}")
}

fn unix(date: &DateTime<Utc>) -> i64 {
    date.timestamp_millis().div_euclid(1000)
}

fn ms_date(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).single().unwrap_or_else(Utc::now)
}

/// The per-app state the JS keeps in module variables: the processed message ids, the per-guild-and-site lock and
/// the hourly warning throttle.
pub struct BumpRuntime {
    seen: Mutex<(HashSet<String>, VecDeque<String>)>,
    lock: bot_core::sync::KeyedMutex,
    warn_once_per_hour: bot_util::async_util::Throttle,
}

impl Default for BumpRuntime {
    fn default() -> Self {
        Self {
            seen: Mutex::new((HashSet::new(), VecDeque::new())),
            lock: bot_core::sync::KeyedMutex::new(),
            warn_once_per_hour: bot_util::async_util::create_throttle(HOUR_MS as u64),
        }
    }
}


/// `markSeen(messageId)`: marks a message id as processed; false when it already was. Bounded (oldest forgotten
/// first).
pub fn mark_seen(app: &App, message_id: &str) -> bool {
    if message_id.is_empty() {
        return false;
    }
    let rt = app.ext::<BumpRuntime>();
    let mut g = rt.seen.lock().unwrap_or_else(|e| e.into_inner());
    let (set, order) = &mut *g;
    if set.contains(message_id) {
        return false;
    }
    set.insert(message_id.to_string());
    order.push_back(message_id.to_string());
    if set.len() > SEEN_MAX
        && let Some(old) = order.pop_front()
    {
        set.remove(&old);
    }
    true
}

/// `__resetSeen()`: forgets the processed message ids (tests).
pub fn reset_seen(app: &App) {
    let rt = app.ext::<BumpRuntime>();
    let mut g = rt.seen.lock().unwrap_or_else(|e| e.into_inner());
    g.0.clear();
    g.1.clear();
}

fn warn_limited(app: &App, guild_id: &str, text: String) {
    if app.ext::<BumpRuntime>().warn_once_per_hour.allow(guild_id) {
        bot_util::log_warn!(text);
    } else {
        bot_util::log_debug!(text);
    }
}

// ── State and counts ──────────────────────────────────────────────────────────

/// `writeState(...)`: writes the guild's state for a site unless a bump at or after `at` is already recorded (a
/// repeated message changes nothing). True when this bump is now the recorded one.
async fn write_state(pool: &PgPool, guild_id: &str, site: &str, at: DateTime<Utc>, bumper_id: Option<&str>, channel_id: &str, next: DateTime<Utc>) -> anyhow::Result<bool> {
    let updated = sqlx::query(
        r#"UPDATE "BumpState" SET "lastBumpAt" = $3, "lastBumperId" = $4, "channelId" = $5, "nextBumpAt" = $6, "remindedAt" = NULL WHERE "guildId" = $1 AND "site" = $2 AND "lastBumpAt" < $3"#,
    )
    .bind(guild_id)
    .bind(site)
    .bind(at.naive_utc())
    .bind(bumper_id)
    .bind(channel_id)
    .bind(next.naive_utc())
    .execute(pool)
    .await
    .map_err(bot_db::DbError::from)?;
    if updated.rows_affected() > 0 {
        return Ok(true);
    }
    let created = sqlx::query(
        r#"INSERT INTO "BumpState" ("guildId", "site", "lastBumpAt", "lastBumperId", "channelId", "nextBumpAt", "remindedAt") VALUES ($1, $2, $3, $4, $5, $6, NULL)"#,
    )
    .bind(guild_id)
    .bind(site)
    .bind(at.naive_utc())
    .bind(bumper_id)
    .bind(channel_id)
    .bind(next.naive_utc())
    .execute(pool)
    .await
    .map_err(bot_db::DbError::from);
    match created {
        Ok(_) => Ok(true),
        // The row exists with a bump at or after `at` (Prisma P2002).
        Err(e) if e.is_unique_violation() => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// `incrementCount(guildId, userId, at)`: the bumper's new total (clamped to [`BUMP_COUNT_MAX`]), `None` when it
/// could not be read.
async fn increment_count(pool: &PgPool, guild_id: &str, user_id: &str, at: DateTime<Utc>) -> anyhow::Result<Option<i64>> {
    let sql = increment_count_sql();
    let row: Option<(i32,)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(guild_id)
        .bind(user_id)
        .bind(bot_db::sql_utc_param(&at))
        .fetch_optional(pool)
        .await
        .map_err(bot_db::DbError::from)?;
    Ok(row.map(|r| i64::from(r.0)))
}

/// The `bump.remind` job the JS schedules (type, run time, payload, dedupe key, conflict mode).
pub fn reminder_job(guild_id: &str, site: &str, run_at: DateTime<Utc>) -> ScheduleJob {
    ScheduleJob::new(guild_id, JobType::BumpRemind, run_at)
        .payload(json!({ "site": site }))
        .dedupe_key(reminder_dedupe_key(guild_id, site))
        .on_conflict(OnConflict::Replace)
}

async fn schedule_reminder(app: &App, guild_id: &str, site: &str, run_at: DateTime<Utc>) {
    if let Err(e) = app.scheduler().schedule_job(reminder_job(guild_id, site, run_at)).await {
        bot_util::log_warn!(format!("[bumps] {guild_id}: scheduling the reminder failed, retrying once ({e})"));
        if let Err(e2) = app.scheduler().schedule_job(reminder_job(guild_id, site, run_at)).await {
            // The bump itself is recorded; the thank-you and the count must still happen.
            bot_util::log_error!(format!("[bumps] {guild_id}: the {site} reminder could not be scheduled"), bot_core::error::log_arg(&e2));
        }
    }
}

/// What [`record_bump`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    /// False: an equal or newer bump was already known.
    pub recorded: bool,
    pub count: Option<i64>,
    pub next_bump_at: Option<DateTime<Utc>>,
}

/// `recordBump({ guildId, site, channelId, bumperId, at })`: records one successful bump (the guild's state, the
/// reminder job and the bumper's count), serialised per guild and site. An unknown site is an error.
pub fn record_bump(
    app: &App,
    guild_id: &str,
    site_key: &str,
    channel_id: &str,
    bumper_id: Option<&str>,
    at: DateTime<Utc>,
) -> impl std::future::Future<Output = anyhow::Result<Recorded>> + Send + 'static + use<> {
    let def = site(site_key).copied();
    let (app, g, s, ch, bumper) = (app.clone(), guild_id.to_string(), site_key.to_string(), channel_id.to_string(), bumper_id.map(str::to_string));
    let at = ms_date(at.timestamp_millis());
    let locked = def.map(|def| {
        let a = app.clone();
        let rt = app.ext::<BumpRuntime>();
        rt.lock.run(&format!("{g}:{s}"), move || async move {
            a.settings().ensure_guild_settings(&g).await?;
            let next = ms_date(at.timestamp_millis() + def.cooldown_ms);
            if !write_state(a.db(), &g, &s, at, bumper.as_deref(), &ch, next).await? {
                return Ok(Recorded { recorded: false, count: None, next_bump_at: None });
            }
            // The reminder first: a failing count must never cost the server its reminder.
            schedule_reminder(&a, &g, &s, next).await;
            let mut count = None;
            if let Some(b) = &bumper {
                match increment_count(a.db(), &g, b, at).await {
                    Ok(c) => count = c,
                    Err(e) => bot_util::log_error!(format!("[bumps] {g}: could not count the bump of {b}"), bot_core::error::log_arg(&e)),
                }
            }
            Ok(Recorded { recorded: true, count, next_bump_at: Some(next) })
        })
    });
    let key = site_key.to_string();
    async move {
        match locked {
            Some(f) => f.await,
            None => Err(anyhow::anyhow!("Unknown bump site \"{key}\"")),
        }
    }
}

// ── The hook ──────────────────────────────────────────────────────────────────

/// The thank-you text for a bump (`sendThanks`'s render), or `None` when it renders empty.
pub fn thanks_text(template: Option<&str>, mut ctx: RenderContext, site_name: &str, count: Option<i64>, next: &DateTime<Utc>) -> Option<String> {
    let t = template.filter(|t| !t.is_empty()).unwrap_or(DEFAULT_THANKS_MESSAGE);
    ctx.vars = Some(vec![
        ("next".into(), VarValue::Text(format!("<t:{}:R>", unix(next)))),
        ("count".into(), count.map(|c| VarValue::Number(c as f64)).unwrap_or_else(|| VarValue::Text("?".into()))),
        ("site".into(), VarValue::Text(site_name.into())),
    ]);
    let text = render_placeholders(t, &ctx, Some(MESSAGE_MAX));
    (!bot_util::js::trim(&text).is_empty()).then_some(text)
}

#[allow(clippy::too_many_arguments)]
async fn send_thanks(app: &App, guild_id: GuildId, channel_id: ChannelId, bumper: &User, settings: &Settings, def: &BumpSite, count: Option<i64>, next: &DateTime<Utc>) -> Result<bool, IoError> {
    if !settings.bump_thanks {
        return Ok(false);
    }
    // checkChannelUsable asks for Send Messages in Threads when the bump was made in a thread.
    let usable = check_channel_usable(app, guild_id, Some(channel_id), &["ViewChannel", "SendMessages"]);
    if !usable.ok {
        let why = if usable.missing.is_empty() { usable.reason.clone().unwrap_or_default() } else { usable.missing.join(", ") };
        bot_util::log_debug!(format!("[bumps] {guild_id}: no thank-you in {channel_id} ({why})"));
        return Ok(false);
    }
    let member = cached_guild(app, guild_id).and_then(|g| g.members.get(&bumper.id).cloned());
    let ctx = render_context(app, Some(guild_id), bumper, member.as_ref(), Some(channel_id));
    let Some(text) = thanks_text(settings.bump_thanks_message.as_deref(), ctx, def.name, count, next) else { return Ok(false) };
    // Only the bumper can be pinged, whatever the admin's text contains.
    let payload = Payload::text(text).allowed_mentions(CreateAllowedMentions::new().users(vec![bumper.id]));
    io(app).send(channel_id, payload).await?;
    Ok(true)
}

async fn reward_bumper(app: &App, guild_id: GuildId, bumper: &User, channel_id: ChannelId, settings: &Settings) {
    let amount = i64::from(settings.bump_reward_coins).min(i64::from(bot_config::COIN_MAX));
    if amount < 1 {
        return;
    }
    let member = cached_guild(app, guild_id).and_then(|g| g.members.get(&bumper.id).cloned());
    let args = RewardArgs::new(guild_id, bumper.id.to_string(), amount).reason("Bumped the server").member(member).channel(Some(channel_id));
    let res = app.rewards().grant(app, RewardKind::Coins, args).await;
    // The economy being off or missing is normal; anything else is worth a trace.
    if !res.ok && !matches!(res.reason.as_deref(), Some("module-disabled") | Some("unavailable")) {
        bot_util::log_debug!(format!("[bumps] {guild_id}: the bump reward for {} failed ({})", bumper.id, res.reason.unwrap_or_default()));
    }
}

/// `onBotMessage(ctx)`: a bump site's answer to a successful bump is recorded, the bumper thanked and rewarded. Never
/// fails for Discord failures; a database failure while recording is returned (the hook runner logs it).
pub async fn on_bot_message(ctx: Arc<HookCtx<BotMessage>>) -> BotResult {
    let message = &ctx.data.message;
    let Some(site_key) = site_key_for_bot(&message.author.id.to_string()) else { return Ok(()) };
    let Some(def) = site(site_key) else { return Ok(()) };
    let guild_id = ctx.data.guild_id;
    let success = (def.is_success)(&message.embeds);
    if !success || !mark_seen(&ctx.app, &message.id.to_string()) {
        return Ok(());
    }
    let Some(settings) = ctx.get_settings().await else { return Ok(()) };
    if !is_module_enabled(Some(&settings), Some(ModuleKey::Bumps)) {
        return Ok(());
    }
    let bumper = bumper_of(message);
    let now = ctx.app.clock().now_ms();
    let created = message.timestamp.timestamp_millis();
    let at = ms_date(if created > 0 { created.min(now) } else { now });
    let channel_id = ctx.data.channel_id;
    let g = guild_id.to_string();
    let out = record_bump(&ctx.app, &g, site_key, &channel_id.to_string(), bumper.as_ref().map(|u| u.id.to_string()).as_deref(), at).await?;
    if !out.recorded {
        return Ok(());
    }
    let next = out.next_bump_at.unwrap_or(at);
    let who = bumper.as_ref().map(|u| u.id.to_string()).unwrap_or_else(|| "an unknown member".into());
    bot_util::log_info!(format!("[bumps] {g}: {site_key} bump by {who}, next at {}", bot_db::format_js_date(&next)));

    let Some(bumper) = bumper else { return Ok(()) };
    if let Err(e) = send_thanks(&ctx.app, guild_id, channel_id, &bumper, &settings, def, out.count, &next).await {
        bot_util::log_warn!(format!("[bumps] {g}: the thank-you failed ({})", e.message));
    }
    reward_bumper(&ctx.app, guild_id, &bumper, channel_id, &settings).await;
    Ok(())
}

// ── The reminder job ──────────────────────────────────────────────────────────

/// `reminderChannel(guild, ids)`: the first channel of `ids` the bot can post in; `None` when none. An uncached
/// channel is fetched over REST (`guild.channels.fetch`): a permanent Discord error skips it, any other error is
/// returned so the job retries.
async fn reminder_channel(app: &App, guild_id: GuildId, ids: &[String]) -> Result<Option<GuildChannel>, IoError> {
    use bot_core::interaction_helpers::{FetchChannelError, check_channel_usable_for, fetch_channel};
    for id in ids {
        let Some(c) = snowflake(id).map(ChannelId::new) else { continue };
        let channel = match fetch_channel(app, guild_id, c).await {
            Ok(ch) => ch,
            Err(FetchChannelError::Rest(e)) => {
                let e = IoError::from(e);
                if e.code.is_some_and(|code| PERMANENT_CODES.contains(&code)) {
                    continue;
                }
                return Err(e);
            }
            // discord.js throws GuildChannelUnowned (no numeric code), which the job retries.
            Err(FetchChannelError::Unowned) => return Err(IoError::new(None, "The fetched channel does not belong to this manager's guild.")),
            // No REST client: nothing can be posted anyway.
            Err(FetchChannelError::Offline) => continue,
        };
        if check_channel_usable_for(app, guild_id, &channel, &["ViewChannel", "SendMessages"]).ok {
            return Ok(Some(channel));
        }
    }
    Ok(None)
}

/// The reminder's text and the roles it may ping: `(content, roles)`.
pub fn reminder_content(template: Option<&str>, ctx: RenderContext, site_name: &str, pingable: &[String]) -> String {
    let mut ctx = ctx;
    ctx.vars = Some(vec![("site".into(), VarValue::Text(site_name.into()))]);
    let t = template.filter(|t| !t.is_empty()).unwrap_or(DEFAULT_REMINDER_MESSAGE);
    let rendered = render_placeholders(t, &ctx, Some(MESSAGE_MAX));
    let trimmed = bot_util::js::trim(&rendered);
    let text = if trimmed.is_empty() { DEFAULT_REMINDER_MESSAGE } else { trimmed };
    let content = match pingable.first() {
        Some(r) => format!("<@&{r}> {text}"),
        None => text.to_string(),
    };
    bot_util::js::utf16_prefix(&content, 2000).to_string()
}

/// `sendReminder(...)`: posts the reminder for a state row. `Ok(false)` when there is nowhere to post it; `Err` on
/// a Discord error.
async fn send_reminder(app: &App, guild_id: GuildId, settings: &Settings, state: &BumpState, def: &BumpSite) -> Result<bool, IoError> {
    let mut ids: Vec<String> = Vec::new();
    for id in [settings.bump_channel_id.clone(), Some(state.channel_id.clone())].into_iter().flatten() {
        if !id.is_empty() && !ids.contains(&id) {
            ids.push(id);
        }
    }
    let Some(channel) = reminder_channel(app, guild_id, &ids).await? else { return Ok(false) };
    let Some(guild) = cached_guild(app, guild_id) else { return Ok(false) };
    let ctx = RenderContext { guild: Some(placeholder_guild(&guild)), channel: Some(placeholder_channel(&channel)), ..Default::default() };
    // The role is pinged only when the bot may ping it (mentionable, or Mention Everyone), and only that role ever is.
    let roles: Vec<String> = match (settings.bump_role_id.as_deref().filter(|r| !r.is_empty()), crate::io::bot_user_id(app)) {
        (Some(role), Some(bot)) => {
            let my_roles = guild.members.get(&bot).map(|m| m.roles.clone()).unwrap_or_default();
            pingable_role_ids(&guild, &channel, Some((bot, &my_roles)), &[role]).allowed
        }
        (Some(role), None) => pingable_role_ids(&guild, &channel, None, &[role]).allowed,
        _ => Vec::new(),
    };
    let content = reminder_content(settings.bump_reminder_message.as_deref(), ctx, def.name, &roles);
    let role_ids: Vec<serenity::all::RoleId> = roles.iter().filter_map(|r| snowflake(r)).map(serenity::all::RoleId::new).collect();
    let payload = Payload::text(content).allowed_mentions(CreateAllowedMentions::new().roles(role_ids));
    io(app).send(channel.id, payload).await?;
    Ok(true)
}

/// Reads one guild's state row for a site.
pub async fn get_bump_state(pool: &PgPool, guild_id: &str, site: &str) -> anyhow::Result<Option<BumpState>> {
    Ok(sqlx::query_as::<_, BumpState>(r#"SELECT * FROM "BumpState" WHERE "guildId" = $1 AND "site" = $2"#)
        .bind(guild_id)
        .bind(site)
        .fetch_optional(pool)
        .await
        .map_err(bot_db::DbError::from)?)
}

/// `handleRemindJob(job, ctx)`: when the cooldown has ended and the module is on, posts the reminder once.
/// Idempotent: `remindedAt` is claimed before the post (a retry never reminds twice) and released again when nothing
/// was posted.
pub async fn handle_remind_job(job: Job, ctx: JobCtx) -> BotResult<JobOutcome> {
    let site_value = job.payload.get("site");
    let def = site_value.and_then(Value::as_str).and_then(site);
    let Some(def) = def else {
        let shown = site_value.map(|v| v.to_string()).unwrap_or_else(|| "undefined".into());
        return Err(PermanentJobError::new(format!("bump.remind: unknown site {shown}")).into());
    };
    let app = &ctx.app;
    let guild_id = job.guild_id.clone();
    let settings = app.settings().get_settings(&guild_id).await?;
    if !is_module_enabled(Some(&settings), Some(ModuleKey::Bumps)) {
        return Ok(JobOutcome::Done);
    }
    let Some(state) = get_bump_state(app.db(), &guild_id, def.key).await? else { return Ok(JobOutcome::Done) };
    if state.reminded_at.is_some() {
        return Ok(JobOutcome::Done);
    }
    // The state moved on (a later bump): run again when it says the next bump is possible.
    if state.next_bump_at.timestamp_millis() - app.clock().now_ms() > EARLY_SLACK_MS {
        return Ok(JobOutcome::Reschedule { next_run_at: state.next_bump_at, payload: None });
    }

    let claimed_at = app.clock().now();
    let claim = sqlx::query(r#"UPDATE "BumpState" SET "remindedAt" = $3 WHERE "guildId" = $1 AND "site" = $2 AND "remindedAt" IS NULL AND "lastBumpAt" = $4"#)
        .bind(&guild_id)
        .bind(def.key)
        .bind(claimed_at.naive_utc())
        .bind(state.last_bump_at.naive_utc())
        .execute(app.db())
        .await
        .map_err(bot_db::DbError::from)?;
    if claim.rows_affected() == 0 {
        return Ok(JobOutcome::Done);
    }
    let release = || async {
        let r = sqlx::query(r#"UPDATE "BumpState" SET "remindedAt" = NULL WHERE "guildId" = $1 AND "site" = $2 AND "remindedAt" = $3"#)
            .bind(&guild_id)
            .bind(def.key)
            .bind(claimed_at.naive_utc())
            .execute(app.db())
            .await;
        if let Err(e) = r {
            bot_util::log_warn!(format!("[bumps] {guild_id}: could not release the reminder claim ({e})"));
        }
    };

    let gid = GuildId::new(snowflake(&guild_id).unwrap_or(1));
    let sent = match send_reminder(app, gid, &settings, &state, def).await {
        Ok(s) => s,
        Err(e) => {
            release().await;
            if e.code.is_some_and(|c| PERMANENT_CODES.contains(&c)) {
                warn_limited(app, &guild_id, format!("[bumps] {guild_id}: the reminder could not be posted ({})", e.message));
                return Ok(JobOutcome::Done);
            }
            return Err(e.into());
        }
    };
    if !sent {
        release().await;
        warn_limited(
            app,
            &guild_id,
            format!("[bumps] {guild_id}: no channel to post the bump reminder in (set one with /bumps setup, and give me View Channel and Send Messages there)"),
        );
        return Ok(JobOutcome::Done);
    }
    bot_util::log_info!(format!("[bumps] {guild_id}: {} reminder posted", def.key));
    Ok(JobOutcome::Done)
}

// ── Cleanup hooks ─────────────────────────────────────────────────────────────

/// `channelDeleteFilter(ctx)`: skipped once the memoised settings show another channel (no memo: runs).
pub fn channel_delete_filter(app: &App, guild_id: &str, channel_id: &str) -> bool {
    match app.settings().peek_settings(guild_id) {
        None => true,
        Some(s) => s.bump_channel_id.as_deref() == Some(channel_id),
    }
}

/// `roleDeleteFilter(ctx)`, the same idea.
pub fn role_delete_filter(app: &App, guild_id: &str, role_id: &str) -> bool {
    match app.settings().peek_settings(guild_id) {
        None => true,
        Some(s) => s.bump_role_id.as_deref() == Some(role_id),
    }
}

/// `onChannelDelete(ctx)`: the reminder channel is unset (reminders then go to the channel of the last bump).
pub async fn on_channel_delete(app: &App, guild_id: &str, channel_id: &str) {
    let run = async {
        let settings = app.settings().get_settings_with(guild_id, true).await?;
        if settings.bump_channel_id.as_deref() != Some(channel_id) {
            return Ok::<_, anyhow::Error>(());
        }
        app.settings().update_settings(guild_id, SettingsPatch::new().set("bumpChannelId", Value::Null)).await?;
        bot_util::log_info!(format!("[bumps] {guild_id}: the reminder channel was deleted, so it is unset"));
        Ok(())
    };
    if let Err(e) = run.await {
        bot_util::log_error!(format!("[bumps] {guild_id}: channel delete cleanup failed"), bot_core::error::log_arg(&e));
    }
}

/// `onRoleDelete(ctx)`: the role to ping is unset.
pub async fn on_role_delete(app: &App, guild_id: &str, role_id: &str) {
    let run = async {
        let settings = app.settings().get_settings_with(guild_id, true).await?;
        if settings.bump_role_id.as_deref() != Some(role_id) {
            return Ok::<_, anyhow::Error>(());
        }
        app.settings().update_settings(guild_id, SettingsPatch::new().set("bumpRoleId", Value::Null)).await?;
        bot_util::log_info!(format!("[bumps] {guild_id}: the role to ping was deleted, so it is unset"));
        Ok(())
    };
    if let Err(e) = run.await {
        bot_util::log_error!(format!("[bumps] {guild_id}: role delete cleanup failed"), bot_core::error::log_arg(&e));
    }
}

// ── Reads and resets for the command ──────────────────────────────────────────

/// `getBumpStates(guildId)`: the guild's state rows, one per site that has seen a bump.
pub async fn get_bump_states(pool: &PgPool, guild_id: &str) -> anyhow::Result<Vec<BumpState>> {
    Ok(sqlx::query_as::<_, BumpState>(r#"SELECT * FROM "BumpState" WHERE "guildId" = $1"#).bind(guild_id).fetch_all(pool).await.map_err(bot_db::DbError::from)?)
}

/// `topBumpers(guildId, limit)`: most bumps first, the earlier last bump wins a tie.
pub async fn top_bumpers(pool: &PgPool, guild_id: &str, limit: i64) -> anyhow::Result<Vec<BumpCount>> {
    Ok(sqlx::query_as::<_, BumpCount>(r#"SELECT * FROM "BumpCount" WHERE "guildId" = $1 AND "count" > 0 ORDER BY "count" DESC, "lastBumpAt" ASC LIMIT $2"#)
        .bind(guild_id)
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(bot_db::DbError::from)?)
}

/// `bumpCountOf(guildId, userId)`: one member's count (0 when none).
pub async fn bump_count_of(pool: &PgPool, guild_id: &str, user_id: &str) -> anyhow::Result<i64> {
    let row: Option<(i32,)> = sqlx::query_as(r#"SELECT "count" FROM "BumpCount" WHERE "guildId" = $1 AND "userId" = $2"#)
        .bind(guild_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .map_err(bot_db::DbError::from)?;
    Ok(row.map(|r| i64::from(r.0)).unwrap_or(0))
}

/// `resetCounts(guildId, userId)`: clears one member's bump count, or everyone's. States and reminders are left
/// alone. Returns the rows removed.
pub async fn reset_counts(pool: &PgPool, guild_id: &str, user_id: Option<&str>) -> anyhow::Result<u64> {
    let res = match user_id {
        Some(u) => sqlx::query(r#"DELETE FROM "BumpCount" WHERE "guildId" = $1 AND "userId" = $2"#).bind(guild_id).bind(u).execute(pool).await,
        None => sqlx::query(r#"DELETE FROM "BumpCount" WHERE "guildId" = $1"#).bind(guild_id).execute(pool).await,
    };
    Ok(res.map_err(bot_db::DbError::from)?.rows_affected())
}
