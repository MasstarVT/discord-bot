//! Port of `src/services/dashboardTasks/polls.js`: the `dashboard.task` kinds of the Polls module, done with /poll's own
//! service functions (feat_giveaways_polls::poll_service), so the message with its vote buttons, the end job, the
//! results post and the cap behave as when a moderator uses the command.
//!   `poll.start`   /poll create (the form's submit)  payload: question, options, channelId, maxChoices, duration, roles, hideResults
//!   `poll.end`     /poll end                          payload: `{ pollId }`
//!   `poll.delete`  (dashboard only)                   payload: `{ pollId }` ends it silently, removes its message and the record
//! Every task re-checks what the router and the command check: the module is on, the dashboard user is still in the
//! server and is at least a moderator, then the command's and the form's own rules.

use bot_config::{ModuleKey, PermissionLevel};
use bot_core::App;
use bot_core::settings::is_module_enabled;
use bot_db::query::{SelectBuilder, delete_where};
use bot_db::{POLL_INFO, Poll, PollStatus};
use bot_util::text::clamp_text;
use feat_giveaways_polls::discord_ops::ops;
use feat_giveaways_polls::poll_service::{self as svc, DurationError, EndPollError, EndReason, PendingPoll, PollInput, PostError, poll};
use serde_json::{Value, json};
use serenity::builder::CreateEmbed;

use crate::giveaways::is_cuid;
use crate::index::{Actor, DashboardTask, TaskCtx, TaskOutcome};
use crate::shared::{TaskError, task_error};
use crate::util::{self, is_id};

const MAX_GATE_ROLES: usize = 10;
const INVALID: &str = "Those poll details were not valid. Reload the page and try again.";

/// `DURATION_ERRORS`.
pub fn duration_error_text(e: DurationError) -> &'static str {
    match e {
        DurationError::Invalid => "Use a length like 30m, 2h, 1d or 1w.",
        DurationError::TooShort => "Polls must run for at least 1 minute.",
        DurationError::TooLong => "Polls can run for at most 30 days.",
    }
}

fn invalid() -> TaskError {
    TaskError::new(INVALID)
}

fn id_list(value: Option<&Value>) -> Result<Vec<String>, TaskError> {
    if util::is_nullish(value) {
        return Ok(Vec::new());
    }
    let Some(arr) = value.and_then(Value::as_array) else { return Err(invalid()) };
    if arr.len() > MAX_GATE_ROLES || !arr.iter().all(|v| is_id(Some(v))) {
        return Err(invalid());
    }
    let mut out: Vec<String> = Vec::new();
    for v in arr.iter().filter_map(Value::as_str) {
        if !out.iter().any(|x| x == v) {
            out.push(v.to_string());
        }
    }
    Ok(out)
}

fn poll_id(payload: &Value) -> Result<String, TaskError> {
    match payload.get("pollId").and_then(Value::as_str) {
        Some(id) if is_cuid(id) => Ok(id.to_string()),
        _ => Err(TaskError::new("That poll no longer exists.")),
    }
}

/// `embedText(embed, guild)`: the text of an error embed from the poll service, as plain text for a toast.
pub fn embed_text(app: &App, guild_id: &str, embed: &CreateEmbed) -> String {
    let v = serde_json::to_value(embed).unwrap_or(Value::Null);
    let text = v.get("description").and_then(Value::as_str).unwrap_or("I cannot post there.");
    util::replace_channel_mentions(&util::strip_bold(text), |id| {
        format!("#{}", util::cached_channel_name(app, guild_id, id).unwrap_or_else(|| "the channel".into()))
    })
}

