//! Port of `src/services/giveawayService.js`: the logic behind `/giveaway`, the persistent Participate button, the
//! `messageDelete` / `messageDeleteBulk` hooks and the `giveaway.end` job. Pure helpers (weights, the draw, packing,
//! embeds, rows, references, end-time validation) come first, then the database and Discord flows.
//!
//! Concurrency, as in the JS: every state change of one giveaway runs inside the giveaway's keyed lock
//! ([`GiveawayLocks`], in `app.ext`) and every status transition is a conditional `UPDATE` whose `WHERE` names the
//! expected status, so two processes that briefly overlap cannot both claim the same transition. A Participate click
//! never takes the lock: it is a single row operation guarded by `@@unique([giveawayId, userId])`.
//!
//! Errors: functions that the JS lets throw return `anyhow::Result<Result<T, GiveawayError>>`, the inner `Err` being
//! the JS `{ ok: false, code }`. [`GiveawayError::code`] is the JS code string.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;

use bot_config::{ModuleKey, colors, custom_id, limits};
use bot_core::embed_builder::{error_embed, guild_embed, info_embed, success_embed, warn_embed};
use bot_core::interaction_helpers::{Payload, safe_defer, safe_reply};
use bot_core::prelude::*;
use bot_core::rewards::RewardArgs;
use bot_db::sqlx::{self, AssertSqlSafe, Row};
use bot_db::{GIVEAWAY_ENTRY_INFO, GIVEAWAY_INFO, Giveaway, GiveawayStatus, GuildSettings, InsertBuilder};
use bot_util::text::{clamp_text, format_number, join_with_limit_default};
use bot_util::time::discord_timestamp;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use serenity::all::{ButtonStyle, GuildChannel, Member, ReactionType, Timestamp};
use serenity::builder::{CreateActionRow, CreateButton, CreateEmbed};

use crate::discord_ops::{DiscordError, SentMessage, ops};
use crate::guild_view::{self, channel_usable, fetch_guild_channel, guild_id_of};

// ── Constants (pkg-giveaways §3.4) ───────────────────────────────────────────────

pub const END_JOB_TYPE: &str = "giveaway.end";
/// `endJobKey(giveawayId)`.
pub fn end_job_key(giveaway_id: &str) -> String {
    format!("giveaway.end:{giveaway_id}")
}
/// `ctx.attempt >= END_MAX_ATTEMPTS` is the final attempt.
pub const END_MAX_ATTEMPTS: i32 = 5;
pub const MIN_DURATION_MS: i64 = 60_000;
pub const MAX_DURATION_MS: i64 = 90 * 86_400_000;
pub const MAX_ROLE_WEIGHTS: usize = 10;
pub const MIN_BONUS_WEIGHT: i64 = 2;
pub const MAX_WEIGHT: i64 = 10;
pub const DEFAULT_BONUS_WEIGHT: i64 = 2;
pub const COUNT_REFRESH_MS: i64 = 10_000;
/// Members looked up per resolver call (discord.js's `user_ids` limit of a member chunk request).
pub const DRAW_BATCH: usize = 100;
pub const DRAW_BUDGET_MS: i64 = 30_000;
pub const MEMBER_FETCH_TIMEOUT_MS: u64 = 15_000;
pub const ANNOUNCE_CHUNK: usize = 1700;
pub const ENTRANTS_SHOWN: i64 = 1000;
pub const LIST_SHOWN: i64 = 250;
pub const MAX_PRIZE_AMOUNT: i64 = 1_000_000;
pub const NOT_DUE_SLACK_MS: i64 = 60_000;

/// `STATUS_TEXT`.
pub fn status_text(s: GiveawayStatus) -> &'static str {
    match s {
        GiveawayStatus::Active => "still running",
        GiveawayStatus::Draft => "a draft",
        GiveawayStatus::Ended => "already ended",
        GiveawayStatus::Cancelled => "cancelled",
    }
}
/// `STATUS_TEXT_SHORT`.
pub fn status_text_short(s: GiveawayStatus) -> &'static str {
    match s {
        GiveawayStatus::Active => "running",
        GiveawayStatus::Draft => "draft",
        GiveawayStatus::Ended => "ended",
        GiveawayStatus::Cancelled => "cancelled",
    }
}
/// `STATUS_EMOJI`.
pub fn status_emoji(s: GiveawayStatus) -> &'static str {
    match s {
        GiveawayStatus::Active => "🟢",
        GiveawayStatus::Draft => "📝",
        GiveawayStatus::Ended => "🏁",
        GiveawayStatus::Cancelled => "🚫",
    }
}

/// Permissions the bot needs in the giveaway channel to post, edit and reply.
pub const POST_NEEDS: [&str; 4] = ["ViewChannel", "SendMessages", "EmbedLinks", "ReadMessageHistory"];

/// The per-giveaway lock (`createKeyedMutex()`), one per app; also `start:{guildId}`.
#[derive(Default)]
pub struct GiveawayLocks(pub bot_core::sync::KeyedMutex);

fn run_locked<T, F, Fut>(app: &App, key: &str, f: F) -> impl std::future::Future<Output = T> + Send + 'static + use<T, F, Fut>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    app.ext::<GiveawayLocks>().0.run(key, f)
}

// ── Results ─────────────────────────────────────────────────────────────────────

/// The JS `{ ok: false, code, … }`.
#[derive(Debug, Clone, PartialEq)]
pub enum GiveawayError {
    Limit,
    SendFailed(DiscordError),
    DbError(String),
    NotFound,
    NotActive,
    NotDue { next_run_at: DateTime<Utc> },
    ResolveFailed,
    Drafted,
    NoAccess { missing: Vec<String>, reason: Option<String> },
    NotEnded,
    NoEntrants,
    Conflict,
    NotOpen,
    NotDraft,
    NotSet,
}

impl GiveawayError {
    /// The JS code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Limit => "LIMIT",
            Self::SendFailed(_) => "SEND_FAILED",
            Self::DbError(_) => "DB_ERROR",
            Self::NotFound => "NOT_FOUND",
            Self::NotActive => "NOT_ACTIVE",
            Self::NotDue { .. } => "NOT_DUE",
            Self::ResolveFailed => "RESOLVE_FAILED",
            Self::Drafted => "DRAFTED",
            Self::NoAccess { .. } => "NO_ACCESS",
            Self::NotEnded => "NOT_ENDED",
            Self::NoEntrants => "NO_ENTRANTS",
            Self::Conflict => "CONFLICT",
            Self::NotOpen => "NOT_OPEN",
            Self::NotDraft => "NOT_DRAFT",
            Self::NotSet => "NOT_SET",
        }
    }
}

// ── Pure helpers ─────────────────────────────────────────────────────────────────

/// `SNOWFLAKE_RE` (`/^\d{17,20}$/`).
pub fn is_snowflake(s: &str) -> bool {
    (17..=20).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

/// One bonus-odds entry of `Giveaway.roleWeights`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoleWeight {
    pub role_id: String,
    pub weight: i64,
}

/// JS `Number.isInteger(v)` for a JSON value.
fn js_integer(v: &Value) -> Option<i64> {
    let f = v.as_f64()?;
    (f.is_finite() && f.fract() == 0.0).then_some(f as i64)
}

/// `normalizeRoleWeights(raw)`: keeps `{ roleId, weight }` entries with a snowflake roleId and an integer weight
/// 1..10, de-duplicates by roleId (highest weight wins), sorts by weight descending then roleId, keeps the first 10.
pub fn normalize_role_weights(raw: &Value) -> Vec<RoleWeight> {
    let Some(arr) = raw.as_array() else { return Vec::new() };
    let mut best: Vec<(String, i64)> = Vec::new();
    for entry in arr {
        let Some(obj) = entry.as_object() else { continue };
        let Some(role_id) = obj.get("roleId").and_then(Value::as_str) else { continue };
        if !is_snowflake(role_id) {
            continue;
        }
        let Some(weight) = obj.get("weight").and_then(js_integer) else { continue };
        if !(1..=MAX_WEIGHT).contains(&weight) {
            continue;
        }
        match best.iter_mut().find(|(r, _)| r == role_id) {
            Some(e) => {
                if e.1 < weight {
                    e.1 = weight;
                }
            }
            None => best.push((role_id.to_string(), weight)),
        }
    }
    let mut out: Vec<RoleWeight> = best.into_iter().map(|(role_id, weight)| RoleWeight { role_id, weight }).collect();
    out.sort_by(|a, b| b.weight.cmp(&a.weight).then_with(|| a.role_id.cmp(&b.role_id)));
    out.truncate(MAX_ROLE_WEIGHTS);
    out
}

/// [`normalize_role_weights`] of a typed list.
pub fn normalize_weights_list(list: &[RoleWeight]) -> Vec<RoleWeight> {
    normalize_role_weights(&serde_json::to_value(list).unwrap_or(Value::Null))
}

/// `memberWeight(hasRole, weights)`: the highest matching weight wins; 1 is the default.
pub fn member_weight(has_role: impl Fn(&str) -> bool, weights: &[RoleWeight]) -> i64 {
    let mut w = 1;
    for e in weights {
        if e.weight > w && has_role(&e.role_id) {
            w = e.weight;
        }
    }
    w
}

/// `meetsRequirements(hasRole, requiredRoleIds)`: every required role is held.
pub fn meets_requirements<S: AsRef<str>>(has_role: impl Fn(&str) -> bool, required: &[S]) -> bool {
    required.iter().all(|id| has_role(id.as_ref()))
}

/// `heldDeniedRole(hasRole, deniedRoleIds)`: the first denied role the member holds.
pub fn held_denied_role<S: AsRef<str>>(has_role: impl Fn(&str) -> bool, denied: &[S]) -> Option<String> {
    denied.iter().find(|id| has_role(id.as_ref())).map(|s| s.as_ref().to_string())
}

/// `secureRandom()`: a uniform number in (0, 1) from the OS-seeded CSPRNG.
pub fn secure_random() -> f64 {
    let n: u64 = rand::random_range(1..(1u64 << 48));
    n as f64 / (1u64 << 48) as f64
}

/// What [`draw_winners`] returns.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DrawResult {
    pub winner_ids: Vec<String>,
    pub complete: bool,
    pub unknown: usize,
}

/// A resolver answer: `Some(weight)` eligible, `None` ineligible; an id missing from the map is unknown.
pub type Resolved = HashMap<String, Option<i64>>;

/// `drawWinners({ userIds, count, maxWeight, resolve, rng, batchSize, deadline, now })`: exact weighted sampling
/// without replacement (Efraimidis-Spirakis A-ES), evaluated lazily so the member lookups stop as soon as no unvisited
/// entrant can still enter the top `count`.
#[allow(clippy::too_many_arguments)]
pub async fn draw_winners<R, Fut>(
    user_ids: &[String],
    count: i64,
    max_weight: i64,
    mut resolve: R,
    mut rng: impl FnMut() -> f64,
    batch_size: usize,
    deadline_ms: f64,
    now_ms: impl Fn() -> f64,
) -> DrawResult
where
    R: FnMut(Vec<String>) -> Fut,
    Fut: std::future::Future<Output = Resolved>,
{
    let mut ids: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for id in user_ids {
        if seen.insert(id.clone()) {
            ids.push(id.clone());
        }
    }
    if count <= 0 || ids.is_empty() {
        return DrawResult { winner_ids: Vec::new(), complete: true, unknown: 0 };
    }
    let count = count as usize;
    let w = max_weight.max(1) as f64;
    struct Item {
        id: String,
        u: f64,
        ub: f64,
    }
    let mut items: Vec<Item> = ids
        .into_iter()
        .map(|id| {
            let u = rng();
            Item { id, u, ub: u.powf(1.0 / w) }
        })
        .collect();
    items.sort_by(|a, b| b.ub.partial_cmp(&a.ub).unwrap_or(std::cmp::Ordering::Equal));

    let mut best: Vec<(String, f64)> = Vec::new();
    let mut unknown = 0usize;
    let mut i = 0usize;
    let batch_size = batch_size.max(1);
    while i < items.len() {
        if best.len() == count && best[count - 1].1 >= items[i].ub {
            break;
        }
        if now_ms() > deadline_ms {
            return DrawResult { winner_ids: best.into_iter().map(|b| b.0).collect(), complete: false, unknown };
        }
        let end = (i + batch_size).min(items.len());
        let batch = &items[i..end];
        i = end;
        let res = resolve(batch.iter().map(|b| b.id.clone()).collect()).await;
        for b in batch {
            let Some(r) = res.get(&b.id) else {
                unknown += 1;
                continue;
            };
            let Some(weight) = r else { continue };
            let key = b.u.powf(1.0 / *weight as f64);
            let mut at = best.len();
            while at > 0 && best[at - 1].1 < key {
                at -= 1;
            }
            if at < count {
                best.insert(at, (b.id.clone(), key));
                if best.len() > count {
                    best.pop();
                }
            }
        }
    }
    DrawResult { winner_ids: best.into_iter().map(|b| b.0).collect(), complete: true, unknown }
}

