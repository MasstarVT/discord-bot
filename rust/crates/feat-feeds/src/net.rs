//! The outbound seams of this crate, kept per app in `app.ext` so tests can swap them (the JS `__setFetchImpl`):
//!
//! * [`FeedFetcher`]: every HTTP request. The default is `bot_util::safe_fetch` (SSRF-safe, deadline, byte cap).
//! * [`DiscordRest`]: the Discord REST calls the feed poll and the reminder job make outside an interaction: post a
//!   message, fetch a channel the cache does not have, read the messages after one. The default goes through
//!   `app.http()`; offline (no token) every call fails like a network error.

use std::sync::{Arc, RwLock};

use bot_core::App;
use bot_util::safe_fetch::{FetchOptions, FetchResponse, SafeFetchError};
use futures::future::BoxFuture;
use serde_json::Value;
use serenity::all::{ChannelId, GuildChannel, GuildId, MessageId};

/// A failed request: a `SafeFetchError`, or anything else (the JS `NETWORK` branch).
#[derive(Debug, Clone, PartialEq)]
pub enum FetchFailure {
    Safe(SafeFetchError),
    Other(String),
}

/// The HTTP client of the feeds.
pub trait FeedFetcher: Send + Sync {
    /// `safeFetch(url, opts)`: a response for any status (`ok` is false for a non-2xx), or a failure.
    fn fetch(&self, url: String, opts: FetchOptions) -> BoxFuture<'static, Result<FetchResponse, FetchFailure>>;
    /// `safeFetchJson(url, opts)`: a non-2xx status is an `HTTP_ERROR` failure.
    fn fetch_json(&self, url: String, opts: FetchOptions) -> BoxFuture<'static, Result<Value, FetchFailure>>;
}

/// The production fetcher.
pub struct SafeFetcher;

impl FeedFetcher for SafeFetcher {
    fn fetch(&self, url: String, opts: FetchOptions) -> BoxFuture<'static, Result<FetchResponse, FetchFailure>> {
        Box::pin(async move { bot_util::safe_fetch::safe_fetch(&url, &opts).await.map_err(FetchFailure::Safe) })
    }
    fn fetch_json(&self, url: String, opts: FetchOptions) -> BoxFuture<'static, Result<Value, FetchFailure>> {
        Box::pin(async move { bot_util::safe_fetch::safe_fetch_json(&url, &opts).await.map_err(FetchFailure::Safe) })
    }
}

/// A Discord REST failure: the JSON error `code`, the HTTP status, the message; `timeout` for a call that did not
/// answer in time.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RestError {
    pub code: Option<u64>,
    pub status: Option<u16>,
    pub message: String,
    pub timeout: bool,
}

impl RestError {
    pub fn of(err: &serenity::Error) -> Self {
        let status = match err {
            serenity::Error::Http(serenity::all::HttpError::UnsuccessfulRequest(r)) => Some(r.status_code.as_u16()),
            _ => None,
        };
        Self { code: bot_core::discord::error_code(err), status, message: bot_core::discord::error_message(err), timeout: false }
    }

    pub fn code(code: u64, message: &str) -> Self {
        Self { code: Some(code), status: None, message: message.into(), timeout: false }
    }

    pub fn timed_out(label: &str) -> Self {
        Self { code: None, status: None, message: format!("{label} timed out"), timeout: true }
    }
}

impl std::fmt::Display for RestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RestError {}

/// What the activity check reads of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageLite {
    pub bot: bool,
    pub system: bool,
}

/// The Discord REST calls of the jobs.
pub trait DiscordRest: Send + Sync {
    /// `channel.send(payload)`: the body is Discord's create-message JSON. Returns the new message id.
    fn send_message(&self, app: App, channel_id: ChannelId, body: Value) -> BoxFuture<'static, Result<String, RestError>>;
    /// `guild.channels.fetch(id)`: `Ok(None)` when Discord answered with something that is not a guild channel.
    fn fetch_channel(&self, app: App, channel_id: ChannelId) -> BoxFuture<'static, Result<Option<GuildChannel>, RestError>>;
    /// `channel.messages.fetch({ after, limit })`.
    fn messages_after(&self, app: App, channel_id: ChannelId, after: MessageId, limit: u8) -> BoxFuture<'static, Result<Vec<MessageLite>, RestError>>;
}

