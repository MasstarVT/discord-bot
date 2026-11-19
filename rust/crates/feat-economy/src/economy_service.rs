//! Port of `src/services/economyService.js`: per-server economy accounts (with the lazy copy of the legacy global
//! wallet), escrowed casino bets, streak `/daily`, `/work`, `/pity`, coin boosts, transfers, the shop and inventory
//! transactions, the reset, the `coins` rewards provider and the casino lock. This is the only module of the crate
//! that runs SQL.
//!
//! Invariants (pkg-economy.md §3.2), kept exactly as in the JS:
//!  - `0 <= EconomyAccount.balance <= economy_config(settings).max_balance <= COIN_MAX (1e9) < 2^31`. Every balance write
//!    is ONE SQL statement (or one transaction) using `::bigint` maths and `LEAST(.., cap)` / a conditional `WHERE`;
//!    there is no read-modify-write in Rust. The one exception to the per-server cap: [`refund_purchase`] returns the
//!    full price up to `COIN_MAX`, so a refund is never lost.
//!  - `UserEconomy` (the legacy global wallet) is read only, and only by [`get_account`] / [`peek_account`].
//!  - Everything that needs the pool runs BEFORE a transaction opens; inside it only the transaction's connection is
//!    used.
//!
//! Errors: functions return `anyhow::Result`. An expected failure is an [`EconomyError`] inside the `anyhow::Error`
//! (the JS `EconomyError`); read it with [`economy_error`]. Anything else is a database failure.

use std::collections::HashSet;
use std::sync::Mutex;

use bot_config::COIN_MAX;
use bot_config::limits;
use bot_core::App;
use bot_core::interaction::Responder;
use bot_core::interaction_helpers::{Payload, is_interaction_gone};
use bot_core::rewards::{RewardArgs, RewardProvider, RewardResult};
use bot_core::settings::is_module_enabled;
use bot_db::sqlx::postgres::{PgArguments, PgRow};
use bot_db::sqlx::{self, Postgres, Row};
use bot_db::{
    BoostPeriod, ChannelListMode, DbError, ECONOMY_ACCOUNT_INFO, ECONOMY_BOOST_INFO, EconomyAccount, EconomyBoost, GuildSettings,
    INVENTORY_ITEM_INFO, InventoryItem, InsertBuilder, SHOP_ITEM_INFO, SQL_NOW, SelectBuilder, ShopItem, ShopItemType, TxOptions,
    UpdateBuilder,
};
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use serde::Serialize;
use serde_json::{Map, Value, json};
use serenity::builder::CreateEmbed;

// ── Constants ─────────────────────────────────────────────────────────────────

pub const PITY_THRESHOLD: i64 = 100;
pub const PITY_AMOUNT: i64 = 500;
pub const PITY_COOLDOWN_MS: i64 = 12 * 60 * 60 * 1000;
pub const DAILY_COOLDOWN_MS: i64 = 24 * 60 * 60 * 1000;
pub const DAILY_STREAK_WINDOW_MS: i64 = 48 * 60 * 60 * 1000;
/// Day 1..6, then day 7+.
pub const DAILY_MULTIPLIER_TENTHS: [i64; 7] = [10, 11, 12, 14, 18, 25, 40];
pub const ITEM_NAME_MAX: usize = 50;
pub const ITEM_DESCRIPTION_MAX: usize = 200;
pub const ITEM_MESSAGE_MAX: usize = 1000;
pub const ITEM_IMAGE_MAX: usize = 500;
pub const MAX_STOCK: i64 = 1_000_000;
/// daily/work min/max upper bound.
pub const RANGE_MAX: i64 = 1_000_000;
pub const WORK_COOLDOWN_MAX_MINUTES: i64 = 10_080;
pub const RICHEST_LIMIT: i64 = 100;
pub const GAME_CHANNELS_MAX: usize = limits::ECONOMY_GAME_CHANNELS;
pub const BOOST_NAME_MAX: usize = 32;
pub const BOOST_PERCENT_MAX: i64 = 500;
pub const DEFAULT_BUY_MESSAGE: &str = "You bought **{quantity}× {item}** for {currency.emoji} **{coins}** {currency}!";
pub const DEFAULT_USE_MESSAGE: &str = "{user} used **{item}**!";
pub const WORK_LINES: [&str; 12] = [
    "You delivered pizzas all over town",
    "You fixed a bug in production",
    "You walked the neighbours' dogs",
    "You streamed for six hours straight",
    "You moderated a very busy chat",
    "You baked bread at the local bakery",
    "You mowed lawns all afternoon",
    "You tutored a student in maths",
    "You drove a taxi through rush hour",
    "You washed cars at the car wash",
    "You sold lemonade on the corner",
    "You designed a logo for a client",
];

const COIN_MAX_I64: i64 = COIN_MAX as i64;
const COPIES_MAX: i64 = limits::ITEM_COPIES_PER_USER as i64;
/// Number.MAX_SAFE_INTEGER.
const MAX_SAFE: i64 = 9_007_199_254_740_991;

/// `ITEM_TYPE_LABELS`.
pub fn item_type_label(t: ShopItemType) -> &'static str {
    match t {
        ShopItemType::Static => "📦 Collectible",
        ShopItemType::Usable => "✨ Usable",
        ShopItemType::Role => "🎭 Role",
    }
}

/// `BOOST_PERIOD_LABELS`.
pub fn boost_period_label(p: BoostPeriod) -> &'static str {
    match p {
        BoostPeriod::OneTime => "One-time",
        BoostPeriod::Daily => "Daily",
        BoostPeriod::Weekly => "Weekly",
        BoostPeriod::Monthly => "Monthly",
    }
}

// ── EconomyError ──────────────────────────────────────────────────────────────

/// The `code` of an [`EconomyError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EconomyCode {
    Invalid,
    Insufficient,
    MaxBet,
    OutOfStock,
    MaxOwned,
    RecipientCap,
    NotOwned,
    ItemGone,
}

impl EconomyCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "INVALID",
            Self::Insufficient => "INSUFFICIENT",
            Self::MaxBet => "MAX_BET",
            Self::OutOfStock => "OUT_OF_STOCK",
            Self::MaxOwned => "MAX_OWNED",
            Self::RecipientCap => "RECIPIENT_CAP",
            Self::NotOwned => "NOT_OWNED",
            Self::ItemGone => "ITEM_GONE",
        }
    }
}

/// Expected failure of an economy operation (`class EconomyError`): `code` and the JS `data` object
/// (`{ max }`, `{ total }`, `{ reason }`, `{ max, owned }`).
#[derive(Debug, Clone, PartialEq)]
pub struct EconomyError {
    pub code: EconomyCode,
    pub data: Map<String, Value>,
}

impl EconomyError {
    pub fn new(code: EconomyCode) -> Self {
        Self { code, data: Map::new() }
    }

    pub fn with(code: EconomyCode, data: Value) -> Self {
        Self { code, data: data.as_object().cloned().unwrap_or_default() }
    }

    /// `err.data[key]` as an integer.
    pub fn data_i64(&self, key: &str) -> Option<i64> {
        self.data.get(key).and_then(Value::as_i64)
    }

    /// `err.data.reason`.
    pub fn reason(&self) -> Option<&str> {
        self.data.get("reason").and_then(Value::as_str)
    }
}

impl std::fmt::Display for EconomyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code.as_str())
    }
}

impl std::error::Error for EconomyError {}

fn fail<T>(code: EconomyCode) -> anyhow::Result<T> {
    Err(EconomyError::new(code).into())
}

fn fail_with<T>(code: EconomyCode, data: Value) -> anyhow::Result<T> {
    Err(EconomyError::with(code, data).into())
}

/// `err instanceof EconomyError ? err : null`.
pub fn economy_error(err: &anyhow::Error) -> Option<&EconomyError> {
    err.downcast_ref::<EconomyError>()
}

/// `err instanceof EconomyError && err.code === code`.
pub fn is_code(err: &anyhow::Error, code: EconomyCode) -> bool {
    economy_error(err).is_some_and(|e| e.code == code)
}

/// The PostgreSQL SQLSTATE behind an error (a bot-db or sqlx error anywhere in the chain).
pub fn sqlstate_of(err: &anyhow::Error) -> Option<String> {
    for cause in err.chain() {
        if let Some(db) = cause.downcast_ref::<DbError>()
            && let Some(code) = db.sqlstate()
        {
            return Some(code);
        }
        if let Some(sqlx::Error::Database(e)) = cause.downcast_ref::<sqlx::Error>()
            && let Some(code) = e.code()
        {
            return Some(code.into_owned());
        }
    }
    None
}

/// `isForeignKeyError(err)`: SQLSTATE 23503 (Prisma P2003).
fn is_foreign_key_error(err: &anyhow::Error) -> bool {
    sqlstate_of(err).as_deref() == Some("23503")
}

/// Prisma P2002 (unique violation, SQLSTATE 23505).
fn is_unique_error(err: &anyhow::Error) -> bool {
    sqlstate_of(err).as_deref() == Some("23505")
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

/// `clampInt(n, min, max)`: `n` truncated and clamped to min..max; a non-finite value gives `min`.
pub fn clamp_int(n: f64, min: i64, max: i64) -> i64 {
    if n.is_finite() { (n.trunc().max(min as f64).min(max as f64)) as i64 } else { min }
}

/// `big(n)`: `BigInt(Math.trunc(n))`, the bigint SQL parameter of an amount.
pub fn big(n: f64) -> i64 {
    n.trunc() as i64
}

/// `Number.isSafeInteger(n)` for an `i64`.
pub fn is_safe_integer(n: i64) -> bool {
    (-MAX_SAFE..=MAX_SAFE).contains(&n)
}

/// Sanitised economy settings (`economyConfig(settings)`). `None` gives every default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EconomyConfig {
    pub currency_name: String,
    pub currency_emoji: String,
    pub starting_balance: i64,
    pub max_balance: i64,
    pub max_bet: i64,
    pub daily_min: i64,
    pub daily_max: i64,
    pub work_min: i64,
    pub work_max: i64,
    pub work_cooldown_minutes: i64,
    pub game_channel_ids: Vec<String>,
    pub game_channel_mode: ChannelListMode,
    pub max_payout: i64,
}

/// `economyConfig(settings)`.
pub fn economy_config(settings: Option<&GuildSettings>) -> EconomyConfig {
    let int = |f: fn(&GuildSettings) -> i32, fallback: i64| settings.map(|s| i64::from(f(s))).unwrap_or(fallback) as f64;
    let max_balance = clamp_int(int(|s| s.economy_max_balance, COIN_MAX_I64), 1, COIN_MAX_I64);
    let starting_balance = clamp_int(int(|s| s.economy_starting_balance, 1000), 0, max_balance);
    let max_bet = clamp_int(int(|s| s.economy_max_bet, 0), 0, COIN_MAX_I64);
    let daily_min = clamp_int(int(|s| s.daily_min, 50), 1, RANGE_MAX);
    let daily_max = daily_min.max(clamp_int(int(|s| s.daily_max, 120), 1, RANGE_MAX));
    let work_min = clamp_int(int(|s| s.work_min, 100), 1, RANGE_MAX);
    let work_max = work_min.max(clamp_int(int(|s| s.work_max, 300), 1, RANGE_MAX));
    let text = |v: Option<&String>, d: &str| v.filter(|s| !s.is_empty()).cloned().unwrap_or_else(|| d.to_string());
    EconomyConfig {
        currency_name: text(settings.map(|s| &s.currency_name), "coins"),
        currency_emoji: text(settings.map(|s| &s.currency_emoji), "🪙"),
        starting_balance,
        max_balance,
        max_bet,
        daily_min,
        daily_max,
        work_min,
        work_max,
        work_cooldown_minutes: clamp_int(int(|s| s.work_cooldown_minutes, 60), 1, WORK_COOLDOWN_MAX_MINUTES),
        game_channel_ids: settings.map(|s| s.economy_game_channel_ids.clone()).unwrap_or_default(),
        game_channel_mode: if settings.is_some_and(|s| s.economy_game_channel_mode == ChannelListMode::Deny) {
            ChannelListMode::Deny
        } else {
            ChannelListMode::Allow
        },
        max_payout: clamp_int(int(|s| s.economy_max_payout, 0), 0, COIN_MAX_I64),
    }
}

/// `formatCoins(n, settings, { bold })`: `🪙 **1,234** coins`.
pub fn format_coins(n: i64, settings: Option<&GuildSettings>, bold: bool) -> String {
    let cfg = economy_config(settings);
    let b = if bold { "**" } else { "" };
    format!("{} {b}{}{b} {}", cfg.currency_emoji, fmt_num(n), cfg.currency_name)
}

/// `formatNet(net, settings)`: `+🪙 **5** coins`, `−…` (U+2212) or `±…`.
pub fn format_net(net: i64, settings: Option<&GuildSettings>) -> String {
    match net.cmp(&0) {
        std::cmp::Ordering::Greater => format!("+{}", format_coins(net, settings, true)),
        std::cmp::Ordering::Less => format!("−{}", format_coins(-net, settings, true)),
        std::cmp::Ordering::Equal => format!("±{}", format_coins(0, settings, true)),
    }
}

/// `formatNumber(n)` for an integer.
pub fn fmt_num(n: i64) -> String {
    bot_util::text::format_number(n as f64)
}

/// `streakTenths(streak)`: the streak multiplier in tenths for a streak day (1..6, then 7+).
pub fn streak_tenths(streak: i64) -> i64 {
    let s = if streak == 0 { 1 } else { streak };
    DAILY_MULTIPLIER_TENTHS[(s.clamp(1, 7) - 1) as usize]
}

