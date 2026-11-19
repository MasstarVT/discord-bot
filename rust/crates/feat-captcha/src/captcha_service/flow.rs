//! The challenge and verification flow, the hook bodies, the `captcha.timeout` job and the component handlers
//! (`captchaService.js`).

use std::sync::{Arc, RwLock};

use bot_core::App;
use bot_core::error::BoxFuture;
use bot_core::interaction_helpers::{is_interaction_gone, member_role_ids, with_cached_guild};
use bot_core::prelude::*;
use bot_db::GuildSettings;
use feat_moderation::guild_ops::UserRef;
use feat_moderation::moderation_service::{ModActionInput, execute_mod_action};
use serenity::all::{GuildId, Member, MessageFlags, RoleId, UserId};
use serenity::builder::{CreateActionRow, CreateInputText, CreateInteractionResponseMessage, CreateMessage, CreateModal};

use super::panel::{DiscordFailure, parse_snowflake};
use super::*;

// ── Challenge and verification ────────────────────────────────────────────────

/// `issueChallenge(member, settings, { content })`: draws a new code, stores only its hash and returns the challenge
/// message. Renders BEFORE the write, so a failed render keeps the old challenge.
pub async fn issue_challenge(app: &App, guild_id: &str, user_id: &str, settings: Option<&GuildSettings>, content: &str) -> anyhow::Result<Reply> {
    let code = generate_code();
    let expires_at = DateTime::from_timestamp_millis(app.clock().now_ms() + CHALLENGE_TTL_MS).unwrap_or_else(Utc::now);
    let image = render_captcha_image_async(code.clone()).await?;
    let answer_hash = hash_answer(guild_id, user_id, &code);
    store_challenge(app, guild_id, user_id, &answer_hash, expires_at).await?;
    Ok(build_challenge_payload(settings, &expires_at, image, content))
}

/// What [`complete_verification`] did.
#[derive(Debug, Clone)]
pub enum Completion {
    /// Verified: `added` is whether the role was given now; `member` has the role.
    Done { added: bool, member: Box<Member> },
    /// Not verified: the reply to show.
    Failed(Reply),
}

/// `completeVerification(member, settings, role)`: gives the verified role, clears the pending row and the timeout job,
/// then the autoroles (role first, so welcome's captcha gate is open). Discord permission problems are a
/// [`Completion::Failed`]; database errors are errors.
pub async fn complete_verification(app: &App, member: &Member, role_id: RoleId) -> anyhow::Result<Completion> {
    let guild_id = member.guild_id.to_string();
    let user_id = member.user.id.to_string();
    // The guildMemberUpdate hook skips this member, so the autoroles are granted exactly once.
    let _guard = InFlightGuard::new(app, format!("{guild_id}:{user_id}"));
    let mut m = member.clone();
    let mut added = false;
    if !m.roles.contains(&role_id) {
        let result = match app.http() {
            Some(http) => http.add_member_role(member.guild_id, member.user.id, role_id, Some("Captcha verification passed")).await.map_err(|e| DiscordFailure::of(&e)),
            None => Err(DiscordFailure::offline()),
        };
        match result {
            Ok(()) => {
                m.roles.push(role_id);
                added = true;
            }
            // 10011: the role is gone; 10007: left mid-flow (nobody will see it).
            Err(f) if matches!(f.code, Some(10011) | Some(10007)) => return Ok(Completion::Failed(msg::not_set_up())),
            Err(f) if matches!(f.code, Some(50013) | Some(50001)) => {
                throttled_warn(app, &format!("{guild_id}:grant"), &format!("Captcha: cannot give the verified role in {guild_id}: {}", f.text()));
                return Ok(Completion::Failed(msg::role_failed()));
            }
            Err(f) => return Err(f.into()),
        }
    }
    let count = delete_row(app.db(), &guild_id, &user_id).await?;
    if count > 0 {
        cancel_timeout(app, &guild_id, &user_id).await;
    }
    if added || count > 0 {
        // Never fails; skips only the captcha gate.
        feat_welcome::welcome_service::assign_autorole(app, &m, true).await;
    }
    Ok(Completion::Done { added, member: Box::new(m) })
}

// ── Hook bodies ───────────────────────────────────────────────────────────────

