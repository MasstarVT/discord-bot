//! Port of `src/services/tagService.js`: custom commands and triggers (pkg-custom-commands.md).
//!
//! One service for both halves of the module:
//! * custom commands (`CustomTag`): names, the action model, arguments, access lists, cooldowns, the shared
//!   executor [`run_tag`] (`/tag use` and the prefix hook) and the prefix hook body;
//! * triggers (`AutoTrigger`): the cache, safe regex compilation, selection and the trigger hook body.
//!
//! Every user-supplied regular expression goes through `bot_util::safe_regex` (validated at write time, compiled
//! once, executed in one batched, time-limited call), and every outgoing text goes through `render_template`, which
//! clamps after expansion. Mentions of users are allowed; roles only through per-action `pingRoleIds` filtered by
//! `pingable_role_ids`; @everyone and @here are never parsed.
//!
//! The executor talks to Discord through [`TagIo`] ([`crate::tag_io::DiscordIo`] in production), so its decisions
//! are tested offline with a fake.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bot_config::tables::limits;
use bot_core::custom_embed::{RenderOptions, Rendered, render_template};
use bot_core::deletion_tracker::{mark_bot_deletion, was_deleted_by_bot};
use bot_core::embed_builder::{error_embed, success_embed, warn_embed};
use bot_core::hooks::HookCtx;
use bot_core::hooks::events::MessageCreate;
use bot_core::interaction_helpers::{ChannelUsable, INTERACTION_GONE_CODES, Payload, RoleCheck};
use bot_core::App;
use bot_db::{AUTO_TRIGGER_INFO, AutoTrigger, CUSTOM_TAG_INFO, CooldownScope, CustomTag, DbError, GuildSettings, InsertBuilder, SelectBuilder, UpdateBuilder};
use bot_util::placeholder_parser::{RenderContext, render_placeholders_detailed};
use bot_util::safe_regex::{SafeRegex, compile_safe_regex, is_quarantined, safe_regex_first_match, validate_regex};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use serenity::all::{Guild, GuildChannel};
use serenity::builder::{CreateAllowedMentions, CreateEmbed};

// ── Constants ─────────────────────────────────────────────────────────────────

/// `NEW_TAG_NAME_MAX`.
pub const NEW_TAG_NAME_MAX: usize = 32;
/// `PLAIN_MAX`: the message content limit (write and render).
pub const PLAIN_MAX: usize = 2000;
/// `EMBED_TEXT_MAX`: the write limit for embed text; the render clamps to 4096.
pub const EMBED_TEXT_MAX: usize = 4000;
/// `DESCRIPTION_MAX`.
pub const DESCRIPTION_MAX: usize = 100;
/// `ARGS_MAX`: arguments kept per invocation.
pub const ARGS_MAX: usize = 100;
/// `COOLDOWN_MAX_SECONDS`: 365 days.
pub const COOLDOWN_MAX_SECONDS: i64 = 31_536_000;
/// `TRIGGER_PATTERN_MAX`.
pub const TRIGGER_PATTERN_MAX: usize = 500;
/// `TRIGGER_COOLDOWN_MAX`.
pub const TRIGGER_COOLDOWN_MAX: i64 = 86_400;
/// `TAG_ACCESS_MAX` (`LIMITS.TAG_ACCESS_ENTRIES`, per list).
pub const TAG_ACCESS_MAX: usize = limits::TAG_ACCESS_ENTRIES;
/// `TRIGGER_SCOPE_MAX` (`LIMITS.TRIGGER_SCOPE_ENTRIES`: channelIds / exemptRoleIds).
pub const TRIGGER_SCOPE_MAX: usize = limits::TRIGGER_SCOPE_ENTRIES;
/// `PING_ROLES_MAX` (`LIMITS.TAG_PING_ROLES`: pingRoleIds per action).
pub const PING_ROLES_MAX: usize = limits::TAG_PING_ROLES;
/// `MEMO_TTL_MS`.
pub const MEMO_TTL_MS: i64 = 60_000;
/// `MEMO_MAX_GUILDS`.
pub const MEMO_MAX_GUILDS: usize = 5_000;
/// `NOTICE_TTL_MS`: prefix usage and cooldown notices are deleted after this.
pub const NOTICE_TTL_MS: u64 = 10_000;
/// `MESSAGE_ACTION_TYPES`.
pub const MESSAGE_ACTION_TYPES: [&str; 2] = ["reply", "send"];
/// `ROLE_ACTION_TYPES`.
pub const ROLE_ACTION_TYPES: [&str; 3] = ["addRole", "removeRole", "toggleRole"];
/// `MATCH_TYPES`.
pub const MATCH_TYPES: [&str; 5] = ["CONTAINS", "EXACT", "STARTS_WITH", "ENDS_WITH", "REGEX"];
/// `MATCH_LABELS`.
pub const MATCH_LABELS: [(&str, &str); 5] =
    [("CONTAINS", "Contains"), ("EXACT", "Exact"), ("STARTS_WITH", "Starts with"), ("ENDS_WITH", "Ends with"), ("REGEX", "Regex")];
const COMPILED_MAX: usize = 5_000;
/// The TTL of `guild:{g}:triggers` (seconds).
pub const TRIGGER_CACHE_TTL: u64 = 300;

/// `MATCH_LABELS[type]`.
pub fn match_label(match_type: &str) -> Option<&'static str> {
    MATCH_LABELS.iter().find(|(k, _)| *k == match_type).map(|(_, v)| *v)
}

/// `TAG_NAME_RE` (`/^[a-z0-9_-]{1,50}$/`): every stored name (legacy rows were up to 50).
pub fn is_tag_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 50 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// `SNOWFLAKE_RE` (`/^\d{17,20}$/`).
pub fn is_snowflake(s: &str) -> bool {
    (17..=20).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

// ── In-process state (per shard) ──────────────────────────────────────────────

struct Memo<V> {
    map: HashMap<String, (V, i64, u64)>,
    seq: u64,
}

impl<V: Clone> Memo<V> {
    fn new() -> Self {
        Self { map: HashMap::new(), seq: 0 }
    }

    fn get(&self, key: &str, now: i64) -> Option<V> {
        self.map.get(key).filter(|(_, at, _)| now - at < MEMO_TTL_MS).map(|(v, _, _)| v.clone())
    }

    /// `memoPut`: insertion order = age; the oldest guild goes first past `MEMO_MAX_GUILDS`.
    fn put(&mut self, key: &str, value: V, now: i64) {
        self.seq += 1;
        self.map.insert(key.to_string(), (value, now, self.seq));
        while self.map.len() > MEMO_MAX_GUILDS {
            let Some(oldest) = self.map.iter().min_by_key(|(_, (_, _, s))| *s).map(|(k, _)| k.clone()) else { break };
            self.map.remove(&oldest);
        }
    }

    fn remove(&mut self, key: &str) {
        self.map.remove(key);
    }
}

/// Compiled regexes by `id:v:caseSensitive` (`None`: unsafe or invalid), in insertion order.
type CompiledMemo = (HashMap<String, Option<Arc<SafeRegex>>>, VecDeque<String>);

/// The module-level state of the JS file, one per app (`app.ext::<TagState>()`).
pub struct TagState {
    tag_index: Mutex<Memo<Arc<HashSet<String>>>>,
    trigger_memo: Mutex<Memo<Arc<Vec<TriggerEntry>>>>,
    compiled: Mutex<CompiledMemo>,
    primed: Mutex<HashMap<String, HashMap<String, CustomTag>>>,
    prefix_throttle: bot_util::async_util::Throttle,
    notice_throttle: bot_util::async_util::Throttle,
    trigger_channel_throttle: bot_util::async_util::Throttle,
    warn_throttle: bot_util::async_util::Throttle,
    format_throttle: bot_util::async_util::Throttle,
    count_uses: AtomicBool,
    /// Uses increments started (test observability).
    pub increments: AtomicU64,
}

impl Default for TagState {
    fn default() -> Self {
        Self {
            tag_index: Mutex::new(Memo::new()),
            trigger_memo: Mutex::new(Memo::new()),
            compiled: Mutex::new((HashMap::new(), VecDeque::new())),
            primed: Mutex::new(HashMap::new()),
            prefix_throttle: bot_util::async_util::create_throttle(1_000),
            notice_throttle: bot_util::async_util::create_throttle(10_000),
            trigger_channel_throttle: bot_util::async_util::create_throttle(1_000),
            warn_throttle: bot_util::async_util::create_throttle(10 * 60_000),
            format_throttle: bot_util::async_util::create_throttle(60 * 60_000),
            count_uses: AtomicBool::new(true),
            increments: AtomicU64::new(0),
        }
    }
}

fn state(app: &App) -> Arc<TagState> {
    app.ext::<TagState>()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// `warnOnce(key, message)`: one warning per key per 10 minutes.
fn warn_once(app: &App, key: &str, message: String) {
    if state(app).warn_throttle.allow(key) {
        bot_util::log_warn!(message);
    }
}

/// `__testing.primeTags(guildId, rows)`: the name index becomes the enabled names of `rows`, and
/// [`find_tag_row`] returns these rows without the database. Offline tests only.
pub fn prime_tags(app: &App, guild_id: &str, rows: Vec<CustomTag>) {
    let s = state(app);
    let names: HashSet<String> = rows.iter().filter(|r| r.enabled).map(|r| r.name.clone()).collect();
    lock(&s.primed).insert(guild_id.to_string(), rows.into_iter().map(|r| (r.name.clone(), r)).collect());
    lock(&s.tag_index).put(guild_id, Arc::new(names), app.clock().now_ms());
}

/// `__testing.primeTriggers(guildId, projections)`: a trigger memo entry without the database or Redis.
pub fn prime_triggers(app: &App, guild_id: &str, projections: Vec<TriggerProjection>) {
    let entries = make_entries(app, projections, guild_id);
    lock(&state(app).trigger_memo).put(guild_id, Arc::new(entries), app.clock().now_ms());
}

/// `__testing.configure({ countUses })`: offline tests switch the fire-and-forget uses counters off.
pub fn configure_count_uses(app: &App, count: bool) {
    state(app).count_uses.store(count, Ordering::SeqCst);
}

// ── Errors ────────────────────────────────────────────────────────────────────

/// `TagError`: a failure with a title and text an admin can read; commands render it as `error_embed(title, message)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagError {
    pub code: &'static str,
    pub title: String,
    pub message: String,
}

impl TagError {
    pub fn new(code: &'static str, title: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code, title: title.into(), message: message.into() }
    }
}

impl std::fmt::Display for TagError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TagError {}

// ── Raw SQL ───────────────────────────────────────────────────────────────────

/// `lockSql(key)`: a per-guild advisory transaction lock.
pub const LOCK_SQL: &str = "SELECT 1 AS ok FROM (SELECT pg_advisory_xact_lock(hashtext($1))) AS l";
/// `tagUsesSql(id)`: `uses` is an int4 column, so the maths run in bigint and are clamped to 2147483647.
pub const TAG_USES_SQL: &str = r#"UPDATE "CustomTag" SET "uses" = LEAST("uses"::bigint + 1, 2147483647)::int WHERE "id" = $1"#;
/// `triggerUsesSql(id)`.
pub const TRIGGER_USES_SQL: &str = r#"UPDATE "AutoTrigger" SET "uses" = LEAST("uses"::bigint + 1, 2147483647)::int WHERE "id" = $1"#;

/// `RAW_SQL`: every raw statement with a sample parameter.
pub const RAW_SQL: [(&str, &str, &str); 4] = [
    ("tag.lockGuildTags", LOCK_SQL, "customtag:0"),
    ("tag.lockGuildTriggers", LOCK_SQL, "autotrigger:0"),
    ("tag.incrementUses", TAG_USES_SQL, "0"),
    ("trigger.incrementUses", TRIGGER_USES_SQL, "0"),
];

// ── Display helpers ───────────────────────────────────────────────────────────

/// `codeSafe(s)`: backticks inside inline code would end it; ˋ (U+02CB) looks the same.
pub fn code_safe(s: &str) -> String {
    s.replace('`', "ˋ")
}

/// `oneLine(s)`: whitespace collapsed to single spaces.
pub fn one_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if bot_util::js::is_js_space(c) {
            space = true;
        } else {
            if space && !out.is_empty() {
                out.push(' ');
            }
            space = false;
            out.push(c);
        }
    }
    out
}

/// Whether `id` is a channel (or thread) of `guild` (`guild.channels.cache.has(id)`).
pub fn guild_has_channel(guild: &Guild, id: &str) -> bool {
    id.parse::<u64>().ok().filter(|v| *v != 0).is_some_and(|v| bot_core::permissions::find_channel(guild, serenity::all::ChannelId::new(v)).is_some())
}

/// Whether `id` is a role of `guild` (`guild.roles.cache.has(id)`).
pub fn guild_has_role(guild: &Guild, id: &str) -> bool {
    id.parse::<u64>().ok().filter(|v| *v != 0).is_some_and(|v| guild.roles.contains_key(&serenity::all::RoleId::new(v)))
}

/// What [`format_id_list`] lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdKind {
    Role,
    Channel,
}

