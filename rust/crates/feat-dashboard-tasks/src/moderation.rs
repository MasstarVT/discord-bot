//! Port of `src/services/dashboardTasks/moderation.js`: moderation actions the web dashboard asks the bot to do (its
//! `cases` list). Each task does what the matching slash command does, with the dashboard user as the moderator:
//!   `mod.action`          /warn /timeout /unmute /kick /softban /ban /unban -> execute_mod_action / execute_unmute /
//!                         execute_unban (source 'dashboard': roles, immunity and self-moderation are checked like a
//!                         command; DM per modDmTargets, case log, escalation, scheduled unban / expiry)
//!   `mod.editCase`        /case reason | proof (and its case-log note)
//!   `mod.clearWarnings`   /case clear-history (administrators): pardons the active warnings and records a NOTE case
//! The dashboard shows its own message and posts nothing in a chat channel. Commands that are refused while the
//! Moderation module is off are refused here too.

use bot_config::{ModuleKey, PermissionLevel};
use bot_core::App;
use bot_core::embed_builder::neutral_embed;
use bot_core::error::log_arg;
use bot_core::settings::{Settings, is_module_enabled};
use bot_db::query::UpdateBuilder;
use bot_db::{INFRACTION_INFO, InfractionType};
use bot_util::text::clamp_text;
use feat_moderation::guild_ops::UserRef;
use feat_moderation::moderation_service::{
    self as mod_service, CreateOpts, DELETE_MESSAGES_MAX_S, EscalationOutcome, ModActionInput, REASON_MAX, msg, tier_action_label,
};
use serde_json::{Value, json};
use serenity::all::Member;

use crate::index::{Actor, DashboardTask, TaskCtx, TaskOutcome};
use crate::shared::{TaskError, task_error};
use crate::util;

/// What `mod.action` accepts (the dashboard's case form).
pub const DASHBOARD_MOD_ACTIONS: [&str; 7] = ["WARN", "TIMEOUT", "UNMUTE", "KICK", "SOFTBAN", "BAN", "UNBAN"];
/// The action commands' `reason` option is at most 512 characters; /case reason allows REASON_MAX.
pub const COMMAND_REASON_MAX: usize = 512;
const PROOF_ACTIONS: [&str; 5] = ["WARN", "TIMEOUT", "KICK", "SOFTBAN", "BAN"];
const MAX_CASE_ID: f64 = 2_147_483_647.0;
const DAY_S: i64 = 86_400;

/// Optional text: null/undefined/blank -> None; else trimmed, at most `max` characters.
fn optional_text(value: Option<&Value>, max: usize, what: &str) -> Result<Option<String>, TaskError> {
    let Some(value) = value.filter(|v| !v.is_null()) else { return Ok(None) };
    let Some(s) = value.as_str() else { return Err(TaskError::new(format!("{what} must be text."))) };
    let text = bot_util::js::trim(s);
    if text.is_empty() {
        return Ok(None);
    }
    if util::len16(text) > max {
        return Err(TaskError::new(format!("{what} can be at most {max} characters.")));
    }
    Ok(Some(text.to_string()))
}

fn user_id_of(payload: &Value) -> Result<String, TaskError> {
    let id = util::js_string_or_empty(payload.get("userId"));
    if !util::is_snowflake(&id) {
        return Err(TaskError::new("Pick a member, or paste a user ID."));
    }
    Ok(id)
}

/// A re-validated `mod.action` payload.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionPayload {
    pub action: String,
    pub user_id: String,
    pub reason: Option<String>,
    pub duration_ms: Option<f64>,
    pub delete_message_seconds: Option<i64>,
    pub proof_url: Option<String>,
}

impl ActionPayload {
    /// The JS shape `{ action, userId, reason, durationMs, deleteMessageSeconds, proofUrl }`.
    pub fn to_json(&self) -> Value {
        json!({
            "action": self.action,
            "userId": self.user_id,
            "reason": self.reason,
            "durationMs": self.duration_ms,
            "deleteMessageSeconds": self.delete_message_seconds,
            "proofUrl": self.proof_url,
        })
    }
}