/// `autoVerifyBot(member, role, settings)`: bots skip verification.
async fn auto_verify_bot(app: &App, member: &Member, role_id: RoleId, settings: &GuildSettings) {
    let mut m = member.clone();
    if check_verified_role_cached(app, member.guild_id, role_id, Some(settings), None).ok {
        let r = match app.http() {
            Some(http) => http.add_member_role(member.guild_id, member.user.id, role_id, Some("Captcha: bots skip verification")).await.map_err(|e| DiscordFailure::of(&e)),
            None => Err(DiscordFailure::offline()),
        };
        match r {
            Ok(()) => m.roles.push(role_id),
            Err(f) => throttled_warn(
                app,
                &format!("{}:botgrant", member.guild_id),
                &format!("Captcha: cannot give the verified role to bot {} in {}: {}", member.user.id, member.guild_id, f.text()),
            ),
        }
    }
    feat_welcome::welcome_service::assign_autorole(app, &m, true).await;
}

fn cached_role(app: &App, guild_id: GuildId, role_id: Option<&str>) -> Option<RoleId> {
    let rid = RoleId::new(parse_snowflake(role_id?)?);
    with_cached_guild(app, guild_id, |g| g.roles.contains_key(&rid)).filter(|ok| *ok).map(|_| rid)
}

/// `onMemberJoin(ctx)`: the join gate (guildMemberAdd, module-gated; bots arrive here too).
pub async fn on_member_join(ctx: Arc<HookCtx<GuildMemberAdd>>) -> BotResult {
    let app = &ctx.app;
    let member = &ctx.data.member;
    let Some(settings) = ctx.get_settings().await else { return Ok(()) };
    if !settings.captcha_enabled {
        return Ok(());
    }
    // A deleted role never gates (matches welcome's gate).
    let Some(role_id) = cached_role(app, ctx.data.guild_id, settings.captcha_role_id.as_deref()) else { return Ok(()) };
    if member.roles.contains(&role_id) {
        return Ok(());
    }
    if member.user.bot {
        auto_verify_bot(app, member, role_id, &settings).await;
        return Ok(());
    }
    // Before any I/O: welcome must see it even if this hook is cut short.
    ctx.state.insert(feat_welcome::welcome_service::CAPTCHA_PENDING_KEY, true);
    let (g, u) = (ctx.data.guild_id.to_string(), member.user.id.to_string());
    if let Err(e) = record_pending(app, &g, &u).await {
        bot_util::log_error!(format!("Captcha: could not record {u} in {g}"), bot_core::error::log_arg(&e));
        // Still gated; pressing Verify upserts the row later.
        return Ok(());
    }
    if settings.captcha_timeout_minutes > 0 {
        let joined = member.joined_at.map(|t| t.timestamp_millis());
        let run_at = timeout_run_at(joined, i64::from(settings.captcha_timeout_minutes), app.clock().now_ms());
        let job = ScheduleJob::new(g.clone(), JobType::CaptchaTimeout, run_at)
            .payload(serde_json::json!({ "userId": u }))
            .dedupe_key(timeout_job_key(&g, &u))
            // A rejoin resets the deadline.
            .on_conflict(OnConflict::Replace);
        if let Err(e) = app.scheduler().schedule_job(job).await {
            bot_util::log_error!(format!("Captcha: could not schedule timeout for {u} in {g}"), bot_core::error::log_arg(&e));
        }
    }
    Ok(())
}

/// `onMemberLeave(ctx)`: guildMemberRemove (runs even while the module is off).
pub async fn on_member_leave(ctx: Arc<HookCtx<GuildMemberRemove>>) -> BotResult {
    let (g, u) = (ctx.data.guild_id.to_string(), ctx.data.user.id.to_string());
    if delete_row(ctx.app.db(), &g, &u).await? > 0 {
        cancel_timeout(&ctx.app, &g, &u).await;
    }
    Ok(())
}

/// `onMemberUpdate(ctx)`: the verified role given by someone else verifies a pending member.
pub async fn on_member_update(ctx: Arc<HookCtx<GuildMemberUpdate>>) -> BotResult {
    let Some(settings) = ctx.get_settings().await else { return Ok(()) };
    let Some(rid) = settings.captcha_role_id.clone().filter(|r| !r.is_empty()) else { return Ok(()) };
    let nm = &ctx.data.member;
    let old = ctx.data.old_member.as_ref().map(|m| m.roles.as_slice());
    if !is_role_newly_added(old, &nm.roles, ctx.data.old_known, Some(&rid)) {
        return Ok(());
    }
    let (g, u) = (ctx.data.guild_id.to_string(), nm.user.id.to_string());
    if is_in_flight(&ctx.app, &g, &u) || is_bulk_target(&ctx.app, &g, &u) {
        return Ok(());
    }
    // Not pending: a role shuffle, verify-existing, or our own grant.
    if delete_row(ctx.app.db(), &g, &u).await? == 0 {
        return Ok(());
    }
    cancel_timeout(&ctx.app, &g, &u).await;
    // Still in Rules Screening: let welcome schedule its screening watch instead of forcing roles now.
    feat_welcome::welcome_service::assign_autorole(&ctx.app, nm, !nm.pending).await;
    Ok(())
}

