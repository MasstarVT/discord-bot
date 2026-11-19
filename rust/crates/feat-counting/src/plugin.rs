//! Port of `src/plugins/counting.js`: the counting game in `GuildSettings.countingChannelId`.
//!
//! A message that is only a number, or maths that comes to one (`2+2`), counts: the next number from a different
//! member gets ✅, anything else gets ❌ and, once the count has started, resets it to 0 with a note in the channel.
//! Other messages are chat and are ignored. Rules and state: [`crate::counting_service`].
//!
//! [`judge_message`] decides under the guild's lock and returns the replies as a plain list of [`Action`]s;
//! [`run_actions`] sends them once the lock is released, so REST calls never hold up the next count.

use std::sync::Arc;

use bot_core::interaction_helpers::{PermTarget, missing_bot_permissions};
use bot_core::prelude::*;
use bot_core::settings::Settings;
use serenity::all::{ChannelId, GuildId, MessageId, ReactionType};
use serenity::builder::{CreateAllowedMentions, CreateEmbed, CreateMessage};

use crate::counting_service::{NewState, current_state, judge_count, load_state, parse_count, run_locked, save_state};

/// `COUNTING.OK_EMOJI`.
pub const OK_EMOJI: &str = "✅";
/// `COUNTING.FAIL_EMOJI`.
pub const FAIL_EMOJI: &str = "❌";
/// A wrong first number has nothing to reset, so its "start at 1" hint is posted at most this often per channel.
pub const START_HINT_EVERY_MS: u64 = 60_000;

/// The per-channel throttle of the start hint (`createThrottle(START_HINT_EVERY_MS)`), one per app.
pub struct StartHint(bot_util::async_util::Throttle);

impl Default for StartHint {
    fn default() -> Self {
        Self(bot_util::async_util::create_throttle(START_HINT_EVERY_MS))
    }
}

/// `looksLikeCount(message)`: the cheap synchronous pre-filter (`/^\s*[\d(]/`); the full parse happens later.
pub fn looks_like_count(content: Option<&str>) -> bool {
    let Some(c) = content else { return false };
    c.chars().find(|ch| !bot_util::js::is_js_space(*ch)).is_some_and(|ch| ch.is_ascii_digit() || ch == '(')
}

/// `countMath(content)`: the maths a count was written as, on one line, or `None` when it was just the number.
pub fn count_math(content: &str) -> Option<String> {
    let text = bot_util::js::trim(content);
    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for c in text.chars() {
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
    Some(out)
}

/// What [`ruined_text`] says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ruined<'a> {
    pub user_id: &'a str,
    pub count: i64,
    pub reason: crate::counting_service::Reason,
    pub expected: i64,
    pub best: i64,
    /// `countMath` of the message, and what it came to.
    pub math: Option<String>,
    pub got: Option<i64>,
}

fn fmt(n: i64) -> String {
    bot_util::text::format_number(n as f64)
}

/// `ruinedText(...)`: the note posted when a run ends.
pub fn ruined_text(r: &Ruined<'_>) -> String {
    let next = format!("The next number was **{}**.", fmt(r.expected));
    let why = match (r.reason, &r.math) {
        (crate::counting_service::Reason::Twice, _) => "You can't count twice in a row.".to_string(),
        (_, Some(math)) if !math.is_empty() => format!("`{math}` is **{}**. {next}", r.got.map(fmt).unwrap_or_else(|| "0".into())),
        _ => next,
    };
    format!("<@{}> ruined it at **{}**! {why}\nStart again from **1**. Best run: **{}**.", r.user_id, fmt(r.count), fmt(r.best))
}

/// `deletedText({ userId, count })`: the note posted when the last counted message is deleted.
pub fn deleted_text(user_id: Option<&str>, count: i64) -> String {
    let who = match user_id.filter(|u| !u.is_empty()) {
        Some(u) => format!("<@{u}>"),
        None => "Someone".to_string(),
    };
    format!("{who} deleted their number **{}**. The next number is **{}**.", fmt(count), fmt(count + 1))
}

/// A reply to send once the lock is released.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)] // at most two per message
pub enum Action {
    /// React to the counted message.
    React(&'static str),
    /// Post this embed in the counting channel.
    Note(CreateEmbed),
}

/// The message being judged (the fields `judgeMessage` reads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CountMessage {
    pub id: String,
    pub channel_id: String,
    pub author_id: String,
    pub content: String,
}