/// `readActionPayload(payload)`: re-validates a `mod.action` payload (the dashboard already did; never trust it).
pub fn read_action_payload(payload: &Value) -> Result<ActionPayload, TaskError> {
    if !util::is_object(payload) {
        return Err(TaskError::new("The request was malformed."));
    }
    let Some(action) = payload.get("action").and_then(Value::as_str).filter(|a| DASHBOARD_MOD_ACTIONS.contains(a)) else {
        return Err(TaskError::new("Unknown moderation action."));
    };
    let user_id = user_id_of(payload)?;
    let reason = optional_text(payload.get("reason"), COMMAND_REASON_MAX, "The reason")?;
    if action == "WARN" && reason.is_none() {
        return Err(TaskError::new("A warning needs a reason."));
    }

    let mut duration_ms = None;
    if action == "TIMEOUT" || action == "BAN" {
        let d = payload.get("durationMs");
        if !util::is_nullish(d) {
            match util::js_safe_integer(d) {
                Some(n) if n > 0.0 => duration_ms = Some(n),
                _ => return Err(TaskError::new("Invalid duration.")),
            }
        }
        if action == "TIMEOUT" && duration_ms.is_none() {
            return Err(TaskError::new("Timeouts need a duration."));
        }
    }

    let mut delete_message_seconds = None;
    if action == "BAN" || action == "SOFTBAN" {
        let raw = payload.get("deleteMessageSeconds").filter(|v| !v.is_null());
        let fallback = json!(if action == "BAN" { 0 } else { DELETE_MESSAGES_MAX_S });
        let min = if action == "SOFTBAN" { DAY_S } else { 0 };
        match util::js_integer(Some(raw.unwrap_or(&fallback))) {
            Some(s) if s >= min as f64 && s <= DELETE_MESSAGES_MAX_S as f64 => delete_message_seconds = Some(s as i64),
            _ => return Err(TaskError::new(format!("Message deletion must be between {} and 7 days.", min / DAY_S))),
        }
    }

    let mut proof_url = None;
    let raw_proof = payload.get("proofUrl");
    if PROOF_ACTIONS.contains(&action) && !util::is_nullish(raw_proof) {
        let url = raw_proof.and_then(Value::as_str).map(bot_util::js::trim).unwrap_or("").to_string();
        if !bot_core::custom_embed::is_http_url(&url) || util::len16(&url) > REASON_MAX {
            return Err(TaskError::new(msg::BAD_PROOF));
        }
        proof_url = Some(url);
    }
    Ok(ActionPayload { action: action.into(), user_id, reason, duration_ms, delete_message_seconds, proof_url })
}

/// The checks the command router makes before a moderation command runs, for the dashboard user: still a member, the
/// module on, and their permission level.
async fn moderator_checks(app: &App, guild_id: &str, actor: &Actor, level: PermissionLevel) -> anyhow::Result<(Settings, Member)> {
    let Some(member) = actor.member.clone() else {
        return Err(task_error(if actor.is_bot_owner {
            "Join this server to moderate it: the bot compares your roles with the member's, as it does for the slash commands."
        } else {
            "You are no longer a member of this server."
        }));
    };
    let Some(settings) = app.settings().get_settings_safe(guild_id).await else {
        return Err(task_error("The bot couldn't load this server's settings. Try again in a moment."));
    };
    if !is_module_enabled(Some(&settings), Some(ModuleKey::Moderation)) {
        return Err(task_error("The Moderation module is off. Turn it on to do that."));
    }
    if util::member_level(app, guild_id, &member, Some(&settings)) < level {
        return Err(task_error(if level >= PermissionLevel::Admin {
            "Only administrators (Manage Server) can do that."
        } else {
            "You need moderator rights in this server (Kick, Ban or Timeout Members, or a moderator role) to do that."
        }));
    }
    Ok((settings, member))
}

/// A user's name for a message: their server nickname, global name or username; the id when unknown.
async fn name_of(app: &App, guild_id: &str, user_id: &str) -> String {
    if let Some(m) = util::cached_member(app, guild_id, user_id) {
        let n = util::member_display_name(&m);
        if !n.is_empty() {
            return n;
        }
    }
    util::fetch_user(app, user_id).await.map(|u| util::user_name(&u)).filter(|n| !n.is_empty()).unwrap_or_else(|| user_id.to_string())
}

/// What `actionMessage` reads of a service result (`executeModAction`, `executeUnban` or `executeUnmute`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActionResult {
    pub case_id: i32,
    /// `infraction.type`.
    pub infraction_type: String,
    /// `res.action` (absent for unban and unmute).
    pub action: Option<String>,
    /// `res.dm`.
    pub dm: Option<String>,
    pub warnings: Vec<String>,
    pub note: Option<String>,
    /// `res.removed` (unmute).
    pub removed: Option<Vec<String>>,
    /// `res.cleared` (unban, unmute).
    pub cleared: Option<u64>,
    pub escalation: Option<EscalationOutcome>,
}

fn tier_label(action: &str) -> String {
    tier_action_label(action).map(str::to_string).unwrap_or_else(|| action.to_string())
}

