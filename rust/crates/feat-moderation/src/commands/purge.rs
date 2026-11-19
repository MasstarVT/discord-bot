//! Port of `src/commands/moderation/purge.js`: `/purge` bulk-deletes recent messages in this channel. Every mode is
//! bounded by 1,000 examined messages and a 10-minute deadline; the report is always private.

use std::collections::HashSet;
use std::sync::Mutex;

use bot_core::interaction_helpers::{Payload, reply_error, require_bot_permissions, safe_defer, safe_reply};
use bot_core::prelude::*;
use bot_util::parse_duration::{format_duration_long, parse_duration};
use bot_util::text::format_number;
use serenity::all::{ChannelId, Message, MessageId, Permissions};
use serenity::builder::CreateEmbed;

use crate::guild_ops::{ModRest, rest};

/// Discord's maximum per fetch and per bulk delete.
pub const PAGE_SIZE: usize = 100;
/// The option's maximum, and the limit when `amount` is omitted.
pub const MAX_AMOUNT: usize = 1000;
/// The hard cap on messages examined per run, in every mode.
pub const SCAN_CAP: usize = 1000;
/// 14 days.
pub const AGE_LIMIT_MS: i64 = 14 * 24 * 60 * 60 * 1000;
/// Messages within 5 minutes of the 14-day limit are skipped.
pub const AGE_MARGIN_MS: i64 = 5 * 60 * 1000;
/// No new REST call after 10 minutes (the interaction token lives 15).
pub const DEADLINE_MS: i64 = 10 * 60 * 1000;
const PROGRESS_AFTER_MS: i64 = 5_000;
const PROGRESS_INTERVAL_MS: i64 = 3_000;

/// `FILTER_LABELS[filter]`.
pub fn filter_label(filter: &str) -> Option<&'static str> {
    Some(match filter {
        "bots" => "Bots only",
        "humans" => "Humans only",
        "embeds" => "Has embed",
        "attachments" => "Has attachment",
        _ => return None,
    })
}

/// `ACTIVE_PURGES`: channels with a purge in progress (one per channel).
#[derive(Default)]
pub struct ActivePurges(pub Mutex<HashSet<ChannelId>>);

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("purge", "Bulk-delete recent messages in this channel (MEE6 /clear)")
        .default_member_permissions(Permissions::MODERATE_MEMBERS)
        .option(CommandOption::integer("amount", "How many messages to delete (1-1000)").min_value(1).max_value(1000))
        .option(CommandOption::string("timeframe", "Only messages from the last X, e.g. 30m, 2h, 7d (max 14d)").max_length(32))
        .option(CommandOption::user("user", "Only messages from this user"))
        .option(
            CommandOption::string("filter", "Only messages of this kind")
                .choice("Bots only", "bots")
                .choice("Humans only", "humans")
                .choice("Has embed", "embeds")
                .choice("Has attachment", "attachments"),
        )
        .option(CommandOption::string("contains", "Only messages containing this text").max_length(100))
        .option(CommandOption::boolean("pinned", "Also delete pinned messages (default: keep them)"))
}

/// `/purge` (`module 'moderation'`, MODERATOR).
pub fn command() -> CommandDef {
    CommandDef::new("moderation", data(), Some(ModuleKey::Moderation), execute).permission_level(PermissionLevel::Moderator)
}

// ── Helpers ────────────────────────────────────────────────────────────────────

/// Why a purge stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    Exhausted,
    Limit,
    Age,
    Timeframe,
    ScanCap,
    Deadline,
    Permission,
    ChannelGone,
    Error,
}

fn stop_for_error(code: Option<u64>) -> Option<Stop> {
    match code {
        Some(50001) | Some(50013) => Some(Stop::Permission),
        Some(10003) => Some(Stop::ChannelGone),
        _ => None,
    }
}

/// The filters of [`matches_filters`]; `contains` already trimmed and lowercased.
#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub user_id: Option<String>,
    pub filter: Option<String>,
    pub contains: Option<String>,
}

