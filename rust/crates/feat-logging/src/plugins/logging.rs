//! Port of `src/plugins/logging.js`: the glue between hook payloads, the per-event toggles and ignore list, the pure
//! embed builders ([`crate::log_events`]) and [`send_log`].
//!
//! Every hook checks [`ready`] before building anything, so a disabled event costs no work. The moderation log
//! (`LogType::Mod`) is never written from here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bot_core::deletion_tracker::was_deleted_by_bot;
use bot_core::prelude::*;
use bot_core::settings::Settings;
use indexmap::IndexMap;
use serde_json::Value;
use serenity::all::{ChannelId, GuildId, Permissions};

use crate::adapters;
use crate::message_snapshot as snap;
use crate::log_events::{self as ev, InviteInfo, LogFile, LogMessage, effective_text};
use crate::logging_service::{
    ChannelChain, LogType, MESSAGE_LOG_EVENTS, SendLogOpts, channel_cleanup_data, forget_dead_channels, is_ignored_channel, is_log_event_enabled, log_api, log_event,
    mark_log_channel_deleted, send_log,
};
use crate::plugins::invites::InvitePending;

const TIMEOUT_KEY_MIN_TTL_S: i64 = 60;
/// 28 days, Discord's longest timeout.
const TIMEOUT_KEY_MAX_TTL_S: i64 = 2_419_200;
const MANAGED_MEMO_TTL_MS: i64 = 10 * 60_000;
const MANAGED_MEMO_MAX: usize = 2_000;
const AUDIT_LOG_DELAY_MS: u64 = 1_500;
const AUDIT_LOOKUPS_PER_WINDOW: u32 = 5;
const AUDIT_WINDOW_MS: i64 = 10_000;

fn invite_hint_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?i)discord(?:app)?\.com/invite/|discord\.gg/").expect("valid regex"))
}

/// `INVITE_HINT_RE.test(text)`: the cheap pre-filter of the invite-posted hook.
pub fn has_invite_hint(text: &str) -> bool {
    invite_hint_re().is_match(text)
}

// ── Hook context helpers (§6.2) ───────────────────────────────────────────────

/// `ready(ctx, event, channel)`: the settings when the event should be logged, otherwise `None` (settings missing,
/// the event disabled or, for message events, the channel ignored).
async fn ready<E: HookEvent>(ctx: &HookCtx<E>, event: &str, channel: Option<&ChannelChain>) -> Option<Settings> {
    let settings = ctx.get_settings().await?;
    if !is_log_event_enabled(Some(&settings), event) {
        return None;
    }
    if channel.is_some() && MESSAGE_LOG_EVENTS.contains(&event) && is_ignored_channel(Some(&settings), channel) {
        return None;
    }
    Some(settings)
}

/// `emit(ctx, event, embed, extra)`.
pub async fn emit(app: &App, guild_id: GuildId, settings: Option<Settings>, event: &str, embed: Option<Value>, file: Option<LogFile>) {
    let (Some(embed), Some(e)) = (embed, log_event(event)) else { return };
    let mut opts = SendLogOpts::default().event(event).settings(settings);
    if let Some(f) = file {
        opts = opts.file(f);
    }
    send_log(app, guild_id, e.log_type, embed, opts).await;
}

fn is_log_channel_of(settings: &bot_db::GuildSettings, channel_id: &str) -> bool {
    LogType::ALL.into_iter().any(|t| t.channel_of(settings) == Some(channel_id))
}

fn bot_id(app: &App) -> Option<String> {
    app.discord().map(|d| d.cache.current_user().id.to_string())
}

fn now(app: &App) -> i64 {
    app.clock().now_ms()
}

// ── Messages (§6.3) ───────────────────────────────────────────────────────────

/// Runs `fut` without holding up the hook (`void promise`): the Redis snapshot calls never block a log.
fn detach(fut: impl std::future::Future<Output = ()> + Send + 'static) {
    tokio::spawn(fut);
}

async fn on_message_delete(ctx: Arc<HookCtx<MessageDelete>>) -> BotResult {
    let d = &ctx.data;
    // Bot-authored panels (giveaways, polls, starboard posts) are never logged.
    if let Some(m) = &d.message
        && (m.author.bot || m.webhook_id.is_some())
    {
        return Ok(());
    }
    log_message_delete(&ctx).await;
    // Whether or not the delete was logged (event off, channel ignored): the text must not outlive the message.
    let app = ctx.app.clone();
    let (channel_id, message_id) = (d.channel_id.to_string(), d.message_id.to_string());
    detach(async move { snap::forget_snapshots(&app, &channel_id, &[message_id]).await });
    Ok(())
}

