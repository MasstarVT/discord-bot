//! Port of `src/services/autoThreadService.js`: a thread on every member message in chosen channels, and
//! auto-publishing of announcement posts. The lists live in `GuildSettings` (`autoThreadChannels`,
//! `autoPublishChannelIds`), so the hot path reads them from the settings memo and never queries Postgres. Hook handling
//! and the close button: [`crate::plugins::auto_threads`]; the command: [`crate::commands::autothread`].

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bot_config::{ModuleKey, limits};
use bot_core::App;
use bot_core::interaction_helpers::{PermTarget, missing_bot_permissions};
use bot_core::permissions::permission_names;
use bot_core::settings::{Settings, SettingsPatch, is_module_enabled};
use bot_db::GuildSettings;
use bot_util::jsre::{JsMatch, JsRegex, LazyRe};
use bot_util::lazy_re;
use bot_util::placeholder_parser::{PlaceholderChannel, PlaceholderGuild, PlaceholderMember, PlaceholderUser, RenderContext, VarValue, render_placeholders, render_placeholders_detailed};
use bot_util::text::clamp_text_with;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use serenity::all::{ButtonStyle, ChannelId, ChannelType, GuildId, MessageFlags, MessageId, MessageReferenceKind, MessageType, ReactionType};
use serenity::builder::{CreateActionRow, CreateAllowedMentions, CreateButton, CreateMessage};

/// `AUTOTHREAD`.
pub mod consts {
    /// GuildSettings stores the archive time as one of these strings (the dashboard's enum).
    pub const ARCHIVE_CHOICES: [&str; 4] = ["60", "1440", "4320", "10080"];
    pub const DEFAULT_ARCHIVE: &str = "1440";
    pub const DEFAULT_NAME: &str = "{content}";
    /// Discord's limit for a thread name.
    pub const NAME_MAX: usize = 100;
    /// How much of a message's first line is read for {content}.
    pub const LINE_MAX: usize = 300;
    /// How many lines are looked at to find the first one with text.
    pub const LINES_MAX: usize = 5;
    /// A channel the bot cannot use is complained about in the log at most this often.
    pub const WARN_MS: u64 = 60 * 60_000;
    /// Discord allows about this many crossposts per channel per hour.
    pub const PUBLISH_MAX: usize = 10;
    pub const PUBLISH_WINDOW_MS: i64 = 60 * 60_000;
    pub const PUBLISH_MAX_CHANNELS: usize = 1_000;
    pub const CLOSE_TEXT: &str = "Done here? The person who started this thread, or a moderator, can close it.";
}

/// `AUTOTHREAD.ARCHIVE_LABELS`.
pub fn archive_label(minutes: &str) -> &'static str {
    match minutes {
        "60" => "1 hour",
        "1440" => "1 day",
        "4320" => "3 days",
        "10080" => "1 week",
        _ => "undefined",
    }
}

/// Unknown channel or message, missing access or permission.
const PERMISSION_CODES: [u64; 2] = [50001, 50013];
/// 10008 unknown message, 160004 the message already has a thread, 40033 already crossposted, 50068 invalid type.
const HARMLESS_CODES: [u64; 5] = [10003, 10008, 160004, 40033, 50068];

/// `AutoThreadError`: an admin-facing refusal; `title` and the message are shown as they are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoThreadError {
    pub code: &'static str,
    pub title: String,
    pub message: String,
}

impl std::fmt::Display for AutoThreadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AutoThreadError {}

// ── Reading the lists ─────────────────────────────────────────────────────────

fn is_id(v: &str) -> bool {
    (5..=25).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_digit())
}

/// One stored entry (`{ channelId, name, archiveMinutes, closeButton }`), key order as the JS writes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    #[serde(rename = "channelId")]
    pub channel_id: String,
    pub name: String,
    #[serde(rename = "archiveMinutes")]
    pub archive_minutes: String,
    #[serde(rename = "closeButton")]
    pub close_button: bool,
}