/// `formatIdList(ids, { kind, guild, empty, max })`: mentions of ids for an embed (they render and never ping),
/// `⚠️ deleted (id)` for an id missing from the cache.
pub fn format_id_list(ids: &[String], kind: IdKind, guild: Option<&Guild>, empty: &str, max: usize) -> String {
    if ids.is_empty() {
        return empty.to_string();
    }
    let items: Vec<String> = ids
        .iter()
        .map(|id| {
            let missing = guild.is_some_and(|g| match kind {
                IdKind::Role => !guild_has_role(g, id),
                IdKind::Channel => !guild_has_channel(g, id),
            });
            if missing {
                format!("⚠️ deleted ({id})")
            } else if kind == IdKind::Role {
                format!("<@&{id}>")
            } else {
                format!("<#{id}>")
            }
        })
        .collect();
    bot_util::text::join_with_limit(&items, ", ", max, bot_util::text::default_more)
}

fn reply_count(n: usize) -> String {
    format!("{n} repl{}", if n == 1 { "y" } else { "ies" })
}

/// `describeAction(a, guild)`: one line describing an action, with mentions. A missing role or channel gets
/// ` ⚠️ deleted` when `guild` is given.
pub fn describe_action(a: &TagAction, guild: Option<&Guild>) -> String {
    match a.kind {
        ActionKind::Reply | ActionKind::Send => {
            let ping = if a.ping_role_ids.is_empty() {
                String::new()
            } else {
                format!(" · pings {}", a.ping_role_ids.iter().map(|id| format!("<@&{id}>")).collect::<Vec<_>>().join(" "))
            };
            let suffix = format!("{} · {}{ping}", if a.embed { " · embed" } else { "" }, reply_count(a.responses.len()));
            if a.kind == ActionKind::Send {
                let channel = a.channel_id.clone().unwrap_or_default();
                let gone = if guild.is_some_and(|g| !guild_has_channel(g, &channel)) { " ⚠️ deleted" } else { "" };
                return format!("📤 Send to <#{channel}>{gone}{suffix}");
            }
            format!("{}{suffix}", if a.dm { "📨 Reply by DM" } else { "💬 Reply in channel" })
        }
        _ => {
            let id = a.role_id.clone().unwrap_or_default();
            let gone = if guild.is_some_and(|g| !guild_has_role(g, &id)) { " ⚠️ deleted" } else { "" };
            let role = format!("<@&{id}>{gone}");
            match a.kind {
                ActionKind::AddRole => format!("➕ Give {role}"),
                ActionKind::RemoveRole => format!("➖ Remove {role}"),
                _ => format!("🔁 Toggle {role}"),
            }
        }
    }
}

/// Names of channels and roles for [`action_label`] (`guild.channels.cache.get(id)?.name`).
pub trait NameLookup {
    fn channel_name(&self, id: &str) -> Option<String>;
    fn role_name(&self, id: &str) -> Option<String>;
}

impl NameLookup for Guild {
    fn channel_name(&self, id: &str) -> Option<String> {
        let v = id.parse::<u64>().ok().filter(|v| *v != 0)?;
        bot_core::permissions::find_channel(self, serenity::all::ChannelId::new(v)).map(|c| c.name.clone())
    }
    fn role_name(&self, id: &str) -> Option<String> {
        let v = id.parse::<u64>().ok().filter(|v| *v != 0)?;
        self.roles.get(&serenity::all::RoleId::new(v)).map(|r| r.name.clone())
    }
}

/// No guild: every channel and role shows as deleted.
pub struct NoNames;

impl NameLookup for NoNames {
    fn channel_name(&self, _: &str) -> Option<String> {
        None
    }
    fn role_name(&self, _: &str) -> Option<String> {
        None
    }
}

/// `actionLabel(a, guild)`: the plain-text variant of [`describe_action`] (autocomplete choices cannot hold mentions).
pub fn action_label(a: &TagAction, names: &dyn NameLookup) -> String {
    let channel = |id: &Option<String>| id.as_deref().and_then(|i| names.channel_name(i)).map(|n| format!("#{n}")).unwrap_or_else(|| "(deleted)".into());
    let role = |id: &Option<String>| id.as_deref().and_then(|i| names.role_name(i)).map(|n| format!("@{n}")).unwrap_or_else(|| "(deleted)".into());
    match a.kind {
        ActionKind::Send => format!("Send to {}", channel(&a.channel_id)),
        ActionKind::Reply => (if a.dm { "Reply by DM" } else { "Reply in channel" }).to_string(),
        ActionKind::AddRole => format!("Give {}", role(&a.role_id)),
        ActionKind::RemoveRole => format!("Remove {}", role(&a.role_id)),
        ActionKind::ToggleRole => format!("Toggle {}", role(&a.role_id)),
    }
}

/// `cooldownLabel(tag)`: `None`, `Server-wide · 5 minutes` or `Per user · 5 minutes`.
pub fn cooldown_label(scope: CooldownScope, seconds: i64) -> String {
    if scope == CooldownScope::None || seconds <= 0 {
        return "None".into();
    }
    let who = if scope == CooldownScope::Server { "Server-wide" } else { "Per user" };
    format!("{who} · {}", bot_util::parse_duration::format_duration_long(seconds as f64 * 1000.0))
}

/// The failure codes of a step (`FAILURE_TEXT` keys).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailureCode {
    Empty,
    DmClosed,
    ChannelGone,
    MissingPerms,
    RoleGone,
    RoleUnassignable,
    MemberGone,
    Expired,
    DiscordError,
}

impl FailureCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "EMPTY",
            Self::DmClosed => "DM_CLOSED",
            Self::ChannelGone => "CHANNEL_GONE",
            Self::MissingPerms => "MISSING_PERMS",
            Self::RoleGone => "ROLE_GONE",
            Self::RoleUnassignable => "ROLE_UNASSIGNABLE",
            Self::MemberGone => "MEMBER_GONE",
            Self::Expired => "EXPIRED",
            Self::DiscordError => "DISCORD_ERROR",
        }
    }

    /// `FAILURE_TEXT[code]`.
    pub fn text(self) -> &'static str {
        match self {
            Self::Empty => "The message was empty after variables were filled in.",
            Self::DmClosed => "I couldn't DM you; your DMs may be closed.",
            Self::ChannelGone => "The target channel no longer exists.",
            Self::MissingPerms => "I'm missing permissions there: {detail}.",
            Self::RoleGone => "The role no longer exists.",
            Self::RoleUnassignable => "{detail}",
            Self::MemberGone => "You're no longer in the server.",
            Self::Expired => "The interaction expired before I could reply.",
            Self::DiscordError => "Discord rejected the request.",
        }
    }
}

/// `failureText(code, detail)`.
pub fn failure_text(code: Option<FailureCode>, detail: Option<&str>) -> String {
    let text = code.unwrap_or(FailureCode::DiscordError).text();
    if !text.contains("{detail}") {
        return text.to_string();
    }
    match detail.filter(|d| !d.is_empty()) {
        Some(d) => text.replacen("{detail}", d, 1),
        None => text.replacen(": {detail}", "", 1).replacen("{detail}", "", 1),
    }
}

// ── Names ─────────────────────────────────────────────────────────────────────

fn collapse_spaces_to_dash(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if bot_util::js::is_js_space(c) {
            if !space {
                out.push('-');
            }
            space = true;
        } else {
            out.push(c);
            space = false;
        }
    }
    out
}

/// `normalizeTagName(input)`: the legacy create normalisation plus trim and the 50 cap; every stored name is a fixed
/// point.
pub fn normalize_tag_name(input: &str) -> String {
    let lowered = bot_util::js::lower(bot_util::js::trim(input));
    collapse_spaces_to_dash(&lowered)
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_' || *c == '-')
        .take(50)
        .collect()
}

/// The reason [`validate_new_tag_name`] refuses a name.
pub const INVALID_NAME_REASON: &str = "Names can use only a–z, 0–9, - and _ (up to 32 characters), e.g. `server-rules`.";

/// `validateNewTagName(input)`: creation never strips characters silently: `c++` is refused instead of becoming `c`.
pub fn validate_new_tag_name(input: &str) -> Result<String, &'static str> {
    let base = collapse_spaces_to_dash(&bot_util::js::lower(bot_util::js::trim(input)));
    let ok = (1..=NEW_TAG_NAME_MAX).contains(&base.len())
        && base.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if ok { Ok(base) } else { Err(INVALID_NAME_REASON) }
}

// ── Lookups and the name index ────────────────────────────────────────────────

/// `findTag(guildId, rawName)`: fails on a database error so commands reach the router's error reply.
pub async fn find_tag(app: &App, guild_id: &str, raw_name: &str) -> anyhow::Result<Option<CustomTag>> {
    let name = normalize_tag_name(raw_name);
    if name.is_empty() {
        return Ok(None);
    }
    Ok(SelectBuilder::new(&CUSTOM_TAG_INFO).filter("guildId", json!(guild_id))?.filter("name", json!(name))?.build()?.fetch_optional(app.db()).await?)
}

/// A custom command by id (`prisma.customTag.findUnique({ where: { id } })`).
pub async fn find_tag_by_id(app: &App, id: &str) -> anyhow::Result<Option<CustomTag>> {
    Ok(SelectBuilder::new(&CUSTOM_TAG_INFO).filter("id", json!(id))?.build()?.fetch_optional(app.db()).await?)
}

/// `invalidateTagIndex(guildId)`: drops the memoised name set of a guild; called after every tag write.
pub fn invalidate_tag_index(app: &App, guild_id: &str) {
    lock(&state(app).tag_index).remove(guild_id);
}

/// `hasTagName(guildId, name)`: true when an enabled command has this name. Never fails: a database error gives
/// false (prefix commands go silent).
pub async fn has_tag_name(app: &App, guild_id: &str, name: &str) -> bool {
    let s = state(app);
    let now = app.clock().now_ms();
    if let Some(names) = lock(&s.tag_index).get(guild_id, now) {
        return names.contains(name);
    }
    let rows: Result<Vec<(String,)>, sqlx::Error> =
        sqlx::query_as(r#"SELECT "name" FROM "CustomTag" WHERE "guildId" = $1 AND "enabled" = true"#).bind(guild_id).fetch_all(app.db()).await;
    match rows {
        Ok(rows) => {
            let names: HashSet<String> = rows.into_iter().map(|(n,)| n).collect();
            let hit = names.contains(name);
            lock(&s.tag_index).put(guild_id, Arc::new(names), app.clock().now_ms());
            hit
        }
        Err(e) => {
            warn_once(app, &format!("{guild_id}:tagindex:0"), format!("Custom commands: could not load the name index of {guild_id}: {e}"));
            false
        }
    }
}

/// `findTagRow(guildId, name)`: the hook-path lookup: primed test rows first, then the database. Never fails (`None`).
pub async fn find_tag_row(app: &App, guild_id: &str, name: &str) -> Option<CustomTag> {
    if let Some(rows) = lock(&state(app).primed).get(guild_id) {
        return rows.get(name).cloned();
    }
    let found = async { SelectBuilder::new(&CUSTOM_TAG_INFO).filter("guildId", json!(guild_id))?.filter("name", json!(name))?.build()?.fetch_optional(app.db()).await }.await;
    match found {
        Ok(row) => row,
        Err(e) => {
            warn_once(app, &format!("{guild_id}:tagrow:0"), format!("Custom commands: lookup of \"{name}\" failed in {guild_id}: {e}"));
            None
        }
    }
}

// ── Arguments, usage, prefix parsing ──────────────────────────────────────────

/// `parseArgs(str)`: MEE6 arguments are space-separated and quotes mean nothing; use `{n...}` for the rest.
pub fn parse_args(s: &str) -> Vec<String> {
    let s = bot_util::js::trim(s);
    if s.is_empty() {
        return Vec::new();
    }
    s.split(bot_util::js::is_js_space).filter(|w| !w.is_empty()).take(ARGS_MAX).map(str::to_string).collect()
}

/// `effectivePrefix(p)`: the prefix commands answer to. `""` (set by `/settings prefix off`) disables prefix
/// invocation (`None`); any other invalid value (a hand-edited row) falls back to `!`. `None` input is `null`.
pub fn effective_prefix(p: Option<&str>) -> Option<String> {
    match p {
        Some("") => None,
        Some(s) if s.chars().count() <= 5 && !s.chars().any(bot_util::js::is_js_space) => Some(s.to_string()),
        _ => Some("!".into()),
    }
}

/// The result of [`parse_invocation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub name: String,
    pub args: Vec<String>,
}

/// `parseInvocation(content, prefix)`: `!name args…` → the lowercased name and the arguments.
pub fn parse_invocation(content: &str, prefix: Option<&str>) -> Option<Invocation> {
    let prefix = prefix.filter(|p| !p.is_empty())?;
    let rest = content.strip_prefix(prefix)?;
    // /^(\S{1,50})(?:\s+|$)/u
    let word_end = rest.char_indices().find(|(_, c)| bot_util::js::is_js_space(*c)).map(|(i, _)| i).unwrap_or(rest.len());
    let word = &rest[..word_end];
    let n = word.chars().count();
    if n == 0 || n > 50 {
        return None;
    }
    let name = bot_util::js::lower(word);
    if !is_tag_name(&name) {
        return None;
    }
    Some(Invocation { name, args: parse_args(&rest[word_end..]) })
}