// ── Timeout job ───────────────────────────────────────────────────────────────

/// What the kick through moderation answered (`executeModAction`'s `{ ok }` or `{ code, error, applied }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KickOutcome {
    Kicked,
    Failed { code: Option<String>, error: String, applied: bool },
}

/// The `deps.executeModAction` test seam: replaces the kick through moderation.
pub type ModActionFn = Arc<dyn Fn(ModActionInput) -> BoxFuture<KickOutcome> + Send + Sync>;

/// Where the seam lives (per app).
#[derive(Default)]
pub struct ModActionOverride(RwLock<Option<ModActionFn>>);

/// Installs (or with `None` removes) the kick seam for this app.
pub fn set_mod_action(app: &App, f: Option<ModActionFn>) {
    if let Ok(mut w) = app.ext::<ModActionOverride>().0.write() {
        *w = f;
    }
}

async fn run_mod_action(app: &App, input: ModActionInput) -> KickOutcome {
    let seam = app.ext::<ModActionOverride>().0.read().ok().and_then(|r| r.clone());
    if let Some(f) = seam {
        return f(input).await;
    }
    match execute_mod_action(app, input).await {
        Ok(_) => KickOutcome::Kicked,
        Err(e) => KickOutcome::Failed { code: Some(e.code.as_str().to_string()), error: e.error, applied: e.applied },
    }
}

/// A friendly DM before the kick. Best-effort: closed DMs (50007) are normal and never reported.
async fn dm_before_kick(app: &App, guild_id: GuildId, guild_name: &str, user_id: UserId, span: &str) {
    let n = app.cache().sliding_window(&kick_dm_key(&guild_id.to_string()), KICK_DM_LIMIT.window_ms, None).await;
    // Over the cap: skip the DM. None (Redis down): fail-open, send it.
    if n.is_some_and(|n| n > KICK_DM_LIMIT.max) {
        return;
    }
    let Some(http) = app.http() else { return };
    let content = format!(
        "You were removed from **{}** because you didn't complete verification within {span}. You're welcome to join again and verify.",
        bot_util::text::clamp_text(guild_name, 100)
    );
    let send = async move {
        let ch = user_id.create_dm_channel(http.as_ref()).await?;
        ch.id.send_message(http.as_ref(), CreateMessage::new().content(content).allowed_mentions(serenity::builder::CreateAllowedMentions::new())).await
    };
    match tokio::time::timeout(std::time::Duration::from_millis(3000), send).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => bot_util::log_debug!(format!("captcha kick DM not delivered: {}", bot_core::discord::error_message(&e))),
        Err(_) => bot_util::log_debug!("captcha kick DM not delivered: captcha kick dm timed out"),
    }
}

/// Whether a job payload's `userId` is a snowflake (`/^\d{17,20}$/`).
pub fn valid_user_id(v: Option<&serde_json::Value>) -> Option<String> {
    let s = v?.as_str()?;
    (17..=20).contains(&s.len()).then_some(()).filter(|_| s.bytes().all(|b| b.is_ascii_digit())).map(|_| s.to_string())
}

/// `runTimeoutJob(job, ctx)`: the `captcha.timeout` handler. Idempotent: a second run finds no row or an unknown member.
/// The kick goes through moderation (an infraction and a mod-log case).
pub async fn run_timeout_job(job: Job, ctx: JobCtx) -> anyhow::Result<JobOutcome> {
    let Some(user_id) = valid_user_id(job.payload.get("userId")) else {
        return Err(PermanentJobError::new("captcha.timeout: invalid payload").into());
    };
    let guild_id = ctx.guild_id.clone();
    // Same key as the Verify / answer handlers: a member who passes at the deadline is either verified before this runs
    // or kicked first; never verified mid-kick and removed.
    let key = format!("{guild_id}:{user_id}");
    let app = ctx.app.clone();
    let attempt = ctx.attempt;
    state(&app).user_mutex.run(&key, move || async move { kick_if_still_pending(&app, &guild_id, &user_id, attempt).await }).await?;
    Ok(JobOutcome::Done)
}

