//! Port of `src/dashboard/discordApi.js`: Discord REST access for the dashboard. The logged-in user's profile and guild
//! list (user token, cached), whether the bot is in a guild, a guild's channels, roles and emojis, message writes
//! (bot token), the invite URL and CDN image URLs.
//!
//! Every call has a timeout and every cache is bounded. The API base URL, the OAuth endpoints, the clock and the pause
//! before a rate-limited bot-guild page is retried are injectable ([`DiscordApiOptions`]), so tests point the client
//! at a local stub.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use dash_auth::auth::{OAuthClient, OAuthCredentials, TokenGrant};
use dash_collections::discord::{
    DashboardDiscord, DiscordApiError, DiscordResources, InviteRef, MemberProfile, MemberRoles, MessageRef, ResBot, ResChannel, ResEmoji,
    ResGuild, ResRole,
};
use futures::future::{BoxFuture, FutureExt, Shared};
use futures::{StreamExt, TryStreamExt};
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::collate::name_compare;
use crate::logger::DashLogger;
use crate::util::{compare_ids, form_encode, is_snowflake, js_bigint, js_int};

/// `RouteBases.api`.
pub const API_BASE: &str = "https://discord.com/api/v10";
/// `RouteBases.cdn`.
pub const CDN_BASE: &str = "https://cdn.discordapp.com";
pub const REQUEST_TIMEOUT_MS: u64 = 10_000;
pub const USER_GUILDS_TTL_MS: f64 = 60_000.0;
pub const USER_GUILDS_MAX_STALE_MS: f64 = 5.0 * 60_000.0;
pub const USER_GUILDS_CACHE_MAX: usize = 5_000;
/// An entry this old is returned and refreshed in the background (never served past USER_GUILDS_TTL_MS).
pub const USER_GUILDS_REFRESH_AHEAD_MS: f64 = USER_GUILDS_TTL_MS / 2.0;
/// After a failed background refresh, the next one waits this long.
pub const USER_GUILDS_REFRESH_RETRY_MS: f64 = 10_000.0;
pub const BOT_GUILDS_TTL_MS: f64 = 60_000.0;
/// A bot list this old is returned and refreshed in the background (never served past BOT_GUILDS_TTL_MS).
pub const BOT_GUILDS_REFRESH_AHEAD_MS: f64 = BOT_GUILDS_TTL_MS / 2.0;
/// After a failed background refresh-ahead, the next one waits this long.
pub const BOT_GUILDS_REFRESH_RETRY_MS: f64 = 10_000.0;
/// With `swr`, the bot's guild list is served up to this old while it refreshes in the background.
pub const BOT_GUILDS_MAX_STALE_MS: f64 = 10.0 * 60_000.0;
pub const BOT_GUILDS_MAX_PAGES: usize = 100;
/// How long a bot-owner invite lasts (it also works only once).
pub const OWNER_INVITE_MAX_AGE_SECONDS: u64 = 1800;
pub const BOT_GUILDS_MAX_RETRIES: u32 = 3;
pub const BOT_GUILDS_MAX_WAIT_SECONDS: u64 = 10;
pub const RESOURCES_TTL_MS: f64 = 30_000.0;
pub const RESOURCES_CACHE_MAX: usize = 500;
/// With `swr`, a guild's resources are served up to this old while they reload in the background.
pub const RESOURCES_MAX_STALE_MS: f64 = 10.0 * 60_000.0;
pub const USERS_TTL_MS: f64 = 60.0 * 60_000.0;
pub const USERS_CACHE_MAX: usize = 5_000;
pub const MEMBER_SEARCH_MAX: usize = 25;
const USER_LOOKUP_CONCURRENCY: usize = 4;
/// The User-Agent Node's fetch sends (the JS client sets none of its own).
pub const USER_AGENT: &str = "node";
const BOT_MISS_TTL_MS: f64 = 5_000.0;
const BOT_MISS_MAX: usize = 1_000;

/// What the invite asks for: what the modules use (README "Setup"), never Administrator. `INVITE_PERMISSIONS`.
pub const INVITE_PERMISSIONS: u64 = 1_393_066_110_199;
/// Permissions a role the bot hands out must not carry (`DANGEROUS_PERMISSIONS`).
pub const DANGEROUS_PERMISSIONS: i128 = 1_100_317_073_470;
const ADMINISTRATOR: i128 = 8;
const MANAGE_ROLES: i128 = 268_435_456;
const CREATE_INSTANT_INVITE: i128 = 1;

fn is_image_hash(s: &str) -> bool {
    let h = s.strip_prefix("a_").unwrap_or(s);
    h.len() == 32 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_invite_code(s: &str) -> bool {
    (2..=32).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn api_error(message: impl Into<String>, code: &str) -> DiscordApiError {
    DiscordApiError::new(message, 0, code)
}

fn code_for_status(status: u16) -> &'static str {
    match status {
        401 => "unauthenticated",
        403 => "forbidden",
        404 => "not_found",
        429 => "rate_limited",
        _ => "unavailable",
    }
}

/// `retryAfterSeconds(res, body)`: whole seconds, 1..3600, from the body's `retry_after`, else the header.
fn retry_after_seconds(header: Option<&str>, body: &Value) -> u64 {
    let from_body = dash_schema::js::to_number(body.as_object().and_then(|o| o.get("retry_after")));
    let from_header = match header {
        Some(h) => dash_schema::js::to_number(Some(&Value::String(h.to_string()))),
        None => 0.0,
    };
    let seconds = if from_body.is_finite() && from_body > 0.0 {
        from_body
    } else if from_header.is_finite() && from_header > 0.0 {
        from_header
    } else {
        1.0
    };
    seconds.ceil().clamp(1.0, 3600.0) as u64
}

/// The avatar URL of a user (the default avatar when there is no custom one). `userAvatarUrl`.
pub fn user_avatar_url(id: Option<&str>, avatar: Option<&str>) -> String {
    let id = id.filter(|s| is_snowflake(s)).unwrap_or("0");
    if let Some(hash) = avatar.filter(|a| is_image_hash(a)) {
        return format!("{CDN_BASE}/avatars/{id}/{hash}.webp?size=128");
    }
    let n: u128 = id.parse().unwrap_or(0);
    format!("{CDN_BASE}/embed/avatars/{}.png", (n >> 22) % 6)
}

/// The icon URL of a guild, or None when it has none. `guildIconUrl`.
pub fn guild_icon_url(id: Option<&str>, icon: Option<&str>) -> Option<String> {
    let id = id.filter(|s| is_snowflake(s))?;
    let icon = icon.filter(|i| is_image_hash(i))?;
    Some(format!("{CDN_BASE}/icons/{id}/{icon}.webp?size=128"))
}

/// One entry of the user's OAuth guild list (`normalizeGuild`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthGuild {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
    pub owner: bool,
    pub permissions: String,
}

