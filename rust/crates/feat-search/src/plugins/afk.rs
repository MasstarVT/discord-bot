//! Port of `src/plugins/afk.js`: ends a member's AFK when they write, and tells whoever mentions an AFK member that
//! they are away.
//!  (1) The author is AFK (and the channel is not ignored, and the AFK is older than a few seconds): the row is
//!      deleted, `[AFK] ` comes off their nickname and a "Welcome back" reply with the AFK length and jump links to the
//!      pings they got deletes itself shortly after.
//!  (2) Each AFK member the message mentions (at most 5, never the author) is recorded as a ping and announced with
//!      one short reply, at most once a minute per channel and member.
//! Nothing here pings: replies send `allowedMentions: { parse: [] }`. Chat from members who are not AFK and who mention
//! nobody who is never reaches Postgres: the sync filter checks the memoised set of AFK ids.

use std::collections::HashSet;
use std::sync::Arc;

use bot_core::markdown::escape_markdown;
use bot_core::prelude::*;
use bot_db::AfkStatus;
use serenity::all::{ChannelId, GuildId, Member, MessageId, User};

use crate::afk_service::{
    self as svc, ChannelPlace, afk, afk_length, clear_afk, forget_afk_member, is_ignored_channel, jump_link, load_afk_ids, load_status,
    load_statuses, may_notify, name_without_prefix, peek_afk_ids, plain_text, plain_text_default, read_pings, record_ping, remove_nickname_prefix,
};
use crate::ops;

/// `shownName(member, user)` from the raw display name (`member?.displayName ?? user?.globalName ?? user?.username`):
/// without our prefix, markdown escaped, never a mention.
pub fn shown_name(raw: Option<&str>) -> String {
    let name = escape_markdown(&plain_text(name_without_prefix(raw.unwrap_or("Someone")), 40));
    if name.is_empty() { "Someone".into() } else { name }
}

/// The display name discord.js shows for a user without a member: `globalName ?? username`.
pub fn user_display(user: &User) -> String {
    user.global_name.clone().unwrap_or_else(|| user.name.clone())
}

/// `welcomeText({ name, row, guildId, now })`: the reply to a member who is back.
pub fn welcome_text(name: &str, since_ms: i64, ping_count: i64, pings: &serde_json::Value, guild_id: &str, now_ms: i64) -> String {
    let mut lines = vec![format!("👋 Welcome back, **{name}**! You were AFK for **{}**.", afk_length(now_ms - since_ms))];
    if ping_count > 0 {
        let all = read_pings(pings);
        let skip = all.len().saturating_sub(afk::MAX_LINKS);
        let links: Vec<String> =
            all[skip..].iter().enumerate().map(|(i, p)| format!("[{}]({})", i + 1, jump_link(guild_id, &p.channel_id, &p.message_id))).collect();
        let tail = if links.is_empty() { ".".to_string() } else { format!(": {}", links.join(" · ")) };
        lines.push(format!("You were mentioned **{ping_count}** time{}{tail}", if ping_count == 1 { "" } else { "s" }));
    }
    lines.join("\n")
}

/// `noticeText({ name, row })`: the line that tells others a member is away.
pub fn notice_text(name: &str, reason: Option<&str>, since_ms: i64) -> String {
    let reason = reason.filter(|r| !r.is_empty()).map(plain_text_default).unwrap_or_default();
    let reason = if reason.is_empty() { String::new() } else { format!(": {reason}") };
    format!("{} **{name}** is AFK{reason} (since {})", afk::EMOJI, bot_util::time::discord_timestamp_ms(since_ms as f64, "R"))
}

fn since_ms(row: &AfkStatus) -> i64 {
    row.since.timestamp_millis()
}

/// `canReply(channel)`: View Channel, Send Messages (in Threads) and Read Message History.
pub fn can_reply(app: &App, guild_id: GuildId, channel_id: ChannelId) -> bool {
    let o = ops::ops(app);
    let Some(bot) = o.bot_id(app) else { return false };
    ops::view(app, guild_id, |g| {
        let Some(ch) = bot_core::permissions::find_channel(g, channel_id) else { return false };
        let send = if bot_core::permissions::is_thread(ch) { "SendMessagesInThreads" } else { "SendMessages" };
        let perms = bot_core::permissions::me(g, bot).and_then(|me| bot_core::permissions::member_permissions_in(g, ch, bot, &me.roles, true));
        bot_core::interaction_helpers::missing_from(perms, &["ViewChannel", send, "ReadMessageHistory"]).is_empty()
    })
    .unwrap_or(false)
}

