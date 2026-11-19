//! Port of `src/dashboard/collections/channelAccess.js`: the slash commands' channel rule for the dashboard's record
//! stores. Whoever configures a bot post must be able to use the channel themselves (View Channel, plus Send Messages
//! where the bot posts). The dashboard only knows the admin's guild-level permissions, so the check is a bot task
//! (`access.channels`) that resolves the admin as a real member. The guild owner, the bot owner (both `owner` in
//! `ctx.member`) and Administrators are exempt without asking the bot; anyone else is refused when the check cannot be
//! made.

use serde_json::{Map, Value, json};

use crate::errors::{RecordError, TaskStatus, task_outcome};
use crate::perms::{flags, has_flag};
use crate::store::{DashboardMember, StoreCtx, StoreError};
use crate::util::{js_string, slice16};

/// The needs of a post (memberChannelProblem's default).
pub const SEND_NEEDS: [&str; 2] = ["ViewChannel", "SendMessages"];
/// The needs of looking at a message the bot posted.
pub const VIEW_NEEDS: [&str; 1] = ["ViewChannel"];

/// `isChannelExempt(member)`: the owner or an Administrator.
pub fn is_channel_exempt(member: Option<&DashboardMember>) -> bool {
    let Some(m) = member else { return false };
    m.owner || has_flag(&m.permissions, flags::ADMINISTRATOR)
}

/// One check of [`require_channel_access`]: `needs` are PermissionFlagsBits keys (default View Channel and Send
/// Messages); `path` the field the refusal is about; `optional` a channel the bot cannot find passes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelCheck {
    pub channel_id: Option<String>,
    pub needs: Option<Vec<String>>,
    pub path: Option<String>,
    pub optional: bool,
}

impl ChannelCheck {
    /// A check of `channel_id` with the default needs.
    pub fn new(channel_id: impl Into<String>) -> Self {
        Self { channel_id: Some(channel_id.into()), ..Self::default() }
    }

    /// With needs.
    pub fn needs(mut self, needs: &[&str]) -> Self {
        self.needs = Some(needs.iter().map(|s| (*s).to_string()).collect());
        self
    }

    /// With the field the refusal is about.
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// A channel the bot cannot find passes.
    pub fn optional(mut self) -> Self {
        self.optional = true;
        self
    }

    /// The task payload entry (`{ channelId, needs?, optional? }`).
    fn payload(&self) -> Value {
        let mut m = Map::new();
        m.insert("channelId".into(), json!(self.channel_id));
        if let Some(n) = &self.needs {
            m.insert("needs".into(), json!(n));
        }
        if self.optional {
            m.insert("optional".into(), json!(true));
        }
        Value::Object(m)
    }
}

const NO_BOT: &str = "The bot cannot be reached from the dashboard right now, so your access to the channel cannot be checked.";
const NO_ANSWER: &str = "The bot could not check your access to the channel in time. Nothing was changed; try again in a moment.";

/// The task payload `requireChannelAccess` sends (`{ checks: [...] }`), for the checks with a channel.
pub fn access_payload(checks: &[ChannelCheck]) -> Value {
    let wanted: Vec<Value> = checks.iter().filter(|c| c.channel_id.as_deref().is_some_and(|id| !id.is_empty())).map(ChannelCheck::payload).collect();
    json!({ "checks": wanted })
}

/// JS truthiness of a JSON value.
fn js_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// The array index `wanted[index]` reads: a whole non-negative number, or a string that is one in canonical form
/// (`'0'`, `'12'`; not `'01'` or `'1.0'`), as JS property keys work.
fn array_index(v: &Value) -> Option<usize> {
    match v {
        Value::Number(n) => n.as_f64().filter(|i| i.fract() == 0.0 && *i >= 0.0 && *i < 4_294_967_295.0).map(|i| i as usize),
        Value::String(s) => {
            let ok = s == "0" || (!s.is_empty() && !s.starts_with('0') && s.len() <= 10 && s.bytes().all(|b| b.is_ascii_digit()));
            if ok { s.parse::<u64>().ok().filter(|i| *i < 4_294_967_295).map(|i| i as usize) } else { None }
        }
        _ => None,
    }
}

/// `requireChannelAccess(ctx, checks, { lead })`: refuses the write when the dashboard user cannot use a channel the
/// way the write will; the first failed check fails. `lead` is put before the bot's refusal.
/// Errors: RecordError invalid (a channel the user cannot use), failed (the bot refused, e.g. the user left the
/// server) or unavailable (the check could not be made: nothing is written); a task-runner failure is Internal.
pub async fn require_channel_access(ctx: &StoreCtx, checks: &[ChannelCheck], lead: &str) -> Result<(), StoreError> {
    let wanted: Vec<&ChannelCheck> = checks.iter().filter(|c| c.channel_id.as_deref().is_some_and(|id| !id.is_empty())).collect();
    if wanted.is_empty() || is_channel_exempt(ctx.member.as_ref()) {
        return Ok(());
    }
    if !ctx.can_run_tasks() {
        return Err(RecordError::unavailable(NO_BOT).into());
    }
    let result = ctx.run_task("access.channels", access_payload(checks)).await?;
    let out = task_outcome(result.as_ref())?;
    let problems = match (&out.status, out.data.as_ref().and_then(|d| d.get("problems")).and_then(Value::as_array)) {
        (TaskStatus::Done, Some(p)) => p.clone(),
        _ => return Err(RecordError::unavailable(NO_ANSWER).into()),
    };
    // `if (!first) return;`: a falsy first entry (null, false, 0, '') means no problem.
    let Some(first) = problems.first().filter(|f| js_truthy(f)) else { return Ok(()) };
    let path = first
        .get("index")
        .and_then(array_index)
        .and_then(|i| wanted.get(i))
        .and_then(|c| c.path.clone())
        .filter(|p| !p.is_empty());
    let text = match first.get("text") {
        None | Some(Value::Null) => "You cannot use that channel.".to_string(),
        Some(t) => js_string(Some(t)),
    };
    let message = slice16(&format!("{lead}{text}"), 300).to_string();
    let mut err = RecordError::invalid(message);
    if let Some(p) = path {
        err = err.with_path(p);
    }
    Err(err.into())
}
