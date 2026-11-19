//! Port of `src/dashboard/collections/feeds.js`: the `feeds` collection (Social Feeds). The source is entered exactly
//! as /feed add takes it and parsed by the same pure parsers (feat-feeds' feed_parsers).

use std::sync::LazyLock;

use feat_feeds::feed_parsers::{normalize_feed_url, parse_bluesky_actor_input, parse_subreddit_input, parse_youtube_channel_input};
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::index::{CheckCtx, Mode, Problem};
use crate::util::{raw, strip_undefined, iso, len16, merged, or_null};

/// `FEED_TYPE_OPTIONS` (value, label).
pub const FEED_TYPE_OPTIONS: [(&str, &str); 5] = [
    ("RSS", "RSS or Atom feed"),
    ("YOUTUBE", "YouTube channel"),
    ("REDDIT", "Subreddit"),
    ("BLUESKY", "Bluesky account"),
    ("PODCAST", "Podcast"),
];

/// The variables of a feed message (feedService.FEED_VARS).
pub const FEED_PLACEHOLDERS: [&str; 11] = [
    "{title}", "{url}", "{link}", "{author}", "{feed}", "{youtube_channel}", "{subreddit}", "{bluesky_name}", "{bluesky_handle}",
    "{podcast_title}", "{episode_title}",
];

/// The message option's limit (feedService.parseMessageOption).
pub const FEED_MESSAGE_MAX: usize = 1000;

static HOST_PATH_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)+/").expect("valid regex"));

/// `PODCAST_URL_RE` (`/^(https?:\/\/|feed:)|^[\w-]+(\.[\w-]+)+\/\S*/i`): the /feed add podcast rule.
fn is_podcast_url(s: &str) -> bool {
    let head: String = s.chars().take(8).collect::<String>().to_ascii_lowercase();
    head.starts_with("http://") || head.starts_with("https://") || head.starts_with("feed:") || HOST_PATH_RE.is_match(s)
}

/// What resolveFeedSource takes (`parseFeedSource`'s `input`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedInput {
    Url(String),
    ChannelId(String),
    Name(String),
    Actor(String),
    Query(String),
}

impl FeedInput {
    /// The JS object (`{ url }`, `{ channelId }`, `{ name }`, `{ actor }`, `{ query }`).
    pub fn to_json(&self) -> Value {
        match self {
            Self::Url(v) => json!({ "url": v }),
            Self::ChannelId(v) => json!({ "channelId": v }),
            Self::Name(v) => json!({ "name": v }),
            Self::Actor(v) => json!({ "actor": v }),
            Self::Query(v) => json!({ "query": v }),
        }
    }
}

fn plain(s: &str) -> String {
    s.replace("**", "")
}

/// `parseFeedSource(type, raw)`: parses a source as /feed add does (readAddSource).
pub fn parse_feed_source(ty: &str, raw: &str) -> Result<FeedInput, String> {
    match ty {
        "RSS" => normalize_feed_url(raw).map(FeedInput::Url).map_err(|r| plain(&r)),
        "YOUTUBE" => parse_youtube_channel_input(raw).map(FeedInput::ChannelId).map_err(|r| plain(&r.reason)),
        "REDDIT" => parse_subreddit_input(raw).map(FeedInput::Name).map_err(|r| plain(&r)),
        "BLUESKY" => parse_bluesky_actor_input(raw).map(FeedInput::Actor).map_err(|r| plain(&r)),
        "PODCAST" => {
            let s = bot_util::js::trim(raw);
            if is_podcast_url(s) {
                return normalize_feed_url(s).map(FeedInput::Url).map_err(|r| plain(&r));
            }
            if len16(s) < 2 {
                return Err("Type at least 2 characters of the podcast's name, or paste its RSS feed URL.".into());
            }
            Ok(FeedInput::Query(s.to_string()))
        }
        _ => Err("That kind of feed is not supported.".into()),
    }
}

/// `String(v ?? '')`.
fn text_of(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(v) => dash_schema::js::to_js_string(Some(v)),
    }
}

/// `checkFeed(data, { mode, current })`: the source must parse for its type (on create), and only Bluesky feeds
/// include reposts.
pub fn check_feed(data: &Map<String, Value>, ctx: &CheckCtx) -> Option<Problem> {
    let f = if ctx.mode == Mode::Update && ctx.current.is_some() { merged(ctx.current, data) } else { data.clone() };
    if ctx.mode == Mode::Create {
        let ty = text_of(f.get("type"));
        if let Err(message) = parse_feed_source(&ty, &text_of(f.get("source"))) {
            return Some(Problem::new("source", message));
        }
    }
    if f.get("includeReposts") == Some(&Value::Bool(true)) && f.get("type").and_then(Value::as_str) != Some("BLUESKY") {
        return Some(Problem::new("includeReposts", "Only Bluesky feeds can include reposts."));
    }
    None
}

/// `feedItem(row, { statusText, sourceUrl })`: a Feed row as a collection item.
pub fn feed_item(row: &Value, status_text: &str, source_url: Option<&str>) -> Value {
    strip_undefined(json!({
        "id": raw(row.get("id")),
        "type": raw(row.get("type")),
        "source": raw(row.get("source")),
        "channelId": raw(row.get("channelId")),
        "pingRoleId": or_null(row.get("pingRoleId")),
        "messageTemplate": or_null(row.get("messageTemplate")),
        "includeReposts": row.get("includeReposts") == Some(&Value::Bool(true)),
        "enabled": row.get("enabled") != Some(&Value::Bool(false)),
        "displayName": raw(row.get("displayName")),
        "statusText": status_text,
        "sourceUrl": source_url,
        "lastCheckedAt": iso(row.get("lastCheckedAt")),
        "lastPostedAt": iso(row.get("lastPostedAt")),
        "createdAt": iso(row.get("createdAt")),
    }))
}