/// `matchesFilters(message, opts)`: every condition is ANDed; webhooks count as bots; `embeds` needs an embed.
pub fn matches_filters(m: &Message, f: &Filters) -> bool {
    if let Some(u) = &f.user_id
        && m.author.id.to_string() != *u
    {
        return false;
    }
    let botlike = m.author.bot || m.webhook_id.is_some();
    match f.filter.as_deref() {
        Some("bots") if !botlike => return false,
        Some("humans") if botlike => return false,
        Some("embeds") if m.embeds.is_empty() => return false,
        Some("attachments") if m.attachments.is_empty() => return false,
        _ => {}
    }
    if let Some(c) = &f.contains
        && !c.is_empty()
        && !m.content.to_lowercase().contains(c.as_str())
    {
        return false;
    }
    true
}

/// discord.js `message.deletable` for a channel where the bot has Manage Messages: only the
/// `UndeletableMessageTypes` of discord.js 14.27 (RecipientAdd, RecipientRemove, Call, ChannelNameChange,
/// ChannelIconChange, ThreadStarterMessage) are not.
pub fn deletable(m: &Message) -> bool {
    !matches!(u8::from(m.kind), 1 | 2 | 3 | 4 | 5 | 21)
}

fn created_ms(m: &Message) -> i64 {
    m.timestamp.unix_timestamp() * 1000 + i64::from(m.timestamp.timestamp_subsec_millis())
}

/// The options of [`collect_purge_targets`].
#[derive(Debug, Clone)]
pub struct CollectOptions {
    pub limit: usize,
    pub cutoff_ts: i64,
    /// `true`: the cut-off is the timeframe (else the 14-day age limit).
    pub cutoff_is_timeframe: bool,
    pub filters: Filters,
    pub include_pinned: bool,
    pub scan_cap: usize,
    pub deadline: i64,
}

impl Default for CollectOptions {
    fn default() -> Self {
        Self {
            limit: MAX_AMOUNT,
            cutoff_ts: i64::MIN,
            cutoff_is_timeframe: false,
            filters: Filters::default(),
            include_pinned: false,
            scan_cap: SCAN_CAP,
            deadline: i64::MAX,
        }
    }
}

/// `collectPurgeTargets`'s result.
#[derive(Debug, Clone)]
pub struct Collected {
    pub targets: Vec<MessageId>,
    pub scanned: usize,
    pub skipped_pinned: usize,
    pub skipped_undeletable: usize,
    pub stop_reason: Stop,
    pub error_code: Option<u64>,
}

/// `collectPurgeTargets(channel, opts)`: pages the channel newest to oldest and collects the messages to delete.
/// `now` is the clock (`Date.now`). Never fails.
pub async fn collect_purge_targets(r: &dyn ModRest, channel: ChannelId, o: &CollectOptions, now: &(dyn Fn() -> i64 + Sync)) -> Collected {
    let mut out = Collected { targets: Vec::new(), scanned: 0, skipped_pinned: 0, skipped_undeletable: 0, stop_reason: Stop::Exhausted, error_code: None };
    let mut before: Option<MessageId> = None;
    loop {
        if now() > o.deadline {
            out.stop_reason = Stop::Deadline;
            break;
        }
        let page_size = PAGE_SIZE.min(o.scan_cap.saturating_sub(out.scanned));
        if page_size == 0 {
            out.stop_reason = Stop::ScanCap;
            break;
        }
        let mut msgs = match r.get_messages(channel, before, page_size as u8).await {
            Ok(p) => p,
            Err(e) => {
                out.stop_reason = stop_for_error(e.code).unwrap_or(Stop::Error);
                out.error_code = e.code;
                break;
            }
        };
        if msgs.is_empty() {
            out.stop_reason = Stop::Exhausted;
            break;
        }
        msgs.sort_by_key(|m| std::cmp::Reverse(created_ms(m)));
        let mut done = false;
        for m in &msgs {
            // Pages come newest first, so everything after the first message at or below the cut-off is older too.
            if created_ms(m) <= o.cutoff_ts {
                out.stop_reason = if o.cutoff_is_timeframe { Stop::Timeframe } else { Stop::Age };
                done = true;
                break;
            }
            out.scanned += 1;
            if !matches_filters(m, &o.filters) {
                continue;
            }
            if m.pinned && !o.include_pinned {
                out.skipped_pinned += 1;
                continue;
            }
            if !deletable(m) {
                out.skipped_undeletable += 1;
                continue;
            }
            out.targets.push(m.id);
            if out.targets.len() >= o.limit {
                out.stop_reason = Stop::Limit;
                done = true;
                break;
            }
        }
        if done {
            break;
        }
        if msgs.len() < page_size {
            out.stop_reason = Stop::Exhausted;
            break;
        }
        before = msgs.iter().map(|m| m.id).min();
    }
    out
}