/// `reply(message, content, deleteAfterMs)`: replies without pinging anyone or unfurling links; with
/// `delete_after_ms` the reply deletes itself (a detached one-shot timer). Never fails.
pub async fn reply(app: &App, guild_id: GuildId, channel_id: ChannelId, message_id: MessageId, content: String, delete_after_ms: u64) {
    if !can_reply(app, guild_id, channel_id) {
        return;
    }
    let o = ops::ops(app);
    match o.reply(app, channel_id, message_id, content).await {
        Ok(sent) => {
            if delete_after_ms > 0 {
                let app2 = app.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(delete_after_ms)).await;
                    let _ = ops::ops(&app2).delete_message(&app2, channel_id, sent).await;
                });
            }
        }
        Err(e) => {
            if !svc::is_gone(&e) {
                bot_util::log_warn!(format!("[afk] {guild_id}: could not reply: {}", e.message));
            }
        }
    }
}

/// The parts of a `messageCreate` payload the AFK handler reads.
#[derive(Debug, Clone)]
pub struct AfkMessage {
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    pub message_id: MessageId,
    pub author: User,
    pub member: Option<Member>,
    /// `message.mentions.users` in order, with the mentioned member's display name when Discord sent the member.
    pub mentions: Vec<(User, Option<String>)>,
    /// The channel when the payload had it (for the ignored-channel check when the guild is not cached).
    pub parent_id: Option<ChannelId>,
}

impl AfkMessage {
    pub fn of(e: &MessageCreate) -> Self {
        let mentions = e
            .message
            .mentions
            .iter()
            .map(|u| {
                let display = u.member.as_ref().map(|m| m.nick.clone().unwrap_or_else(|| user_display(u)));
                (u.clone(), display)
            })
            .collect();
        Self {
            guild_id: e.guild_id,
            channel_id: e.channel_id,
            message_id: e.message.id,
            author: e.message.author.clone(),
            member: e.member.clone(),
            mentions,
            parent_id: e.channel.as_ref().and_then(|c| c.parent_id),
        }
    }

    fn mention_ids(&self) -> impl Iterator<Item = String> + '_ {
        self.mentions.iter().map(|(u, _)| u.id.to_string())
    }
}

async fn member_of(app: &App, m: &AfkMessage) -> Option<Member> {
    if let Some(member) = &m.member {
        return Some(member.clone());
    }
    ops::ops(app).fetch_member(app, m.guild_id, m.author.id).await
}

/// `welcomeBack(ctx)`: the author wrote, so their AFK ends.
async fn welcome_back(app: &App, m: &AfkMessage, welcome_delete_ms: u64) -> anyhow::Result<()> {
    let g = m.guild_id.to_string();
    let author = m.author.id.to_string();
    let Some(row) = load_status(app.db(), &g, &author).await? else {
        forget_afk_member(app, &g, &author); // the memo was behind: drop the id
        return Ok(());
    };
    let now = app.clock().now_ms();
    if now - since_ms(&row) <= afk::GRACE_MS {
        return Ok(());
    }
    let Some(cleared) = clear_afk(app, &g, &author).await? else { return Ok(()) }; // /afk clear got there first
    let member = member_of(app, m).await;
    let raw = member.as_ref().map(bot_core::adapters::member_display_name).unwrap_or_else(|| user_display(&m.author));
    let text = welcome_text(&shown_name(Some(&raw)), since_ms(&cleared), i64::from(cleared.ping_count), &cleared.pings, &g, now);
    let nick = async {
        if cleared.nick_changed {
            remove_nickname_prefix(app, m.guild_id, member.as_ref(), cleared.previous_nick.as_deref()).await;
        }
    };
    futures::join!(nick, reply(app, m.guild_id, m.channel_id, m.message_id, text, welcome_delete_ms));
    Ok(())
}