fn weights_of(g: &Giveaway) -> Vec<RoleWeight> {
    normalize_role_weights(g.role_weights.as_ref().unwrap_or(&Value::Null))
}

/// `maxWeightFor(g, guild)`: the largest weight a member can have; deleted bonus roles are ignored.
pub fn max_weight_for(g: &Giveaway, role_exists: impl Fn(&str) -> bool) -> i64 {
    weights_of(g).into_iter().filter(|w| role_exists(&w.role_id)).map(|w| w.weight).fold(1, i64::max)
}

/// The eligibility rule of `makeResolver`: `None` for a bot or a member who misses a required role or holds a
/// denied one, else the member's weight.
pub fn evaluate_member(is_bot: bool, member_roles: &[String], required: &[String], denied: &[String], weights: &[RoleWeight]) -> Option<i64> {
    if is_bot {
        return None;
    }
    let has = |r: &str| member_roles.iter().any(|m| m == r);
    if !meets_requirements(has, required) {
        return None;
    }
    if held_denied_role(has, denied).is_some() {
        return None;
    }
    Some(member_weight(has, weights))
}

fn member_eval(m: &Member, required: &[String], denied: &[String], weights: &[RoleWeight]) -> Option<i64> {
    let roles: Vec<String> = m.roles.iter().map(|r| r.to_string()).collect();
    evaluate_member(m.user.bot, &roles, required, denied, weights)
}

/// `makeResolver(guild, g)`: members from the cache, the rest over REST (see `gaps`: discord.js sends one gateway
/// chunk request per batch). A deleted required or denied role no longer restricts anyone. A member that is not in
/// the guild is ineligible (`None`); a failed lookup leaves the id out, which counts as unknown.
pub fn make_resolver(app: &App, guild_id: &str, g: &Giveaway) -> impl FnMut(Vec<String>) -> futures::future::BoxFuture<'static, Resolved> + Send + 'static {
    let exists = guild_view::role_exists_fn(app, guild_id);
    let required: Vec<String> = g.required_role_ids.iter().filter(|id| exists(id)).cloned().collect();
    let denied: Vec<String> = g.denied_role_ids.iter().filter(|id| exists(id)).cloned().collect();
    let weights: Vec<RoleWeight> = weights_of(g).into_iter().filter(|w| exists(&w.role_id)).collect();
    let app = app.clone();
    let guild_id = guild_id.to_string();
    move |ids: Vec<String>| {
        let (app, guild_id, required, denied, weights) = (app.clone(), guild_id.clone(), required.clone(), denied.clone(), weights.clone());
        Box::pin(async move {
            let mut out = Resolved::new();
            let mut misses = Vec::new();
            for id in ids {
                match guild_view::cached_member(&app, &guild_id, &id) {
                    Some(m) => {
                        out.insert(id, member_eval(&m, &required, &denied, &weights));
                    }
                    None => misses.push(id),
                }
            }
            if !misses.is_empty() && !crate::discord_ops::has_override(&app) && app.discord().is_some() {
                // `guild.members.fetch({ user: misses, time })`: one gateway member request for the batch. An id
                // missing from a successful answer has left the server; a failed request leaves every id unknown.
                let users: Vec<serenity::all::UserId> =
                    misses.iter().filter_map(|id| guild_view::snowflake(id)).map(serenity::all::UserId::new).collect();
                let fetched = match guild_id_of(&guild_id) {
                    Some(g) => bot_core::guild_members::fetch_members_by_ids(&app, g, &users, MEMBER_FETCH_TIMEOUT_MS).await.map_err(|e| e.to_string()),
                    None => Err("invalid guild id".to_string()),
                };
                match fetched {
                    Ok(found) => {
                        for id in misses {
                            let member = guild_view::snowflake(&id).and_then(|u| found.get(&serenity::all::UserId::new(u)));
                            let r = member.and_then(|m| member_eval(m, &required, &denied, &weights));
                            out.insert(id, r);
                        }
                    }
                    Err(e) => {
                        bot_util::log_warn!(
                            format!("Giveaway member lookup failed for {} entrant(s) in guild {guild_id}", misses.len()),
                            err_arg(&e)
                        );
                    }
                }
            } else if !misses.is_empty() {
                // A host with an installed DiscordOps (tests, no gateway): one REST lookup per member.
                let o = ops(&app);
                let gid = guild_id.clone();
                let lookups = bot_util::async_util::map_limit(misses.clone(), 5, |id: String, _| {
                    let (o, gid) = (o.clone(), gid.clone());
                    async move {
                        let r = tokio::time::timeout(std::time::Duration::from_millis(MEMBER_FETCH_TIMEOUT_MS), o.fetch_member(&gid, &id)).await;
                        match r {
                            Ok(Ok(m)) => Ok((id, m)),
                            Ok(Err(e)) => Err(e),
                            Err(_) => Err(DiscordError::new(None, "Members didn't arrive in time.")),
                        }
                    }
                })
                .await;
                let mut failed = 0usize;
                let mut first_err = None;
                for r in lookups {
                    match r {
                        Ok((id, Some(m))) => {
                            out.insert(id, member_eval(&m, &required, &denied, &weights));
                        }
                        Ok((id, None)) => {
                            out.insert(id, None);
                        }
                        Err(e) => {
                            failed += 1;
                            first_err.get_or_insert(e);
                        }
                    }
                }
                if failed > 0 {
                    bot_util::log_warn!(
                        format!("Giveaway member lookup failed for {failed} entrant(s) in guild {guild_id}"),
                        err_arg(&first_err.unwrap_or_else(DiscordError::offline))
                    );
                }
            }
            out
        })
    }
}

pub(crate) fn err_arg(e: &dyn std::fmt::Display) -> bot_util::logger::LogArg {
    bot_util::logger::LogArg::Error { message: e.to_string(), stack: None }
}

