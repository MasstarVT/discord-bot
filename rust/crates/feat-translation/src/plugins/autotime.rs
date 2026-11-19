//! Port of `src/plugins/autotime.js`: answers times typed in chat ("fri 5pm", "tomorrow at 18:30", "7pm EST") with
//! Discord timestamps, which every reader sees in their own timezone. A time is read in the author's zone: their own
//! (`/timezone set`), else their timezone role. Members with neither are answered only when the text names the zone,
//! and are told (at most hourly) to pick a timezone role or use `/timezone set`; the server timezone is never assumed.
//! Which text counts as a time: `bot_util::time_mentions`.

use std::sync::Arc;

use bot_core::interaction_helpers::{PermTarget, cached_guild, missing_bot_permissions};
use bot_core::prelude::*;
use bot_db::GuildSettings;
use bot_util::time::discord_timestamp_ms;
use bot_util::time_mentions::{ReadTime, extract_time_spans, may_mention_time, read_time_spans};
use bot_util::timezones::{ZoneSource, normalize_timezone_roles};
use serenity::all::{ChannelId, Guild, GuildChannel, GuildId, Message, RoleId};
use serenity::builder::{CreateAllowedMentions, CreateMessage};

use crate::translation_service::ReplyError;
use crate::user_timezone_service::{get_user_timezone, resolve_member_timezone};

/// `AUTOTIME.EMOJI`.
pub const EMOJI: &str = "🕒";
/// One answer per member per channel this often, so a burst of messages gets one reply.
pub const MEMBER_COOLDOWN_MS: u64 = 5_000;
/// The same instant is answered once per channel this often ("fri 5pm?" "yes fri 5pm").
pub const REPEAT_COOLDOWN_MS: u64 = 10 * 60_000;
/// A time that passed less than this long ago is still answered ("the raid started at 8pm").
pub const PAST_GRACE_MS: f64 = 60.0 * 60_000.0;
/// A member with no timezone is told how to set one once per server this often.
pub const ZONE_HINT_COOLDOWN_MS: u64 = 60 * 60_000;
/// `extractTimeSpans`'s default cap.
const SPAN_MAX: usize = 10;

/// Unknown channel or message, missing access or permission: the target is gone, nothing to retry.
const GONE_CODES: [u64; 4] = [10003, 10008, 50001, 50013];

/// The three cooldowns (`createThrottle`), one set per app, on the app clock (tests move them with the
/// `ManualClock`).
pub struct AutotimeThrottles {
    member: bot_util::async_util::Throttle,
    repeat: bot_util::async_util::Throttle,
    zone_hint: bot_util::async_util::Throttle,
}

impl AutotimeThrottles {
    fn new(app: &App) -> Self {
        let c = app.clock();
        Self {
            member: bot_core::clock::clock_throttle(c, MEMBER_COOLDOWN_MS),
            repeat: bot_core::clock::clock_throttle(c, REPEAT_COOLDOWN_MS),
            zone_hint: bot_core::clock::clock_throttle(c, ZONE_HINT_COOLDOWN_MS),
        }
    }
}

/// The app's [`AutotimeThrottles`], built on first use.
#[derive(Default)]
struct AutotimeState(std::sync::OnceLock<AutotimeThrottles>);

fn throttles(app: &App) -> Arc<AutotimeState> {
    let st = app.ext::<AutotimeState>();
    st.0.get_or_init(|| AutotimeThrottles::new(app));
    st
}

/// Echoes typed text inside a code span: no span breakers, bounded.
fn shown(raw: &str) -> String {
    let cleaned: String = raw.chars().filter(|c| *c != '`' && *c != '\\').collect();
    let s = bot_util::text::clamp_text(bot_util::js::trim(&cleaned), 60);
    if s.is_empty() { "(time)".to_string() } else { s }
}

/// The channel a message was sent in, as `isIgnoredChannel` reads it: its id, its parent (a thread's channel or a
/// channel's category) and the parent's parent.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChannelInfo {
    pub id: String,
    pub parent_id: Option<String>,
    pub grandparent_id: Option<String>,
    pub is_thread: bool,
}