async fn log_message_delete(ctx: &Arc<HookCtx<MessageDelete>>) {
    let d = &ctx.data;
    let app = &ctx.app;
    let chain = ChannelChain::of(app, d.guild_id, d.channel_id);
    let Some(settings) = ready(ctx, "messageDelete", Some(&chain)).await else { return };
    let (channel_id, message_id) = (d.channel_id.to_string(), d.message_id.to_string());
    let bot_reason = was_deleted_by_bot(app, &message_id);
    let mut view = match &d.message {
        Some(m) => adapters::log_message(app, m),
        // discord.js reads createdTimestamp from the snowflake, so even a partial message says when it was sent.
        None => LogMessage { created_timestamp: Some(bot_core::adapters::snowflake_ms(d.message_id.get()) as i64), ..LogMessage::partial(&message_id, Some(&channel_id)) },
    };
    let mut skip = false;
    if view.partial {
        if bot_reason.is_some() {
            skip = true; // nothing useful is known about it
        } else if let Some(s) = snap::load_snapshot(app, &channel_id, &message_id).await {
            // Not in memory (restart, eviction, an old message): the snapshot from messageCreate still knows it.
            if s.author.bot {
                skip = true;
            } else {
                view = snap::snapshot_to_message(&s);
            }
        }
    }
    if !skip {
        let known = adapters::known_channels(app, d.guild_id);
        let embed = ev::build_message_delete_embed(&view, bot_reason.as_deref(), bot_id(app).as_deref(), now(app), known.as_ref());
        emit(app, d.guild_id, Some(settings), "messageDelete", Some(embed), None).await;
    }
}

async fn on_message_edit(ctx: Arc<HookCtx<MessageUpdate>>) -> BotResult {
    let d = &ctx.data;
    let app = &ctx.app;
    // Core already dropped non-edits; these are the defensive repeats.
    if !d.old_known && d.message.edited_timestamp.is_none() {
        return Ok(());
    }
    if d.old_known && d.old_message.as_ref().map(|o| &o.content) == Some(&d.message.content) {
        return Ok(());
    }
    let chain = ChannelChain::of(app, d.guild_id, d.channel_id);
    let settings = ctx.get_settings().await;
    let log_settings = ready(&ctx, "messageEdit", Some(&chain)).await;
    let keep = snap::should_snapshot(app, settings.as_ref(), d.guild_id, &d.message);
    if log_settings.is_none() && !keep {
        return Ok(());
    }
    let new = adapters::log_message(app, &d.message);
    if let Some(log_settings) = log_settings {
        let mut old = d.old_message.as_ref().map(|m| adapters::log_message(app, m));
        let mut old_known = d.old_known;
        if !old_known {
            // The old text is not in memory: the snapshot is the "before" state.
            let loaded = snap::load_snapshot(app, &d.channel_id.to_string(), &d.message.id.to_string()).await;
            if let Some(s) = loaded.filter(|s| !s.author.bot) {
                old = Some(snap::snapshot_to_message(&s));
                old_known = true;
            }
        }
        let unchanged = old_known && old.as_ref().map(|o| &o.content) == Some(&new.content);
        if !unchanged {
            let known = adapters::known_channels(app, d.guild_id);
            let embed = ev::build_message_edit_embed(old.as_ref(), &new, old_known, now(app), known.as_ref());
            emit(app, d.guild_id, Some(log_settings), "messageEdit", Some(embed), None).await;
        }
    }
    // The next edit or delete needs the latest text, whether or not this edit was logged.
    if keep {
        let app = app.clone();
        detach(async move {
            snap::store_snapshot(&app, &new).await;
        });
    }
    Ok(())
}