/// `packMentions(userIds, maxLen)`: `<@id>` mentions joined with ', ', packed greedily into chunks of at most
/// `max_len` characters; a mention is never split.
pub fn pack_mentions<S: AsRef<str>>(user_ids: &[S], max_len: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for id in user_ids {
        let m = format!("<@{}>", id.as_ref());
        if cur.is_empty() {
            cur = m;
        } else if cur.len() + 2 + m.len() <= max_len {
            cur.push_str(", ");
            cur.push_str(&m);
        } else {
            chunks.push(std::mem::replace(&mut cur, m));
        }
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

/// `idsIn(chunk)`: the user ids mentioned in a chunk.
pub fn ids_in(chunk: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = chunk;
    while let Some(at) = rest.find("<@") {
        let after = &rest[at + 2..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() && after[digits.len()..].starts_with('>') {
            out.push(digits.clone());
            rest = &after[digits.len() + 1..];
        } else {
            rest = after;
        }
    }
    out
}

/// What [`parse_giveaway_ref`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GiveawayRef {
    Id(String),
    Message { message_id: String, guild_id: Option<String> },
}

/// `parseGiveawayRef(input)`: an id, a message id or a message link.
pub fn parse_giveaway_ref(input: &str) -> Option<GiveawayRef> {
    let s = bot_util::js::trim(input);
    if let Some(r) = parse_message_link(s) {
        return Some(r);
    }
    if is_snowflake(s) {
        return Some(GiveawayRef::Message { message_id: s.to_string(), guild_id: None });
    }
    let b = s.as_bytes();
    if (21..=32).contains(&b.len()) && b[0] == b'c' && b[1..].iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()) {
        return Some(GiveawayRef::Id(s.to_string()));
    }
    None
}

fn parse_message_link(s: &str) -> Option<GiveawayRef> {
    let rest = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://"))?;
    let rest = rest.strip_prefix("ptb.").or_else(|| rest.strip_prefix("canary.")).unwrap_or(rest);
    let rest = rest.strip_prefix("discord.com/").or_else(|| rest.strip_prefix("discordapp.com/"))?;
    let rest = rest.strip_prefix("channels/")?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() != 3 || !parts.iter().all(|p| is_snowflake(p)) {
        return None;
    }
    Some(GiveawayRef::Message { message_id: parts[2].to_string(), guild_id: Some(parts[0].to_string()) })
}

/// `resolveGiveawayRef(guildId, input)`: looks a giveaway up in this guild by id or message id/link.
pub async fn resolve_giveaway_ref(db: &bot_db::PgPool, guild_id: &str, input: &str) -> anyhow::Result<Option<Giveaway>> {
    let Some(r) = parse_giveaway_ref(input) else { return Ok(None) };
    let (col, value) = match r {
        GiveawayRef::Message { guild_id: Some(g), .. } if g != guild_id => return Ok(None),
        GiveawayRef::Id(id) => ("id", id),
        GiveawayRef::Message { message_id, .. } => ("messageId", message_id),
    };
    let sql = format!(r#"SELECT * FROM "Giveaway" WHERE "{col}" = $1 AND "guildId" = $2 LIMIT 1"#);
    Ok(sqlx::query_as::<_, Giveaway>(AssertSqlSafe(sql)).bind(value).bind(guild_id).fetch_optional(db).await?)
}

/// `validateEndsAt(endsAt, now, tz)`: `Err((title, message))` when the end time is unusable.
pub fn validate_ends_at(ends_at: Option<DateTime<Utc>>, now: DateTime<Utc>, tz: &str) -> Result<(), (String, String)> {
    let Some(ends_at) = ends_at else {
        return Err((
            "Invalid Duration".into(),
            format!("I couldn't understand that. Use a length like `30m`, `2h` or `3d`, or an end time like `2026-12-24 18:00` (server timezone: {tz})."),
        ));
    };
    let diff = (ends_at - now).num_milliseconds();
    if diff <= 0 {
        return Err(("Invalid Duration".into(), format!("That end time is in the past (server timezone: {tz}).")));
    }
    if diff < MIN_DURATION_MS {
        return Err(("Too Short".into(), "A giveaway must run for at least 1 minute.".into()));
    }
    if diff > MAX_DURATION_MS {
        return Err(("Too Long".into(), "A giveaway can run for at most 90 days.".into()));
    }
    Ok(())
}

fn prize_kind_label(kind: RewardKind) -> &'static str {
    match kind {
        RewardKind::Xp => "XP",
        RewardKind::Coins => "Coin",
    }
}

/// The pure decision of `rewardUnavailableMessage`, given whether a provider is installed and the reward is usable.
pub fn reward_unavailable_text(kind: RewardKind, has_provider: bool, available: bool) -> Option<String> {
    if available {
        return None;
    }
    let key = kind.module();
    let m = key.info();
    let label = prize_kind_label(kind);
    if !has_provider {
        return Some(format!("{label} prizes are not available on this bot (the {} {} module is not installed).", m.emoji, m.label));
    }
    Some(format!("{label} prizes need the **{} {}** module. Enable it with `/module enable {}`.", m.emoji, m.label, key.as_str()))
}

/// `rewardUnavailableMessage(kind, settings)`: why a prize of this kind cannot be paid right now.
pub fn reward_unavailable_message(app: &App, kind: RewardKind, settings: Option<&GuildSettings>) -> Option<String> {
    let rewards = app.rewards();
    reward_unavailable_text(kind, rewards.has_provider(kind), rewards.is_reward_available(kind, settings))
}

/// A row of `/giveaway list` (`select { id, prize, status, channelId, endsAt, endedAt, _count.entries }`).
#[derive(Debug, Clone, PartialEq)]
pub struct ListRow {
    pub id: String,
    pub prize: String,
    pub status: GiveawayStatus,
    pub channel_id: String,
    pub ends_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub entries: i64,
}

/// `formatListLine(row)`: one line of `/giveaway list`.
pub fn format_list_line(row: &ListRow) -> String {
    let n = row.entries;
    let when = match row.status {
        GiveawayStatus::Active => format!("ends {}", discord_timestamp(&row.ends_at, "R")),
        GiveawayStatus::Draft => "draft".to_string(),
        GiveawayStatus::Ended => format!("ended {}", discord_timestamp(&row.ended_at.unwrap_or(row.ends_at), "R")),
        GiveawayStatus::Cancelled => "cancelled".to_string(),
    };
    format!(
        "{} **{}** · <#{}> · {when} · {} {}\n`{}`",
        status_emoji(row.status),
        clamp_text(&row.prize, 80),
        row.channel_id,
        format_number(n as f64),
        if n == 1 { "entry" } else { "entries" },
        row.id
    )
}

// ── Embeds and rows (pure) ────────────────────────────────────────────────────────

/// `buildJoinRow(giveawayId, count, { disabled })`: the Participate button row.
pub fn build_join_row(giveaway_id: &str, count: i64, disabled: bool) -> CreateActionRow {
    let id = custom_id!("giveaway", "join", giveaway_id).unwrap_or_else(|_| format!("giveaway:join:{giveaway_id}"));
    CreateActionRow::Buttons(vec![
        CreateButton::new(id)
            .style(ButtonStyle::Primary)
            .emoji(ReactionType::Unicode("🎉".into()))
            .label(format!("Participate ({})", format_number(count as f64)))
            .disabled(disabled),
    ])
}

fn currency(settings: Option<&GuildSettings>) -> (String, String) {
    (
        settings.map(|s| s.currency_emoji.clone()).unwrap_or_else(|| "🪙".into()),
        settings.map(|s| s.currency_name.clone()).unwrap_or_else(|| "coins".into()),
    )
}

fn xp_of(g: &Giveaway) -> i64 {
    i64::from(g.xp_prize.unwrap_or(0))
}

fn coins_of(g: &Giveaway) -> i64 {
    i64::from(g.coin_prize.unwrap_or(0))
}

fn prize_parts(g: &Giveaway, settings: Option<&GuildSettings>) -> Vec<String> {
    let mut parts = Vec::new();
    if xp_of(g) > 0 {
        parts.push(format!("`{} XP`", format_number(xp_of(g) as f64)));
    }
    if coins_of(g) > 0 {
        let (emoji, name) = currency(settings);
        parts.push(format!("`{emoji} {} {name}`", format_number(coins_of(g) as f64)));
    }
    parts
}

fn role_list(ids: &[String], role_exists: &dyn Fn(&str) -> bool) -> String {
    ids.iter().filter(|id| role_exists(id)).map(|id| format!("<@&{id}>")).collect::<Vec<_>>().join(", ")
}

fn ts(dt: &DateTime<Utc>) -> Timestamp {
    Timestamp::from(*dt)
}

/// `buildGiveawayEmbed(g, { settings, entryCount, roleExists, now })`: the giveaway message embed. ACTIVE (and DRAFT,
/// as a preview), ENDED and CANCELLED look different. Prize and description are rendered literally and clamped.
pub fn build_giveaway_embed(
    g: &Giveaway,
    settings: Option<&GuildSettings>,
    entry_count: i64,
    role_exists: &dyn Fn(&str) -> bool,
    now: DateTime<Utc>,
) -> CreateEmbed {
    let mut block: Vec<String> = Vec::new();
    if let Some(d) = g.description.as_deref().filter(|d| !d.is_empty()) {
        block.push(clamp_text(d, 1000));
        block.push(String::new());
    }
    let hosted = format!("**Hosted by:** <@{}>", g.host_id);
    let prize = prize_parts(g, settings);

    if g.status == GiveawayStatus::Cancelled {
        let mut lines = block;
        lines.push("This giveaway was cancelled.".into());
        lines.push(hosted);
        let footer = format!("ID: {} · Cancelled", g.id);
        return guild_embed(settings, Some(&clamp_text(&format!("🚫 {}", g.prize), 256)), Some(&lines.join("\n")), Some(&footer))
            .color(colors::NEUTRAL)
            .timestamp(ts(&g.ended_at.unwrap_or(now)));
    }

    if g.status == GiveawayStatus::Ended {
        let winners = if g.winner_ids.is_empty() {
            "No valid entrants.".to_string()
        } else {
            join_with_limit_default(&g.winner_ids.iter().map(|id| format!("<@{id}>")).collect::<Vec<_>>())
        };
        let mut lines = block;
        lines.push(format!("**Ended:** {}", discord_timestamp(&g.ended_at.unwrap_or(now), "R")));
        lines.push(hosted);
        lines.push(format!("**Winners:** {winners}"));
        lines.push(format!("**Entries:** {}", format_number(entry_count as f64)));
        if !prize.is_empty() {
            lines.push(format!("**Bonus prize:** {} each", prize.join(" + ")));
        }
        let footer = format!("ID: {} · Ended at", g.id);
        return guild_embed(settings, Some(&clamp_text(&format!("🎉 {}", g.prize), 256)), Some(&lines.join("\n")), Some(&footer))
            .color(colors::NEUTRAL)
            .timestamp(ts(&g.ended_at.unwrap_or(now)));
    }

    let mut lines = block;
    lines.push("Click **🎉 Participate** below to enter. Click it again to leave.".into());
    lines.push(String::new());
    lines.push(format!("**Ends:** {} ({})", discord_timestamp(&g.ends_at, "R"), discord_timestamp(&g.ends_at, "f")));
    lines.push(hosted);
    lines.push(format!("**Winners:** {}", g.winner_count));
    let required = role_list(&g.required_role_ids, role_exists);
    if !required.is_empty() {
        lines.push(format!("**Required role:** {required}"));
    }
    let denied = role_list(&g.denied_role_ids, role_exists);
    if !denied.is_empty() {
        lines.push(format!("**Not for:** {denied}"));
    }
    let weights: Vec<RoleWeight> = weights_of(g).into_iter().filter(|w| role_exists(&w.role_id)).collect();
    if !weights.is_empty() {
        lines.push(format!("**Bonus odds:** {}", weights.iter().map(|w| format!("<@&{}> ×{}", w.role_id, w.weight)).collect::<Vec<_>>().join(", ")));
    }
    if !prize.is_empty() {
        lines.push(format!("**Bonus prize:** {} each", prize.join(" + ")));
    }
    let footer = format!("ID: {} · Ends at", g.id);
    let mut embed = guild_embed(
        settings,
        Some(&clamp_text(&format!("🎉 {}", g.prize), 256)),
        Some(&clamp_text(&lines.join("\n"), 4096)),
        Some(&footer),
    )
    .timestamp(ts(&g.ends_at));
    if let Some(c) = g.embed_color.filter(|c| (0..=0xffffff).contains(c)) {
        embed = embed.color(if c == 0 { 0x010101 } else { c as u32 });
    }
    embed
}

pub(crate) fn embed_json(e: &CreateEmbed) -> Value {
    serde_json::to_value(e).unwrap_or(Value::Null)
}

pub(crate) fn row_json(r: &CreateActionRow) -> Value {
    serde_json::to_value(r).unwrap_or(Value::Null)
}

// ── Discord lookups ───────────────────────────────────────────────────────────────

fn no_access_code(code: Option<u64>) -> bool {
    matches!(code, Some(50001) | Some(50013))
}

/// `fetchChannel(guild, channelId)`: cache first, then REST; `Ok(None)` only on Unknown Channel.
pub async fn fetch_channel(app: &App, guild_id: &str, channel_id: &str) -> Result<Option<GuildChannel>, DiscordError> {
    fetch_guild_channel(app, guild_id, channel_id).await
}

/// What [`locate_message`] found.
#[derive(Debug, Clone)]
pub enum Located {
    Found { channel: GuildChannel },
    Gone { reason: &'static str, channel: Option<GuildChannel> },
    NoAccess { missing: Vec<String>, reason: Option<String>, channel: Option<GuildChannel> },
}

impl Located {
    pub fn channel(&self) -> Option<&GuildChannel> {
        match self {
            Located::Found { channel } => Some(channel),
            Located::Gone { channel, .. } | Located::NoAccess { channel, .. } => channel.as_ref(),
        }
    }
}

/// `locateMessage(guild, g)`: finds the channel and the message of a giveaway. Errors other than the gone and
/// no-access codes are returned.
pub async fn locate_message(app: &App, g: &Giveaway) -> Result<Located, DiscordError> {
    let channel = match fetch_channel(app, &g.guild_id, &g.channel_id).await {
        Ok(c) => c,
        Err(e) if no_access_code(e.code) => return Ok(Located::NoAccess { missing: vec!["ViewChannel".into()], reason: None, channel: None }),
        Err(e) => return Err(e),
    };
    let Some(channel) = channel else { return Ok(Located::Gone { reason: "channel deleted", channel: None }) };
    let usable = channel_usable(app, channel.guild_id, &channel, &POST_NEEDS);
    if !usable.ok {
        return Ok(Located::NoAccess { missing: usable.missing, reason: usable.reason, channel: Some(channel) });
    }
    let Some(message_id) = g.message_id.as_deref() else { return Ok(Located::Gone { reason: "message missing", channel: Some(channel) }) };
    match ops(app).fetch_message(&g.channel_id, message_id).await {
        Ok(()) => Ok(Located::Found { channel }),
        Err(e) if e.code == Some(10008) => Ok(Located::Gone { reason: "message deleted", channel: Some(channel) }),
        Err(e) if no_access_code(e.code) => {
            Ok(Located::NoAccess { missing: vec!["ViewChannel".into(), "ReadMessageHistory".into()], reason: None, channel: Some(channel) })
        }
        Err(e) => Err(e),
    }
}

/// `editGiveawayMessage(...)`: edits the embed and the button. Never fails; Unknown Message is ignored.
async fn edit_giveaway_message(app: &App, channel_id: &str, message_id: &str, g: &Giveaway, settings: Option<&GuildSettings>, entry_count: i64, disabled: bool) {
    let exists = guild_view::role_exists_fn(app, &g.guild_id);
    let embed = build_giveaway_embed(g, settings, entry_count, &exists, app.clock().now());
    let body = json!({ "embeds": [embed_json(&embed)], "components": [row_json(&build_join_row(&g.id, entry_count, disabled))] });
    if let Err(e) = ops(app).edit_message(channel_id, message_id, body).await {
        if e.code == Some(10008) {
            return;
        }
        bot_util::log_warn!(format!("Giveaway {}: could not edit the message", g.id), err_arg(&e));
    }
}

// ── Prizes ────────────────────────────────────────────────────────────────────────

fn reason_text(reason: &str, m: &bot_config::ModuleInfo) -> String {
    match reason {
        "unavailable" => format!("the {} {} module is not installed", m.emoji, m.label),
        "module-disabled" => format!("the {} {} module is turned off", m.emoji, m.label),
        "invalid-amount" => "the amount is invalid".into(),
        _ => "an error occurred".into(),
    }
}

/// Per-kind totals of [`pay_prizes`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PayTally {
    pub ok: usize,
    pub failed: usize,
    pub reasons: Vec<String>,
}

/// What [`pay_prizes`] returns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PayResult {
    pub warnings: Vec<String>,
    /// Keyed by `xp` / `coins`.
    pub results: BTreeMap<String, PayTally>,
}

/// The warning lines of [`pay_prizes`] for its totals (kinds in the JS order: xp, coins).
pub fn pay_warnings(results: &BTreeMap<String, PayTally>) -> Vec<String> {
    let mut warnings = Vec::new();
    for kind in [RewardKind::Xp, RewardKind::Coins] {
        let Some(t) = results.get(kind.as_str()) else { continue };
        if t.failed == 0 {
            continue;
        }
        let m = kind.module().info();
        warnings.push(format!(
            "⚠️ The {} prize could not be paid to {} winner{}: {}. Please pay it manually.",
            if kind == RewardKind::Xp { "XP" } else { "coin" },
            t.failed,
            if t.failed == 1 { "" } else { "s" },
            reason_text(t.reasons.first().map(String::as_str).unwrap_or("error"), m)
        ));
    }
    warnings
}