async fn kick_if_still_pending(app: &App, guild_id: &str, user_id: &str, attempt: i32) -> anyhow::Result<()> {
    // A database failure fails the run: the scheduler retries.
    let settings = app.settings().get_settings(guild_id).await?;
    // Module off or "never": no kick, row kept.
    if !settings.captcha_enabled || settings.captcha_timeout_minutes <= 0 {
        return Ok(());
    }
    let gid = GuildId::new(parse_snowflake(guild_id).ok_or_else(|| anyhow::anyhow!("captcha.timeout: bad guild id"))?);
    // Misconfigured: never kick.
    let Some(role_id) = cached_role(app, gid, settings.captcha_role_id.as_deref()) else { return Ok(()) };
    // Verified or left meanwhile.
    if find_row(app.db(), guild_id, user_id).await?.is_none() {
        return Ok(());
    }
    let uid = UserId::new(parse_snowflake(user_id).ok_or_else(|| anyhow::anyhow!("captcha.timeout: bad user id"))?);
    let http = app.http().ok_or_else(|| anyhow::anyhow!("captcha.timeout: Discord is not connected"))?;
    let member = match http.get_member(gid, uid).await {
        Ok(m) => m,
        Err(e) => {
            // A REST answer of "unknown member / user" (the scheduler only hands us available guilds).
            if matches!(bot_core::discord::error_code(&e), Some(10007) | Some(10013)) {
                delete_row(app.db(), guild_id, user_id).await?;
                return Ok(());
            }
            return Err(DiscordFailure::of(&e).into());
        }
    };
    if member.roles.contains(&role_id) {
        // Verified while no event reached us.
        delete_row(app.db(), guild_id, user_id).await?;
        return Ok(());
    }
    // Cheap local pre-checks, so nobody is told "you were removed" and then stays. Moderation is authoritative.
    let bot_owner = app.config().bot_owner_id.clone();
    let checks = bot_core::interaction_helpers::cached_guild(app, gid).map(|g| {
        let exempt = is_kick_exempt(&g, &member, Some(&settings), bot_owner.as_deref());
        let bot = bot_user_id(app).unwrap_or(UserId::new(1));
        let view = feat_moderation::guild_ops::MemberView::of(&g, bot, &member);
        let me_can = bot_core::permissions::me(&g, bot)
            .map(|me| bot_core::permissions::js_has(bot_core::permissions::guild_permissions(&g, bot, &me.roles), serenity::all::Permissions::KICK_MEMBERS))
            .unwrap_or(false);
        (exempt, me_can && view.kickable, g.name.clone(), bot)
    });
    let Some((exempt, kickable, guild_name, bot)) = checks else { return Err(anyhow::anyhow!("captcha.timeout: guild not cached")) };
    if exempt {
        return Ok(());
    }
    if !kickable {
        throttled_warn(app, &format!("{guild_id}:kick"), &format!("Captcha timeout: cannot kick {user_id} in {guild_id} (permission or hierarchy)"));
        return Ok(());
    }

    let span = bot_util::parse_duration::format_duration_long(f64::from(settings.captcha_timeout_minutes) * 60_000.0);
    // A scheduler retry must not DM twice.
    if attempt <= 1 {
        dm_before_kick(app, gid, &guild_name, uid, &span).await;
    }
    let moderator = app.discord().map(|d| UserRef::of(&d.cache.current_user())).unwrap_or_else(|| UserRef::id(bot.to_string()));
    let mut input = ModActionInput::new(guild_id, user_id, moderator, "KICK");
    input.target_member = Some(member.clone());
    input.target_user = Some(member.user.clone());
    input.reason = Some(bot_util::text::clamp_text(&format!("Captcha: not verified within {span}"), 500));
    input.source = "captcha".into();
    // Our own DM above; moderation does not DM again.
    input.silent = true;
    input.skip_escalation = true;
    match run_mod_action(app, input).await {
        KickOutcome::Kicked => {
            delete_row(app.db(), guild_id, user_id).await?;
            Ok(())
        }
        KickOutcome::Failed { applied: true, error, .. } => {
            // The member was kicked but the case could not be saved: do not kick again, do not retry.
            throttled_warn(app, &format!("{guild_id}:kickcase"), &format!("Captcha timeout: {user_id} was kicked in {guild_id} but the case was not saved: {error}"));
            delete_row(app.db(), guild_id, user_id).await?;
            Ok(())
        }
        KickOutcome::Failed { code, error, .. } => match code.as_deref() {
            Some("NOT_IN_GUILD") => {
                delete_row(app.db(), guild_id, user_id).await?;
                Ok(())
            }
            Some(c @ ("IMMUNE" | "HIERARCHY" | "NOT_PERMITTED")) => {
                // No retry: the row stays until the member verifies or leaves.
                throttled_warn(app, &format!("{guild_id}:kick"), &format!("Captcha timeout: {user_id} not kicked in {guild_id} ({c}): {error}"));
                Ok(())
            }
            other => Err(anyhow::anyhow!("captcha.timeout kick failed ({}): {}", other.unwrap_or("unknown"), if error.is_empty() { "no result" } else { &error })),
        },
    }
}