impl OAuthGuild {
    /// The JSON `canManageGuild` reads.
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// `normalizeGuild(g)`.
pub fn normalize_guild(g: &Value) -> Option<OAuthGuild> {
    let o = g.as_object()?;
    let id = o.get("id")?.as_str().filter(|s| is_snowflake(s))?;
    Some(OAuthGuild {
        id: id.to_string(),
        name: o.get("name").and_then(Value::as_str).unwrap_or("Unknown server").to_string(),
        icon: o.get("icon").and_then(Value::as_str).map(str::to_string),
        owner: o.get("owner") == Some(&Value::Bool(true)),
        permissions: o.get("permissions").and_then(Value::as_str).unwrap_or("0").to_string(),
    })
}

/// One server the bot is in (`getBotGuilds`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotGuild {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
}

/// The logged-in user (`getCurrentUser`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentUser {
    pub id: String,
    pub username: String,
    pub global_name: Option<String>,
    pub avatar: Option<String>,
}

const VOICE_TYPES: [i64; 2] = [2, 13];

/// Channels in Discord's sidebar order: channels without a category first, then each category followed by its
/// channels; inside a parent, text-like channels before voice-like ones, then by position and id. `sortChannels`.
pub fn sort_channels(channels: Vec<ResChannel>) -> Vec<ResChannel> {
    let in_parent = |a: &ResChannel, b: &ResChannel| {
        let va = i32::from(VOICE_TYPES.contains(&a.kind_id));
        let vb = i32::from(VOICE_TYPES.contains(&b.kind_id));
        va.cmp(&vb).then(a.position.cmp(&b.position)).then_with(|| compare_ids(&a.id, &b.id))
    };
    let mut categories: Vec<ResChannel> = channels.iter().filter(|c| c.kind == "category").cloned().collect();
    categories.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| compare_ids(&a.id, &b.id)));
    let cat_ids: HashSet<String> = categories.iter().map(|c| c.id.clone()).collect();
    let mut children: HashMap<String, Vec<ResChannel>> = categories.iter().map(|c| (c.id.clone(), Vec::new())).collect();
    let mut top = Vec::new();
    for c in channels {
        if c.kind == "category" {
            continue;
        }
        match c.parent_id.clone().filter(|p| cat_ids.contains(p)) {
            Some(p) => children.entry(p).or_default().push(c),
            None => top.push(c),
        }
    }
    top.sort_by(in_parent);
    let mut out = top;
    for cat in categories {
        let mut kids = children.remove(&cat.id).unwrap_or_default();
        kids.sort_by(in_parent);
        out.push(cat);
        out.extend(kids);
    }
    out
}

fn role_color(v: Option<&Value>) -> Option<String> {
    let n = js_int(v)?;
    (n > 0 && n <= 0xff_ffff).then(|| format!("#{n:06X}"))
}

fn perms_of(r: Option<&Value>) -> i128 {
    match r.and_then(|r| r.get("permissions")) {
        None | Some(Value::Null) => 0,
        Some(v) => js_bigint(v).unwrap_or(0),
    }
}

fn snowflake_of(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str).filter(|s| is_snowflake(s))
}

/// The REST answers of one guild in the shapes of `getGuildResources` (`mapGuildResources`).
pub fn map_guild_resources(
    guild_id: &str,
    bot_user_id: &str,
    guild: &Value,
    raw_channels: &Value,
    raw_roles: &Value,
    raw_emojis: &Value,
    bot_member: &Value,
) -> DiscordResources {
    let empty = Vec::new();
    let role_list: Vec<&Value> =
        raw_roles.as_array().unwrap_or(&empty).iter().filter(|r| snowflake_of(r.get("id")).is_some()).collect();
    let mut position_of: HashMap<&str, i64> = HashMap::new();
    for r in &role_list {
        let id = r.get("id").and_then(Value::as_str).unwrap_or_default();
        position_of.insert(id, js_int(r.get("position")).unwrap_or(0));
    }
    let bot_role_ids: Vec<&str> = bot_member
        .get("roles")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).filter(|id| position_of.contains_key(id)).collect())
        .unwrap_or_default();
    let top_role_position = bot_role_ids.iter().map(|id| position_of[id]).fold(0, i64::max);
    let find_role = |id: &str| role_list.iter().copied().find(|r| r.get("id").and_then(Value::as_str) == Some(id));
    let mut bot_perms = perms_of(find_role(guild_id));
    for id in &bot_role_ids {
        bot_perms |= perms_of(find_role(id));
    }
    let has = |flag: i128| (bot_perms & ADMINISTRATOR) != 0 || (bot_perms & flag) == flag;

    let mut roles: Vec<ResRole> = role_list
        .iter()
        .map(|r| {
            let id = r.get("id").and_then(Value::as_str).unwrap_or_default();
            let position = position_of[id];
            let managed = r.get("managed") == Some(&Value::Bool(true));
            let everyone = id == guild_id;
            ResRole {
                id: id.to_string(),
                name: r.get("name").and_then(Value::as_str).unwrap_or("unknown role").to_string(),
                color: role_color(r.get("color")),
                position,
                managed,
                everyone,
                mentionable: Some(r.get("mentionable") == Some(&Value::Bool(true))),
                assignable: Some(!managed && !everyone && position < top_role_position),
                dangerous: Some((perms_of(Some(r)) & DANGEROUS_PERMISSIONS) != 0),
                permissions: None,
            }
        })
        .collect();
    roles.sort_by(|a, b| b.position.cmp(&a.position).then_with(|| compare_ids(&a.id, &b.id)));

    let channels: Vec<ResChannel> = raw_channels
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter(|c| snowflake_of(c.get("id")).is_some() && js_int(c.get("type")).is_some())
        .map(|c| {
            let kind_id = js_int(c.get("type")).unwrap_or(0);
            ResChannel {
                id: c.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
                name: c.get("name").and_then(Value::as_str).unwrap_or("unknown").to_string(),
                kind_id,
                kind: dash_schema::channel_kind(kind_id).to_string(),
                parent_id: snowflake_of(c.get("parent_id")).map(str::to_string),
                position: js_int(c.get("position")).unwrap_or(0),
                nsfw: c.get("nsfw") == Some(&Value::Bool(true)),
            }
        })
        .collect();
    let channels = sort_channels(channels);

    let mut emojis: Vec<ResEmoji> = raw_emojis
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter(|e| snowflake_of(e.get("id")).is_some() && e.get("name").is_some_and(Value::is_string))
        .map(|e| ResEmoji {
            id: e.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
            name: e.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
            animated: e.get("animated") == Some(&Value::Bool(true)),
        })
        .collect();
    emojis.sort_by(|a, b| name_compare(&a.name, &b.name).then_with(|| compare_ids(&a.id, &b.id)));

    let count = match guild.get("approximate_member_count") {
        None | Some(Value::Null) => guild.get("member_count"),
        some => some,
    };
    let int_or_zero = |k: &str| js_int(guild.get(k)).unwrap_or(0);
    DiscordResources {
        guild: ResGuild {
            id: guild_id.to_string(),
            name: guild.get("name").and_then(Value::as_str).unwrap_or("Unknown server").to_string(),
            icon: guild.get("icon").and_then(Value::as_str).filter(|i| is_image_hash(i)).map(str::to_string),
            member_count: js_int(count),
            owner_id: snowflake_of(guild.get("owner_id")).map(str::to_string),
            premium_tier: int_or_zero("premium_tier"),
            premium_subscription_count: int_or_zero("premium_subscription_count"),
            verification_level: int_or_zero("verification_level"),
        },
        channels,
        roles,
        emojis,
        bot: ResBot {
            user_id: bot_user_id.to_string(),
            top_role_position,
            manage_roles: has(MANAGE_ROLES),
            create_invite: has(CREATE_INSTANT_INVITE),
        },
    }
}

