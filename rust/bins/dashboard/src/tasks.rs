//! Port of `src/dashboard/tasks.js`: the dashboard's way to have the bot do something that needs the gateway client
//! (ban with the moderation rules, post a giveaway, sync level roles, ...). It queues a `dashboard.task` job for the
//! shard that has the guild, then waits a few seconds for its outcome. The bot runs it within ~5 s
//! (feat-dashboard-tasks). A task nobody picks up in time is cancelled, so it can never run later by surprise.
//!
//! The protocol is the JS one byte for byte: the job (type `dashboard.task`, payload `{ kind, actorId, payload }`,
//! dedupe key `dashboard.task:<uuid>`, one attempt), the FAILED/CANCELLED row's `lastError`, and the Redis result
//! `dashboard:task:<jobId>` written when the bot finishes (the row is deleted on success).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use bot_config::JobType;
use bot_core::App;
use bot_core::scheduler::ScheduleJob;
use dash_collections::{TaskOpts, TaskResult, TaskRunner, TaskStatus};
use feat_dashboard_tasks::shared::{is_task_kind, task_dedupe_key, task_result_key};
use serde_json::{Map, Value, json};

/// How long a request waits for the bot (the bot polls every 5 s; most tasks take under a second).
pub const TASK_WAIT_MS: u64 = 12_000;
/// How often the outcome is read while waiting.
pub const TASK_POLL_MS: u64 = 400;

/// The pause between two reads (injectable for tests).
pub type Sleep = Arc<dyn Fn(u64) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;
/// Milliseconds since the epoch (injectable for tests).
pub type Now = Arc<dyn Fn() -> i64 + Send + Sync>;
/// Queues the job and answers its id (`schedule` of createTaskRunner; injectable for tests).
pub type Schedule = Arc<dyn Fn(ScheduleJob) -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send>> + Send + Sync>;

/// `DONE_TEXT`, `FAILED_TEXT`, ...: the texts the JS runner answers with.
pub const DONE_TEXT: &str = "Done.";
pub const FAILED_TEXT: &str = "The bot could not do that.";
pub const OFFLINE_TEXT: &str = "The bot did not pick this up in time, so nothing was done. Check that the bot is online, then try again.";
pub const PENDING_TEXT: &str = "The bot is still working on it. Refresh in a moment to see the result.";

/// `createTaskRunner({ logger, waitMs, pollMs, sleep, db, kv, schedule })`.
#[derive(Clone)]
pub struct TaskClient {
    app: App,
    wait_ms: u64,
    poll_ms: u64,
    sleep: Sleep,
    now: Now,
    schedule: Option<Schedule>,
}

impl TaskClient {
    /// The runner with the JS defaults (12 s wait, 400 ms polls) over the app's database, Redis and scheduler.
    pub fn new(app: App) -> Self {
        Self {
            app,
            wait_ms: TASK_WAIT_MS,
            poll_ms: TASK_POLL_MS,
            sleep: Arc::new(|ms| Box::pin(tokio::time::sleep(std::time::Duration::from_millis(ms)))),
            now: Arc::new(|| chrono::Utc::now().timestamp_millis()),
            schedule: None,
        }
    }

    /// `waitMs`.
    pub fn wait_ms(mut self, ms: u64) -> Self {
        self.wait_ms = ms;
        self
    }

    /// `pollMs`.
    pub fn poll_ms(mut self, ms: u64) -> Self {
        self.poll_ms = ms;
        self
    }

    /// `sleep`.
    pub fn sleep(mut self, sleep: Sleep) -> Self {
        self.sleep = sleep;
        self
    }

    /// The clock the deadline is measured with.
    pub fn now(mut self, now: Now) -> Self {
        self.now = now;
        self
    }

    /// `schedule` (a test double of scheduleJob that answers the job id).
    pub fn schedule(mut self, schedule: Schedule) -> Self {
        self.schedule = Some(schedule);
        self
    }

    /// The job createTaskRunner().run queues (`uuid` is the randomUUID of the dedupe key).
    pub fn task_job(guild_id: &str, kind: &str, payload: &Value, actor_id: &str, uuid: &str, run_at: chrono::DateTime<chrono::Utc>) -> ScheduleJob {
        let mut body = Map::new();
        body.insert("kind".into(), json!(kind));
        body.insert("actorId".into(), json!(actor_id));
        // `payload ?? {}`
        body.insert("payload".into(), if payload.is_null() { json!({}) } else { payload.clone() });
        ScheduleJob::new(guild_id, JobType::DashboardTask, run_at).payload(Value::Object(body)).dedupe_key(task_dedupe_key(uuid)).max_attempts(1)
    }

