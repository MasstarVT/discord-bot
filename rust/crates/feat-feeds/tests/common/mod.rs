//! Test seams shared by `tests/feeds.rs` and `tests/reminders.rs`: a stub fetcher (the JS `__setFetchImpl`), a
//! recording Discord REST client, and a fake guild with a text channel.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use bot_core::App;
use bot_core::testing::{FakeGuild, TEST_BOT_ID};
use bot_util::safe_fetch::{FetchOptions, FetchResponse, SafeFetchCode, SafeFetchError};
use feat_feeds::net::{DiscordRest, FeedFetcher, FetchFailure, MessageLite, RestError};
use futures::future::BoxFuture;
use serde_json::Value;
use serenity::all::{ChannelId, GuildChannel, MessageId, Permissions};

pub type Route = dyn Fn(&str, &FetchOptions) -> Result<FetchResponse, FetchFailure> + Send + Sync;

/// A fetcher answering from `route`, recording every call.
pub struct StubFetcher {
    route: Box<Route>,
    pub calls: Mutex<Vec<(String, FetchOptions)>>,
}

impl StubFetcher {
    pub fn install(app: &App, route: impl Fn(&str, &FetchOptions) -> Result<FetchResponse, FetchFailure> + Send + Sync + 'static) -> Arc<StubFetcher> {
        let s = Arc::new(StubFetcher { route: Box::new(route), calls: Mutex::new(Vec::new()) });
        feat_feeds::net::set_fetcher(app, Some(s.clone()));
        s
    }

    pub fn len(&self) -> usize {
        self.calls.lock().map(|c| c.len()).unwrap_or(0)
    }

    pub fn call(&self, i: usize) -> (String, FetchOptions) {
        self.calls.lock().map(|c| c[i].clone()).unwrap_or_else(|_| (String::new(), FetchOptions::default()))
    }

    pub fn header(&self, i: usize, name: &str) -> Option<String> {
        self.call(i).1.headers.iter().rev().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }
}

impl FeedFetcher for StubFetcher {
    fn fetch(&self, url: String, opts: FetchOptions) -> BoxFuture<'static, Result<FetchResponse, FetchFailure>> {
        if let Ok(mut c) = self.calls.lock() {
            c.push((url.clone(), opts.clone()));
        }
        let r = (self.route)(&url, &opts);
        Box::pin(async move { r })
    }

    fn fetch_json(&self, url: String, opts: FetchOptions) -> BoxFuture<'static, Result<Value, FetchFailure>> {
        if let Ok(mut c) = self.calls.lock() {
            c.push((url.clone(), opts.clone()));
        }
        let r = (self.route)(&url, &opts).and_then(|res| {
            if !res.ok {
                return Err(FetchFailure::Safe(SafeFetchError::with_status(SafeFetchCode::HttpError, res.status)));
            }
            serde_json::from_slice(&res.buffer).map_err(|e| FetchFailure::Other(e.to_string()))
        });
        Box::pin(async move { r })
    }
}

/// A response like the JS tests' `resp(status, body, headers, url)`.
pub fn resp(status: u16, body: &str, headers: &[(&str, &str)]) -> Result<FetchResponse, FetchFailure> {
    Ok(FetchResponse {
        ok: (200..300).contains(&status),
        status,
        status_text: String::new(),
        headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        url: "https://feed.test/f.xml".into(),
        buffer: body.as_bytes().to_vec(),
    })
}

pub fn resp_at(status: u16, body: &str, headers: &[(&str, &str)], url: &str) -> Result<FetchResponse, FetchFailure> {
    resp(status, body, headers).map(|mut r| {
        r.url = url.into();
        r
    })
}

pub fn safe_err(code: SafeFetchCode, status: Option<u16>) -> Result<FetchResponse, FetchFailure> {
    let mut e = SafeFetchError::new(code, None);
    e.status = status;
    Err(FetchFailure::Safe(e))
}

pub const XML_HEADERS: [(&str, &str); 3] =
    [("content-type", "application/rss+xml"), ("etag", "\"abc\""), ("last-modified", "Fri, 09 Oct 2026 10:00:00 GMT")];

/// A Discord REST client that records sends and answers from settable behaviours.
#[derive(Default)]
pub struct RecRest {
    pub sent: Mutex<Vec<(ChannelId, Value)>>,
    pub fail_send: Mutex<Option<RestError>>,
    pub fail_once: Mutex<Vec<RestError>>,
    pub channel_fetch: Mutex<Option<Result<Option<GuildChannel>, RestError>>>,
    pub fetches: Mutex<usize>,
    pub history: Mutex<Option<Result<Vec<MessageLite>, RestError>>>,
}