/// `streakMultiplierLabel(streak)`: `×1`, `×1.1`, ... `×4`.
pub fn streak_multiplier_label(streak: i64) -> String {
    format!("×{}", bot_util::js::number_to_string(streak_tenths(streak) as f64 / 10.0))
}

/// `dailyAmount(base, streak)`: round-half-up of `base × multiplier`; identical to the SQL of [`claim_daily`].
pub fn daily_amount(base: i64, streak: i64) -> i64 {
    (base * streak_tenths(streak) + 5).div_euclid(10)
}

/// `nextDailyStreak(lastDaily, currentStreak, now)`: the streak a claim made now would have (display only).
pub fn next_daily_streak(last_daily: Option<DateTime<Utc>>, current: i64, now: DateTime<Utc>) -> i64 {
    match last_daily {
        Some(l) if (now - l).num_milliseconds() <= DAILY_STREAK_WINDOW_MS => current.min(999_999) + 1,
        _ => 1,
    }
}

/// `activeStreak(lastDaily, currentStreak, now)`: the streak still alive (0 after more than 48 h).
pub fn active_streak(last_daily: Option<DateTime<Utc>>, current: i64, now: DateTime<Utc>) -> i64 {
    match last_daily {
        Some(l) if (now - l).num_milliseconds() <= DAILY_STREAK_WINDOW_MS => current,
        _ => 0,
    }
}

/// The result of [`game_channel_check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GameChannelCheck {
    Ok,
    /// Allow mode and the channel is not listed.
    NotAllowed { allowed_ids: Vec<String> },
    /// Deny mode and the channel (or its parent) is listed.
    Denied { denied_ids: Vec<String> },
}

impl GameChannelCheck {
    pub fn ok(&self) -> bool {
        matches!(self, Self::Ok)
    }

    /// The JS object: `{ ok: true }`, `{ ok: false, mode: 'ALLOW', allowedIds }` or `{ ok: false, mode: 'DENY', deniedIds }`.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Ok => json!({ "ok": true }),
            Self::NotAllowed { allowed_ids } => json!({ "ok": false, "mode": "ALLOW", "allowedIds": allowed_ids }),
            Self::Denied { denied_ids } => json!({ "ok": false, "mode": "DENY", "deniedIds": denied_ids }),
        }
    }
}

/// `gameChannelCheck(settings, channel, guild)`: whether a casino game may run in `channel_id`. Only channels that
/// still exist (`exists`) count; an empty (or fully deleted) list means "everywhere" in both modes. A thread counts
/// as its parent (`thread_parent`).
pub fn game_channel_check(
    settings: Option<&GuildSettings>,
    channel_id: &str,
    thread_parent: Option<&str>,
    exists: impl Fn(&str) -> bool,
) -> GameChannelCheck {
    let cfg = economy_config(settings);
    let ids: Vec<String> = cfg.game_channel_ids.into_iter().filter(|id| exists(id)).collect();
    if ids.is_empty() {
        return GameChannelCheck::Ok;
    }
    let listed = ids.iter().any(|i| i == channel_id) || thread_parent.is_some_and(|p| ids.iter().any(|i| i == p));
    if cfg.game_channel_mode == ChannelListMode::Deny {
        return if listed { GameChannelCheck::Denied { denied_ids: ids } } else { GameChannelCheck::Ok };
    }
    if listed { GameChannelCheck::Ok } else { GameChannelCheck::NotAllowed { allowed_ids: ids } }
}

/// [`game_channel_check`] against the serenity cache: a channel exists when the guild has it (channels and threads),
/// and a thread's parent comes from the cache or from the interaction's partial channel. Offline (no cache) nothing
/// exists, so every channel is allowed (fail-open, as the JS with an empty cache).
pub fn game_channel_check_cached(
    app: &App,
    settings: Option<&GuildSettings>,
    guild_id: serenity::all::GuildId,
    channel: Option<&serenity::all::PartialChannel>,
    channel_id: serenity::all::ChannelId,
) -> GameChannelCheck {
    let guild = app.discord().and_then(|d| d.cache.guild(guild_id).map(|g| g.clone()));
    let exists = |id: &str| {
        let Some(g) = guild.as_ref() else { return false };
        let Some(cid) = id.parse::<u64>().ok().filter(|v| *v != 0).map(serenity::all::ChannelId::new) else { return false };
        g.channels.contains_key(&cid) || g.threads.iter().any(|t| t.id == cid)
    };
    let is_thread = |k: serenity::all::ChannelType| {
        matches!(k, serenity::all::ChannelType::PublicThread | serenity::all::ChannelType::PrivateThread | serenity::all::ChannelType::NewsThread)
    };
    let parent = guild
        .as_ref()
        .and_then(|g| g.threads.iter().find(|t| t.id == channel_id).and_then(|t| t.parent_id))
        .or_else(|| channel.filter(|c| is_thread(c.kind)).and_then(|c| c.parent_id))
        .map(|p| p.to_string());
    game_channel_check(settings, &channel_id.to_string(), parent.as_deref(), exists)
}

/// `clampPayout(payout, stake, maxPayout)`: limits the gross payout of one game to `stake + maxPayout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ClampedPayout {
    pub payout: i64,
    pub capped: bool,
}

pub fn clamp_payout(payout: i64, stake: i64, max_payout: i64) -> ClampedPayout {
    if max_payout <= 0 {
        return ClampedPayout { payout, capped: false };
    }
    let limit = stake + max_payout;
    if payout > limit { ClampedPayout { payout: limit, capped: true } } else { ClampedPayout { payout, capped: false } }
}

/// `str.trim().replace(/\s+/g, ' ')` with the JS definitions of whitespace.
fn collapse_spaces(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_space = false;
    for c in raw.chars() {
        if bot_util::js::is_js_space(c) {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

/// `validateCurrencyName(str)`: `Ok(value)` or `Err(reason)`.
pub fn validate_currency_name(s: &str) -> Result<String, String> {
    let raw = bot_util::js::trim(s);
    let value = collapse_spaces(raw);
    let len = bot_util::js::utf16_len(&value);
    if !(1..=32).contains(&len) {
        return Err("The currency name must be 1 to 32 characters.".into());
    }
    if raw.chars().any(|c| matches!(c, '\r' | '\n' | '`' | '*' | '_' | '~' | '|' | '<' | '>' | '@' | '#' | '\\')) {
        return Err("Use letters, numbers, spaces and simple punctuation only (no markdown, mentions or newlines).".into());
    }
    Ok(value)
}

fn custom_emoji_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^<a?:[A-Za-z0-9_]{2,32}:(\d{17,20})>$").unwrap_or_else(|_| unreachable!()))
}

fn unicode_emoji_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"^(?:\p{Extended_Pictographic}|\p{Regional_Indicator})(?:\p{Extended_Pictographic}|\p{Regional_Indicator}|\p{Emoji_Modifier}|\x{FE0F}|\x{200D}|\x{20E3})*$",
        )
        .unwrap_or_else(|_| unreachable!())
    })
}

/// `validateCurrencyEmoji(str)`: `Ok((value, customId))` or `Err(reason)`. `\d` is ASCII only, as in the JS.
pub fn validate_currency_emoji(s: &str) -> Result<(String, Option<String>), String> {
    let value = bot_util::js::trim(s).to_string();
    if let Some(c) = custom_emoji_re().captures(&value)
        && let Some(id) = c.get(1)
        && id.as_str().bytes().all(|b| b.is_ascii_digit())
    {
        let id = id.as_str().to_string();
        return Ok((value, Some(id)));
    }
    let n = value.chars().count();
    if (1..=10).contains(&n) && unicode_emoji_re().is_match(&value) {
        return Ok((value, None));
    }
    Err("That is not an emoji. Use a standard emoji like 🪙 or a custom emoji like <:coin:123…>.".into())
}

/// `validateItemName(str)`.
pub fn validate_item_name(s: &str) -> Result<String, String> {
    let raw = bot_util::js::trim(s);
    let value = collapse_spaces(raw);
    let len = bot_util::js::utf16_len(&value);
    if !(1..=ITEM_NAME_MAX).contains(&len) {
        return Err(format!("The name must be 1 to {ITEM_NAME_MAX} characters."));
    }
    if raw.contains(['\r', '\n']) {
        return Err("The name must be a single line.".into());
    }
    if value.to_lowercase() == "none" {
        return Err("Pick a different name.".into());
    }
    Ok(value)
}

/// `validateBoostName(str)`: the item name rules, up to [`BOOST_NAME_MAX`].
pub fn validate_boost_name(s: &str) -> Result<String, String> {
    let raw = bot_util::js::trim(s);
    let value = collapse_spaces(raw);
    let len = bot_util::js::utf16_len(&value);
    if !(1..=BOOST_NAME_MAX).contains(&len) {
        return Err(format!("The name must be 1 to {BOOST_NAME_MAX} characters."));
    }
    if raw.contains(['\r', '\n']) {
        return Err("The name must be a single line.".into());
    }
    Ok(value)
}

/// `effectiveMaxOwned(item)`: how many copies of an item one member may own.
pub fn effective_max_owned(item: &ShopItem) -> i64 {
    max_owned_of(item.can_buy_multiple, i64::from(item.max_per_user))
}

/// [`effective_max_owned`] from the two fields.
pub fn max_owned_of(can_buy_multiple: bool, max_per_user: i64) -> i64 {
    if can_buy_multiple { max_per_user.min(COPIES_MAX) } else { 1 }
}

/// `goesToInventory(item)`: an item used on purchase and destroyed by use never reaches the inventory.
pub fn goes_to_inventory(item: &ShopItem) -> bool {
    goes_to_inventory_of(item.use_on_buy, item.destroy_on_use, item.r#type)
}

pub fn goes_to_inventory_of(use_on_buy: bool, destroy_on_use: bool, t: ShopItemType) -> bool {
    !(use_on_buy && destroy_on_use && t != ShopItemType::Static)
}

/// `itemVars(item, { quantity, total, settings })`: the placeholder variables of the buy and use messages, in the JS
/// key order.
pub fn item_vars(name: &str, price: i64, quantity: i64, total: i64, settings: Option<&GuildSettings>) -> Vec<(String, String)> {
    let cfg = economy_config(settings);
    vec![
        ("item".into(), name.to_string()),
        ("quantity".into(), quantity.to_string()),
        ("coins".into(), fmt_num(total)),
        ("price".into(), fmt_num(price)),
        ("currency".into(), cfg.currency_name),
        ("currency.emoji".into(), cfg.currency_emoji),
    ]
}

/// `itemLabel(item)`: the bold, escaped, clamped item name.
pub fn item_label(name: &str) -> String {
    format!("**{}**", bot_util::text::clamp_text(&bot_core::markdown::escape_markdown(name), 100))
}

// ── Boost helpers (pure) ──────────────────────────────────────────────────────

/// `boostPeriodKey(period, tz, now)`: 'once', 'D:YYYY-MM-DD', 'W:YYYY-Www' (ISO week) or 'M:YYYY-MM' in the guild's
/// timezone (an invalid zone is UTC).
pub fn boost_period_key(period: BoostPeriod, tz: &str, now: DateTime<Utc>) -> String {
    if period == BoostPeriod::OneTime {
        return "once".into();
    }
    let p = bot_util::time::zoned_parts(&now, tz);
    let yyyy = |y: i64| format!("{y:04}");
    match period {
        BoostPeriod::Monthly => format!("M:{}-{:02}", yyyy(p.year), p.month),
        BoostPeriod::Daily => format!("D:{}-{:02}-{:02}", yyyy(p.year), p.month, p.day),
        _ => {
            // ISO 8601 week: the week belongs to the year that holds its Thursday.
            let Some(d) = i32::try_from(p.year).ok().and_then(|y| NaiveDate::from_ymd_opt(y, p.month, p.day)) else {
                return format!("W:{}-W01", yyyy(p.year));
            };
            let w = d.iso_week();
            format!("W:{}-W{:02}", yyyy(i64::from(w.year())), w.week())
        }
    }
}

/// `orderBoosts(boosts, memberRoleIds)`: enabled boosts the member qualifies for (holds `roleId`, or everyone when
/// it is null), highest percent first, then oldest, then by id.
pub fn order_boosts(boosts: &[EconomyBoost], member_role_ids: &[String]) -> Vec<EconomyBoost> {
    let roles: HashSet<&str> = member_role_ids.iter().map(String::as_str).collect();
    let mut out: Vec<EconomyBoost> =
        boosts.iter().filter(|b| b.enabled && b.role_id.as_deref().is_none_or(|r| roles.contains(r))).cloned().collect();
    out.sort_by(|a, b| b.percent.cmp(&a.percent).then(a.created_at.cmp(&b.created_at)).then_with(|| a.id.cmp(&b.id)));
    out
}

/// `boostedAmount(base, percent)`: `base` × (100 + percent) %, rounded down.
pub fn boosted_amount(base: i64, percent: i64) -> i64 {
    (base * (100 + percent)).div_euclid(100)
}

// ── Raw SQL ───────────────────────────────────────────────────────────────────
// Prisma DateTime columns are timestamp(3) WITHOUT time zone holding UTC: every timestamp uses SQL_NOW. Raw UPDATEs set
// "updatedAt"; raw INSERTs supply "id", "createdAt" and "updatedAt". Amounts are bound as bigint and cast.

/// A bound parameter of a [`Stmt`].
#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    Text(String),
    Int(i32),
    Big(i64),
    Bool(bool),
}

/// One raw statement and its parameters (`Prisma.sql`).
#[derive(Debug, Clone, PartialEq)]
pub struct Stmt {
    pub sql: String,
    pub args: Vec<Arg>,
}

