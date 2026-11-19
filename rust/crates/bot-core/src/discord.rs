//! The Discord connection as the framework sees it, and the guild view the scheduler needs.
//!
//! Part B's client boot creates a [`DiscordHandle`] once the serenity client exists and installs it with
//! [`crate::App::set_discord`]. Until then (and in tests) the app has no Discord connection: Discord-facing code checks
//! `app.discord()` and degrades, exactly as the JS degrades when a fetch fails.

use std::sync::Arc;

use serenity::all::{Cache, GuildId, Http};
use serenity::builder::CreateAllowedMentions;

/// The client default `allowedMentions: { parse: ['users'], repliedUser: false }` (bot.js). Part B passes it to
/// serenity's `HttpBuilder::default_allowed_mentions` so every message and interaction reply gets it unless the
/// builder sets its own.
pub fn default_allowed_mentions() -> CreateAllowedMentions {
    CreateAllowedMentions::new().all_users(true).replied_user(false)
}

/// Discord REST codes that mean the target is gone: unknown channel (10003), message (10008), role (10011), missing
/// access (50001) or permission (50013). Degrade and clean up, never retry (`architecture.md`, global conventions).
pub const GONE_CODES: [u64; 5] = [10003, 10008, 10011, 50001, 50013];

/// The JSON error code of a failed Discord REST call (`err.code` in discord.js), when Discord sent one.
pub fn error_code(err: &serenity::Error) -> Option<u64> {
    match err {
        serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(r)) => u64::try_from(r.error.code).ok().filter(|c| *c != 0),
        _ => None,
    }
}

/// The HTTP status of a failed Discord REST call.
pub fn error_status(err: &serenity::Error) -> Option<u16> {
    match err {
        serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(r)) => Some(r.status_code.as_u16()),
        _ => None,
    }
}

/// `GONE_CODES.has(err?.code)`: the REST error means the channel, message, role or access is gone.
pub fn is_gone(err: &serenity::Error) -> bool {
    error_code(err).is_some_and(|c| GONE_CODES.contains(&c))
}

/// `err?.message` of a serenity error, as discord.js words it where it can (Discord's own message text).
pub fn error_message(err: &serenity::Error) -> String {
    match err {
        serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(r)) => r.error.message.clone(),
        other => other.to_string(),
    }
}

/// The serenity HTTP client and cache, plus which shards this process runs.
#[derive(Clone)]
pub struct DiscordHandle {
    pub http: Arc<Http>,
    pub cache: Arc<Cache>,
    /// `client.shard?.ids`: `None` when the process is not sharded.
    pub shard_ids: Option<Vec<u64>>,
    /// `client.shard?.count ?? 1`.
    pub shard_count: u64,
}

impl DiscordHandle {
    pub fn new(http: Arc<Http>, cache: Arc<Cache>, shard_ids: Option<Vec<u64>>, shard_count: u64) -> Self {
        Self { http, cache, shard_ids, shard_count: shard_count.max(1) }
    }

    /// The bot's own user id (from the cache once READY arrived).
    pub fn bot_user_id(&self) -> serenity::all::UserId {
        self.cache.current_user().id
    }
}

/// Whether a guild can run work now (`client.guilds.cache.get(id)` and `guild.available`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuildState {
    /// Not in this shard's cache.
    Missing,
    /// Cached but unavailable (a Discord outage).
    Unavailable,
    Available,
}

/// What the scheduler and the guild-ready runner need to know about the guilds this process holds.
pub trait GuildDirectory: Send + Sync + 'static {
    /// This shard's available guilds.
    fn available_guild_ids(&self) -> Vec<String>;
    fn guild_state(&self, guild_id: &str) -> GuildState;
    /// `client.shard?.ids`.
    fn shard_ids(&self) -> Option<Vec<u64>>;
    /// `client.shard?.count ?? 1`.
    fn shard_count(&self) -> u64;
}

impl GuildDirectory for DiscordHandle {
    fn available_guild_ids(&self) -> Vec<String> {
        let unavailable = self.cache.unavailable_guilds();
        self.cache.guilds().into_iter().filter(|g| !unavailable.get(g).is_some()).map(|g| g.to_string()).collect()
    }

    fn guild_state(&self, guild_id: &str) -> GuildState {
        let Ok(id) = guild_id.parse::<u64>() else { return GuildState::Missing };
        if id == 0 {
            return GuildState::Missing;
        }
        let id = GuildId::new(id);
        if self.cache.unavailable_guilds().get(&id).is_some() {
            return GuildState::Unavailable;
        }
        if self.cache.guild(id).is_some() { GuildState::Available } else { GuildState::Missing }
    }

    fn shard_ids(&self) -> Option<Vec<u64>> {
        self.shard_ids.clone()
    }

    fn shard_count(&self) -> u64 {
        self.shard_count
    }
}

