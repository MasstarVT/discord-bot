//! The panel and channel, the lock-down, verify-existing, diagnostics and the status embed (`captchaService.js`).

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use bot_core::App;
use bot_core::embed_builder::apply_guild_color;
use bot_core::interaction_helpers::{PermTarget, Payload, cached_channel, cached_guild, check_channel_usable, missing_bot_permissions};
use bot_core::prelude::*;
use bot_db::{ApplicationForm, GuildSettings};
use serde_json::json;
use serenity::all::{ChannelId, Guild, GuildChannel, GuildId, Member, Message, MessageId, Permissions, RoleId, TargetId, UserId};
use serenity::builder::{CreateAllowedMentions, CreateEmbed, CreateInteractionResponseFollowup, EditInteractionResponse};

use super::*;

/// A failed Discord call: its code and message (`err.code`, `err.message`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordFailure {
    pub code: Option<u64>,
    pub message: String,
}

impl DiscordFailure {
    pub fn of(e: &serenity::Error) -> Self {
        Self { code: bot_core::discord::error_code(e), message: bot_core::discord::error_message(e) }
    }

    pub fn offline() -> Self {
        Self { code: None, message: "Discord is not connected".into() }
    }

    /// `discordErrorText(err)`.
    pub fn text(&self) -> String {
        discord_error_text(self.code, Some(&self.message))
    }
}

impl std::fmt::Display for DiscordFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DiscordFailure {}

pub(crate) fn parse_snowflake(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|v| *v != 0)
}

// ── Panel and channel ─────────────────────────────────────────────────────────

fn to_config_error(err: DiscordFailure, channel_id: ChannelId) -> anyhow::Error {
    if matches!(err.code, Some(50001) | Some(50013)) {
        return CaptchaConfigError::new(
            format!("I can't post in <#{channel_id}>: I need **View Channel, Send Messages, Embed Links and Read Message History** there."),
            Some("Cannot Post Panel"),
        )
        .into();
    }
    err.into()
}

/// The panel payload for a cached channel (`buildPanelPayload(settings, guild, channel)`).
pub fn panel_payload_for(app: &App, guild_id: GuildId, channel_id: ChannelId, settings: &GuildSettings) -> Payload {
    let ctx = bot_core::interaction_helpers::with_cached_guild(app, guild_id, |g| {
        guild_render_context(Some(g), bot_core::permissions::find_channel(g, channel_id))
    })
    .unwrap_or_default();
    build_panel_payload(Some(settings), &ctx).to_payload()
}

/// `createVerificationChannel(guild, interaction)`: #verification, read-only for @everyone, writable for the bot.
pub async fn create_verification_channel(app: &App, guild_id: GuildId, reason: &str) -> Result<GuildChannel, DiscordFailure> {
    let http = app.http().ok_or_else(DiscordFailure::offline)?;
    let bot = bot_user_id(app).ok_or_else(DiscordFailure::offline)?;
    let p = Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY;
    let deny = Permissions::SEND_MESSAGES | Permissions::ADD_REACTIONS | Permissions::CREATE_PUBLIC_THREADS | Permissions::CREATE_PRIVATE_THREADS;
    let me = Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES | Permissions::EMBED_LINKS | Permissions::READ_MESSAGE_HISTORY;
    let body = json!({
        "name": DEFAULT_CHANNEL_NAME,
        "type": 0,
        "position": 0,
        "topic": "Verify here to get access to the server.",
        "permission_overwrites": [
            { "id": guild_id.to_string(), "type": 0, "allow": p.bits().to_string(), "deny": deny.bits().to_string() },
            { "id": bot.to_string(), "type": 1, "allow": me.bits().to_string(), "deny": "0" },
        ],
    });
    let channel = http.create_channel(guild_id, &body, Some(reason)).await.map_err(|e| DiscordFailure::of(&e))?;
    bot_core::discord::cache_created_channel(app, &channel);
    Ok(channel)
}

/// `publishPanel(guild, channel, settings, previous)`: posts the panel, or edits the previous one in place when it is in
/// the same channel. A moved panel deletes the old message best-effort. Fails with [`CaptchaConfigError`] for
/// permission problems.
pub async fn publish_panel(
    app: &App,
    guild_id: GuildId,
    channel_id: ChannelId,
    settings: &GuildSettings,
    prev_channel: Option<&str>,
    prev_message: Option<&str>,
) -> anyhow::Result<Message> {
    let http = app.http().ok_or_else(|| anyhow::Error::from(DiscordFailure::offline()))?;
    let payload = panel_payload_for(app, guild_id, channel_id, settings);
    let channel_str = channel_id.to_string();
    if let Some(mid) = prev_message.and_then(parse_snowflake)
        && prev_channel == Some(channel_str.as_str())
    {
        let mut edit = payload.clone();
        if edit.content.is_none() {
            edit.content = Some(String::new());
        }
        match channel_id.edit_message(http.as_ref(), MessageId::new(mid), edit.to_message_edit()).await {
            Ok(m) => return Ok(m),
            Err(e) => {
                let f = DiscordFailure::of(&e);
                // 10008: the panel was deleted: fall through and post a new one.
                if f.code != Some(10008) {
                    return Err(to_config_error(f, channel_id));
                }
            }
        }
    }
    let msg = channel_id.send_message(http.as_ref(), payload.to_message()).await.map_err(|e| to_config_error(DiscordFailure::of(&e), channel_id))?;
    if let (Some(pm), Some(pc)) = (prev_message.and_then(parse_snowflake), prev_channel.and_then(parse_snowflake))
        && pc != channel_id.get()
        && cached_channel(app, guild_id, ChannelId::new(pc)).is_some()
    {
        let http = http.clone();
        tokio::spawn(async move {
            let _ = http.delete_message(ChannelId::new(pc), MessageId::new(pm), None).await;
        });
    }
    Ok(msg)
}

