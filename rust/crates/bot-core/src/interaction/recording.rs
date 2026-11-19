//! The test backend: records every response as JSON (the body Discord would receive) and fabricates the messages
//! Discord would return, so command and component logic runs offline.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use serde_json::{Value, json};
use serenity::all::{InteractionId, Message, MessageId};
use serenity::builder::{CreateInteractionResponse, CreateInteractionResponseFollowup, EditInteractionResponse};

use super::{InteractionBackend, InteractionError};

/// One recorded call.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedCall {
    /// The discord.js method: `reply`, `deferReply`, `deferUpdate`, `update`, `showModal`, `respond`, `editReply`,
    /// `deleteReply`, `fetchReply`, `followUp`, `editFollowUp`, `deleteFollowUp`.
    pub method: &'static str,
    /// The JSON body (for responses: the callback's `data`, for edits and follow-ups: the message body).
    pub body: Value,
    /// The interaction token the call was made with.
    pub token: String,
    /// The id of the interaction the call belongs to (known once that interaction made its first response).
    pub interaction_id: Option<u64>,
}

impl RecordedCall {
    /// `data.flags & 64`.
    pub fn ephemeral(&self) -> bool {
        self.body.get("flags").and_then(Value::as_u64).is_some_and(|f| f & super::EPHEMERAL_FLAG != 0)
    }

    /// `content`, when set.
    pub fn content(&self) -> Option<&str> {
        self.body.get("content").and_then(Value::as_str)
    }

    /// The first embed's title, when set.
    pub fn embed_title(&self) -> Option<&str> {
        self.body.get("embeds").and_then(|e| e.get(0)).and_then(|e| e.get("title")).and_then(Value::as_str)
    }

    /// The first embed's description, when set.
    pub fn embed_description(&self) -> Option<&str> {
        self.body.get("embeds").and_then(|e| e.get(0)).and_then(|e| e.get("description")).and_then(Value::as_str)
    }

    /// `embed_title()`, else `content()` (the JS tests' `titleOf`).
    pub fn title_or_content(&self) -> Option<&str> {
        self.embed_title().or_else(|| self.content())
    }

    /// The autocomplete choices of a `respond`.
    pub fn choices(&self) -> Vec<Value> {
        self.body.get("choices").and_then(Value::as_array).cloned().unwrap_or_default()
    }
}

/// The recording backend.
#[derive(Default)]
pub struct RecordingBackend {
    calls: Mutex<Vec<RecordedCall>>,
    failing: AtomicBool,
    next_id: AtomicU64,
    /// `(method, code)`: the next call of `method` fails with that Discord error code (once each).
    fail_once: Mutex<Vec<(&'static str, u64)>>,
    /// token -> interaction id, learned from each interaction's first response.
    ids: Mutex<std::collections::HashMap<String, u64>>,
}

impl RecordingBackend {
    pub fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            failing: AtomicBool::new(false),
            next_id: AtomicU64::new(1_100_000_000_000_000_000),
            fail_once: Mutex::new(Vec::new()),
            ids: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// The calls made for one interaction (the JS tests' `verbs(click)`), by its id.
    pub fn calls_of(&self, interaction_id: u64) -> Vec<RecordedCall> {
        self.calls().into_iter().filter(|c| c.interaction_id == Some(interaction_id)).collect()
    }

    /// The method names of one interaction's calls.
    pub fn names_of(&self, interaction_id: u64) -> Vec<&'static str> {
        self.calls_of(interaction_id).iter().map(|c| c.method).collect()
    }

    /// Every call so far.
    pub fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The method names so far (the JS tests' `names(i)`).
    pub fn names(&self) -> Vec<&'static str> {
        self.calls().iter().map(|c| c.method).collect()
    }

