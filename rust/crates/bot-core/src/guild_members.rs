//! Port of `src/utils/guildMembers.js`: one capped full member fetch per guild at a time, with a size cap, a timeout
//! and rate-limit memory. Never fails.
//!
//! discord.js asks the gateway (opcode 8); here the members are read through REST (`GET /guilds/{id}/members`, 1000
//! a page) and written into the serenity cache as a `GUILD_MEMBERS_CHUNK`, so later cache reads see them, as with
//! discord.js. Concurrent callers share the one in-flight fetch.

use std::collections::HashMap;
use std::sync::Mutex;

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use serenity::all::{GuildId, Member, UserId};

use crate::context::App;

/// `FETCH_ALL_DEFAULT_TIMEOUT_MS`.
pub const FETCH_ALL_DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// `FETCH_ALL_MAX_MEMBERS`.
pub const FETCH_ALL_MAX_MEMBERS: u64 = 100_000;
const MAX_RETRY_MS: i64 = 10 * 60_000;

/// Why a full fetch did not happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchFailure {
    TooLarge,
    RateLimited { retry_after_ms: i64 },
    Timeout,
    Error,
}

impl FetchFailure {
    /// The JS `reason`.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::TooLarge => "too-large",
            Self::RateLimited { .. } => "rate-limited",
            Self::Timeout => "timeout",
            Self::Error => "error",
        }
    }
}

/// `{ ok: true, members }` or `{ ok: false, reason, retryAfterMs? }`.
pub type FetchAll = Result<Vec<Member>, FetchFailure>;

type InFlight = Shared<BoxFuture<'static, FetchAll>>;

/// Per-app state (`inFlight`, `retryUntil`).
#[derive(Default)]
struct MembersState {
    in_flight: Mutex<HashMap<GuildId, (u64, InFlight)>>,
    retry_until: Mutex<HashMap<GuildId, i64>>,
    next: std::sync::atomic::AtomicU64,
}

/// The rate-limit pause after a 429 (`retry_after` seconds, else 30 s), between 1 s and 10 min.
pub fn retry_ms(retry_after_secs: Option<f64>) -> i64 {
    let secs = retry_after_secs.filter(|s| s.is_finite()).unwrap_or(30.0);
    ((secs * 1000.0).ceil() as i64).clamp(1_000, MAX_RETRY_MS)
}

async fn fetch_rest(app: App, guild_id: GuildId) -> FetchAll {
    let Some(d) = app.discord().cloned() else { return Err(FetchFailure::Error) };
    let mut members: Vec<Member> = Vec::new();
    let mut after: Option<UserId> = None;
    loop {
        match guild_id.members(d.http.as_ref(), Some(1000), after).await {
            Ok(page) => {
                let done = page.len() < 1000;
                after = page.last().map(|m| m.user.id);
                members.extend(page);
                if done || after.is_none() {
                    break;
                }
            }
            Err(e) => {
                if crate::discord::error_status(&e) == Some(429) {
                    let ms = retry_ms(None);
                    let state = app.ext::<MembersState>();
                    state.retry_until.lock().unwrap_or_else(|p| p.into_inner()).insert(guild_id, app.clock().now_ms() + ms);
                    return Err(FetchFailure::RateLimited { retry_after_ms: ms });
                }
                bot_util::log_warn!(format!("fetchAllMembers failed for guild {guild_id}: {}", crate::discord::error_message(&e)));
                return Err(FetchFailure::Error);
            }
        }
    }
    // Into the cache, as the gateway's member chunks would put them.
    let chunk = serde_json::json!({ "guild_id": guild_id, "members": members, "chunk_index": 0, "chunk_count": 1 });
    if let Ok(mut event) = serde_json::from_value::<serenity::all::GuildMembersChunkEvent>(chunk) {
        d.cache.update(&mut event);
    }
    Ok(members)
}