impl Stmt {
    fn new(sql: String, args: Vec<Arg>) -> Self {
        Self { sql, args }
    }

    /// The sqlx query with every parameter bound.
    pub fn query(&self) -> sqlx::query::Query<'static, Postgres, PgArguments> {
        let mut q = sqlx::query(sqlx::AssertSqlSafe(self.sql.clone()));
        for a in &self.args {
            q = match a {
                Arg::Text(s) => q.bind(s.clone()),
                Arg::Int(n) => q.bind(*n),
                Arg::Big(n) => q.bind(*n),
                Arg::Bool(b) => q.bind(*b),
            };
        }
        q
    }
}

fn t(s: &str) -> Arg {
    Arg::Text(s.to_string())
}

/// `int` parameter (clamped into the int4 range, as Prisma would refuse anything else).
fn i(n: i64) -> Arg {
    Arg::Int(n.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32)
}

/// The statement builders (`const SQL = { ... }`), statement for statement.
pub mod sql {
    use super::{Arg, Stmt, i, t};
    use bot_db::SQL_NOW as NOW;

    pub fn insert_account(guild_id: &str, user_id: &str, balance: i64, imported_global: bool) -> Stmt {
        Stmt::new(
            format!(
                r#"
    INSERT INTO "EconomyAccount" ("id","guildId","userId","balance","dailyStreak","importedGlobal","createdAt","updatedAt")
    VALUES ($1, $2, $3, $4::bigint::int, 0, $5, {NOW}, {NOW})
    ON CONFLICT ("guildId","userId") DO NOTHING"#
            ),
            vec![t(&bot_db::new_uuid()), t(guild_id), t(user_id), Arg::Big(balance), Arg::Bool(imported_global)],
        )
    }

    pub fn credit(guild_id: &str, user_id: &str, amount: i64, cap: i64) -> Stmt {
        Stmt::new(
            format!(
                r#"
    UPDATE "EconomyAccount"
    SET "balance" = GREATEST("balance"::bigint, LEAST("balance"::bigint + $1::bigint, $2::bigint))::int,
        "updatedAt" = {NOW}
    WHERE "guildId" = $3 AND "userId" = $4
    RETURNING "balance""#
            ),
            vec![Arg::Big(amount), Arg::Big(cap), t(guild_id), t(user_id)],
        )
    }

    pub fn debit(guild_id: &str, user_id: &str, amount: i64) -> Stmt {
        Stmt::new(
            format!(
                r#"
    UPDATE "EconomyAccount"
    SET "balance" = ("balance"::bigint - $1::bigint)::int, "updatedAt" = {NOW}
    WHERE "guildId" = $2 AND "userId" = $3 AND "balance" >= $4::bigint
    RETURNING "balance""#
            ),
            vec![Arg::Big(amount), t(guild_id), t(user_id), Arg::Big(amount)],
        )
    }

    pub fn lock_accounts(guild_id: &str, from_id: &str, to_id: &str) -> Stmt {
        Stmt::new(
            r#"
    SELECT "id" FROM "EconomyAccount"
    WHERE "guildId" = $1 AND "userId" IN ($2, $3)
    ORDER BY "userId" FOR UPDATE"#
                .to_string(),
            vec![t(guild_id), t(from_id), t(to_id)],
        )
    }

    pub fn credit_checked(guild_id: &str, user_id: &str, amount: i64, cap: i64) -> Stmt {
        Stmt::new(
            format!(
                r#"
    UPDATE "EconomyAccount"
    SET "balance" = ("balance"::bigint + $1::bigint)::int, "updatedAt" = {NOW}
    WHERE "guildId" = $2 AND "userId" = $3
      AND "balance"::bigint + $4::bigint <= $5::bigint
    RETURNING "balance""#
            ),
            vec![Arg::Big(amount), t(guild_id), t(user_id), Arg::Big(amount), Arg::Big(cap)],
        )
    }

    pub fn remove_coins(guild_id: &str, user_id: &str, amount: i64) -> Stmt {
        Stmt::new(
            format!(
                r#"
    WITH old AS (
      SELECT "id", "balance" FROM "EconomyAccount"
      WHERE "guildId" = $1 AND "userId" = $2 FOR UPDATE
    )
    UPDATE "EconomyAccount" a
    SET "balance" = GREATEST(old."balance"::bigint - $3::bigint, 0)::int, "updatedAt" = {NOW}
    FROM old WHERE a."id" = old."id"
    RETURNING a."balance" AS "balance", old."balance" AS "before""#
            ),
            vec![t(guild_id), t(user_id), Arg::Big(amount)],
        )
    }

    /// `base`: the unboosted base reward; `boost_percent`: 0 when no boost applies. The streak (ns) exists only inside
    /// this statement.
    pub fn claim_daily(guild_id: &str, user_id: &str, base: i64, cap: i64, boost_percent: i64) -> Stmt {
        Stmt::new(
            format!(
                r#"
    WITH cur AS (
      SELECT "id",
             CASE WHEN "lastDaily" IS NULL OR "lastDaily" < {NOW} - interval '48 hours'
                  THEN 1 ELSE LEAST("dailyStreak", 999999) + 1 END AS ns
      FROM "EconomyAccount"
      WHERE "guildId" = $1 AND "userId" = $2
        AND ("lastDaily" IS NULL OR "lastDaily" <= {NOW} - interval '24 hours')
      FOR UPDATE
    )
    UPDATE "EconomyAccount" a SET
      "dailyStreak" = cur.ns,
      "balance" = GREATEST(a."balance"::bigint, LEAST(
          a."balance"::bigint
          + ((($3::bigint * (CASE LEAST(cur.ns, 7) WHEN 1 THEN 10 WHEN 2 THEN 11 WHEN 3 THEN 12
                                       WHEN 4 THEN 14 WHEN 5 THEN 18 WHEN 6 THEN 25 ELSE 40 END) + 5) / 10)
             * (100 + $4::bigint)) / 100,
          $5::bigint))::int,
      "lastDaily" = {NOW},
      "updatedAt" = {NOW}
    FROM cur
    WHERE a."id" = cur."id"
    RETURNING a."balance" AS "balance", a."dailyStreak" AS "dailyStreak", a."lastDaily" AS "lastDaily""#
            ),
            vec![t(guild_id), t(user_id), Arg::Big(base), Arg::Big(boost_percent), Arg::Big(cap)],
        )
    }

    pub fn claim_work(guild_id: &str, user_id: &str, amount: i64, cap: i64, cooldown_minutes: i64) -> Stmt {
        Stmt::new(
            format!(
                r#"
    UPDATE "EconomyAccount"
    SET "balance" = GREATEST("balance"::bigint, LEAST("balance"::bigint + $1::bigint, $2::bigint))::int,
        "lastWork" = {NOW}, "updatedAt" = {NOW}
    WHERE "guildId" = $3 AND "userId" = $4
      AND ("lastWork" IS NULL OR "lastWork" <= {NOW} - make_interval(mins => $5::int))
    RETURNING "balance", "lastWork""#
            ),
            vec![Arg::Big(amount), Arg::Big(cap), t(guild_id), t(user_id), i(cooldown_minutes)],
        )
    }

    pub fn claim_pity(guild_id: &str, user_id: &str, cap: i64) -> Stmt {
        Stmt::new(
            format!(
                r#"
    UPDATE "EconomyAccount"
    SET "balance" = GREATEST("balance"::bigint, LEAST("balance"::bigint + $1::bigint, $2::bigint))::int,
        "lastPity" = {NOW}, "updatedAt" = {NOW}
    WHERE "guildId" = $3 AND "userId" = $4
      AND "balance" < $5::int
      AND ("lastPity" IS NULL OR "lastPity" <= {NOW} - interval '12 hours')
    RETURNING "balance""#
            ),
            vec![Arg::Big(super::PITY_AMOUNT), Arg::Big(cap), t(guild_id), t(user_id), i(super::PITY_THRESHOLD)],
        )
    }

    pub fn insert_boost_use(boost_id: &str, user_id: &str, period_key: &str) -> Stmt {
        Stmt::new(
            format!(
                r#"
    INSERT INTO "EconomyBoostUse" ("id","boostId","userId","periodKey","createdAt")
    VALUES ($1, $2, $3, $4, {NOW})
    ON CONFLICT ("boostId","userId","periodKey") DO NOTHING
    RETURNING "id""#
            ),
            vec![t(&bot_db::new_uuid()), t(boost_id), t(user_id), t(period_key)],
        )
    }