/// `refreshPanel(guild, settings)`: re-renders the stored panel after a settings change. `Err` is the reason to show.
pub async fn refresh_panel(app: &App, guild_id: GuildId, settings: &GuildSettings) -> Result<(), String> {
    let (Some(cid), Some(mid)) = (
        settings.captcha_channel_id.as_deref().filter(|c| !c.is_empty()),
        settings.captcha_message_id.as_deref().filter(|m| !m.is_empty()),
    ) else {
        return Err("No panel has been posted yet: run `/captcha setup`.".into());
    };
    let channel = parse_snowflake(cid).map(ChannelId::new).filter(|c| cached_channel(app, guild_id, *c).is_some());
    let Some(channel) = channel else { return Err("The verification channel was deleted: run `/captcha setup`.".into()) };
    let Some(http) = app.http() else { return Err("The panel couldn't be updated right now.".into()) };
    let mut edit = panel_payload_for(app, guild_id, channel, settings);
    if edit.content.is_none() {
        edit.content = Some(String::new());
    }
    let result = match parse_snowflake(mid) {
        Some(m) => channel.edit_message(http.as_ref(), MessageId::new(m), edit.to_message_edit()).await.map_err(|e| DiscordFailure::of(&e)),
        None => Err(DiscordFailure { code: Some(10008), message: "Unknown Message".into() }),
    };
    match result {
        Ok(_) => Ok(()),
        Err(f) if f.code == Some(10008) => Err("The panel message was deleted: run `/captcha setup` to post it again.".into()),
        Err(f) if matches!(f.code, Some(50001) | Some(50013)) => Err(format!("I can't edit the panel: check my permissions in <#{cid}>.")),
        Err(f) => {
            bot_util::log_warn!(format!("Captcha panel refresh failed in {guild_id}"), f.message);
            Err("The panel couldn't be updated right now.".into())
        }
    }
}

// ── Lock-down ─────────────────────────────────────────────────────────────────

/// What [`apply_lock`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockResult {
    pub done: Vec<String>,
    pub error: Option<String>,
}

/// `applyLock(guild, { role, channel, plan, reason })`: runs the lock-down steps in an order that never hides a
/// channel from someone halfway, stopping at the first failure. Never fails.
pub async fn apply_lock(app: &App, guild_id: GuildId, role_id: RoleId, channel_id: ChannelId, plan: LockPlan, reason: &str) -> LockResult {
    let mut done = Vec::new();
    let refused = |f: DiscordFailure| {
        if f.code == Some(50013) {
            format!("Discord refused the change: I need **Manage Roles**, and my highest role must be above <@&{role_id}>.")
        } else {
            f.text()
        }
    };
    let Some(http) = app.http() else { return LockResult { done, error: Some(DiscordFailure::offline().text()) } };
    let guild = cached_guild(app, guild_id);
    if plan.channel {
        // permissionOverwrites.edit merges: the overwrite's other bits are kept.
        let (mut allow, mut deny) = guild
            .as_ref()
            .and_then(|g| bot_core::permissions::find_channel(g, channel_id).cloned())
            .and_then(|c| {
                c.permission_overwrites
                    .iter()
                    .find(|o| matches!(o.kind, serenity::all::PermissionOverwriteType::Role(r) if r.get() == guild_id.get()))
                    .map(|o| (o.allow, o.deny))
            })
            .unwrap_or((Permissions::empty(), Permissions::empty()));
        let bits = Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY;
        allow |= bits;
        deny &= !bits;
        let body = json!({ "allow": allow.bits().to_string(), "deny": deny.bits().to_string(), "type": 0 });
        if let Err(e) = http.create_permission(channel_id, TargetId::new(guild_id.get()), &body, Some(reason)).await {
            return LockResult { done, error: Some(refused(DiscordFailure::of(&e))) };
        }
        done.push("verification channel opened to @everyone".to_string());
    }
    if plan.role {
        let perms = guild.as_ref().and_then(|g| g.roles.get(&role_id)).map(|r| r.permissions).unwrap_or_else(Permissions::empty) | Permissions::VIEW_CHANNEL;
        if let Err(e) = http.edit_role(guild_id, role_id, &json!({ "permissions": perms.bits().to_string() }), Some(reason)).await {
            return LockResult { done, error: Some(refused(DiscordFailure::of(&e))) };
        }
        done.push("View Channels given to the verified role".to_string());
    }
    if plan.everyone {
        let everyone = RoleId::new(guild_id.get());
        let perms = guild.as_ref().and_then(|g| g.roles.get(&everyone)).map(|r| r.permissions).unwrap_or_else(Permissions::empty) & !Permissions::VIEW_CHANNEL;
        if let Err(e) = http.edit_role(guild_id, everyone, &json!({ "permissions": perms.bits().to_string() }), Some(reason)).await {
            return LockResult { done, error: Some(refused(DiscordFailure::of(&e))) };
        }
        done.push("View Channels removed from @everyone".to_string());
    }
    LockResult { done, error: None }
}