impl CountMessage {
    pub fn of(message: &serenity::all::Message) -> Self {
        Self {
            id: message.id.to_string(),
            channel_id: message.channel_id.to_string(),
            author_id: message.author.id.to_string(),
            content: message.content.clone(),
        }
    }
}

/// `inCountingChannel(ctx)`: the synchronous hook filter. False when the memoised settings name another counting
/// channel (or none), so chat elsewhere never waits on the guild's lock; without a memo it passes and the handler
/// checks after loading the settings.
pub fn in_counting_channel(app: &App, guild_id: &str, channel_id: &str) -> bool {
    match app.settings().peek_settings(guild_id) {
        None => true,
        Some(s) => s.counting_channel_id.as_deref().is_some_and(|c| !c.is_empty() && c == channel_id),
    }
}

/// `judgeMessage(ctx)`: judges one message in the counting channel and saves the new state. Runs under the guild's
/// lock. Returns the replies to send once the lock is released; `None` for none.
pub async fn judge_message(app: &App, settings: Option<Settings>, guild_id: &str, message: &CountMessage) -> anyhow::Result<Option<Vec<Action>>> {
    let Some(channel_id) = settings.as_ref().and_then(|s| s.counting_channel_id.clone()).filter(|c| !c.is_empty()) else { return Ok(None) };
    if message.channel_id != channel_id {
        return Ok(None);
    }
    let Some(number) = parse_count(&message.content) else { return Ok(None) };

    let row = load_state(app.db(), guild_id).await?;
    let state = current_state(row.as_ref(), Some(&channel_id));
    let verdict = judge_count(state.count, state.last_user_id.as_deref(), &message.author_id, number);

    if verdict.ok {
        let next = NewState {
            channel_id,
            count: number,
            last_user_id: Some(message.author_id.clone()),
            last_message_id: Some(message.id.clone()),
            best: state.best.max(number),
        };
        save_state(app, guild_id, &next).await?;
        return Ok(Some(vec![Action::React(OK_EMOJI)]));
    }

    if state.count == 0 {
        // Nothing to lose yet: no reset and no public shaming, just a hint now and then.
        let hint = app.ext::<StartHint>().0.allow(&channel_id);
        let mut actions = vec![Action::React(FAIL_EMOJI)];
        if hint {
            actions.push(Action::Note(info_embed("Counting", Some("The count starts at **1**."), None)));
        }
        return Ok(Some(actions));
    }

    let reset = NewState { channel_id, count: 0, last_user_id: None, last_message_id: None, best: state.best };
    save_state(app, guild_id, &reset).await?;
    let text = ruined_text(&Ruined {
        user_id: &message.author_id,
        count: state.count,
        reason: verdict.reason.unwrap_or(crate::counting_service::Reason::Wrong),
        expected: verdict.expected,
        best: state.best,
        math: count_math(&message.content),
        got: Some(number),
    });
    Ok(Some(vec![Action::React(FAIL_EMOJI), Action::Note(error_embed("Count Ruined", Some(&text), None))]))
}

fn parse_id(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|v| *v != 0)
}

/// `react(message, emoji)`: reacts unless the bot lacks Add Reactions or Read Message History; a "gone" error is
/// silent, any other is logged.
pub async fn react(app: &App, guild_id: GuildId, channel_id: ChannelId, message_id: MessageId, emoji: &str) {
    if !missing_bot_permissions(app, PermTarget::Channel(guild_id, channel_id), &["AddReactions", "ReadMessageHistory"]).is_empty() {
        return;
    }
    let Some(http) = app.http() else { return };
    if let Err(e) = http.create_reaction(channel_id, message_id, &ReactionType::Unicode(emoji.to_string())).await
        && !bot_core::discord::is_gone(&e)
    {
        bot_util::log_warn!(format!("[counting] {guild_id}: could not react: {}", bot_core::discord::error_message(&e)));
    }
}

/// `postNote(channel, embed)`: posts a bot note in the counting channel; never pings and never fails.
pub async fn post_note(app: &App, guild_id: GuildId, channel_id: ChannelId, embed: CreateEmbed) {
    if !missing_bot_permissions(app, PermTarget::Channel(guild_id, channel_id), &["ViewChannel", "SendMessages", "EmbedLinks"]).is_empty() {
        return;
    }
    let Some(http) = app.http() else { return };
    let msg = CreateMessage::new().embed(embed).allowed_mentions(CreateAllowedMentions::new());
    if let Err(e) = channel_id.send_message(http.as_ref(), msg).await
        && !bot_core::discord::is_gone(&e)
    {
        bot_util::log_warn!(format!("[counting] {guild_id}: could not post: {}", bot_core::discord::error_message(&e)));
    }
}