/// A fixed guild list (tests, and the app before Discord connects: no guilds).
#[derive(Debug, Clone, Default)]
pub struct StaticGuilds {
    pub available: Vec<String>,
    pub unavailable: Vec<String>,
    /// Every lookup misses the cache although the guild is listed (the JS tests' `cacheMiss`).
    pub cache_miss: bool,
    pub shard_ids: Option<Vec<u64>>,
    pub shard_count: u64,
}

impl StaticGuilds {
    /// These guilds, all available, unsharded.
    pub fn available<S: Into<String>>(ids: impl IntoIterator<Item = S>) -> Self {
        Self { available: ids.into_iter().map(Into::into).collect(), shard_count: 1, ..Default::default() }
    }
}

impl GuildDirectory for StaticGuilds {
    fn available_guild_ids(&self) -> Vec<String> {
        self.available.clone()
    }

    fn guild_state(&self, guild_id: &str) -> GuildState {
        if self.cache_miss {
            return GuildState::Missing;
        }
        if self.available.iter().any(|g| g == guild_id) {
            GuildState::Available
        } else if self.unavailable.iter().any(|g| g == guild_id) {
            GuildState::Unavailable
        } else {
            GuildState::Missing
        }
    }

    fn shard_ids(&self) -> Option<Vec<u64>> {
        self.shard_ids.clone()
    }

    fn shard_count(&self) -> u64 {
        self.shard_count.max(1)
    }
}

// ── Putting REST results into the cache (discord.js `channels._add`, `roles._add`) ──

/// Whether a channel is a thread (public, private or announcement thread).
fn is_thread_kind(kind: serenity::all::ChannelType) -> bool {
    matches!(kind, serenity::all::ChannelType::PublicThread | serenity::all::ChannelType::PrivateThread | serenity::all::ChannelType::NewsThread)
}

/// Puts a channel the bot just created or fetched over REST into the serenity cache (discord.js `channels._add`), so
/// cache lookups (`cached_channel`, `check_channel_usable`, permission checks) see it before the gateway event
/// arrives. Threads go to the guild's thread list. A no-op without a Discord connection or when the guild is not
/// cached. Returns whether it was cached.
pub fn cache_created_channel(app: &crate::App, channel: &serenity::all::GuildChannel) -> bool {
    let Some(d) = app.discord() else { return false };
    if d.cache.guild(channel.guild_id).is_none() {
        return false;
    }
    let Ok(json) = serde_json::to_value(channel) else { return false };
    if is_thread_kind(channel.kind) {
        if let Ok(mut event) = serde_json::from_value::<serenity::all::ThreadCreateEvent>(json) {
            d.cache.update(&mut event);
            return true;
        }
    } else if let Ok(mut event) = serde_json::from_value::<serenity::all::ChannelCreateEvent>(json) {
        d.cache.update(&mut event);
        return true;
    }
    false
}

/// Puts a role the bot just created over REST into the serenity cache (`roles._add`). Same rules as
/// [`cache_created_channel`].
pub fn cache_created_role(app: &crate::App, guild_id: GuildId, role: &serenity::all::Role) -> bool {
    let Some(d) = app.discord() else { return false };
    if d.cache.guild(guild_id).is_none() {
        return false;
    }
    let Ok(role_json) = serde_json::to_value(role) else { return false };
    let json = serde_json::json!({ "guild_id": guild_id, "role": role_json });
    match serde_json::from_value::<serenity::all::GuildRoleCreateEvent>(json) {
        Ok(mut event) => {
            d.cache.update(&mut event);
            true
        }
        Err(_) => false,
    }
}

/// Writes incident actions Discord returned over REST (`PUT /guilds/{g}/incident-actions`) into the cached guild, as
/// discord.js updates `guild.incidentsData` from the answer. serenity's GUILD_UPDATE handling does not copy
/// `incidents_data`, so the cached guild is re-inserted with the new value. `false` when the guild is not cached.
pub fn cache_guild_incidents(app: &crate::App, guild_id: GuildId, data: Option<&serenity::all::IncidentsData>) -> bool {
    let Some(d) = app.discord() else { return false };
    let Some(mut guild) = d.cache.guild(guild_id).map(|g| g.clone()) else { return false };
    guild.incidents_data = data.cloned();
    let Ok(json) = serde_json::to_value(&guild) else { return false };
    match serde_json::from_value::<serenity::all::GuildCreateEvent>(json) {
        Ok(mut event) => {
            d.cache.update(&mut event);
            true
        }
        Err(_) => false,
    }
}

// ── REST calls serenity does not offer in the discord.js shape ──