/// The router's gate and the command's level: the module is on, the user is in the server and is a moderator.
async fn access(app: &App, guild_id: &str, actor: &Actor) -> anyhow::Result<()> {
    if actor.member.is_none() && !actor.is_bot_owner {
        return Err(task_error("You are no longer a member of this server."));
    }
    let Some(settings) = app.settings().get_settings_safe(guild_id).await else {
        return Err(task_error("The server settings could not be loaded. Try again in a moment."));
    };
    if !is_module_enabled(Some(&settings), Some(ModuleKey::Polls)) {
        let m = ModuleKey::Polls.info();
        return Err(task_error(format!("The {} {} module is turned off on this server. Turn it on first.", m.emoji, m.label)));
    }
    let level = if actor.is_bot_owner {
        PermissionLevel::Owner
    } else {
        actor.member.as_ref().map(|m| util::member_level(app, guild_id, m, Some(&settings))).unwrap_or(PermissionLevel::Member)
    };
    if level < PermissionLevel::Moderator {
        return Err(task_error("You need to be a moderator of this server to manage polls."));
    }
    Ok(())
}

async fn load_poll(app: &App, guild_id: &str, id: &str) -> anyhow::Result<Poll> {
    let p: Option<Poll> =
        SelectBuilder::new(&POLL_INFO).filter("id", json!(id))?.filter("guildId", json!(guild_id))?.limit(1).build()?.fetch_optional(app.db()).await?;
    p.ok_or_else(|| task_error("That poll no longer exists."))
}

fn channel_name(app: &App, guild_id: &str, id: &str) -> String {
    util::cached_channel_name(app, guild_id, id).unwrap_or_else(|| "the channel".into())
}

/// `poll.start`.
pub async fn start(ctx: TaskCtx) -> anyhow::Result<TaskOutcome> {
    let app = &ctx.app;
    let guild_id = ctx.guild_id.as_str();
    let actor = &ctx.actor;
    let payload = &ctx.payload;
    access(app, guild_id, actor).await?;

    // The payload is the dashboard's validated form; it is read again as untrusted JSON.
    let options: Option<Vec<String>> = payload.get("options").and_then(Value::as_array).and_then(|a| {
        a.iter().map(|o| o.as_str().filter(|s| !s.contains(['\r', '\n'])).map(String::from)).collect::<Option<Vec<_>>>()
    });
    let (Some(question), Some(options)) = (payload.get("question").and_then(Value::as_str), options) else { return Err(invalid().into()) };
    let Some(channel_id) = payload.get("channelId").and_then(Value::as_str).filter(|s| util::is_snowflake(s)) else { return Err(invalid().into()) };
    let requested = match payload.get("maxChoices") {
        None => Some(1.0),
        v => util::js_integer(v),
    };
    let requested = match requested {
        Some(n) if n >= 1.0 && n <= poll::OPTIONS_MAX as f64 => n as i64,
        _ => return Err(task_error(format!("Pick between 1 and {} choices per member.", poll::OPTIONS_MAX))),
    };
    let duration = match payload.get("duration") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        _ => return Err(invalid().into()),
    };
    let hide_results = match payload.get("hideResults") {
        None => false,
        Some(Value::Bool(b)) => *b,
        _ => return Err(invalid().into()),
    };
    let allowed_ids = id_list(payload.get("allowedRoleIds"))?;
    let denied_ids = id_list(payload.get("deniedRoleIds"))?;

    let (question, q_errors) = svc::validate_question(question);
    let (options, o_errors) = svc::parse_poll_options(&options.join("\n"));
    let problems: Vec<String> = q_errors.into_iter().chain(o_errors).collect();
    if !problems.is_empty() {
        return Err(task_error(format!("That poll is not valid. {}", problems.join(" "))));
    }

    let duration_ms = svc::parse_duration_option(duration.as_deref()).map_err(|e| task_error(duration_error_text(e)))?;

    if denied_ids.iter().any(|id| id == guild_id) {
        return Err(task_error("You can't block @everyone, or nobody could vote."));
    }
    if allowed_ids.iter().any(|id| denied_ids.contains(id)) {
        return Err(task_error("The allowed roles and the blocked roles must be different."));
    }
    for id in allowed_ids.iter().chain(denied_ids.iter()) {
        if id != guild_id && util::fetch_role_name(app, guild_id, id).await.is_none() {
            return Err(task_error("A role you picked no longer exists. Pick another one."));
        }
    }
    let allowed_role_ids: Vec<String> = allowed_ids.into_iter().filter(|id| id != guild_id).collect(); // @everyone = no restriction

    let channel = util::find_channel(app, guild_id, channel_id).await;
    // A bot owner outside the server has no member to check; the bot's own access is still checked.
    let checker = match &actor.member {
        Some(m) => Some(m.clone()),
        None => app.discord().and_then(|d| util::cached_member(app, guild_id, &d.bot_user_id().to_string())),
    };
    if let Some(problem) = svc::channel_problem(app, channel.as_ref(), checker.as_ref()) {
        return Err(task_error(if channel.is_some() { embed_text(app, guild_id, &problem) } else { "That channel no longer exists. Pick another one.".into() }));
    }
    let Some(channel) = channel else { return Err(task_error("That channel no longer exists. Pick another one.")) };

    let max_choices = requested.min(options.len() as i64);
    let pending = PendingPoll {
        guild_id: guild_id.to_string(),
        channel_id: channel.id.to_string(),
        user_id: actor.id.clone(),
        max_choices: requested,
        allowed_role_ids,
        denied_role_ids: denied_ids,
        hide_results,
        duration_ms,
    };
    // Create, post, record the message id and schedule the end: the same steps (and clean-ups) as the /poll form.
    let input = PollInput { question: question.clone(), options, max_choices };
    let out = svc::post_poll_with(app, guild_id, &channel, &pending, &input, "dashboard poll").await?;
    let posted = out.map_err(|e| task_error(post_failure_text(&e, &channel.name)))?;
    let ends_at = if duration_ms.is_some() { posted.poll.ends_at } else { None };
    let message = start_message(&question, &channel.name, ends_at, max_choices, requested, posted.timer_failed);
    Ok(TaskOutcome::new(message, json!({ "pollId": posted.poll.id })))
}