/// `String(v ?? '')` of a JSON value.
fn js_string(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.as_f64().map(bot_util::js::number_to_string).unwrap_or_default(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Array(a)) => a.iter().map(|x| if x.is_null() { String::new() } else { js_string(Some(x)) }).collect::<Vec<_>>().join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// `normalizeEntry(raw)`: one stored entry, or `None` when it names no channel. Anything else that is off falls
/// back to the default.
pub fn normalize_entry(raw: &Value) -> Option<Entry> {
    let obj = raw.as_object()?;
    let channel_id = obj.get("channelId").and_then(Value::as_str).filter(|s| is_id(s))?;
    let name = obj.get("name").and_then(Value::as_str).map(bot_util::js::trim).unwrap_or("");
    let archive = js_string(obj.get("archiveMinutes"));
    Some(Entry {
        channel_id: channel_id.to_string(),
        name: if name.is_empty() { consts::DEFAULT_NAME.to_string() } else { bot_util::js::utf16_prefix(name, consts::NAME_MAX).to_string() },
        archive_minutes: if consts::ARCHIVE_CHOICES.contains(&archive.as_str()) { archive } else { consts::DEFAULT_ARCHIVE.to_string() },
        close_button: obj.get("closeButton") == Some(&Value::Bool(true)),
    })
}

/// `readEntries(settings)`: valid entries, first of a channel wins, capped.
pub fn read_entries(list: &Value) -> Vec<Entry> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for raw in list.as_array().into_iter().flatten() {
        let Some(entry) = normalize_entry(raw) else { continue };
        if !seen.insert(entry.channel_id.clone()) {
            continue;
        }
        out.push(entry);
        if out.len() >= limits::AUTOTHREAD_CHANNELS {
            break;
        }
    }
    out
}

/// `readPublishIds(settings)`: the channel ids to publish in, valid and unique, capped.
pub fn read_publish_ids(ids: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in ids {
        if is_id(id) && !out.contains(id) {
            out.push(id.clone());
        }
    }
    out.truncate(limits::AUTOPUBLISH_CHANNELS);
    out
}

/// The guild's lists as lookups (`channelIndex`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelIndex {
    pub threads: HashMap<String, Entry>,
    pub publish: HashSet<String>,
}

/// `channelIndex(settings)`: `threads` (channelId -> entry) and `publish`. Synchronous and free of I/O.
pub fn channel_index(settings: Option<&GuildSettings>) -> ChannelIndex {
    let Some(s) = settings else { return ChannelIndex::default() };
    ChannelIndex {
        threads: read_entries(&s.auto_thread_channels).into_iter().map(|e| (e.channel_id.clone(), e)).collect(),
        publish: read_publish_ids(&s.auto_publish_channel_ids).into_iter().collect(),
    }
}

/// `listedChannel(settings, channelId)`: the module is on and the channel is in either list. `None` when the
/// settings are unknown (not memoised yet).
pub fn listed_channel(settings: Option<&GuildSettings>, channel_id: &str) -> Option<bool> {
    let s = settings?;
    if !is_module_enabled(Some(s), Some(ModuleKey::Autothreads)) {
        return Some(false);
    }
    let index = channel_index(Some(s));
    Some(index.threads.contains_key(channel_id) || index.publish.contains(channel_id))
}

// ── Thread names ──────────────────────────────────────────────────────────────

/// `$1`-style expansion of a replacement string.
fn expand(m: &JsMatch, t: &[u16], with: &str) -> String {
    let mut out = String::new();
    let mut chars = with.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$'
            && let Some(d) = chars.peek().and_then(|d| d.to_digit(10))
        {
            chars.next();
            out.push_str(&m.group(t, d as usize).unwrap_or_default());
        } else {
            out.push(c);
        }
    }
    out
}

fn sub(re: &JsRegex, s: &str, with: &str) -> String {
    re.replace_all_with(s, |m, t| expand(m, t, with))
}