/// `encodeURIComponent(reason)`: how discord.js (`@discordjs/rest`) writes `X-Audit-Log-Reason`.
pub fn encode_audit_reason(reason: &str) -> String {
    let mut out = String::with_capacity(reason.len());
    for b in reason.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn reason_headers(reason: Option<&str>) -> Option<http::HeaderMap> {
    let reason = reason.filter(|r| !r.is_empty())?;
    let mut h = http::HeaderMap::new();
    let value = http::HeaderValue::from_str(&encode_audit_reason(reason)).ok()?;
    h.insert("X-Audit-Log-Reason", value);
    Some(h)
}

#[allow(clippy::result_large_err)] // serenity::Error, as serenity returns it
async fn send(http: &Http, req: serenity::http::Request<'_>) -> serenity::Result<Vec<u8>> {
    let resp = http.request(req).await?;
    let bytes = resp.bytes().await.map_err(|e| serenity::Error::Http(serenity::http::HttpError::Request(e)))?;
    Ok(bytes.to_vec())
}

/// `guild.bans.create(user, { deleteMessageSeconds, reason })`: `PUT /guilds/{g}/bans/{u}` with
/// `{ delete_message_seconds }` in the body (left out when `None`, as discord.js leaves it undefined).
#[allow(clippy::result_large_err)] // serenity::Error, as serenity returns it
pub async fn ban_with_delete_seconds(
    http: &Http,
    guild_id: GuildId,
    user_id: serenity::all::UserId,
    delete_message_seconds: Option<u32>,
    reason: Option<&str>,
) -> serenity::Result<()> {
    let body = match delete_message_seconds {
        Some(s) => serde_json::json!({ "delete_message_seconds": s }),
        None => serde_json::json!({}),
    };
    let req = serenity::http::Request::new(serenity::http::Route::GuildBan { guild_id, user_id }, serenity::http::LightMethod::Put)
        .body(Some(serde_json::to_vec(&body)?))
        .headers(reason_headers(reason));
    send(http, req).await.map(|_| ())
}

/// Where a ban page starts (`guild.bans.fetch({ limit, before, after })`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BanCursor {
    Before(serenity::all::UserId),
    After(serenity::all::UserId),
}

/// `GET /guilds/{g}/bans?limit=N` with `limit` up to 1000 (serenity's `get_bans` caps it at a `u8`).
#[allow(clippy::result_large_err)] // serenity::Error, as serenity returns it
pub async fn fetch_bans_page(http: &Http, guild_id: GuildId, limit: u16, cursor: Option<BanCursor>) -> serenity::Result<Vec<serenity::all::Ban>> {
    let mut params = vec![("limit", limit.clamp(1, 1000).to_string())];
    match cursor {
        Some(BanCursor::Before(id)) => params.push(("before", id.to_string())),
        Some(BanCursor::After(id)) => params.push(("after", id.to_string())),
        None => {}
    }
    let req = serenity::http::Request::new(serenity::http::Route::GuildBans { guild_id }, serenity::http::LightMethod::Get).params(Some(params));
    let bytes = send(http, req).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// `PATCH /guilds/{g}/roles` with the whole `[{ id, position }]` body and an audit reason (discord.js
/// `guild.roles.setPositions` / `role.setPosition`, which send every moved role at once). Returns the guild's roles.
#[allow(clippy::result_large_err)] // serenity::Error, as serenity returns it
pub async fn set_role_positions(http: &Http, guild_id: GuildId, positions: &[(serenity::all::RoleId, u16)], reason: Option<&str>) -> serenity::Result<Vec<serenity::all::Role>> {
    let body: Vec<serde_json::Value> = positions.iter().map(|(id, pos)| serde_json::json!({ "id": id.to_string(), "position": pos })).collect();
    let req = serenity::http::Request::new(serenity::http::Route::GuildRoles { guild_id }, serenity::http::LightMethod::Patch)
        .body(Some(serde_json::to_vec(&body)?))
        .headers(reason_headers(reason));
    let bytes = send(http, req).await?;
    let mut value: serde_json::Value = serde_json::from_slice(&bytes)?;
    if let Some(list) = value.as_array_mut() {
        for r in list.iter_mut() {
            r["guild_id"] = serde_json::json!(guild_id.to_string());
        }
    }
    Ok(serde_json::from_value(value)?)
}

/// A raw REST GET through serenity's HTTP client (its rate limiter included), parsed as the JSON Discord returned
/// (what discord.js reads for routes serenity has no typed call for, e.g. `GET /guilds/{g}?with_counts=true`).
pub async fn rest_get_json(
    http: &Http,
    route: serenity::http::Route<'_>,
    params: Option<Vec<(&'static str, String)>>,
) -> anyhow::Result<serde_json::Value> {
    let response = http.request(serenity::http::Request::new(route, serenity::http::LightMethod::Get).params(params)).await?;
    let bytes = response.bytes().await?;
    Ok(serde_json::from_slice(&bytes)?)
}