/// `payPrizes({ guild, giveaway, winnerIds, channel })`: pays the XP and coin prizes through the rewards bus (the
/// only way giveaways touch XP or coins). Never fails.
pub async fn pay_prizes(app: &App, giveaway: &Giveaway, winner_ids: &[String], channel_id: Option<&str>) -> PayResult {
    let mut kinds: Vec<(RewardKind, i64)> = Vec::new();
    if xp_of(giveaway) > 0 {
        kinds.push((RewardKind::Xp, xp_of(giveaway)));
    }
    if coins_of(giveaway) > 0 {
        kinds.push((RewardKind::Coins, coins_of(giveaway)));
    }
    let mut results: BTreeMap<String, PayTally> = BTreeMap::new();
    for (k, _) in &kinds {
        results.insert(k.as_str().to_string(), PayTally::default());
    }
    if kinds.is_empty() || winner_ids.is_empty() {
        return PayResult { warnings: Vec::new(), results };
    }
    let Some(gid) = guild_id_of(&giveaway.guild_id) else {
        return PayResult { warnings: vec!["⚠️ The prizes could not be paid. Please pay them manually.".into()], results };
    };
    let reason = format!("Giveaway prize: {}", clamp_text(&giveaway.prize, 80));
    let channel = channel_id.and_then(guild_view::channel_id_of);
    let per_user = bot_util::async_util::map_limit(winner_ids.to_vec(), 3, |user_id: String, _| {
        let (app, kinds, reason, guild_id) = (app.clone(), kinds.clone(), reason.clone(), giveaway.guild_id.clone());
        async move {
            let mut out = Vec::new();
            for (kind, amount) in kinds {
                let member = guild_view::cached_member(&app, &guild_id, &user_id);
                let args = RewardArgs::new(gid, user_id.clone(), amount).reason(reason.clone()).member(member).channel(channel);
                let r = app.rewards().grant(&app, kind, args).await;
                out.push((kind, r));
            }
            Ok::<_, std::convert::Infallible>(out)
        }
    })
    .await;
    for r in per_user.into_iter().flatten() {
        for (kind, res) in r {
            let t = results.entry(kind.as_str().to_string()).or_default();
            if res.ok {
                t.ok += 1;
            } else {
                t.failed += 1;
                t.reasons.push(res.reason.unwrap_or_else(|| "error".into()));
            }
        }
    }
    PayResult { warnings: pay_warnings(&results), results }
}

fn prize_line(g: &Giveaway, settings: Option<&GuildSettings>) -> Option<String> {
    let mut parts = Vec::new();
    if xp_of(g) > 0 {
        parts.push(format!("**{} XP**", format_number(xp_of(g) as f64)));
    }
    if coins_of(g) > 0 {
        let (emoji, name) = currency(settings);
        parts.push(format!("**{emoji} {} {name}**", format_number(coins_of(g) as f64)));
    }
    (!parts.is_empty()).then(|| format!("Each winner also gets {}.", parts.join(" and ")))
}

fn reply_ref(message_id: Option<&str>) -> Option<Value> {
    message_id.map(|m| json!({ "message_id": m, "fail_if_not_exists": false }))
}

/// The messages `announceWinners` sends, in order (bodies as Discord receives them). Pure.
pub fn announcement_bodies(g: &Giveaway, new_winner_ids: &[String], settings: Option<&GuildSettings>, reroll: bool) -> Vec<Value> {
    let reference = reply_ref(g.message_id.as_deref());
    let with_ref = |mut body: Value| {
        if let Some(r) = &reference {
            body["message_reference"] = r.clone();
        }
        body
    };
    let prize = clamp_text(&g.prize, 200);
    if new_winner_ids.is_empty() {
        return vec![with_ref(json!({
            "content": format!("No valid entrants for **{prize}**, so no winner could be chosen."),
            "allowed_mentions": { "parse": [] },
        }))];
    }
    let line = prize_line(g, settings);
    let n = new_winner_ids.len();
    let head_of = |mentions: &str| {
        if reroll {
            format!("🎉 New winner{}: {mentions}! Congratulations, you won **{prize}**!", if n == 1 { "" } else { "s" })
        } else {
            format!("🎉 Congratulations {mentions}! You won **{prize}**!")
        }
    };
    let head_len = bot_util::js::utf16_len(&head_of("")) as i64;
    let line_len = line.as_deref().map(|l| bot_util::js::utf16_len(l) as i64 + 1).unwrap_or(0);
    let room = (ANNOUNCE_CHUNK as i64).min(2000 - head_len - line_len).max(100) as usize;
    let chunks = pack_mentions(new_winner_ids, room);
    let head = head_of(&chunks[0]);
    let mut out = vec![with_ref(json!({
        "content": match &line { Some(l) => format!("{head}\n{l}"), None => head },
        "allowed_mentions": { "parse": [], "users": ids_in(&chunks[0]) },
    }))];
    for chunk in &chunks[1..] {
        out.push(json!({ "content": format!("🎉 {chunk}"), "allowed_mentions": { "parse": [], "users": ids_in(chunk) } }));
    }
    out
}

/// `announceWinners(channel, g, newWinnerIds, { settings, reroll })`: announces winners in the giveaway channel,
/// pinging the winners only. Never fails; `true` when the first message was sent.
pub async fn announce_winners(app: &App, channel_id: &str, g: &Giveaway, new_winner_ids: &[String], settings: Option<&GuildSettings>, reroll: bool) -> bool {
    let bodies = announcement_bodies(g, new_winner_ids, settings, reroll);
    let o = ops(app);
    let mut it = bodies.into_iter();
    let Some(first) = it.next() else { return false };
    if let Err(e) = o.send_message(&g.guild_id, channel_id, first).await {
        bot_util::log_warn!(format!("Giveaway {}: could not announce the winners", g.id), err_arg(&e));
        return false;
    }
    for body in it {
        if let Err(e) = o.send_message(&g.guild_id, channel_id, body).await {
            bot_util::log_warn!(format!("Giveaway {}: could not send an announcement chunk", g.id), err_arg(&e));
        }
    }
    true
}

async fn send_warnings(app: &App, guild_id: &str, channel_id: Option<&str>, warnings: &[String]) {
    let Some(channel_id) = channel_id else { return };
    if warnings.is_empty() {
        return;
    }
    let body = json!({ "content": clamp_text(&warnings.join("\n"), 2000), "allowed_mentions": { "parse": [] } });
    if let Err(e) = ops(app).send_message(guild_id, channel_id, body).await {
        bot_util::log_warn!("Giveaway: could not post the prize warnings", err_arg(&e));
    }
}

// ── SQL ───────────────────────────────────────────────────────────────────────────

const SQL_BY_ID: &str = r#"SELECT * FROM "Giveaway" WHERE "id" = $1"#;
const SQL_COUNT_OPEN: &str = r#"SELECT COUNT(*)::bigint AS n FROM "Giveaway" WHERE "guildId" = $1 AND "status" IN ('ACTIVE', 'DRAFT')"#;
const SQL_SET_MESSAGE: &str = r#"UPDATE "Giveaway" SET "messageId" = $2, "updatedAt" = $3 WHERE "id" = $1"#;
const SQL_DELETE: &str = r#"DELETE FROM "Giveaway" WHERE "id" = $1"#;
const SQL_ENTRIES_UNTIL: &str = r#"SELECT "userId" FROM "GiveawayEntry" WHERE "giveawayId" = $1 AND "createdAt" <= $2 ORDER BY "createdAt" ASC"#;
const SQL_ENTRIES_ALL: &str = r#"SELECT "userId" FROM "GiveawayEntry" WHERE "giveawayId" = $1 ORDER BY "createdAt" ASC"#;
const SQL_STILL_IN: &str = r#"SELECT "userId" FROM "GiveawayEntry" WHERE "giveawayId" = $1 AND "userId" = ANY($2)"#;
const SQL_ENTRY_COUNT: &str = r#"SELECT COUNT(*)::bigint AS n FROM "GiveawayEntry" WHERE "giveawayId" = $1"#;
const SQL_DRAFT_ACTIVE: &str = r#"UPDATE "Giveaway" SET "status" = 'DRAFT', "messageId" = NULL, "updatedAt" = $2 WHERE "id" = $1 AND "status" = 'ACTIVE'"#;
const SQL_DRAFT_IF_MESSAGE: &str =
    r#"UPDATE "Giveaway" SET "status" = 'DRAFT', "messageId" = NULL, "updatedAt" = $3 WHERE "id" = $1 AND "status" = 'ACTIVE' AND "messageId" = $2"#;
const SQL_CLEAR_MESSAGE: &str =
    r#"UPDATE "Giveaway" SET "messageId" = NULL, "updatedAt" = $3 WHERE "id" = $1 AND "messageId" = $2 AND "status" <> 'ACTIVE'"#;
const SQL_CLAIM_END: &str =
    r#"UPDATE "Giveaway" SET "status" = 'ENDED', "endedAt" = $2, "winnerIds" = $3, "updatedAt" = $4 WHERE "id" = $1 AND "status" = 'ACTIVE'"#;
const SQL_CLAIM_REROLL: &str =
    r#"UPDATE "Giveaway" SET "winnerIds" = $2, "updatedAt" = $3 WHERE "id" = $1 AND "status" = 'ENDED' AND "updatedAt" = $4"#;
const SQL_CANCEL: &str =
    r#"UPDATE "Giveaway" SET "status" = 'CANCELLED', "endedAt" = $2, "updatedAt" = $2 WHERE "id" = $1 AND "status" IN ('ACTIVE', 'DRAFT')"#;
const SQL_RESTART: &str = r#"UPDATE "Giveaway" SET "status" = 'ACTIVE', "channelId" = $2, "messageId" = $3, "endsAt" = $4, "updatedAt" = $5 WHERE "id" = $1 AND "status" = 'DRAFT'"#;
const SQL_SET_WEIGHTS: &str = r#"UPDATE "Giveaway" SET "roleWeights" = $2, "updatedAt" = $3 WHERE "id" = $1 AND "status" IN ('ACTIVE', 'DRAFT')"#;
const SQL_LEAVE: &str = r#"DELETE FROM "GiveawayEntry" WHERE "giveawayId" = $1 AND "userId" = $2"#;
const SQL_FIND_BY_MESSAGE: &str = r#"SELECT "id" FROM "Giveaway" WHERE "guildId" = $1 AND "messageId" = $2 LIMIT 1"#;
const SQL_ACTIVE_BY_MESSAGES: &str =
    r#"SELECT "id", "messageId" FROM "Giveaway" WHERE "guildId" = $1 AND "messageId" = ANY($2) AND "status" = 'ACTIVE'"#;

/// Every raw statement of this service, with sample parameters (`test/db.test.js` runs each in a rolled-back
/// transaction). Parameters: text, except `$2`/`$3`/`$4`/`$5` as listed.
pub const RAW_SQL: &[(&str, &str)] = &[
    ("giveaways.byId", SQL_BY_ID),
    ("giveaways.countOpen", SQL_COUNT_OPEN),
    ("giveaways.setMessage", SQL_SET_MESSAGE),
    ("giveaways.delete", SQL_DELETE),
    ("giveaways.entriesUntil", SQL_ENTRIES_UNTIL),
    ("giveaways.entriesAll", SQL_ENTRIES_ALL),
    ("giveaways.stillIn", SQL_STILL_IN),
    ("giveaways.entryCount", SQL_ENTRY_COUNT),
    ("giveaways.draftActive", SQL_DRAFT_ACTIVE),
    ("giveaways.draftIfMessage", SQL_DRAFT_IF_MESSAGE),
    ("giveaways.clearMessage", SQL_CLEAR_MESSAGE),
    ("giveaways.claimEnd", SQL_CLAIM_END),
    ("giveaways.claimReroll", SQL_CLAIM_REROLL),
    ("giveaways.cancel", SQL_CANCEL),
    ("giveaways.restart", SQL_RESTART),
    ("giveaways.setWeights", SQL_SET_WEIGHTS),
    ("giveaways.leave", SQL_LEAVE),
    ("giveaways.findByMessage", SQL_FIND_BY_MESSAGE),
    ("giveaways.activeByMessages", SQL_ACTIVE_BY_MESSAGES),
];

fn naive(dt: &DateTime<Utc>) -> chrono::NaiveDateTime {
    dt.naive_utc()
}