/// `deleteInChunks`'s result.
#[derive(Debug, Clone, Default)]
pub struct Deleted {
    pub deleted: usize,
    pub already_gone: usize,
    pub failed: usize,
    pub stop_reason: Option<Stop>,
    pub error_code: Option<u64>,
}

/// `deleteInChunks(channel, messages, { reason, deadline, onProgress })`: deletes in chunks of 100, newest first. A
/// message counts as deleted only when its REST call succeeded. The deletion tracker is marked right before every
/// REST delete. Never fails.
pub async fn delete_in_chunks(
    app: &App,
    r: &dyn ModRest,
    channel: ChannelId,
    messages: &[MessageId],
    reason: &str,
    deadline: i64,
    mut on_progress: impl FnMut(usize),
) -> Deleted {
    let mut out = Deleted::default();
    let mut warned = false;
    let now = || app.clock().now_ms();
    'outer: for chunk in messages.chunks(PAGE_SIZE) {
        if now() > deadline {
            out.stop_reason = Some(Stop::Deadline);
            break;
        }
        let ids: Vec<String> = chunk.iter().map(|m| m.to_string()).collect();
        bot_core::deletion_tracker::mark_bot_deletion(app, &ids, Some(reason), None);
        let mut singles: Vec<MessageId> = Vec::new();
        if chunk.len() == 1 {
            singles.push(chunk[0]);
        } else {
            match r.bulk_delete(channel, chunk.to_vec()).await {
                Ok(()) => out.deleted += chunk.len(),
                Err(e) => {
                    if let Some(stop) = stop_for_error(e.code) {
                        out.stop_reason = Some(stop);
                        out.error_code = e.code;
                        break;
                    }
                    // 50034 (too old), 50035, 10008, 5xx ...: delete this chunk one message at a time.
                    singles.extend_from_slice(chunk);
                }
            }
        }
        let single_mode = chunk.len() > 1;
        for id in singles {
            if single_mode {
                if now() > deadline {
                    out.stop_reason = Some(Stop::Deadline);
                    break 'outer;
                }
                bot_core::deletion_tracker::mark_bot_deletion(app, &[id.to_string()], Some(reason), None);
            }
            match r.delete_message(channel, id).await {
                Ok(()) => out.deleted += 1,
                Err(e) if e.code == Some(10008) => out.already_gone += 1,
                Err(e) => {
                    if let Some(stop) = stop_for_error(e.code) {
                        out.stop_reason = Some(stop);
                        out.error_code = e.code;
                        break 'outer;
                    }
                    out.failed += 1;
                    if !warned {
                        warned = true;
                        let what = e.code.map(|c| c.to_string()).unwrap_or_else(|| e.message.clone());
                        bot_util::log_warn!(format!("purge: delete {id} failed ({what})"));
                    }
                }
            }
        }
        on_progress(out.deleted);
    }
    out
}

/// The report's options echo.
#[derive(Debug, Clone, Default)]
pub struct ReportOpts {
    pub amount: Option<i64>,
    pub timeframe_ms: Option<i64>,
    pub user_id: Option<String>,
    pub filter: Option<String>,
    pub contains: Option<String>,
}