/// allowed_mentions for a dashboard write: never @everyone/@here; nothing at all unless the caller lists it.
/// `safeMentions`.
pub fn safe_mentions(payload: Value) -> Value {
    let am = payload.get("allowed_mentions").cloned().unwrap_or(Value::Null);
    let parse: Vec<Value> = am
        .get("parse")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter(|p| p.as_str() == Some("users") || p.as_str() == Some("roles")).cloned().collect())
        .unwrap_or_default();
    let ids = |list: Option<&Value>| -> Option<Vec<Value>> {
        list.and_then(Value::as_array)
            .map(|a| a.iter().filter(|id| id.as_str().is_some_and(is_snowflake)).take(100).cloned().collect())
    };
    let has = |k: &str| parse.iter().any(|p| p.as_str() == Some(k));
    let users = if has("users") { None } else { ids(am.get("users")) };
    let roles = if has("roles") { None } else { ids(am.get("roles")) };
    let mut out = Map::new();
    out.insert("parse".into(), Value::Array(parse));
    if let Some(u) = users {
        out.insert("users".into(), Value::Array(u));
    }
    if let Some(r) = roles {
        out.insert("roles".into(), Value::Array(r));
    }
    if am.get("replied_user") == Some(&Value::Bool(true)) {
        out.insert("replied_user".into(), Value::Bool(true));
    }
    // `{ ...payload, allowed_mentions: out }`: an existing key keeps its place.
    let mut obj = match payload {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    obj.insert("allowed_mentions".into(), Value::Object(out));
    Value::Object(obj)
}

fn message_ref(data: &Value, channel_id: &str) -> Result<MessageRef, DiscordApiError> {
    let Some(id) = snowflake_of(data.get("id")) else {
        return Err(api_error("Discord returned an unexpected message", "unavailable"));
    };
    Ok(MessageRef {
        id: id.to_string(),
        channel_id: data.get("channel_id").and_then(Value::as_str).unwrap_or(channel_id).to_string(),
    })
}

/// A member or user as the dashboard shows it (from a guild member object or a bare user object). `memberProfile`.
pub fn member_profile(m: &Value) -> Option<MemberProfile> {
    let u = match m.get("user") {
        Some(u) if !u.is_null() => u,
        _ => m,
    };
    let id = snowflake_of(u.get("id"))?;
    Some(MemberProfile {
        id: id.to_string(),
        username: u.get("username").and_then(Value::as_str).unwrap_or("unknown").to_string(),
        global_name: u.get("global_name").and_then(Value::as_str).map(str::to_string),
        nick: m.get("nick").and_then(Value::as_str).map(str::to_string),
        avatar_url: user_avatar_url(Some(id), u.get("avatar").and_then(Value::as_str)),
        bot: u.get("bot") == Some(&Value::Bool(true)),
    })
}

fn assert_snowflake(id: &str, what: &str) -> Result<(), DiscordApiError> {
    if is_snowflake(id) { Ok(()) } else { Err(api_error(format!("Invalid {what} id"), "not_found")) }
}

fn token_fingerprint(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    b64[..22].to_string()
}

/// The invite link for the bot (bot + applications.commands; with a guild id, that guild preselected). `inviteUrl`.
pub fn invite_url(client_id: &str, guild_id: Option<&str>) -> String {
    let permissions = INVITE_PERMISSIONS.to_string();
    let mut pairs = vec![("client_id", client_id), ("scope", "bot applications.commands"), ("permissions", permissions.as_str())];
    if let Some(g) = guild_id.filter(|g| is_snowflake(g)) {
        pairs.push(("guild_id", g));
        pairs.push(("disable_guild_select", "true"));
    }
    format!("https://discord.com/oauth2/authorize?{}", form_encode(&pairs))
}

/// What the dashboard server needs from its Discord client (server.js `deps.discord`): the stores' calls
/// ([`DashboardDiscord`]) plus the login, guild-list and invite calls. [`DiscordApi`] implements it over Discord's
/// REST API; tests implement it with fakes.
#[async_trait]
pub trait ServerDiscord: DashboardDiscord {
    /// `getCurrentUser(accessToken)`.
    async fn get_current_user(&self, access_token: &str) -> Result<CurrentUser, DiscordApiError>;
    /// `getUserGuilds(userId, accessToken, { allowStale })`.
    async fn get_user_guilds(&self, user_id: &str, access_token: &str, allow_stale: bool) -> Result<Arc<Vec<OAuthGuild>>, DiscordApiError>;
    /// `forgetUser(userId)`: drops the cached guild list of a user (after logout) and stops a load still in flight
    /// from putting it back.
    fn forget_user(&self, _user_id: &str) {}
    /// `warmUserGuilds(userId, accessToken)`: starts loading the user's list in the background with the token just
    /// issued. Never fails; the default does nothing.
    fn warm_user_guilds(&self, _user_id: &str, _access_token: &str) {}
    /// `getBotGuildIds({ swr: true })` started without waiting for it (the server list loads the bot's list while the
    /// user's loads). Never fails; the default does nothing.
    fn prefetch_bot_guilds(&self) {}
    /// `getBotGuilds()`: the servers the bot is in.
    async fn get_bot_guilds(&self) -> Result<Arc<Vec<BotGuild>>, DiscordApiError>;
    /// `getBotGuilds({ swr: true })`: display only; a list up to 10 minutes old may be served while it refreshes.
    async fn get_bot_guilds_swr(&self) -> Result<Arc<Vec<BotGuild>>, DiscordApiError> {
        self.get_bot_guilds().await
    }
    /// `isBotInGuild(guildId, { verify })`.
    async fn is_bot_in_guild(&self, guild_id: &str, verify: bool) -> Result<bool, DiscordApiError>;
    /// `isBotInGuild(guildId, { verify, swr: true })`: a hit may come from a list up to 10 minutes old (a miss with
    /// `verify` is still checked live).
    async fn is_bot_in_guild_swr(&self, guild_id: &str, verify: bool) -> Result<bool, DiscordApiError> {
        self.is_bot_in_guild(guild_id, verify).await
    }
    /// `inviteUrl(guildId?)`.
    fn invite_url(&self, guild_id: Option<&str>) -> String;
    /// `exchangeCode(code)`: the error's message is logged.
    async fn exchange_code(&self, code: &str) -> anyhow::Result<TokenGrant>;
    /// `revokeToken(accessToken)`: best effort.
    async fn revoke_token(&self, _access_token: &str) -> bool {
        false
    }
}