/// `new Date()` of this app's clock (whole milliseconds, as JS dates are).
pub(crate) fn now_of(app: &App) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(app.clock().now_ms()).unwrap_or_else(Utc::now)
}

/// `prisma.giveaway.findUnique({ where: { id } })`.
pub async fn find_giveaway(db: &bot_db::PgPool, id: &str) -> anyhow::Result<Option<Giveaway>> {
    Ok(sqlx::query_as::<_, Giveaway>(SQL_BY_ID).bind(id).fetch_optional(db).await?)
}

/// `prisma.giveawayEntry.count({ where: { giveawayId } })`.
pub async fn entry_count(db: &bot_db::PgPool, giveaway_id: &str) -> anyhow::Result<i64> {
    Ok(sqlx::query(SQL_ENTRY_COUNT).bind(giveaway_id).fetch_one(db).await?.try_get::<i64, _>("n")?)
}

async fn entries_until(db: &bot_db::PgPool, giveaway_id: &str, until: Option<&DateTime<Utc>>) -> anyhow::Result<Vec<String>> {
    let rows = match until {
        Some(t) => sqlx::query(SQL_ENTRIES_UNTIL).bind(giveaway_id).bind(naive(t)).fetch_all(db).await?,
        None => sqlx::query(SQL_ENTRIES_ALL).bind(giveaway_id).fetch_all(db).await?,
    };
    Ok(rows.iter().filter_map(|r| r.try_get::<String, _>("userId").ok()).collect())
}

async fn cancel_end_job(app: &App, giveaway_id: &str) {
    app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(end_job_key(giveaway_id)), ..Default::default() }).await;
}

// ── Flows ─────────────────────────────────────────────────────────────────────────

/// The send-time role ping: the executor is the bot.
fn ping_for(app: &App, channel: &GuildChannel, ping_role_id: Option<&str>) -> Vec<String> {
    match ping_role_id {
        Some(r) => guild_view::pingable_by_bot(app, channel.guild_id, channel, &[r.to_string()]).allowed,
        None => Vec::new(),
    }
}

/// The body of the giveaway message (`postGiveawayMessage`).
pub fn giveaway_message_body(g: &Giveaway, settings: Option<&GuildSettings>, entry_count: i64, ping: &[String], role_exists: &dyn Fn(&str) -> bool, now: DateTime<Utc>) -> Value {
    let embed = build_giveaway_embed(g, settings, entry_count, role_exists, now);
    let mut body = json!({
        "embeds": [embed_json(&embed)],
        "components": [row_json(&build_join_row(&g.id, entry_count, false))],
        "allowed_mentions": { "parse": [], "roles": ping },
    });
    if let Some(first) = ping.first() {
        body["content"] = json!(format!("<@&{first}>"));
    }
    body
}

async fn post_giveaway_message(
    app: &App,
    channel: &GuildChannel,
    g: &Giveaway,
    settings: Option<&GuildSettings>,
    entry_count: i64,
    ping_role_id: Option<&str>,
) -> Result<SentMessage, DiscordError> {
    let ping = ping_for(app, channel, ping_role_id);
    let exists = guild_view::role_exists_fn(app, &g.guild_id);
    let body = giveaway_message_body(g, settings, entry_count, &ping, &exists, now_of(app));
    ops(app).send_message(&g.guild_id, &channel.id.to_string(), body).await
}

/// `scheduleEnd(guildId, giveawayId, runAt)`.
pub async fn schedule_end(app: &App, guild_id: &str, giveaway_id: &str, run_at: DateTime<Utc>) -> anyhow::Result<()> {
    app.scheduler()
        .schedule_job(
            ScheduleJob::new(guild_id, JobType::GiveawayEnd, run_at)
                .payload(json!({ "giveawayId": giveaway_id }))
                .dedupe_key(end_job_key(giveaway_id))
                .max_attempts(END_MAX_ATTEMPTS)
                .on_conflict(OnConflict::Replace),
        )
        .await?;
    Ok(())
}

/// The options of [`start_giveaway`].
#[derive(Debug, Clone)]
pub struct StartGiveaway {
    pub guild_id: String,
    pub channel: GuildChannel,
    pub host_id: String,
    pub prize: String,
    pub description: Option<String>,
    pub winner_count: i64,
    pub ends_at: DateTime<Utc>,
    pub required_role_ids: Vec<String>,
    pub denied_role_ids: Vec<String>,
    pub role_weights: Vec<RoleWeight>,
    pub xp_prize: Option<i64>,
    pub coin_prize: Option<i64>,
    pub ping_role_id: Option<String>,
    pub embed_color: Option<u32>,
    pub settings: Option<Settings>,
}

/// What [`start_giveaway`] returns.
#[derive(Debug, Clone)]
pub struct Started {
    pub giveaway: Giveaway,
    pub message: SentMessage,
}

/// `startGiveaway(opts)`: creates a giveaway, posts its message and schedules the end job.
/// Errors: `LIMIT`, `SEND_FAILED`, `DB_ERROR`.
pub async fn start_giveaway(app: &App, opts: StartGiveaway) -> Result<Started, GiveawayError> {
    let app2 = app.clone();
    run_locked(app, &format!("start:{}", opts.guild_id), move || async move { start_locked(&app2, opts).await }).await
}

async fn create_row(app: &App, o: &StartGiveaway) -> anyhow::Result<Option<Giveaway>> {
    let open: i64 = sqlx::query(SQL_COUNT_OPEN).bind(&o.guild_id).fetch_one(app.db()).await?.try_get("n")?;
    if open >= limits::ACTIVE_GIVEAWAYS as i64 {
        return Ok(None);
    }
    app.settings().ensure_guild_settings(&o.guild_id).await?;
    let row: Giveaway = InsertBuilder::new(&GIVEAWAY_INFO)
        .values([
            ("guildId", json!(o.guild_id)),
            ("channelId", json!(o.channel.id.to_string())),
            ("hostId", json!(o.host_id)),
            ("prize", json!(o.prize)),
            ("description", json!(o.description)),
            ("winnerCount", json!(o.winner_count)),
            ("requiredRoleIds", json!(o.required_role_ids)),
            ("deniedRoleIds", json!(o.denied_role_ids)),
            ("roleWeights", serde_json::to_value(normalize_weights_list(&o.role_weights))?),
            ("xpPrize", json!(o.xp_prize)),
            ("coinPrize", json!(o.coin_prize)),
            ("pingRoleId", json!(o.ping_role_id)),
            ("embedColor", json!(o.embed_color)),
            ("endsAt", json!(bot_db::format_js_date(&o.ends_at))),
            ("status", json!("ACTIVE")),
        ])?
        .build()?
        .fetch_one(app.db())
        .await?;
    Ok(Some(row))
}

async fn start_locked(app: &App, o: StartGiveaway) -> Result<Started, GiveawayError> {
    let g = match create_row(app, &o).await {
        Ok(Some(g)) => g,
        Ok(None) => return Err(GiveawayError::Limit),
        Err(e) => {
            bot_util::log_error!("Giveaway create failed", bot_core::error::log_arg(&e));
            return Err(GiveawayError::DbError(e.to_string()));
        }
    };
    let settings = o.settings.as_deref();
    let message = match post_giveaway_message(app, &o.channel, &g, settings, 0, o.ping_role_id.as_deref()).await {
        Ok(m) => m,
        Err(e) => {
            let code = e.code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".into());
            bot_util::log_warn!(format!("Giveaway {}: could not post in {} (Discord error {code})", g.id, o.channel.id), err_arg(&e));
            let _ = sqlx::query(SQL_DELETE).bind(&g.id).execute(app.db()).await;
            return Err(GiveawayError::SendFailed(e));
        }
    };
    let recorded: anyhow::Result<()> = async {
        sqlx::query(SQL_SET_MESSAGE).bind(&g.id).bind(&message.id).bind(naive(&now_of(app))).execute(app.db()).await?;
        schedule_end(app, &o.guild_id, &g.id, o.ends_at).await
    }
    .await;
    if let Err(e) = recorded {
        bot_util::log_error!(format!("Giveaway {}: could not record or schedule the giveaway", g.id), bot_core::error::log_arg(&e));
        let _ = sqlx::query(SQL_DELETE).bind(&g.id).execute(app.db()).await;
        let _ = ops(app).delete_message(&message.channel_id, &message.id).await;
        return Err(GiveawayError::DbError(e.to_string()));
    }
    let mut giveaway = g;
    giveaway.message_id = Some(message.id.clone());
    Ok(Started { giveaway, message })
}

/// Who ends a giveaway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndSource {
    Job,
    Command,
}

/// What [`end_giveaway`] returns on success.
#[derive(Debug, Clone, PartialEq)]
pub struct Ended {
    pub winner_ids: Vec<String>,
    pub entry_count: i64,
    pub announced: bool,
    pub pay_warnings: Vec<String>,
}

/// `endGiveaway({ guild, giveawayId, source, attempt })`: draws, claims, edits the message, announces and pays. The
/// outer `Err` (the JS throw, so the scheduler retries) happens only for a job that is not on its final attempt and
/// hit a transient problem, or on unexpected database/Discord errors.
pub async fn end_giveaway(app: &App, guild_id: &str, giveaway_id: &str, source: EndSource, attempt: i32) -> anyhow::Result<Result<Ended, GiveawayError>> {
    let (app2, gid, id) = (app.clone(), guild_id.to_string(), giveaway_id.to_string());
    run_locked(app, giveaway_id, move || async move { end_locked(&app2, &gid, &id, source, attempt).await }).await
}

async fn draw(app: &App, guild_id: &str, g: &Giveaway, user_ids: &[String], count: i64) -> DrawResult {
    let exists = guild_view::role_exists_fn(app, guild_id);
    let clock = app.clock().clone();
    let deadline = (clock.now_ms() + DRAW_BUDGET_MS) as f64;
    draw_winners(user_ids, count, max_weight_for(g, &exists), make_resolver(app, guild_id, g), secure_random, DRAW_BATCH, deadline, move || {
        clock.now_ms() as f64
    })
    .await
}

