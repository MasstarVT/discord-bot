//! Port of `src/services/dashboardTasks/index.js`: runs `dashboard.task` jobs in the bot. Every kind the web dashboard
//! may queue is collected from the feature modules of this crate (each exports `tasks()`, a list of
//! `('<area>.<verb>', task)`), plus the job handler the dashboard-tasks plugin registers. A task runs once
//! (maxAttempts 1): its result, or the reason it failed, goes back to the waiting dashboard.

use std::sync::{Arc, OnceLock};

use bot_core::error::log_arg;
use bot_core::scheduler::{Job, JobCtx, JobOutcome};
use bot_core::{App, BotResult, BoxFuture, PermanentJobError};
use serde_json::{Map, Value, json};
use serenity::all::Member;

use crate::shared::{TASK_MESSAGE_MAX, TASK_RESULT_TTL_SECONDS, TaskError, task_result_key};
use crate::util;

/// The dashboard user who asked: `{ id, member (null when not in the guild), isBotOwner }`.
#[derive(Debug, Clone)]
pub struct Actor {
    /// `actorId` as the JS template literal prints it (`String(actorId)`).
    pub id: String,
    /// The raw `actorId` of the payload.
    pub raw_id: Option<Value>,
    pub member: Option<Member>,
    pub is_bot_owner: bool,
}

/// What a task gets: `{ guild, client, actor, payload, taskId }` (the guild and the client are `app` and `guild_id`).
#[derive(Clone)]
pub struct TaskCtx {
    pub app: App,
    pub guild_id: String,
    pub actor: Actor,
    pub payload: Value,
    pub task_id: String,
}

/// What a task returns: `{ message, data? }`.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskOutcome {
    pub message: String,
    pub data: Option<Value>,
}

impl TaskOutcome {
    pub fn new(message: impl Into<String>, data: Value) -> Self {
        Self { message: message.into(), data: Some(data) }
    }

    pub fn message(message: impl Into<String>) -> Self {
        Self { message: message.into(), data: None }
    }
}

/// `task.run(ctx)`: an `Ok` outcome, or an error (a [`TaskError`] is shown to the admin, anything else is generic).
pub type TaskRun = Arc<dyn Fn(TaskCtx) -> BoxFuture<anyhow::Result<TaskOutcome>> + Send + Sync>;

/// A task: `{ run }`.
#[derive(Clone)]
pub struct DashboardTask {
    pub run: TaskRun,
}

impl DashboardTask {
    pub fn new<F, Fut>(f: F) -> Self
    where
        F: Fn(TaskCtx) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = anyhow::Result<TaskOutcome>> + Send + 'static,
    {
        Self { run: Arc::new(move |ctx| Box::pin(f(ctx))) }
    }
}

/// kind -> task, in declaration order. A kind is claimed by exactly one module.
#[derive(Clone, Default)]
pub struct TaskMap(Vec<(String, DashboardTask)>);