    pub fn lock_settings_row(guild_id: &str) -> Stmt {
        Stmt::new(r#"SELECT 1 FROM "GuildSettings" WHERE "guildId" = $1 FOR UPDATE"#.to_string(), vec![t(guild_id)])
    }

    pub fn reserve_stock(guild_id: &str, item_id: &str, quantity: i64) -> Stmt {
        Stmt::new(
            format!(
                r#"
    UPDATE "ShopItem"
    SET "stock" = CASE WHEN "stock" IS NULL THEN NULL ELSE "stock" - $1::int END, "updatedAt" = {NOW}
    WHERE "id" = $2 AND "guildId" = $3 AND "enabled" = true AND ("stock" IS NULL OR "stock" >= $4::int)
    RETURNING *"#
            ),
            vec![i(quantity), t(item_id), t(guild_id), i(quantity)],
        )
    }

    pub fn add_inventory_capped(guild_id: &str, user_id: &str, item_id: &str, quantity: i64, max: i64) -> Stmt {
        Stmt::new(
            format!(
                r#"
    INSERT INTO "InventoryItem" ("id","guildId","userId","itemId","quantity","createdAt","updatedAt")
    VALUES ($1, $2, $3, $4, $5::int, {NOW}, {NOW})
    ON CONFLICT ("itemId","userId") DO UPDATE
    SET "quantity" = "InventoryItem"."quantity" + EXCLUDED."quantity", "updatedAt" = {NOW}
    WHERE "InventoryItem"."quantity" + EXCLUDED."quantity" <= $6::int
    RETURNING "quantity""#
            ),
            vec![t(&bot_db::new_uuid()), t(guild_id), t(user_id), t(item_id), i(quantity), i(max)],
        )
    }

    pub fn add_inventory(guild_id: &str, user_id: &str, item_id: &str, quantity: i64) -> Stmt {
        Stmt::new(
            format!(
                r#"
    INSERT INTO "InventoryItem" ("id","guildId","userId","itemId","quantity","createdAt","updatedAt")
    VALUES ($1, $2, $3, $4, $5::int, {NOW}, {NOW})
    ON CONFLICT ("itemId","userId") DO UPDATE
    SET "quantity" = "InventoryItem"."quantity" + EXCLUDED."quantity", "updatedAt" = {NOW}
    RETURNING "quantity""#
            ),
            vec![t(&bot_db::new_uuid()), t(guild_id), t(user_id), t(item_id), i(quantity)],
        )
    }

    pub fn restore_stock(guild_id: &str, item_id: &str, quantity: i64) -> Stmt {
        Stmt::new(
            format!(
                r#"
    UPDATE "ShopItem" SET "stock" = "stock" + $1::int, "updatedAt" = {NOW}
    WHERE "id" = $2 AND "guildId" = $3 AND "stock" IS NOT NULL"#
            ),
            vec![i(quantity), t(item_id), t(guild_id)],
        )
    }

    pub fn reduce_inventory(guild_id: &str, user_id: &str, item_id: &str, quantity: i64) -> Stmt {
        Stmt::new(
            format!(
                r#"
    UPDATE "InventoryItem"
    SET "quantity" = GREATEST("quantity" - $1::int, 0), "updatedAt" = {NOW}
    WHERE "itemId" = $2 AND "userId" = $3 AND "guildId" = $4"#
            ),
            vec![i(quantity), t(item_id), t(user_id), t(guild_id)],
        )
    }

    pub fn delete_empty_inventory(guild_id: &str, user_id: &str, item_id: &str) -> Stmt {
        Stmt::new(
            r#"
    DELETE FROM "InventoryItem"
    WHERE "itemId" = $1 AND "userId" = $2 AND "guildId" = $3 AND "quantity" <= 0"#
                .to_string(),
            vec![t(item_id), t(user_id), t(guild_id)],
        )
    }

    pub fn consume_item(guild_id: &str, user_id: &str, item_id: &str) -> Stmt {
        Stmt::new(
            format!(
                r#"
    UPDATE "InventoryItem"
    SET "quantity" = "quantity" - 1, "updatedAt" = {NOW}
    WHERE "itemId" = $1 AND "userId" = $2 AND "guildId" = $3 AND "quantity" >= 1
    RETURNING "quantity""#
            ),
            vec![t(item_id), t(user_id), t(guild_id)],
        )
    }

    pub fn take_item(guild_id: &str, user_id: &str, item_id: &str, quantity: i64) -> Stmt {
        Stmt::new(
            format!(
                r#"
    WITH old AS (
      SELECT "id", "quantity" FROM "InventoryItem"
      WHERE "itemId" = $1 AND "userId" = $2 AND "guildId" = $3 FOR UPDATE
    )
    UPDATE "InventoryItem" i
    SET "quantity" = GREATEST(old."quantity" - $4::int, 0), "updatedAt" = {NOW}
    FROM old WHERE i."id" = old."id"
    RETURNING i."quantity" AS "remaining", old."quantity" AS "before""#
            ),
            vec![t(item_id), t(user_id), t(guild_id), i(quantity)],
        )
    }
}

/// `RAW_SQL`: one entry per raw statement with sample parameters (run each under `EXPLAIN` in the tests).
pub fn raw_sql() -> Vec<(&'static str, Stmt)> {
    vec![
        ("economy.insertAccount", sql::insert_account("0", "0", 0, false)),
        ("economy.credit", sql::credit("0", "0", 1, COIN_MAX_I64)),
        ("economy.debit", sql::debit("0", "0", 1)),
        ("economy.lockAccounts", sql::lock_accounts("0", "1", "2")),
        ("economy.creditChecked", sql::credit_checked("0", "0", 1, COIN_MAX_I64)),
        ("economy.removeCoins", sql::remove_coins("0", "0", 1)),
        ("economy.claimDaily", sql::claim_daily("0", "0", 100, COIN_MAX_I64, 50)),
        ("economy.claimWork", sql::claim_work("0", "0", 100, COIN_MAX_I64, 60)),
        ("economy.claimPity", sql::claim_pity("0", "0", COIN_MAX_I64)),
        ("economy.insertBoostUse", sql::insert_boost_use("0", "0", "once")),
        ("economy.lockSettingsRow", sql::lock_settings_row("0")),
        ("economy.reserveStock", sql::reserve_stock("0", "0", 1)),
        ("economy.addInventoryCapped", sql::add_inventory_capped("0", "0", "0", 1, 200)),
        ("economy.addInventory", sql::add_inventory("0", "0", "0", 1)),
        ("economy.restoreStock", sql::restore_stock("0", "0", 1)),
        ("economy.reduceInventory", sql::reduce_inventory("0", "0", "0", 1)),
        ("economy.deleteEmptyInventory", sql::delete_empty_inventory("0", "0", "0")),
        ("economy.consumeItem", sql::consume_item("0", "0", "0")),
        ("economy.takeItem", sql::take_item("0", "0", "0", 1)),
    ]
}

/// The other statements this module runs (the Prisma client calls of the JS, written out), with sample parameters,
/// for the same `EXPLAIN` check.
pub fn client_sql() -> Vec<(&'static str, Stmt)> {
    vec![
        ("economy.clampBalancesToCap", clamp_balances_stmt("0", 1)),
        ("economy.topAccounts", top_accounts_stmt("0", 1)),
        ("economy.getRank", Stmt::new(RANK_SQL.into(), vec![t("0"), Arg::Big(0)])),
        ("economy.searchItems", search_items_stmt("0", "x", true, 25)),
        ("economy.listItems", list_items_stmt("0", false, true)),
        ("economy.getInventory", Stmt::new(inventory_sql(false, false, false), vec![t("0"), t("0")])),
        ("economy.searchInventory", Stmt::new(inventory_sql(true, true, true), vec![t("0"), t("0"), t("%x%")])),
        ("economy.listBoosts", Stmt::new(LIST_BOOSTS_SQL.into(), vec![t("0")])),
        ("economy.resetAccounts", reset_accounts_stmt("0", Some("0"), 0)),
    ]
}

/// Runs a statement and returns its rows.
async fn rows<'e, E: sqlx::PgExecutor<'e>>(exec: E, stmt: Stmt) -> anyhow::Result<Vec<PgRow>> {
    Ok(stmt.query().fetch_all(exec).await?)
}

async fn exec<'e, E: sqlx::PgExecutor<'e>>(exec: E, stmt: Stmt) -> anyhow::Result<u64> {
    Ok(stmt.query().execute(exec).await?.rows_affected())
}

fn int_col(row: &PgRow, col: &str) -> anyhow::Result<i64> {
    Ok(i64::from(row.try_get::<i32, _>(col)?))
}

fn date_col(row: &PgRow, col: &str) -> anyhow::Result<Option<DateTime<Utc>>> {
    Ok(row.try_get::<Option<chrono::NaiveDateTime>, _>(col)?.map(|d| d.and_utc()))
}

// ── Accounts ──────────────────────────────────────────────────────────────────

async fn find_account(app: &App, guild_id: &str, user_id: &str) -> anyhow::Result<Option<EconomyAccount>> {
    let q = SelectBuilder::new(&ECONOMY_ACCOUNT_INFO).filter("guildId", json!(guild_id))?.filter("userId", json!(user_id))?.build()?;
    Ok(q.fetch_optional::<EconomyAccount, _>(app.db()).await?)
}

async fn account_balance(app: &App, guild_id: &str, user_id: &str) -> anyhow::Result<Option<i64>> {
    Ok(find_account(app, guild_id, user_id).await?.map(|a| i64::from(a.balance)))
}

/// `initialBalance(userId, settings)`: the legacy global balance (clamped) unless the guild turned the import off,
/// else the starting balance. `(balance, importedGlobal)`.
async fn initial_balance(app: &App, user_id: &str, settings: Option<&GuildSettings>) -> anyhow::Result<(i64, bool)> {
    let cfg = economy_config(settings);
    if settings.is_some_and(|s| !s.economy_import_global) {
        return Ok((cfg.starting_balance, false));
    }
    let legacy: Option<(i32,)> =
        sqlx::query_as(r#"SELECT "balance" FROM "UserEconomy" WHERE "userId" = $1 LIMIT 1"#).bind(user_id).fetch_optional(app.db()).await?;
    Ok(match legacy {
        None => (cfg.starting_balance, false),
        Some((b,)) => (clamp_int(f64::from(b), 0, cfg.max_balance), true),
    })
}

/// THE lazy global copy (`getAccount`): the member's account on this server, created on first touch with the legacy
/// global `UserEconomy` balance (clamped to 0..max balance), or the starting balance when there is none or the import
/// is off. Idempotent and race-safe (`ON CONFLICT DO NOTHING`, then read back).
pub async fn get_account(app: &App, guild_id: &str, user_id: &str, settings: Option<&GuildSettings>) -> anyhow::Result<EconomyAccount> {
    if let Some(existing) = find_account(app, guild_id, user_id).await? {
        return Ok(existing);
    }
    app.settings().ensure_guild_settings(guild_id).await?;
    let (balance, imported) = initial_balance(app, user_id, settings).await?;
    exec(app.db(), sql::insert_account(guild_id, user_id, balance, imported)).await?;
    find_account(app, guild_id, user_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Economy account {guild_id}/{user_id} is missing right after creation"))
}

/// `peekAccount(...)`: the stored account or the balance [`get_account`] would create. Never creates rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountPeek {
    pub balance: i64,
    pub daily_streak: i64,
    #[serde(with = "bot_db::serde_js::date_opt")]
    pub last_daily: Option<DateTime<Utc>>,
    #[serde(with = "bot_db::serde_js::date_opt")]
    pub last_work: Option<DateTime<Utc>>,
    #[serde(with = "bot_db::serde_js::date_opt")]
    pub last_pity: Option<DateTime<Utc>>,
    pub exists: bool,
    pub imported_global: bool,
}

impl From<&EconomyAccount> for AccountPeek {
    fn from(row: &EconomyAccount) -> Self {
        Self {
            balance: i64::from(row.balance),
            daily_streak: i64::from(row.daily_streak),
            last_daily: row.last_daily,
            last_work: row.last_work,
            last_pity: row.last_pity,
            exists: true,
            imported_global: row.imported_global,
        }
    }
}

/// `peekAccount(guildId, userId, settings)`: read-only preview for viewing ANOTHER member.
pub async fn peek_account(app: &App, guild_id: &str, user_id: &str, settings: Option<&GuildSettings>) -> anyhow::Result<AccountPeek> {
    if let Some(row) = find_account(app, guild_id, user_id).await? {
        return Ok(AccountPeek::from(&row));
    }
    let (balance, imported_global) = initial_balance(app, user_id, settings).await?;
    Ok(AccountPeek { balance, daily_streak: 0, last_daily: None, last_work: None, last_pity: None, exists: false, imported_global })
}

const RANK_SQL: &str = r#"SELECT COUNT(*)::bigint AS "n" FROM "EconomyAccount" WHERE "guildId" = $1 AND "balance" > $2"#;

/// `getRank(guildId, balance)`: 1-based rank by balance (ties share the better rank).
pub async fn get_rank(app: &App, guild_id: &str, balance: i64) -> anyhow::Result<i64> {
    let n: i64 = sqlx::query_scalar(RANK_SQL).bind(guild_id).bind(balance).fetch_one(app.db()).await?;
    Ok(n + 1)
}

/// `{ balance, capped }` of [`credit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Credited {
    pub balance: i64,
    pub capped: bool,
}

/// `credit(guildId, userId, amount, settings)`: adds coins (clamped to the server maximum) in one statement.
/// `INVALID` for an amount that is not an integer >= 0.
pub async fn credit(app: &App, guild_id: &str, user_id: &str, amount: i64, settings: Option<&GuildSettings>) -> anyhow::Result<Credited> {
    if !is_safe_integer(amount) || amount < 0 {
        return fail(EconomyCode::Invalid);
    }
    let cap = economy_config(settings).max_balance;
    let mut r = rows(app.db(), sql::credit(guild_id, user_id, amount, cap)).await?;
    if r.is_empty() {
        get_account(app, guild_id, user_id, settings).await?;
        r = rows(app.db(), sql::credit(guild_id, user_id, amount, cap)).await?;
    }
    let Some(row) = r.first() else { anyhow::bail!("Economy credit found no account for {guild_id}/{user_id}") };
    let balance = int_col(row, "balance")?;
    Ok(Credited { balance, capped: balance >= cap })
}

/// `{ ok, balance }` of [`debit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Debited {
    pub ok: bool,
    pub balance: i64,
}

/// `debit(...)`: removes coins only when the balance covers them. The account is created first when needed.
/// `INVALID` for an amount that is not an integer >= 1.
pub async fn debit(app: &App, guild_id: &str, user_id: &str, amount: i64, settings: Option<&GuildSettings>) -> anyhow::Result<Debited> {
    if !is_safe_integer(amount) || amount < 1 {
        return fail(EconomyCode::Invalid);
    }
    let mut r = rows(app.db(), sql::debit(guild_id, user_id, amount)).await?;
    if let Some(row) = r.first() {
        return Ok(Debited { ok: true, balance: int_col(row, "balance")? });
    }
    let acc = get_account(app, guild_id, user_id, settings).await?;
    if i64::from(acc.balance) >= amount {
        r = rows(app.db(), sql::debit(guild_id, user_id, amount)).await?; // the account was just created
    }
    if let Some(row) = r.first() {
        return Ok(Debited { ok: true, balance: int_col(row, "balance")? });
    }
    let now = account_balance(app, guild_id, user_id).await?;
    Ok(Debited { ok: false, balance: now.unwrap_or(i64::from(acc.balance)) })
}

/// The result of [`transfer`]: `{ ok: true, fromBalance, toBalance }` or `{ ok: false, code, balance }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transfer {
    Done { from_balance: i64, to_balance: i64 },
    Refused { code: EconomyCode, balance: i64 },
}

impl Transfer {
    pub fn to_json(&self) -> Value {
        match self {
            Self::Done { from_balance, to_balance } => json!({ "ok": true, "fromBalance": from_balance, "toBalance": to_balance }),
            Self::Refused { code, balance } => json!({ "ok": false, "code": code.as_str(), "balance": balance }),
        }
    }
}

/// `transfer(guildId, fromId, toId, amount, settings)`: moves coins in one transaction (both rows locked in userId
/// order). A recipient that would exceed the server maximum rolls the debit back. `INVALID` for the same member or an
/// amount outside 1..COIN_MAX.
pub async fn transfer(app: &App, guild_id: &str, from_id: &str, to_id: &str, amount: i64, settings: Option<&GuildSettings>) -> anyhow::Result<Transfer> {
    if from_id == to_id || !(1..=COIN_MAX_I64).contains(&amount) {
        return fail(EconomyCode::Invalid);
    }
    let cap = economy_config(settings).max_balance;
    get_account(app, guild_id, from_id, settings).await?;
    get_account(app, guild_id, to_id, settings).await?;
    let (g, f, to) = (guild_id.to_string(), from_id.to_string(), to_id.to_string());
    let out: anyhow::Result<(i64, i64)> = bot_db::transaction(app.db(), TxOptions::TEN_SECONDS, move |tx| {
        Box::pin(async move {
            rows(&mut *tx, sql::lock_accounts(&g, &f, &to)).await?;
            let out = rows(&mut *tx, sql::debit(&g, &f, amount)).await?;
            let Some(o) = out.first() else { return fail(EconomyCode::Insufficient) };
            let into = rows(&mut *tx, sql::credit_checked(&g, &to, amount, cap)).await?;
            let Some(n) = into.first() else { return fail(EconomyCode::RecipientCap) };
            Ok((int_col(o, "balance")?, int_col(n, "balance")?))
        })
    })
    .await;
    match out {
        Ok((from_balance, to_balance)) => Ok(Transfer::Done { from_balance, to_balance }),
        Err(err) => {
            let Some(e) = economy_error(&err) else { return Err(err) };
            let code = e.code;
            let balance = account_balance(app, guild_id, from_id).await?.unwrap_or(0);
            Ok(Transfer::Refused { code, balance })
        }
    }
}

/// `setBalance(...)` (admin): `INVALID { max }` for an amount outside 0..max balance.
pub async fn set_balance(app: &App, guild_id: &str, user_id: &str, amount: i64, settings: Option<&GuildSettings>) -> anyhow::Result<i64> {
    let cap = economy_config(settings).max_balance;
    if !is_safe_integer(amount) || amount < 0 || amount > cap {
        return fail_with(EconomyCode::Invalid, json!({ "max": cap }));
    }
    get_account(app, guild_id, user_id, settings).await?;
    let q = UpdateBuilder::new(&ECONOMY_ACCOUNT_INFO)
        .set("balance", json!(amount))?
        .filter("guildId", json!(guild_id))?
        .filter("userId", json!(user_id))?
        .build()?;
    let row: EconomyAccount = q.fetch_one(app.db()).await?;
    Ok(i64::from(row.balance))
}