/// `OPTIONAL_ARG_RE` (`/\{(?:\d{1,3}:[^{}\n]{0,60}|\d{0,3}\.\.\.\d{0,3})\}/`): `{n:default}`, `{n...}`, `{...n}`,
/// `{a...b}`, `{...}`.
pub fn has_optional_arg(text: &str) -> bool {
    let units: Vec<u16> = text.encode_utf16().collect();
    let digit = |u: u16| (u16::from(b'0')..=u16::from(b'9')).contains(&u);
    let (open, close, colon, dot, nl) = (u16::from(b'{'), u16::from(b'}'), u16::from(b':'), u16::from(b'.'), u16::from(b'\n'));
    for start in 0..units.len() {
        if units[start] != open {
            continue;
        }
        let mut i = start + 1;
        let mut d = 0;
        while i < units.len() && digit(units[i]) && d < 3 {
            i += 1;
            d += 1;
        }
        // {n:default}
        if (1..=3).contains(&d) && i < units.len() && units[i] == colon {
            let mut j = i + 1;
            let mut k = 0;
            while j < units.len() && k <= 60 {
                let u = units[j];
                if u == close {
                    return true;
                }
                if u == open || u == nl {
                    break;
                }
                j += 1;
                k += 1;
            }
        }
        // {a...b}
        if i + 2 < units.len() && units[i] == dot && units[i + 1] == dot && units[i + 2] == dot {
            let mut j = i + 3;
            let mut k = 0;
            while j < units.len() && digit(units[j]) && k < 3 {
                j += 1;
                k += 1;
            }
            if j < units.len() && units[j] == close {
                return true;
            }
        }
    }
    false
}

fn args_ctx(args: Vec<String>) -> RenderContext {
    RenderContext { args: Some(args), ..Default::default() }
}

/// `buildUsage(tag, { prefix, slash })`: `!name <1> [more…]` or `/tag use name:x args:<1>`. With no prefix (prefix
/// commands off) the slash form is returned. Arguments are positional, so a command that only uses `{2}` still needs
/// a first word.
pub fn build_usage(tag: &CustomTag, prefix: Option<&str>, slash: bool) -> String {
    let mut texts: Vec<&str> = Vec::new();
    let actions = get_effective_actions(tag);
    for a in &actions {
        if a.is_message() {
            texts.extend(a.responses.iter().map(String::as_str));
        }
    }
    let empty = args_ctx(Vec::new());
    let mut max_req = 0u32;
    for t in &texts {
        for n in render_placeholders_detailed(t, &empty, None).missing_args {
            max_req = max_req.max(n);
        }
    }
    let optional = texts.iter().any(|t| has_optional_arg(t));
    let mut parts: Vec<String> = (1..=max_req).map(|i| format!("<{i}>")).collect();
    if optional {
        parts.push("[more…]".into());
    }
    let arg_text = parts.join(" ");
    match prefix.filter(|p| !p.is_empty() && !slash) {
        None => format!("/tag use name:{}{}", tag.name, if arg_text.is_empty() { String::new() } else { format!(" args:{arg_text}") }),
        Some(p) => format!("{p}{}{}", tag.name, if arg_text.is_empty() { String::new() } else { format!(" {arg_text}") }),
    }
}

// ── The action model ──────────────────────────────────────────────────────────

/// An action's type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionKind {
    Reply,
    Send,
    AddRole,
    RemoveRole,
    ToggleRole,
}

impl ActionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reply => "reply",
            Self::Send => "send",
            Self::AddRole => "addRole",
            Self::RemoveRole => "removeRole",
            Self::ToggleRole => "toggleRole",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "reply" => Self::Reply,
            "send" => Self::Send,
            "addRole" => Self::AddRole,
            "removeRole" => Self::RemoveRole,
            "toggleRole" => Self::ToggleRole,
            _ => return None,
        })
    }
}

/// One normalised action (`{ type, responses, embed, dm | channelId, pingRoleIds? }` or `{ type, roleId }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagAction {
    pub kind: ActionKind,
    /// Message actions: the random replies (at least one).
    pub responses: Vec<String>,
    pub embed: bool,
    /// `reply` only.
    pub dm: bool,
    /// `send` only.
    pub channel_id: Option<String>,
    /// Role actions only.
    pub role_id: Option<String>,
    /// Message actions (never a DM): roles the text may ping.
    pub ping_role_ids: Vec<String>,
}

impl TagAction {
    /// `{ type: 'reply', dm, embed, responses: [text] }`.
    pub fn reply(dm: bool, embed: bool, text: &str) -> Self {
        Self { kind: ActionKind::Reply, responses: vec![text.to_string()], embed, dm, channel_id: None, role_id: None, ping_role_ids: Vec::new() }
    }

    /// `{ type: 'send', channelId, embed, responses: [text] }`.
    pub fn send(channel_id: &str, embed: bool, text: &str) -> Self {
        Self {
            kind: ActionKind::Send,
            responses: vec![text.to_string()],
            embed,
            dm: false,
            channel_id: Some(channel_id.to_string()),
            role_id: None,
            ping_role_ids: Vec::new(),
        }
    }

    /// `{ type, roleId }`.
    pub fn role(kind: ActionKind, role_id: &str) -> Self {
        Self { kind, responses: Vec::new(), embed: false, dm: false, channel_id: None, role_id: Some(role_id.to_string()), ping_role_ids: Vec::new() }
    }

    /// `MESSAGE_ACTION_TYPES.includes(a.type)` (the JS `a.responses` truthiness).
    pub fn is_message(&self) -> bool {
        matches!(self.kind, ActionKind::Reply | ActionKind::Send)
    }

    /// The stored JSON, in the key order `normalizeActions` produces.
    pub fn to_json(&self) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("type".into(), json!(self.kind.as_str()));
        if self.is_message() {
            m.insert("responses".into(), json!(self.responses));
            m.insert("embed".into(), json!(self.embed));
            if self.kind == ActionKind::Reply {
                m.insert("dm".into(), json!(self.dm));
            } else {
                m.insert("channelId".into(), json!(self.channel_id));
            }
            if !self.ping_role_ids.is_empty() {
                m.insert("pingRoleIds".into(), json!(self.ping_role_ids));
            }
        } else {
            m.insert("roleId".into(), json!(self.role_id));
        }
        Value::Object(m)
    }
}

/// A list of actions as stored JSON.
pub fn actions_to_json(actions: &[TagAction]) -> Value {
    Value::Array(actions.iter().map(TagAction::to_json).collect())
}

/// `normalizeActions(json)`: pure, tolerant, never fails. `None` = legacy (null) or an unknown format (a non-array).
/// Invalid entries are dropped silently; the result is capped at `LIMITS.TAG_ACTIONS`.
pub fn normalize_actions(json: Option<&Value>) -> Option<Vec<TagAction>> {
    let list = json?.as_array()?;
    let mut out = Vec::new();
    for a in list.iter().take(limits::TAG_ACTIONS) {
        let Some(obj) = a.as_object() else { continue };
        let Some(kind) = obj.get("type").and_then(Value::as_str).and_then(ActionKind::parse) else { continue };
        match kind {
            ActionKind::Reply | ActionKind::Send => {
                let responses: Vec<String> = obj
                    .get("responses")
                    .and_then(Value::as_array)
                    .map(|r| {
                        r.iter()
                            .filter_map(Value::as_str)
                            .filter(|s| !bot_util::js::trim(s).is_empty())
                            .take(limits::TAG_RESPONSES)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                if responses.is_empty() {
                    continue;
                }
                let channel_id = obj.get("channelId").and_then(Value::as_str).filter(|c| is_snowflake(c)).map(str::to_string);
                if kind == ActionKind::Send && channel_id.is_none() {
                    continue;
                }
                let embed = obj.get("embed") == Some(&Value::Bool(true));
                let dm = kind == ActionKind::Reply && obj.get("dm") == Some(&Value::Bool(true));
                let mut pings: Vec<String> = Vec::new();
                for id in obj.get("pingRoleIds").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                    if is_snowflake(id) && !pings.iter().any(|p| p == id) {
                        pings.push(id.to_string());
                    }
                }
                pings.truncate(PING_ROLES_MAX);
                out.push(TagAction {
                    kind,
                    responses,
                    embed,
                    dm,
                    channel_id: if kind == ActionKind::Send { channel_id } else { None },
                    role_id: None,
                    ping_role_ids: if dm { Vec::new() } else { pings },
                });
            }
            _ => {
                if let Some(role) = obj.get("roleId").and_then(Value::as_str).filter(|r| is_snowflake(r)) {
                    out.push(TagAction::role(kind, role));
                }
            }
        }
    }
    Some(out)
}

/// `legacyActions(tag)`: the effective action list of a legacy tag (actions null).
pub fn legacy_actions(content: &str, use_embed: bool) -> Vec<TagAction> {
    vec![TagAction::reply(false, use_embed, content)]
}

/// `getEffectiveActions(tag)`: `normalizeActions(tag.actions) ?? legacyActions(tag)` (pure; see
/// [`warn_unknown_actions_format`] for the JS warning).
pub fn get_effective_actions(tag: &CustomTag) -> Vec<TagAction> {
    normalize_actions(tag.actions.as_ref()).unwrap_or_else(|| legacy_actions(&tag.content, tag.use_embed))
}

/// The warning `getEffectiveActions` logs for an unknown stored format, once per tag per hour.
pub fn warn_unknown_actions_format(app: &App, tag: &CustomTag) {
    let stored = tag.actions.as_ref().filter(|v| !v.is_null());
    if stored.is_some() && normalize_actions(stored).is_none() && state(app).format_throttle.allow(&format!("{}:{}", tag.guild_id, tag.id)) {
        bot_util::log_warn!(format!("Custom command {} has an unknown actions format; using its content", tag.id));
    }
}

/// `primaryMessageIndex(actions)`: the first reply or send action.
pub fn primary_message_index(actions: &[TagAction]) -> Option<usize> {
    actions.iter().position(TagAction::is_message)
}

/// `channelReplyIndex(actions)`: the first reply that is not a DM.
pub fn channel_reply_index(actions: &[TagAction]) -> Option<usize> {
    actions.iter().position(|a| a.kind == ActionKind::Reply && !a.dm)
}

/// `syncLegacyColumns(actions)`: the mirror columns written together with `actions` (content and useEmbed follow the
/// primary message action).
pub fn sync_legacy_columns(actions: &[TagAction]) -> Vec<(&'static str, Value)> {
    match primary_message_index(actions) {
        None => Vec::new(),
        Some(i) => vec![("content", json!(actions[i].responses[0])), ("useEmbed", json!(actions[i].embed))],
    }
}

// ── Access and scope ──────────────────────────────────────────────────────────

/// A message channel as the scope checks see it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelScope {
    pub id: String,
    pub thread: bool,
    /// A channel's category, or a thread's parent channel.
    pub parent_id: Option<String>,
    /// A thread's parent's category.
    pub parent_parent_id: Option<String>,
}

impl ChannelScope {
    /// The scope of a cached guild channel (a thread's parent read from the guild).
    pub fn of(guild: Option<&Guild>, channel: &GuildChannel) -> Self {
        let thread = bot_core::permissions::is_thread(channel);
        let parent_id = channel.parent_id.map(|p| p.to_string());
        let parent_parent_id = if thread {
            channel.parent_id.and_then(|p| guild.and_then(|g| bot_core::permissions::find_channel(g, p))).and_then(|p| p.parent_id).map(|c| c.to_string())
        } else {
            None
        };
        Self { id: channel.id.to_string(), thread, parent_id, parent_parent_id }
    }

    /// The scope of a channel the cache does not have: just its id.
    pub fn bare(id: &str) -> Self {
        Self { id: id.to_string(), ..Default::default() }
    }
}

/// `member.roles.cache` as discord.js builds it: @everyone (the guild id) first, then the member's roles that the
/// guild still has (every role when the guild is not cached).
pub fn member_role_ids(guild: Option<&Guild>, guild_id: &str, roles: &[serenity::all::RoleId]) -> Vec<String> {
    let mut out = vec![guild_id.to_string()];
    for r in roles {
        let id = r.to_string();
        if id != guild_id && guild.is_none_or(|g| g.roles.contains_key(r)) && !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// `channelScopeIds(channel)`: the ids a message channel counts as: itself, a thread's parent and that parent's
/// category, or its own category.
pub fn channel_scope_ids(channel: Option<&ChannelScope>) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    let Some(c) = channel else { return ids };
    let mut add = |id: Option<&str>| {
        if let Some(id) = id.filter(|i| !i.is_empty())
            && !ids.iter().any(|x| x == id)
        {
            ids.push(id.to_string());
        }
    };
    add(Some(&c.id));
    add(c.parent_id.as_deref());
    if c.thread {
        add(c.parent_parent_id.as_deref());
    }
    ids
}

/// Why access was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessCode {
    ChannelDenied,
    ChannelNotAllowed,
    RoleDenied,
    RoleNotAllowed,
}