impl TaskMap {
    /// Builds the map; a kind declared twice is an error (the JS throws at import).
    pub fn from_groups(groups: Vec<Vec<(&'static str, DashboardTask)>>) -> Result<Self, String> {
        let mut out: Vec<(String, DashboardTask)> = Vec::new();
        for group in groups {
            for (kind, task) in group {
                if out.iter().any(|(k, _)| k == kind) {
                    return Err(format!("dashboard task {kind} is declared twice"));
                }
                out.push((kind.to_string(), task));
            }
        }
        Ok(Self(out))
    }

    pub fn get(&self, kind: &str) -> Option<&DashboardTask> {
        self.0.iter().find(|(k, _)| k == kind).map(|(_, t)| t)
    }

    /// `Object.keys(TASKS)`.
    pub fn kinds(&self) -> Vec<&str> {
        self.0.iter().map(|(k, _)| k.as_str()).collect()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// `TASKS`: every kind of this crate.
pub fn tasks() -> &'static TaskMap {
    static TASKS: OnceLock<TaskMap> = OnceLock::new();
    TASKS.get_or_init(|| {
        TaskMap::from_groups(vec![
            crate::moderation::tasks(),
            crate::members::tasks(),
            crate::giveaways::tasks(),
            crate::polls::tasks(),
            crate::reaction_roles::tasks(),
            crate::tickets::tasks(),
            crate::channel_access::tasks(),
        ])
        .unwrap_or_else(|e| panic!("{e}"))
    })
}

/// `String(text ?? '').slice(0, TASK_MESSAGE_MAX)`.
pub fn clamp_message(text: &str) -> String {
    util::slice16(text, TASK_MESSAGE_MAX)
}

/// `resolveActor(guild, actorId)`.
pub async fn resolve_actor(app: &App, guild_id: &str, actor_id: Option<&Value>) -> Actor {
    let as_str = actor_id.and_then(Value::as_str);
    let is_bot_owner = match (app.config().bot_owner_id.as_deref(), as_str) {
        (Some(owner), Some(id)) => !owner.is_empty() && owner == id,
        _ => false,
    };
    let member = match as_str {
        Some(id) if util::is_snowflake(id) => util::fetch_member(app, guild_id, id).await,
        _ => None,
    };
    Actor { id: util::js_string(actor_id), raw_id: actor_id.cloned(), member, is_bot_owner }
}

/// The `dashboard.task` job handler. Expected failures (TaskError) and unknown kinds fail the job permanently with
/// the message the dashboard shows; anything else is logged and fails it with a generic message.
pub async fn run_dashboard_task(job: Job, ctx: JobCtx) -> BotResult<JobOutcome> {
    execute_task(tasks(), &job, &ctx.app, &ctx.guild_id).await?;
    Ok(JobOutcome::Done)
}

/// Runs one task from `tasks` ([`tasks()`], or a test double): the outcome is stored for the dashboard, a failure
/// becomes a [`PermanentJobError`] whose message the dashboard shows.
pub async fn execute_task(tasks: &TaskMap, job: &Job, app: &App, guild_id: &str) -> anyhow::Result<()> {
    let empty = Map::new();
    let outer = job.payload.as_object().unwrap_or(&empty);
    let kind = outer.get("kind");
    let actor_id = outer.get("actorId");
    // `payload = {}` is the default only when the key is missing (undefined); an explicit null stays null.
    let payload = outer.get("payload").cloned().unwrap_or_else(|| json!({}));
    let task = kind.and_then(Value::as_str).and_then(|k| tasks.get(k)).cloned();
    let Some(task) = task else {
        let shown = util::slice16(&util::js_string(kind), 40);
        return Err(PermanentJobError::new(format!("This bot version cannot do \"{shown}\" yet. Update the bot.")).into());
    };
    let kind = kind.and_then(Value::as_str).unwrap_or_default().to_string();

    let actor = resolve_actor(app, guild_id, actor_id).await;
    let actor_shown = actor.id.clone();
    let ctx = TaskCtx { app: app.clone(), guild_id: guild_id.to_string(), actor, payload, task_id: job.id.clone() };
    let result = match (task.run)(ctx).await {
        Ok(r) => r,
        Err(err) => {
            if let Some(te) = TaskError::of(&err) {
                return Err(PermanentJobError::new(clamp_message(&te.0)).into());
            }
            bot_util::log_error!(format!("Dashboard task {kind} (job {}, guild {guild_id}) failed", job.id), log_arg(&err));
            return Err(PermanentJobError::new("The bot hit an error doing that. Check its logs, then try again.").into());
        }
    };
    let message = clamp_message(if result.message.is_empty() { "Done." } else { &result.message });
    let value = json!({ "ok": true, "message": message, "data": result.data.unwrap_or(Value::Null) });
    app.cache().set(&task_result_key(&job.id), &value, TASK_RESULT_TTL_SECONDS).await;
    bot_util::log_info!(format!("Dashboard task {kind} done in guild {guild_id} for {actor_shown}: {message}"));
    Ok(())
}