/// `removeLock(guild, reason)`: gives @everyone View Channels back. `Err` is the text to show. Never fails otherwise.
pub async fn remove_lock(app: &App, guild_id: GuildId, reason: &str) -> Result<(), String> {
    let text = |f: DiscordFailure| if f.code == Some(50013) { "Discord refused the change: I need **Manage Roles**.".to_string() } else { f.text() };
    let Some(http) = app.http() else { return Err(text(DiscordFailure::offline())) };
    let everyone = RoleId::new(guild_id.get());
    let perms = cached_guild(app, guild_id).and_then(|g| g.roles.get(&everyone).map(|r| r.permissions)).unwrap_or_else(Permissions::empty) | Permissions::VIEW_CHANNEL;
    http.edit_role(guild_id, everyone, &json!({ "permissions": perms.bits().to_string() }), Some(reason)).await.map(|_| ()).map_err(|e| text(DiscordFailure::of(&e)))
}

// ── verify-existing ───────────────────────────────────────────────────────────

/// `getBulkRun(guildId)`: a snapshot of the guild's running batch.
pub fn get_bulk_run(app: &App, guild_id: &str) -> Option<BulkRun> {
    let st = state(app);
    let runs = st.bulk_runs.lock().ok()?;
    runs.get(guild_id).and_then(|r| r.lock().ok().map(|r| r.clone()))
}

/// `isBulkTarget(guildId, userId)`: the member is one of the running batch's (synchronous: hook filters use it).
pub fn is_bulk_target(app: &App, guild_id: &str, user_id: &str) -> bool {
    let st = state(app);
    let Ok(runs) = st.bulk_runs.lock() else { return false };
    runs.get(guild_id).and_then(|r| r.lock().ok().map(|r| r.targets.contains(user_id))).unwrap_or(false)
}

/// `/captcha verify-existing stop:true`: asks the running batch to stop after the current member. The progress text, or
/// `None` when nothing runs.
pub fn stop_bulk_run(app: &App, guild_id: &str) -> Option<String> {
    let st = state(app);
    let runs = st.bulk_runs.lock().ok()?;
    let run = runs.get(guild_id)?;
    let mut r = run.lock().ok()?;
    r.abort = true;
    Some(progress_text(&r))
}

fn release_run(app: &App, guild_id: &str, run: &Arc<Mutex<BulkRun>>) {
    let st = state(app);
    if let Ok(mut runs) = st.bulk_runs.lock()
        && runs.get(guild_id).is_some_and(|r| Arc::ptr_eq(r, run))
    {
        runs.remove(guild_id);
    }
}

/// `etaText(run)`.
pub fn eta_text(run: &BulkRun, now_ms: i64) -> String {
    let elapsed = now_ms - run.started_at;
    if run.done < 5 || elapsed <= 0 || run.done >= run.total {
        return String::new();
    }
    let ms_left = ((run.total - run.done) as f64 / run.done as f64) * elapsed as f64;
    let rounded = ((ms_left / 1000.0).ceil() * 1000.0).max(1000.0);
    format!("\n⏱️ About {} left.", bot_util::parse_duration::format_duration_long(rounded))
}

/// The progress or final report's embed (`report(...)`'s text and embed).
pub fn report_embed(run: &BulkRun, is_final: bool, now_ms: i64) -> (CreateEmbed, String) {
    let more_still = run.remaining + if run.abort { run.total.saturating_sub(run.done) } else { 0 };
    let mut text = progress_text(run);
    if !is_final {
        text.push_str(&eta_text(run, now_ms));
    }
    if let Some(e) = &run.error {
        text.push_str(&format!("\n⛔ {e}"));
    }
    if is_final {
        if run.abort && run.error.is_none() {
            text.push_str("\nStopped.");
        }
        text.push_str(&format!("\nGave <@&{}> to {} member(s).", run.role_id, fmt_n(run.ok as f64)));
        if more_still > 0 {
            text.push_str(&format!(" {} more still need it: run `/captcha verify-existing` again to continue.", fmt_n(more_still as f64)));
        }
    }
    let embed = if is_final {
        let title = if run.abort { "Batch Stopped" } else { "Existing Members Verified" };
        if run.error.is_some() { warn_embed(title, Some(&text), None) } else { success_embed(title, Some(&text), None) }
    } else {
        info_embed("Verifying Existing Members", Some(&text), None)
    };
    (embed, text)
}

/// Where a batch reports: the deferred reply while its token lives (15 minutes), then a channel message.
#[derive(Clone)]
pub struct BatchReporter {
    pub app: App,
    pub responder: Responder,
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    pub created_ms: i64,
}