impl AccessCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChannelDenied => "CHANNEL_DENIED",
            Self::ChannelNotAllowed => "CHANNEL_NOT_ALLOWED",
            Self::RoleDenied => "ROLE_DENIED",
            Self::RoleNotAllowed => "ROLE_NOT_ALLOWED",
        }
    }
}

/// The four allow and deny lists of a command.
#[derive(Debug, Clone, Copy)]
pub struct AccessLists<'a> {
    pub allow_role_ids: &'a [String],
    pub deny_role_ids: &'a [String],
    pub allow_channel_ids: &'a [String],
    pub deny_channel_ids: &'a [String],
}

impl<'a> AccessLists<'a> {
    pub fn of(tag: &'a CustomTag) -> Self {
        Self {
            allow_role_ids: &tag.allow_role_ids,
            deny_role_ids: &tag.deny_role_ids,
            allow_channel_ids: &tag.allow_channel_ids,
            deny_channel_ids: &tag.deny_channel_ids,
        }
    }
}

/// `checkTagAccess(tag, { member, channel })`: allow and deny lists. There is no admin bypass (MEE6 applies the lists
/// to everyone). `member_roles` `None` is no member.
pub fn check_tag_access(tag: AccessLists<'_>, member_roles: Option<&[String]>, channel: Option<&ChannelScope>) -> Result<(), AccessCode> {
    let ids = channel_scope_ids(channel);
    if tag.deny_channel_ids.iter().any(|id| ids.contains(id)) {
        return Err(AccessCode::ChannelDenied);
    }
    if !tag.allow_channel_ids.is_empty() && !tag.allow_channel_ids.iter().any(|id| ids.contains(id)) {
        return Err(AccessCode::ChannelNotAllowed);
    }
    check_tag_roles(tag, member_roles)
}

/// `checkTagRoles(tag, member)`: the role half of [`check_tag_access`]. Also decides what members may read: a command
/// their roles cannot run stays hidden from `/tag info`, `raw`, `list` and `action list`.
pub fn check_tag_roles(tag: AccessLists<'_>, member_roles: Option<&[String]>) -> Result<(), AccessCode> {
    let has = |id: &String| member_roles.is_some_and(|r| r.contains(id));
    if tag.deny_role_ids.iter().any(has) {
        return Err(AccessCode::RoleDenied);
    }
    if !tag.allow_role_ids.is_empty() && !tag.allow_role_ids.iter().any(has) {
        return Err(AccessCode::RoleNotAllowed);
    }
    Ok(())
}

/// `triggerAppliesHere(t, { channel, member })`: channel scope and exempt roles of a trigger; stale ids never match.
pub fn trigger_applies_here(t: &TriggerProjection, channel: Option<&ChannelScope>, member_roles: Option<&[String]>) -> bool {
    if !t.channel_ids.is_empty() {
        let ids = channel_scope_ids(channel);
        if !t.channel_ids.iter().any(|id| ids.contains(id)) {
            return false;
        }
    }
    !(t.exempt_role_ids.iter().any(|id| member_roles.is_some_and(|r| r.contains(id))))
}

// ── Rendering message actions ─────────────────────────────────────────────────

/// A random number in `[0, 1)` (`Math.random`).
pub type Rng = Arc<dyn Fn() -> f64 + Send + Sync>;

/// `Math.random`.
pub fn default_rng() -> Rng {
    Arc::new(rand::random::<f64>)
}

/// `pickResponse(responses, rng)`.
pub fn pick_response(responses: &[String], rng: &dyn Fn() -> f64) -> String {
    if responses.is_empty() {
        return String::new();
    }
    let i = (rng() * responses.len() as f64).floor();
    let i = if i.is_finite() { i } else { 0.0 };
    let i = i.max(0.0).min((responses.len() - 1) as f64) as usize;
    responses[i].clone()
}

/// `{ parse: ['users'] }`, plus `roles` when some are allowed.
pub fn user_mentions(roles: &[String]) -> CreateAllowedMentions {
    let m = CreateAllowedMentions::new().all_users(true);
    if roles.is_empty() {
        return m;
    }
    m.roles(roles.iter().filter_map(|r| r.parse::<u64>().ok().filter(|v| *v != 0)).map(serenity::all::RoleId::new))
}

/// `buildMessagePayload(action, response, renderCtx, settings, { pingRoleIds })`: one response of a message action
/// rendered into a payload, or `None` when nothing visible remains. Content is clamped to 2000 and an embed
/// description to 4096 AFTER expansion; `ping_role_ids` (already filtered) defaults to the action's.
pub fn build_message_payload(
    action: &TagAction,
    response: &str,
    ctx: &RenderContext,
    settings: Option<&GuildSettings>,
    ping_role_ids: Option<&[String]>,
) -> Option<Rendered> {
    let tpl = if action.embed { json!({ "embed": { "description": response, "timestamp": true } }) } else { json!({ "content": response }) };
    let color = settings.and_then(|s| s.embed_color).filter(|c| (0..=0xff_ffff).contains(c)).map(|c| c as u32);
    let roles = ping_role_ids.unwrap_or(&action.ping_role_ids);
    render_template(
        &tpl,
        ctx,
        RenderOptions { default_color: Some(color.unwrap_or(bot_config::colors::INFO)), allowed_mentions: Some(user_mentions(roles)), extra_components: Vec::new() },
    )
}

/// One step of a run: the action, and for a message action the response picked for this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub action: TagAction,
    pub response: Option<String>,
}

/// The result of [`plan_run`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub steps: Vec<Step>,
    /// Argument numbers asked for but not given (sorted).
    pub missing: Vec<u32>,
}

/// `planRun(tag, renderCtx, rng)`: picks one response per message action and finds the argument numbers that were
/// asked for but not given.
pub fn plan_run(actions: Vec<TagAction>, ctx: &RenderContext, rng: &dyn Fn() -> f64) -> Plan {
    let steps: Vec<Step> = actions
        .into_iter()
        .map(|a| {
            let response = a.is_message().then(|| pick_response(&a.responses, rng));
            Step { action: a, response }
        })
        .collect();
    let mut missing: Vec<u32> = Vec::new();
    for s in &steps {
        if let Some(r) = &s.response {
            for n in render_placeholders_detailed(r, ctx, None).missing_args {
                if !missing.contains(&n) {
                    missing.push(n);
                }
            }
        }
    }
    missing.sort_unstable();
    Plan { steps, missing }
}

// ── Cooldowns ─────────────────────────────────────────────────────────────────

/// `tagCooldownKey(tag, guildId, userId)`: `None` for NONE (or 0 seconds); a per-guild key for SERVER; a per-user
/// key for USER.
pub fn tag_cooldown_key(tag_id: &str, scope: CooldownScope, seconds: i64, guild_id: &str, user_id: &str) -> Option<String> {
    if seconds <= 0 {
        return None;
    }
    match scope {
        CooldownScope::None => None,
        CooldownScope::Server => Some(format!("tag:cd:{guild_id}:{tag_id}")),
        CooldownScope::User => Some(format!("tag:cd:{guild_id}:{tag_id}:{user_id}")),
    }
}

/// `trigger:cd:{g}:{id}:{user}`.
pub fn trigger_cooldown_key(guild_id: &str, trigger_id: &str, user_id: &str) -> String {
    format!("trigger:cd:{guild_id}:{trigger_id}:{user_id}")
}

/// The result of [`acquire_cooldown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cooldown {
    /// Taken: the run may go ahead.
    Ok,
    /// Redis is unavailable (fail closed).
    Unavailable,
    /// On cooldown until this time (ms).
    Until(i64),
}

/// `acquireCooldown(key, seconds)`: atomic SET NX EX. Fails CLOSED when Redis is unavailable.
pub async fn acquire_cooldown(app: &App, key: &str, seconds: i64) -> Cooldown {
    let until = app.clock().now_ms() + seconds * 1000;
    match app.cache().set_nx(key, &until, seconds).await {
        Some(true) => Cooldown::Ok,
        None => Cooldown::Unavailable,
        Some(false) => {
            let v = app.cache().get(key).await.and_then(|v| match v {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => bot_util::js::trim(&s).parse::<f64>().ok(),
                _ => None,
            });
            match v {
                Some(v) if v.is_finite() && v > app.clock().now_ms() as f64 => Cooldown::Until(v as i64),
                _ => Cooldown::Until(until),
            }
        }
    }
}

/// `releaseCooldown(key)`: a run that did nothing does not burn the cooldown.
pub async fn release_cooldown(app: &App, key: &str) {
    app.cache().del_one(key).await;
}

/// `clearTagCooldowns(guildId, tagId)`: drops the server key and every per-user key of a command.
pub async fn clear_tag_cooldowns(app: &App, guild_id: &str, tag_id: &str) {
    app.cache().del_one(&format!("tag:cd:{guild_id}:{tag_id}")).await;
    app.cache().invalidate(&format!("tag:cd:{guild_id}:{tag_id}:*")).await;
}

// ── Usage counters ────────────────────────────────────────────────────────────

fn spawn_increment(app: &App, sql: &'static str, id: &str, what: &'static str) {
    let s = state(app);
    if !s.count_uses.load(Ordering::SeqCst) {
        return;
    }
    s.increments.fetch_add(1, Ordering::SeqCst);
    let (pool, id) = (app.db().clone(), id.to_string());
    let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
    rt.spawn(async move {
        if let Err(e) = sqlx::query(sql).bind(&id).execute(&pool).await {
            bot_util::log_debug!(format!("{what} uses increment failed: {e}"));
        }
    });
}

/// `incrementTagUses(id)`: fire-and-forget, clamped to int4 in SQL; raw SQL leaves `updatedAt` for real edits.
pub fn increment_tag_uses(app: &App, id: &str) {
    spawn_increment(app, TAG_USES_SQL, id, "Custom command");
}

/// `incrementTriggerUses(id)`.
pub fn increment_trigger_uses(app: &App, id: &str) {
    spawn_increment(app, TRIGGER_USES_SQL, id, "Trigger");
}

// ── The executor's view of Discord ────────────────────────────────────────────

/// A failed Discord call as the executor sees it (`err.code`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoError {
    pub code: Option<u64>,
    /// The interaction can no longer be answered (`isInteractionGone`).
    pub interaction_gone: bool,
    pub message: String,
}

impl IoError {
    pub fn code(code: u64) -> Self {
        Self { code: Some(code), interaction_gone: false, message: format!("Discord error {code}") }
    }
}

/// Everything [`run_tag`] and the hook bodies read from or do to Discord. [`crate::tag_io::DiscordIo`] is the
/// production one (serenity cache and REST, and the interaction's `Responder`); tests use a fake.
pub trait TagIo: Send + Sync {
    /// `guild.channels.cache.has(id)`.
    fn channel_exists(&self, channel_id: &str) -> bool;
    /// `checkChannelUsable(channel, { needs })` for the bot.
    fn channel_usable(&self, channel_id: &str, needs: &[&str]) -> ChannelUsable;
    /// `checkAssignableRole(guild, role)` (no executor, no dangerous permissions); `Role not found.` when missing.
    fn check_assignable_role(&self, role_id: &str) -> RoleCheck;
    /// Whether the role exists (`guild.roles.cache.get(id)`).
    fn role_exists(&self, role_id: &str) -> bool;
    /// `pingableRoleIds(channel, guild.members.me, ids).allowed`: the bot is the sender.
    fn pingable_by_bot(&self, channel_id: &str, role_ids: &[String]) -> Vec<String>;
    /// `channel.permissionsFor(me).has(ManageMessages)`.
    fn bot_can_manage_messages(&self, channel_id: &str) -> bool;
    /// Channel and role names for [`action_label`].
    fn names(&self) -> &dyn NameLookup;

    /// `guild.members.fetch(userId)`'s role ids (`None`: not a member).
    fn fetch_member_roles(&self, user_id: &str) -> impl Future<Output = Option<Vec<String>>> + Send;
    /// `member.roles.add(roleId, reason)`.
    fn add_role(&self, user_id: &str, role_id: &str, reason: &str) -> impl Future<Output = Result<(), IoError>> + Send;
    /// `member.roles.remove(roleId, reason)`.
    fn remove_role(&self, user_id: &str, role_id: &str, reason: &str) -> impl Future<Output = Result<(), IoError>> + Send;
    /// `channel.send(payload)`.
    fn send_channel(&self, channel_id: &str, payload: Payload) -> impl Future<Output = Result<(), IoError>> + Send;
    /// `user.send(payload)`.
    fn send_dm(&self, user_id: &str, payload: Payload) -> impl Future<Output = Result<(), IoError>> + Send;
    /// `message.delete()`.
    fn delete_message(&self, channel_id: &str, message_id: &str) -> impl Future<Output = Result<(), IoError>> + Send;
    /// `message.reply({ embeds: [embed], allowedMentions: { parse: [], repliedUser: false } })`: the sent message's id.
    fn reply_notice(&self, channel_id: &str, message_id: &str, embed: CreateEmbed) -> impl Future<Output = Result<String, IoError>> + Send;
    /// `setTimeout(() => { markBotDeletion(id); sent.delete() }, after_ms)`: never fails.
    fn schedule_delete(&self, channel_id: &str, message_id: &str, after_ms: u64);