/// `buildPurgeReport(result)`: the final (ephemeral) report.
pub fn build_purge_report(c: &Collected, d: &Deleted, o: &ReportOpts) -> CreateEmbed {
    let n = |v: usize| format_number(v as f64);
    let mut lines = vec![format!("**Deleted:** {} message(s)", n(d.deleted)), format!("**Checked:** {} message(s)", n(c.scanned))];
    if let Some(a) = o.amount.filter(|a| *a != 0) {
        lines.push(format!("**Limit:** {a}"));
    }
    if let Some(t) = o.timeframe_ms.filter(|t| *t != 0) {
        lines.push(format!("**Timeframe:** last {}", format_duration_long(t as f64)));
    }
    if let Some(u) = &o.user_id {
        lines.push(format!("**User:** <@{u}>"));
    }
    if let Some(label) = o.filter.as_deref().and_then(filter_label) {
        lines.push(format!("**Filter:** {label}"));
    }
    if let Some(text) = o.contains.as_deref().filter(|s| !s.is_empty()) {
        lines.push(format!("**Contains:** `{}`", text.replace('`', "")));
    }

    let stops = [Some(c.stop_reason), d.stop_reason];
    let has = |s: Stop| stops.contains(&Some(s));
    let mut notes: Vec<String> = Vec::new();
    if c.stop_reason == Stop::Age {
        notes.push("Stopped at messages older than 14 days; Discord does not let bots bulk-delete those.".into());
    }
    if c.stop_reason == Stop::ScanCap {
        notes.push(format!("Stopped after checking {} messages (the per-run limit). Run the command again to continue.", n(SCAN_CAP)));
    }
    if c.skipped_pinned > 0 {
        notes.push(format!("Kept {} pinned message(s). Set `pinned` to True to include them.", c.skipped_pinned));
    }
    if c.skipped_undeletable > 0 {
        notes.push(format!("Skipped {} system message(s) that cannot be deleted.", c.skipped_undeletable));
    }
    if d.already_gone > 0 {
        notes.push(format!("{} message(s) were already deleted by someone else.", d.already_gone));
    }
    if d.failed > 0 {
        notes.push(format!("{} message(s) could not be deleted.", d.failed));
    }
    if has(Stop::Permission) {
        notes.push("Stopped early: I lost access or the Manage Messages permission in this channel.".into());
    }
    if has(Stop::ChannelGone) {
        notes.push("Stopped early: this channel was deleted.".into());
    }
    if has(Stop::Deadline) {
        notes.push("Stopped early: the purge reached the 10-minute time limit.".into());
    }
    if c.stop_reason == Stop::Error {
        let code = c.error_code.map(|c| format!(" (code {c})")).unwrap_or_default();
        notes.push(format!("Stopped early: Discord returned an error{code}. Try again later."));
    }
    let note_text = notes.iter().map(|n| format!("• {n}")).collect::<Vec<_>>().join("\n");
    let with_notes = |head: &str| if note_text.is_empty() { head.to_string() } else { format!("{head}\n\n{note_text}") };
    let early = [Stop::Permission, Stop::ChannelGone, Stop::Deadline, Stop::Error].iter().any(|s| has(*s));

    if d.deleted > 0 {
        let body = with_notes(&lines.join("\n"));
        return if early || d.failed > 0 { warn_embed("Purge Partly Complete", Some(&body), None) } else { success_embed("Purge Complete", Some(&body), None) };
    }
    if c.targets.is_empty() {
        if matches!(c.stop_reason, Stop::Permission | Stop::ChannelGone | Stop::Deadline | Stop::Error) {
            if c.stop_reason == Stop::Permission && c.scanned == 0 {
                return error_embed("Purge Failed", Some("I can't read the message history in this channel."), None);
            }
            return error_embed("Purge Failed", Some(&note_text), None);
        }
        let first = if c.scanned == 0 && c.stop_reason == Stop::Exhausted {
            "This channel has no messages.".to_string()
        } else if c.scanned == 0 && c.stop_reason == Stop::Age {
            "Every message here is older than 14 days. Discord does not let bots bulk-delete those.".to_string()
        } else if c.scanned == 0 && c.stop_reason == Stop::Timeframe {
            format!("No messages were sent in the last {}.", format_duration_long(o.timeframe_ms.unwrap_or(0) as f64))
        } else {
            format!("No messages matched your options among the {} message(s) I checked.", n(c.scanned))
        };
        return error_embed("Nothing to Delete", Some(&with_notes(&first)), None);
    }
    error_embed("Purge Failed", Some(&with_notes(&lines.join("\n"))), None)
}