/// Why `postPoll` failed, for `poll.start`.
pub fn post_failure_text(e: &PostError, channel_name: &str) -> String {
    match e {
        PostError::Limit => format!("This server already has {} active polls. End one first.", poll::ACTIVE_MAX),
        PostError::Send(err) => format!("I couldn't post the poll in #{channel_name}: {}", svc::send_failure_reason(Some(err))),
        PostError::Save(_) => "I couldn't save the poll. Please try again in a moment.".into(),
    }
}

/// The message of a posted poll (`ends_at` only when it has a length).
pub fn start_message(question: &str, channel_name: &str, ends_at: Option<chrono::DateTime<chrono::Utc>>, max_choices: i64, requested: i64, timer_failed: bool) -> String {
    let mut message = format!("Posted the poll \"{}\" in #{channel_name}.", clamp_text(question, 80));
    if let Some(ends_at) = ends_at {
        message.push_str(&format!(" It ends {}.", util::utc(ends_at)));
    }
    if max_choices < requested {
        message.push_str(&format!(" Choices per member was lowered to {max_choices} (the number of options)."));
    }
    if timer_failed {
        message.push_str(" The end timer could not be scheduled, so end the poll by hand.");
    }
    message
}

/// The answer of `poll.end` for an `endPoll` result: whether the results were posted, or the refusal.
pub fn end_text(question: &str, res: Result<bool, EndPollError>, place: &str) -> Result<String, String> {
    match res {
        Ok(true) => Ok(format!("Ended \"{}\". The results were posted in #{place}.", clamp_text(question, 80))),
        Ok(false) => Ok(format!("The poll is closed, but I couldn't post the results in #{place}.")),
        Err(EndPollError::AlreadyEnded) => Err("That poll has already ended.".into()),
        Err(_) => Err("That poll no longer exists.".into()),
    }
}