    /// `interaction.reply(payload)`.
    fn interaction_reply(&self, payload: Payload) -> impl Future<Output = Result<(), IoError>> + Send;
    /// `interaction.followUp(payload)`.
    fn interaction_follow_up(&self, payload: Payload) -> impl Future<Output = Result<(), IoError>> + Send;
    /// `interaction.editReply(payload)`.
    fn interaction_edit_reply(&self, payload: Payload) -> impl Future<Output = Result<(), IoError>> + Send;
    /// `safeDefer(interaction, { ephemeral: true })`.
    fn interaction_defer_ephemeral(&self) -> impl Future<Output = bool> + Send;
    /// `safeReply(interaction, payload)`.
    fn interaction_safe_reply(&self, payload: Payload) -> impl Future<Output = ()> + Send;
}

// ── The executor ──────────────────────────────────────────────────────────────

fn needs_for(a: &TagAction) -> &'static [&'static str] {
    if a.embed { &["ViewChannel", "SendMessages", "EmbedLinks"] } else { &["ViewChannel", "SendMessages"] }
}

/// Where a run came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Slash,
    Prefix,
}

/// The invoking message of a prefix run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvokingMessage {
    pub id: String,
    pub channel_id: String,
    /// `#name` for the "Manage Messages" warning.
    pub channel_name: Option<String>,
}

/// `runTag` options.
pub struct RunOpts<'a> {
    pub source: Source,
    pub guild_id: String,
    /// The channel the command was used in.
    pub channel: ChannelScope,
    /// The member's role ids (`None`: no member).
    pub member_roles: Option<Vec<String>>,
    pub user_id: String,
    pub args: Vec<String>,
    pub settings: Option<&'a GuildSettings>,
    /// `null`: prefix commands are off.
    pub prefix: Option<String>,
    /// The render context of the invocation (`args` is filled in by [`run_tag`]).
    pub render: RenderContext,
    pub rng: Option<Rng>,
    /// `false` only in offline tests.
    pub check_permissions: bool,
    /// Slash: what is left of the 3 s window (`ackBudgetMs`), `None` = 2000 ms.
    pub ack_budget_ms: Option<u64>,
    /// Prefix: the usage message.
    pub message: Option<InvokingMessage>,
}

/// The status of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Denied,
    Empty,
    Usage,
    Unavailable,
    Cooldown,
    Ran,
}

/// The outcome of one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepResult {
    pub ok: bool,
    pub code: Option<FailureCode>,
    pub detail: Option<String>,
    pub summary: Option<String>,
}

impl StepResult {
    fn ok(summary: impl Into<String>) -> Self {
        Self { ok: true, code: None, detail: None, summary: Some(summary.into()) }
    }
    fn fail(code: FailureCode) -> Self {
        Self { ok: false, code: Some(code), detail: None, summary: None }
    }
    fn fail_with(code: FailureCode, detail: impl Into<String>) -> Self {
        Self { ok: false, code: Some(code), detail: Some(detail.into()), summary: None }
    }
}

/// The result of [`run_tag`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    pub status: RunStatus,
    pub results: Vec<StepResult>,
    pub success: bool,
    pub deleted: bool,
}

impl RunResult {
    fn early(status: RunStatus) -> Self {
        Self { status, results: Vec::new(), success: false, deleted: false }
    }
}

/// What a step was doing when Discord refused it (`failureFromError`'s `kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepKind {
    Dm,
    Reply,
    Send,
    Role,
}

/// `failureFromError(err, kind)`: maps a Discord error onto a failure code.
fn failure_from_error(err: &IoError, kind: StepKind) -> StepResult {
    let code = err.code;
    if err.interaction_gone || code.is_some_and(|c| INTERACTION_GONE_CODES.contains(&c)) {
        return StepResult::fail(FailureCode::Expired);
    }
    match (kind, code) {
        (StepKind::Dm, Some(50007)) => return StepResult::fail(FailureCode::DmClosed),
        (StepKind::Send | StepKind::Reply, Some(10003)) => return StepResult::fail(FailureCode::ChannelGone),
        (StepKind::Role, Some(10011)) => return StepResult::fail(FailureCode::RoleGone),
        (StepKind::Role, Some(10007)) => return StepResult::fail(FailureCode::MemberGone),
        (_, Some(50001 | 50013)) => return StepResult::fail(FailureCode::MissingPerms),
        _ => {}
    }
    let kind_text = match kind {
        StepKind::Dm => "dm",
        StepKind::Reply => "reply",
        StepKind::Send => "send",
        StepKind::Role => "role",
    };
    bot_util::log_warn!(format!(
        "Custom command step failed ({kind_text}): {} {}",
        code.map(|c| c.to_string()).unwrap_or_default(),
        err.message
    ));
    StepResult::fail(FailureCode::DiscordError)
}

/// How a public "reply in channel" step is posted when it is not the interaction's primary reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum How {
    FollowUp,
    Channel,
}

struct Exec<'a, I: TagIo> {
    io: &'a I,
    opts: &'a RunOpts<'a>,
    tag_name: &'a str,
    member_roles: Option<Vec<String>>,
}

impl<I: TagIo> Exec<'_, I> {
    /// Payload of a message step; role pings follow `pingableRoleIds` with the bot as the sender.
    fn payload_for(&self, step: &Step, target: Option<&str>) -> Option<Rendered> {
        let a = &step.action;
        let ping: Option<Vec<String>> = if !a.ping_role_ids.is_empty() && !(a.kind == ActionKind::Reply && a.dm) {
            Some(match target {
                Some(t) => self.io.pingable_by_bot(t, &a.ping_role_ids),
                None => Vec::new(),
            })
        } else {
            None
        };
        build_message_payload(a, step.response.as_deref().unwrap_or(""), &self.opts.render, self.opts.settings, ping.as_deref())
    }

    async fn post_channel_reply(&self, step: &Step, how: How) -> Result<StepResult, IoError> {
        let channel = self.opts.channel.id.clone();
        let Some(payload) = self.payload_for(step, Some(&channel)) else { return Ok(StepResult::fail(FailureCode::Empty)) };
        if how == How::FollowUp {
            self.io.interaction_follow_up(payload.to_payload()).await?;
        } else {
            if self.opts.check_permissions {
                let usable = self.io.channel_usable(&channel, needs_for(&step.action));
                if !usable.ok {
                    return Ok(StepResult::fail_with(FailureCode::MissingPerms, bot_core::permissions::permission_names(&usable.missing).join(", ")));
                }
            }
            self.io.send_channel(&channel, payload.to_payload()).await?;
        }
        Ok(StepResult::ok("💬 Replied in the channel."))
    }

    async fn exec_step(&mut self, step: &Step, how: How) -> StepResult {
        let a = &step.action;
        let kind = match a.kind {
            ActionKind::Reply if a.dm => StepKind::Dm,
            ActionKind::Reply => StepKind::Reply,
            ActionKind::Send => StepKind::Send,
            _ => StepKind::Role,
        };
        match self.exec_inner(step, how).await {
            Ok(r) => r,
            Err(e) => failure_from_error(&e, kind),
        }
    }

    async fn exec_inner(&mut self, step: &Step, how: How) -> Result<StepResult, IoError> {
        let a = &step.action;
        if a.kind == ActionKind::Reply && a.dm {
            let Some(payload) = self.payload_for(step, None) else { return Ok(StepResult::fail(FailureCode::Empty)) };
            self.io.send_dm(&self.opts.user_id, payload.to_payload()).await?;
            return Ok(StepResult::ok("📨 Sent you a DM."));
        }
        if a.kind == ActionKind::Reply {
            return self.post_channel_reply(step, how).await;
        }
        if a.kind == ActionKind::Send {
            let target = a.channel_id.clone().unwrap_or_default();
            if !self.io.channel_exists(&target) {
                return Ok(StepResult::fail(FailureCode::ChannelGone));
            }
            let Some(payload) = self.payload_for(step, Some(&target)) else { return Ok(StepResult::fail(FailureCode::Empty)) };
            if self.opts.check_permissions {
                let usable = self.io.channel_usable(&target, needs_for(a));
                if !usable.ok {
                    return Ok(StepResult::fail_with(FailureCode::MissingPerms, bot_core::permissions::permission_names(&usable.missing).join(", ")));
                }
            }
            self.io.send_channel(&target, payload.to_payload()).await?;
            return Ok(StepResult::ok(format!("📤 Posted in <#{target}>.")));
        }
        // Role actions, one role at a time (never the array form).
        let role = a.role_id.clone().unwrap_or_default();
        if !self.io.role_exists(&role) {
            return Ok(StepResult::fail(FailureCode::RoleGone));
        }
        let assignable = self.io.check_assignable_role(&role);
        if !assignable.ok {
            return Ok(StepResult::fail_with(FailureCode::RoleUnassignable, assignable.reason.unwrap_or_default()));
        }
        if self.member_roles.is_none() {
            self.member_roles = self.io.fetch_member_roles(&self.opts.user_id).await;
        }
        let Some(roles) = self.member_roles.as_ref() else { return Ok(StepResult::fail(FailureCode::MemberGone)) };
        let has = roles.contains(&role);
        let reason = format!("Custom command {}", self.tag_name);
        let give = a.kind == ActionKind::AddRole || (a.kind == ActionKind::ToggleRole && !has);
        if give {
            if has {
                return Ok(StepResult::ok(format!("ℹ️ You already have <@&{role}>.")));
            }
            self.io.add_role(&self.opts.user_id, &role, &reason).await?;
            if let Some(r) = self.member_roles.as_mut() {
                r.push(role.clone());
            }
            return Ok(StepResult::ok(format!("➕ Gave you <@&{role}>.")));
        }
        if !has {
            return Ok(StepResult::ok(format!("ℹ️ You don't have <@&{role}>.")));
        }
        self.io.remove_role(&self.opts.user_id, &role, &reason).await?;
        if let Some(r) = self.member_roles.as_mut() {
            r.retain(|x| x != &role);
        }
        Ok(StepResult::ok(format!("➖ Removed <@&{role}>.")))
    }
}

fn embed_payload(embed: CreateEmbed) -> Payload {
    Payload::embed(embed)
}