    /// The outcome once the job finished, or `None` while it is queued or running.
    pub async fn outcome(&self, id: &str) -> anyhow::Result<Option<TaskResult>> {
        let row: Option<(String, Option<String>)> = sqlx::query_as(r#"SELECT "status"::text, "lastError" FROM "ScheduledJob" WHERE "id" = $1"#)
            .bind(id)
            .fetch_optional(self.app.db())
            .await?;
        let Some((status, last_error)) = row else {
            // Success deletes the row; the message waits in Redis (a Redis outage only loses the wording).
            let result = self.app.cache().get(&task_result_key(id)).await;
            let message = result.as_ref().and_then(|r| r.get("message")).and_then(Value::as_str).unwrap_or(DONE_TEXT).to_string();
            let data = result.as_ref().and_then(|r| r.get("data")).filter(|d| !d.is_null()).cloned();
            return Ok(Some(TaskResult { status: TaskStatus::Done, message, data }));
        };
        if status == "FAILED" || status == "CANCELLED" {
            // `row.lastError || '...'`: an empty error counts as none.
            let message = last_error.filter(|e| !e.is_empty()).unwrap_or_else(|| FAILED_TEXT.to_string());
            return Ok(Some(TaskResult { status: TaskStatus::Failed, message, data: None }));
        }
        Ok(None)
    }

    /// `run(guildId, kind, payload, { actorId, waitMs })`: queues a task for the bot and waits for it.
    /// `Err` on an invalid kind (the JS TypeError) or a database failure (nothing was queued, or the outcome could not
    /// be read).
    pub async fn run_task(&self, guild_id: &str, kind: &str, payload: Value, actor_id: &str, wait_ms: Option<u64>) -> anyhow::Result<TaskResult> {
        if !is_task_kind(kind) {
            anyhow::bail!("invalid dashboard task kind {kind}");
        }
        let wait = wait_ms.unwrap_or(self.wait_ms);
        let job = Self::task_job(guild_id, kind, &payload, actor_id, &bot_db::new_uuid(), chrono::Utc::now());
        let id = match &self.schedule {
            Some(f) => f(job).await?,
            None => self.app.scheduler().schedule_job(job).await?.id,
        };

        let deadline = (self.now)() + wait as i64;
        loop {
            if let Some(done) = self.outcome(&id).await? {
                return Ok(done);
            }
            if (self.now)() >= deadline {
                break;
            }
            (self.sleep)(self.poll_ms).await;
        }

        // Still queued: cancel it unless a shard claimed it in the meantime (only a PENDING row is deleted).
        let count = sqlx::query(r#"DELETE FROM "ScheduledJob" WHERE "id" = $1 AND "status" = 'PENDING'"#).bind(&id).execute(self.app.db()).await?.rows_affected();
        if count == 0
            && let Some(late) = self.outcome(&id).await?
        {
            return Ok(late);
        }
        if count == 1 {
            bot_util::log_warn!(format!(
                "Dashboard: task {kind} in guild {guild_id} was not picked up in {} s; cancelled",
                bot_util::js::number_to_string((wait as f64 / 1000.0).round())
            ));
            return Ok(TaskResult { status: TaskStatus::Offline, message: OFFLINE_TEXT.to_string(), data: None });
        }
        Ok(TaskResult { status: TaskStatus::Pending, message: PENDING_TEXT.to_string(), data: None })
    }
}

#[async_trait]
impl TaskRunner for TaskClient {
    async fn run(&self, guild_id: &str, kind: &str, payload: Value, opts: TaskOpts) -> anyhow::Result<TaskResult> {
        self.run_task(guild_id, kind, payload, &opts.actor_id, opts.wait_ms).await
    }
}

/// A result as the JS runner returns it (`{ status, message, data }`), for comparisons with the JS.
pub fn result_json(r: &TaskResult) -> Value {
    let status = match r.status {
        TaskStatus::Done => "done",
        TaskStatus::Failed => "failed",
        TaskStatus::Pending => "pending",
        TaskStatus::Offline => "offline",
    };
    json!({ "status": status, "message": r.message, "data": r.data.clone().unwrap_or(Value::Null) })
}