static SPOILER: LazyRe = lazy_re!(r"\|\|[\s\S]*?\|\|", "g");
static CUSTOM_EMOJI: LazyRe = lazy_re!(r"<a?:(\w{2,32}):\d{15,25}>", "g");
static MENTION_TAG: LazyRe = lazy_re!(r"<(?:@[!&]?|#)\d{15,25}>", "g");
static SLASH_MENTION: LazyRe = lazy_re!(r"<\/([\w -]{1,32}):\d{15,25}>", "g");
static TIMESTAMP: LazyRe = lazy_re!(r"<t:-?\d{1,15}(?::[a-zA-Z])?>", "g");
static EVERYONE: LazyRe = lazy_re!(r"@\u200b?(everyone|here)\b", "gi");
static LEAD_MARK: LazyRe = lazy_re!(r"^\s*(?:-#|#{1,3}|>{1,3}|[-*+])\s+", "");
static LINK: LazyRe = lazy_re!(r"\[([^\]]{1,200})\]\(\s*<?[^)\s]{1,500}>?\s*\)", "g");
static FENCE: LazyRe = lazy_re!(r"```[a-z0-9+-]*", "gi");
static CODE: LazyRe = lazy_re!(r"`([^`]+)`", "g");
static EMPHASIS: LazyRe = lazy_re!(r"\*\*([^*]+)\*\*|\*([^*]+)\*|~~([^~]+)~~|__([^_]+)__", "g");
static UNDERSCORE: LazyRe = lazy_re!(r"(^|[\s(])_([^_\s][^_]*?)_(?=$|[\s).,!?])", "g");
static AT_HASH: LazyRe = lazy_re!(r"(^|[\s(\[])[@#]", "g");
static MARKUP_CHARS: LazyRe = lazy_re!(r"[<>*~|\\]", "g");
static CONTROL: LazyRe = lazy_re!(r"[\u0000-\u001f\u007f\u200b-\u200f\u2028\u2029]", "g");
static SPACES: LazyRe = lazy_re!(r"\s+", "g");
static LINE_SPLIT: LazyRe = lazy_re!(r"\r?\n", "g");
static HAS_WORD: LazyRe = lazy_re!(r"[\p{L}\p{N}]", "u");
static TIDY_CONTROL: LazyRe = lazy_re!(r"[\u0000-\u001f\u007f\u2028\u2029]", "g");

/// `plainText(text)`: message text as plain words (markdown, mention and emoji tags, spoilers and quote/heading
/// marks removed).
pub fn plain_text(text: &str) -> String {
    let mut s = bot_util::js::utf16_prefix(text, consts::LINE_MAX).to_string();
    s = SPOILER.replace_all(&s, " ");
    s = sub(&CUSTOM_EMOJI, &s, ":$1:");
    s = MENTION_TAG.replace_all(&s, " ");
    s = sub(&SLASH_MENTION, &s, "/$1");
    s = TIMESTAMP.replace_all(&s, " ");
    s = EVERYONE.replace_all(&s, " ");
    s = LEAD_MARK.replace_first_with(&s, |_, _| String::new());
    s = sub(&LINK, &s, "$1");
    s = FENCE.replace_all(&s, " ");
    s = sub(&CODE, &s, "$1");
    s = EMPHASIS.replace_all_with(&s, |m, t| (1..=4).find_map(|i| m.group(t, i)).unwrap_or_default());
    s = sub(&UNDERSCORE, &s, "$1$2");
    s = sub(&AT_HASH, &s, "$1");
    s = MARKUP_CHARS.replace_all(&s, " ");
    s = CONTROL.replace_all(&s, " ");
    s = SPACES.replace_all(&s, " ");
    bot_util::js::trim(&s).to_string()
}

/// `String.prototype.split(/\r?\n/, limit)`.
fn split_lines(text: &str, limit: usize) -> Vec<String> {
    let t = bot_util::jsre::u16s(text);
    let mut out = Vec::new();
    let mut last = 0;
    for m in LINE_SPLIT.find_all(&t) {
        if out.len() >= limit {
            return out;
        }
        out.push(String::from_utf16_lossy(&t[last..m.start]));
        last = m.end;
    }
    if out.len() < limit {
        out.push(String::from_utf16_lossy(&t[last..]));
    }
    out
}

/// `contentTitle(text)`: the first line of the message that has any words left once its markup is removed.
pub fn content_title(text: &str) -> String {
    for line in split_lines(text, consts::LINES_MAX) {
        let plain = plain_text(&line);
        if HAS_WORD.test(&plain) {
            return plain;
        }
    }
    String::new()
}

/// What a thread name is made from (`threadName(template, source)`).
#[derive(Debug, Clone, Default)]
pub struct NameSource {
    pub member: Option<PlaceholderMember>,
    pub user: Option<PlaceholderUser>,
    pub guild: Option<PlaceholderGuild>,
    pub channel: Option<PlaceholderChannel>,
    /// Usually `message.cleanContent`.
    pub content: String,
    /// The message's creation time (epoch ms), `None` for now.
    pub created_at_ms: Option<f64>,
    /// `Date.now()`.
    pub now_ms: f64,
}

fn tidy(s: &str) -> String {
    let s = TIDY_CONTROL.replace_all(s, " ");
    let s = SPACES.replace_all(&s, " ");
    clamp_text_with(bot_util::js::trim(&s), consts::NAME_MAX, "")
}