/// `runTag(tag, opts)`: runs a custom command: access, arguments, cooldown, every action in order, the outcome report
/// and (prefix only) deleting the usage. Nothing with a side effect happens before the cooldown is taken.
pub async fn run_tag<I: TagIo>(app: &App, tag: &CustomTag, mut opts: RunOpts<'_>, io: &I) -> RunResult {
    let slash = opts.source == Source::Slash;
    let guild_id = opts.guild_id.clone();
    opts.render.args = Some(opts.args.clone());
    let opts = opts;

    // Ephemeral answer to a slash invocation that did not get to run; silent for prefix use.
    let tell = |embed: CreateEmbed| async move {
        if slash {
            io.interaction_safe_reply(embed_payload(embed).ephemeral(true)).await;
        }
    };

    // 1. Access.
    if let Err(code) = check_tag_access(AccessLists::of(tag), opts.member_roles.as_deref(), Some(&opts.channel)) {
        let text = if matches!(code, AccessCode::ChannelDenied | AccessCode::ChannelNotAllowed) {
            "This command can't be used in this channel."
        } else {
            "You don't have a role that can use this command."
        };
        tell(error_embed("Not Allowed", Some(text), None)).await;
        return RunResult::early(RunStatus::Denied);
    }

    // 2. Actions exist.
    warn_unknown_actions_format(app, tag);
    let rng = opts.rng.clone().unwrap_or_else(default_rng);
    let plan = plan_run(get_effective_actions(tag), &opts.render, rng.as_ref());
    let steps = plan.steps;
    if steps.is_empty() {
        tell(warn_embed("Nothing to Do", Some("This command has no actions."), None)).await;
        return RunResult::early(RunStatus::Empty);
    }

    // 3. Arguments (the MEE6 usage reply).
    if !plan.missing.is_empty() {
        let usage = build_usage(tag, opts.prefix.as_deref(), slash);
        let text = format!("Usage: `{usage}`{}", tag.description.as_deref().filter(|d| !d.is_empty()).map(|d| format!("\n{d}")).unwrap_or_default());
        let embed = warn_embed("Missing Arguments", Some(&text), None);
        if slash {
            tell(embed).await;
        } else if let Some(m) = &opts.message {
            send_notice(app, io, &guild_id, &opts.user_id, m, embed, &tag.id, opts.check_permissions).await;
        }
        return RunResult::early(RunStatus::Usage);
    }

    // 4. Cooldown (atomic; fails closed when Redis is down).
    let key = tag_cooldown_key(&tag.id, tag.cooldown_scope, i64::from(tag.cooldown_seconds), &guild_id, &opts.user_id);
    if let Some(key) = &key {
        let seconds = i64::from(tag.cooldown_seconds);
        let cd = if slash {
            // The cooldown store must not eat the 3-second acknowledgement window.
            let budget = opts.ack_budget_ms.unwrap_or(2000);
            let (a, k) = (app.clone(), key.clone());
            let fut = async move { Ok::<_, std::convert::Infallible>(acquire_cooldown(&a, &k, seconds).await) };
            bot_util::async_util::with_timeout(fut, budget, "tag cooldown").await.unwrap_or(Cooldown::Unavailable)
        } else {
            acquire_cooldown(app, key, seconds).await
        };
        match cd {
            Cooldown::Ok => {}
            Cooldown::Unavailable => {
                if slash {
                    tell(warn_embed(
                        "Temporarily Unavailable",
                        Some("This command has a cooldown that I can't check right now. Try again in a moment."),
                        None,
                    ))
                    .await;
                } else {
                    warn_once(app, &format!("{guild_id}:cdstore:0"), format!("Custom command cooldown store unavailable in {guild_id}"));
                }
                return RunResult::early(RunStatus::Unavailable);
            }
            Cooldown::Until(until) => {
                let when = format!("<t:{}:R>", (until as f64 / 1000.0).ceil() as i64);
                let text = if tag.cooldown_scope == CooldownScope::Server {
                    format!("This command can be used again {when}.")
                } else {
                    format!("You can use this command again {when}.")
                };
                let embed = warn_embed("Cooldown", Some(&text), None);
                if slash {
                    tell(embed).await;
                } else if let Some(m) = &opts.message {
                    send_notice(app, io, &guild_id, &opts.user_id, m, embed, &tag.id, opts.check_permissions).await;
                }
                return RunResult::early(RunStatus::Cooldown);
            }
        }
    }

    // 5. Execute.
    let mut exec = Exec { io, opts: &opts, tag_name: &tag.name, member_roles: opts.member_roles.clone() };
    let mut results: Vec<Option<StepResult>> = vec![None; steps.len()];
    #[derive(PartialEq)]
    enum Acked {
        Replied,
        Deferred,
        Gone,
    }
    let mut acked = Acked::Gone;
    if slash {
        let actions: Vec<TagAction> = steps.iter().map(|s| s.action.clone()).collect();
        if let Some(p) = channel_reply_index(&actions) {
            let mut primary_done = false;
            match exec.payload_for(&steps[p], Some(&opts.channel.id)) {
                None => results[p] = Some(StepResult::fail(FailureCode::Empty)),
                Some(payload) => match io.interaction_reply(payload.to_payload()).await {
                    Ok(()) => {
                        results[p] = Some(StepResult::ok("💬 Replied in the channel."));
                        acked = Acked::Replied;
                        primary_done = true;
                    }
                    Err(e) => {
                        let r = failure_from_error(&e, StepKind::Reply);
                        if r.code == Some(FailureCode::Expired) {
                            primary_done = true; // nothing can be acknowledged any more
                        }
                        results[p] = Some(r);
                    }
                },
            }
            if !primary_done {
                // Never leave the interaction unacknowledged: the summary goes out through editReply.
                acked = if io.interaction_defer_ephemeral().await { Acked::Deferred } else { Acked::Gone };
            }
        } else {
            acked = if io.interaction_defer_ephemeral().await { Acked::Deferred } else { Acked::Gone };
        }
        for (i, step) in steps.iter().enumerate() {
            if results[i].is_some() {
                continue;
            }
            // After the primary reply further channel replies are new public messages; after a defer they are posted
            // with channel.send, because a follow-up would replace the "thinking" message.
            let how = if acked == Acked::Replied { How::FollowUp } else { How::Channel };
            results[i] = Some(exec.exec_step(step, how).await);
        }
    } else {
        for (i, step) in steps.iter().enumerate() {
            results[i] = Some(exec.exec_step(step, How::Channel).await);
        }
    }
    let results: Vec<StepResult> = results.into_iter().map(|r| r.unwrap_or_else(|| StepResult::fail(FailureCode::DiscordError))).collect();

    // 6. Outcome.
    let success = results.iter().any(|r| r.ok);
    if !success && let Some(key) = &key {
        release_cooldown(app, key).await; // a failed run does not burn the cooldown
    }
    if success {
        increment_tag_uses(app, &tag.id);
    }

    // 7. Report.
    let mut failure_lines: Vec<String> = Vec::new();
    let mut done_lines: Vec<String> = Vec::new();
    for (i, r) in results.iter().enumerate() {
        if r.ok {
            if let Some(s) = &r.summary {
                done_lines.push(s.clone());
            }
            continue;
        }
        failure_lines.push(format!("⚠️ Action {} ({}): {}", i + 1, action_label(&steps[i].action, io.names()), failure_text(r.code, r.detail.as_deref())));
    }
    if slash {
        let sent = match acked {
            Acked::Replied if !failure_lines.is_empty() => {
                let text = bot_util::text::join_with_limit(&failure_lines, "\n", 4000, bot_util::text::default_more);
                io.interaction_follow_up(embed_payload(warn_embed("Some Actions Failed", Some(&text), None)).ephemeral(true)).await
            }
            Acked::Deferred => {
                let mut all = done_lines.clone();
                all.extend(failure_lines.iter().cloned());
                let lines = bot_util::text::join_with_limit(&all, "\n", 4000, bot_util::text::default_more);
                let embed = if failure_lines.is_empty() {
                    success_embed("Done", Some(&lines), None)
                } else {
                    warn_embed("Some Actions Failed", Some(&lines), None)
                };
                io.interaction_edit_reply(embed_payload(embed)).await
            }
            _ => Ok(()),
        };
        if let Err(e) = sent
            && !(e.interaction_gone || e.code.is_some_and(|c| INTERACTION_GONE_CODES.contains(&c)))
        {
            bot_util::log_warn!(format!("Custom command {}: could not report the outcome: {}", tag.name, e.message));
        }
    } else {
        for (i, r) in results.iter().enumerate() {
            if !r.ok {
                let code = r.code.map(FailureCode::as_str).unwrap_or("DISCORD_ERROR");
                warn_once(app, &format!("{guild_id}:cc:{}:{code}", tag.id), format!("Custom command {} ({guild_id}): action {} failed: {code}", tag.name, i + 1));
            }
        }
    }

    // 8. Delete the usage (prefix only, after at least one action worked).
    let mut deleted = false;
    if !slash
        && tag.delete_invocation
        && success
        && let Some(m) = &opts.message
    {
        let can_delete = !opts.check_permissions || io.bot_can_manage_messages(&m.channel_id);
        if !can_delete {
            warn_once(
                app,
                &format!("{guild_id}:ccdel:{}", tag.id),
                format!(
                    "Custom command {} ({guild_id}): I need Manage Messages in #{} to delete usages",
                    tag.name,
                    m.channel_name.clone().unwrap_or_else(|| m.channel_id.clone())
                ),
            );
        } else {
            mark_bot_deletion(app, &[m.id.as_str()], Some("custom-command"), None); // immediately before the delete
            match io.delete_message(&m.channel_id, &m.id).await {
                Ok(()) => deleted = true,
                Err(e) if e.code == Some(10008) => deleted = true, // already gone
                Err(e) => warn_once(
                    app,
                    &format!("{guild_id}:ccdel:{}", tag.id),
                    format!(
                        "Custom command {} ({guild_id}): could not delete the usage ({})",
                        tag.name,
                        e.code.map(|c| c.to_string()).unwrap_or(e.message)
                    ),
                ),
            }
        }
    }
    RunResult { status: RunStatus::Ran, results, success, deleted }
}

// ── The prefix hook ───────────────────────────────────────────────────────────

/// `isPossibleInvocation(ctx)`: the sync filter of the prefix hook (a memoised prefix check, no I/O).
pub fn is_possible_invocation(app: &App, guild_id: &str, content: &str) -> bool {
    if bot_util::js::utf16_len(content) < 2 {
        return false;
    }
    match app.settings().peek_settings(guild_id) {
        None => true,
        Some(s) => effective_prefix(Some(&s.custom_prefix)).is_some_and(|p| content.starts_with(&p)),
    }
}

/// `sendNotice(message, embed, tag)`: prefix usage and cooldown notices, a short reply that deletes itself after
/// `NOTICE_TTL_MS`. One per user per command per 10 seconds; skipped when the bot cannot reply there.
#[allow(clippy::too_many_arguments)]
pub async fn send_notice<I: TagIo>(
    app: &App,
    io: &I,
    guild_id: &str,
    user_id: &str,
    message: &InvokingMessage,
    embed: CreateEmbed,
    tag_id: &str,
    check_permissions: bool,
) {
    if !state(app).notice_throttle.allow(&format!("{guild_id}:{tag_id}:{user_id}")) {
        return;
    }
    if check_permissions && !io.channel_usable(&message.channel_id, &["ViewChannel", "SendMessages", "EmbedLinks", "ReadMessageHistory"]).ok {
        return;
    }
    match io.reply_notice(&message.channel_id, &message.id, embed).await {
        Ok(sent) => io.schedule_delete(&message.channel_id, &sent, NOTICE_TTL_MS),
        Err(e) => bot_util::log_debug!(format!("Custom command notice failed: {}", e.message)),
    }
}

/// `ctx.state.customCommand`: set by the prefix hook whenever the message invoked an existing enabled command
/// (whether or not it then ran); the trigger hook skips such messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommandState {
    pub tag_id: String,
    pub name: String,
}

/// The `ctx.state` key of [`CustomCommandState`].
pub const CUSTOM_COMMAND_STATE: &str = "customCommand";

/// The message of a `messageCreate` dispatch as the hooks see it.
pub struct HookMessage {
    pub guild_id: String,
    pub message: InvokingMessage,
    pub content: String,
    pub author_id: String,
    pub channel: ChannelScope,
    pub member_roles: Option<Vec<String>>,
}

impl HookMessage {
    /// From the dispatch (the channel's scope from the serenity cache).
    pub fn of(app: &App, data: &MessageCreate) -> Self {
        let gid = data.guild_id.to_string();
        // Read under the cache lock instead of cloning the whole guild (members included) for every message.
        let view = |guild: Option<&Guild>| {
            let channel_obj: Option<&GuildChannel> = data.channel.as_ref().or_else(|| guild.and_then(|g| bot_core::permissions::find_channel(g, data.channel_id)));
            let channel = match channel_obj {
                Some(c) => ChannelScope::of(guild, c),
                None => ChannelScope::bare(&data.channel_id.to_string()),
            };
            let roles = data.member.as_ref().map(|m| member_role_ids(guild, &gid, &m.roles));
            (channel_obj.map(|c| c.name.clone()), channel, roles)
        };
        let (channel_name, channel, member_roles) = bot_core::interaction_helpers::with_cached_guild(app, data.guild_id, |g| view(Some(g))).unwrap_or_else(|| view(None));
        Self {
            guild_id: gid.clone(),
            message: InvokingMessage { id: data.message.id.to_string(), channel_id: data.channel_id.to_string(), channel_name },
            content: data.message.content.clone(),
            author_id: data.message.author.id.to_string(),
            channel,
            member_roles,
        }
    }
}

fn render_ctx_of(app: &App, data: &MessageCreate) -> RenderContext {
    bot_core::adapters::render_context(app, Some(data.guild_id), &data.message.author, data.member.as_ref(), Some(data.channel_id))
}

/// `handlePrefixInvocation(ctx, { checkPermissions })`: the body of the `custom-commands` hook. Never fails. Calls
/// `ctx.stop()` only after the invocation message was deleted, so later hooks (levels, triggers, translation) skip a
/// message that is gone.
pub async fn handle_prefix_invocation<I: TagIo>(ctx: &HookCtx<MessageCreate>, io: &I, check_permissions: bool) {
    let app = &ctx.app;
    let m = HookMessage::of(app, &ctx.data);
    let guild_id = m.guild_id.clone();
    if was_deleted_by_bot(app, &m.message.id).is_some() {
        return;
    }
    let Some(settings) = ctx.get_settings().await else { return };
    let Some(prefix) = effective_prefix(Some(&settings.custom_prefix)) else { return }; // prefix commands are off
    let Some(inv) = parse_invocation(&m.content, Some(&prefix)) else { return };
    // Unknown names are silent: other bots share prefixes.
    if !has_tag_name(app, &guild_id, &inv.name).await {
        return;
    }
    let Some(tag) = find_tag_row(app, &guild_id, &inv.name).await else { return };
    if !tag.enabled {
        return;
    }
    ctx.state.insert(CUSTOM_COMMAND_STATE, CustomCommandState { tag_id: tag.id.clone(), name: tag.name.clone() });
    if !state(app).prefix_throttle.allow(&format!("{guild_id}:{}", m.author_id)) {
        return; // silent
    }
    let opts = RunOpts {
        source: Source::Prefix,
        guild_id: guild_id.clone(),
        channel: m.channel.clone(),
        member_roles: m.member_roles.clone(),
        user_id: m.author_id.clone(),
        args: inv.args,
        settings: Some(&settings),
        prefix: Some(prefix),
        render: render_ctx_of(app, &ctx.data),
        rng: None,
        check_permissions,
        ack_budget_ms: None,
        message: Some(m.message.clone()),
    };
    let res = run_tag(app, &tag, opts, io).await;
    if res.deleted {
        ctx.stop("custom-commands: usage deleted");
    }
}

// ── Triggers: validation, cache, compilation, selection ───────────────────────