impl BatchReporter {
    async fn report(&self, run: &Arc<Mutex<BulkRun>>, is_final: bool) {
        let now = self.app.clock().now_ms();
        let Ok(snapshot) = run.lock().map(|r| r.clone()) else { return };
        let (embed, text) = report_embed(&snapshot, is_final, now);
        if now - self.created_ms < 14 * 60_000 {
            if is_final {
                // The summary goes out once, as a new message so it notifies the admin; the progress message goes away.
                match self.responder.follow_up(CreateInteractionResponseFollowup::new().embed(embed)).await {
                    Ok(_) => {
                        if let Err(e) = self.responder.delete_reply().await {
                            bot_util::log_debug!(format!("verify-existing progress delete: {e}"));
                        }
                    }
                    Err(e) => bot_util::log_debug!(format!("verify-existing report: {e}")),
                }
            } else if let Err(e) = self.responder.edit_reply(EditInteractionResponse::new().embed(embed)).await {
                bot_util::log_debug!(format!("verify-existing report: {e}"));
            }
        } else if check_channel_usable(&self.app, self.guild_id, Some(self.channel_id), &bot_core::interaction_helpers::CHANNEL_POST_NEEDS).ok
            && let Some(http) = self.app.http()
        {
            let existing = snapshot.channel_message;
            let result = match existing {
                None => self
                    .channel_id
                    .send_message(http.as_ref(), Payload::embed(embed).allowed_mentions(CreateAllowedMentions::new()).to_message())
                    .await
                    .map(|m| Some(m.id)),
                Some(mid) => self.channel_id.edit_message(http.as_ref(), mid, Payload::embed(embed).to_message_edit()).await.map(|_| None),
            };
            match result {
                Ok(Some(id)) => {
                    if let Ok(mut r) = run.lock() {
                        r.channel_message = Some(id);
                    }
                }
                Ok(None) => {}
                Err(e) => bot_util::log_debug!(format!("verify-existing channel report: {}", bot_core::discord::error_message(&e))),
            }
        }
        if is_final {
            bot_util::log_info!(format!("captcha verify-existing finished in {}: {}", self.guild_id, text.replace('\n', " ")));
        }
    }
}

async fn run_batch(rep: BatchReporter, run: Arc<Mutex<BulkRun>>, role_id: RoleId, pending: HashSet<String>, targets: Vec<Member>) {
    let app = rep.app.clone();
    let guild_id = rep.guild_id.to_string();
    let mut consecutive_forbidden = 0;
    let mut last_report = app.clock().now_ms();
    for m in targets {
        if run.lock().map(|r| r.abort).unwrap_or(true) {
            break;
        }
        let result = match app.http() {
            Some(http) => http.add_member_role(rep.guild_id, m.user.id, role_id, Some("Captcha: verify existing members")).await.map_err(|e| DiscordFailure::of(&e)),
            None => Err(DiscordFailure::offline()),
        };
        match result {
            Ok(()) => {
                if let Ok(mut r) = run.lock() {
                    r.ok += 1;
                }
                consecutive_forbidden = 0;
                let uid = m.user.id.to_string();
                if pending.contains(&uid) {
                    // include_pending: finish their verification too.
                    let mut updated = m.clone();
                    if !updated.roles.contains(&role_id) {
                        updated.roles.push(role_id);
                    }
                    // A database failure lands in the JS catch: counted as failed too (after ok), the rest skipped.
                    if let Err(e) = delete_row(app.db(), &guild_id, &uid).await {
                        if let Ok(mut r) = run.lock() {
                            r.failed += 1;
                        }
                        throttled_warn(&app, &format!("{guild_id}:bulk"), &format!("verify-existing: {e}"));
                    } else {
                        cancel_timeout(&app, &guild_id, &uid).await;
                        feat_welcome::welcome_service::assign_autorole(&app, &updated, !updated.pending).await;
                    }
                }
            }
            Err(f) => {
                let mut r = match run.lock() {
                    Ok(r) => r,
                    Err(_) => break,
                };
                match f.code {
                    Some(10007) => r.left += 1,
                    Some(10011) => {
                        r.abort = true;
                        r.error = Some("The verified role was deleted.".into());
                    }
                    Some(50013) => {
                        r.failed += 1;
                        consecutive_forbidden += 1;
                        if consecutive_forbidden >= 5 {
                            r.abort = true;
                            r.error = Some("I lost the Manage Roles permission, or the role is now above mine.".into());
                        }
                    }
                    _ => {
                        r.failed += 1;
                        drop(r);
                        throttled_warn(&app, &format!("{guild_id}:bulk"), &format!("verify-existing: {}", f.message));
                    }
                }
            }
        }
        if let Ok(mut r) = run.lock() {
            r.done += 1;
        }
        if app.clock().now_ms() - last_report >= 15_000 {
            last_report = app.clock().now_ms();
            rep.report(&run, false).await;
        }
    }
    rep.report(&run, true).await;
}