// ── Command ────────────────────────────────────────────────────────────────────

/// Channel types a purge works in (`typeof channel.bulkDelete === 'function'`): text, announcement, voice, stage
/// and threads.
pub fn purgeable(kind: u8) -> bool {
    matches!(kind, 0 | 2 | 5 | 10 | 11 | 12 | 13)
}

async fn execute(ctx: CommandCtx) -> BotResult {
    let r = &ctx.responder;
    // 1. Channel.
    let channel_id = ctx.channel_id();
    let channel = ctx.guild_id().and_then(|g| bot_core::interaction_helpers::cached_channel(&ctx.app, g, channel_id));
    let Some(channel) = channel.filter(|c| purgeable(u8::from(c.kind))) else {
        reply_error(r, "Unsupported Channel", Some("Use this command in a text channel, a thread, or a voice/stage channel chat."), true).await;
        return Ok(());
    };

    // 2. Options.
    let amount = ctx.get_integer("amount");
    let timeframe_str = ctx.get_string("timeframe").map(|s| bot_util::js::trim(&s).to_string()).filter(|s| !s.is_empty());
    let user = ctx.get_user_id("user");
    let filter = ctx.get_string("filter");
    let contains = ctx.get_string("contains").map(|s| bot_util::js::trim(&s).to_lowercase()).filter(|s| !s.is_empty());
    let include_pinned = ctx.get_boolean("pinned").unwrap_or(false);

    if amount.is_none_or(|a| a == 0) && timeframe_str.is_none() {
        reply_error(r, "Missing Options", Some("Provide an `amount`, a `timeframe`, or both."), true).await;
        return Ok(());
    }
    let mut timeframe_ms = None;
    if let Some(tf) = &timeframe_str {
        let Some(ms) = parse_duration(tf) else {
            let clean = tf.replace('`', "");
            reply_error(r, "Invalid Timeframe", Some(&format!("Could not read `{clean}`. Try `30m`, `2h` or `7d`.")), true).await;
            return Ok(());
        };
        if ms > AGE_LIMIT_MS {
            reply_error(r, "Timeframe Too Large", Some("Discord only lets bots bulk-delete messages from the last **14 days**."), true).await;
            return Ok(());
        }
        timeframe_ms = Some(ms);
    }

    // 3. Archived thread.
    if bot_core::permissions::is_thread(&channel) && channel.thread_metadata.is_some_and(|t| t.archived) {
        reply_error(r, "Thread Archived", Some("This thread is archived. Unarchive it, then try again."), true).await;
        return Ok(());
    }

    // 4. Bot timed out.
    if let Some(g) = ctx.guild_id()
        && let Some(guild) = crate::guild_ops::load_guild(&ctx.app, g)
        && guild.view.me.as_ref().is_some_and(|m| m.is_communication_disabled(ctx.app.clock().now_ms()))
    {
        let admin = bot_core::interaction_helpers::bot_permissions(&ctx.app, bot_core::interaction_helpers::PermTarget::Channel(g, channel_id))
            .is_some_and(|p| p.contains(Permissions::ADMINISTRATOR));
        if !admin {
            reply_error(r, "I Am Timed Out", Some("I am timed out in this server, so I cannot delete messages until the timeout ends."), true).await;
            return Ok(());
        }
    }

    // 5. Bot permissions: checked before the deferral, so the refusal is immediate and private.
    if !require_bot_permissions(&ctx, &["ViewChannel", "ReadMessageHistory", "ManageMessages"], Some(channel_id), true).await {
        return Ok(());
    }

    // 6. One purge per channel.
    let active = ctx.app.ext::<ActivePurges>();
    let claimed = active.0.lock().unwrap_or_else(|e| e.into_inner()).insert(channel_id);
    if !claimed {
        reply_error(r, "Purge In Progress", Some("Another purge is already running in this channel. Wait for it to finish, then try again."), true).await;
        return Ok(());
    }
    // Released however the run ends (the JS `finally`), a panic included.
    let _release = Release { active: active.clone(), channel: channel_id };
    run_purge(&ctx, channel_id, amount, timeframe_ms, user.map(|u| u.to_string()), filter, contains, include_pinned).await;
    Ok(())
}