/// `validateTriggerPattern(pattern, matchType, caseSensitive)`: write-time validation. REGEX patterns go through
/// `validate_regex` (nested quantifiers, backreferences, more than 200 characters, syntax).
pub fn validate_trigger_pattern(pattern: &str, match_type: &str, case_sensitive: bool) -> Result<(), String> {
    if bot_util::js::trim(pattern).is_empty() {
        return Err("The pattern is empty.".into());
    }
    if bot_util::js::utf16_len(pattern) > TRIGGER_PATTERN_MAX {
        return Err(format!("The pattern can be at most {TRIGGER_PATTERN_MAX} characters."));
    }
    if match_type == "REGEX" {
        return validate_regex(pattern, if case_sensitive { "" } else { "i" });
    }
    if !MATCH_TYPES.contains(&match_type) {
        return Err("Unknown match type.".into());
    }
    Ok(())
}

fn int_from_number<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    let v = Value::deserialize(d)?;
    match &v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f as i64)).ok_or_else(|| serde::de::Error::custom("not a number")),
        _ => Err(serde::de::Error::custom("not a number")),
    }
}

/// The cached shape of one trigger (`toProjection(row)`), as stored in `guild:{g}:triggers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TriggerProjection {
    pub id: String,
    pub trigger: String,
    pub response: String,
    pub match_type: String,
    pub case_sensitive: bool,
    #[serde(deserialize_with = "int_from_number")]
    pub cooldown: i64,
    pub delete_message: bool,
    #[serde(default)]
    pub channel_ids: Vec<String>,
    #[serde(default)]
    pub exempt_role_ids: Vec<String>,
    /// `updatedAt` as an ISO string: the compiled-regex memo key.
    pub v: String,
}

impl TriggerProjection {
    /// `toProjection(row)`.
    pub fn of(row: &AutoTrigger) -> Self {
        Self {
            id: row.id.clone(),
            trigger: row.trigger.clone(),
            response: row.response.clone(),
            match_type: row.match_type.as_str().to_string(),
            case_sensitive: row.case_sensitive,
            cooldown: i64::from(row.cooldown),
            delete_message: row.delete_message,
            channel_ids: row.channel_ids.clone(),
            exempt_role_ids: row.exempt_role_ids.clone(),
            v: bot_db::format_js_date(&row.updated_at),
        }
    }
}

/// One trigger of a guild with its compiled regex (`{ t, re }`).
#[derive(Debug, Clone)]
pub struct TriggerEntry {
    pub t: TriggerProjection,
    pub re: Option<Arc<SafeRegex>>,
}

/// `compiledFor(t, guildId)`: the compiled regex of a REGEX projection, memoised per id + updatedAt. `None` for unsafe
/// or invalid patterns (legacy rows created before validation), which therefore never run.
pub fn compiled_for(app: &App, t: &TriggerProjection, guild_id: &str) -> Option<Arc<SafeRegex>> {
    let s = state(app);
    let key = format!("{}:{}:{}", t.id, t.v, if t.case_sensitive { 1 } else { 0 });
    if let Some(hit) = lock(&s.compiled).0.get(&key) {
        return hit.clone();
    }
    let re = compile_safe_regex(&t.trigger, if t.case_sensitive { "" } else { "i" });
    if re.is_none() {
        warn_once(app, &format!("{guild_id}:trigger:{}", t.id), format!("Trigger {} in {guild_id} has an unsafe or invalid regex and is skipped", t.id));
    }
    let mut g = lock(&s.compiled);
    let (map, order) = &mut *g;
    if map.insert(key.clone(), re.clone()).is_none() {
        order.push_back(key);
    }
    while map.len() > COMPILED_MAX {
        let Some(old) = order.pop_front() else { break };
        map.remove(&old);
    }
    re
}

/// `guild:{g}:triggers`.
pub fn trigger_cache_key(guild_id: &str) -> String {
    format!("guild:{guild_id}:triggers")
}

/// `invalidateTriggerCache(guildId)`: drops the memo and the Redis list of a guild; called after every trigger write.
pub async fn invalidate_trigger_cache(app: &App, guild_id: &str) {
    lock(&state(app).trigger_memo).remove(guild_id);
    app.cache().del_one(&trigger_cache_key(guild_id)).await;
}

fn make_entries(app: &App, projections: Vec<TriggerProjection>, guild_id: &str) -> Vec<TriggerEntry> {
    projections
        .into_iter()
        .map(|t| {
            let re = if t.match_type == "REGEX" { compiled_for(app, &t, guild_id) } else { None };
            TriggerEntry { t, re }
        })
        .collect()
}

/// Reads a cached projection list; legacy-shaped values (full rows without `v`) and anything else count as a miss.
pub fn parse_cached_projections(v: &Value) -> Option<Vec<TriggerProjection>> {
    let list = v.as_array()?;
    if !list.iter().all(|t| t.get("v").is_some_and(Value::is_string)) {
        return None;
    }
    list.iter().map(|t| serde_json::from_value::<TriggerProjection>(t.clone()).ok()).collect()
}

/// The enabled triggers of a guild in creation order (`prisma.autoTrigger.findMany({ where: { guildId, enabled: true } })`).
pub async fn load_enabled_triggers(app: &App, guild_id: &str) -> anyhow::Result<Vec<AutoTrigger>> {
    Ok(SelectBuilder::new(&AUTO_TRIGGER_INFO)
        .filter("guildId", json!(guild_id))?
        .filter("enabled", json!(true))?
        .order_by("createdAt", true)?
        .order_by("id", true)?
        .build()?
        .fetch_all(app.db())
        .await?)
}

/// `getTriggerSet(guildId)`: the enabled triggers of a guild in creation order, with their compiled regexes. Memo,
/// then Redis (fail open: a miss falls through to PostgreSQL), then PostgreSQL. Never fails; `None` = triggers stay
/// silent.
pub async fn get_trigger_set(app: &App, guild_id: &str) -> Option<Arc<Vec<TriggerEntry>>> {
    let s = state(app);
    if let Some(hit) = lock(&s.trigger_memo).get(guild_id, app.clock().now_ms()) {
        return Some(hit);
    }
    let key = trigger_cache_key(guild_id);
    let mut projections = app.cache().get(&key).await.and_then(|v| parse_cached_projections(&v));
    if projections.is_none() {
        match load_enabled_triggers(app, guild_id).await {
            Ok(rows) => {
                let p: Vec<TriggerProjection> = rows.iter().map(TriggerProjection::of).collect();
                app.cache().set(&key, &p, TRIGGER_CACHE_TTL).await;
                projections = Some(p);
            }
            Err(e) => {
                warn_once(app, &format!("{guild_id}:triggerset:0"), format!("Triggers: could not load the triggers of {guild_id}: {e}"));
                return None;
            }
        }
    }
    let entries = Arc::new(make_entries(app, projections.unwrap_or_default(), guild_id));
    lock(&s.trigger_memo).put(guild_id, entries.clone(), app.clock().now_ms());
    Some(entries)
}

/// `matchesPlain(t, content)`: non-regex matching. EXACT compares trimmed text (trailing spaces are invisible).
pub fn matches_plain(t: &TriggerProjection, content: &str) -> bool {
    let (s, p) = if t.case_sensitive { (content.to_string(), t.trigger.clone()) } else { (bot_util::js::lower(content), bot_util::js::lower(&t.trigger)) };
    match t.match_type.as_str() {
        "EXACT" => bot_util::js::trim(&s) == bot_util::js::trim(&p),
        "CONTAINS" => s.contains(&p),
        "STARTS_WITH" => s.starts_with(&p),
        "ENDS_WITH" => s.ends_with(&p),
        _ => false,
    }
}

/// `selectTrigger(entries, content, scope)`: the first applicable matching trigger in creation order. Cheap scope
/// checks come first, plain matches are string operations, and every candidate regex runs in ONE batched,
/// time-limited call.
pub fn select_trigger<'a>(entries: &'a [TriggerEntry], content: &str, channel: Option<&ChannelScope>, member_roles: Option<&[String]>) -> Option<&'a TriggerProjection> {
    let mut first_plain: Option<usize> = None;
    let mut regex_cands: Vec<usize> = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        if !trigger_applies_here(&e.t, channel, member_roles) {
            continue;
        }
        if e.t.match_type == "REGEX" {
            if e.re.is_some() {
                regex_cands.push(i);
            }
            continue;
        }
        if matches_plain(&e.t, content) {
            first_plain = Some(i);
            break;
        }
    }
    if !regex_cands.is_empty() {
        // Only regexes created before the first plain match can win.
        let list: Vec<Option<Arc<SafeRegex>>> = regex_cands.iter().map(|i| entries[*i].re.clone()).collect();
        let k = safe_regex_first_match(&list, content, None);
        if k >= 0 {
            return Some(&entries[regex_cands[k as usize]].t);
        }
    }
    first_plain.map(|i| &entries[i].t)
}

/// `triggerQuarantined(row)`: true when the regex of a trigger timed out recently and is being skipped.
pub fn trigger_quarantined(app: &App, row: &AutoTrigger) -> bool {
    compiled_for(app, &TriggerProjection::of(row), &row.guild_id).is_some_and(|re| is_quarantined(&re))
}

// ── The trigger hook ──────────────────────────────────────────────────────────

/// Whether the guild has enabled triggers, from the memo only (`None`: not loaded yet). Lets the hook skip building
/// its Discord view for guilds without triggers.
pub fn memoised_trigger_count(app: &App, guild_id: &str) -> Option<usize> {
    lock(&state(app).trigger_memo).get(guild_id, app.clock().now_ms()).map(|s| s.len())
}

/// `hasTriggerContent(ctx)`: the sync filter of the trigger hook: there is text, no custom command already answered
/// this message, and triggers are not switched off in the memoised settings.
pub fn has_trigger_content(app: &App, guild_id: &str, content: &str, answered: bool) -> bool {
    if content.is_empty() || answered {
        return false;
    }
    !app.settings().peek_settings(guild_id).is_some_and(|s| !s.auto_triggers_enabled)
}

/// `handleTriggers(ctx, { checkPermissions })`: the body of the `custom-commands:triggers` hook. Never fails. Never
/// calls `ctx.stop()`: a deleted trigger message is skipped by later hooks through `was_deleted_by_bot`.
pub async fn handle_triggers<I: TagIo>(ctx: &HookCtx<MessageCreate>, io: &I, check_permissions: bool) {
    let app = &ctx.app;
    let guild_id = ctx.data.guild_id.to_string();
    if ctx.state.contains(CUSTOM_COMMAND_STATE) || was_deleted_by_bot(app, &ctx.data.message.id.to_string()).is_some() {
        return;
    }
    // Triggers have their own switch next to the module flag (the runner already checked the module).
    let settings = ctx.get_settings().await;
    if settings.as_ref().is_some_and(|s| !s.auto_triggers_enabled) {
        return;
    }
    let Some(set) = get_trigger_set(app, &guild_id).await else { return };
    if set.is_empty() {
        return;
    }
    // Only now the cached guild is read (scope ids, member roles): most messages stop above.
    let m = HookMessage::of(app, &ctx.data);
    let Some(t) = select_trigger(&set, &m.content, Some(&m.channel), m.member_roles.as_deref()).cloned() else { return };
    if !state(app).trigger_channel_throttle.allow(&m.message.channel_id) {
        return;
    }
    if check_permissions {
        let usable = io.channel_usable(&m.message.channel_id, &["ViewChannel", "SendMessages"]);
        if !usable.ok {
            warn_once(
                app,
                &format!("{guild_id}:trigperm:{}", m.message.channel_id),
                format!("Trigger {} in {guild_id}: {}", t.id, usable.reason.unwrap_or_else(|| "cannot send".into())),
            );
            return;
        }
    }

    // The matching trigger on cooldown ends the scan: no other trigger fires. Fails closed when Redis is down.
    let mut cd_key: Option<String> = None;
    if t.cooldown > 0 {
        let key = trigger_cooldown_key(&guild_id, &t.id, &m.author_id);
        if acquire_cooldown(app, &key, t.cooldown).await != Cooldown::Ok {
            return;
        }
        cd_key = Some(key);
    }

    let render = render_ctx_of(app, &ctx.data);
    let payload = render_template(
        &json!({ "content": t.response }),
        &render,
        RenderOptions { allowed_mentions: Some(user_mentions(&[])), ..Default::default() },
    );
    let Some(payload) = payload else {
        if let Some(k) = &cd_key {
            release_cooldown(app, k).await;
        }
        return;
    };
    if let Err(e) = io.send_channel(&m.message.channel_id, payload.to_payload()).await {
        if let Some(k) = &cd_key {
            release_cooldown(app, k).await;
        }
        warn_once(
            app,
            &format!("{guild_id}:trigsend:{}", t.id),
            format!("Trigger {} in {guild_id}: send failed ({})", t.id, e.code.map(|c| c.to_string()).unwrap_or(e.message)),
        );
        return; // a failed send never deletes the member's message
    }
    increment_trigger_uses(app, &t.id);

    if t.delete_message {
        let can_delete = !check_permissions || io.bot_can_manage_messages(&m.message.channel_id);
        if !can_delete {
            warn_once(app, &format!("{guild_id}:trigdel:{}", t.id), format!("Trigger {} in {guild_id}: I need Manage Messages to delete the matching message", t.id));
        } else {
            mark_bot_deletion(app, &[m.message.id.as_str()], Some("trigger"), None);
            if let Err(e) = io.delete_message(&m.message.channel_id, &m.message.id).await
                && e.code != Some(10008)
            {
                warn_once(
                    app,
                    &format!("{guild_id}:trigdel:{}", t.id),
                    format!("Trigger {} in {guild_id}: could not delete the message ({})", t.id, e.code.map(|c| c.to_string()).unwrap_or(e.message)),
                );
            }
        }
    }
}