fn opt_num(n: Option<i32>) -> String {
    n.map(|n| n.to_string()).unwrap_or_else(|| "undefined".into())
}

/// `actionMessage(input, res, name, { activeWarns })`: the confirmation of a successful action, like the command's
/// reply (title, case, duration, warnings).
pub fn action_message(input: &ActionPayload, res: &ActionResult, name: &str, active_warns: Option<i64>) -> String {
    let case_ref = format!("case #{}", res.case_id);
    let duration = || bot_util::parse_duration::format_duration_long(input.duration_ms.unwrap_or(0.0));
    let days = input.delete_message_seconds.unwrap_or(0) as f64 / DAY_S as f64;
    let days = bot_util::js::number_to_string(days);
    let mut extra: Vec<String> = Vec::new();
    let main = match input.action.as_str() {
        "WARN" => {
            if let Some(n) = active_warns {
                extra.push(format!("Active warnings: {n}."));
            }
            if let Some(e) = res.escalation.as_ref().filter(|e| e.ok) {
                extra.push(format!("Escalation T{}: {} applied (case #{}).", e.tier_id, tier_label(&e.action), opt_num(e.case_id)));
            }
            format!("Warned {name} ({case_ref}).")
        }
        "TIMEOUT" => {
            if res.action.as_deref() == Some("MUTE") {
                format!("Muted {name} for {} ({case_ref}).", duration())
            } else {
                format!("Timed out {name} for {} ({case_ref}).", duration())
            }
        }
        "UNMUTE" => {
            let removed = res.removed.as_ref().map(|r| r.join(" and ")).filter(|s| !s.is_empty()).unwrap_or_else(|| "nothing (records only)".into());
            format!("Unmuted {name} ({case_ref}). Removed: {removed}.")
        }
        "KICK" => format!("Kicked {name} ({case_ref})."),
        "SOFTBAN" => {
            if res.infraction_type == "BAN" {
                format!("Banned {name}, but the softban's automatic unban failed ({case_ref}).")
            } else {
                format!("Softbanned {name} ({case_ref}). Deleted {days} day(s) of messages.")
            }
        }
        "BAN" => {
            if input.delete_message_seconds.unwrap_or(0) > 0 {
                extra.push(format!("Deleted {days} day(s) of messages."));
            }
            if res.action.as_deref() == Some("TEMPBAN") {
                format!("Banned {name} for {} ({case_ref}).", duration())
            } else {
                format!("Banned {name} ({case_ref}).")
            }
        }
        "UNBAN" => {
            if res.cleared.unwrap_or(0) > 1 {
                extra.push(format!("Ban records closed: {}.", res.cleared.unwrap_or(0)));
            }
            format!("Unbanned {name} ({case_ref}).")
        }
        _ => format!("Done ({case_ref})."),
    };
    let mut warnings: Vec<String> = Vec::new();
    if res.dm.as_deref() == Some("failed") {
        warnings.push("I couldn't DM the user.".into());
    }
    warnings.extend(res.warnings.iter().cloned());
    if let Some(note) = res.note.as_ref().filter(|n| !n.is_empty()) {
        warnings.push(note.clone());
    }
    if let Some(e) = res.escalation.as_ref().filter(|e| !e.ok) {
        warnings.push(format!(
            "Escalation T{} ({}) could not be applied: {}",
            e.tier_id,
            tier_label(&e.action),
            clamp_text(e.error.as_deref().unwrap_or("unknown error"), 200)
        ));
    }
    std::iter::once(main).chain(extra).chain(warnings).collect::<Vec<_>>().join(" ")
}

