//! Port of `src/dashboard/collections/levels.js` (the XP leaderboard) and `balances.js` (coins per member): the two
//! member lists, plus the pure helpers the three member lists share (levels, balances, cases): member names and
//! avatars from ctx.discord, the member search and the page cursors.

use std::collections::HashMap;
use std::sync::Arc;

use bot_config::constants::xp_for_level;
use serde_json::{Value, json};

use crate::collections::embeds::user_avatar_url;
use crate::discord::{DashboardDiscord, MemberProfile};
use crate::errors::RecordError;
use crate::util::{raw, strip_undefined, fmt_num, is_snowflake, iso, number_of};

/// Rows per page of a member list (the API answers `nextCursor` while there are more).
pub const PAGE_SIZE: usize = 25;

/// `isUserId(v)`.
pub fn is_user_id(v: &str) -> bool {
    is_snowflake(v)
}

/// `fmt(n)`: `1,234`.
pub fn fmt(n: f64) -> String {
    fmt_num(n)
}

/// `rankCursor(value, userId)`: `<value>_<userId>` (a list ordered by a number descending, then the user id).
pub fn rank_cursor(value: i64, user_id: &str) -> String {
    format!("{value}_{user_id}")
}

/// A read rank cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankCursor {
    pub value: i64,
    pub user_id: String,
}

impl RankCursor {
    /// The SQL condition of the rows after the cursor in "value desc, userId asc" order (`afterRankCursor`'s Prisma
    /// `where`), with `$value_param` / `$user_param` placeholders: `("col" < $1 OR ("col" = $1 AND "userId" > $2))`.
    pub fn sql_after(&self, column: &str, value_param: usize, user_param: usize) -> String {
        format!("(\"{column}\" < ${value_param} OR (\"{column}\" = ${value_param} AND \"userId\" > ${user_param}))")
    }
}

/// `readRankCursor(cursor)`: anything else than `<1-10 digits>_<snowflake>` with a value that fits int4 is None (the
/// first page).
pub fn read_rank_cursor(cursor: &str) -> Option<RankCursor> {
    let (v, id) = cursor.split_once('_')?;
    if !(1..=10).contains(&v.len()) || !v.bytes().all(|b| b.is_ascii_digit()) || !is_snowflake(id) {
        return None;
    }
    let value: i64 = v.parse().ok()?;
    (value <= 2_147_483_647).then(|| RankCursor { value, user_id: id.to_string() })
}

/// `afterRankCursor(column, cursor)`: the Prisma `where` of the JS (`{}` for the first page), for parity checks; SQL
/// stores use [`RankCursor::sql_after`].
pub fn after_rank_cursor(column: &str, cursor: &str) -> Value {
    match read_rank_cursor(cursor) {
        None => json!({}),
        Some(c) => json!({ "OR": [{ column: { "lt": c.value } }, { column: c.value, "userId": { "gt": c.user_id } }] }),
    }
}

/// `loadProfiles(discord, ids)`: names and avatars of users (cached by the dashboard's client for an hour). Never
/// fails: without Discord (or when it fails, logged) the map is empty and the lists show ids and default avatars.
pub async fn load_profiles(discord: Option<&Arc<dyn DashboardDiscord>>, ids: &[String]) -> HashMap<String, Option<MemberProfile>> {
    let mut wanted: Vec<String> = Vec::new();
    for id in ids {
        if is_user_id(id) && !wanted.contains(id) {
            wanted.push(id.clone());
        }
    }
    wanted.truncate(100);
    let Some(discord) = discord.filter(|_| !wanted.is_empty()) else { return HashMap::new() };
    match discord.get_users(&wanted).await {
        Ok(map) => map,
        Err(err) => {
            tracing::warn!("Dashboard: member names could not be loaded: {}", err.message);
            HashMap::new()
        }
    }
}