// ── Resolving a trigger reference ─────────────────────────────────────────────

/// Every trigger of a guild in creation order (`/tag trigger list`).
pub async fn list_triggers(app: &App, guild_id: &str) -> anyhow::Result<Vec<AutoTrigger>> {
    Ok(SelectBuilder::new(&AUTO_TRIGGER_INFO).filter("guildId", json!(guild_id))?.order_by("createdAt", true)?.order_by("id", true)?.build()?.fetch_all(app.db()).await?)
}

/// A trigger by id (`prisma.autoTrigger.findUnique({ where: { id } })`).
pub async fn find_trigger_by_id(app: &App, id: &str) -> anyhow::Result<Option<AutoTrigger>> {
    Ok(SelectBuilder::new(&AUTO_TRIGGER_INFO).filter("id", json!(id))?.build()?.fetch_optional(app.db()).await?)
}

/// `resolveTrigger(guildId, input)`: `#3` / `3` (the number `/tag trigger list` shows), an ID (autocomplete value) or
/// an exact pattern (case-insensitive). Fails with `TagError` NOT_FOUND or AMBIGUOUS.
pub async fn resolve_trigger(app: &App, guild_id: &str, input: &str) -> anyhow::Result<AutoTrigger> {
    let s = bot_util::js::trim(input).to_string();
    let digits = s.strip_prefix('#').unwrap_or(&s);
    if (1..=4).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit()) {
        let n: i64 = digits.parse().unwrap_or(0);
        if n >= 1 {
            let sql = r#"SELECT * FROM "AutoTrigger" WHERE "guildId" = $1 ORDER BY "createdAt" ASC, "id" ASC LIMIT 1 OFFSET $2"#;
            let row: Option<AutoTrigger> = sqlx::query_as(sql).bind(guild_id).bind(n - 1).fetch_optional(app.db()).await?;
            if let Some(r) = row {
                return Ok(r);
            }
        }
    }
    if !s.is_empty() {
        let by_id: Option<AutoTrigger> =
            SelectBuilder::new(&AUTO_TRIGGER_INFO).filter("id", json!(s))?.filter("guildId", json!(guild_id))?.build()?.fetch_optional(app.db()).await?;
        if let Some(r) = by_id {
            return Ok(r);
        }
        // Prisma's `equals` with `mode: 'insensitive'` is `ILIKE` with the text unescaped, and `take` orders by id.
        let sql = r#"SELECT * FROM "AutoTrigger" WHERE "guildId" = $1 AND "trigger" ILIKE $2 ORDER BY "id" ASC LIMIT 2"#;
        let rows: Vec<AutoTrigger> = sqlx::query_as(sql).bind(guild_id).bind(&s).fetch_all(app.db()).await?;
        if rows.len() == 1 {
            return rows.into_iter().next().ok_or_else(|| anyhow::anyhow!("unreachable"));
        }
        if rows.len() == 2 {
            return Err(TagError::new("AMBIGUOUS", "Several Matches", "Several triggers use that pattern; use its #number or ID from /tag trigger list.").into());
        }
    }
    Err(TagError::new(
        "NOT_FOUND",
        "Not Found",
        format!("No trigger matches `{}`. Use /tag trigger list to see numbers and IDs.", code_safe(&bot_util::text::clamp_text(&s, 80))),
    )
    .into())
}

/// `triggerNumber(guildId, row)`: the `#n` of a trigger: the count of rows ordered up to and including it.
pub async fn trigger_number(app: &App, guild_id: &str, row: &AutoTrigger) -> anyhow::Result<i64> {
    let sql = r#"SELECT COUNT(*)::bigint FROM "AutoTrigger" WHERE "guildId" = $1 AND ("createdAt" < $2 OR ("createdAt" = $2 AND "id" <= $3))"#;
    let (n,): (i64,) = sqlx::query_as(sql).bind(guild_id).bind(row.created_at.naive_utc()).bind(&row.id).fetch_one(app.db()).await?;
    Ok(n)
}

// ── Writes, caps and concurrency ──────────────────────────────────────────────

/// The fields of a new custom command (`createTagRow(data)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTag {
    pub guild_id: String,
    pub name: String,
    pub content: String,
    pub use_embed: bool,
    pub description: Option<String>,
    pub created_by: String,
}

/// `createTagRow(data)`: creates a `CustomTag` under a per-guild advisory lock so concurrent creates cannot pass the
/// cap together. A duplicate name comes back as the `DbError` (`is_unique_violation`); the cap as `TagError` LIMIT.
pub async fn create_tag_row(app: &App, data: NewTag) -> anyhow::Result<CustomTag> {
    let fields: Patch = vec![
        ("name", json!(data.name)),
        ("content", json!(data.content)),
        ("useEmbed", json!(data.use_embed)),
        ("description", json!(data.description)),
        ("createdBy", json!(data.created_by)),
    ];
    create_tag_row_with(app, &data.guild_id, fields).await
}

/// `createTagRow({ guildId, ...fields })` with any `CustomTag` columns (Prisma field names), as the dashboard calls it.
pub async fn create_tag_row_with(app: &App, guild_id: &str, fields: Patch) -> anyhow::Result<CustomTag> {
    let q = InsertBuilder::new(&CUSTOM_TAG_INFO).value("guildId", json!(guild_id))?.values(fields)?.build()?;
    let gid = guild_id.to_string();
    bot_db::transaction(app.db(), bot_db::TxOptions::default(), move |tx| {
        Box::pin(async move {
            sqlx::query(LOCK_SQL).bind(format!("customtag:{gid}")).execute(&mut *tx).await.map_err(DbError::from)?;
            let (n,): (i64,) = sqlx::query_as(r#"SELECT COUNT(*)::bigint FROM "CustomTag" WHERE "guildId" = $1"#).bind(&gid).fetch_one(&mut *tx).await.map_err(DbError::from)?;
            if n >= limits::CUSTOM_TAGS as i64 {
                return Err(anyhow::Error::from(TagError::new(
                    "LIMIT",
                    "Limit Reached",
                    format!("This server already has {} custom commands, the maximum. Delete one with `/tag delete` first.", limits::CUSTOM_TAGS),
                )));
            }
            Ok(q.fetch_one::<CustomTag, _>(&mut *tx).await?)
        })
    })
    .await
}

/// The fields of a new trigger (`createTriggerRow(data)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTrigger {
    pub guild_id: String,
    pub trigger: String,
    pub response: String,
    pub match_type: String,
    pub cooldown: i64,
    pub delete_message: bool,
    pub case_sensitive: bool,
    pub channel_ids: Vec<String>,
    pub exempt_role_ids: Vec<String>,
    pub created_by: String,
}

/// `createTriggerRow(data)`: like [`create_tag_row`], for triggers (`TagError` LIMIT).
pub async fn create_trigger_row(app: &App, data: NewTrigger) -> anyhow::Result<AutoTrigger> {
    let fields: Patch = vec![
        ("trigger", json!(data.trigger)),
        ("response", json!(data.response)),
        ("matchType", json!(data.match_type)),
        ("cooldown", json!(data.cooldown)),
        ("deleteMessage", json!(data.delete_message)),
        ("caseSensitive", json!(data.case_sensitive)),
        ("channelIds", json!(data.channel_ids)),
        ("exemptRoleIds", json!(data.exempt_role_ids)),
        ("createdBy", json!(data.created_by)),
    ];
    create_trigger_row_with(app, &data.guild_id, fields).await
}

/// `createTriggerRow({ guildId, ...fields })` with any `AutoTrigger` columns, as the dashboard calls it.
pub async fn create_trigger_row_with(app: &App, guild_id: &str, fields: Patch) -> anyhow::Result<AutoTrigger> {
    let q = InsertBuilder::new(&AUTO_TRIGGER_INFO).value("guildId", json!(guild_id))?.values(fields)?.build()?;
    let gid = guild_id.to_string();
    bot_db::transaction(app.db(), bot_db::TxOptions::default(), move |tx| {
        Box::pin(async move {
            sqlx::query(LOCK_SQL).bind(format!("autotrigger:{gid}")).execute(&mut *tx).await.map_err(DbError::from)?;
            let (n,): (i64,) = sqlx::query_as(r#"SELECT COUNT(*)::bigint FROM "AutoTrigger" WHERE "guildId" = $1"#).bind(&gid).fetch_one(&mut *tx).await.map_err(DbError::from)?;
            if n >= limits::AUTO_TRIGGERS as i64 {
                return Err(anyhow::Error::from(TagError::new(
                    "LIMIT",
                    "Limit Reached",
                    format!("This server already has {} triggers, the maximum. Remove one with `/tag trigger remove`.", limits::AUTO_TRIGGERS),
                )));
            }
            Ok(q.fetch_one::<AutoTrigger, _>(&mut *tx).await?)
        })
    })
    .await
}

/// The fields a write changes: Prisma field names and the JSON values the JS passed.
pub type Patch = Vec<(&'static str, Value)>;

/// `writeTag(guildId, rawName, mutator)`: read-modify-write of one custom command with optimistic concurrency
/// (`updatedAt` is the token), so two admins never lose each other's change to the JSON or array fields. Two
/// attempts. The mutator is pure and returns the fields to change (empty: nothing to write, the row comes back
/// unchanged); its `TagError` comes back unchanged. Fails with `TagError` NOT_FOUND or CONFLICT.
pub async fn write_tag<F>(app: &App, guild_id: &str, raw_name: &str, mut mutator: F) -> anyhow::Result<CustomTag>
where
    F: FnMut(&CustomTag) -> Result<Patch, TagError> + Send,
{
    for _ in 0..2 {
        let Some(tag) = find_tag(app, guild_id, raw_name).await? else {
            return Err(TagError::new("NOT_FOUND", "Not Found", format!("No custom command named `{}` exists here.", code_safe(&normalize_tag_name(raw_name)))).into());
        };
        let data = mutator(&tag)?;
        if data.is_empty() {
            return Ok(tag);
        }
        let count = UpdateBuilder::new(&CUSTOM_TAG_INFO)
            .set_all(data)?
            .filter("id", json!(tag.id))?
            .filter("updatedAt", json!(bot_db::format_js_date(&tag.updated_at)))?
            .without_returning()
            .build()?
            .execute(app.db())
            .await?;
        if count == 1 {
            invalidate_tag_index(app, guild_id);
            return find_tag_by_id(app, &tag.id).await?.ok_or_else(|| TagError::new("NOT_FOUND", "Not Found", "That custom command was deleted meanwhile.").into());
        }
    }
    Err(TagError::new("CONFLICT", "Changed Meanwhile", "Someone else changed this command. Run the command again.").into())
}

/// `writeTrigger(guildId, row, data)`: optimistic update of one trigger; the admin's options were computed from
/// `row`, so there is no retry. Fails with `TagError` CONFLICT.
pub async fn write_trigger(app: &App, guild_id: &str, row: &AutoTrigger, data: Patch) -> anyhow::Result<AutoTrigger> {
    let count = UpdateBuilder::new(&AUTO_TRIGGER_INFO)
        .set_all(data)?
        .filter("id", json!(row.id))?
        .filter("guildId", json!(guild_id))?
        .filter("updatedAt", json!(bot_db::format_js_date(&row.updated_at)))?
        .without_returning()
        .build()?
        .execute(app.db())
        .await?;
    if count == 0 {
        return Err(TagError::new("CONFLICT", "Changed Meanwhile", "Someone else changed this trigger. Run the command again.").into());
    }
    find_trigger_by_id(app, &row.id).await?.ok_or_else(|| TagError::new("CONFLICT", "Changed Meanwhile", "Someone else changed this trigger. Run the command again.").into())
}

/// `prisma.customTag.delete({ where: { id } })`: false when the row was already gone (P2025).
pub async fn delete_tag_row(app: &App, id: &str) -> anyhow::Result<bool> {
    Ok(bot_db::delete_where(&CUSTOM_TAG_INFO, [("id", json!(id))])?.execute(app.db()).await? > 0)
}

/// `prisma.autoTrigger.delete({ where: { id } })`: false when the row was already gone (P2025).
pub async fn delete_trigger_row(app: &App, id: &str) -> anyhow::Result<bool> {
    Ok(bot_db::delete_where(&AUTO_TRIGGER_INFO, [("id", json!(id))])?.execute(app.db()).await? > 0)
}

/// A `TagError` inside an `anyhow::Error`.
pub fn as_tag_error(err: &anyhow::Error) -> Option<&TagError> {
    err.downcast_ref::<TagError>()
}

/// A unique-violation (Prisma P2002) inside an `anyhow::Error`.
pub fn is_unique_violation(err: &anyhow::Error) -> bool {
    err.downcast_ref::<DbError>().is_some_and(DbError::is_unique_violation)
}