/// `mod.action`: `{ action, userId, reason, durationMs, deleteMessageSeconds, proofUrl }` -> `{ caseId, action, userId }`.
pub async fn mod_action(ctx: TaskCtx) -> anyhow::Result<TaskOutcome> {
    let input = read_action_payload(&ctx.payload)?;
    let app = &ctx.app;
    let guild_id = ctx.guild_id.as_str();
    let (_settings, member) = moderator_checks(app, guild_id, &ctx.actor, PermissionLevel::Moderator).await?;
    let moderator = UserRef::of(&member.user);
    let name = name_of(app, guild_id, &input.user_id).await;
    let reason = input.reason.as_deref();
    let res: ActionResult = match input.action.as_str() {
        "UNBAN" => match mod_service::execute_unban(app, guild_id, &input.user_id, &moderator, reason, "dashboard").await {
            Ok(r) => ActionResult {
                case_id: r.infraction.case_id,
                infraction_type: r.infraction.r#type.as_str().into(),
                cleared: Some(r.cleared),
                ..Default::default()
            },
            Err(e) => return Err(task_error(if e.error.is_empty() { msg::INTERNAL.to_string() } else { e.error })),
        },
        "UNMUTE" => {
            match mod_service::execute_unmute(app, guild_id, &UserRef::id(&input.user_id), None, &moderator, reason, "dashboard").await {
                Ok(r) => ActionResult {
                    case_id: r.infraction.case_id,
                    infraction_type: r.infraction.r#type.as_str().into(),
                    removed: Some(r.removed),
                    cleared: Some(r.cleared),
                    note: r.note,
                    ..Default::default()
                },
                Err(e) => return Err(task_error(if e.error.is_empty() { msg::INTERNAL.to_string() } else { e.error })),
            }
        }
        other => {
            let action = if other == "BAN" && input.duration_ms.is_some() { "TEMPBAN" } else { other };
            let mut mi = ModActionInput::new(guild_id, &input.user_id, moderator.clone(), action);
            mi.reason = input.reason.clone();
            mi.duration_ms = input.duration_ms;
            mi.delete_message_seconds = input.delete_message_seconds;
            mi.proof_url = input.proof_url.clone();
            mi.source = "dashboard".into();
            match mod_service::execute_mod_action(app, mi).await {
                Ok(r) => ActionResult {
                    case_id: r.infraction.case_id,
                    infraction_type: r.infraction.r#type.as_str().into(),
                    action: Some(r.action),
                    dm: Some(r.dm.as_str().into()),
                    warnings: r.warnings,
                    escalation: r.escalation,
                    ..Default::default()
                },
                Err(e) => return Err(task_error(if e.error.is_empty() { msg::INTERNAL.to_string() } else { e.error })),
            }
        }
    };

    let mut active_warns = None;
    if input.action == "WARN" {
        // The count follows the action, so it includes this warning (and ignores pardoned ones), as /warn shows it.
        match mod_service::count_active_warns(app, guild_id, &input.user_id).await {
            Ok(n) => active_warns = Some(n),
            Err(err) => bot_util::log_warn!(format!("Dashboard mod.action: could not count active warnings: {err}")),
        }
    }
    let data = json!({
        "caseId": res.case_id,
        "action": res.action.clone().unwrap_or_else(|| input.action.clone()),
        "userId": input.user_id,
    });
    Ok(TaskOutcome::new(action_message(&input, &res, &name, active_warns), data))
}

/// A re-validated `mod.editCase` payload: `(caseId, reason, proofUrl)`.
pub fn read_edit_payload(payload: &Value) -> Result<(i32, Option<String>, Option<String>), TaskError> {
    if !util::is_object(payload) {
        return Err(TaskError::new("The request was malformed."));
    }
    let case_id = match util::js_integer(payload.get("caseId")) {
        Some(n) if (1.0..=MAX_CASE_ID).contains(&n) => n as i32,
        _ => return Err(TaskError::new("That case does not exist.")),
    };
    let mut reason = None;
    if let Some(r) = payload.get("reason") {
        let text = clamp_text(r.as_str().map(bot_util::js::trim).unwrap_or(""), REASON_MAX);
        if text.is_empty() {
            return Err(TaskError::new("The reason can't be empty."));
        }
        reason = Some(text);
    }
    let mut proof = None;
    if let Some(p) = payload.get("proofUrl") {
        let url = p.as_str().map(bot_util::js::trim).unwrap_or("").to_string();
        if !bot_core::custom_embed::is_http_url(&url) || util::len16(&url) > REASON_MAX {
            return Err(TaskError::new(msg::BAD_PROOF));
        }
        proof = Some(url);
    }
    if reason.is_none() && proof.is_none() {
        return Err(TaskError::new("Nothing to change."));
    }
    Ok((case_id, reason, proof))
}

/// The case-log notes of an edit (one per changed field).
pub fn edit_notes(moderator_id: &str, reason: Option<&str>, proof: Option<&str>) -> Vec<String> {
    let quote = |t: &str| t.replace('\n', "\n> ");
    let mut notes = Vec::new();
    if let Some(r) = reason.filter(|r| !r.is_empty()) {
        notes.push(format!("Reason changed by <@{moderator_id}> on the dashboard:\n> {}", quote(r)));
    }
    if let Some(p) = proof.filter(|p| !p.is_empty()) {
        notes.push(format!("Proof link changed by <@{moderator_id}> on the dashboard:\n{p}"));
    }
    notes
}