/// Removes the channel from [`ActivePurges`] when dropped.
struct Release {
    active: std::sync::Arc<ActivePurges>,
    channel: ChannelId,
}

impl Drop for Release {
    fn drop(&mut self) {
        self.active.0.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.channel);
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_purge(
    ctx: &CommandCtx,
    channel_id: ChannelId,
    amount: Option<i64>,
    timeframe_ms: Option<i64>,
    user_id: Option<String>,
    filter: Option<String>,
    contains: Option<String>,
    include_pinned: bool,
) {
    // 7. Defer (always private, whatever modVisibility says).
    if !safe_defer(&ctx.responder, true).await {
        return;
    }
    let app = &ctx.app;
    let rst = rest(app);
    // 8. Cut-offs.
    let start = app.clock().now_ms();
    let deadline = start + DEADLINE_MS;
    let age_cutoff = start - (AGE_LIMIT_MS - AGE_MARGIN_MS);
    let tf_cutoff = timeframe_ms.map(|t| start - t).unwrap_or(i64::MIN);
    let opts = CollectOptions {
        limit: amount.filter(|a| *a > 0).map(|a| a as usize).unwrap_or(MAX_AMOUNT),
        cutoff_ts: age_cutoff.max(tf_cutoff),
        cutoff_is_timeframe: tf_cutoff > age_cutoff,
        filters: Filters { user_id: user_id.clone(), filter: filter.clone(), contains: contains.clone() },
        include_pinned,
        scan_cap: SCAN_CAP,
        deadline,
    };

    // 9. Collect.
    let now = || app.clock().now_ms();
    let collected = collect_purge_targets(rst.as_ref(), channel_id, &opts, &now).await;

    // 10. Delete.
    let mut deletion = Deleted::default();
    let mut last_progress: Option<tokio::task::JoinHandle<()>> = None;
    if !collected.targets.is_empty() {
        let reason = format!("/purge by {} ({})", bot_core::adapters::user_tag(ctx.user()), ctx.user().id);
        let total = collected.targets.len();
        let mut last_at = 0i64;
        let responder = ctx.responder.clone();
        let on_progress = |so_far: usize| {
            let t = app.clock().now_ms();
            if t - start < PROGRESS_AFTER_MS || t - last_at < PROGRESS_INTERVAL_MS {
                return;
            }
            last_at = t;
            // Not awaited: the deletion loop never waits for the edit.
            let text = format!("Deleted {} of {} message(s) so far…", format_number(so_far as f64), format_number(total as f64));
            let rsp = responder.clone();
            let prev = last_progress.take();
            last_progress = Some(tokio::spawn(async move {
                if let Some(p) = prev {
                    let _ = p.await;
                }
                let _ = rsp.edit_reply(Payload::embed(info_embed("Purging…", Some(&text), None)).to_edit()).await;
            }));
        };
        deletion = delete_in_chunks(app, rst.as_ref(), channel_id, &collected.targets, &reason, deadline, on_progress).await;
    }

    // 11. Report. A late progress edit must never overwrite it, so wait for the last one first.
    if let Some(p) = last_progress {
        let _ = p.await;
    }
    let report = build_purge_report(&collected, &deletion, &ReportOpts { amount, timeframe_ms, user_id, filter, contains });
    safe_reply(&ctx.responder, Payload::embed(report)).await;
}