/// `fetchAllMembers(guild, { timeoutMs, maxMembers })`.
pub async fn fetch_all_members(app: &App, guild_id: GuildId, timeout_ms: u64, max_members: u64) -> FetchAll {
    let Some(d) = app.discord() else { return Err(FetchFailure::Error) };
    let Some(count) = d.cache.guild(guild_id).map(|g| g.member_count) else { return Err(FetchFailure::Error) };
    if count > max_members {
        return Err(FetchFailure::TooLarge);
    }
    let state = app.ext::<MembersState>();
    let now = app.clock().now_ms();
    {
        let mut retry = state.retry_until.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(until) = retry.get(&guild_id).copied() {
            if until > now {
                return Err(FetchFailure::RateLimited { retry_after_ms: until - now });
            }
            retry.remove(&guild_id);
        }
    }
    let pending = {
        let mut map = state.in_flight.lock().unwrap_or_else(|p| p.into_inner());
        match map.get(&guild_id) {
            Some((_, f)) => f.clone(),
            None => {
                let id = state.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let a = app.clone();
                let st = state.clone();
                let fut: BoxFuture<'static, FetchAll> = Box::pin(async move {
                    let r = fetch_rest(a, guild_id).await;
                    let mut map = st.in_flight.lock().unwrap_or_else(|p| p.into_inner());
                    if map.get(&guild_id).is_some_and(|(i, _)| *i == id) {
                        map.remove(&guild_id);
                    }
                    r
                });
                let shared = fut.shared();
                map.insert(guild_id, (id, shared.clone()));
                // Runs even when every caller times out, like the JS promise.
                let driver = shared.clone();
                tokio::spawn(async move {
                    let _ = driver.await;
                });
                shared
            }
        }
    };
    match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), pending).await {
        Ok(r) => r,
        Err(_) => Err(FetchFailure::Timeout),
    }
}

/// [`fetch_all_members`] with the JS defaults (2 min, 100 000 members).
pub async fn fetch_all_members_default(app: &App, guild_id: GuildId) -> FetchAll {
    fetch_all_members(app, guild_id, FETCH_ALL_DEFAULT_TIMEOUT_MS, FETCH_ALL_MAX_MEMBERS).await
}

// ── guild.members.fetch({ user: ids, time }) over the gateway ───────────────────

/// Sends a `REQUEST_GUILD_MEMBERS` (opcode 8) for `(guild, user ids, nonce)` on the guild's shard; `false` when it
/// could not be sent (no runner for the shard).
pub type ChunkSender = std::sync::Arc<dyn Fn(GuildId, Vec<UserId>, String) -> BoxFuture<'static, bool> + Send + Sync>;

/// The member-chunk requests in flight (`app.ext::<MemberChunks>()`): the gateway installs the sender
/// ([`set_chunk_sender`]) and feeds every `GUILD_MEMBERS_CHUNK` to [`deliver_members_chunk`].
#[derive(Default)]
pub struct MemberChunks {
    sender: std::sync::RwLock<Option<ChunkSender>>,
    waiters: Mutex<HashMap<String, tokio::sync::mpsc::UnboundedSender<serenity::all::GuildMembersChunkEvent>>>,
    next: std::sync::atomic::AtomicU64,
}

/// Installs how chunk requests reach the gateway (the client boot does it; tests may install a fake).
pub fn set_chunk_sender(app: &App, sender: Option<ChunkSender>) {
    *app.ext::<MemberChunks>().sender.write().unwrap_or_else(|p| p.into_inner()) = sender;
}

/// Whether the gateway's chunk sender is installed: [`fetch_members_by_ids`] then asks the gateway (a failure is an
/// error); without it, it falls back to one REST lookup per id, where any failed lookup counts as "not a member".
/// Callers for whom a transient REST error must not read as "absent" can check this first.
pub fn has_chunk_sender(app: &App) -> bool {
    app.ext::<MemberChunks>().sender.read().unwrap_or_else(|p| p.into_inner()).is_some()
}