async fn end_locked(app: &App, guild_id: &str, giveaway_id: &str, source: EndSource, attempt: i32) -> anyhow::Result<Result<Ended, GiveawayError>> {
    let is_final = source == EndSource::Job && attempt >= END_MAX_ATTEMPTS;
    let retry = source == EndSource::Job && !is_final;
    let Some(g) = find_giveaway(app.db(), giveaway_id).await? else { return Ok(Err(GiveawayError::NotFound)) };
    if g.guild_id != guild_id {
        return Ok(Err(GiveawayError::NotFound));
    }
    if g.status != GiveawayStatus::Active {
        return Ok(Err(GiveawayError::NotActive));
    }
    if source == EndSource::Job && (g.ends_at.timestamp_millis() - app.clock().now_ms()) > NOT_DUE_SLACK_MS {
        return Ok(Err(GiveawayError::NotDue { next_run_at: g.ends_at }));
    }
    if !guild_view::guild_available(app, guild_id) {
        if retry {
            anyhow::bail!("giveaway {}: guild {guild_id} is unavailable", g.id);
        }
        return Ok(Err(GiveawayError::ResolveFailed));
    }

    let settings = app.settings().get_settings_safe(guild_id).await;
    let loc = locate_message(app, &g).await?;
    let mut channel: Option<GuildChannel> = None;
    match loc {
        Located::Gone { reason, .. } => {
            let moved = sqlx::query(SQL_DRAFT_ACTIVE).bind(&g.id).bind(naive(&now_of(app))).execute(app.db()).await?.rows_affected();
            if moved == 0 {
                return Ok(Err(GiveawayError::NotActive));
            }
            cancel_end_job(app, &g.id).await;
            bot_util::log_info!(format!("Giveaway {} moved to draft: {reason}", g.id));
            return Ok(Err(GiveawayError::Drafted));
        }
        Located::NoAccess { missing, reason, .. } => {
            if source == EndSource::Command {
                return Ok(Err(GiveawayError::NoAccess { missing, reason }));
            }
            if retry {
                let what = if !missing.is_empty() { missing.join(",") } else { reason.unwrap_or_else(|| "unknown".into()) };
                anyhow::bail!("giveaway {}: channel unusable ({what})", g.id);
            }
            // The final attempt: end without posting.
        }
        Located::Found { channel: c } => channel = Some(c),
    }

    // The draw sees the entries made up to `endedAt`, which is taken before they are read.
    let ended_at = now_of(app);
    let entries = entries_until(app.db(), &g.id, Some(&ended_at)).await?;
    let entry_count = entries.len() as i64;
    let d = draw(app, guild_id, &g, &entries, i64::from(g.winner_count)).await;
    if !d.complete || d.unknown > 0 {
        if source == EndSource::Command {
            return Ok(Err(GiveawayError::ResolveFailed));
        }
        if retry {
            anyhow::bail!("giveaway {}: could not check every entrant ({} unknown, complete={})", g.id, d.unknown, d.complete);
        }
        bot_util::log_warn!(format!("Giveaway {}: ending with {} unresolved entrant(s) (complete={})", g.id, d.unknown, d.complete));
    }

    // A winner who clicked Leave while the draw ran is replaced instead of announced and paid.
    let mut winner_ids = d.winner_ids;
    if !winner_ids.is_empty() {
        let still: HashSet<String> = sqlx::query(SQL_STILL_IN)
            .bind(&g.id)
            .bind(&winner_ids)
            .fetch_all(app.db())
            .await?
            .iter()
            .filter_map(|r| r.try_get::<String, _>("userId").ok())
            .collect();
        let kept: Vec<String> = winner_ids.iter().filter(|id| still.contains(*id)).cloned().collect();
        if kept.len() < winner_ids.len() {
            let fresh = entries_until(app.db(), &g.id, Some(&ended_at)).await?;
            let kept_set: HashSet<&String> = kept.iter().collect();
            let pool: Vec<String> = fresh.into_iter().filter(|id| !kept_set.contains(id)).collect();
            let redraw = draw(app, guild_id, &g, &pool, (winner_ids.len() - kept.len()) as i64).await;
            if !redraw.complete || redraw.unknown > 0 {
                if source == EndSource::Command {
                    return Ok(Err(GiveawayError::ResolveFailed));
                }
                if retry {
                    anyhow::bail!("giveaway {}: could not check every entrant ({} unknown, complete={})", g.id, redraw.unknown, redraw.complete);
                }
            }
            winner_ids = kept.into_iter().chain(redraw.winner_ids).collect();
        }
    }

    // The claim is the idempotency gate: prizes are paid only after it, so at most once.
    let claimed = sqlx::query(SQL_CLAIM_END)
        .bind(&g.id)
        .bind(naive(&ended_at))
        .bind(&winner_ids)
        .bind(naive(&ended_at))
        .execute(app.db())
        .await?
        .rows_affected();
    if claimed == 0 {
        return Ok(Err(GiveawayError::NotActive));
    }
    cancel_end_job(app, &g.id).await;

    // From here on nothing may fail.
    let mut ended = g.clone();
    ended.status = GiveawayStatus::Ended;
    ended.ended_at = Some(ended_at);
    ended.winner_ids = winner_ids.clone();
    let s = settings.as_deref();
    let mut announced = false;
    let channel_id = channel.as_ref().map(|c| c.id.to_string());
    if let (Some(cid), Some(mid)) = (channel_id.as_deref(), g.message_id.as_deref()) {
        edit_giveaway_message(app, cid, mid, &ended, s, entry_count, true).await;
    }
    if let Some(cid) = channel_id.as_deref() {
        announced = announce_winners(app, cid, &ended, &winner_ids, s, false).await;
    }
    let pay = pay_prizes(app, &ended, &winner_ids, channel_id.as_deref()).await;
    for w in &pay.warnings {
        bot_util::log_warn!(format!("Giveaway {}: {w}", g.id));
    }
    send_warnings(app, &g.guild_id, channel_id.as_deref(), &pay.warnings).await;
    Ok(Ok(Ended { winner_ids, entry_count, announced, pay_warnings: pay.warnings }))
}

/// What [`reroll_giveaway`] returns on success.
#[derive(Debug, Clone, PartialEq)]
pub struct Rerolled {
    pub new_winner_ids: Vec<String>,
    pub announced: bool,
    pub pay_warnings: Vec<String>,
}

/// `rerollGiveaway({ guild, giveawayId, count })`: draws `count` more winners of an ended giveaway, excluding
/// everyone drawn before. Errors: `NOT_FOUND`, `NOT_ENDED`, `NO_ENTRANTS`, `CONFLICT`, `RESOLVE_FAILED`.
pub async fn reroll_giveaway(app: &App, guild_id: &str, giveaway_id: &str, count: i64) -> anyhow::Result<Result<Rerolled, GiveawayError>> {
    let (app2, gid, id) = (app.clone(), guild_id.to_string(), giveaway_id.to_string());
    run_locked(app, giveaway_id, move || async move { reroll_locked(&app2, &gid, &id, count).await }).await
}

async fn reroll_locked(app: &App, guild_id: &str, giveaway_id: &str, count: i64) -> anyhow::Result<Result<Rerolled, GiveawayError>> {
    let Some(g) = find_giveaway(app.db(), giveaway_id).await? else { return Ok(Err(GiveawayError::NotFound)) };
    if g.guild_id != guild_id {
        return Ok(Err(GiveawayError::NotFound));
    }
    if g.status != GiveawayStatus::Ended {
        return Ok(Err(GiveawayError::NotEnded));
    }
    // Only entries made up to `endedAt` were in the draw.
    let entries = entries_until(app.db(), &g.id, g.ended_at.as_ref()).await?;
    let exclude: HashSet<&String> = g.winner_ids.iter().collect();
    let pool: Vec<String> = entries.iter().filter(|id| !exclude.contains(id)).cloned().collect();
    let d = draw(app, guild_id, &g, &pool, count).await;
    if !d.complete || d.unknown > 0 {
        return Ok(Err(GiveawayError::ResolveFailed));
    }
    if d.winner_ids.is_empty() {
        return Ok(Err(GiveawayError::NoEntrants));
    }
    let winner_ids: Vec<String> = g.winner_ids.iter().cloned().chain(d.winner_ids.iter().cloned()).collect();
    let claimed = sqlx::query(SQL_CLAIM_REROLL)
        .bind(&g.id)
        .bind(&winner_ids)
        .bind(naive(&now_of(app)))
        .bind(naive(&g.updated_at))
        .execute(app.db())
        .await?
        .rows_affected();
    if claimed == 0 {
        return Ok(Err(GiveawayError::Conflict));
    }

    let mut updated = g.clone();
    updated.winner_ids = winner_ids;
    let settings = app.settings().get_settings_safe(guild_id).await;
    let s = settings.as_deref();
    let loc = match locate_message(app, &g).await {
        Ok(l) => Some(l),
        Err(e) => {
            bot_util::log_warn!(format!("Giveaway {}: could not locate the message for a reroll", g.id), err_arg(&e));
            None
        }
    };
    let channel = loc.as_ref().and_then(Located::channel).filter(|c| channel_usable(app, c.guild_id, c, &POST_NEEDS).ok).cloned();
    let channel_id = channel.as_ref().map(|c| c.id.to_string());
    if let (Some(cid), Some(Located::Found { .. }), Some(mid)) = (channel_id.as_deref(), loc.as_ref(), g.message_id.as_deref()) {
        edit_giveaway_message(app, cid, mid, &updated, s, entries.len() as i64, true).await;
    }
    let mut announced = false;
    if let Some(cid) = channel_id.as_deref() {
        announced = announce_winners(app, cid, &updated, &d.winner_ids, s, true).await;
    }
    let pay = pay_prizes(app, &updated, &d.winner_ids, channel_id.as_deref()).await;
    for w in &pay.warnings {
        bot_util::log_warn!(format!("Giveaway {}: {w}", g.id));
    }
    send_warnings(app, &g.guild_id, channel_id.as_deref(), &pay.warnings).await;
    Ok(Ok(Rerolled { new_winner_ids: d.winner_ids, announced, pay_warnings: pay.warnings }))
}

/// `cancelGiveaway({ guild, giveawayId })`: cancels an ACTIVE or DRAFT giveaway without drawing; entries are kept.
/// Errors: `NOT_OPEN`, `NOT_FOUND`.
pub async fn cancel_giveaway(app: &App, guild_id: &str, giveaway_id: &str) -> anyhow::Result<Result<(), GiveawayError>> {
    let (app2, gid, id) = (app.clone(), guild_id.to_string(), giveaway_id.to_string());
    run_locked(app, giveaway_id, move || async move { cancel_locked(&app2, &gid, &id).await }).await
}

async fn cancel_locked(app: &App, guild_id: &str, giveaway_id: &str) -> anyhow::Result<Result<(), GiveawayError>> {
    let Some(g) = find_giveaway(app.db(), giveaway_id).await? else { return Ok(Err(GiveawayError::NotFound)) };
    if g.guild_id != guild_id {
        return Ok(Err(GiveawayError::NotFound));
    }
    let ended_at = now_of(app);
    let moved = sqlx::query(SQL_CANCEL).bind(&g.id).bind(naive(&ended_at)).execute(app.db()).await?.rows_affected();
    if moved == 0 {
        return Ok(Err(GiveawayError::NotOpen));
    }
    cancel_end_job(app, &g.id).await;
    if let Some(mid) = g.message_id.clone() {
        let r: anyhow::Result<()> = async {
            let loc = locate_message(app, &g).await?;
            if let Located::Found { channel } = loc {
                let settings = app.settings().get_settings_safe(guild_id).await;
                let n = entry_count(app.db(), &g.id).await?;
                let mut c = g.clone();
                c.status = GiveawayStatus::Cancelled;
                c.ended_at = Some(ended_at);
                edit_giveaway_message(app, &channel.id.to_string(), &mid, &c, settings.as_deref(), n, true).await;
            }
            Ok(())
        }
        .await;
        if let Err(e) = r {
            bot_util::log_warn!(format!("Giveaway {}: could not update the message after cancelling", g.id), bot_core::error::log_arg(&e));
        }
    }
    Ok(Ok(()))
}

/// `restartGiveaway({ guild, giveawayId, channel, endsAt, settings })`: republishes a draft giveaway with a fresh
/// message and end job; entries are kept. Errors: `NOT_DRAFT`, `CONFLICT`, `SEND_FAILED`, `DB_ERROR`.
pub async fn restart_giveaway(
    app: &App,
    guild_id: &str,
    giveaway_id: &str,
    channel: GuildChannel,
    ends_at: DateTime<Utc>,
    settings: Option<Settings>,
) -> Result<SentMessage, GiveawayError> {
    let (app2, gid, id) = (app.clone(), guild_id.to_string(), giveaway_id.to_string());
    run_locked(app, giveaway_id, move || async move { restart_locked(&app2, &gid, &id, channel, ends_at, settings).await }).await
}