/// `startVerifyExisting(interaction, settings, { includePending })`: starts the verify-existing batch for an
/// already-deferred interaction and returns once it is running (the batch is not awaited; its task is returned). At most
/// one run per guild and [`VERIFY_EXISTING_BATCH`] members per run.
pub async fn start_verify_existing(
    rep: BatchReporter,
    settings: &GuildSettings,
    executor: Option<&Member>,
    include_pending: bool,
) -> anyhow::Result<Option<tokio::task::JoinHandle<()>>> {
    let app = rep.app.clone();
    let gid = rep.guild_id.to_string();
    let edit = |embed: CreateEmbed| {
        let r = rep.responder.clone();
        async move {
            let _ = r.edit_reply(EditInteractionResponse::new().embed(embed)).await;
        }
    };
    if let Some(existing) = get_bulk_run(&app, &gid) {
        edit(warn_embed("Already Running", Some(&progress_text(&existing)), None)).await;
        return Ok(None);
    }
    let role = settings.captcha_role_id.as_deref().and_then(parse_snowflake).map(RoleId::new);
    let role_exists = role.is_some_and(|r| bot_core::interaction_helpers::with_cached_guild(&app, rep.guild_id, |g| g.roles.contains_key(&r)).unwrap_or(false));
    let (Some(role_id), true) = (role, role_exists) else {
        edit(error_embed("Set Up First", Some("Set the verified role with `/captcha setup` or `/captcha role` first."), None)).await;
        return Ok(None);
    };
    let chk = check_verified_role_cached(&app, rep.guild_id, role_id, Some(settings), executor);
    if !chk.ok {
        edit(error_embed("Cannot Use This Role", chk.reason.as_deref(), None)).await;
        return Ok(None);
    }

    // Claim the guild's slot before the first await below, so two admins cannot start two runs.
    let run = Arc::new(Mutex::new(BulkRun { role_id: role_id.to_string(), started_at: app.clock().now_ms(), ..Default::default() }));
    let raced: Option<BulkRun> = {
        let st = state(&app);
        let Ok(mut runs) = st.bulk_runs.lock() else { return Ok(None) };
        match runs.get(&gid).and_then(|r| r.lock().ok().map(|r| r.clone())) {
            Some(existing) => Some(existing),
            None => {
                runs.insert(gid.clone(), run.clone());
                None
            }
        }
    };
    if let Some(existing) = raced {
        edit(warn_embed("Already Running", Some(&progress_text(&existing)), None)).await;
        return Ok(None);
    }
    // A database failure rejects (the router answers its error embed); the guild's slot is released either way.
    let started = prepare_batch(&rep, &run, role_id, include_pending).await;
    match started {
        Ok(Some((pending, targets))) => {
            let (r2, run2, app2, gid2) = (rep.clone(), run.clone(), app.clone(), gid.clone());
            Ok(Some(tokio::spawn(async move {
                run_batch(r2, run2.clone(), role_id, pending, targets).await;
                release_run(&app2, &gid2, &run2);
            })))
        }
        other => {
            release_run(&app, &gid, &run);
            other.map(|_| None)
        }
    }
}

/// Loads the members and answers the start; `None` when the batch does not start.
async fn prepare_batch(rep: &BatchReporter, run: &Arc<Mutex<BulkRun>>, role_id: RoleId, include_pending: bool) -> anyhow::Result<Option<(HashSet<String>, Vec<Member>)>> {
    let app = &rep.app;
    let gid = rep.guild_id.to_string();
    let edit = |embed: CreateEmbed| {
        let r = rep.responder.clone();
        async move { r.edit_reply(EditInteractionResponse::new().embed(embed)).await.map(|_| ()) }
    };
    let _ = edit(info_embed("Loading Members", Some("Fetching the member list…"), None)).await;
    let members = match bot_core::guild_members::fetch_all_members(app, rep.guild_id, 120_000, 100_000).await {
        Ok(m) => m,
        Err(f) => {
            let embed = if f.reason() == "too-large" {
                error_embed("Server Too Large", Some("verify-existing works for servers with up to 100,000 members."), None)
            } else {
                error_embed("Could Not Load Members", Some("Discord did not send the member list in time. Try again in a minute."), None)
            };
            let _ = edit(embed).await;
            return Ok(None);
        }
    };
    let ids: HashSet<String> = members.iter().map(|m| m.user.id.to_string()).collect();
    let mut pending: HashSet<String> = pending_user_ids(app.db(), &gid).await?.into_iter().collect();

    // Stale rows (members who left while the bot was away): only after a complete, successful fetch of an available guild.
    if app.guild_directory().guild_state(&gid) == bot_core::discord::GuildState::Available {
        let stale: Vec<String> = pending.iter().filter(|id| !ids.contains(*id)).cloned().collect();
        if !stale.is_empty() {
            delete_rows(app.db(), &gid, &stale).await?;
            for id in &stale {
                cancel_timeout(app, &gid, id).await;
                pending.remove(id);
            }
        }
    }

    let needing: Vec<Member> = members.iter().filter(|m| !m.roles.contains(&role_id) && (include_pending || !pending.contains(&m.user.id.to_string()))).cloned().collect();
    let skipped_pending =
        if include_pending { 0 } else { members.iter().filter(|m| pending.contains(&m.user.id.to_string()) && !m.roles.contains(&role_id)).count() };
    if run.lock().map(|r| r.abort).unwrap_or(false) {
        let _ = edit(info_embed("Stopped", Some("The batch was stopped before it started."), None)).await;
        return Ok(None);
    }
    if needing.is_empty() {
        let mut text = format!("Everyone here already has <@&{role_id}>.");
        if skipped_pending > 0 {
            text.push_str(&format!(" {skipped_pending} member(s) still waiting to verify were skipped (use include_pending:true)."));
        }
        let _ = edit(success_embed("Nothing To Do", Some(&text), None)).await;
        return Ok(None);
    }

    // Bots are included, so a locked server keeps working for them.
    let targets: Vec<Member> = needing.iter().take(VERIFY_EXISTING_BATCH).cloned().collect();
    let (total, remaining) = (targets.len(), needing.len() - targets.len());
    if let Ok(mut r) = run.lock() {
        r.total = total;
        r.remaining = remaining;
        // Only this run's members: the update hook still handles the rest.
        r.targets = targets.iter().map(|m| m.user.id.to_string()).collect();
        r.started_at = app.clock().now_ms();
    }
    let mut text = format!(
        "Giving <@&{role_id}> to **{}** member(s) in the background. Progress shows here. Stop with `/captcha verify-existing stop:true`.",
        fmt_n(total as f64)
    );
    if remaining > 0 {
        text.push_str(&format!("\n{} more will need another run.", fmt_n(remaining as f64)));
    }
    if skipped_pending > 0 {
        text.push_str(&format!("\nSkipped {skipped_pending} member(s) still waiting to verify."));
    }
    if let Err(e) = edit(info_embed("Verifying Existing Members", Some(&text), None)).await {
        bot_util::log_debug!(format!("verify-existing start reply: {e}"));
    }
    Ok(Some((pending, targets)))
}