// ── Component handlers ────────────────────────────────────────────────────────

/// `handleCaptchaButton(interaction)`: `captcha:start` and `captcha:answer` (module-gated by the plugin loader).
pub async fn handle_captcha_button(ctx: ComponentCtx) -> BotResult {
    let (_, params) = ctx.parsed();
    match params.first().map(String::as_str) {
        // The first response; no I/O before it.
        Some("answer") => show_answer_modal(&ctx).await,
        Some("start") => handle_start(ctx).await?,
        _ => {
            let _ = ctx.reply(CreateInteractionResponseMessage::new().content("This button is no longer active.").ephemeral(true)).await;
        }
    }
    Ok(())
}

/// The answer form (`captcha-modal:answer`).
pub fn answer_modal() -> CreateModal {
    let input = CreateInputText::new(serenity::all::InputTextStyle::Short, "Code from the image", custom_ids::ANSWER_FIELD)
        .min_length(1)
        .max_length(16)
        .required(true)
        .placeholder("Type the 6 characters");
    CreateModal::new(custom_ids::MODAL_ANSWER, "Enter the code").components(vec![CreateActionRow::InputText(input)])
}

async fn show_answer_modal(ctx: &ComponentCtx) {
    if let Err(e) = ctx.show_modal(answer_modal()).await {
        if is_interaction_gone(&e) {
            bot_util::log_debug!(format!("captcha modal: interaction gone ({})", e.code().unwrap_or(0)));
        } else {
            bot_util::log_warn!("captcha modal failed", e.to_string());
        }
    }
}

/// `makeRespond(interaction)`: `editReply(payload)`, logging (never failing).
pub async fn respond(r: &Responder, reply: Reply) {
    if let Err(e) = r.edit_reply(reply.to_edit()).await {
        if is_interaction_gone(&e) {
            bot_util::log_debug!(format!("captcha respond: interaction gone ({})", e.code().unwrap_or(0)));
        } else {
            bot_util::log_warn!("captcha respond failed", e.to_string());
        }
    }
}

/// `replyEphemeral(interaction, payload)`: replies without failing.
pub async fn reply_ephemeral(r: &Responder, reply: Reply) {
    if let Err(e) = r.reply(reply.to_ephemeral_reply()).await {
        if is_interaction_gone(&e) {
            bot_util::log_debug!(format!("captcha reply: interaction gone ({})", e.code().unwrap_or(0)));
        } else {
            bot_util::log_warn!("captcha reply failed", e.to_string());
        }
    }
}

fn cached_guild_exists(app: &App, guild_id: Option<GuildId>) -> Option<GuildId> {
    let g = guild_id?;
    with_cached_guild(app, g, |_| ()).map(|_| g)
}

/// `buildNeedsRoleReply({ guild, member, settings, roleIds })`: the "role required" reply, with the form button when the
/// fallback form is usable. Never fails.
async fn build_needs_role_reply(app: &App, guild_id: GuildId, member_roles: &[String], settings: &GuildSettings, role_ids: Vec<String>) -> Reply {
    let linked: Vec<String> = with_cached_guild(app, guild_id, |g| {
        role_ids
            .iter()
            .filter(|id| parse_snowflake(id).and_then(|r| g.roles.get(&RoleId::new(r))).is_some_and(|r| r.tags.guild_connections))
            .cloned()
            .collect()
    })
    .unwrap_or_default();
    let fb = inspect_fallback_form(app, guild_id, settings, member_roles).await;
    // The Applications panel's own Apply button.
    let form_button = match (&fb.form, &fb.problem) {
        (Some(form), None) => Some(feat_welcome::application_service::build_apply_button(&form.id, Some(FORM_BUTTON_LABEL))),
        _ => None,
    };
    msg::needs_role(NeedsRole { role_ids, linked_role_ids: linked, form_button })
}