/// `{ balance, removed }` of [`remove_coins`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Removed {
    pub balance: i64,
    pub removed: i64,
}

/// `removeCoins(...)` (admin): removes up to `amount` coins, never below 0.
pub async fn remove_coins(app: &App, guild_id: &str, user_id: &str, amount: i64, settings: Option<&GuildSettings>) -> anyhow::Result<Removed> {
    if !is_safe_integer(amount) || amount < 1 {
        return fail(EconomyCode::Invalid);
    }
    get_account(app, guild_id, user_id, settings).await?;
    let r = rows(app.db(), sql::remove_coins(guild_id, user_id, amount)).await?;
    let Some(row) = r.first() else { anyhow::bail!("Economy removeCoins found no account for {guild_id}/{user_id}") };
    let (balance, before) = (int_col(row, "balance")?, int_col(row, "before")?);
    Ok(Removed { balance, removed: before - balance })
}

fn clamp_balances_stmt(guild_id: &str, cap: i64) -> Stmt {
    Stmt::new(
        format!(r#"UPDATE "EconomyAccount" SET "balance" = $1, "updatedAt" = {SQL_NOW} WHERE "guildId" = $2 AND "balance" > $3"#),
        vec![i(cap), t(guild_id), i(cap)],
    )
}

/// `clampBalancesToCap(guildId, cap)`: lowers every balance above `cap` to `cap`. Returns the accounts changed.
pub async fn clamp_balances_to_cap(app: &App, guild_id: &str, cap: i64) -> anyhow::Result<u64> {
    exec(app.db(), clamp_balances_stmt(guild_id, cap)).await
}

/// One row of [`top_accounts`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TopAccount {
    pub user_id: String,
    pub balance: i64,
}

fn top_accounts_stmt(guild_id: &str, take: i64) -> Stmt {
    Stmt::new(
        r#"SELECT "userId", "balance" FROM "EconomyAccount" WHERE "guildId" = $1 AND "balance" > 0 ORDER BY "balance" DESC, "userId" ASC LIMIT $2"#
            .into(),
        vec![t(guild_id), Arg::Big(take)],
    )
}

/// `topAccounts(guildId, take = RICHEST_LIMIT)`: richest first (balance > 0).
pub async fn top_accounts(app: &App, guild_id: &str, take: i64) -> anyhow::Result<Vec<TopAccount>> {
    rows(app.db(), top_accounts_stmt(guild_id, take))
        .await?
        .iter()
        .map(|r| Ok(TopAccount { user_id: r.try_get("userId")?, balance: int_col(r, "balance")? }))
        .collect()
}

// ── Casino lock and escrow ────────────────────────────────────────────────────
// The contract of economy-casino (pkg-economy.md §4.4). One casino game per member per server at a time; the lock lives
// in memory (per app) and is lost on restart, which is harmless because the bet is already debited.

/// The casino locks of one app (`casinoLocks`).
#[derive(Default)]
pub struct CasinoLocks(Mutex<HashSet<String>>);

fn casino_set(app: &App) -> std::sync::Arc<CasinoLocks> {
    app.ext::<CasinoLocks>()
}

fn lock_key(guild_id: &str, user_id: &str) -> String {
    format!("{guild_id}:{user_id}")
}

/// `tryLockCasino(guildId, userId)`: synchronous; `false` when the member already has a game running.
pub fn try_lock_casino(app: &App, guild_id: &str, user_id: &str) -> bool {
    let locks = casino_set(app);
    let mut set = locks.0.lock().unwrap_or_else(|e| e.into_inner());
    set.insert(lock_key(guild_id, user_id))
}

/// `unlockCasino(guildId, userId)`: synchronous and idempotent.
pub fn unlock_casino(app: &App, guild_id: &str, user_id: &str) {
    let locks = casino_set(app);
    locks.0.lock().unwrap_or_else(|e| e.into_inner()).remove(&lock_key(guild_id, user_id));
}

/// `isCasinoLocked(guildId, userId)`.
pub fn is_casino_locked(app: &App, guild_id: &str, user_id: &str) -> bool {
    let locks = casino_set(app);
    locks.0.lock().unwrap_or_else(|e| e.into_inner()).contains(&lock_key(guild_id, user_id))
}

/// The result of [`reserve_bet`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reserved {
    /// `balance` is after the debit, `before` is balance + bet.
    Ok { balance: i64, before: i64 },
    Invalid,
    MaxBet { max_bet: i64 },
    Insufficient { balance: i64 },
}

impl Reserved {
    pub fn to_json(&self) -> Value {
        match self {
            Self::Ok { balance, before } => json!({ "ok": true, "balance": balance, "before": before }),
            Self::Invalid => json!({ "ok": false, "code": "INVALID" }),
            Self::MaxBet { max_bet } => json!({ "ok": false, "code": "MAX_BET", "maxBet": max_bet }),
            Self::Insufficient { balance } => json!({ "ok": false, "code": "INSUFFICIENT", "balance": balance }),
        }
    }
}

/// `reserveBet(guildId, userId, bet, settings)`: escrows a bet (debits it atomically BEFORE any card is dealt or wheel
/// spun). Creates the account first when needed. Errors only on a database failure (nothing was debited).
pub async fn reserve_bet(app: &App, guild_id: &str, user_id: &str, bet: i64, settings: Option<&GuildSettings>) -> anyhow::Result<Reserved> {
    if !(1..=COIN_MAX_I64).contains(&bet) {
        return Ok(Reserved::Invalid);
    }
    let cfg = economy_config(settings);
    if cfg.max_bet > 0 && bet > cfg.max_bet {
        return Ok(Reserved::MaxBet { max_bet: cfg.max_bet });
    }
    let r = debit(app, guild_id, user_id, bet, settings).await?;
    if !r.ok {
        return Ok(Reserved::Insufficient { balance: r.balance });
    }
    Ok(Reserved::Ok { balance: r.balance, before: r.balance + bet })
}

/// `{ balance, capped, payoutCapped }` of [`settle_bet`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Settled {
    pub balance: i64,
    pub capped: bool,
    pub payout_capped: bool,
}

/// `settleBet(guildId, userId, payout, settings, { stake })`: credits the GROSS `payout` (stake included; 0 for a loss,
/// the stake for a push). The net winnings are limited to the max payout first and the balance to the server maximum.
/// A bet settles AT MOST ONCE: the credit is retried (once, after 250 ms) only when [`failed_before_send`] proves the
/// statement never reached the database; any other failure is logged and returned, never repeated.
/// `stake` is the total amount escrowed for this game (2x the bet after a double-down). `INVALID` for a negative stake
/// or payout.
pub async fn settle_bet(
    app: &App,
    guild_id: &str,
    user_id: &str,
    payout: i64,
    settings: Option<&GuildSettings>,
    stake: i64,
) -> anyhow::Result<Settled> {
    if !is_safe_integer(stake) || stake < 0 || !is_safe_integer(payout) || payout < 0 {
        return fail(EconomyCode::Invalid);
    }
    let ClampedPayout { payout: fin, capped: payout_capped } = clamp_payout(payout, stake, economy_config(settings).max_payout);
    if fin == 0 {
        let balance = account_balance(app, guild_id, user_id).await?.unwrap_or(0);
        return Ok(Settled { balance, capped: false, payout_capped });
    }
    let log_ctx = || json!({ "guildId": guild_id, "userId": user_id, "payout": fin });
    let result = match credit(app, guild_id, user_id, fin, settings).await {
        Ok(r) => r,
        Err(first) => {
            if !failed_before_send(&first) {
                bot_util::log_error!(
                    "Economy: settlement outcome unknown — check the balance, then pay manually if the payout is missing",
                    log_ctx(),
                    bot_core::error::log_arg(&first)
                );
                return Err(first);
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            match credit(app, guild_id, user_id, fin, settings).await {
                Ok(r) => r,
                Err(err) => {
                    bot_util::log_error!(
                        "Economy: settlement failed — outcome may be unknown, check the balance, then pay manually if the payout is missing",
                        log_ctx(),
                        bot_core::error::log_arg(&err)
                    );
                    return Err(err);
                }
            }
        }
    };
    Ok(Settled { balance: result.balance, capped: result.capped, payout_capped })
}

/// `failedBeforeSend(err)`: true only for database errors that PROVE a statement never reached the server, so
/// repeating it cannot apply it twice. The Prisma codes P1001 / P2024 / an initialisation error map to sqlx's "no
/// pooled connection could be acquired" (`PoolTimedOut`) and a closed pool (`PoolClosed`); a timeout of a running
/// statement, a dropped connection or any other error may have happened after the commit.
pub fn failed_before_send(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        let sx = cause.downcast_ref::<sqlx::Error>().or_else(|| match cause.downcast_ref::<DbError>() {
            Some(DbError::Sqlx(e)) => Some(e),
            _ => None,
        });
        matches!(sx, Some(sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed))
    })
}

// ── Claims (atomic) ───────────────────────────────────────────────────────────

/// Zero rows inside a boosted claim: roll the boost use back.
#[derive(Debug)]
struct ClaimRollback;

impl std::fmt::Display for ClaimRollback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cooldown")
    }
}

impl std::error::Error for ClaimRollback {}

async fn load_enabled_boosts(app: &App, guild_id: &str) -> anyhow::Result<Vec<EconomyBoost>> {
    let q = SelectBuilder::new(&ECONOMY_BOOST_INFO).filter("guildId", json!(guild_id))?.filter("enabled", json!(true))?.build()?;
    Ok(q.fetch_all::<EconomyBoost, _>(app.db()).await?)
}

/// `withBoost(boosts, userId, tz, earn)`: runs the claim built by `earn(appliedBoost)` with the first qualifying boost
/// the member has not used in this period, inside one transaction. Zero rows (cooldown) roll the boost use back.
async fn with_boost<F>(app: &App, boosts: Vec<EconomyBoost>, user_id: &str, tz: &str, earn: F) -> anyhow::Result<(Vec<PgRow>, Option<EconomyBoost>)>
where
    F: FnOnce(Option<&EconomyBoost>) -> Stmt + Send + 'static,
{
    if boosts.is_empty() {
        return Ok((rows(app.db(), earn(None)).await?, None));
    }
    let now = app.clock().now();
    let (user, tz) = (user_id.to_string(), tz.to_string());
    let out = bot_db::transaction(app.db(), TxOptions::TEN_SECONDS, move |tx| {
        Box::pin(async move {
            let mut applied = None;
            for boost in boosts {
                let key = boost_period_key(boost.period, &tz, now);
                let used = rows(&mut *tx, sql::insert_boost_use(&boost.id, &user, &key)).await?;
                if !used.is_empty() {
                    applied = Some(boost);
                    break;
                }
            }
            let r = rows(&mut *tx, earn(applied.as_ref())).await?;
            if r.is_empty() {
                return Err(anyhow::Error::new(ClaimRollback));
            }
            Ok::<_, anyhow::Error>((r, applied))
        })
    })
    .await;
    match out {
        Err(err) if err.downcast_ref::<ClaimRollback>().is_some() => Ok((Vec::new(), None)),
        other => other,
    }
}

/// `{ name, percent }` of the boost a claim used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BoostInfo {
    pub name: String,
    pub percent: i64,
}

fn boost_info(b: &EconomyBoost) -> BoostInfo {
    BoostInfo { name: b.name.clone(), percent: i64::from(b.percent) }
}

/// The result of [`claim_daily`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DailyClaim {
    Claimed { amount: i64, base: i64, streak: i64, balance: i64, capped: bool, next_at: DateTime<Utc>, boost: Option<BoostInfo> },
    NotYet { next_at: DateTime<Utc>, streak: i64 },
}

fn random_between(min: i64, max_inclusive: i64) -> i64 {
    if max_inclusive <= min { min } else { rand::random_range(min..=max_inclusive) }
}

/// `claimDaily(guildId, userId, settings, { memberRoleIds })`: one atomic claim with the streak bonus (x1, 1.1, 1.2,
/// 1.4, 1.8, 2.5, then 4 from day 7; the streak resets after 48 h) and at most one coin boost.
pub async fn claim_daily(
    app: &App,
    guild_id: &str,
    user_id: &str,
    settings: Option<&GuildSettings>,
    member_role_ids: &[String],
) -> anyhow::Result<DailyClaim> {
    let cfg = economy_config(settings);
    let cap = cfg.max_balance;
    get_account(app, guild_id, user_id, settings).await?;
    let boosts = order_boosts(&load_enabled_boosts(app, guild_id).await?, member_role_ids);
    let base = random_between(cfg.daily_min, cfg.daily_max);
    let tz = settings.map(|s| s.timezone.clone()).unwrap_or_else(|| "UTC".into());
    let (g, u) = (guild_id.to_string(), user_id.to_string());
    let (r, boost) = with_boost(app, boosts, user_id, &tz, move |applied| {
        sql::claim_daily(&g, &u, base, cap, applied.map(|b| i64::from(b.percent)).unwrap_or(0))
    })
    .await?;
    if let Some(row) = r.first() {
        let streak = int_col(row, "dailyStreak")?;
        let balance = int_col(row, "balance")?;
        let last = date_col(row, "lastDaily")?.unwrap_or_else(|| app.clock().now());
        let raw = daily_amount(base, streak);
        return Ok(DailyClaim::Claimed {
            amount: boost.as_ref().map(|b| boosted_amount(raw, i64::from(b.percent))).unwrap_or(raw),
            base,
            streak,
            balance,
            capped: balance >= cap,
            next_at: last + chrono::Duration::milliseconds(DAILY_COOLDOWN_MS),
            boost: boost.as_ref().map(boost_info),
        });
    }
    let cur = find_account(app, guild_id, user_id).await?;
    let last = cur.as_ref().and_then(|c| c.last_daily);
    let now = app.clock().now();
    Ok(DailyClaim::NotYet {
        next_at: last.unwrap_or(now) + chrono::Duration::milliseconds(DAILY_COOLDOWN_MS),
        streak: active_streak(last, cur.map(|c| i64::from(c.daily_streak)).unwrap_or(0), now),
    })
}