async fn restart_locked(
    app: &App,
    guild_id: &str,
    giveaway_id: &str,
    channel: GuildChannel,
    ends_at: DateTime<Utc>,
    settings: Option<Settings>,
) -> Result<SentMessage, GiveawayError> {
    let looked: anyhow::Result<Option<(Giveaway, i64)>> = async {
        let Some(g) = find_giveaway(app.db(), giveaway_id).await? else { return Ok(None) };
        if g.guild_id != guild_id || g.status != GiveawayStatus::Draft {
            return Ok(None);
        }
        let n = entry_count(app.db(), giveaway_id).await?;
        Ok(Some((g, n)))
    }
    .await;
    let (g, n) = match looked {
        Ok(Some(v)) => v,
        Ok(None) => return Err(GiveawayError::NotDraft),
        Err(e) => {
            bot_util::log_error!(format!("Giveaway {giveaway_id}: restart lookup failed"), bot_core::error::log_arg(&e));
            return Err(GiveawayError::DbError(e.to_string()));
        }
    };
    let exists = guild_view::role_exists_fn(app, guild_id);
    let ping_role = g.ping_role_id.clone().filter(|r| exists(r));
    let mut preview = g.clone();
    preview.status = GiveawayStatus::Active;
    preview.ends_at = ends_at;
    preview.channel_id = channel.id.to_string();
    let message = match post_giveaway_message(app, &channel, &preview, settings.as_deref(), n, ping_role.as_deref()).await {
        Ok(m) => m,
        Err(e) => {
            let code = e.code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".into());
            bot_util::log_warn!(format!("Giveaway {}: could not repost in {} (Discord error {code})", g.id, channel.id), err_arg(&e));
            return Err(GiveawayError::SendFailed(e));
        }
    };
    let o = ops(app);
    let moved = sqlx::query(SQL_RESTART)
        .bind(&g.id)
        .bind(channel.id.to_string())
        .bind(&message.id)
        .bind(naive(&ends_at))
        .bind(naive(&now_of(app)))
        .execute(app.db())
        .await;
    match moved {
        Ok(r) if r.rows_affected() == 0 => {
            let _ = o.delete_message(&message.channel_id, &message.id).await;
            return Err(GiveawayError::Conflict);
        }
        Ok(_) => {}
        Err(e) => {
            bot_util::log_error!(format!("Giveaway {}: restart transition failed", g.id), err_arg(&e));
            let _ = o.delete_message(&message.channel_id, &message.id).await;
            return Err(GiveawayError::DbError(e.to_string()));
        }
    }
    if let Err(e) = schedule_end(app, guild_id, &g.id, ends_at).await {
        bot_util::log_error!(format!("Giveaway {}: could not schedule the end after a restart", g.id), bot_core::error::log_arg(&e));
        // Clear messageId before deleting the message so the messageDelete hook has nothing left to do.
        let sql = r#"UPDATE "Giveaway" SET "status" = 'DRAFT', "messageId" = NULL, "updatedAt" = $3 WHERE "id" = $1 AND "status" = 'ACTIVE' AND "messageId" = $2"#;
        let _ = sqlx::query(sql).bind(&g.id).bind(&message.id).bind(naive(&now_of(app))).execute(app.db()).await;
        let _ = o.delete_message(&message.channel_id, &message.id).await;
        return Err(GiveawayError::DbError(e.to_string()));
    }
    Ok(message)
}

/// The operation of [`update_odds`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OddsOp {
    Add { weight: i64 },
    Remove,
}

/// The pure decision of `updateOdds`: the next list, or `LIMIT` / `NOT_SET`.
pub fn next_odds(current: &[RoleWeight], role_id: &str, op: OddsOp) -> Result<Vec<RoleWeight>, GiveawayError> {
    let others: Vec<RoleWeight> = current.iter().filter(|w| w.role_id != role_id).cloned().collect();
    match op {
        OddsOp::Add { weight } => {
            // Checked before normalizing, which would silently drop an 11th role. Replacing a weight needs no room.
            if others.len() >= MAX_ROLE_WEIGHTS {
                return Err(GiveawayError::Limit);
            }
            let mut list = others;
            list.push(RoleWeight { role_id: role_id.to_string(), weight });
            Ok(normalize_weights_list(&list))
        }
        OddsOp::Remove => {
            if others.len() == current.len() {
                return Err(GiveawayError::NotSet);
            }
            Ok(others)
        }
    }
}

/// `updateOdds({ guild, giveawayId, op, roleId, weight, settings })`: adds, changes or removes a role's bonus odds on
/// an ACTIVE or DRAFT giveaway, and refreshes the embed. Errors: `NOT_FOUND`, `NOT_OPEN`, `LIMIT`, `NOT_SET`.
pub async fn update_odds(
    app: &App,
    guild_id: &str,
    giveaway_id: &str,
    op: OddsOp,
    role_id: &str,
    settings: Option<Settings>,
) -> anyhow::Result<Result<Vec<RoleWeight>, GiveawayError>> {
    let (app2, gid, id, rid) = (app.clone(), guild_id.to_string(), giveaway_id.to_string(), role_id.to_string());
    run_locked(app, giveaway_id, move || async move {
        let app = &app2;
        let Some(g) = find_giveaway(app.db(), &id).await? else { return Ok(Err(GiveawayError::NotFound)) };
        if g.guild_id != gid {
            return Ok(Err(GiveawayError::NotFound));
        }
        if !matches!(g.status, GiveawayStatus::Active | GiveawayStatus::Draft) {
            return Ok(Err(GiveawayError::NotOpen));
        }
        let next = match next_odds(&weights_of(&g), &rid, op) {
            Ok(n) => n,
            Err(e) => return Ok(Err(e)),
        };
        let moved = sqlx::query(SQL_SET_WEIGHTS)
            .bind(&g.id)
            .bind(serde_json::to_value(&next)?)
            .bind(naive(&now_of(app)))
            .execute(app.db())
            .await?
            .rows_affected();
        if moved == 0 {
            return Ok(Err(GiveawayError::NotOpen));
        }
        if g.status == GiveawayStatus::Active
            && let Some(mid) = g.message_id.clone()
        {
            let r: anyhow::Result<()> = async {
                if let Some(channel) = fetch_channel(app, &gid, &g.channel_id).await? {
                    let n = entry_count(app.db(), &g.id).await?;
                    let mut ng = g.clone();
                    ng.role_weights = Some(serde_json::to_value(&next)?);
                    let exists = guild_view::role_exists_fn(app, &gid);
                    let embed = build_giveaway_embed(&ng, settings.as_deref(), n, &exists, now_of(app));
                    ops(app).edit_message(&channel.id.to_string(), &mid, json!({ "embeds": [embed_json(&embed)] })).await?;
                }
                Ok(())
            }
            .await;
            if let Err(e) = r {
                let gone = e.downcast_ref::<DiscordError>().is_some_and(|d| d.code == Some(10008));
                if !gone {
                    bot_util::log_warn!(format!("Giveaway {}: could not refresh the embed after an odds change", g.id), bot_core::error::log_arg(&e));
                }
            }
        }
        Ok(Ok(next))
    })
    .await
}

// ── The Participate button ────────────────────────────────────────────────────────

/// What a Participate click decided, before any reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinOutcome {
    NotFound,
    Paused,
    Closed,
    Left { prize: String },
    Denied { role_id: String },
    Missing { role_ids: Vec<String> },
    Entered { prize: String },
}

/// The reply embed of a click.
pub fn join_reply(outcome: &JoinOutcome) -> CreateEmbed {
    match outcome {
        JoinOutcome::NotFound => error_embed("Giveaway Not Found", Some("This giveaway no longer exists."), None),
        JoinOutcome::Paused => warn_embed("Giveaway Paused", Some("This giveaway is not running right now."), None),
        JoinOutcome::Closed => warn_embed("Giveaway Closed", Some("This giveaway has ended."), None),
        JoinOutcome::Left { prize } => info_embed("Left Giveaway", Some(&format!("You have left the giveaway for **{prize}**.")), None),
        JoinOutcome::Denied { role_id } => {
            error_embed("Requirement Not Met", Some(&format!("You can't enter this giveaway with the <@&{role_id}> role.")), None)
        }
        JoinOutcome::Missing { role_ids } => error_embed(
            "Requirement Not Met",
            Some(&format!(
                "You need the {} role to enter this giveaway.",
                role_ids.iter().map(|r| format!("<@&{r}>")).collect::<Vec<_>>().join(", ")
            )),
            None,
        ),
        JoinOutcome::Entered { prize } => success_embed(
            "Entered",
            Some(&format!("You have entered the giveaway for **{prize}**. Good luck! Click **Participate** again to leave.")),
            None,
        ),
    }
}

/// The database side of a Participate click: toggles the entry of `user_id`. `member_roles` are the clicker's roles
/// and `role_exists` the guild's roles (a deleted role no longer restricts anyone).
pub async fn toggle_entry(
    app: &App,
    guild_id: &str,
    giveaway_id: &str,
    user_id: &str,
    member_roles: &[String],
    role_exists: &(dyn Fn(&str) -> bool + Send + Sync),
) -> anyhow::Result<JoinOutcome> {
    let Some(g) = find_giveaway(app.db(), giveaway_id).await? else { return Ok(JoinOutcome::NotFound) };
    if g.guild_id != guild_id {
        return Ok(JoinOutcome::NotFound);
    }
    if g.status == GiveawayStatus::Draft {
        return Ok(JoinOutcome::Paused);
    }
    if g.status != GiveawayStatus::Active || g.ends_at.timestamp_millis() <= app.clock().now_ms() {
        return Ok(JoinOutcome::Closed);
    }
    let prize = clamp_text(&g.prize, 200);
    // Toggle off. Leaving is always allowed, even after losing a required role.
    let left = sqlx::query(SQL_LEAVE).bind(giveaway_id).bind(user_id).execute(app.db()).await?.rows_affected();
    if left > 0 {
        return Ok(JoinOutcome::Left { prize });
    }
    let has = |r: &str| member_roles.iter().any(|m| m == r);
    let denied: Vec<&String> = g.denied_role_ids.iter().filter(|r| role_exists(r)).collect();
    if let Some(role_id) = held_denied_role(has, &denied) {
        return Ok(JoinOutcome::Denied { role_id });
    }
    let missing: Vec<String> = g.required_role_ids.iter().filter(|r| role_exists(r) && !has(r)).cloned().collect();
    if !missing.is_empty() {
        return Ok(JoinOutcome::Missing { role_ids: missing });
    }
    // Toggle on. A unique violation is a concurrent double click that already entered; a foreign-key violation: the
    // giveaway was deleted meanwhile.
    let insert = InsertBuilder::new(&GIVEAWAY_ENTRY_INFO)
        .values([("giveawayId", json!(giveaway_id)), ("userId", json!(user_id))])?
        .without_returning()
        .build()?;
    if let Err(e) = insert.execute(app.db()).await {
        if e.sqlstate().as_deref() == Some("23503") {
            return Ok(JoinOutcome::NotFound);
        }
        if !e.is_unique_violation() {
            return Err(e.into());
        }
    }
    Ok(JoinOutcome::Entered { prize })
}

/// The persistent `giveaway:join:{id}` button: toggles entry. The module gate has already run.
pub async fn handle_join_button(ctx: ComponentCtx) -> BotResult {
    let (_, params) = ctx.parsed();
    let action = params.first().map(String::as_str);
    let id = params.get(1).cloned().filter(|s| !s.is_empty());
    let Some(id) = id.filter(|_| action == Some("join")) else {
        safe_reply(&ctx.responder, Payload::text("This button is no longer active.").ephemeral(true)).await;
        return Ok(());
    };
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let reply = |embed: CreateEmbed| safe_reply(&ctx.responder, Payload::embed(embed));
    let guild_id = ctx.guild_id().map(|g| g.to_string());
    let in_cached = ctx.guild_id().is_some_and(|gid| guild_view::guild_cached(&ctx.app, gid));
    let (Some(guild_id), true, Some(member)) = (guild_id, in_cached, ctx.member().cloned()) else {
        reply(error_embed("Try Again", Some("I can't check your roles right now. Please try again in a moment."), None)).await;
        return Ok(());
    };
    let roles: Vec<String> = member.roles.iter().map(|r| r.to_string()).collect();
    let exists = guild_view::role_exists_fn(&ctx.app, &guild_id);
    let outcome = toggle_entry(&ctx.app, &guild_id, &id, &ctx.user().id.to_string(), &roles, &exists).await?;
    reply(join_reply(&outcome)).await;
    if matches!(outcome, JoinOutcome::Left { .. } | JoinOutcome::Entered { .. }) {
        schedule_count_refresh(&ctx.app, &id);
    }
    Ok(())
}

// ── The button count ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default)]
struct RefreshSlot {
    last_at: i64,
    queued: bool,
}

/// `refreshState`: giveawayId -> { lastAt, timer }, at most one count edit per [`COUNT_REFRESH_MS`] per giveaway,
/// trailing edge included. One per app.
#[derive(Default)]
pub struct CountRefresh {
    slots: Mutex<HashMap<String, RefreshSlot>>,
    /// Bumped by [`reset_count_refresh`]: timers of an older generation do nothing.
    generation: std::sync::atomic::AtomicU64,
}

impl CountRefresh {
    /// Pending refreshes (tests).
    pub fn queued(&self) -> usize {
        self.slots.lock().unwrap_or_else(|e| e.into_inner()).values().filter(|s| s.queued).count()
    }
}