// ── Diagnostics and status ────────────────────────────────────────────────────

/// The saved fallback form and whether members could use it (`problem` is `None` when they can).
#[derive(Debug, Clone, Default)]
pub struct FallbackForm {
    pub form_id: Option<String>,
    pub form: Option<ApplicationForm>,
    pub problem: Option<String>,
}

/// `inspectFallbackForm(guild, settings, memberRoleIds)`: the one cheap, bounded lookup `/captcha status`, `/captcha
/// require` and the "role required" reply share. Never fails: a failed lookup is a problem text.
pub async fn inspect_fallback_form(app: &App, guild_id: GuildId, settings: &GuildSettings, member_role_ids: &[String]) -> FallbackForm {
    let Some(form_id) = settings.captcha_fallback_form_id.clone().filter(|f| !f.is_empty()) else { return FallbackForm::default() };
    let (pool, g, f) = (app.db().clone(), guild_id.to_string(), form_id.clone());
    let lookup = async move { feat_welcome::application_service::find_form_by_id(&pool, &g, &f).await };
    let form = match bot_util::async_util::with_timeout(lookup, FORM_LOOKUP_MS, "captcha fallback form").await {
        Ok(form) => form,
        Err(e) => {
            throttled_warn(app, &format!("{guild_id}:fallbackform"), &format!("Captcha: could not load the fallback form in {guild_id}: {e}"));
            return FallbackForm { form_id: Some(form_id), form: None, problem: Some("The form could not be checked right now.".into()) };
        }
    };
    let review = form.as_ref().and_then(|f| feat_welcome::application_service::review_channel_issue(app, guild_id, f));
    let module_on = is_module_enabled(Some(settings), Some(ModuleKey::Applications));
    let problem = fallback_form_problem(form.as_ref(), module_on, member_role_ids, review, app.clock().now());
    FallbackForm { form_id: Some(form_id), form, problem }
}

/// `describeRequirements(guild, settings, fallback)`: the "Required roles" and "Fallback form" texts.
pub fn describe_requirements(role_exists: impl Fn(&str) -> bool, settings: &GuildSettings, fallback: &FallbackForm) -> (String, String) {
    let required = &settings.captcha_required_role_ids;
    let roles = if required.is_empty() {
        "None".to_string()
    } else {
        let items: Vec<String> = required
            .iter()
            .map(|id| {
                if !role_exists(id) {
                    format!("⚠️ deleted (`{id}`)")
                } else if settings.captcha_role_id.as_deref() == Some(id.as_str()) {
                    format!("<@&{id}> (⚠️ verified role: ignored)")
                } else {
                    format!("<@&{id}>")
                }
            })
            .collect();
        bot_util::text::join_with_limit(&items, ", ", 1024, bot_util::text::default_more)
    };
    let mut form = "None".to_string();
    if let Some(id) = &fallback.form_id {
        let name = match &fallback.form {
            Some(f) => format!("**{}**", bot_util::text::clamp_text(&f.name, 100)),
            None => format!("`{id}`"),
        };
        form = match &fallback.problem {
            Some(p) => format!("{name} · ⚠️ {p}"),
            None => name,
        };
    }
    (roles, form)
}