/// `threadName(template, source)`: the template through the placeholder engine (`{user}` is the display name,
/// `{content}` the start of the message, `{channel}` the channel's name, `{date}` the UTC date), whitespace collapsed,
/// at most 100 characters. A name that comes out empty is the author's display name, else `Thread`.
pub fn thread_name(template: &str, src: &NameSource) -> String {
    let display = src
        .member
        .as_ref()
        .and_then(|m| m.display_name.clone())
        .or_else(|| src.user.as_ref().and_then(|u| u.global_name.clone()))
        .or_else(|| src.user.as_ref().and_then(|u| u.username.clone()))
        .unwrap_or_default();
    let author = plain_text(&display);
    let when = src.created_at_ms.unwrap_or(src.now_ms);
    let date = bot_util::js::to_iso_string(when).or_else(|| bot_util::js::to_iso_string(src.now_ms)).map(|s| s[..10].to_string()).unwrap_or_default();
    let ctx = RenderContext {
        member: src.member.clone(),
        user: src.user.clone(),
        guild: src.guild.clone(),
        channel: src.channel.clone(),
        vars: Some(vec![
            ("user".into(), VarValue::Text(author.clone())),
            ("content".into(), VarValue::Text(content_title(&src.content))),
            ("channel".into(), VarValue::Text(plain_text(src.channel.as_ref().and_then(|c| c.name.as_deref()).unwrap_or("")))),
            ("date".into(), VarValue::Text(date)),
        ]),
        args: None,
    };
    let tpl = if template.is_empty() { consts::DEFAULT_NAME } else { template };
    let rendered = tidy(&render_placeholders(tpl, &ctx, None));
    if !rendered.is_empty() {
        return rendered;
    }
    let a = tidy(&author);
    if a.is_empty() { "Thread".into() } else { a }
}

/// `unknownTokens(template)`: the placeholders in a name template that nothing knows.
pub fn unknown_tokens(template: &str) -> Vec<String> {
    let ctx = RenderContext {
        vars: Some(["user", "content", "channel", "date"].iter().map(|k| (k.to_string(), VarValue::Text(String::new()))).collect()),
        ..Default::default()
    };
    render_placeholders_detailed(template, &ctx, None).unknown_tokens
}

// ── Permissions ───────────────────────────────────────────────────────────────

/// `threadNeeds(entry)`: what the bot needs in a configured channel to start threads (and post the close button).
pub fn thread_needs(close_button: bool) -> &'static [&'static str] {
    if close_button { &["ViewChannel", "CreatePublicThreads", "SendMessagesInThreads"] } else { &["ViewChannel", "CreatePublicThreads"] }
}

/// `PUBLISH_NEEDS`: what the bot needs in an announcement channel to publish posts made by others.
pub const PUBLISH_NEEDS: [&str; 3] = ["ViewChannel", "SendMessages", "ManageMessages"];

/// `missingNames(channel, needs)`: the display names of the permissions the bot lacks there.
pub fn missing_names(app: &App, guild_id: GuildId, channel_id: ChannelId, needs: &[&str]) -> Vec<String> {
    permission_names(&missing_bot_permissions(app, PermTarget::Channel(guild_id, channel_id), needs))
}

/// Per-app state: the warn throttle, the publish limiter and the detached tasks.
pub struct AutoThreadState {
    warn: bot_util::async_util::Throttle,
    pub limiter: PublishLimiter,
    inflight: AtomicUsize,
    idle: tokio::sync::Notify,
}

impl Default for AutoThreadState {
    fn default() -> Self {
        Self {
            warn: bot_util::async_util::create_throttle(consts::WARN_MS),
            limiter: PublishLimiter::new(consts::PUBLISH_MAX, consts::PUBLISH_WINDOW_MS, consts::PUBLISH_MAX_CHANNELS),
            inflight: AtomicUsize::new(0),
            idle: tokio::sync::Notify::new(),
        }
    }
}

/// `warnOnce(kind, channel, text)`: logs a problem with a channel at most once per hour per kind.
fn warn_once(app: &App, kind: &str, guild_id: GuildId, channel_id: ChannelId, text: &str) {
    if app.ext::<AutoThreadState>().warn.allow(&format!("{kind}:{channel_id}")) {
        bot_util::log_warn!(format!("[auto-threads] {guild_id}: {text}"));
    }
}

// ── Starting threads ──────────────────────────────────────────────────────────

/// `closeRow(starterId)`: the row with the Close button, for the thread started by `starter_id`.
pub fn close_row(starter_id: &str) -> CreateActionRow {
    let id = bot_config::custom_id!("autothread", "close", starter_id).unwrap_or_default();
    CreateActionRow::Buttons(vec![CreateButton::new(id).label("Close thread").emoji(ReactionType::Unicode("🔒".into())).style(ButtonStyle::Secondary)])
}