/// The client's configuration (`{ clientId, clientSecret, botToken, redirectUri }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordApiConfig {
    pub client_id: String,
    pub client_secret: String,
    pub bot_token: String,
    pub redirect_uri: String,
}

impl From<&dash_auth::DashboardConfig> for DiscordApiConfig {
    fn from(c: &dash_auth::DashboardConfig) -> Self {
        Self {
            client_id: c.client_id.clone(),
            client_secret: c.client_secret.clone(),
            bot_token: c.bot_token.clone(),
            redirect_uri: c.redirect_uri.clone(),
        }
    }
}

/// A clock: ms since epoch.
pub type ClockFn = Arc<dyn Fn() -> f64 + Send + Sync>;
/// The pause before a rate-limited bot-guild page is retried (ms).
pub type SleepFn = Arc<dyn Fn(u64) -> BoxFuture<'static, ()> + Send + Sync>;

/// `createDiscordApi({ config, fetchImpl, now, logger, sleep })`: everything but the config is optional.
#[derive(Clone)]
pub struct DiscordApiOptions {
    pub config: DiscordApiConfig,
    /// Discord's REST base (`RouteBases.api`); tests point it at a local stub.
    pub api_base: String,
    /// The OAuth token and revocation endpoints.
    pub token_url: String,
    pub revoke_url: String,
    pub now: Option<ClockFn>,
    pub logger: Option<Arc<dyn DashLogger>>,
    pub sleep: Option<SleepFn>,
}

impl DiscordApiOptions {
    pub fn new(config: DiscordApiConfig) -> Self {
        Self {
            config,
            api_base: API_BASE.to_string(),
            token_url: dash_auth::auth::TOKEN_URL.to_string(),
            revoke_url: dash_auth::auth::TOKEN_REVOCATION_URL.to_string(),
            now: None,
            logger: None,
            sleep: None,
        }
    }
}

type SharedLoad<T> = Shared<BoxFuture<'static, Result<T, DiscordApiError>>>;

struct UserGuildsEntry {
    fp: String,
    guilds: Arc<Vec<OAuthGuild>>,
    fetched_at: f64,
    /// No background refresh before this (after a failed one).
    retry_at: f64,
}

/// A user's load in flight; `cancelled` is set by forget_user so a late answer is not stored.
struct UserSlot {
    load: SharedLoad<Arc<Vec<OAuthGuild>>>,
    cancelled: Arc<AtomicBool>,
}

struct ResourcesEntry {
    value: Arc<DiscordResources>,
    at: f64,
    /// No background reload before this (after a failed one).
    retry_at: f64,
}

#[derive(Default)]
struct BotGuildsState {
    ids: Option<HashSet<String>>,
    entries: Arc<Vec<BotGuild>>,
    at: f64,
    /// When the list last loaded successfully (`at` also moves on a failed refresh, to space retries).
    loaded_at: f64,
    /// No refresh-ahead before this (after a failed one).
    retry_at: f64,
}

#[derive(Default)]
struct State {
    user_guilds: IndexMap<String, UserGuildsEntry>,
    user_inflight: HashMap<String, UserSlot>,
    bot: BotGuildsState,
    bot_inflight: Option<SharedLoad<()>>,
    bot_misses: IndexMap<String, f64>,
    resources: IndexMap<String, ResourcesEntry>,
    resources_inflight: HashMap<String, SharedLoad<Arc<DiscordResources>>>,
    users: IndexMap<String, (Option<MemberProfile>, f64)>,
}

struct Inner {
    config: DiscordApiConfig,
    bot: String,
    api_base: String,
    http: reqwest::Client,
    oauth: OAuthClient,
    now: ClockFn,
    sleep: SleepFn,
    logger: Option<Arc<dyn DashLogger>>,
    state: Mutex<State>,
}

/// The dashboard's Discord client (`createDiscordApi`). Cheap to clone; clones share the caches.
#[derive(Clone)]
pub struct DiscordApi {
    inner: Arc<Inner>,
}

fn spawn_shared<T: Clone + Send + Sync + 'static>(
    fut: impl std::future::Future<Output = Result<T, DiscordApiError>> + Send + 'static,
) -> SharedLoad<T> {
    let handle = tokio::spawn(fut);
    async move {
        handle.await.unwrap_or_else(|e| Err(api_error(format!("Discord request failed: {e}"), "unavailable")))
    }
    .boxed()
    .shared()
}

impl DiscordApi {
    pub fn new(opts: DiscordApiOptions) -> Self {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
            .user_agent(USER_AGENT)
            .build()
            .expect("the Discord HTTP client (TLS backend) builds");
        let now = opts.now.unwrap_or_else(|| Arc::new(bot_util::js::now_ms));
        let sleep = opts.sleep.unwrap_or_else(|| Arc::new(|ms| tokio::time::sleep(Duration::from_millis(ms)).boxed()));
        let bot = format!("Bot {}", opts.config.bot_token);
        let oauth = OAuthClient::with_endpoints(&opts.token_url, &opts.revoke_url);
        Self {
            inner: Arc::new(Inner {
                config: opts.config,
                bot,
                api_base: opts.api_base.trim_end_matches('/').to_string(),
                http,
                oauth,
                now,
                sleep,
                logger: opts.logger,
                state: Mutex::new(State::default()),
            }),
        }
    }

    /// The client for a dashboard config, against Discord itself.
    pub fn from_config(config: &dash_auth::DashboardConfig, logger: Option<Arc<dyn DashLogger>>) -> Self {
        let mut opts = DiscordApiOptions::new(DiscordApiConfig::from(config));
        opts.logger = logger;
        Self::new(opts)
    }

    /// `getBotGuildIds()`: the bot's guild ids, refreshed at most every BOT_GUILDS_TTL_MS. A failed refresh keeps the
    /// old set; an error only when no list was ever loaded.
    pub async fn get_bot_guild_ids(&self) -> Result<HashSet<String>, DiscordApiError> {
        self.inner.ensure_bot_guilds(false).await?;
        Ok(self.inner.state.lock().bot.ids.clone().unwrap_or_default())
    }
}

impl Inner {
    fn now(&self) -> f64 {
        (self.now)()
    }