impl ChannelInfo {
    /// From the gateway channel (or the cache), looking the parent's parent up in the cached guild.
    pub fn resolve(app: &App, guild_id: GuildId, channel_id: ChannelId, channel: Option<&GuildChannel>) -> Self {
        let cached = match channel {
            Some(c) => Some(c.clone()),
            None => bot_core::interaction_helpers::cached_channel(app, guild_id, channel_id),
        };
        let parent_id = cached.as_ref().and_then(|c| c.parent_id);
        let grandparent_id = parent_id.and_then(|p| bot_core::interaction_helpers::cached_channel(app, guild_id, p)).and_then(|p| p.parent_id);
        Self {
            id: channel_id.to_string(),
            parent_id: parent_id.map(|p| p.to_string()),
            grandparent_id: grandparent_id.map(|p| p.to_string()),
            is_thread: cached.as_ref().is_some_and(bot_core::permissions::is_thread),
        }
    }
}

/// `isIgnoredChannel(settings, channel)`: the channel, its parent or the parent's category is on
/// `GuildSettings.autoTimeIgnoredChannelIds`.
pub fn is_ignored_channel(settings: Option<&GuildSettings>, channel: Option<&ChannelInfo>) -> bool {
    let (Some(s), Some(c)) = (settings, channel) else { return false };
    let list = &s.auto_time_ignored_channel_ids;
    if list.is_empty() {
        return false;
    }
    [Some(&c.id), c.parent_id.as_ref(), c.grandparent_id.as_ref()].into_iter().flatten().any(|id| list.contains(id))
}

/// `mayAnswer(ctx)`: the sync hook filter: the text may hold a time, and the memoised settings (when there are any)
/// do not ignore the channel. Runs on every message, so it never awaits.
pub fn may_answer(app: &App, guild_id: &str, content: &str, channel: &ChannelInfo) -> bool {
    if !may_mention_time(content) {
        return false;
    }
    match app.settings().peek_settings(guild_id) {
        None => true,
        Some(s) => !is_ignored_channel(Some(&s), Some(channel)),
    }
}

/// `answerText(times)`: each time as a full date and a relative time, which Discord shows in every reader's own
/// timezone. The author's zone is not shown (a personal zone is private). Times are `(typed text, epoch ms)`.
pub fn answer_text(times: &[(String, f64)]) -> String {
    let mut lines: Vec<String> = times
        .iter()
        .map(|(text, ms)| format!("{EMOJI} `{}` → {} ({})", shown(text), discord_timestamp_ms(*ms, "F"), discord_timestamp_ms(*ms, "R")))
        .collect();
    lines.push("-# Everyone sees this in their own timezone.".to_string());
    lines.join("\n")
}

/// `zoneHintText(text, hasRoles)`: the note for a member with no timezone. Timezone roles are offered only when the
/// server has some.
pub fn zone_hint_text(text: &str, has_roles: bool) -> String {
    let how = if has_roles { "Pick a timezone role, or save yours with `/timezone set`." } else { "Save yours with `/timezone set`." };
    format!("⚠️ I can't convert `{}` without your timezone. {how}", shown(text))
}

/// The message fields the plugin reads.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TimeMessage {
    pub guild_id: String,
    pub channel: ChannelInfo,
    pub author_id: String,
    pub content: String,
    /// The roles of the author's member.
    pub member_roles: Vec<String>,
}

impl TimeMessage {
    /// From a `messageCreate` payload.
    pub fn of(app: &App, data: &MessageCreate) -> Self {
        let member_roles: Vec<RoleId> = match (&data.member, &data.message.member) {
            (Some(m), _) => m.roles.clone(),
            (None, Some(p)) => p.roles.clone(),
            _ => Vec::new(),
        };
        Self {
            guild_id: data.guild_id.to_string(),
            channel: ChannelInfo::resolve(app, data.guild_id, data.channel_id, data.channel.as_ref()),
            author_id: data.message.author.id.to_string(),
            content: data.message.content.clone(),
            member_roles: member_roles.iter().map(|r| r.to_string()).collect(),
        }
    }
}