fn code_of(e: &serenity::Error) -> Option<u64> {
    bot_core::discord::error_code(e)
}

/// `postCloseButton(thread, starterId)`: posts the Close button in a new thread. Never fails and never pings.
pub async fn post_close_button(app: &App, guild_id: GuildId, parent_id: ChannelId, thread_id: ChannelId, starter_id: &str) -> bool {
    let Some(http) = app.http() else { return false };
    let msg = CreateMessage::new().content(consts::CLOSE_TEXT).components(vec![close_row(starter_id)]).allowed_mentions(CreateAllowedMentions::new());
    match thread_id.send_message(http.as_ref(), msg).await {
        Ok(_) => true,
        Err(e) => {
            if !code_of(&e).is_some_and(|c| HARMLESS_CODES.contains(&c)) {
                warn_once(app, "close", guild_id, parent_id, &format!("could not post the close button: {}", bot_core::discord::error_message(&e)));
            }
            false
        }
    }
}

/// `detach(promise)`: runs `fut` in the background; tracked so [`settled`] can wait for it.
pub fn detach<F>(app: &App, fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let state = app.ext::<AutoThreadState>();
    state.inflight.fetch_add(1, Ordering::SeqCst);
    tokio::spawn(async move {
        fut.await;
        if state.inflight.fetch_sub(1, Ordering::SeqCst) == 1 {
            state.idle.notify_waiters();
        }
    });
}

/// `settled()`: resolves when every detached task has finished (tests).
pub async fn settled(app: &App) {
    let state = app.ext::<AutoThreadState>();
    loop {
        let notified = state.idle.notified();
        if state.inflight.load(Ordering::SeqCst) == 0 {
            return;
        }
        notified.await;
    }
}

/// What [`create_thread`] and [`publish_message`] report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Created,
    Published,
    Skipped,
    Perms,
    Limited,
    Failed,
}

/// The message a thread is started on (what `createThread` reads).
#[derive(Debug, Clone)]
pub struct ThreadSource {
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    pub message_id: MessageId,
    pub author_id: String,
    pub name: NameSource,
    /// The channel is a thread (no thread is started on a message in a thread).
    pub in_thread: bool,
}

/// `createThread(message, entry)`: starts the thread for one member message; the close button is posted in the
/// background.
pub async fn create_thread(app: &App, src: &ThreadSource, entry: &Entry) -> Outcome {
    if src.in_thread {
        return Outcome::Skipped;
    }
    let missing = missing_names(app, src.guild_id, src.channel_id, thread_needs(entry.close_button));
    if !missing.is_empty() {
        warn_once(app, "perms", src.guild_id, src.channel_id, &format!("cannot start threads in {}: missing {}", src.channel_id, missing.join(", ")));
        return Outcome::Perms;
    }
    let Some(http) = app.http() else { return Outcome::Failed };
    let body = json!({
        "name": thread_name(&entry.name, &src.name),
        "auto_archive_duration": entry.archive_minutes.parse::<u32>().unwrap_or(1440),
    });
    let thread = match http.create_thread_from_message(src.channel_id, src.message_id, &body, Some("Auto Threads: new message")).await {
        Ok(t) => t,
        Err(e) => {
            let code = code_of(&e);
            if code.is_some_and(|c| PERMISSION_CODES.contains(&c)) {
                warn_once(app, "perms", src.guild_id, src.channel_id, &format!("cannot start threads in {}: Discord refused ({})", src.channel_id, code.unwrap_or(0)));
                return Outcome::Perms;
            }
            if !code.is_some_and(|c| HARMLESS_CODES.contains(&c)) {
                warn_once(app, "fail", src.guild_id, src.channel_id, &format!("could not start a thread in {}: {}", src.channel_id, bot_core::discord::error_message(&e)));
            }
            return Outcome::Failed;
        }
    };
    if entry.close_button {
        let (a, g, c, t, s) = (app.clone(), src.guild_id, src.channel_id, thread.id, src.author_id.clone());
        detach(app, async move {
            post_close_button(&a, g, c, t, &s).await;
        });
    }
    Outcome::Created
}

// ── Publishing ────────────────────────────────────────────────────────────────

/// Channel id -> (recency, hit times), with the recency counter.
type Hits = (u64, HashMap<String, (u64, Vec<i64>)>);