    /// `requestJson(fetchImpl, method, route, authorization, body, extraHeaders)`: Value::Null for 204 No Content.
    async fn request_json(
        &self,
        method: reqwest::Method,
        route: &str,
        authorization: &str,
        body: Option<&Value>,
        extra: &[(&str, String)],
    ) -> Result<Value, DiscordApiError> {
        let where_ = format!("{} {}", method.as_str(), route.split('?').next().unwrap_or(route));
        let mut req = self.http.request(method, format!("{}{route}", self.api_base));
        for (k, v) in extra {
            req = req.header(*k, v);
        }
        req = req.header("Authorization", authorization).header("Accept", "application/json");
        if let Some(b) = body {
            req = req.header("Content-Type", "application/json").body(serde_json::to_string(b).unwrap_or_default());
        }
        let res = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                let why = if e.is_timeout() { "timed out" } else { "fetch failed" };
                return Err(api_error(format!("Discord request failed: {why}"), "unavailable"));
            }
        };
        let status = res.status().as_u16();
        if (300..400).contains(&status) {
            // fetch's redirect: 'error'.
            return Err(api_error("Discord request failed: fetch failed", "unavailable"));
        }
        if !(200..300).contains(&status) {
            let header = res.headers().get("retry-after").and_then(|h| h.to_str().ok()).map(str::to_string);
            let data: Value = match res.bytes().await {
                Ok(b) => serde_json::from_slice(&b).unwrap_or(Value::Null),
                Err(_) => Value::Null,
            };
            let mut err = DiscordApiError::new(format!("Discord returned HTTP {status} for {where_}"), status, code_for_status(status));
            if status == 429 {
                err = err.with_retry_after(retry_after_seconds(header.as_deref(), &data));
            }
            if let Some(code) = js_int(data.get("code")) {
                err.discord_code = Some(code);
            }
            return Err(err);
        }
        if status == 204 {
            return Ok(Value::Null);
        }
        let invalid = || DiscordApiError::new(format!("Discord returned invalid JSON for {where_}"), status, "unavailable");
        let bytes = res.bytes().await.map_err(|_| invalid())?;
        serde_json::from_slice(&bytes).map_err(|_| invalid())
    }

    async fn get_json(&self, route: &str, authorization: &str) -> Result<Value, DiscordApiError> {
        self.request_json(reqwest::Method::GET, route, authorization, None, &[]).await
    }

    async fn lookup_user(self: &Arc<Self>, user_id: String) -> Result<(String, Option<MemberProfile>), DiscordApiError> {
        if let Some((value, at)) = self.state.lock().users.get(&user_id)
            && self.now() - at < USERS_TTL_MS
        {
            return Ok((user_id.clone(), value.clone()));
        }
        let value = match self.get_json(&format!("/users/{user_id}"), &self.bot).await {
            Ok(v) => member_profile(&v),
            Err(e) if e.code == "not_found" => None,
            Err(e) => return Err(e),
        };
        let at = self.now();
        let mut st = self.state.lock();
        st.users.shift_remove(&user_id);
        st.users.insert(user_id.clone(), (value.clone(), at));
        while st.users.len() > USERS_CACHE_MAX {
            st.users.shift_remove_index(0);
        }
        Ok((user_id, value))
    }

    async fn load_guild_resources(self: Arc<Self>, guild_id: String) -> Result<Arc<DiscordResources>, DiscordApiError> {
        let (guild_route, channels_route) = (format!("/guilds/{guild_id}?with_counts=true"), format!("/guilds/{guild_id}/channels"));
        let member_route = format!("/guilds/{guild_id}/members/{}", self.config.client_id);
        let (guild, channels, bot_member) = futures::try_join!(
            self.get_json(&guild_route, &self.bot),
            self.get_json(&channels_route, &self.bot),
            self.get_json(&member_route, &self.bot),
        )?;
        // GET /guilds/{id} carries the roles and emojis; the list routes are only asked when it does not.
        let roles_fut = async {
            match guild.get("roles") {
                Some(r @ Value::Array(_)) => Ok(r.clone()),
                _ => self.get_json(&format!("/guilds/{guild_id}/roles"), &self.bot).await,
            }
        };
        let emojis_fut = async {
            match guild.get("emojis") {
                Some(e @ Value::Array(_)) => Ok(e.clone()),
                _ => self.get_json(&format!("/guilds/{guild_id}/emojis"), &self.bot).await,
            }
        };
        let (roles, emojis) = futures::try_join!(roles_fut, emojis_fut)?;
        let value =
            Arc::new(map_guild_resources(&guild_id, &self.config.client_id, &guild, &channels, &roles, &emojis, &bot_member));
        let at = self.now();
        let mut st = self.state.lock();
        st.resources.shift_remove(&guild_id);
        st.resources.insert(guild_id.clone(), ResourcesEntry { value: value.clone(), at, retry_at: 0.0 });
        while st.resources.len() > RESOURCES_CACHE_MAX {
            st.resources.shift_remove_index(0);
        }
        Ok(value)
    }

    /// The shared load of one user's list (one per user and token). Called with the state locked.
    fn start_user_fetch(self: &Arc<Self>, st: &mut State, user_id: &str, token: &str, fp: &str) -> SharedLoad<Arc<Vec<OAuthGuild>>> {
        let key = format!("{user_id}:{fp}");
        if let Some(slot) = st.user_inflight.get(&key) {
            return slot.load.clone();
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let me = self.clone();
        let (uid, tok, f, k, c) = (user_id.to_string(), token.to_string(), fp.to_string(), key.clone(), cancelled.clone());
        let load = spawn_shared(async move {
            let result = me.clone().fetch_user_guilds(uid, tok, f, c.clone()).await;
            let mut st = me.state.lock();
            if st.user_inflight.get(&k).is_some_and(|s| Arc::ptr_eq(&s.cancelled, &c)) {
                st.user_inflight.remove(&k);
            }
            result
        });
        st.user_inflight.insert(key, UserSlot { load: load.clone(), cancelled });
        load
    }

    async fn fetch_user_guilds(
        self: Arc<Self>,
        user_id: String,
        token: String,
        fp: String,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Arc<Vec<OAuthGuild>>, DiscordApiError> {
        let result = async {
            let data = self.get_json("/users/@me/guilds?limit=200", &format!("Bearer {token}")).await?;
            let Some(list) = data.as_array() else {
                return Err(api_error("Discord returned an unexpected guild list", "unavailable"));
            };
            let guilds = Arc::new(list.iter().filter_map(normalize_guild).collect::<Vec<_>>());
            let at = self.now();
            let mut st = self.state.lock();
            if !cancelled.load(Ordering::SeqCst) {
                st.user_guilds.shift_remove(&user_id);
                st.user_guilds.insert(user_id.clone(), UserGuildsEntry { fp: fp.clone(), guilds: guilds.clone(), fetched_at: at, retry_at: 0.0 });
                while st.user_guilds.len() > USER_GUILDS_CACHE_MAX {
                    st.user_guilds.shift_remove_index(0);
                }
            }
            Ok(guilds)
        }
        .await;
        if let Err(e) = &result {
            let mut st = self.state.lock();
            if e.code == "unauthenticated" {
                st.user_guilds.shift_remove(&user_id);
            } else if let Some(entry) = st.user_guilds.get_mut(&user_id)
                && entry.fp == fp
            {
                // A failed refresh keeps the entry; the next try waits a little.
                entry.retry_at = self.now() + USER_GUILDS_REFRESH_RETRY_MS;
            }
        }
        result
    }

    async fn refresh_bot_guilds(self: Arc<Self>) -> Result<(), DiscordApiError> {
        let mut ids: HashSet<String> = HashSet::new();
        let mut entries: Vec<BotGuild> = Vec::new();
        let mut after = "0".to_string();
        let mut retries_left = BOT_GUILDS_MAX_RETRIES;
        for _ in 0..BOT_GUILDS_MAX_PAGES {
            // One page; a short 429 is waited out and the same page retried (a few times per refresh).
            let data = loop {
                match self.get_json(&format!("/users/@me/guilds?limit=200&after={after}"), &self.bot).await {
                    Ok(d) => break d,
                    Err(err) => {
                        let wait = if err.code == "rate_limited" { err.retry_after } else { None };
                        match wait {
                            Some(w) if w <= BOT_GUILDS_MAX_WAIT_SECONDS && retries_left > 0 => {
                                retries_left -= 1;
                                (self.sleep)(w * 1000).await;
                            }
                            _ => return Err(err),
                        }
                    }
                }
            };
            let Some(list) = data.as_array() else {
                return Err(api_error("Discord returned an unexpected guild list", "unavailable"));
            };
            for g in list {
                let Some(id) = snowflake_of(g.get("id")) else { continue };
                if !ids.insert(id.to_string()) {
                    continue;
                }
                entries.push(BotGuild {
                    id: id.to_string(),
                    name: g.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).unwrap_or("Unknown server").to_string(),
                    icon: g.get("icon").and_then(Value::as_str).map(str::to_string),
                });
            }
            if list.len() < 200 {
                break;
            }
            match snowflake_of(list.last().and_then(|g| g.get("id"))) {
                Some(last) => after = last.to_string(),
                None => break,
            }
        }
        let at = self.now();
        let mut st = self.state.lock();
        st.bot.ids = Some(ids);
        st.bot.entries = Arc::new(entries);
        st.bot.at = at;
        st.bot.loaded_at = at;
        st.bot_misses.clear();
        Ok(())
    }

    /// The shared refresh of the bot's list. Called with the state locked.
    fn start_bot_refresh(self: &Arc<Self>, st: &mut State) -> SharedLoad<()> {
        if let Some(p) = &st.bot_inflight {
            return p.clone();
        }
        let me = self.clone();
        let p = spawn_shared(async move {
            let result = me.clone().refresh_bot_guilds().await;
            me.state.lock().bot_inflight = None;
            result
        });
        st.bot_inflight = Some(p.clone());
        p
    }

    /// A failed refresh keeps the old list and retries after another TTL instead of on every request.
    fn bot_refresh_failed(&self, err: &DiscordApiError) {
        if let Some(l) = &self.logger {
            l.warn(&format!("Dashboard: could not refresh the bot's server list ({}); using the previous one.", err.message));
        }
        self.state.lock().bot.at = self.now();
    }

    /// The bot's list, refreshed at most every BOT_GUILDS_TTL_MS. With `swr` (display and presence reads, never a check
    /// that must be current) a list younger than BOT_GUILDS_MAX_STALE_MS is returned at once and refreshed in the
    /// background once it is past the TTL.
    async fn ensure_bot_guilds(self: &Arc<Self>, swr: bool) -> Result<(), DiscordApiError> {
        let pending = {
            let mut st = self.state.lock();
            let now = self.now();
            if st.bot.ids.is_some() && now - st.bot.at < BOT_GUILDS_TTL_MS {
                // Refresh ahead: past half the TTL the list is still returned (it is under the TTL) and one background
                // refresh through the single in-flight slot replaces it before it expires.
                if now - st.bot.at >= BOT_GUILDS_REFRESH_AHEAD_MS && now >= st.bot.retry_at && st.bot_inflight.is_none() {
                    let p = self.start_bot_refresh(&mut st);
                    let me = self.clone();
                    tokio::spawn(async move {
                        if let Err(err) = p.await {
                            if let Some(l) = &me.logger {
                                l.warn(&format!(
                                    "Dashboard: could not refresh the bot's server list ahead of time ({}); retrying shortly.",
                                    err.message
                                ));
                            }
                            let retry = me.now() + BOT_GUILDS_REFRESH_RETRY_MS;
                            me.state.lock().bot.retry_at = retry;
                        }
                    });
                }
                return Ok(());
            }
            if swr && st.bot.ids.is_some() && now - st.bot.loaded_at < BOT_GUILDS_MAX_STALE_MS {
                if st.bot_inflight.is_none() {
                    let p = self.start_bot_refresh(&mut st);
                    let me = self.clone();
                    tokio::spawn(async move {
                        if let Err(err) = p.await {
                            me.bot_refresh_failed(&err);
                        }
                    });
                }
                return Ok(());
            }
            self.start_bot_refresh(&mut st)
        };
        match pending.await {
            Ok(()) => Ok(()),
            Err(err) => {
                if self.state.lock().bot.ids.is_some() {
                    self.bot_refresh_failed(&err);
                    Ok(())
                } else {
                    Err(err)
                }
            }
        }
    }

    /// `getGuildResources(guildId, { fresh, swr })`. Called with the state locked.
    fn start_resources_load(self: &Arc<Self>, st: &mut State, guild_id: &str, fresh: bool, background: bool) -> SharedLoad<Arc<DiscordResources>> {
        let key = if fresh { format!("{guild_id}:fresh") } else { guild_id.to_string() };
        let existing = st
            .resources_inflight
            .get(&key)
            .cloned()
            .or_else(|| if fresh { None } else { st.resources_inflight.get(&format!("{guild_id}:fresh")).cloned() });
        if let Some(p) = existing {
            return p;
        }
        let me = self.clone();
        let (gid, k) = (guild_id.to_string(), key.clone());
        let p = spawn_shared(async move {
            let result = me.clone().load_guild_resources(gid.clone()).await;
            let mut st = me.state.lock();
            st.resources_inflight.remove(&k);
            if result.is_err() && background {
                // A failed reload keeps the entry; the next try waits another TTL.
                let retry = me.now() + RESOURCES_TTL_MS;
                if let Some(e) = st.resources.get_mut(&gid) {
                    e.retry_at = retry;
                }
            }
            result
        });
        st.resources_inflight.insert(key, p.clone());
        p
    }

    async fn resources(self: &Arc<Self>, guild_id: &str, fresh: bool, swr: bool) -> Result<Arc<DiscordResources>, DiscordApiError> {
        assert_snowflake(guild_id, "server")?;
        let pending = {
            let mut st = self.state.lock();
            let now = self.now();
            if !fresh && let Some(e) = st.resources.get(guild_id) {
                if now - e.at < RESOURCES_TTL_MS {
                    return Ok(e.value.clone());
                }
                if swr && now - e.at < RESOURCES_MAX_STALE_MS {
                    let (value, retry_at) = (e.value.clone(), e.retry_at);
                    if now >= retry_at
                        && !st.resources_inflight.contains_key(guild_id)
                        && !st.resources_inflight.contains_key(&format!("{guild_id}:fresh"))
                    {
                        let _ = self.start_resources_load(&mut st, guild_id, false, true);
                    }
                    return Ok(value);
                }
            }
            self.start_resources_load(&mut st, guild_id, fresh, false)
        };
        pending.await
    }
}