/// `resetCountRefresh()`: clears every pending count refresh (tests and shutdown).
pub fn reset_count_refresh(app: &App) {
    let st = app.ext::<CountRefresh>();
    st.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    st.slots.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// `scheduleCountRefresh(client, giveawayId)`: queues a refresh of the Participate (N) label. Synchronous; never
/// fails. Needs a Tokio runtime (it spawns the timer).
pub fn schedule_count_refresh(app: &App, giveaway_id: &str) {
    let st = app.ext::<CountRefresh>();
    let now = app.clock().now_ms();
    let wait = {
        let mut slots = st.slots.lock().unwrap_or_else(|e| e.into_inner());
        if !slots.contains_key(giveaway_id) && slots.len() > 500 {
            let cutoff = now - 60_000;
            slots.retain(|_, s| s.queued || s.last_at >= cutoff);
        }
        let s = slots.entry(giveaway_id.to_string()).or_default();
        if s.queued {
            return; // an update is already queued; it will include this click
        }
        s.queued = true;
        (s.last_at + COUNT_REFRESH_MS - now).max(0)
    };
    let generation = st.generation.load(std::sync::atomic::Ordering::SeqCst);
    let (app, id) = (app.clone(), giveaway_id.to_string());
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        bot_util::log_warn!("scheduleCountRefresh failed", "no runtime");
        return;
    };
    handle.spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(wait as u64)).await;
        let st = app.ext::<CountRefresh>();
        if st.generation.load(std::sync::atomic::Ordering::SeqCst) != generation {
            return;
        }
        {
            let mut slots = st.slots.lock().unwrap_or_else(|e| e.into_inner());
            let s = slots.entry(id.clone()).or_default();
            s.queued = false;
            s.last_at = app.clock().now_ms();
        }
        if let Err(e) = refresh_entry_count(&app, &id).await {
            bot_util::log_warn!("Giveaway count refresh failed", bot_core::error::log_arg(&e));
        }
    });
}

/// `refreshEntryCount(client, giveawayId)`: edits only the button row. Inside the giveaway lock, so it can never
/// re-enable the button after the end edited it.
pub async fn refresh_entry_count(app: &App, giveaway_id: &str) -> anyhow::Result<()> {
    let (app2, id) = (app.clone(), giveaway_id.to_string());
    run_locked(app, giveaway_id, move || async move {
        let app = &app2;
        let Some(g) = find_giveaway(app.db(), &id).await? else { return Ok(()) };
        let Some(mid) = g.message_id.clone().filter(|_| g.status == GiveawayStatus::Active) else { return Ok(()) };
        let (Some(gid), Some(cid)) = (guild_id_of(&g.guild_id), guild_view::channel_id_of(&g.channel_id)) else { return Ok(()) };
        if bot_core::interaction_helpers::cached_channel(app, gid, cid).is_none() {
            return Ok(());
        }
        let count = entry_count(app.db(), &id).await?;
        if let Err(e) = ops(app).edit_message(&g.channel_id, &mid, json!({ "components": [row_json(&build_join_row(&id, count, false))] })).await {
            if e.code == Some(10008) {
                return Ok(()); // the messageDelete hook or the end path will draft it
            }
            bot_util::log_warn!(format!("Giveaway {id}: could not refresh the entry count"), err_arg(&e));
        }
        Ok(())
    })
    .await
}

// ── Hooks and the job ─────────────────────────────────────────────────────────────

async fn draft_if_active(app: &App, giveaway_id: &str, message_id: &str) -> anyhow::Result<bool> {
    let moved =
        sqlx::query(SQL_DRAFT_IF_MESSAGE).bind(giveaway_id).bind(message_id).bind(naive(&now_of(app))).execute(app.db()).await?.rows_affected();
    if moved > 0 {
        cancel_end_job(app, giveaway_id).await;
        bot_util::log_info!(format!("Giveaway {giveaway_id} moved to draft (message deleted)"));
    }
    Ok(moved > 0)
}

/// `handleGiveawayMessageDelete(ctx)`: an ACTIVE giveaway whose message was deleted becomes a DRAFT (its end job is
/// cancelled). On an ENDED or CANCELLED row only the dangling messageId is cleared.
pub async fn handle_giveaway_message_delete(app: &App, guild_id: &str, message_id: &str) -> anyhow::Result<()> {
    let row = sqlx::query(SQL_FIND_BY_MESSAGE).bind(guild_id).bind(message_id).fetch_optional(app.db()).await?;
    let Some(id) = row.and_then(|r| r.try_get::<String, _>("id").ok()) else { return Ok(()) };
    let (app2, gid, mid) = (app.clone(), id.clone(), message_id.to_string());
    run_locked(app, &id, move || async move {
        if draft_if_active(&app2, &gid, &mid).await? {
            return Ok(());
        }
        sqlx::query(SQL_CLEAR_MESSAGE).bind(&gid).bind(&mid).bind(naive(&now_of(&app2))).execute(app2.db()).await?;
        Ok(())
    })
    .await
}

/// `handleGiveawayBulkDelete(ctx)`: the same for every ACTIVE giveaway among the deleted messages, one lookup.
pub async fn handle_giveaway_bulk_delete(app: &App, guild_id: &str, message_ids: &[String]) -> anyhow::Result<()> {
    if message_ids.is_empty() {
        return Ok(());
    }
    let rows = sqlx::query(SQL_ACTIVE_BY_MESSAGES).bind(guild_id).bind(message_ids).fetch_all(app.db()).await?;
    for r in rows {
        let (Ok(id), Ok(Some(mid))) = (r.try_get::<String, _>("id"), r.try_get::<Option<String>, _>("messageId")) else { continue };
        let (app2, gid) = (app.clone(), id.clone());
        let res = run_locked(app, &id, move || async move { draft_if_active(&app2, &gid, &mid).await }).await;
        if let Err(e) = res {
            bot_util::log_error!(format!("Giveaway {id}: bulk-delete handling failed"), bot_core::error::log_arg(&e));
        }
    }
    Ok(())
}

/// The `giveaway.end` job payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndPayload {
    pub giveaway_id: String,
}

/// `handleEndJob(job, ctx)`: retries come from the errors of [`end_giveaway`] (the scheduler backs off, 5 attempts);
/// on the final attempt it stops retrying and ends with what it can do.
pub async fn handle_end_job(job: Job, ctx: JobCtx) -> BotResult<JobOutcome> {
    let Some(id) = job.payload.get("giveawayId").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string) else {
        return Err(PermanentJobError::new("giveaway.end: payload.giveawayId missing").into());
    };
    let res = end_giveaway(&ctx.app, &ctx.guild_id, &id, EndSource::Job, ctx.attempt).await?;
    if let Err(GiveawayError::NotDue { next_run_at }) = res {
        return Ok(JobOutcome::Reschedule { next_run_at, payload: None });
    }
    Ok(JobOutcome::Done)
}

// ── Command queries (used by commands/community/giveaway.js) ────────────────────

/// The `status` filter of `/giveaway list` (`LIST_FILTERS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListFilter {
    Open,
    Ended,
    Cancelled,
    All,
}

impl ListFilter {
    /// The option value, falling back to `open` for anything unknown.
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("ended") => Self::Ended,
            Some("cancelled") => Self::Cancelled,
            Some("all") => Self::All,
            _ => Self::Open,
        }
    }

    fn sql(self) -> &'static str {
        match self {
            Self::Open => r#" AND g."status" IN ('ACTIVE', 'DRAFT')"#,
            Self::Ended => r#" AND g."status" = 'ENDED'"#,
            Self::Cancelled => r#" AND g."status" = 'CANCELLED'"#,
            Self::All => "",
        }
    }

    /// `LIST_FILTER_NAMES`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Open => "open (active or draft) ",
            Self::Ended => "ended ",
            Self::Cancelled => "cancelled ",
            Self::All => "",
        }
    }
}

/// The rows of `/giveaway list`: newest first, at most [`LIST_SHOWN`], with their entry counts.
pub async fn list_giveaways(db: &bot_db::PgPool, guild_id: &str, filter: ListFilter) -> anyhow::Result<Vec<ListRow>> {
    let sql = format!(
        r#"SELECT g."id", g."prize", g."status"::text AS status, g."channelId", g."endsAt", g."endedAt", (SELECT COUNT(*) FROM "GiveawayEntry" e WHERE e."giveawayId" = g."id")::bigint AS entries FROM "Giveaway" g WHERE g."guildId" = $1{} ORDER BY g."createdAt" DESC LIMIT $2"#,
        filter.sql()
    );
    let rows = sqlx::query(AssertSqlSafe(sql)).bind(guild_id).bind(LIST_SHOWN).fetch_all(db).await?;
    let mut out = Vec::new();
    for r in rows {
        out.push(ListRow {
            id: r.try_get("id")?,
            prize: r.try_get("prize")?,
            status: GiveawayStatus::parse(&r.try_get::<String, _>("status")?).unwrap_or(GiveawayStatus::Active),
            channel_id: r.try_get("channelId")?,
            ends_at: r.try_get::<chrono::NaiveDateTime, _>("endsAt")?.and_utc(),
            ended_at: r.try_get::<Option<chrono::NaiveDateTime>, _>("endedAt")?.map(|d| d.and_utc()),
            entries: r.try_get("entries")?,
        });
    }
    Ok(out)
}

/// The first [`ENTRANTS_SHOWN`] entrants of a giveaway, oldest first: `(userId, createdAt)`.
pub async fn list_entrants(db: &bot_db::PgPool, giveaway_id: &str) -> anyhow::Result<Vec<(String, DateTime<Utc>)>> {
    let sql = r#"SELECT "userId", "createdAt" FROM "GiveawayEntry" WHERE "giveawayId" = $1 ORDER BY "createdAt" ASC LIMIT $2"#;
    let rows = sqlx::query(sql).bind(giveaway_id).bind(ENTRANTS_SHOWN).fetch_all(db).await?;
    let mut out = Vec::new();
    for r in rows {
        out.push((r.try_get("userId")?, r.try_get::<chrono::NaiveDateTime, _>("createdAt")?.and_utc()));
    }
    Ok(out)
}

/// An autocomplete row (`select { id, prize, status, channelId, endsAt }`).
#[derive(Debug, Clone, PartialEq)]
pub struct AutocompleteRow {
    pub id: String,
    pub prize: String,
    pub status: GiveawayStatus,
    pub channel_id: String,
    pub ends_at: DateTime<Utc>,
}

/// Prisma's `contains` / `startsWith` patterns do not escape `%`, `_` or `\` (checked against Prisma 5.22 on
/// PostgreSQL), so neither does this.
fn like_escape(q: &str) -> String {
    q.to_string()
}

/// The giveaways offered by autocomplete: the guild's giveaways in `statuses` whose prize contains `q`
/// (case-insensitive), whose id starts with `q` or whose message id is `q`; newest first, at most 25.
pub async fn find_giveaways_for_autocomplete(db: &bot_db::PgPool, guild_id: &str, statuses: &[&str], q: &str) -> anyhow::Result<Vec<AutocompleteRow>> {
    let mut sql = String::from(
        r#"SELECT "id", "prize", "status"::text AS status, "channelId", "endsAt" FROM "Giveaway" WHERE "guildId" = $1 AND "status"::text = ANY($2)"#,
    );
    if !q.is_empty() {
        sql.push_str(r#" AND ("prize" ILIKE $3 OR "id" LIKE $4 OR "messageId" = $5)"#);
    }
    sql.push_str(r#" ORDER BY "createdAt" DESC LIMIT 25"#);
    let statuses: Vec<String> = statuses.iter().map(|s| s.to_string()).collect();
    let mut query = sqlx::query(AssertSqlSafe(sql)).bind(guild_id).bind(statuses);
    if !q.is_empty() {
        let e = like_escape(q);
        query = query.bind(format!("%{e}%")).bind(format!("{e}%")).bind(q.to_string());
    }
    let mut out = Vec::new();
    for r in query.fetch_all(db).await? {
        out.push(AutocompleteRow {
            id: r.try_get("id")?,
            prize: r.try_get("prize")?,
            status: GiveawayStatus::parse(&r.try_get::<String, _>("status")?).unwrap_or(GiveawayStatus::Active),
            channel_id: r.try_get("channelId")?,
            ends_at: r.try_get::<chrono::NaiveDateTime, _>("endsAt")?.and_utc(),
        });
    }
    Ok(out)
}

/// `ModuleKey::Giveaways` (for the plugin and command declarations).
pub const MODULE: ModuleKey = ModuleKey::Giveaways;