/// `createPublishLimiter`: counts crossposts per channel over a sliding window (in memory). Attempts are counted, not
/// successes. Times are passed in (epoch ms) so the app clock drives it.
pub struct PublishLimiter {
    max: usize,
    window_ms: i64,
    max_channels: usize,
    /// channelId -> (recency, hit times).
    hits: Mutex<Hits>,
}

impl PublishLimiter {
    pub fn new(max: usize, window_ms: i64, max_channels: usize) -> Self {
        Self { max, window_ms, max_channels, hits: Mutex::new((0, HashMap::new())) }
    }

    fn prune(list: &mut Vec<i64>, t: i64, window: i64) {
        list.retain(|h| *h > t - window);
    }

    /// Takes a slot for the channel; false when the window is full.
    pub fn take(&self, channel_id: &str, now: i64) -> bool {
        let mut g = self.hits.lock().unwrap_or_else(|e| e.into_inner());
        let (seq, hits) = &mut *g;
        let before_seq = hits.get(channel_id).map(|(s, _)| *s);
        let mut list = hits.remove(channel_id).map(|(_, l)| l).unwrap_or_default();
        Self::prune(&mut list, now, self.window_ms);
        if list.len() >= self.max {
            // Stays where it was in the recency order.
            let s = before_seq.unwrap_or_else(|| {
                *seq += 1;
                *seq
            });
            hits.insert(channel_id.to_string(), (s, list));
            return false;
        }
        list.push(now);
        *seq += 1;
        hits.insert(channel_id.to_string(), (*seq, list));
        if hits.len() > self.max_channels {
            let window = self.window_ms;
            hits.retain(|_, (_, l)| {
                Self::prune(l, now, window);
                !l.is_empty()
            });
            while hits.len() > self.max_channels {
                let Some(oldest) = hits.iter().min_by_key(|(_, (s, _))| *s).map(|(k, _)| k.clone()) else { break };
                hits.remove(&oldest);
            }
        }
        true
    }

    /// Gives back the channel's newest slot (a request Discord refused as invalid did not publish anything).
    pub fn release(&self, channel_id: &str) {
        let mut g = self.hits.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, l)) = g.1.get_mut(channel_id) {
            l.pop();
        }
    }

    /// Slots left in the channel's window.
    pub fn remaining(&self, channel_id: &str, now: i64) -> usize {
        let mut g = self.hits.lock().unwrap_or_else(|e| e.into_inner());
        match g.1.get_mut(channel_id) {
            Some((_, l)) => {
                Self::prune(l, now, self.window_ms);
                self.max.saturating_sub(l.len())
            }
            None => self.max,
        }
    }

    pub fn size(&self) -> usize {
        self.hits.lock().unwrap_or_else(|e| e.into_inner()).1.len()
    }

    pub fn clear(&self) {
        self.hits.lock().unwrap_or_else(|e| e.into_inner()).1.clear();
    }
}

/// What `isPublishable` reads from a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishInfo {
    pub kind: MessageType,
    pub has_poll: bool,
    pub forward: bool,
    pub flags: Option<MessageFlags>,
}

impl PublishInfo {
    pub fn of(m: &serenity::all::Message) -> Self {
        Self {
            kind: m.kind,
            has_poll: m.poll.is_some(),
            forward: m.message_reference.as_ref().is_some_and(|r| r.kind == MessageReferenceKind::Forward),
            flags: m.flags,
        }
    }
}

/// `isPublishable(message)`: a post Discord lets a channel publish (not a system message, a poll, a forward, a
/// crosspost, or a deferred reply's "thinking" placeholder).
pub fn is_publishable(m: &PublishInfo) -> bool {
    let types = [MessageType::Regular, MessageType::InlineReply, MessageType::ChatInputCommand, MessageType::ContextMenuCommand];
    if !types.contains(&m.kind) || m.has_poll || m.forward {
        return false;
    }
    if let Some(f) = m.flags
        && (f.contains(MessageFlags::CROSSPOSTED) || f.contains(MessageFlags::IS_CROSSPOST) || f.contains(MessageFlags::LOADING))
    {
        return false;
    }
    true
}