impl RecRest {
    pub fn install(app: &App) -> Arc<RecRest> {
        let r = Arc::new(RecRest::default());
        feat_feeds::net::set_rest(app, Some(r.clone()));
        r
    }

    pub fn sent(&self) -> Vec<(ChannelId, Value)> {
        self.sent.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn count(&self) -> usize {
        self.sent.lock().map(|s| s.len()).unwrap_or(0)
    }

    pub fn fail_with(&self, e: Option<RestError>) {
        if let Ok(mut f) = self.fail_send.lock() {
            *f = e;
        }
    }

    pub fn on_fetch(&self, r: Option<Result<Option<GuildChannel>, RestError>>) {
        if let Ok(mut f) = self.channel_fetch.lock() {
            *f = r;
        }
    }
}

impl DiscordRest for RecRest {
    fn send_message(&self, _app: App, channel_id: ChannelId, body: Value) -> BoxFuture<'static, Result<String, RestError>> {
        let once = self.fail_once.lock().ok().and_then(|mut f| if f.is_empty() { None } else { Some(f.remove(0)) });
        let fail = once.or_else(|| self.fail_send.lock().ok().and_then(|f| f.clone()));
        let r = match fail {
            Some(e) => Err(e),
            None => {
                let mut s = self.sent.lock().unwrap_or_else(|e| e.into_inner());
                s.push((channel_id, body));
                Ok(format!("{}", 1_300_000_000_000_000_000u64 + s.len() as u64))
            }
        };
        Box::pin(async move { r })
    }

    fn fetch_channel(&self, _app: App, _channel_id: ChannelId) -> BoxFuture<'static, Result<Option<GuildChannel>, RestError>> {
        if let Ok(mut n) = self.fetches.lock() {
            *n += 1;
        }
        let r = self.channel_fetch.lock().ok().and_then(|f| f.clone()).unwrap_or_else(|| Err(RestError { message: "gateway hiccup".into(), ..Default::default() }));
        Box::pin(async move { r })
    }

    fn messages_after(&self, _app: App, _channel_id: ChannelId, _after: MessageId, _limit: u8) -> BoxFuture<'static, Result<Vec<MessageLite>, RestError>> {
        let r = self.history.lock().ok().and_then(|h| h.clone()).unwrap_or_else(|| Ok(Vec::new()));
        Box::pin(async move { r })
    }
}

pub const OWNER: &str = "600000000000000099";
pub const ADMIN: &str = "600000000000000009";
pub const BOT_ROLE: &str = "820000000000000001";
pub const ADMIN_ROLE: &str = "820000000000000002";

/// A guild where the bot can post in `channel` (named `news`) and `other`, with a mentionable role `role_ok` and a
/// non-mentionable `role_no`; `denied` channel overwrites take bot permissions away in `channel`.
pub fn fake_guild(guild: &str, channel: &str, denied: Permissions) -> FakeGuild {
    let bot = Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES | Permissions::EMBED_LINKS | Permissions::READ_MESSAGE_HISTORY;
    let mut g = FakeGuild::new(guild, OWNER)
        .role(BOT_ROLE, 1, bot)
        .member(TEST_BOT_ID, &[BOT_ROLE])
        .role(ADMIN_ROLE, 2, Permissions::MANAGE_GUILD)
        .member(ADMIN, &[ADMIN_ROLE])
        .role_with("820000000000000005", 0, Permissions::empty(), |v| v["mentionable"] = Value::Bool(true))
        .role("820000000000000006", 0, Permissions::empty());
    let ow: Vec<(&str, bool, Permissions, Permissions)> = if denied.is_empty() { Vec::new() } else { vec![(BOT_ROLE, false, Permissions::empty(), denied)] };
    g = g.channel(channel, 0, "news", &ow).channel("810000000000000077", 0, "other", &[]).channel("810000000000000078", 2, "voice", &[]);
    g
}

/// What a REST fetch of channel `id` would return: the cached `other` text channel of `guild`, renumbered and moved
/// to `owner` (another guild when it differs from `guild`).
pub fn rest_channel(app: &App, guild: &str, owner: &str, id: &str) -> GuildChannel {
    let g = serenity::all::GuildId::new(guild.parse().expect("guild id"));
    let mut ch = bot_core::interaction_helpers::cached_channel(app, g, ChannelId::new(810000000000000077)).expect("the other channel is cached");
    ch.id = ChannelId::new(id.parse().expect("channel id"));
    ch.guild_id = serenity::all::GuildId::new(owner.parse().expect("owner id"));
    ch
}

pub const ROLE_OK: &str = "820000000000000005";
pub const ROLE_NO: &str = "820000000000000006";