/// `mod.editCase`: `{ caseId, reason?, proofUrl? }` -> `{ caseId }`, with a case-log note per change (/case).
pub async fn edit_case(ctx: TaskCtx) -> anyhow::Result<TaskOutcome> {
    let (case_id, reason, proof) = read_edit_payload(&ctx.payload)?;
    let app = &ctx.app;
    let guild_id = ctx.guild_id.as_str();
    let (_settings, member) = moderator_checks(app, guild_id, &ctx.actor, PermissionLevel::Moderator).await?;

    let mut q = UpdateBuilder::new(&INFRACTION_INFO);
    if let Some(r) = &reason {
        q = q.set("reason", json!(r))?;
    }
    if let Some(p) = &proof {
        q = q.set("proofUrl", json!(p))?;
    }
    let n = q.filter("guildId", json!(guild_id))?.filter("caseId", json!(case_id))?.without_returning().build()?.execute(app.db()).await?;
    if n == 0 {
        return Err(task_error(format!("Case #{case_id} doesn't exist in this server.")));
    }
    if let Some(g) = util::gid(guild_id) {
        for text in edit_notes(&member.user.id.to_string(), reason.as_deref(), proof.as_deref()) {
            let title = format!("📝 Case #{case_id} edited");
            mod_service::post_mod_log(app, g, neutral_embed(Some(&title), Some(&clamp_text(&text, 4000)), None)).await;
        }
    }
    Ok(TaskOutcome::new(format!("Case #{case_id} updated."), json!({ "caseId": case_id })))
}

/// The NOTE reason of a pardon.
pub fn pardon_reason(count: u64, reason: Option<&str>) -> String {
    clamp_text(&format!("[Pardon] Cleared {count} warning(s): {}", reason.filter(|r| !r.is_empty()).unwrap_or("No reason provided")), REASON_MAX)
}

/// The message of a pardon.
pub fn pardon_message(count: u64, name: &str, note_case: Option<i32>) -> String {
    match note_case {
        Some(c) => format!("Cleared {count} warning(s) for {name} (case #{c})."),
        None => format!("Cleared {count} warning(s) for {name}, but the pardon could not be saved as a case."),
    }
}

/// `mod.clearWarnings`: `{ userId, reason }` -> `{ cleared, caseId }` (/case clear-history, administrators only).
pub async fn clear_warnings(ctx: TaskCtx) -> anyhow::Result<TaskOutcome> {
    let payload = &ctx.payload;
    if !util::is_object(payload) {
        return Err(task_error("The request was malformed."));
    }
    let user_id = user_id_of(payload)?;
    let reason = optional_text(payload.get("reason"), COMMAND_REASON_MAX, "The reason")?;
    let app = &ctx.app;
    let guild_id = ctx.guild_id.as_str();
    let (_settings, member) = moderator_checks(app, guild_id, &ctx.actor, PermissionLevel::Admin).await?;
    let name = name_of(app, guild_id, &user_id).await;

    if mod_service::count_active_warns(app, guild_id, &user_id).await? == 0 {
        return Err(task_error(format!("{name} has no active warnings.")));
    }
    let count = UpdateBuilder::new(&INFRACTION_INFO)
        .set("active", json!(false))?
        .filter("guildId", json!(guild_id))?
        .filter("targetUserId", json!(user_id))?
        .filter("type", json!("WARN"))?
        .filter("active", json!(true))?
        .without_returning()
        .build()?
        .execute(app.db())
        .await?;
    let note_reason = pardon_reason(count, reason.as_deref());

    // The pardon is the update above; the NOTE only records it as a case (and posts the case log).
    let mut note = None;
    if let Some(g) = util::gid(guild_id) {
        let target = util::fetch_user(app, &user_id).await.map(|u| UserRef::of(&u)).unwrap_or_else(|| UserRef::id(&user_id));
        let opts = CreateOpts { reason: Some(note_reason), silent: true, skip_escalation: true, source: Some("dashboard".into()), ..Default::default() };
        match mod_service::create_infraction(app, g, &target, &UserRef::of(&member.user), InfractionType::Note, opts).await {
            Ok(inf) => note = Some(inf.case_id),
            Err(err) => bot_util::log_error!(format!("Dashboard mod.clearWarnings: could not record the pardon note in {guild_id}"), log_arg(&err)),
        }
    }
    Ok(TaskOutcome::new(pardon_message(count, &name, note), json!({ "cleared": count, "caseId": note, "userId": user_id })))
}

/// `createModerationTasks()`.
pub fn tasks() -> Vec<(&'static str, DashboardTask)> {
    vec![
        ("mod.action", DashboardTask::new(mod_action)),
        ("mod.editCase", DashboardTask::new(edit_case)),
        ("mod.clearWarnings", DashboardTask::new(clear_warnings)),
    ]
}