/// The result of [`claim_work`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkClaim {
    Claimed { amount: i64, balance: i64, capped: bool, next_at: DateTime<Utc>, line: String, boost: Option<BoostInfo> },
    NotYet { next_at: DateTime<Utc> },
}

/// `claimWork(guildId, userId, settings, { memberRoleIds })`: one atomic claim per `workCooldownMinutes`, with at most
/// one coin boost.
pub async fn claim_work(
    app: &App,
    guild_id: &str,
    user_id: &str,
    settings: Option<&GuildSettings>,
    member_role_ids: &[String],
) -> anyhow::Result<WorkClaim> {
    let cfg = economy_config(settings);
    let cap = cfg.max_balance;
    get_account(app, guild_id, user_id, settings).await?;
    let boosts = order_boosts(&load_enabled_boosts(app, guild_id).await?, member_role_ids);
    let base = random_between(cfg.work_min, cfg.work_max);
    let line = WORK_LINES[rand::random_range(0..WORK_LINES.len())].to_string();
    let tz = settings.map(|s| s.timezone.clone()).unwrap_or_else(|| "UTC".into());
    let cooldown = cfg.work_cooldown_minutes;
    let (g, u) = (guild_id.to_string(), user_id.to_string());
    let (r, boost) = with_boost(app, boosts, user_id, &tz, move |applied| {
        let amount = applied.map(|b| boosted_amount(base, i64::from(b.percent))).unwrap_or(base);
        sql::claim_work(&g, &u, amount, cap, cooldown)
    })
    .await?;
    let cooldown_ms = chrono::Duration::milliseconds(cooldown * 60_000);
    if let Some(row) = r.first() {
        let balance = int_col(row, "balance")?;
        let last = date_col(row, "lastWork")?.unwrap_or_else(|| app.clock().now());
        return Ok(WorkClaim::Claimed {
            amount: boost.as_ref().map(|b| boosted_amount(base, i64::from(b.percent))).unwrap_or(base),
            balance,
            capped: balance >= cap,
            next_at: last + cooldown_ms,
            line,
            boost: boost.as_ref().map(boost_info),
        });
    }
    let last = find_account(app, guild_id, user_id).await?.and_then(|c| c.last_work);
    Ok(WorkClaim::NotYet { next_at: last.unwrap_or_else(|| app.clock().now()) + cooldown_ms })
}

/// The result of [`claim_pity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PityClaim {
    Claimed { amount: i64, balance: i64 },
    TooRich { balance: i64 },
    NotYet { next_at: DateTime<Utc> },
}

impl PityClaim {
    pub fn to_json(&self) -> Value {
        match self {
            Self::Claimed { amount, balance } => json!({ "claimed": true, "amount": amount, "balance": balance }),
            Self::TooRich { balance } => json!({ "claimed": false, "tooRich": true, "balance": balance }),
            Self::NotYet { next_at } => json!({ "claimed": false, "nextAt": bot_db::format_js_date(next_at) }),
        }
    }
}

/// `claimPity(guildId, userId, settings)`: [`PITY_AMOUNT`] coins when the balance is below [`PITY_THRESHOLD`], once
/// per 12 h. Never boosted.
pub async fn claim_pity(app: &App, guild_id: &str, user_id: &str, settings: Option<&GuildSettings>) -> anyhow::Result<PityClaim> {
    let cap = economy_config(settings).max_balance;
    get_account(app, guild_id, user_id, settings).await?;
    let r = rows(app.db(), sql::claim_pity(guild_id, user_id, cap)).await?;
    if let Some(row) = r.first() {
        return Ok(PityClaim::Claimed { amount: PITY_AMOUNT, balance: int_col(row, "balance")? });
    }
    let cur = find_account(app, guild_id, user_id).await?;
    match cur {
        Some(c) if i64::from(c.balance) < PITY_THRESHOLD => {
            let last = c.last_pity.unwrap_or_else(|| app.clock().now());
            Ok(PityClaim::NotYet { next_at: last + chrono::Duration::milliseconds(PITY_COOLDOWN_MS) })
        }
        c => Ok(PityClaim::TooRich { balance: c.map(|c| i64::from(c.balance)).unwrap_or(0) }),
    }
}

// ── Shop and inventory ────────────────────────────────────────────────────────

const ITEM_COLUMNS: &str = r#""id","guildId","name","description","price","type","roleId","imageUrl","useOnBuy","destroyOnUse","useMessage","buyMessage","canBuyMultiple","maxPerUser","stock","enabled","createdAt","updatedAt""#;