#[async_trait]
impl DashboardDiscord for DiscordApi {
    /// A guild's channels (Discord sidebar order), roles (highest first), emojis (by name) and the bot, cached for
    /// RESOURCES_TTL_MS per guild; concurrent callers share one load; `fresh` skips the cache (and refreshes it).
    async fn get_guild_resources(&self, guild_id: &str, fresh: bool) -> Result<Arc<DiscordResources>, DiscordApiError> {
        self.inner.resources(guild_id, fresh, false).await
    }

    /// The display read: an entry up to RESOURCES_MAX_STALE_MS old is served while it reloads in the background.
    async fn get_guild_resources_display(&self, guild_id: &str) -> Result<Arc<DiscordResources>, DiscordApiError> {
        self.inner.resources(guild_id, false, true).await
    }

    /// A member's role ids (bot token), or None when the user is not in the guild. Not cached.
    async fn get_member(&self, guild_id: &str, user_id: &str) -> Result<Option<MemberRoles>, DiscordApiError> {
        assert_snowflake(guild_id, "server")?;
        assert_snowflake(user_id, "user")?;
        match self.inner.get_json(&format!("/guilds/{guild_id}/members/{user_id}"), &self.inner.bot).await {
            Ok(m) => Ok(Some(MemberRoles {
                roles: m
                    .get("roles")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).filter(|id| is_snowflake(id)).map(str::to_string).collect())
                    .unwrap_or_default(),
            })),
            Err(e) if e.code == "not_found" => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Members whose username or nickname starts with `query` (Discord's member search), or the one member when
    /// `query` is a user id. At most MEMBER_SEARCH_MAX.
    async fn search_members(&self, guild_id: &str, query: &str) -> Result<Vec<MemberProfile>, DiscordApiError> {
        self.search_members_limit(guild_id, query, MEMBER_SEARCH_MAX as f64).await
    }

    /// Names and avatars of users by id, cached for an hour. Unknown ids map to None. At most 100 ids per call.
    async fn get_users(&self, ids: &[String]) -> Result<HashMap<String, Option<MemberProfile>>, DiscordApiError> {
        let mut wanted: Vec<String> = Vec::new();
        for id in ids {
            if is_snowflake(id) && !wanted.contains(id) {
                wanted.push(id.clone());
            }
        }
        wanted.truncate(100);
        let inner = self.inner.clone();
        let found: Vec<(String, Option<MemberProfile>)> = futures::stream::iter(wanted)
            .map(|id| {
                let inner = inner.clone();
                async move { inner.lookup_user(id).await }
            })
            .buffered(USER_LOOKUP_CONCURRENCY)
            .try_collect()
            .await?;
        Ok(found.into_iter().collect())
    }

    /// Posts a message as the bot; its allowed_mentions never allow @everyone/@here and default to no pings at all.
    async fn send_message(&self, channel_id: &str, payload: Value) -> Result<MessageRef, DiscordApiError> {
        assert_snowflake(channel_id, "channel")?;
        let body = safe_mentions(payload);
        let data = self
            .inner
            .request_json(reqwest::Method::POST, &format!("/channels/{channel_id}/messages"), &self.inner.bot, Some(&body), &[])
            .await?;
        message_ref(&data, channel_id)
    }

    /// Edits a message the bot posted (same payload rules as send_message).
    async fn edit_message(&self, channel_id: &str, message_id: &str, payload: Value) -> Result<MessageRef, DiscordApiError> {
        assert_snowflake(channel_id, "channel")?;
        assert_snowflake(message_id, "message")?;
        let body = safe_mentions(payload);
        let route = format!("/channels/{channel_id}/messages/{message_id}");
        let data = self.inner.request_json(reqwest::Method::PATCH, &route, &self.inner.bot, Some(&body), &[]).await?;
        message_ref(&data, channel_id)
    }

    /// Deletes a message. A message (or channel) that is already gone counts as deleted.
    async fn delete_message(&self, channel_id: &str, message_id: &str) -> Result<(), DiscordApiError> {
        assert_snowflake(channel_id, "channel")?;
        assert_snowflake(message_id, "message")?;
        let route = format!("/channels/{channel_id}/messages/{message_id}");
        match self.inner.request_json(reqwest::Method::DELETE, &route, &self.inner.bot, None, &[]).await {
            Ok(_) => Ok(()),
            Err(e) if e.code == "not_found" => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Creates a single-use, 30-minute invite as the bot; `reason` shows in the server's audit log.
    async fn create_invite(&self, channel_id: &str, reason: &str) -> Result<InviteRef, DiscordApiError> {
        assert_snowflake(channel_id, "channel")?;
        let mut headers = Vec::new();
        if !reason.is_empty() {
            headers.push(("X-Audit-Log-Reason", bot_util::js::encode_uri_component(bot_util::js::utf16_prefix(reason, 512))));
        }
        let body = json!({ "max_age": OWNER_INVITE_MAX_AGE_SECONDS, "max_uses": 1, "unique": true, "temporary": false });
        let route = format!("/channels/{channel_id}/invites");
        let data = self.inner.request_json(reqwest::Method::POST, &route, &self.inner.bot, Some(&body), &headers).await?;
        let Some(code) = data.get("code").and_then(Value::as_str).filter(|c| is_invite_code(c)) else {
            return Err(api_error("Discord returned an unexpected invite", "unavailable"));
        };
        Ok(InviteRef {
            code: code.to_string(),
            url: format!("https://discord.gg/{code}"),
            expires_at: data.get("expires_at").and_then(Value::as_str).map(str::to_string),
        })
    }
}

impl DiscordApi {
    /// `searchMembers(guildId, query, limit)`.
    pub async fn search_members_limit(&self, guild_id: &str, query: &str, limit: f64) -> Result<Vec<MemberProfile>, DiscordApiError> {
        assert_snowflake(guild_id, "server")?;
        let q = bot_util::js::utf16_prefix(bot_util::js::trim(query), 100);
        if q.is_empty() {
            return Ok(Vec::new());
        }
        if is_snowflake(q) {
            return match self.inner.get_json(&format!("/guilds/{guild_id}/members/{q}"), &self.inner.bot).await {
                Ok(m) => Ok(member_profile(&m).into_iter().collect()),
                Err(e) if e.code == "not_found" => Ok(Vec::new()),
                Err(e) => Err(e),
            };
        }
        let t = limit.trunc();
        let n = if t.is_nan() || t == 0.0 { 1.0 } else { t }.max(1.0).min(MEMBER_SEARCH_MAX as f64) as u64;
        let n = n.to_string();
        let route = format!("/guilds/{guild_id}/members/search?{}", form_encode(&[("query", q), ("limit", &n)]));
        let data = self.inner.get_json(&route, &self.inner.bot).await?;
        Ok(data.as_array().map(|a| a.iter().filter_map(member_profile).collect()).unwrap_or_default())
    }
}

#[async_trait]
impl ServerDiscord for DiscordApi {
    async fn get_current_user(&self, access_token: &str) -> Result<CurrentUser, DiscordApiError> {
        let u = self.inner.get_json("/users/@me", &format!("Bearer {access_token}")).await?;
        let Some(id) = snowflake_of(u.get("id")) else {
            return Err(api_error("Discord returned an unexpected user", "unavailable"));
        };
        Ok(CurrentUser {
            id: id.to_string(),
            username: u.get("username").and_then(Value::as_str).unwrap_or("unknown").to_string(),
            global_name: u.get("global_name").and_then(Value::as_str).map(str::to_string),
            avatar: u.get("avatar").and_then(Value::as_str).map(str::to_string),
        })
    }

    /// The user's guilds, cached for 60 s per user and token and shared by concurrent callers. With `allow_stale`
    /// (display only, never for an access decision) a Discord 429 or outage serves a list up to
    /// USER_GUILDS_MAX_STALE_MS old; otherwise the error is returned.
    async fn get_user_guilds(&self, user_id: &str, access_token: &str, allow_stale: bool) -> Result<Arc<Vec<OAuthGuild>>, DiscordApiError> {
        let inner = &self.inner;
        let fp = token_fingerprint(access_token);
        let pending = {
            let mut st = inner.state.lock();
            let now = inner.now();
            let hit = st
                .user_guilds
                .get(user_id)
                .filter(|h| h.fp == fp && now - h.fetched_at < USER_GUILDS_TTL_MS)
                .map(|h| (h.guilds.clone(), h.fetched_at, h.retry_at));
            if let Some((guilds, fetched_at, retry_at)) = hit {
                // Refresh ahead: past half the TTL the entry is still returned (it is under the TTL) and one background
                // refresh with this session's own token replaces it before it expires.
                if now - fetched_at >= USER_GUILDS_REFRESH_AHEAD_MS && now >= retry_at {
                    let _ = inner.start_user_fetch(&mut st, user_id, access_token, &fp);
                }
                return Ok(guilds);
            }
            inner.start_user_fetch(&mut st, user_id, access_token, &fp)
        };
        match pending.await {
            Ok(g) => Ok(g),
            Err(err) => {
                if allow_stale && (err.code == "rate_limited" || err.code == "unavailable") {
                    let st = inner.state.lock();
                    if let Some(stale) = st.user_guilds.get(user_id)
                        && stale.fp == fp
                        && inner.now() - stale.fetched_at <= USER_GUILDS_MAX_STALE_MS
                    {
                        return Ok(stale.guilds.clone());
                    }
                }
                Err(err)
            }
        }
    }

    fn warm_user_guilds(&self, user_id: &str, access_token: &str) {
        let inner = &self.inner;
        let fp = token_fingerprint(access_token);
        let mut st = inner.state.lock();
        let now = inner.now();
        if st.user_guilds.get(user_id).is_some_and(|h| h.fp == fp && now - h.fetched_at < USER_GUILDS_TTL_MS) {
            return;
        }
        let _ = inner.start_user_fetch(&mut st, user_id, access_token, &fp);
    }

    fn prefetch_bot_guilds(&self) {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let _ = inner.ensure_bot_guilds(true).await;
        });
    }

    fn forget_user(&self, user_id: &str) {
        let mut st = self.inner.state.lock();
        st.user_guilds.shift_remove(user_id);
        // A load still in flight must not put the list back after the logout.
        let prefix = format!("{user_id}:");
        let keys: Vec<String> = st.user_inflight.keys().filter(|k| k.starts_with(&prefix)).cloned().collect();
        for k in keys {
            if let Some(slot) = st.user_inflight.remove(&k) {
                slot.cancelled.store(true, Ordering::SeqCst);
            }
        }
    }

    /// The servers the bot is in, from the same cached list as get_bot_guild_ids.
    async fn get_bot_guilds(&self) -> Result<Arc<Vec<BotGuild>>, DiscordApiError> {
        self.inner.ensure_bot_guilds(false).await?;
        Ok(self.inner.state.lock().bot.entries.clone())
    }

    async fn get_bot_guilds_swr(&self) -> Result<Arc<Vec<BotGuild>>, DiscordApiError> {
        self.inner.ensure_bot_guilds(true).await?;
        Ok(self.inner.state.lock().bot.entries.clone())
    }

    /// Whether the bot is in a guild. Uses the cached list; with `verify` a guild missing from it is double-checked
    /// with GET /guilds/{id} (403/404 = absent), so a fresh invite shows up at once.
    async fn is_bot_in_guild(&self, guild_id: &str, verify: bool) -> Result<bool, DiscordApiError> {
        self.is_bot_in_guild_with(guild_id, verify, false).await
    }

    async fn is_bot_in_guild_swr(&self, guild_id: &str, verify: bool) -> Result<bool, DiscordApiError> {
        self.is_bot_in_guild_with(guild_id, verify, true).await
    }

    fn invite_url(&self, guild_id: Option<&str>) -> String {
        invite_url(&self.inner.config.client_id, guild_id)
    }

    async fn exchange_code(&self, code: &str) -> anyhow::Result<TokenGrant> {
        let creds = self.credentials();
        self.inner.oauth.exchange_code(&creds, code).await.map_err(|e| anyhow::anyhow!(e.message))
    }

    async fn revoke_token(&self, access_token: &str) -> bool {
        let creds = self.credentials();
        self.inner.oauth.revoke_token(&creds, access_token).await
    }
}

impl DiscordApi {
    async fn is_bot_in_guild_with(&self, guild_id: &str, verify: bool, swr: bool) -> Result<bool, DiscordApiError> {
        if !is_snowflake(guild_id) {
            return Ok(false);
        }
        let inner = &self.inner;
        let loaded = match inner.ensure_bot_guilds(swr).await {
            Ok(()) => true,
            Err(e) if !verify => return Err(e),
            Err(_) => false,
        };
        if loaded && inner.state.lock().bot.ids.as_ref().is_some_and(|s| s.contains(guild_id)) {
            return Ok(true);
        }
        if !verify {
            return Ok(false);
        }
        if let Some(&until) = inner.state.lock().bot_misses.get(guild_id)
            && until > inner.now()
        {
            return Ok(false);
        }
        match inner.get_json(&format!("/guilds/{guild_id}"), &inner.bot).await {
            Ok(_) => {
                if let Some(ids) = inner.state.lock().bot.ids.as_mut() {
                    ids.insert(guild_id.to_string());
                }
                Ok(true)
            }
            Err(e) if e.code == "forbidden" || e.code == "not_found" => {
                let until = inner.now() + BOT_MISS_TTL_MS;
                let mut st = inner.state.lock();
                st.bot_misses.shift_remove(guild_id);
                st.bot_misses.insert(guild_id.to_string(), until);
                while st.bot_misses.len() > BOT_MISS_MAX {
                    st.bot_misses.shift_remove_index(0);
                }
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }
}

impl DiscordApi {
    fn credentials(&self) -> OAuthCredentials {
        let c = &self.inner.config;
        OAuthCredentials { client_id: c.client_id.clone(), client_secret: c.client_secret.clone(), redirect_uri: c.redirect_uri.clone() }
    }
}