/// `poll.end`.
pub async fn end(ctx: TaskCtx) -> anyhow::Result<TaskOutcome> {
    let app = &ctx.app;
    let guild_id = ctx.guild_id.as_str();
    access(app, guild_id, &ctx.actor).await?;
    let p = load_poll(app, guild_id, &poll_id(&ctx.payload)?).await?;
    let r = svc::end_poll(app, &p.id, EndReason::Manual, Some(guild_id)).await?;
    let place = channel_name(app, guild_id, &p.channel_id);
    let message = end_text(&p.question, r.map(|r| r.posted), &place).map_err(task_error)?;
    Ok(TaskOutcome::new(message, json!({ "pollId": p.id })))
}

/// `poll.delete`.
pub async fn remove(ctx: TaskCtx) -> anyhow::Result<TaskOutcome> {
    let app = &ctx.app;
    let guild_id = ctx.guild_id.as_str();
    access(app, guild_id, &ctx.actor).await?;
    let p = load_poll(app, guild_id, &poll_id(&ctx.payload)?).await?;

    // An active poll ends first, the way it does when its message is deleted: no results post, the end job and any
    // queued live render are dropped.
    if p.status == PollStatus::Active {
        let _ = svc::end_poll(app, &p.id, EndReason::Deleted, Some(guild_id)).await?;
    }

    let mut note = "";
    if let Some(message_id) = p.message_id.as_deref()
        && let Err((code, why)) = delete_poll_message(app, guild_id, &p.channel_id, message_id).await
    {
        note = delete_failure_note(code);
        if !matches!(code, Some(10003) | Some(10008) | Some(50001) | Some(50013) | Some(50083)) {
            bot_util::log_warn!(format!("dashboard poll {}: could not delete the message: {why}", p.id));
        }
    }
    delete_where(&POLL_INFO, [("id", json!(p.id)), ("guildId", json!(guild_id))])?.execute(app.db()).await?;
    Ok(TaskOutcome::new(format!("Deleted the poll \"{}\" and its votes.{note}", clamp_text(&p.question, 80)), json!({ "pollId": p.id })))
}

/// The note of `poll.delete` for a failed message deletion (its Discord code; `None` for an error without one).
pub fn delete_failure_note(code: Option<u64>) -> &'static str {
    match code {
        // Already gone: nothing to delete.
        Some(10003) | Some(10008) => "",
        Some(50001) | Some(50013) | Some(50083) => " I could not delete its message (I am missing permissions there), so delete that by hand.",
        _ => " I could not delete its message, so delete that by hand.",
    }
}

/// `channel = guild.channels.cache.get(id) ?? await guild.channels.fetch(id); markBotDeletion(..);
/// await channel.messages.delete(id)`: the channel must be one of this guild (a channel of another guild is
/// discord.js's GuildChannelUnowned, an error without a code), and the deletion is marked only once it is found.
async fn delete_poll_message(app: &App, guild_id: &str, channel_id: &str, message_id: &str) -> Result<(), (Option<u64>, String)> {
    let (Some(g), Some(c)) = (util::gid(guild_id), util::cid(channel_id)) else { return Err((None, "invalid channel id".into())) };
    match bot_core::interaction_helpers::fetch_channel(app, g, c).await {
        Ok(_) => {}
        Err(bot_core::interaction_helpers::FetchChannelError::Rest(e)) => return Err((bot_core::discord::error_code(&e), e.to_string())),
        Err(e) => return Err((None, e.to_string())),
    }
    bot_core::deletion_tracker::mark_bot_deletion(app, &[message_id], Some("polls"), None);
    ops(app).delete_message(channel_id, message_id).await.map_err(|e| (e.code, e.message))
}

/// `createPollTasks()`.
pub fn tasks() -> Vec<(&'static str, DashboardTask)> {
    vec![("poll.start", DashboardTask::new(start)), ("poll.end", DashboardTask::new(end)), ("poll.delete", DashboardTask::new(remove))]
}