/// `searchUserIds(discord, guildId, query)`: the user ids a list search means: the id itself when `query` is one
/// (a mention `<@id>` too), else the members whose name starts with it (Discord's member search). Without a Discord
/// client only ids can be searched.
pub async fn search_user_ids(discord: Option<&Arc<dyn DashboardDiscord>>, guild_id: &str, query: &str) -> Result<Vec<String>, RecordError> {
    let unwrapped = mention_id(query).unwrap_or(query);
    let q = bot_util::js::trim(unwrapped);
    if q.is_empty() {
        return Ok(Vec::new());
    }
    if is_user_id(q) {
        return Ok(vec![q.to_string()]);
    }
    let Some(discord) = discord else { return Ok(Vec::new()) };
    match discord.search_members(guild_id, q).await {
        Ok(found) => {
            let mut out: Vec<String> = Vec::new();
            for m in found {
                if is_user_id(&m.id) && !out.contains(&m.id) {
                    out.push(m.id);
                }
            }
            Ok(out)
        }
        Err(_) => Err(RecordError::discord("Discord did not answer the member search. Try again in a moment.")),
    }
}

/// The id of a whole-string `<@id>` / `<@!id>` mention.
fn mention_id(q: &str) -> Option<&str> {
    let inner = q.strip_prefix("<@")?.strip_suffix('>')?;
    let id = inner.strip_prefix('!').unwrap_or(inner);
    is_snowflake(id).then_some(id)
}

/// `profileName(profile, fallback)`: "Alice" (global name, else username); `fallback` (the id) for an unknown user.
pub fn profile_name(profile: Option<&MemberProfile>, fallback: Option<&str>) -> String {
    let fb = || fallback.unwrap_or("Unknown user").to_string();
    match profile {
        None => fb(),
        Some(p) => match p.global_name.as_deref().filter(|g| !g.is_empty()) {
            Some(g) => g.to_string(),
            None if !p.username.is_empty() => p.username.clone(),
            None => fb(),
        },
    }
}

/// `profileLabel(profile, id)`: "Alice (@alice)" / "@alice", or "Unknown user (id)".
pub fn profile_label(profile: Option<&MemberProfile>, id: &str) -> String {
    let Some(p) = profile else { return format!("Unknown user ({id})") };
    let at = if p.username.is_empty() { id.to_string() } else { format!("@{}", p.username) };
    match p.global_name.as_deref().filter(|g| !g.is_empty() && *g != p.username) {
        Some(g) => format!("{g} ({at})"),
        None => at,
    }
}

/// `avatarFor(profile, id)`: theirs, else Discord's default for the id.
pub fn avatar_for(profile: Option<&MemberProfile>, id: &str) -> String {
    match profile.map(|p| p.avatar_url.as_str()).filter(|a| !a.is_empty()) {
        Some(a) => a.to_string(),
        None => user_avatar_url(id, None),
    }
}

/// `toIso(d)`.
pub fn to_iso(v: Option<&Value>) -> Value {
    iso(v)
}

/// Shown when the bot was still working on a create when the dashboard stopped waiting.
pub const PENDING_CREATE: &str = "The bot is still working on it. Refresh the list in a moment to see the result.";

/// `levelProgress(total)`: level, XP into the level and XP the level needs, for a total.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LevelProgress {
    pub level: i64,
    pub into: i64,
    pub need: i64,
}

/// `levelProgress(total)` (the same sums as xpService: levelFromXp, xpIntoLevel).
pub fn level_progress(total: f64) -> LevelProgress {
    let mut remaining = if total.is_finite() && total > 0.0 { total.trunc() as i64 } else { 0 };
    let mut level = 0;
    while level < 2000 && remaining >= xp_for_level(level) {
        remaining -= xp_for_level(level);
        level += 1;
    }
    LevelProgress { level, into: remaining, need: xp_for_level(level) }
}

/// /levelconfig xp give and remove take 1 to 1,000,000 XP at a time.
pub const XP_STEP_MAX: i64 = 1_000_000;

