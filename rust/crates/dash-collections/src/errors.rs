//! Port of `src/dashboard/collections/errors.js`: the error a collection store throws for an expected failure (the
//! server maps `code` to an HTTP status and shows `message` to the admin verbatim), the outcome of a dashboard task as a
//! store wants it, and a store's create/update answer with an optional note (`withMessage` / `unwrapResult`).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The `code` of a [`RecordError`] (`RECORD_ERROR_STATUS` keys).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordErrorCode {
    /// The input breaks a rule the descriptors cannot express (shown as is). 400.
    Invalid,
    /// No such record in this server. 404.
    NotFound,
    /// A unique name or key is taken. 409.
    Conflict,
    /// The server already has the maximum number of these records. 409.
    Limit,
    /// Discord refused or failed a write. 502.
    Discord,
    /// The bot ran the action and refused it (dashboard tasks). 422.
    Failed,
    /// The feature cannot be managed from the dashboard right now. 503.
    Unavailable,
}

/// `RECORD_ERROR_STATUS`, in the JS order.
pub const RECORD_ERROR_STATUS: [(RecordErrorCode, u16); 7] = [
    (RecordErrorCode::Invalid, 400),
    (RecordErrorCode::NotFound, 404),
    (RecordErrorCode::Conflict, 409),
    (RecordErrorCode::Limit, 409),
    (RecordErrorCode::Discord, 502),
    (RecordErrorCode::Failed, 422),
    (RecordErrorCode::Unavailable, 503),
];

impl RecordErrorCode {
    /// The JS code string (`'not_found'`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::Limit => "limit",
            Self::Discord => "discord",
            Self::Failed => "failed",
            Self::Unavailable => "unavailable",
        }
    }

    /// A JS code string; an unknown one is `invalid` (the constructor's fallback).
    pub fn parse(code: &str) -> Self {
        RECORD_ERROR_STATUS.iter().map(|(c, _)| *c).find(|c| c.as_str() == code).unwrap_or(Self::Invalid)
    }

    /// The HTTP status (`RECORD_ERROR_STATUS[code]`).
    pub fn status(self) -> u16 {
        RECORD_ERROR_STATUS.iter().find(|(c, _)| *c == self).map_or(400, |(_, s)| *s)
    }
}

/// `RecordError`: an expected failure of a store. `path` names the field the message is about; `saved` says the change
/// was stored before the failure (e.g. a panel saved but not posted), so the browser must not offer the same create again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordError {
    pub code: RecordErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub saved: bool,
}

impl RecordError {
    /// `new RecordError(code, message)`.
    pub fn new(code: RecordErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), path: None, saved: false }
    }

    /// `new RecordError(code, message)` from a JS code string (unknown codes become `invalid`).
    pub fn from_code(code: &str, message: impl Into<String>) -> Self {
        Self::new(RecordErrorCode::parse(code), message)
    }

    /// `{ path }` (an empty path is not set, as in the JS).
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        let p = path.into();
        self.path = (!p.is_empty()).then_some(p);
        self
    }

    /// `{ saved: true }`.
    pub fn saved(mut self) -> Self {
        self.saved = true;
        self
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(RecordErrorCode::Invalid, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(RecordErrorCode::NotFound, message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(RecordErrorCode::Unavailable, message)
    }

    pub fn discord(message: impl Into<String>) -> Self {
        Self::new(RecordErrorCode::Discord, message)
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self::new(RecordErrorCode::Failed, message)
    }

    /// The HTTP status of the code.
    pub fn status(&self) -> u16 {
        self.code.status()
    }

    /// The error as the goldens record it: `{ code, message, path?, saved? }`.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("code".into(), Value::String(self.code.as_str().into()));
        m.insert("message".into(), Value::String(self.message.clone()));
        if let Some(p) = &self.path {
            m.insert("path".into(), Value::String(p.clone()));
        }
        if self.saved {
            m.insert("saved".into(), Value::Bool(true));
        }
        Value::Object(m)
    }
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RecordError {}

/// The `status` of a dashboard task's result (src/dashboard/tasks.js `run`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    /// Finished.
    Done,
    /// The bot refused (the message says why).
    Failed,
    /// Still running when the wait ended.
    Pending,
    /// No shard picked it up in time, so it was cancelled.
    Offline,
}

/// What the task runner answers: `{ status, message, data }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskResult {
    pub status: TaskStatus,
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

impl TaskResult {
    pub fn done(message: impl Into<String>, data: Option<Value>) -> Self {
        Self { status: TaskStatus::Done, message: message.into(), data }
    }
}

/// [`task_outcome`]'s answer: `status` is `Done` or `Pending`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskOutcome {
    pub status: TaskStatus,
    pub message: String,
    pub data: Option<Value>,
}

/// `taskOutcome(result)`: `done` and `pending` are returned, `failed` becomes RecordError('failed') with the bot's
/// reason and `offline` RecordError('unavailable'). `None` (a missing result) reads as done with 'Done.'.
pub fn task_outcome(result: Option<&TaskResult>) -> Result<TaskOutcome, RecordError> {
    match result {
        Some(r) if r.status == TaskStatus::Failed => Err(RecordError::failed(r.message.clone())),
        Some(r) if r.status == TaskStatus::Offline => Err(RecordError::unavailable(r.message.clone())),
        Some(r) => Ok(TaskOutcome {
            status: if r.status == TaskStatus::Pending { TaskStatus::Pending } else { TaskStatus::Done },
            message: r.message.clone(),
            data: r.data.clone().filter(|d| !d.is_null()),
        }),
        None => Ok(TaskOutcome { status: TaskStatus::Done, message: "Done.".into(), data: None }),
    }
}

/// A store's create or update answer (`withMessage(item, message)` / `unwrapResult(value)`): the item, and a note for
/// the admin shown instead of the plain "Saved" toast.
#[derive(Debug, Clone, PartialEq)]
pub struct Saved {
    pub item: Value,
    pub message: Option<String>,
}

impl Saved {
    /// The item as is (no note).
    pub fn item(item: Value) -> Self {
        Self { item, message: None }
    }

    /// `withMessage(item, message)`: an empty or missing message keeps the plain item.
    pub fn with_message(item: Value, message: Option<impl Into<String>>) -> Self {
        let message = message.map(Into::into).filter(|m: &String| !m.is_empty());
        Self { item, message }
    }
}

impl From<Value> for Saved {
    fn from(item: Value) -> Self {
        Self::item(item)
    }
}