/// Sends the replies [`judge_message`] returned, in order.
pub async fn run_actions(app: &App, guild_id: &str, message: &CountMessage, actions: Vec<Action>) {
    let (Some(g), Some(c), Some(m)) = (parse_id(guild_id), parse_id(&message.channel_id), parse_id(&message.id)) else { return };
    let (g, c, m) = (GuildId::new(g), ChannelId::new(c), MessageId::new(m));
    for a in actions {
        match a {
            Action::React(emoji) => react(app, g, c, m, emoji).await,
            Action::Note(embed) => post_note(app, g, c, embed).await,
        }
    }
}

/// `handleCount(ctx)`: judges one message, then sends its replies (outside any lock).
pub async fn handle_count(app: &App, settings: Option<Settings>, guild_id: &str, message: &CountMessage) -> anyhow::Result<()> {
    if let Some(actions) = judge_message(app, settings, guild_id, message).await? {
        run_actions(app, guild_id, message, actions).await;
    }
    Ok(())
}

/// `handleDelete`'s decision: the note to post when the deleted message held the current count, else `None`.
pub async fn deleted_note(app: &App, settings: Option<Settings>, guild_id: &str, channel_id: &str, message_id: &str) -> anyhow::Result<Option<CreateEmbed>> {
    let Some(counting) = settings.as_ref().and_then(|s| s.counting_channel_id.clone()).filter(|c| !c.is_empty()) else { return Ok(None) };
    if channel_id != counting {
        return Ok(None);
    }
    let Some(row) = load_state(app.db(), guild_id).await? else { return Ok(None) };
    if row.channel_id != counting || row.count == 0 || row.last_message_id.as_deref() != Some(message_id) {
        return Ok(None);
    }
    Ok(Some(info_embed("Number Deleted", Some(&deleted_text(row.last_user_id.as_deref(), i64::from(row.count))), None)))
}

/// `handleDelete(ctx)`: tells the channel the next number when the message holding the current count is deleted.
pub async fn handle_delete(app: &App, settings: Option<Settings>, guild_id: GuildId, channel_id: ChannelId, message_id: MessageId) -> anyhow::Result<()> {
    let g = guild_id.to_string();
    if let Some(embed) = deleted_note(app, settings, &g, &channel_id.to_string(), &message_id.to_string()).await? {
        post_note(app, guild_id, channel_id, embed).await;
    }
    Ok(())
}

/// The `messageCreate` hook body. The guild's lock is taken when the hook is called, before any await: the order
/// messages enter it is the order they are judged. The reaction and any note are sent after the lock is released.
pub fn on_message(ctx: Arc<HookCtx<MessageCreate>>) -> impl std::future::Future<Output = BotResult> + Send + 'static {
    let guild_id = ctx.data.guild_id.to_string();
    let message = CountMessage::of(&ctx.data.message);
    let (c, g, m) = (ctx.clone(), guild_id.clone(), message.clone());
    let judged = run_locked(&ctx.app, &guild_id, move || async move {
        let settings = c.get_settings().await;
        judge_message(&c.app, settings, &g, &m).await
    });
    let app = ctx.app.clone();
    async move {
        if let Some(actions) = judged.await? {
            run_actions(&app, &guild_id, &message, actions).await;
        }
        Ok(())
    }
}

/// The `messageDelete` hook body (under the guild's lock, like the JS).
pub fn on_delete(ctx: Arc<HookCtx<MessageDelete>>) -> impl std::future::Future<Output = BotResult> + Send + 'static {
    let guild_id = ctx.data.guild_id;
    let c = ctx.clone();
    run_locked(&ctx.app, &guild_id.to_string(), move || async move {
        let settings = c.get_settings().await;
        handle_delete(&c.app, settings, guild_id, c.data.channel_id, c.data.message_id).await
    })
}

/// The plugin (`src/plugins/counting.js`).
pub fn plugin() -> Plugin {
    Plugin::new("counting", Some(ModuleKey::Counting))
        .hook(
            // HOOK_ORDER.messageCreate: right after automod, so counts keep their gateway order.
            Hook::<MessageCreate>::new("counting", on_message).filter(|ctx| {
                looks_like_count(Some(&ctx.data.message.content))
                    && in_counting_channel(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.channel_id.to_string())
            }),
        )
        .hook(
            Hook::<MessageDelete>::new("counting", on_delete)
                .filter(|ctx| in_counting_channel(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.channel_id.to_string())),
        )
}