/// `publishMessage(message)`: publishes one post of an announcement channel to the servers that follow it.
pub async fn publish_message(app: &App, guild_id: GuildId, channel_id: ChannelId, message_id: MessageId, info: &PublishInfo) -> Outcome {
    let channel = bot_core::interaction_helpers::cached_channel(app, guild_id, channel_id);
    if channel.as_ref().map(|c| c.kind) != Some(ChannelType::News) || !is_publishable(info) {
        return Outcome::Skipped;
    }
    let missing = missing_names(app, guild_id, channel_id, &PUBLISH_NEEDS);
    if !missing.is_empty() {
        warn_once(app, "publish-perms", guild_id, channel_id, &format!("cannot publish in {channel_id}: missing {}", missing.join(", ")));
        return Outcome::Perms;
    }
    let state = app.ext::<AutoThreadState>();
    if !state.limiter.take(&channel_id.to_string(), app.clock().now_ms()) {
        warn_once(
            app,
            "publish-limit",
            guild_id,
            channel_id,
            &format!("{channel_id}: {} posts were published in the last hour; the next ones stay unpublished until the window clears", consts::PUBLISH_MAX),
        );
        return Outcome::Limited;
    }
    let Some(http) = app.http() else { return Outcome::Failed };
    match http.crosspost_message(channel_id, message_id).await {
        Ok(_) => Outcome::Published,
        Err(e) => {
            let code = code_of(&e);
            if code.is_some_and(|c| PERMISSION_CODES.contains(&c)) {
                warn_once(app, "publish-perms", guild_id, channel_id, &format!("cannot publish in {channel_id}: Discord refused ({})", code.unwrap_or(0)));
                return Outcome::Perms;
            }
            if code == Some(50068) {
                state.limiter.release(&channel_id.to_string());
            }
            if !code.is_some_and(|c| HARMLESS_CODES.contains(&c)) {
                warn_once(app, "publish-fail", guild_id, channel_id, &format!("could not publish in {channel_id}: {}", bot_core::discord::error_message(&e)));
            }
            Outcome::Failed
        }
    }
}

// ── Changing the lists ────────────────────────────────────────────────────────

fn list_of(s: &GuildSettings) -> Vec<Value> {
    s.auto_thread_channels.as_array().cloned().unwrap_or_default()
}

/// A change to one thread channel (`setThreadChannel`'s second argument): `None` keeps what is stored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadChange {
    pub channel_id: String,
    pub name: Option<String>,
    pub archive_minutes: Option<String>,
    pub close_button: Option<bool>,
}

/// What [`set_thread_channel`] did.
#[derive(Debug, Clone)]
pub struct SetResult {
    pub created: bool,
    pub entry: Entry,
    pub settings: Settings,
}

fn limit_error(text: String) -> anyhow::Error {
    AutoThreadError { code: "LIMIT", title: "Limit Reached".into(), message: text }.into()
}

/// `setThreadChannel(guildId, change)`: adds a channel to the thread list, or changes the options given for one
/// already there. Other entries are kept exactly as stored. Fails with [`AutoThreadError`] when the list is full.
pub async fn set_thread_channel(app: &App, guild_id: &str, change: ThreadChange) -> anyhow::Result<SetResult> {
    let out: Arc<Mutex<Option<(bool, Entry)>>> = Arc::default();
    let slot = out.clone();
    let settings = app
        .settings()
        .update_settings_with(guild_id, move |current| {
            let list = list_of(&current);
            let at = list.iter().position(|e| e.get("channelId").and_then(Value::as_str) == Some(change.channel_id.as_str()));
            if at.is_none() && list.len() >= limits::AUTOTHREAD_CHANNELS {
                return Err(limit_error(format!(
                    "A server can have at most **{}** auto-thread channels. Remove one with `/autothread remove` first.",
                    limits::AUTOTHREAD_CHANNELS
                )));
            }
            let base = at.and_then(|i| normalize_entry(&list[i])).unwrap_or_else(|| Entry {
                channel_id: change.channel_id.clone(),
                name: consts::DEFAULT_NAME.into(),
                archive_minutes: consts::DEFAULT_ARCHIVE.into(),
                close_button: false,
            });
            let entry = Entry {
                channel_id: change.channel_id.clone(),
                name: change.name.clone().unwrap_or(base.name),
                archive_minutes: change.archive_minutes.clone().unwrap_or(base.archive_minutes),
                close_button: change.close_button.unwrap_or(base.close_button),
            };
            let value = serde_json::to_value(&entry)?;
            let next: Vec<Value> = match at {
                None => list.into_iter().chain(std::iter::once(value)).collect(),
                Some(i) => list.into_iter().enumerate().map(|(j, e)| if j == i { value.clone() } else { e }).collect(),
            };
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some((at.is_none(), entry));
            Ok(SettingsPatch::new().set("autoThreadChannels", Value::Array(next)))
        })
        .await?;
    let (created, entry) = out.lock().unwrap_or_else(|e| e.into_inner()).take().ok_or_else(|| anyhow::anyhow!("setThreadChannel: no result"))?;
    Ok(SetResult { created, entry, settings })
}