/// Hands a `GUILD_MEMBERS_CHUNK` to the request with its nonce; `true` when one was waiting for it.
pub fn deliver_members_chunk(chunks: &MemberChunks, event: &serenity::all::GuildMembersChunkEvent) -> bool {
    let Some(nonce) = event.nonce.as_deref() else { return false };
    let waiters = chunks.waiters.lock().unwrap_or_else(|p| p.into_inner());
    waiters.get(nonce).is_some_and(|tx| tx.send(event.clone()).is_ok())
}

/// Why [`fetch_members_by_ids`] gave up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchMembersError {
    /// No Discord connection.
    Offline,
    /// No chunk arrived for `time` ms (discord.js `GuildMembersTimeout`).
    Timeout,
    /// The request could not be sent to the guild's shard.
    NotSent,
}

impl std::fmt::Display for FetchMembersError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Offline => "Discord is not connected",
            Self::Timeout => "Members didn't arrive in time.",
            Self::NotSent => "the member request could not be sent to the shard",
        })
    }
}

impl std::error::Error for FetchMembersError {}

/// Discord's cap on `user_ids` in one `REQUEST_GUILD_MEMBERS`.
pub const MEMBER_IDS_PER_REQUEST: usize = 100;

/// `guild.members.fetch({ user: ids, time })` (GuildMemberManager `_fetchMany`): asks the gateway for these members
/// and resolves with those Discord found (unknown ids are simply absent). Each chunk resets the `time` timer; the
/// request ends at its last chunk. The members are written into the cache. More than 100 ids are sent as several
/// requests, one after another (Discord rejects a larger `user_ids`).
///
/// Without a gateway sender (the gateway is not running, e.g. tests on a [`crate::testing::FakeRest`]), it falls
/// back to one REST `GET /guilds/{g}/members/{u}` per id (ids Discord does not know are left out).
pub async fn fetch_members_by_ids(app: &App, guild_id: GuildId, user_ids: &[UserId], time_ms: u64) -> Result<HashMap<UserId, Member>, FetchMembersError> {
    let Some(d) = app.discord().cloned() else { return Err(FetchMembersError::Offline) };
    let mut ids: Vec<UserId> = Vec::new();
    for id in user_ids {
        if !ids.contains(id) {
            ids.push(*id);
        }
    }
    let mut out = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let chunks = app.ext::<MemberChunks>();
    let sender = chunks.sender.read().unwrap_or_else(|p| p.into_inner()).clone();
    let Some(sender) = sender else {
        for id in ids {
            if let Ok(m) = guild_id.member(d.http.as_ref(), id).await {
                out.insert(id, m);
            }
        }
        cache_members(&d, guild_id, out.values().cloned().collect());
        return Ok(out);
    };
    for batch in ids.chunks(MEMBER_IDS_PER_REQUEST) {
        let n = chunks.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nonce = format!("m{}-{n}", app.clock().now_ms());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        chunks.waiters.lock().unwrap_or_else(|p| p.into_inner()).insert(nonce.clone(), tx);
        let result = async {
            if !sender(guild_id, batch.to_vec(), nonce.clone()).await {
                return Err(FetchMembersError::NotSent);
            }
            let mut received = 0u32;
            loop {
                let next = tokio::time::timeout(std::time::Duration::from_millis(time_ms), rx.recv()).await;
                let Ok(Some(mut chunk)) = next else { return Err(FetchMembersError::Timeout) };
                received += 1;
                d.cache.update(&mut chunk);
                let size = chunk.members.len();
                out.extend(chunk.members);
                if size < 1_000 || received >= chunk.chunk_count {
                    return Ok(());
                }
            }
        }
        .await;
        chunks.waiters.lock().unwrap_or_else(|p| p.into_inner()).remove(&nonce);
        result?;
    }
    Ok(out)
}

fn cache_members(d: &crate::discord::DiscordHandle, guild_id: GuildId, members: Vec<Member>) {
    if members.is_empty() {
        return;
    }
    let chunk = serde_json::json!({ "guild_id": guild_id, "members": members, "chunk_index": 0, "chunk_count": 1 });
    if let Ok(mut event) = serde_json::from_value::<serenity::all::GuildMembersChunkEvent>(chunk) {
        d.cache.update(&mut event);
    }
}