/// `authorZone(ctx, settings)`: the author's own zone, when they have one: a saved zone or a timezone role.
/// `Some(None)` for the server timezone or UTC; `None` when they have no timezone role and their saved zone could not
/// be looked up.
pub async fn author_zone(app: &App, msg: &TimeMessage, guild: Option<&Guild>, settings: &GuildSettings) -> Option<Option<String>> {
    let personal = get_user_timezone(app, &msg.author_id, false).await; // None when the lookup failed: the role still counts
    let resolved = resolve_member_timezone(&msg.member_roles, guild, Some(settings), personal.clone().flatten().as_deref());
    if matches!(resolved.source, ZoneSource::Personal | ZoneSource::Role) {
        return Some(Some(resolved.zone));
    }
    if personal.is_none() { None } else { Some(None) }
}

/// What to answer in a message: the times, and the note on setting a timezone.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Answer {
    pub times: Vec<ReadTime>,
    pub zone_hint: Option<String>,
}

fn guild_of(app: &App, guild_id: &str) -> Option<Guild> {
    guild_id.parse::<u64>().ok().filter(|v| *v != 0).and_then(|g| cached_guild(app, GuildId::new(g)))
}

/// `readMessage(ctx, now)`: what to answer, after the channel, zone and cooldown checks.
pub async fn read_message(app: &App, msg: &TimeMessage, settings: Option<&GuildSettings>, now_ms: f64) -> Answer {
    let Some(settings) = settings else { return Answer::default() };
    if is_ignored_channel(Some(settings), Some(&msg.channel)) {
        return Answer::default();
    }
    let spans = extract_time_spans(&msg.content, SPAN_MAX);
    if spans.is_empty() {
        return Answer::default();
    }
    let guild = guild_of(app, &msg.guild_id);
    // The zone lookup only happens once the text holds a time, and not at all when every time names its zone.
    let unzoned = spans.iter().find(|s| s.zone.is_none()).cloned();
    let zone = if unzoned.is_some() { author_zone(app, msg, guild.as_ref(), settings).await } else { Some(None) };
    let times = read_time_spans(
        &spans,
        zone.clone().flatten().as_deref(),
        now_ms,
        PAST_GRACE_MS,
        bot_config::tables::limits::AUTOTIME_PER_MESSAGE,
    );
    let no_zone = unzoned.is_some() && zone == Some(None); // None: the lookup failed, so they may well have one
    let st = throttles(app);
    let Some(th) = st.0.get() else { return Answer::default() };
    if (times.is_empty() && !no_zone) || !th.member.allow(&format!("{}:{}", msg.channel.id, msg.author_id)) {
        return Answer::default();
    }
    let null = serde_json::Value::Null;
    let has_roles = normalize_timezone_roles(settings.timezone_roles.as_ref().unwrap_or(&null)).iter().any(|l| {
        guild.as_ref().is_some_and(|g| l.role_id.parse::<u64>().ok().filter(|v| *v != 0).is_some_and(|r| g.roles.contains_key(&RoleId::new(r))))
    });
    let times = times.into_iter().filter(|t| th.repeat.allow(&format!("{}:{}", msg.channel.id, bot_util::js::number_to_string(t.date_ms)))).collect();
    let zone_hint = match &unzoned {
        Some(u) if no_zone && th.zone_hint.allow(&format!("{}:{}", msg.guild_id, msg.author_id)) => Some(zone_hint_text(&u.text, has_roles)),
        _ => None,
    };
    Answer { times, zone_hint }
}

/// `timesToAnswer(ctx, { now })`: the times to answer in a message; empty for none.
pub async fn times_to_answer(app: &App, msg: &TimeMessage, settings: Option<&GuildSettings>, now_ms: f64) -> Vec<ReadTime> {
    read_message(app, msg, settings, now_ms).await.times
}