/// What [`preflight`] decided.
enum Preflight {
    /// Go on with this role.
    Role(RoleId),
    /// Already answered.
    Answered,
}

/// `preflight({ guild, member, settings, respond })`: shared by Verify and the answer form.
async fn preflight(app: &App, guild_id: GuildId, member: &Member, settings: &GuildSettings, r: &Responder) -> anyhow::Result<Preflight> {
    let role_id = settings.captcha_role_id.as_deref().and_then(parse_snowflake).map(RoleId::new);
    let bot = bot_user_id(app);
    let facts = with_cached_guild(app, guild_id, |g| {
        let role = role_id.and_then(|r| g.roles.get(&r));
        let chk = match bot {
            Some(b) => check_verified_role(g, role, b, Some(settings), None),
            None => Check::no("Role not found."),
        };
        let member_roles: Vec<String> = member_role_ids(g, member).iter().map(ToString::to_string).collect();
        let guild_roles: Vec<String> = g.roles.keys().map(ToString::to_string).collect();
        (chk, member_roles, guild_roles)
    });
    let (chk, member_roles, guild_roles) = facts.unwrap_or_else(|| (Check::no("Role not found."), Vec::new(), Vec::new()));
    let (true, Some(role_id)) = (chk.ok, role_id) else {
        throttled_warn(app, &format!("{guild_id}:role"), &format!("Captcha role unusable in {guild_id}: {}", chk.reason.unwrap_or_default()));
        respond(r, msg::not_set_up()).await;
        return Ok(Preflight::Answered);
    };
    if member.roles.contains(&role_id) {
        // Finishes a leftover pending row; no role add.
        complete_verification(app, member, role_id).await?;
        respond(r, msg::already()).await;
        return Ok(Preflight::Answered);
    }
    // The required-role gate (/captcha require): before any challenge is drawn and before the role is given.
    let rid = role_id.to_string();
    if let Some(needs) = missing_requirement(&settings.captcha_required_role_ids, &member_roles, &guild_roles, Some(&rid)) {
        let reply = build_needs_role_reply(app, guild_id, &member_roles, settings, needs).await;
        respond(r, reply).await;
        return Ok(Preflight::Answered);
    }
    Ok(Preflight::Role(role_id))
}

fn guild_name(app: &App, guild_id: GuildId) -> String {
    with_cached_guild(app, guild_id, |g| g.name.clone()).unwrap_or_default()
}

fn completion_reply(app: &App, guild_id: GuildId, c: Completion) -> Reply {
    match c {
        Completion::Done { .. } => msg::verified(&guild_name(app, guild_id)),
        Completion::Failed(r) => r,
    }
}

async fn handle_start(ctx: ComponentCtx) -> BotResult {
    let Some(guild_id) = cached_guild_exists(&ctx.app, ctx.guild_id()) else {
        let _ = ctx.reply(CreateInteractionResponseMessage::new().content("This only works inside the server.").ephemeral(true)).await;
        return Ok(());
    };
    let from_ephemeral = ctx.interaction.message.flags.is_some_and(|f| f.contains(MessageFlags::EPHEMERAL));
    // "New image" on a challenge: deferUpdate; the public panel: an ephemeral deferReply.
    let deferred = if from_ephemeral { ctx.defer_update().await } else { ctx.defer_reply(true).await };
    if let Err(e) = deferred {
        if is_interaction_gone(&e) {
            return Ok(());
        }
        return Err(anyhow::anyhow!(e.to_string()));
    }
    let (app, r) = (ctx.app.clone(), ctx.responder.clone());
    let Some(member) = ctx.member().cloned() else { return Ok(()) };
    let user_id = ctx.user().id.to_string();
    let g = guild_id.to_string();
    let key = format!("{g}:{user_id}");
    let (app2, r2) = (app.clone(), r.clone());
    let result: anyhow::Result<()> = state(&app)
        .user_mutex
        .run(&key, move || async move {
            let (app, r) = (app2, r2);
            let Some(settings) = app.settings().get_settings_safe(&g).await else {
                respond(&r, msg::temp_unavailable()).await;
                return Ok(());
            };
            if !settings.captcha_enabled {
                respond(&r, Reply::text_only(msg::module_off())).await;
                return Ok(());
            }
            let Preflight::Role(role_id) = preflight(&app, guild_id, &member, &settings, &r).await? else { return Ok(()) };
            if member.pending {
                respond(&r, msg::screening_first()).await;
                return Ok(());
            }
            if settings.captcha_mode == bot_db::CaptchaMode::Button || !is_image_captcha_available() {
                if settings.captcha_mode != bot_db::CaptchaMode::Button {
                    throttled_warn(&app, &format!("{g}:font"), "Captcha font missing; using button mode");
                }
                let c = complete_verification(&app, &member, role_id).await?;
                respond(&r, completion_reply(&app, guild_id, c)).await;
                return Ok(());
            }
            if let Some(limited) = check_image_limits(&app, &g, &member.user.id.to_string()).await {
                respond(&r, limited).await;
                return Ok(());
            }
            match issue_challenge(&app, &g, &member.user.id.to_string(), Some(&settings), "").await {
                Ok(reply) => respond(&r, reply).await,
                Err(e) => {
                    bot_util::log_warn!("Captcha image failed", bot_core::error::log_arg(&e));
                    respond(&r, msg::image_failed()).await;
                }
            }
            Ok(())
        })
        .await;
    if let Err(e) = result {
        bot_util::log_error!(format!("Captcha start failed in {guild_id}"), bot_core::error::log_arg(&e));
        respond(&r, msg::temp_unavailable()).await;
    }
    Ok(())
}