    pub fn clear(&self) {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Makes every call fail (an expired interaction) or work again.
    pub fn set_failing(&self, failing: bool) {
        self.failing.store(failing, Ordering::SeqCst);
    }

    /// The next `method` call (`"editReply"`, `"reply"`, ...) fails with Discord error `code` (for example 10008
    /// "Unknown Message" for an `editReply` after `deleteReply`). The call is still recorded.
    pub fn fail_next(&self, method: &'static str, code: u64) {
        self.fail_once.lock().unwrap_or_else(|e| e.into_inner()).push((method, code));
    }

    fn record(&self, token: &str, method: &'static str, body: Value) -> Result<(), InteractionError> {
        let interaction_id = self.ids.lock().unwrap_or_else(|e| e.into_inner()).get(token).copied();
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).push(RecordedCall { method, body, token: token.to_string(), interaction_id });
        if self.failing.load(Ordering::SeqCst) {
            return Err(InteractionError::Api { code: 10062, message: "Unknown interaction".into() });
        }
        let mut once = self.fail_once.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pos) = once.iter().position(|(m, _)| *m == method) {
            let (_, code) = once.remove(pos);
            return Err(InteractionError::Api { code, message: format!("Discord error {code}") });
        }
        Ok(())
    }

    /// A message as Discord would return it for `body`.
    pub fn fabricate(&self, id: Option<u64>, body: &Value) -> Result<Message, InteractionError> {
        let id = id.unwrap_or_else(|| self.next_id.fetch_add(1, Ordering::SeqCst));
        let msg = json!({
            "id": id.to_string(),
            "channel_id": "100000000000000003",
            "author": { "id": "1000000000000000002", "username": "bot", "discriminator": "0000", "avatar": null, "bot": true },
            "content": body.get("content").cloned().unwrap_or(json!("")),
            "timestamp": "2026-01-01T00:00:00.000000+00:00",
            "edited_timestamp": null,
            "tts": false,
            "mention_everyone": false,
            "mentions": [],
            "mention_roles": [],
            "attachments": [],
            "embeds": body.get("embeds").cloned().unwrap_or(json!([])),
            "pinned": false,
            "type": 0,
            "flags": body.get("flags").cloned().unwrap_or(json!(0)),
            "components": body.get("components").cloned().unwrap_or(json!([])),
        });
        serde_json::from_value(msg).map_err(|e| InteractionError::Http(format!("fabricated message: {e}")))
    }
}

fn response_parts(response: &CreateInteractionResponse) -> (&'static str, Value) {
    let v = serde_json::to_value(response).unwrap_or(Value::Null);
    let method = match v.get("type").and_then(Value::as_u64) {
        Some(4) => "reply",
        Some(5) => "deferReply",
        Some(6) => "deferUpdate",
        Some(7) => "update",
        Some(8) => "respond",
        Some(9) => "showModal",
        _ => "createResponse",
    };
    (method, v.get("data").cloned().unwrap_or(Value::Null))
}

#[async_trait]
impl InteractionBackend for RecordingBackend {
    async fn create_response(&self, id: InteractionId, token: &str, response: CreateInteractionResponse) -> Result<(), InteractionError> {
        self.ids.lock().unwrap_or_else(|e| e.into_inner()).insert(token.to_string(), id.get());
        let (method, body) = response_parts(&response);
        self.record(token, method, body)
    }

    async fn get_response(&self, token: &str) -> Result<Message, InteractionError> {
        self.record(token, "fetchReply", Value::Null)?;
        self.fabricate(Some(1_000_000_000_000_000_100), &Value::Null)
    }

    async fn edit_response(&self, token: &str, edit: EditInteractionResponse) -> Result<Message, InteractionError> {
        let body = serde_json::to_value(&edit).unwrap_or(Value::Null);
        self.record(token, "editReply", body.clone())?;
        self.fabricate(Some(1_000_000_000_000_000_100), &body)
    }

    async fn delete_response(&self, token: &str) -> Result<(), InteractionError> {
        self.record(token, "deleteReply", Value::Null)
    }

    async fn create_followup(&self, token: &str, followup: CreateInteractionResponseFollowup) -> Result<Message, InteractionError> {
        let body = serde_json::to_value(&followup).unwrap_or(Value::Null);
        self.record(token, "followUp", body.clone())?;
        self.fabricate(None, &body)
    }

    async fn edit_followup(&self, token: &str, id: MessageId, followup: CreateInteractionResponseFollowup) -> Result<Message, InteractionError> {
        let body = serde_json::to_value(&followup).unwrap_or(Value::Null);
        self.record(token, "editFollowUp", body.clone())?;
        self.fabricate(Some(id.get()), &body)
    }

    async fn delete_followup(&self, token: &str, id: MessageId) -> Result<(), InteractionError> {
        self.record(token, "deleteFollowUp", json!({ "id": id.to_string() }))
    }
}