/// `requirementProblems(guild, settings, fallback)`: roles that no longer count, and a fallback form members cannot use.
async fn requirement_problems(app: &App, guild_id: GuildId, guild: Option<&Guild>, settings: &GuildSettings, fallback: Option<&FallbackForm>) -> Vec<String> {
    let mut problems = Vec::new();
    let required = &settings.captcha_required_role_ids;
    let exists = |id: &str| guild.is_some_and(|g| parse_snowflake(id).is_some_and(|r| g.roles.contains_key(&RoleId::new(r))));
    let verified = settings.captcha_role_id.clone();
    if !required.is_empty() {
        let dead = required.iter().filter(|id| !exists(id)).count();
        let live = required.iter().filter(|id| exists(id) && Some(id.as_str()) != verified.as_deref()).count();
        if let Some(v) = verified.as_deref().filter(|v| required.iter().any(|r| r == v)) {
            problems.push(format!("<@&{v}> is both the verified role and a required role. It is ignored: nobody has it before they verify."));
        }
        if live == 0 {
            problems.push("No required role is left (they were deleted), so nobody is held back. Add roles with `/captcha require role:` or turn the requirement off with `/captcha require clear:true`.".into());
        } else if dead > 0 {
            problems.push(format!("{dead} required role(s) were deleted and are ignored. Any change with `/captcha require` drops them from the list."));
        }
    }
    if settings.captcha_fallback_form_id.as_deref().is_some_and(|f| !f.is_empty()) {
        if required.is_empty() {
            problems.push("A fallback form is set but no role is required, so it is never offered: add one with `/captcha require role:`.".into());
        }
        let owned;
        let fb = match fallback {
            Some(f) => f,
            None => {
                owned = inspect_fallback_form(app, guild_id, settings, &[]).await;
                &owned
            }
        };
        if let Some(p) = &fb.problem {
            problems.push(format!("Fallback form: {p}"));
        }
    }
    problems
}

/// `diagnoseCaptcha(guild, settings, { checkPanel, fallback })`: human-readable problems with the current setup.
pub async fn diagnose_captcha(app: &App, guild_id: GuildId, settings: &GuildSettings, check_panel: bool, fallback: Option<&FallbackForm>) -> Vec<String> {
    let mut problems = Vec::new();
    let guild = cached_guild(app, guild_id);
    let locked = guild.as_ref().is_some_and(is_locked);
    let role_id = settings.captcha_role_id.as_deref().filter(|r| !r.is_empty());
    let role = role_id.and_then(parse_snowflake).and_then(|r| guild.as_ref().and_then(|g| g.roles.get(&RoleId::new(r)).cloned()));
    match (role_id, &role) {
        (None, _) => problems.push("No verified role: run `/captcha setup`.".into()),
        (Some(_), None) => problems.push(format!(
            "{}The verified role was deleted: run `/captcha setup` or `/captcha role`.{}",
            if locked { "⛔ " } else { "" },
            if locked { " New members cannot see any channel until this is fixed." } else { "" }
        )),
        (Some(_), Some(r)) => {
            if let (Some(g), Some(bot)) = (guild.as_ref(), bot_user_id(app)) {
                let chk = check_verified_role(g, Some(r), bot, Some(settings), None);
                if !chk.ok {
                    problems.push(format!("The verified role can't be given: {}", chk.reason.unwrap_or_default()));
                }
            }
        }
    }

    let channel_id = settings.captcha_channel_id.as_deref().filter(|c| !c.is_empty());
    let channel = channel_id.and_then(parse_snowflake).and_then(|c| cached_channel(app, guild_id, ChannelId::new(c)));
    let mut channel_usable = false;
    match (channel_id, &channel) {
        (None, _) => problems.push("No verification channel: run `/captcha setup`.".into()),
        (Some(_), None) => problems.push("The verification channel was deleted: run `/captcha setup`.".into()),
        (Some(_), Some(c)) => {
            let cu = check_channel_usable(app, guild_id, Some(c.id), &PANEL_NEEDS);
            if cu.ok {
                channel_usable = true;
            } else {
                problems.push(cu.reason.unwrap_or_default());
            }
        }
    }

    if check_panel && channel_usable && let Some(c) = &channel {
        match settings.captcha_message_id.as_deref().and_then(parse_snowflake) {
            None => problems.push("The panel message is missing: run `/captcha setup` to post it again.".into()),
            Some(mid) => {
                let fetched = match app.http() {
                    Some(http) => {
                        let (cid, http) = (c.id, http.clone());
                        match tokio::time::timeout(std::time::Duration::from_millis(3000), async move { http.get_message(cid, MessageId::new(mid)).await }).await {
                            Ok(Ok(_)) => Ok(()),
                            Ok(Err(e)) => Err(bot_core::discord::error_code(&e)),
                            Err(_) => Err(None),
                        }
                    }
                    None => Err(None),
                };
                match fetched {
                    Ok(()) => {}
                    Err(Some(10008)) => problems.push("The panel message is missing: run `/captcha setup` to post it again.".into()),
                    Err(_) => problems.push("The panel message could not be checked right now.".into()),
                }
            }
        }
    }

    if let (true, Some(g), Some(c)) = (locked, guild.as_ref(), &channel)
        && !everyone_can_view(g, c)
    {
        problems.push(format!("⛔ @everyone can't see <#{}>, so new members can't verify. Run `/captcha lock state:on` to fix it.", c.id));
    }
    if let (true, Some(r)) = (locked, &role)
        && !bot_core::permissions::js_has(r.permissions, Permissions::VIEW_CHANNEL)
    {
        problems.push(format!("Verified members can't see channels: run `/captcha lock state:on` to give <@&{}> View Channels.", r.id));
    }
    if settings.captcha_mode == bot_db::CaptchaMode::Image && !is_image_captcha_available() {
        problems.push(msg::FONT_MISSING.into());
    }
    if settings.captcha_timeout_minutes > 0 && !missing_bot_permissions(app, PermTarget::Guild(guild_id), &["KickMembers"]).is_empty() {
        problems.push("Unverified members can't be kicked: I need **Kick Members**.".into());
    }
    problems.extend(requirement_problems(app, guild_id, guild.as_ref(), settings, fallback).await);
    problems
}