/// `handleCaptchaModal(interaction)`: `captcha-modal:answer` (checks the module itself) and `captcha-modal:panel` (the
/// admin form).
pub async fn handle_captcha_modal(ctx: ModalCtx) -> BotResult {
    let (_, params) = ctx.parsed();
    match params.first().map(String::as_str) {
        Some("answer") => handle_answer_submit(ctx).await,
        Some("panel") => handle_panel_modal(ctx).await,
        _ => {
            let _ = ctx.reply(CreateInteractionResponseMessage::new().content("This form has expired. Please run the command again.").ephemeral(true)).await;
            Ok(())
        }
    }
}

/// The text of a wrong try.
pub fn wrong_try_text(left: i32) -> String {
    format!("❌ That code is not right. **{left}** {} left.", if left == 1 { "try" } else { "tries" })
}

/// The line above a fresh image after an expired, exhausted or unknown code.
pub fn fresh_image_prefix(v: Verdict) -> &'static str {
    match v {
        Verdict::Exhausted => "❌ Too many wrong tries. Here's a new image.",
        Verdict::Expired => "⌛ That code expired. Here's a new image.",
        _ => "That code is no longer valid. Here's a new image.",
    }
}

async fn handle_answer_submit(ctx: ModalCtx) -> BotResult {
    let Some(guild_id) = cached_guild_exists(&ctx.app, ctx.guild_id()) else { return Ok(()) };
    let g = guild_id.to_string();
    // The modal prefix is not module-gated by the loader and this is the first response: bounded settings read.
    let Some(settings) = settings_for_ack(&ctx.app, &g, ctx.ack_budget_ms()).await else {
        reply_ephemeral(&ctx.responder, msg::temp_unavailable()).await;
        return Ok(());
    };
    if !settings.captcha_enabled {
        reply_ephemeral(&ctx.responder, Reply::text(msg::module_off())).await;
        return Ok(());
    }
    let deferred = if ctx.interaction.message.is_some() { ctx.defer_update().await } else { ctx.defer_reply(true).await };
    if let Err(e) = deferred {
        if is_interaction_gone(&e) {
            return Ok(());
        }
        return Err(anyhow::anyhow!(e.to_string()));
    }
    let (app, r) = (ctx.app.clone(), ctx.responder.clone());
    let Some(member) = ctx.member().cloned() else { return Ok(()) };
    let user_id = ctx.user().id.to_string();
    let input = ctx.value(custom_ids::ANSWER_FIELD).unwrap_or_default();
    let key = format!("{g}:{user_id}");
    let (app2, r2) = (app.clone(), r.clone());
    let result: anyhow::Result<()> = state(&app)
        .user_mutex
        .run(&key, move || async move {
            let (app, r) = (app2, r2);
            let Preflight::Role(role_id) = preflight(&app, guild_id, &member, &settings, &r).await? else { return Ok(()) };
            if settings.captcha_mode == bot_db::CaptchaMode::Button || !is_image_captcha_available() {
                respond(&r, Reply::text_only("Verification changed. Press **Verify** on the panel to continue.")).await;
                return Ok(());
            }
            let row = find_row(app.db(), &g, &user_id).await?;
            let challenge = row.as_ref().map(Challenge::from);
            let ev = evaluate_answer(challenge.as_ref(), &g, &user_id, Some(&input), app.clock().now_ms());
            match ev.result {
                Verdict::Ok => {
                    // Screening state changed mid-challenge.
                    if member.pending {
                        respond(&r, msg::screening_first()).await;
                        return Ok(());
                    }
                    let c = complete_verification(&app, &member, role_id).await?;
                    respond(&r, completion_reply(&app, guild_id, c)).await;
                }
                Verdict::Wrong => {
                    if let Some(hash) = challenge.as_ref().and_then(|c| c.answer_hash.clone()) {
                        count_wrong_try(app.db(), &g, &user_id, &hash).await?;
                    }
                    // Only the text changes.
                    respond(&r, Reply::text(wrong_try_text(ev.attempts_left.unwrap_or(0)))).await;
                }
                v => {
                    let prefix = fresh_image_prefix(v);
                    if let Some(mut limited) = check_image_limits(&app, &g, &user_id).await {
                        clear_challenge(app.db(), &g, &user_id).await?;
                        limited.content = Some(String::new());
                        limited.components = Some(Vec::new());
                        limited.clear_attachments = true;
                        respond(&r, limited).await;
                        return Ok(());
                    }
                    match issue_challenge(&app, &g, &user_id, Some(&settings), prefix).await {
                        Ok(reply) => respond(&r, reply).await,
                        Err(e) => {
                            bot_util::log_warn!("Captcha image failed", bot_core::error::log_arg(&e));
                            respond(&r, msg::image_failed()).await;
                        }
                    }
                }
            }
            Ok(())
        })
        .await;
    if let Err(e) = result {
        bot_util::log_error!(format!("Captcha answer failed in {guild_id}"), bot_core::error::log_arg(&e));
        respond(&r, msg::temp_unavailable()).await;
    }
    Ok(())
}

