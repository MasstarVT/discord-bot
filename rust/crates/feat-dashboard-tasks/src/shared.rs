//! Port of `src/services/dashboardTasks/shared.js`: what the web dashboard and the bot share about dashboard tasks.
//!
//! Actions the dashboard cannot do itself (they need the gateway client, e.g. banning with the moderation rules or
//! posting a giveaway) are queued as `dashboard.task` jobs, run by the bot shard that has the guild, and answered
//! through a short-lived Redis result. Pure.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `TASK_JOB_TYPE`.
pub const TASK_JOB_TYPE: &str = "dashboard.task";

/// How long the bot keeps a task's result for the dashboard to read.
pub const TASK_RESULT_TTL_SECONDS: u64 = 900;

/// Most a task's message may hold (it is shown to the admin as is), in UTF-16 units.
pub const TASK_MESSAGE_MAX: usize = 500;

/// Redis key of a finished task's result `{ ok: true, message, data }` (the job row is gone once it succeeds).
pub fn task_result_key(job_id: &str) -> String {
    format!("dashboard:task:{job_id}")
}

/// The dedupe key of one task (unique per request).
pub fn task_dedupe_key(task_id: &str) -> String {
    format!("dashboard.task:{task_id}")
}

/// `TASK_KIND_RE` (`/^[a-z][a-zA-Z]{0,30}\.[a-z][a-zA-Z]{0,30}$/`).
pub fn is_task_kind(kind: &str) -> bool {
    let Some((area, verb)) = kind.split_once('.') else { return false };
    let part = |p: &str| {
        let mut chars = p.chars();
        matches!(chars.next(), Some('a'..='z')) && p.len() <= 31 && chars.all(|c| c.is_ascii_alphabetic())
    };
    part(area) && part(verb)
}

/// The value stored under [`task_result_key`]: `{ ok: true, message, data }`, key order as the JS writes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskResultValue {
    pub ok: bool,
    pub message: String,
    #[serde(default)]
    pub data: Value,
}

/// An expected failure of a task: `message` is shown to the admin verbatim (e.g. "That member is not in the
/// server."). Anything else a task returns as an error is logged and reported as a generic failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskError(pub String);

impl TaskError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    /// The TaskError in `err`'s chain, if any (`err instanceof TaskError`).
    pub fn of(err: &anyhow::Error) -> Option<&TaskError> {
        err.chain().find_map(|e| e.downcast_ref::<TaskError>())
    }
}

impl std::fmt::Display for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TaskError {}

/// `throw new TaskError(message)` as an `anyhow::Error`.
pub fn task_error(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(TaskError::new(message))
}