/// `buildStatusEmbed(guild, settings)`: the `/captcha status` embed.
pub async fn build_status_embed(app: &App, guild_id: GuildId, settings: &GuildSettings) -> CreateEmbed {
    let embed = apply_guild_color(info_embed("🛡️ Verification", None, None), Some(settings));
    // Once: the problems list and the form field share it.
    let fallback = inspect_fallback_form(app, guild_id, settings, &[]).await;
    let problems = diagnose_captcha(app, guild_id, settings, true, Some(&fallback)).await;
    let guild = cached_guild(app, guild_id);
    let role_exists = |id: &str| guild.as_ref().is_some_and(|g| parse_snowflake(id).is_some_and(|r| g.roles.contains_key(&RoleId::new(r))));
    let (roles, form) = describe_requirements(role_exists, settings, &fallback);
    let role_id = settings.captcha_role_id.as_deref().filter(|r| !r.is_empty());
    let channel_id = settings.captcha_channel_id.as_deref().filter(|c| !c.is_empty());
    let channel = channel_id.and_then(parse_snowflake).and_then(|c| cached_channel(app, guild_id, ChannelId::new(c)));

    let channel_text = match (channel_id, &channel) {
        (None, _) => "⚠️ Not set".to_string(),
        (Some(_), None) => "⚠️ Deleted".to_string(),
        (Some(_), Some(c)) => match settings.captcha_message_id.as_deref().filter(|m| !m.is_empty()) {
            Some(mid) => format!("<#{}> · [panel](https://discord.com/channels/{guild_id}/{}/{mid})", c.id, c.id),
            None => format!("<#{}> · ⚠️ panel missing", c.id),
        },
    };
    let font_fallback = settings.captcha_mode == bot_db::CaptchaMode::Image && !is_image_captcha_available();
    let pending = match count_pending(app.db(), &guild_id.to_string()).await {
        Ok(n) => fmt_n(n as f64),
        Err(e) => {
            bot_util::log_debug!(format!("captcha status: pending count failed: {e}"));
            "—".to_string()
        }
    };
    let mode = format!(
        "{}{}",
        if settings.captcha_mode == bot_db::CaptchaMode::Button { "🔘 Button click" } else { "🖼️ Image code" },
        if font_fallback { " (button fallback: font missing)" } else { "" }
    );
    let role_text = match role_id {
        None => "⚠️ Not set".to_string(),
        Some(r) if role_exists(r) => format!("<@&{r}>"),
        Some(r) => format!("⚠️ Deleted (`{r}`)"),
    };
    let kick = if settings.captcha_timeout_minutes > 0 {
        format!("after {}", bot_util::parse_duration::format_duration_long(f64::from(settings.captcha_timeout_minutes) * 60_000.0))
    } else {
        "never".to_string()
    };
    let locked = guild.as_ref().is_some_and(is_locked);
    let mut embed = embed
        .field("Status", if settings.captcha_enabled { "✅ On" } else { "❌ Off (`/captcha enable`)" }, true)
        .field("Mode", mode, true)
        .field("Verified role", role_text, true)
        .field("Channel", channel_text, true)
        .field("Kick unverified", kick, true)
        .field("Lock-down", if locked { "🔒 On" } else { "🔓 Off" }, true)
        .field("Waiting to verify", pending, true)
        .field("Required roles", roles, false)
        .field("Fallback form", bot_util::text::clamp_text(&form, 1024), false);
    if let Some(run) = get_bulk_run(app, &guild_id.to_string()) {
        embed = embed.field("Batch", progress_text(&run), true);
    }
    let problems_text = if problems.is_empty() {
        "None".to_string()
    } else {
        let items: Vec<String> = problems.iter().map(|p| format!("• {p}")).collect();
        bot_util::text::join_with_limit(&items, "\n", 1024, bot_util::text::default_more)
    };
    embed.field("Problems", problems_text, false)
}

/// The bot's own id (re-exported for the command).
pub fn bot_id(app: &App) -> Option<UserId> {
    bot_user_id(app)
}