/// `removeThreadChannel(guildId, channelId)`: whether it was there.
pub async fn remove_thread_channel(app: &App, guild_id: &str, channel_id: &str) -> anyhow::Result<bool> {
    let found = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (f, c) = (found.clone(), channel_id.to_string());
    app.settings()
        .update_settings_with(guild_id, move |current| {
            let list = list_of(&current);
            let next: Vec<Value> = list.iter().filter(|e| e.get("channelId").and_then(Value::as_str) != Some(c.as_str())).cloned().collect();
            let hit = next.len() != list.len();
            f.store(hit, Ordering::SeqCst);
            Ok(if hit { SettingsPatch::new().set("autoThreadChannels", Value::Array(next)) } else { SettingsPatch::new() })
        })
        .await?;
    Ok(found.load(Ordering::SeqCst))
}

/// What [`add_publish_channel`] did: `added` false when it was already there.
#[derive(Debug, Clone)]
pub struct AddPublish {
    pub added: bool,
    pub settings: Settings,
}

/// `addPublishChannel(guildId, channelId)`. Fails with [`AutoThreadError`] when the list is full.
pub async fn add_publish_channel(app: &App, guild_id: &str, channel_id: &str) -> anyhow::Result<AddPublish> {
    let added = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (a, c) = (added.clone(), channel_id.to_string());
    let settings = app
        .settings()
        .update_settings_with(guild_id, move |current| {
            let ids = current.auto_publish_channel_ids.clone();
            if ids.contains(&c) {
                return Ok(SettingsPatch::new());
            }
            if ids.len() >= limits::AUTOPUBLISH_CHANNELS {
                return Err(limit_error(format!(
                    "A server can publish in at most **{}** channels. Remove one with `/autothread publish remove` first.",
                    limits::AUTOPUBLISH_CHANNELS
                )));
            }
            a.store(true, Ordering::SeqCst);
            let mut next = ids;
            next.push(c);
            Ok(SettingsPatch::new().set("autoPublishChannelIds", json!(next)))
        })
        .await?;
    Ok(AddPublish { added: added.load(Ordering::SeqCst), settings })
}

/// `removePublishChannel(guildId, channelId)`: whether it was there.
pub async fn remove_publish_channel(app: &App, guild_id: &str, channel_id: &str) -> anyhow::Result<bool> {
    let found = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (f, c) = (found.clone(), channel_id.to_string());
    app.settings()
        .update_settings_with(guild_id, move |current| {
            let ids = current.auto_publish_channel_ids.clone();
            let hit = ids.contains(&c);
            f.store(hit, Ordering::SeqCst);
            Ok(if hit {
                SettingsPatch::new().set("autoPublishChannelIds", json!(ids.into_iter().filter(|i| *i != c).collect::<Vec<_>>()))
            } else {
                SettingsPatch::new()
            })
        })
        .await?;
    Ok(found.load(Ordering::SeqCst))
}

/// `dropChannel(guildId, channelId)`: drops a (deleted) channel from both lists; writes nothing when it is in
/// neither. Whether anything changed.
pub async fn drop_channel(app: &App, guild_id: &str, channel_id: &str) -> anyhow::Result<bool> {
    if guild_id.is_empty() || channel_id.is_empty() {
        return Ok(false);
    }
    let changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (f, c) = (changed.clone(), channel_id.to_string());
    app.settings()
        .update_settings_with(guild_id, move |current| {
            let list = list_of(&current);
            let ids = current.auto_publish_channel_ids.clone();
            let mut data = SettingsPatch::new();
            let threads: Vec<Value> = list.iter().filter(|e| e.get("channelId").and_then(Value::as_str) != Some(c.as_str())).cloned().collect();
            if threads.len() != list.len() {
                data = data.set("autoThreadChannels", Value::Array(threads));
            }
            if ids.contains(&c) {
                data = data.set("autoPublishChannelIds", json!(ids.into_iter().filter(|i| *i != c).collect::<Vec<_>>()));
            }
            f.store(!data.is_empty(), Ordering::SeqCst);
            Ok(data)
        })
        .await?;
    Ok(changed.load(Ordering::SeqCst))
}