/// discord.js `ErrorCodes.GuildChannelUnowned`: the error has no Discord code and no HTTP status.
pub const CHANNEL_UNOWNED: &str = "The fetched channel does not belong to this manager's guild.";

/// `guild.channels.fetch(id)` through the [`DiscordRest`] seam, for a channel the cache does not have. Like discord.js,
/// a channel of another guild (or one that is not a guild channel) is refused with the `GuildChannelUnowned` error
/// (no code, so callers treat it as transient), and a fetched channel is added to the cache.
pub async fn fetch_guild_channel(app: &App, guild_id: GuildId, channel_id: ChannelId) -> Result<GuildChannel, RestError> {
    match rest(app).fetch_channel(app.clone(), channel_id).await? {
        Some(ch) if ch.guild_id == guild_id => {
            bot_core::discord::cache_created_channel(app, &ch);
            Ok(ch)
        }
        _ => Err(RestError { message: CHANNEL_UNOWNED.into(), ..Default::default() }),
    }
}

/// The production REST client (`app.http()`).
pub struct SerenityRest;

fn offline() -> RestError {
    RestError { code: None, status: None, message: "Discord is not connected".into(), timeout: false }
}

impl DiscordRest for SerenityRest {
    fn send_message(&self, app: App, channel_id: ChannelId, body: Value) -> BoxFuture<'static, Result<String, RestError>> {
        Box::pin(async move {
            let http = app.http().ok_or_else(offline)?;
            http.send_message(channel_id, Vec::new(), &body).await.map(|m| m.id.to_string()).map_err(|e| RestError::of(&e))
        })
    }

    fn fetch_channel(&self, app: App, channel_id: ChannelId) -> BoxFuture<'static, Result<Option<GuildChannel>, RestError>> {
        Box::pin(async move {
            let http = app.http().ok_or_else(offline)?;
            match http.get_channel(channel_id).await {
                Ok(serenity::all::Channel::Guild(c)) => Ok(Some(c)),
                Ok(_) => Ok(None),
                Err(e) => Err(RestError::of(&e)),
            }
        })
    }

    fn messages_after(&self, app: App, channel_id: ChannelId, after: MessageId, limit: u8) -> BoxFuture<'static, Result<Vec<MessageLite>, RestError>> {
        Box::pin(async move {
            let http = app.http().ok_or_else(offline)?;
            let msgs = http
                .get_messages(channel_id, Some(serenity::all::MessagePagination::After(after)), Some(limit))
                .await
                .map_err(|e| RestError::of(&e))?;
            Ok(msgs
                .iter()
                .map(|m| MessageLite { bot: m.author.bot || m.webhook_id.is_some(), system: is_system(m) })
                .collect())
        })
    }
}

/// discord.js `message.system`: a message type other than the user-sent ones.
fn is_system(m: &serenity::all::Message) -> bool {
    use serenity::all::MessageType as T;
    !matches!(m.kind, T::Regular | T::InlineReply | T::ChatInputCommand | T::ContextMenuCommand)
}

#[derive(Default)]
struct Seams {
    fetcher: RwLock<Option<Arc<dyn FeedFetcher>>>,
    rest: RwLock<Option<Arc<dyn DiscordRest>>>,
}

/// The fetcher of this app.
pub fn fetcher(app: &App) -> Arc<dyn FeedFetcher> {
    let seams = app.ext::<Seams>();
    let current = seams.fetcher.read().map(|g| g.clone()).unwrap_or(None);
    current.unwrap_or_else(|| Arc::new(SafeFetcher))
}

/// Replaces the fetcher of this app (`None` restores the SSRF-safe one).
pub fn set_fetcher(app: &App, f: Option<Arc<dyn FeedFetcher>>) {
    let seams = app.ext::<Seams>();
    if let Ok(mut g) = seams.fetcher.write() {
        *g = f;
    }
}

/// The Discord REST client of this app.
pub fn rest(app: &App) -> Arc<dyn DiscordRest> {
    let seams = app.ext::<Seams>();
    let current = seams.rest.read().map(|g| g.clone()).unwrap_or(None);
    current.unwrap_or_else(|| Arc::new(SerenityRest))
}

/// Replaces the Discord REST client of this app (`None` restores `app.http()`).
pub fn set_rest(app: &App, r: Option<Arc<dyn DiscordRest>>) {
    let seams = app.ext::<Seams>();
    if let Ok(mut g) = seams.rest.write() {
        *g = r;
    }
}