async fn on_bulk_delete(ctx: Arc<HookCtx<MessageDeleteBulk>>) -> BotResult {
    let d = &ctx.data;
    let mut ids: Vec<String> = d.message_ids.iter().map(|i| i.to_string()).collect();
    for m in &d.messages {
        let id = m.id.to_string();
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    log_bulk_delete(&ctx, &ids).await;
    // Whether or not the bulk delete was logged: the texts must not outlive the messages.
    let app = ctx.app.clone();
    let channel_id = d.channel_id.to_string();
    detach(async move { snap::forget_snapshots(&app, &channel_id, &ids).await });
    Ok(())
}

async fn log_bulk_delete(ctx: &Arc<HookCtx<MessageDeleteBulk>>, ids: &[String]) {
    let d = &ctx.data;
    let app = &ctx.app;
    let chain = ChannelChain::of(app, d.guild_id, d.channel_id);
    let Some(settings) = ready(ctx, "messageBulkDelete", Some(&chain)).await else { return };
    let bot_reason = d.message_ids.iter().find_map(|id| was_deleted_by_bot(app, &id.to_string()));
    let cached: HashMap<String, &serenity::all::Message> = d.messages.iter().map(|m| (m.id.to_string(), m)).collect();
    let channel_id = d.channel_id.to_string();
    let mut list: Vec<LogMessage> = ids
        .iter()
        .map(|id| match cached.get(id) {
            Some(m) => adapters::log_message(app, m),
            None => LogMessage::partial(id, Some(&channel_id)),
        })
        .collect();
    // Messages that are not in memory are filled in from their snapshots (one batched read).
    let missing: Vec<String> = list.iter().filter(|m| m.partial || m.author.is_none()).map(|m| m.id.clone()).collect();
    if !missing.is_empty() {
        let snaps = snap::load_snapshots(app, &channel_id, &missing).await;
        for m in list.iter_mut() {
            if let Some(s) = snaps.get(&m.id).filter(|s| !s.author.bot) {
                *m = snap::snapshot_to_message(s);
            }
        }
    }
    let name = d.channel.as_ref().map(|c| c.name.clone()).or_else(|| bot_core::interaction_helpers::cached_channel(app, d.guild_id, d.channel_id).map(|c| c.name));
    let known = adapters::known_channels(app, d.guild_id);
    let (embed, file) = ev::build_bulk_delete_embed(Some(&channel_id), name.as_deref(), &list, bot_reason.as_deref(), bot_id(app).as_deref(), now(app), known.as_ref());
    emit(app, d.guild_id, Some(settings), "messageBulkDelete", Some(embed), file).await;
}

/// `messageCreate`: remembers the message for later delete / edit logs, then the invite check. The Redis write is never
/// awaited by the hook.
async fn on_message_create(ctx: Arc<HookCtx<MessageCreate>>) -> BotResult {
    let c = ctx.clone();
    detach(async move {
        let settings = c.get_settings().await;
        if snap::should_snapshot(&c.app, settings.as_ref(), c.data.guild_id, &c.data.message) {
            let view = adapters::log_message(&c.app, &c.data.message);
            snap::store_snapshot(&c.app, &view).await;
        }
    });
    if invite_filter(&ctx.data.message) {
        return on_invite_posted(ctx).await;
    }
    Ok(())
}

async fn on_invite_posted(ctx: Arc<HookCtx<MessageCreate>>) -> BotResult {
    let d = &ctx.data;
    let app = &ctx.app;
    let chain = ChannelChain::of(app, d.guild_id, d.channel_id);
    let Some(settings) = ready(&ctx, "invitePosted", Some(&chain)).await else { return Ok(()) };
    let view = adapters::log_message(app, &d.message);
    let codes = ev::find_invite_codes(Some(&effective_text(&view))); // a forward carries its text in its snapshots
    if codes.is_empty() {
        return Ok(());
    }
    let vanity = bot_core::interaction_helpers::cached_guild(app, d.guild_id).and_then(|g| g.vanity_url_code);
    let known = adapters::known_channels(app, d.guild_id);
    let embed = ev::build_invite_posted_embed(&view, &codes, vanity.as_deref(), now(app), known.as_ref());
    emit(app, d.guild_id, Some(settings), "invitePosted", Some(embed), None).await;
    Ok(())
}

// ── Members (§6.4) ────────────────────────────────────────────────────────────

/// guildId -> (window start, lookups): audit-log lookups in the current window.
#[derive(Default)]
struct AuditBudget(Mutex<HashMap<String, (i64, u32)>>);

fn take_audit_lookup(app: &App, guild_id: &str) -> bool {
    let now = now(app);
    let budget = app.ext::<AuditBudget>();
    let mut m = budget.0.lock().unwrap_or_else(|e| e.into_inner());
    let fresh = m.get(guild_id).is_none_or(|(start, _)| now - start >= AUDIT_WINDOW_MS);
    if fresh {
        m.retain(|_, (start, _)| now - *start < AUDIT_WINDOW_MS);
        m.insert(guild_id.to_string(), (now, 0));
    }
    let Some(b) = m.get_mut(guild_id) else { return false };
    if b.1 >= AUDIT_LOOKUPS_PER_WINDOW {
        return false;
    }
    b.1 += 1;
    true
}

fn guild_member_count(app: &App, guild_id: GuildId) -> Option<u64> {
    bot_core::interaction_helpers::cached_guild(app, guild_id).map(|g| g.member_count)
}

async fn on_member_join(ctx: Arc<HookCtx<GuildMemberAdd>>) -> BotResult {
    if ready(&ctx, "memberJoin", None).await.is_none() {
        return Ok(());
    }
    let app = ctx.app.clone();
    let guild_id = ctx.data.guild_id;
    let member = adapters::log_member(&app, guild_id, &ctx.data.member);
    let now = now(&app);
    // ctx.state.invite: { code, inviterId, vanity } | null (unknown) | absent (the invites module recorded nothing).
    let pending = ctx.state.get::<InvitePending>("invitePromise");
    let Some(pending) = pending else {
        let invite = ctx.state.get::<Option<InviteInfo>>("invite");
        let settings = ctx.get_settings().await;
        let count = guild_member_count(&app, guild_id);
        let embed = ev::build_member_join_embed(&member, invite.as_ref().map(Option::as_ref), count, now);
        emit(&app, guild_id, settings, "memberJoin", Some(embed), None).await;
        return Ok(());
    };
    // Attribution is still running: post the log when it settles instead of holding up the hooks after this one.
    let c = ctx.clone();
    tokio::spawn(async move {
        let invite = pending.0.await;
        let settings = c.get_settings().await;
        let count = guild_member_count(&c.app, guild_id);
        let embed = ev::build_member_join_embed(&member, Some(invite.as_ref()), count, now);
        emit(&c.app, guild_id, settings, "memberJoin", Some(embed), None).await;
    });
    Ok(())
}

async fn on_member_leave(ctx: Arc<HookCtx<GuildMemberRemove>>) -> BotResult {
    if ready(&ctx, "memberLeave", None).await.is_none() {
        return Ok(());
    }
    let app = &ctx.app;
    let d = &ctx.data;
    let member = match &d.member {
        Some(m) => adapters::log_member(app, d.guild_id, m),
        None => adapters::partial_member(d.guild_id, &d.user),
    };
    let settings = ctx.get_settings().await;
    emit(app, d.guild_id, settings, "memberLeave", Some(ev::build_member_leave_embed(&member, now(app))), None).await;
    Ok(())
}

/// `timeoutIsFresh`: `added` and `changed` always log (the dedupe key is only recorded); `active-unknown` logs when
/// the key is new, and when Redis is down (fail open).
async fn timeout_is_fresh(app: &App, guild_id: GuildId, member_id: &str, timeout: &ev::TimeoutChange) -> bool {
    if matches!(timeout, ev::TimeoutChange::Removed) {
        return true;
    }
    let Some(until_ms) = timeout.until() else { return true };
    let until_sec = until_ms.div_euclid(1000);
    let ttl = (until_sec - now(app).div_euclid(1000)).clamp(TIMEOUT_KEY_MIN_TTL_S, TIMEOUT_KEY_MAX_TTL_S);
    let fresh = app.cache().set_nx(&timeout_key(&guild_id.to_string(), member_id, until_sec), &1, ttl).await;
    if matches!(timeout, ev::TimeoutChange::ActiveUnknown { .. }) {
        return fresh != Some(false);
    }
    true
}

/// `log:timeout:{guildId}:{memberId}:{untilSec}`: the timeout dedupe key.
pub fn timeout_key(guild_id: &str, member_id: &str, until_sec: i64) -> String {
    format!("log:timeout:{guild_id}:{member_id}:{until_sec}")
}

async fn on_member_update(ctx: Arc<HookCtx<GuildMemberUpdate>>) -> BotResult {
    let app = &ctx.app;
    let d = &ctx.data;
    let g = d.guild_id;
    // Housekeeping first, whatever the toggles say: the bot's own roles may have changed.
    if bot_id(app).as_deref() == Some(d.member.user.id.to_string().as_str()) {
        forget_dead_channels(app, &g.to_string(), None);
    }
    let Some(settings) = ctx.get_settings().await else { return Ok(()) };
    let old = d.old_member.as_ref().map(|m| adapters::log_member(app, g, m));
    let new = adapters::log_member(app, g, &d.member);
    let now = now(app);
    let diff = ev::diff_member_update(old.as_ref(), &new, d.old_known, now);

    // One embed per changed part, in this order.
    if let Some(r) = &diff.roles
        && is_log_event_enabled(Some(&settings), "memberRoles")
    {
        emit(app, g, Some(settings.clone()), "memberRoles", Some(ev::build_member_roles_embed(&new, r, now)), None).await;
    }
    if let Some(n) = &diff.nickname
        && is_log_event_enabled(Some(&settings), "memberNickname")
    {
        emit(app, g, Some(settings.clone()), "memberNickname", Some(ev::build_nickname_embed(&new, n, now)), None).await;
    }
    if let Some(t) = &diff.timeout
        && is_log_event_enabled(Some(&settings), "memberTimeout")
        && timeout_is_fresh(app, g, &new.id, t).await
    {
        emit(app, g, Some(settings.clone()), "memberTimeout", Some(ev::build_timeout_embed(&new, t, now)), None).await;
    }
    if let Some(a) = &diff.avatar
        && is_log_event_enabled(Some(&settings), "memberAvatar")
    {
        emit(app, g, Some(settings.clone()), "memberAvatar", Some(ev::build_avatar_embed(&new, a, now)), None).await;
    }
    Ok(())
}

async fn on_user_update(ctx: Arc<HookCtx<UserUpdate>>) -> BotResult {
    let Some(settings) = ready(&ctx, "userUpdate", None).await else { return Ok(()) };
    let app = &ctx.app;
    let old = adapters::log_user(&ctx.data.old_user);
    let new = adapters::log_user(&ctx.data.user);
    let diff = ev::diff_user(Some(&old), Some(&new));
    emit(app, ctx.data.guild_id, Some(settings), "userUpdate", ev::build_user_update_embed(&new, &diff, now(app)), None).await;
    Ok(())
}

fn bot_can_view_audit_log(app: &App, guild_id: GuildId) -> bool {
    bot_core::interaction_helpers::bot_permissions(app, bot_core::interaction_helpers::PermTarget::Guild(guild_id))
        .is_some_and(|p| bot_core::permissions::js_has(p, Permissions::VIEW_AUDIT_LOG))
}

async fn on_ban(app: &App, settings: Settings, guild_id: GuildId, user: &serenity::all::User, ban_reason: Option<&str>, unban: bool) {
    let event = if unban { "memberUnban" } else { "memberBan" };
    // Executor lookup: best-effort, at most 5 per guild per 10 s. Discord writes the audit entry a moment after the
    // gateway event.
    let mut entry = None;
    if bot_can_view_audit_log(app, guild_id) && take_audit_lookup(app, &guild_id.to_string()) {
        tokio::time::sleep(std::time::Duration::from_millis(AUDIT_LOG_DELAY_MS)).await;
        if let Ok(entries) = log_api(app).ban_audit_entries(guild_id, unban).await {
            let uid = user.id.to_string();
            let t = now(app);
            entry = entries.into_iter().find(|e| e.target_id.as_deref() == Some(uid.as_str()) && t - e.created_ms < 60_000);
        }
    }
    let executor = entry.as_ref().and_then(|e| e.executor_id.clone());
    let reason = entry.as_ref().and_then(|e| e.reason.clone()).or_else(|| ban_reason.map(String::from));
    let embed = ev::build_ban_embed(&adapters::log_user(user), executor.as_deref(), reason.as_deref(), unban, bot_id(app).as_deref(), now(app));
    emit(app, guild_id, Some(settings), event, Some(embed), None).await;
}

async fn on_ban_add(ctx: Arc<HookCtx<GuildBanAdd>>) -> BotResult {
    let Some(settings) = ready(&ctx, "memberBan", None).await else { return Ok(()) };
    on_ban(&ctx.app, settings, ctx.data.guild_id, &ctx.data.user, ctx.data.reason.as_deref(), false).await;
    Ok(())
}

async fn on_ban_remove(ctx: Arc<HookCtx<GuildBanRemove>>) -> BotResult {
    let Some(settings) = ready(&ctx, "memberUnban", None).await else { return Ok(()) };
    on_ban(&ctx.app, settings, ctx.data.guild_id, &ctx.data.user, ctx.data.reason.as_deref(), true).await;
    Ok(())
}

// ── Voice (§6.5) ──────────────────────────────────────────────────────────────

async fn on_voice_state(ctx: Arc<HookCtx<VoiceStateUpdate>>) -> BotResult {
    let d = &ctx.data;
    let app = &ctx.app;
    // Classified by channel ids, not by channel objects: a deleted channel still has its id.
    let old = d.old_state.as_ref().map(adapters::log_voice_state);
    let new = adapters::log_voice_state(&d.new_state);
    let c = ev::classify_voice(old.as_ref(), Some(&new));
    let Some(kind) = c.kind else { return Ok(()) };
    let event = match kind {
        ev::VoiceKind::Join => "voiceJoin",
        ev::VoiceKind::Leave => "voiceLeave",
        ev::VoiceKind::Move => "voiceMove",
        ev::VoiceKind::State => "voiceState",
    };
    let Some(settings) = ready(&ctx, event, None).await else { return Ok(()) };
    let member = adapters::log_member(app, d.guild_id, &d.member);
    let known = adapters::known_channels(app, d.guild_id);
    let names = adapters::channel_names(app, d.guild_id);
    emit(app, d.guild_id, Some(settings), event, ev::build_voice_embed(&member, known.as_ref(), names.as_ref(), &c, now(app)), None).await;
    Ok(())
}

// ── Channels (§6.6) ───────────────────────────────────────────────────────────

/// Channel id -> (value, until): whether a StatChannel / TempVoiceChannel row owns the channel.
#[derive(Default)]
struct ManagedMemo(Mutex<IndexMap<String, (bool, i64)>>);

/// `isBotManagedChannel(channelId)`: a read-only lookup of the bot-managed channel tables, memoised per channel for
/// 10 minutes. Fails open (not managed) when the query fails.
pub async fn is_bot_managed_channel(app: &App, channel_id: &str) -> bool {
    let memo = app.ext::<ManagedMemo>();
    if let Some((v, until)) = memo.0.lock().unwrap_or_else(|e| e.into_inner()).get(channel_id).copied()
        && until > now(app)
    {
        return v;
    }
    let sql = r#"SELECT (EXISTS (SELECT 1 FROM "StatChannel" WHERE "channelId" = $1) OR EXISTS (SELECT 1 FROM "TempVoiceChannel" WHERE "channelId" = $1)) AS "managed""#;
    let value: bool = match bot_db::sqlx::query_scalar(sql).bind(channel_id).fetch_one(app.db()).await {
        Ok(v) => v,
        Err(e) => {
            bot_util::log_debug!(format!("logging: bot-managed channel lookup failed: {e}"));
            return false;
        }
    };
    let mut m = memo.0.lock().unwrap_or_else(|e| e.into_inner());
    m.shift_remove(channel_id);
    m.insert(channel_id.to_string(), (value, now(app) + MANAGED_MEMO_TTL_MS));
    while m.len() > MANAGED_MEMO_MAX {
        m.shift_remove_index(0);
    }
    value
}

async fn on_channel_create(ctx: Arc<HookCtx<ChannelCreate>>) -> BotResult {
    let Some(settings) = ready(&ctx, "channelCreate", None).await else { return Ok(()) };
    let app = &ctx.app;
    let ch = adapters::log_channel(app, ctx.data.guild_id, &ctx.data.channel);
    emit(app, ctx.data.guild_id, Some(settings), "channelCreate", Some(ev::build_channel_create_embed(&ch, now(app))), None).await;
    Ok(())
}

async fn on_channel_update(ctx: Arc<HookCtx<ChannelUpdate>>) -> BotResult {
    let app = &ctx.app;
    let d = &ctx.data;
    let id = d.channel.id.to_string();
    // Housekeeping first: a log channel that changed may be usable again.
    if let Some(s) = ctx.get_settings().await
        && is_log_channel_of(&s, &id)
    {
        forget_dead_channels(app, &d.guild_id.to_string(), Some(&id));
    }
    let Some(settings) = ready(&ctx, "channelUpdate", None).await else { return Ok(()) };
    let Some(old) = &d.old_channel else { return Ok(()) };
    let lines = ev::diff_channel(&adapters::log_channel(app, d.guild_id, old), &adapters::log_channel(app, d.guild_id, &d.channel));
    if lines.is_empty() {
        return Ok(());
    }
    // Stat and temp-voice channels are renamed by the bot all the time: skip name-only entries of those.
    if lines.len() == 1 && lines[0].name == "Name" && is_bot_managed_channel(app, &id).await {
        return Ok(());
    }
    let embed = ev::build_channel_update_embed(&adapters::log_channel(app, d.guild_id, &d.channel), &lines, now(app));
    emit(app, d.guild_id, Some(settings), "channelUpdate", embed, None).await;
    Ok(())
}

/// The members connected to a voice channel (`guild.voiceStates.cache`), from the cache.
fn voice_occupants(app: &App, guild_id: GuildId, channel_id: ChannelId) -> Vec<String> {
    let Some(g) = bot_core::interaction_helpers::cached_guild(app, guild_id) else { return Vec::new() };
    g.voice_states.values().filter(|v| v.channel_id == Some(channel_id)).map(|v| v.user_id.to_string()).collect()
}

/// ALWAYS (declared with `always`): the settings cleanup runs even while the module is off.
async fn on_channel_delete(ctx: Arc<HookCtx<ChannelDelete>>) -> BotResult {
    let app = &ctx.app;
    let d = &ctx.data;
    let Some(settings) = ctx.get_settings().await else { return Ok(()) };
    let gid = d.guild_id.to_string();
    let id = d.channel.id.to_string();

    // A deleted log channel: later send_log calls return at once, without a REST call.
    if is_log_channel_of(&settings, &id) {
        mark_log_channel_deleted(app, &gid, &id);
    }

    if is_module_enabled(Some(&settings), Some(ModuleKey::Logging)) && is_log_event_enabled(Some(&settings), "channelDelete") {
        let kind = u8::from(d.channel.kind);
        let occupants = if kind == 2 || kind == 13 { voice_occupants(app, d.guild_id, d.channel.id) } else { Vec::new() };
        let ch = adapters::log_channel(app, d.guild_id, &d.channel);
        let reason = was_deleted_by_bot(app, &id);
        let embed = ev::build_channel_delete_embed(&ch, &occupants, reason.as_deref(), bot_id(app).as_deref(), now(app));
        emit(app, d.guild_id, Some(settings.clone()), "channelDelete", Some(embed), None).await;
    }

    // Cleanup: forget the deleted channel in every log field (the moderation log included) and the ignore list.
    if channel_cleanup_data(Some(&settings), &id).is_some() {
        let cid = id.clone();
        let r = app
            .settings()
            .update_settings_with(&gid, move |cur| {
                let mut patch = SettingsPatch::new();
                if let Some(data) = channel_cleanup_data(Some(&cur), &cid) {
                    for (k, v) in data {
                        patch.insert(&k, v);
                    }
                }
                Ok(patch)
            })
            .await;
        match r {
            Ok(_) => bot_util::log_info!(format!("Cleared deleted log channel {id} from guild {gid} settings")),
            Err(e) => bot_util::log_warn!(format!("Could not clear deleted log channel {id} from guild {gid} settings: {e}")),
        }
    }
    Ok(())
}

// ── Threads ───────────────────────────────────────────────────────────────────
// Thread events are not message events: the ignore list does not apply.

async fn on_thread_create(ctx: Arc<HookCtx<ThreadCreate>>) -> BotResult {
    let Some(settings) = ready(&ctx, "threadCreate", None).await else { return Ok(()) };
    let app = &ctx.app;
    let t = adapters::log_thread(app, ctx.data.guild_id, &ctx.data.thread);
    emit(app, ctx.data.guild_id, Some(settings), "threadCreate", Some(ev::build_thread_create_embed(&t, now(app))), None).await;
    Ok(())
}

async fn on_thread_update(ctx: Arc<HookCtx<ThreadUpdate>>) -> BotResult {
    let Some(settings) = ready(&ctx, "threadUpdate", None).await else { return Ok(()) };
    let app = &ctx.app;
    let d = &ctx.data;
    let new = adapters::log_thread(app, d.guild_id, &d.thread);
    let old = d.old_thread.as_ref().map(|t| adapters::log_thread(app, d.guild_id, t)).unwrap_or_else(|| ev::LogThread { id: new.id.clone(), ..Default::default() });
    let lines = ev::diff_thread(&old, &new);
    if lines.is_empty() {
        return Ok(());
    }
    emit(app, d.guild_id, Some(settings), "threadUpdate", ev::build_thread_update_embed(&new, &lines, now(app)), None).await;
    Ok(())
}

async fn on_thread_delete(ctx: Arc<HookCtx<ThreadDelete>>) -> BotResult {
    let Some(settings) = ready(&ctx, "threadDelete", None).await else { return Ok(()) };
    let app = &ctx.app;
    let d = &ctx.data;
    let t = match &d.thread {
        Some(t) => adapters::log_thread(app, d.guild_id, t),
        None => adapters::bare_thread(d.thread_id, d.parent_id),
    };
    let reason = was_deleted_by_bot(app, &d.thread_id.to_string());
    let known = adapters::known_channels(app, d.guild_id);
    let embed = ev::build_thread_delete_embed(&t, reason.as_deref(), bot_id(app).as_deref(), now(app), known.as_ref());
    emit(app, d.guild_id, Some(settings), "threadDelete", Some(embed), None).await;
    Ok(())
}

// ── Roles (§6.7) ──────────────────────────────────────────────────────────────

async fn on_role_create(ctx: Arc<HookCtx<RoleCreate>>) -> BotResult {
    let Some(settings) = ready(&ctx, "roleCreate", None).await else { return Ok(()) };
    let app = &ctx.app;
    emit(app, ctx.data.guild_id, Some(settings), "roleCreate", Some(ev::build_role_create_embed(&adapters::log_role(&ctx.data.role), now(app))), None).await;
    Ok(())
}

fn bot_has_role(app: &App, guild_id: GuildId, role: serenity::all::RoleId) -> bool {
    let Some(d) = app.discord() else { return false };
    let bot = d.cache.current_user().id;
    let Some(g) = d.cache.guild(guild_id) else { return false };
    bot_core::permissions::me(&g, bot).is_some_and(|m| m.roles.contains(&role))
}

async fn on_role_update(ctx: Arc<HookCtx<RoleUpdate>>) -> BotResult {
    let app = &ctx.app;
    let d = &ctx.data;
    // The bot's own roles (or @everyone) changed: a dead log channel may be usable again.
    if bot_has_role(app, d.guild_id, d.role.id) || d.role.id.get() == d.guild_id.get() {
        forget_dead_channels(app, &d.guild_id.to_string(), None);
    }
    let Some(settings) = ready(&ctx, "roleUpdate", None).await else { return Ok(()) };
    let Some(old) = &d.old_role else { return Ok(()) };
    let new = adapters::log_role(&d.role);
    let lines = ev::diff_role(&adapters::log_role(old), &new);
    emit(app, d.guild_id, Some(settings), "roleUpdate", ev::build_role_update_embed(&new, &lines, now(app)), None).await;
    Ok(())
}

async fn on_role_delete(ctx: Arc<HookCtx<RoleDelete>>) -> BotResult {
    let Some(settings) = ready(&ctx, "roleDelete", None).await else { return Ok(()) };
    let app = &ctx.app;
    let Some(role) = &ctx.data.role else { return Ok(()) };
    emit(app, ctx.data.guild_id, Some(settings), "roleDelete", Some(ev::build_role_delete_embed(&adapters::log_role(role), now(app))), None).await;
    Ok(())
}

// ── Server, emojis, invites (§6.8-§6.10) ──────────────────────────────────────

async fn on_guild_update(ctx: Arc<HookCtx<GuildUpdate>>) -> BotResult {
    let Some(settings) = ready(&ctx, "serverUpdate", None).await else { return Ok(()) };
    let app = &ctx.app;
    let Some(old) = &ctx.data.old_guild else { return Ok(()) };
    let (o, n) = (adapters::log_guild(old), adapters::log_partial_guild(&ctx.data.new_guild));
    let lines = ev::diff_guild(&o, &n);
    emit(app, ctx.data.guild_id, Some(settings), "serverUpdate", ev::build_guild_update_embed(&o, &n, &lines, now(app)), None).await;
    Ok(())
}

async fn on_emoji_create(ctx: Arc<HookCtx<EmojiCreate>>) -> BotResult {
    let Some(settings) = ready(&ctx, "emojiCreate", None).await else { return Ok(()) };
    let app = &ctx.app;
    emit(app, ctx.data.guild_id, Some(settings), "emojiCreate", Some(ev::build_emoji_create_embed(&adapters::log_emoji(&ctx.data.emoji), now(app))), None).await;
    Ok(())
}

async fn on_emoji_update(ctx: Arc<HookCtx<EmojiUpdate>>) -> BotResult {
    let Some(settings) = ready(&ctx, "emojiUpdate", None).await else { return Ok(()) };
    let app = &ctx.app;
    let (o, n) = (adapters::log_emoji(&ctx.data.old_emoji), adapters::log_emoji(&ctx.data.emoji));
    let lines = ev::diff_emoji(&o, &n);
    emit(app, ctx.data.guild_id, Some(settings), "emojiUpdate", ev::build_emoji_update_embed(&n, &lines, now(app)), None).await;
    Ok(())
}

async fn on_emoji_delete(ctx: Arc<HookCtx<EmojiDelete>>) -> BotResult {
    let Some(settings) = ready(&ctx, "emojiDelete", None).await else { return Ok(()) };
    let app = &ctx.app;
    emit(app, ctx.data.guild_id, Some(settings), "emojiDelete", Some(ev::build_emoji_delete_embed(&adapters::log_emoji(&ctx.data.emoji), now(app))), None).await;
    Ok(())
}

/// discord.js `InviteCreateAction` / `InviteDeleteAction` emit nothing when the invite's channel is not cached (a
/// deleted channel's invites): the hooks of both events do nothing then.
pub fn invite_channel_cached(app: &App, guild_id: GuildId, channel_id: ChannelId) -> bool {
    bot_core::interaction_helpers::cached_channel(app, guild_id, channel_id).is_some()
}

async fn on_invite_create(ctx: Arc<HookCtx<InviteCreate>>) -> BotResult {
    if !invite_channel_cached(&ctx.app, ctx.data.guild_id, ctx.data.invite.channel_id) {
        return Ok(());
    }
    let Some(settings) = ready(&ctx, "inviteCreate", None).await else { return Ok(()) };
    let app = &ctx.app;
    let inv = adapters::log_invite_create(&ctx.data.invite);
    emit(app, ctx.data.guild_id, Some(settings), "inviteCreate", Some(ev::build_invite_create_embed(&inv, now(app))), None).await;
    Ok(())
}

async fn on_invite_delete(ctx: Arc<HookCtx<InviteDelete>>) -> BotResult {
    if !invite_channel_cached(&ctx.app, ctx.data.guild_id, ctx.data.invite.channel_id) {
        return Ok(());
    }
    let Some(settings) = ready(&ctx, "inviteDelete", None).await else { return Ok(()) };
    let app = &ctx.app;
    let inv = ev::LogInvite { code: ctx.data.invite.code.clone(), channel_id: Some(ctx.data.invite.channel_id.to_string()), ..Default::default() };
    emit(app, ctx.data.guild_id, Some(settings), "inviteDelete", Some(ev::build_invite_delete_embed(&inv, now(app))), None).await;
    Ok(())
}

// ── Declaration (§7) ──────────────────────────────────────────────────────────

/// The `messageCreate` filter: the message text contains an invite link.
pub fn invite_filter(message: &serenity::all::Message) -> bool {
    let snaps = message.message_snapshots.iter().map(|s| Some(s.content.as_str()));
    has_invite_hint(&bot_util::text::effective_text(Some(&message.content), snaps))
}

/// The plugin (`src/plugins/logging.js`). Every hook id is `logging`; all are module-gated except channelDelete.
pub fn plugin() -> Plugin {
    Plugin::new("logging", Some(ModuleKey::Logging))
        .hook(Hook::<MessageCreate>::new("logging", on_message_create).filter(|ctx| {
            invite_filter(&ctx.data.message)
                || snap::should_snapshot(&ctx.app, ctx.app.settings().peek_settings(&ctx.data.guild_id.to_string()).as_ref(), ctx.data.guild_id, &ctx.data.message)
        }))
        .hook(Hook::<MessageUpdate>::new("logging", on_message_edit))
        .hook(Hook::<MessageDelete>::new("logging", on_message_delete))
        .hook(Hook::<MessageDeleteBulk>::new("logging", on_bulk_delete).timeout_ms(15_000))
        .hook(Hook::<VoiceStateUpdate>::new("logging", on_voice_state))
        .hook(Hook::<GuildMemberAdd>::new("logging", on_member_join))
        .hook(Hook::<GuildMemberRemove>::new("logging", on_member_leave))
        .hook(Hook::<GuildMemberUpdate>::new("logging", on_member_update))
        .hook(Hook::<UserUpdate>::new("logging", on_user_update))
        .hook(Hook::<ChannelCreate>::new("logging", on_channel_create))
        .hook(Hook::<ChannelUpdate>::new("logging", on_channel_update))
        .hook(Hook::<ChannelDelete>::new("logging", on_channel_delete).always())
        .hook(Hook::<ThreadCreate>::new("logging", on_thread_create))
        .hook(Hook::<ThreadUpdate>::new("logging", on_thread_update))
        .hook(Hook::<ThreadDelete>::new("logging", on_thread_delete))
        .hook(Hook::<RoleCreate>::new("logging", on_role_create))
        .hook(Hook::<RoleUpdate>::new("logging", on_role_update))
        .hook(Hook::<RoleDelete>::new("logging", on_role_delete))
        .hook(Hook::<GuildBanAdd>::new("logging", on_ban_add))
        .hook(Hook::<GuildBanRemove>::new("logging", on_ban_remove))
        .hook(Hook::<GuildUpdate>::new("logging", on_guild_update))
        .hook(Hook::<EmojiCreate>::new("logging", on_emoji_create))
        .hook(Hook::<EmojiUpdate>::new("logging", on_emoji_update))
        .hook(Hook::<EmojiDelete>::new("logging", on_emoji_delete))
        .hook(Hook::<InviteCreate>::new("logging", on_invite_create))
        .hook(Hook::<InviteDelete>::new("logging", on_invite_delete))
}