/// The reply text for an [`Answer`], or `None` when there is nothing to say.
pub fn reply_content(answer: &Answer) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if !answer.times.is_empty() {
        parts.push(answer_text(&answer.times.iter().map(|t| (t.text.clone(), t.date_ms)).collect::<Vec<_>>()));
    }
    if let Some(h) = &answer.zone_hint {
        parts.push(h.clone());
    }
    if parts.is_empty() { None } else { Some(parts.join("\n")) }
}

/// The permissions the bot needs to reply in the channel (`SendMessagesInThreads` in a thread).
pub fn reply_needs(channel: &ChannelInfo) -> [&'static str; 3] {
    ["ViewChannel", if channel.is_thread { "SendMessagesInThreads" } else { "SendMessages" }, "ReadMessageHistory"]
}

/// `handleMessage(ctx)`: answers the times in one message, if any, and tells a member with no timezone how to set
/// one. `reply` sends the text (the gateway message's reply in production); the bot's permissions are checked first.
pub async fn handle_message<R, RF>(app: &App, msg: &TimeMessage, settings: Option<&GuildSettings>, reply: R)
where
    R: FnOnce(String) -> RF,
    RF: std::future::Future<Output = Result<(), ReplyError>>,
{
    let answer = read_message(app, msg, settings, app.clock().now_ms() as f64).await;
    let Some(content) = reply_content(&answer) else { return };
    let (Some(g), Some(c)) = (
        msg.guild_id.parse::<u64>().ok().filter(|v| *v != 0),
        msg.channel.id.parse::<u64>().ok().filter(|v| *v != 0),
    ) else {
        return;
    };
    if !missing_bot_permissions(app, PermTarget::Channel(GuildId::new(g), ChannelId::new(c)), &reply_needs(&msg.channel)).is_empty() {
        return;
    }
    if let Err(e) = reply(content).await
        && !e.code.is_some_and(|code| GONE_CODES.contains(&code))
    {
        bot_util::log_warn!(format!("[autotime] {}: could not reply: {}", msg.guild_id, e.message));
    }
}

/// Replies to a message without pinging anyone (`failIfNotExists: false`).
pub async fn send_reply(app: &App, message: &Message, content: String) -> Result<(), ReplyError> {
    let Some(http) = app.http() else { return Err(ReplyError { code: None, message: "Discord is not connected".into() }) };
    let reference = serenity::all::MessageReference::from(message).fail_if_not_exists(false);
    let msg = CreateMessage::new().content(content).allowed_mentions(CreateAllowedMentions::new().replied_user(false)).reference_message(reference);
    message.channel_id.send_message(http.as_ref(), msg).await.map(|_| ()).map_err(|e| ReplyError::of(&e))
}

async fn on_message(ctx: Arc<HookCtx<MessageCreate>>) -> BotResult {
    let msg = TimeMessage::of(&ctx.app, &ctx.data);
    let settings = ctx.get_settings().await;
    let app = ctx.app.clone();
    let message = ctx.data.message.clone();
    handle_message(&ctx.app, &msg, settings.as_deref(), |content| async move { send_reply(&app, &message, content).await }).await;
    Ok(())
}

/// The plugin (`src/plugins/autotime.js`).
pub fn plugin() -> Plugin {
    Plugin::new("autotime", Some(ModuleKey::Autotime)).hook(
        // HOOK_ORDER.messageCreate: last, after automod has removed what it removes.
        Hook::<MessageCreate>::new("autotime", on_message).filter(|ctx| {
            // The cheap text check first: the channel is resolved only for text that may hold a time.
            if !may_mention_time(&ctx.data.message.content) {
                return false;
            }
            let channel = ChannelInfo::resolve(&ctx.app, ctx.data.guild_id, ctx.data.channel_id, ctx.data.channel.as_ref());
            may_answer(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.message.content, &channel)
        }),
    )
}