/// The admin form from `/captcha message`: admin-only; works while the module is off.
async fn handle_panel_modal(ctx: ModalCtx) -> BotResult {
    let Some(guild_id) = cached_guild_exists(&ctx.app, ctx.guild_id()) else { return Ok(()) };
    let g = guild_id.to_string();
    let Some(settings) = settings_for_ack(&ctx.app, &g, ctx.ack_budget_ms()).await else {
        reply_ephemeral(&ctx.responder, msg::temp_unavailable()).await;
        return Ok(());
    };
    let access = ctx.member_access();
    if resolve_permission_level(access.as_ref(), Some(&settings), ctx.app.config().bot_owner_id.as_deref()) < PermissionLevel::Admin {
        let embed = error_embed("Insufficient Permissions", Some("Only server admins can edit the verification panel."), None);
        reply_ephemeral(&ctx.responder, Reply { embeds: Some(vec![embed]), ..Reply::default() }).await;
        return Ok(());
    }
    // Public (admin convention).
    if let Err(e) = ctx.defer_reply(false).await {
        if is_interaction_gone(&e) {
            return Ok(());
        }
        return Err(anyhow::anyhow!(e.to_string()));
    }
    let r = ctx.responder.clone();
    let embed_reply = |e| Reply { embeds: Some(vec![e]), ..Reply::default() };
    let (template, errors) = bot_core::custom_embed::parse_template_modal(|id| ctx.value(id), &panel_template(Some(&settings)), true);
    if !errors.is_empty() {
        respond(&r, embed_reply(error_embed("Not Saved", Some(&bot_util::text::clamp_text(&errors.join("\n"), 4000)), None))).await;
        return Ok(());
    }
    let after = ctx
        .app
        .settings()
        .update_settings_with(&g, move |cur| {
            let mut t = bot_core::custom_embed::normalize_template(&template);
            let btn = parse_verify_button(cur.captcha_template.as_ref().and_then(|t| t.get("verifyButton")));
            if let Some(o) = t.as_object_mut() {
                o.insert("verifyButton".into(), btn.to_json());
            }
            Ok(SettingsPatch::new().set("captchaTemplate", t))
        })
        .await?;
    let text = match refresh_panel(&ctx.app, guild_id, &after).await {
        Ok(()) => "The verification panel was updated.".to_string(),
        Err(reason) => format!("Saved. {reason}"),
    };
    respond(&r, embed_reply(success_embed("Panel Updated", Some(&text), None))).await;
    Ok(())
}