/// `announce(ctx, ids)`: records the pings of the AFK members the message mentions and announces them.
async fn announce(app: &App, m: &AfkMessage, ids: &HashSet<String>) -> anyhow::Result<()> {
    let g = m.guild_id.to_string();
    let author = m.author.id.to_string();
    let mut targets: Vec<String> = Vec::new();
    for id in m.mention_ids() {
        if id != author && ids.contains(&id) {
            targets.push(id);
        }
        if targets.len() >= afk::MAX_NOTICES {
            break;
        }
    }
    if targets.is_empty() {
        return Ok(());
    }
    let rows = load_statuses(app.db(), &g, &targets).await?;
    let (channel, message) = (m.channel_id.to_string(), m.message_id.to_string());
    let pings: Vec<_> = rows.iter().map(|r| record_ping(app, &g, &r.user_id, &channel, &message, &author)).collect();
    futures::future::join_all(pings).await;
    if rows.is_empty() || !can_reply(app, m.guild_id, m.channel_id) {
        return Ok(());
    }
    let mut lines = Vec::new();
    for row in &rows {
        if !may_notify(app, &channel, &row.user_id) {
            continue;
        }
        let raw = m.mentions.iter().find(|(u, _)| u.id.to_string() == row.user_id).map(|(u, display)| display.clone().unwrap_or_else(|| user_display(u)));
        lines.push(notice_text(&shown_name(raw.as_deref()), row.reason.as_deref(), since_ms(row)));
    }
    if !lines.is_empty() {
        reply(app, m.guild_id, m.channel_id, m.message_id, lines.join("\n"), 0).await;
    }
    Ok(())
}

/// `mayConcernAfk(ctx)`: the sync hook filter. False for the common case, where the author is not AFK and mentions
/// nobody who is; with no memoised set yet it passes, and the handler loads it.
pub fn may_concern_afk(app: &App, guild_id: &str, author_id: &str, mention_ids: &[String]) -> bool {
    let Some(ids) = peek_afk_ids(app, guild_id) else { return true };
    if ids.is_empty() {
        return false;
    }
    ids.contains(author_id) || mention_ids.iter().any(|id| ids.contains(id))
}

/// `handleMessage(ctx, { welcomeDeleteMs })`: the author's return, then the mentions of AFK members. Each part is
/// isolated, so one failing does not stop the other.
pub async fn handle_message(app: &App, settings: Option<&bot_db::GuildSettings>, m: &AfkMessage, welcome_delete_ms: u64) {
    let Some(settings) = settings else { return };
    let place = ops::view(app, m.guild_id, |g| ChannelPlace::of(g, m.channel_id))
        .unwrap_or_else(|| ChannelPlace { id: m.channel_id.to_string(), parent_id: m.parent_id.map(|p| p.to_string()), grandparent_id: None });
    if is_ignored_channel(Some(settings), Some(&place)) {
        return;
    }
    let g = m.guild_id.to_string();
    let ids = load_afk_ids(app, &g).await;
    if ids.is_empty() {
        return;
    }
    if ids.contains(&m.author.id.to_string())
        && let Err(e) = welcome_back(app, m, welcome_delete_ms).await
    {
        bot_util::log_warn!(format!("[afk] {g}: welcome back failed: {e}"));
    }
    if let Err(e) = announce(app, m, &ids).await {
        bot_util::log_warn!(format!("[afk] {g}: announcing failed: {e}"));
    }
}

async fn on_message(ctx: Arc<HookCtx<MessageCreate>>) -> BotResult {
    let settings = ctx.get_settings().await;
    let m = AfkMessage::of(&ctx.data);
    handle_message(&ctx.app, settings.as_deref(), &m, afk::WELCOME_DELETE_MS).await;
    Ok(())
}

/// The plugin (`src/plugins/afk.js`).
pub fn plugin() -> Plugin {
    Plugin::new("afk", Some(ModuleKey::Afk)).hook(
        // HOOK_ORDER.messageCreate: after automod has removed what it removes.
        Hook::<MessageCreate>::new("afk", on_message).filter(|ctx| {
            let mentions: Vec<String> = ctx.data.message.mentions.iter().map(|u| u.id.to_string()).collect();
            may_concern_afk(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.message.author.id.to_string(), &mentions)
        }),
    )
}