fn rank_text(rank: Option<i64>) -> Option<String> {
    rank.filter(|r| *r != 0).map(|r| format!("#{}", fmt(r as f64)))
}

/// `levelItem(row, { rank, profile })`: a UserLevel row as a list item. The level is computed from the XP.
pub fn level_item(row: &Value, rank: Option<i64>, profile: Option<&MemberProfile>) -> Value {
    let user_id = row.get("userId").and_then(Value::as_str).unwrap_or_default();
    let xp = number_of(row.get("xp"));
    let p = level_progress(xp);
    let name = profile_name(profile, row.get("userId").and_then(Value::as_str));
    let summary: Vec<String> = [Some(format!("Level {}", p.level)), Some(format!("{} XP", fmt(xp))), rank_text(rank)].into_iter().flatten().collect();
    strip_undefined(json!({
        "id": raw(row.get("userId")),
        "userId": raw(row.get("userId")),
        "amount": null,
        "avatarUrl": avatar_for(profile, user_id),
        "name": name,
        "summary": summary.join(" · "),
        "rank": rank,
        "level": p.level,
        "xp": raw(row.get("xp")),
        "progress": format!("{} / {} XP", fmt(p.into as f64), fmt(p.need as f64)),
        "messages": row.get("messageCount").filter(|v| !v.is_null()).cloned().unwrap_or(json!(0)),
        "voiceMinutes": row.get("voiceMinutes").filter(|v| !v.is_null()).cloned().unwrap_or(json!(0)),
        "lastXpAt": iso(row.get("lastXpAt")),
    }))
}

/// `emptyLevelItem(userId, profile)`: the item of a member whose XP row is gone (after a reset).
pub fn empty_level_item(user_id: &str, profile: Option<&MemberProfile>) -> Value {
    level_item(&json!({ "userId": user_id, "xp": 0, "messageCount": 0, "voiceMinutes": 0, "lastXpAt": null }), None, profile)
}

// ── balances.js ───────────────────────────────────────────────────────────────────────────────────────────────

/// `RESET_OPTIONS` (value, label, description) of /economy-admin reset.
pub const RESET_OPTIONS: [(&str, &str, &str); 3] = [
    ("coins", "Coins", "Their balance goes back to the starting balance; their daily streak and cooldowns are cleared."),
    ("items", "Items", "Every item in their inventory is removed. Shop items stay."),
    ("all", "Coins and items", "Both of the above."),
];

/// The server's currency (`{ name, emoji }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Currency {
    pub name: String,
    pub emoji: String,
}

impl Default for Currency {
    fn default() -> Self {
        Self { name: "coins".into(), emoji: "🪙".into() }
    }
}

/// `balanceItem(row, { rank, profile, currency })`: an EconomyAccount row as a list item.
pub fn balance_item(row: &Value, rank: Option<i64>, profile: Option<&MemberProfile>, currency: &Currency) -> Value {
    let user_id = row.get("userId").and_then(Value::as_str).unwrap_or_default();
    let balance = number_of(row.get("balance"));
    let summary: Vec<String> =
        [Some(format!("{} {} {}", currency.emoji, fmt(balance), currency.name)), rank_text(rank)].into_iter().flatten().collect();
    strip_undefined(json!({
        "id": raw(row.get("userId")),
        "userId": raw(row.get("userId")),
        "amount": null,
        "avatarUrl": avatar_for(profile, user_id),
        "name": profile_name(profile, row.get("userId").and_then(Value::as_str)),
        "summary": summary.join(" · "),
        "rank": rank,
        "balance": raw(row.get("balance")),
        "dailyStreak": row.get("dailyStreak").filter(|v| !v.is_null()).cloned().unwrap_or(json!(0)),
        "lastDaily": iso(row.get("lastDaily")),
        "lastWork": iso(row.get("lastWork")),
        "updatedAt": iso(row.get("updatedAt")),
    }))
}