/// `resolveItem(guildId, value, { enabledOnly })`: a shop item by id or (case-insensitive, Prisma's `ILIKE`) name;
/// always scoped to the guild.
pub async fn resolve_item(app: &App, guild_id: &str, value: &str, enabled_only: bool) -> anyhow::Result<Option<ShopItem>> {
    let text = bot_util::js::trim(value);
    if text.is_empty() {
        return Ok(None);
    }
    let enabled = if enabled_only { r#" AND "enabled" = true"# } else { "" };
    let q = format!(r#"SELECT {ITEM_COLUMNS} FROM "ShopItem" WHERE "guildId" = $1{enabled} AND ("id" = $2 OR "name" ILIKE $3) LIMIT 1"#);
    Ok(sqlx::query_as::<_, ShopItem>(sqlx::AssertSqlSafe(q)).bind(guild_id).bind(text).bind(text).fetch_optional(app.db()).await?)
}

/// One row of [`search_items`] (`{ id, name, price, enabled }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ItemChoice {
    pub id: String,
    pub name: String,
    pub price: i64,
    pub enabled: bool,
}

fn search_items_stmt(guild_id: &str, query: &str, enabled_only: bool, take: i64) -> Stmt {
    let mut sql = r#"SELECT "id", "name", "price", "enabled" FROM "ShopItem" WHERE "guildId" = $1"#.to_string();
    let mut args = vec![t(guild_id)];
    if enabled_only {
        sql.push_str(r#" AND "enabled" = true"#);
    }
    if !query.is_empty() {
        sql.push_str(r#" AND "name" ILIKE $2"#);
        args.push(t(&format!("%{query}%")));
    }
    sql.push_str(&format!(r#" ORDER BY "price" ASC, "name" ASC LIMIT ${}"#, args.len() + 1));
    args.push(Arg::Big(take));
    Stmt::new(sql, args)
}

/// `searchItems(guildId, query, { enabledOnly, take = 25 })`: autocomplete lookup.
pub async fn search_items(app: &App, guild_id: &str, query: &str, enabled_only: bool, take: i64) -> anyhow::Result<Vec<ItemChoice>> {
    rows(app.db(), search_items_stmt(guild_id, query, enabled_only, take))
        .await?
        .iter()
        .map(|r| Ok(ItemChoice { id: r.try_get("id")?, name: r.try_get("name")?, price: int_col(r, "price")?, enabled: r.try_get("enabled")? }))
        .collect()
}

fn list_items_stmt(guild_id: &str, enabled_only: bool, by_name: bool) -> Stmt {
    let enabled = if enabled_only { r#" AND "enabled" = true"# } else { "" };
    let order = if by_name { r#""name" ASC"# } else { r#""price" ASC, "name" ASC"# };
    Stmt::new(format!(r#"SELECT {ITEM_COLUMNS} FROM "ShopItem" WHERE "guildId" = $1{enabled} ORDER BY {order}"#), vec![t(guild_id)])
}

/// `listItems(guildId, { enabledOnly, sort })`: every matching item (`sort` 'name', else by price then name).
pub async fn list_items(app: &App, guild_id: &str, enabled_only: bool, sort_by_name: bool) -> anyhow::Result<Vec<ShopItem>> {
    let stmt = list_items_stmt(guild_id, enabled_only, sort_by_name);
    rows(app.db(), stmt).await?.iter().map(|r| Ok(sqlx::FromRow::from_row(r)?)).collect()
}

/// `countItems(guildId)`.
pub async fn count_items(app: &App, guild_id: &str) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar::<_, i64>(r#"SELECT COUNT(*)::bigint FROM "ShopItem" WHERE "guildId" = $1"#).bind(guild_id).fetch_one(app.db()).await?)
}

/// `findItemByNameInsensitive(guildId, name, { excludeId })`: the uniqueness check for create and rename.
pub async fn find_item_by_name_insensitive(app: &App, guild_id: &str, name: &str, exclude_id: Option<&str>) -> anyhow::Result<Option<ShopItem>> {
    let q = match exclude_id {
        Some(_) => format!(r#"SELECT {ITEM_COLUMNS} FROM "ShopItem" WHERE "guildId" = $1 AND "name" ILIKE $2 AND "id" <> $3 LIMIT 1"#),
        None => format!(r#"SELECT {ITEM_COLUMNS} FROM "ShopItem" WHERE "guildId" = $1 AND "name" ILIKE $2 LIMIT 1"#),
    };
    let mut query = sqlx::query_as::<_, ShopItem>(sqlx::AssertSqlSafe(q)).bind(guild_id).bind(name);
    if let Some(x) = exclude_id {
        query = query.bind(x);
    }
    Ok(query.fetch_optional(app.db()).await?)
}

/// `getOwnedQuantity(guildId, userId, itemId)`: copies of an item the member owns here.
pub async fn get_owned_quantity(app: &App, guild_id: &str, user_id: &str, item_id: &str) -> anyhow::Result<i64> {
    let q = SelectBuilder::new(&INVENTORY_ITEM_INFO).filter("itemId", json!(item_id))?.filter("userId", json!(user_id))?.build()?;
    let row: Option<InventoryItem> = q.fetch_optional(app.db()).await?;
    Ok(row.filter(|r| r.guild_id == guild_id).map(|r| i64::from(r.quantity)).unwrap_or(0))
}

/// The `item` of an inventory row (`{ id, name, type, price, enabled }`; [`search_inventory`] fills `id` and `name`
/// only and leaves the rest out).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InventoryItemRef {
    pub id: String,
    pub name: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub r#type: Option<ShopItemType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// An inventory row with its item (`include: { item: { select } }`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InventoryEntry {
    #[serde(flatten)]
    pub row: InventoryItem,
    pub item: InventoryItemRef,
}

fn inventory_sql(usable_only: bool, with_query: bool, short_item: bool) -> String {
    let mut sql = String::from(
        r#"SELECT i."id", i."guildId", i."userId", i."itemId", i."quantity", i."createdAt", i."updatedAt", s."name" AS "itemName", s."type" AS "itemType", s."price" AS "itemPrice", s."enabled" AS "itemEnabled"
FROM "InventoryItem" i JOIN "ShopItem" s ON s."id" = i."itemId"
WHERE i."guildId" = $1 AND i."userId" = $2 AND i."quantity" > 0"#,
    );
    if usable_only {
        sql.push_str(r#" AND s."type" IN ('USABLE', 'ROLE')"#);
    }
    if with_query {
        sql.push_str(r#" AND s."name" ILIKE $3"#);
    }
    sql.push_str(r#" ORDER BY s."name" ASC"#);
    if short_item {
        sql.push_str(" LIMIT 25");
    }
    sql
}

fn inventory_entry(r: &PgRow, full: bool) -> anyhow::Result<InventoryEntry> {
    let row: InventoryItem = sqlx::FromRow::from_row(r)?;
    let item = InventoryItemRef {
        id: row.item_id.clone(),
        name: r.try_get("itemName")?,
        r#type: if full { Some(r.try_get("itemType")?) } else { None },
        price: if full { Some(int_col(r, "itemPrice")?) } else { None },
        enabled: if full { Some(r.try_get("itemEnabled")?) } else { None },
    };
    Ok(InventoryEntry { row, item })
}

/// `getInventory(guildId, userId)`: the member's rows with `item: { id, name, type, price, enabled }`, by item name.
pub async fn get_inventory(app: &App, guild_id: &str, user_id: &str) -> anyhow::Result<Vec<InventoryEntry>> {
    let stmt = Stmt::new(inventory_sql(false, false, false), vec![t(guild_id), t(user_id)]);
    rows(app.db(), stmt).await?.iter().map(|r| inventory_entry(r, true)).collect()
}

/// `searchInventory(guildId, userId, query, { usableOnly = true })`: autocomplete lookup of the member's own items
/// (at most 25), with `item: { id, name }`.
pub async fn search_inventory(app: &App, guild_id: &str, user_id: &str, query: &str, usable_only: bool) -> anyhow::Result<Vec<InventoryEntry>> {
    let mut args = vec![t(guild_id), t(user_id)];
    if !query.is_empty() {
        args.push(t(&format!("%{query}%")));
    }
    let stmt = Stmt::new(inventory_sql(usable_only, !query.is_empty(), true), args);
    rows(app.db(), stmt).await?.iter().map(|r| inventory_entry(r, false)).collect()
}

/// The result of [`purchase_item`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Purchase {
    /// The locked `ShopItem` row (current price, flags and post-purchase stock).
    pub item: ShopItem,
    pub quantity: i64,
    pub total: i64,
    pub balance: i64,
    /// `None` for items that skip the inventory.
    pub owned: Option<i64>,
    pub to_inventory: bool,
}

/// `purchaseItem({ guildId, userId, itemId, quantity, settings })`: buys an item in ONE transaction (reserve stock,
/// check the ownership limit, debit the price, add to the inventory). Any [`EconomyError`] rolls everything back:
/// `OUT_OF_STOCK` (also disabled or deleted), `MAX_OWNED { max }`, `INSUFFICIENT { total }`, `ITEM_GONE`, `INVALID`.
pub async fn purchase_item(
    app: &App,
    guild_id: &str,
    user_id: &str,
    item_id: &str,
    quantity: i64,
    settings: Option<&GuildSettings>,
) -> anyhow::Result<Purchase> {
    if !(1..=COPIES_MAX).contains(&quantity) {
        return fail(EconomyCode::Invalid);
    }
    app.settings().ensure_guild_settings(guild_id).await?;
    get_account(app, guild_id, user_id, settings).await?;
    let (g, u, it) = (guild_id.to_string(), user_id.to_string(), item_id.to_string());
    let out = bot_db::transaction(app.db(), TxOptions::TEN_SECONDS, move |tx| {
        Box::pin(async move {
            let reserved = rows(&mut *tx, sql::reserve_stock(&g, &it, quantity)).await?;
            let Some(first) = reserved.first() else { return fail(EconomyCode::OutOfStock) };
            let item: ShopItem = sqlx::FromRow::from_row(first)?;
            let to_inventory = goes_to_inventory(&item);
            let max = effective_max_owned(&item);
            if (!item.can_buy_multiple && quantity > 1) || (to_inventory && quantity > max) {
                return fail_with(EconomyCode::MaxOwned, json!({ "max": max }));
            }
            let total = i64::from(item.price) * quantity;
            let paid = rows(&mut *tx, sql::debit(&g, &u, total)).await?;
            let Some(p) = paid.first() else { return fail_with(EconomyCode::Insufficient, json!({ "total": total })) };
            let balance = int_col(p, "balance")?;
            let mut owned = None;
            if to_inventory {
                let inv = rows(&mut *tx, sql::add_inventory_capped(&g, &u, &item.id, quantity, max)).await?;
                let Some(row) = inv.first() else { return fail_with(EconomyCode::MaxOwned, json!({ "max": max })) };
                owned = Some(int_col(row, "quantity")?);
            }
            Ok(Purchase { item, quantity, total, balance, owned, to_inventory })
        })
    })
    .await;
    out.map_err(|err| if is_foreign_key_error(&err) { EconomyError::new(EconomyCode::ItemGone).into() } else { err })
}

/// The arguments of [`refund_purchase`].
#[derive(Debug, Clone)]
pub struct Refund<'a> {
    pub guild_id: &'a str,
    pub user_id: &'a str,
    pub item: &'a ShopItem,
    pub quantity: i64,
    pub total: i64,
    pub to_inventory: bool,
    pub settings: Option<&'a GuildSettings>,
}

/// `refundPurchase(...)`: compensation when the post-commit role grant fails. Restores stock, refunds the FULL price
/// (clamped to COIN_MAX only, never to the server's maximum balance) and removes the copies. Logs and returns errors.
pub async fn refund_purchase(app: &App, r: Refund<'_>) -> anyhow::Result<()> {
    let run = async {
        get_account(app, r.guild_id, r.user_id, r.settings).await?;
        let (g, u, item_id, quantity, total, to_inventory) =
            (r.guild_id.to_string(), r.user_id.to_string(), r.item.id.clone(), r.quantity, r.total, r.to_inventory);
        bot_db::transaction(app.db(), TxOptions::TEN_SECONDS, move |tx| {
            Box::pin(async move {
                exec(&mut *tx, sql::restore_stock(&g, &item_id, quantity)).await?;
                rows(&mut *tx, sql::credit(&g, &u, total, COIN_MAX_I64)).await?;
                if to_inventory {
                    exec(&mut *tx, sql::reduce_inventory(&g, &u, &item_id, quantity)).await?;
                    exec(&mut *tx, sql::delete_empty_inventory(&g, &u, &item_id)).await?;
                }
                Ok::<_, anyhow::Error>(())
            })
        })
        .await
    };
    if let Err(err) = run.await {
        bot_util::log_error!(
            "Economy: purchase refund failed — refund manually",
            json!({ "guildId": r.guild_id, "userId": r.user_id, "itemId": r.item.id, "quantity": r.quantity, "total": r.total }),
            bot_core::error::log_arg(&err)
        );
        return Err(err);
    }
    Ok(())
}

/// `consumeItem({ guildId, userId, itemId })`: removes one copy (for /use). Returns the copies left.
/// `NOT_OWNED` when the member has none (a concurrent use won the race).
pub async fn consume_item(app: &App, guild_id: &str, user_id: &str, item_id: &str) -> anyhow::Result<i64> {
    let r = rows(app.db(), sql::consume_item(guild_id, user_id, item_id)).await?;
    let Some(row) = r.first() else { return fail(EconomyCode::NotOwned) };
    let remaining = int_col(row, "quantity")?;
    if remaining == 0 {
        exec(app.db(), sql::delete_empty_inventory(guild_id, user_id, item_id)).await?;
    }
    Ok(remaining)
}

/// `restoreItem({ guildId, userId, itemId, quantity = 1 })`: undo of [`consume_item`]. Logs and swallows errors.
pub async fn restore_item(app: &App, guild_id: &str, user_id: &str, item_id: &str, quantity: i64) {
    // No cap check: it only gives back what was taken.
    if let Err(err) = rows(app.db(), sql::add_inventory(guild_id, user_id, item_id, quantity)).await {
        bot_util::log_error!(
            "Economy: could not restore a consumed item",
            json!({ "guildId": guild_id, "userId": user_id, "itemId": item_id, "quantity": quantity }),
            bot_core::error::log_arg(&err)
        );
    }
}

/// `giveItem({ guildId, userId, itemId, quantity })` (admin spawn): adds copies, ignoring the item's own limits but
/// never exceeding `LIMITS.ITEM_COPIES_PER_USER`. Returns the copies owned. `MAX_OWNED { max, owned }`, `ITEM_GONE`.
pub async fn give_item(app: &App, guild_id: &str, user_id: &str, item_id: &str, quantity: i64) -> anyhow::Result<i64> {
    if !(1..=COPIES_MAX).contains(&quantity) {
        return fail(EconomyCode::Invalid);
    }
    app.settings().ensure_guild_settings(guild_id).await?;
    // An item of another guild never gets into this guild's inventories (the foreign key alone would allow it).
    let exists: Option<(String,)> =
        sqlx::query_as(r#"SELECT "id" FROM "ShopItem" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#).bind(item_id).bind(guild_id).fetch_optional(app.db()).await?;
    if exists.is_none() {
        return fail(EconomyCode::ItemGone);
    }
    let r = match rows(app.db(), sql::add_inventory_capped(guild_id, user_id, item_id, quantity, COPIES_MAX)).await {
        Ok(r) => r,
        Err(err) if is_foreign_key_error(&err) => return fail(EconomyCode::ItemGone),
        Err(err) => return Err(err),
    };
    let Some(row) = r.first() else {
        let owned = get_owned_quantity(app, guild_id, user_id, item_id).await?;
        return fail_with(EconomyCode::MaxOwned, json!({ "max": COPIES_MAX, "owned": owned }));
    };
    int_col(row, "quantity")
}

/// `{ removed, remaining }` of [`take_item`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Taken {
    pub removed: i64,
    pub remaining: i64,
}

/// `takeItem({ guildId, userId, itemId, quantity })` (admin): removes up to `quantity` copies (the row is deleted at
/// 0). `NOT_OWNED`.
pub async fn take_item(app: &App, guild_id: &str, user_id: &str, item_id: &str, quantity: i64) -> anyhow::Result<Taken> {
    if !is_safe_integer(quantity) || quantity < 1 {
        return fail(EconomyCode::Invalid);
    }
    let r = rows(app.db(), sql::take_item(guild_id, user_id, item_id, quantity)).await?;
    let Some(row) = r.first() else { return fail(EconomyCode::NotOwned) };
    let (remaining, before) = (int_col(row, "remaining")?, int_col(row, "before")?);
    if before <= 0 {
        return fail(EconomyCode::NotOwned);
    }
    if remaining == 0 {
        exec(app.db(), sql::delete_empty_inventory(guild_id, user_id, item_id)).await?;
    }
    Ok(Taken { removed: before - remaining, remaining })
}

async fn locked_count(conn: &mut sqlx::PgConnection, table: &str, guild_id: &str) -> anyhow::Result<i64> {
    let q = format!(r#"SELECT COUNT(*)::bigint FROM "{table}" WHERE "guildId" = $1"#);
    Ok(sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(q)).bind(guild_id).fetch_one(conn).await?)
}

async fn name_clash(conn: &mut sqlx::PgConnection, table: &str, guild_id: &str, name: &str, exclude_id: Option<&str>) -> anyhow::Result<bool> {
    let extra = if exclude_id.is_some() { r#" AND "id" <> $3"# } else { "" };
    let q = format!(r#"SELECT "id" FROM "{table}" WHERE "guildId" = $1 AND "name" ILIKE $2{extra} LIMIT 1"#);
    let mut query = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(q)).bind(guild_id).bind(name);
    if let Some(x) = exclude_id {
        query = query.bind(x);
    }
    Ok(query.fetch_optional(conn).await?.is_some())
}

/// `createItem(guildId, data)`: creates a shop item. `data` holds Prisma field names (`name`, `price`, `type`, ...).
/// The per-guild cap and the case-insensitive name are checked under a lock on the settings row.
/// `INVALID { reason: 'limit' | 'name-taken' }`.
pub async fn create_item(app: &App, guild_id: &str, data: Map<String, Value>) -> anyhow::Result<ShopItem> {
    app.settings().ensure_guild_settings(guild_id).await?;
    let g = guild_id.to_string();
    let out = bot_db::transaction(app.db(), TxOptions::TEN_SECONDS, move |tx| {
        Box::pin(async move {
            rows(&mut *tx, sql::lock_settings_row(&g)).await?;
            if locked_count(&mut *tx, "ShopItem", &g).await? >= limits::SHOP_ITEMS as i64 {
                return fail_with(EconomyCode::Invalid, json!({ "reason": "limit" }));
            }
            let name = data.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
            if name_clash(&mut *tx, "ShopItem", &g, &name, None).await? {
                return fail_with(EconomyCode::Invalid, json!({ "reason": "name-taken" }));
            }
            let q = InsertBuilder::new(&SHOP_ITEM_INFO).value("guildId", json!(g))?.values(data.iter().map(|(k, v)| (k.as_str(), v.clone())))?.build()?;
            Ok::<ShopItem, anyhow::Error>(q.fetch_one(&mut *tx).await?)
        })
    })
    .await;
    out.map_err(|err| if is_unique_error(&err) { EconomyError::with(EconomyCode::Invalid, json!({ "reason": "name-taken" })).into() } else { err })
}

/// `updateItem(guildId, itemId, data)`: updates a shop item of this guild (`data` with Prisma field names).
/// `ITEM_GONE` (missing or another guild's item), `INVALID { reason: 'name-taken' }`.
pub async fn update_item(app: &App, guild_id: &str, item_id: &str, data: Map<String, Value>) -> anyhow::Result<ShopItem> {
    let (g, it) = (guild_id.to_string(), item_id.to_string());
    let out = bot_db::transaction(app.db(), TxOptions::TEN_SECONDS, move |tx| {
        Box::pin(async move {
            let rename = data.get("name").and_then(Value::as_str).map(str::to_string);
            // A rename is checked case-insensitively under the same lock as createItem.
            if rename.is_some() {
                rows(&mut *tx, sql::lock_settings_row(&g)).await?;
            }
            let current: Option<(String,)> =
                sqlx::query_as(r#"SELECT "id" FROM "ShopItem" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#).bind(&it).bind(&g).fetch_optional(&mut *tx).await?;
            if current.is_none() {
                return fail(EconomyCode::ItemGone);
            }
            if let Some(name) = rename
                && name_clash(&mut *tx, "ShopItem", &g, &name, Some(&it)).await?
            {
                return fail_with(EconomyCode::Invalid, json!({ "reason": "name-taken" }));
            }
            let q = UpdateBuilder::new(&SHOP_ITEM_INFO).set_all(data.iter().map(|(k, v)| (k.as_str(), v.clone())))?.filter("id", json!(it))?.build()?;
            let row: Option<ShopItem> = q.fetch_optional(&mut *tx).await?;
            row.ok_or_else(|| EconomyError::new(EconomyCode::ItemGone).into())
        })
    })
    .await;
    out.map_err(|err| if is_unique_error(&err) { EconomyError::with(EconomyCode::Invalid, json!({ "reason": "name-taken" })).into() } else { err })
}

/// `deleteItem(guildId, itemId)`: deletes an item and (by cascade) every copy members own. Returns the copies removed.
pub async fn delete_item(app: &App, guild_id: &str, item_id: &str) -> anyhow::Result<i64> {
    let sum: Option<i64> = sqlx::query_scalar(r#"SELECT SUM("quantity")::bigint FROM "InventoryItem" WHERE "itemId" = $1 AND "guildId" = $2"#)
        .bind(item_id)
        .bind(guild_id)
        .fetch_one(app.db())
        .await?;
    sqlx::query(r#"DELETE FROM "ShopItem" WHERE "id" = $1 AND "guildId" = $2"#).bind(item_id).bind(guild_id).execute(app.db()).await?;
    Ok(sum.unwrap_or(0))
}

// ── Boosts (managed by /economy-admin boost) ──────────────────────────────────

/// A boost with `_count: { uses }` (`listBoosts`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BoostWithUses {
    #[serde(flatten)]
    pub boost: EconomyBoost,
    #[serde(rename = "_count")]
    pub count: BoostUses,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BoostUses {
    pub uses: i64,
}

const LIST_BOOSTS_SQL: &str = r#"SELECT b."id", b."guildId", b."name", b."roleId", b."percent", b."period", b."enabled", b."createdBy", b."createdAt", b."updatedAt",
  (SELECT COUNT(*)::bigint FROM "EconomyBoostUse" u WHERE u."boostId" = b."id") AS "uses"
FROM "EconomyBoost" b WHERE b."guildId" = $1 ORDER BY b."createdAt" ASC, b."name" ASC"#;

/// `listBoosts(guildId)`: every boost of the guild, oldest first, with its use count.
pub async fn list_boosts(app: &App, guild_id: &str) -> anyhow::Result<Vec<BoostWithUses>> {
    let r = sqlx::query(LIST_BOOSTS_SQL).bind(guild_id).fetch_all(app.db()).await?;
    r.iter().map(|row| Ok(BoostWithUses { boost: sqlx::FromRow::from_row(row)?, count: BoostUses { uses: row.try_get("uses")? } })).collect()
}

/// One row of [`search_boosts`] (`{ id, name, percent, enabled }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BoostChoice {
    pub id: String,
    pub name: String,
    pub percent: i64,
    pub enabled: bool,
}

/// `searchBoosts(guildId, query)`: autocomplete lookup (at most 25).
pub async fn search_boosts(app: &App, guild_id: &str, query: &str) -> anyhow::Result<Vec<BoostChoice>> {
    let mut sql = r#"SELECT "id", "name", "percent", "enabled" FROM "EconomyBoost" WHERE "guildId" = $1"#.to_string();
    let mut args = vec![t(guild_id)];
    if !query.is_empty() {
        sql.push_str(r#" AND "name" ILIKE $2"#);
        args.push(t(&format!("%{query}%")));
    }
    sql.push_str(r#" ORDER BY "name" ASC LIMIT 25"#);
    rows(app.db(), Stmt::new(sql, args))
        .await?
        .iter()
        .map(|r| Ok(BoostChoice { id: r.try_get("id")?, name: r.try_get("name")?, percent: int_col(r, "percent")?, enabled: r.try_get("enabled")? }))
        .collect()
}

/// `resolveBoost(guildId, value)`: a boost by id or case-insensitive name.
pub async fn resolve_boost(app: &App, guild_id: &str, value: &str) -> anyhow::Result<Option<EconomyBoost>> {
    let text = bot_util::js::trim(value);
    if text.is_empty() {
        return Ok(None);
    }
    Ok(sqlx::query_as::<_, EconomyBoost>(r#"SELECT * FROM "EconomyBoost" WHERE "guildId" = $1 AND ("id" = $2 OR "name" ILIKE $3) LIMIT 1"#)
        .bind(guild_id)
        .bind(text)
        .bind(text)
        .fetch_optional(app.db())
        .await?)
}

/// The data of [`create_boost`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewBoost {
    pub name: String,
    pub percent: i64,
    pub period: BoostPeriod,
    pub role_id: Option<String>,
    pub created_by: String,
}

/// `createBoost(guildId, { name, percent, period, roleId, createdBy })`: at most `LIMITS.ECONOMY_BOOSTS` per guild,
/// names unique case-insensitively, under a lock on the settings row. `INVALID { reason: 'limit' | 'name-taken' |
/// 'percent' }`.
pub async fn create_boost(app: &App, guild_id: &str, data: NewBoost) -> anyhow::Result<EconomyBoost> {
    if data.percent < 1 || data.percent > BOOST_PERCENT_MAX {
        return fail_with(EconomyCode::Invalid, json!({ "reason": "percent" }));
    }
    app.settings().ensure_guild_settings(guild_id).await?;
    let g = guild_id.to_string();
    let out = bot_db::transaction(app.db(), TxOptions::TEN_SECONDS, move |tx| {
        Box::pin(async move {
            rows(&mut *tx, sql::lock_settings_row(&g)).await?;
            if locked_count(&mut *tx, "EconomyBoost", &g).await? >= limits::ECONOMY_BOOSTS as i64 {
                return fail_with(EconomyCode::Invalid, json!({ "reason": "limit" }));
            }
            if name_clash(&mut *tx, "EconomyBoost", &g, &data.name, None).await? {
                return fail_with(EconomyCode::Invalid, json!({ "reason": "name-taken" }));
            }
            let q = InsertBuilder::new(&ECONOMY_BOOST_INFO)
                .values([
                    ("guildId", json!(g)),
                    ("name", json!(data.name)),
                    ("percent", json!(data.percent)),
                    ("period", json!(data.period.as_str())),
                    ("roleId", json!(data.role_id)),
                    ("createdBy", json!(data.created_by)),
                ])?
                .build()?;
            Ok::<EconomyBoost, anyhow::Error>(q.fetch_one(&mut *tx).await?)
        })
    })
    .await;
    out.map_err(|err| if is_unique_error(&err) { EconomyError::with(EconomyCode::Invalid, json!({ "reason": "name-taken" })).into() } else { err })
}

/// `deleteBoost(guildId, boostId)`: true when a boost was deleted (its uses cascade).
pub async fn delete_boost(app: &App, guild_id: &str, boost_id: &str) -> anyhow::Result<bool> {
    let r = sqlx::query(r#"DELETE FROM "EconomyBoost" WHERE "id" = $1 AND "guildId" = $2"#).bind(boost_id).bind(guild_id).execute(app.db()).await?;
    Ok(r.rows_affected() > 0)
}

/// `setBoostEnabled(guildId, boostId, enabled)`: the updated boost, or `None` when it no longer exists.
pub async fn set_boost_enabled(app: &App, guild_id: &str, boost_id: &str, enabled: bool) -> anyhow::Result<Option<EconomyBoost>> {
    let q = format!(r#"UPDATE "EconomyBoost" SET "enabled" = $1, "updatedAt" = {SQL_NOW} WHERE "id" = $2 AND "guildId" = $3 RETURNING *"#);
    Ok(sqlx::query_as::<_, EconomyBoost>(sqlx::AssertSqlSafe(q)).bind(enabled).bind(boost_id).bind(guild_id).fetch_optional(app.db()).await?)
}

// ── Reset ─────────────────────────────────────────────────────────────────────

/// What [`reset_economy`] resets (`'coins' | 'items' | 'all'`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetWhat {
    Coins,
    Items,
    All,
}

impl ResetWhat {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "coins" => Some(Self::Coins),
            "items" => Some(Self::Items),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    fn coins(self) -> bool {
        matches!(self, Self::Coins | Self::All)
    }

    fn items(self) -> bool {
        matches!(self, Self::Items | Self::All)
    }
}

/// `{ accounts, inventories }` of [`reset_economy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ResetCounts {
    pub accounts: u64,
    pub inventories: u64,
}

fn reset_accounts_stmt(guild_id: &str, user_id: Option<&str>, starting: i64) -> Stmt {
    let mut sql = format!(
        r#"UPDATE "EconomyAccount" SET "balance" = $1, "dailyStreak" = 0, "lastDaily" = NULL, "lastWork" = NULL, "lastPity" = NULL, "updatedAt" = {SQL_NOW} WHERE "guildId" = $2"#
    );
    let mut args = vec![i(starting), t(guild_id)];
    if let Some(u) = user_id {
        sql.push_str(r#" AND "userId" = $3"#);
        args.push(t(u));
    }
    Stmt::new(sql, args)
}

/// `resetEconomy({ guildId, userId, what, settings })`: coins go back to the starting balance (streaks and cooldowns
/// cleared; rows are never deleted, since a deleted row would re-import the global balance). A reset of everyone's coins
/// also turns the global import off. The shop catalogue is never touched.
pub async fn reset_economy(app: &App, guild_id: &str, user_id: Option<&str>, what: ResetWhat, settings: Option<&GuildSettings>) -> anyhow::Result<ResetCounts> {
    let cfg = economy_config(settings);
    let mut counts = ResetCounts { accounts: 0, inventories: 0 };
    if what.coins() {
        if let Some(u) = user_id {
            get_account(app, guild_id, u, settings).await?; // a single-member reset also stops that member's future import
        }
        counts.accounts = exec(app.db(), reset_accounts_stmt(guild_id, user_id, cfg.starting_balance)).await?;
        if user_id.is_none() {
            app.settings().update_settings(guild_id, bot_core::settings::SettingsPatch::new().set("economyImportGlobal", false)).await?;
        }
    }
    if what.items() {
        let r = match user_id {
            Some(u) => sqlx::query(r#"DELETE FROM "InventoryItem" WHERE "guildId" = $1 AND "userId" = $2"#).bind(guild_id).bind(u).execute(app.db()).await?,
            None => sqlx::query(r#"DELETE FROM "InventoryItem" WHERE "guildId" = $1"#).bind(guild_id).execute(app.db()).await?,
        };
        counts.inventories = r.rows_affected();
    }
    Ok(counts)
}

// ── Rewards provider (registered by the economy plugin) ───────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum RewardChange {
    Grant,
    Take,
}

async fn reward_change(app: &App, kind: RewardChange, args: &RewardArgs) -> RewardResult {
    let guild_id = args.guild_id.to_string();
    let run = async {
        let Some(settings) = app.settings().get_settings_safe(&guild_id).await else { return Ok(RewardResult::fail("unavailable")) };
        if !is_module_enabled(Some(&settings), Some(bot_config::ModuleKey::Economy)) {
            return Ok(RewardResult::fail("module-disabled"));
        }
        let amount = args.amount;
        if !(1..=COIN_MAX_I64).contains(&amount) {
            return Ok(RewardResult::fail("invalid-amount"));
        }
        let result = if kind == RewardChange::Grant {
            let c = credit(app, &guild_id, &args.user_id, amount, Some(&settings)).await?;
            RewardResult::ok(Some(c.balance))
        } else {
            let r = debit(app, &guild_id, &args.user_id, amount, Some(&settings)).await?;
            if r.ok { RewardResult::ok(Some(r.balance)) } else { RewardResult { ok: false, total: Some(r.balance), reason: Some("insufficient".into()) } }
        };
        let k = if kind == RewardChange::Grant { "grant" } else { "take" };
        bot_util::log_debug!(format!("Economy reward {k} {amount} → {} in {guild_id} ({})", args.user_id, args.reason));
        Ok::<_, anyhow::Error>(result)
    };
    match run.await {
        Ok(r) => r,
        Err(err) => {
            bot_util::log_error!("Economy reward failed", bot_core::error::log_arg(&err));
            RewardResult::fail("error")
        }
    }
}

/// `grantCoinsReward(args)`: the `coins` provider's grant. Coin boosts never apply. Never fails.
pub async fn grant_coins_reward(app: &App, args: &RewardArgs) -> RewardResult {
    reward_change(app, RewardChange::Grant, args).await
}

/// `takeCoinsReward(args)`: never drives a balance below 0 (`reason: 'insufficient'`, nothing changed). Never fails.
pub async fn take_coins_reward(app: &App, args: &RewardArgs) -> RewardResult {
    reward_change(app, RewardChange::Take, args).await
}

/// The `coins` rewards provider (`provides.coins`).
pub struct CoinsProvider;

#[async_trait::async_trait]
impl RewardProvider for CoinsProvider {
    async fn grant(&self, app: &App, args: RewardArgs) -> bot_core::error::BotResult<RewardResult> {
        Ok(grant_coins_reward(app, &args).await)
    }

    async fn take(&self, app: &App, args: RewardArgs) -> bot_core::error::BotResult<RewardResult> {
        Ok(take_coins_reward(app, &args).await)
    }
}

// ── UI helper ─────────────────────────────────────────────────────────────────

/// `replyFail(interaction, title, description)`: an error embed shown PRIVATELY whatever the interaction state.
pub async fn reply_fail(r: &Responder, title: &str, description: &str) {
    reply_fail_embed(r, bot_core::embed_builder::error_embed(title, Some(description), None)).await;
}

/// `replyFail(interaction, null, null, { embed })`: any embed shown PRIVATELY whatever the interaction state (the
/// "defer first, then private error" pattern). Never fails.
pub async fn reply_fail_embed(r: &Responder, embed: CreateEmbed) {
    let state = r.state();
    let res = if !state.deferred && !state.replied {
        r.reply(Payload::embed(embed).ephemeral(true).to_reply()).await
    } else if state.ephemeral == Some(false) {
        // Public deferral or reply: remove it and say it privately.
        let _ = r.delete_reply().await;
        r.follow_up(Payload::embed(embed).ephemeral(true).to_follow_up()).await.map(|_| ())
    } else if state.ephemeral == Some(true) {
        r.edit_reply(Payload::embed(embed).components(Vec::new()).to_edit()).await.map(|_| ())
    } else {
        // deferUpdate / update: never overwrite the clicked panel.
        r.follow_up(Payload::embed(embed).ephemeral(true).to_follow_up()).await.map(|_| ())
    };
    if let Err(err) = res {
        if is_interaction_gone(&err) {
            bot_util::log_debug!(format!("replyFail: interaction gone ({})", err.code().map(|c| c.to_string()).unwrap_or_else(|| "undefined".into())));
        } else {
            bot_util::log_warn!("replyFail failed", err.to_string());
        }
    }
}
