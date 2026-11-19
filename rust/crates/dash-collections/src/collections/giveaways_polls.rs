//! Port of `src/dashboard/collections/giveaways.js` and `polls.js`: the Giveaway and Poll rows behind /giveaway and
//! /poll (every write goes through a dashboard task), their cross-field rules and their list items.

use std::collections::HashMap;

use bot_util::parse_duration::parse_duration;
use bot_util::text::{clamp_text, percentages};
use serde_json::{Map, Value, json};

use crate::collections::embeds::message_url;
use crate::index::{CheckCtx, Mode, Problem};
use crate::util::{raw, strip_undefined, as_list, int_of, iso, js_string, list_has, or_null, truthy, utc_text};

// ── giveaways.js ──────────────────────────────────────────────────────────────────────────────────────────────

pub const MIN_DURATION_MS: f64 = 60_000.0;
pub const MAX_DURATION_MS: f64 = 90.0 * 86_400_000.0;
pub const MAX_ROLE_WEIGHTS: usize = 10;
pub const MIN_BONUS_WEIGHT: i64 = 2;
pub const MAX_WEIGHT: i64 = 10;
pub const DEFAULT_BONUS_WEIGHT: i64 = 2;
pub const MAX_PRIZE_AMOUNT: i64 = 1_000_000;
pub const PRIZE_MAX: usize = 256;
pub const DESCRIPTION_MAX: usize = 1000;
/// Most required (or denied) roles one giveaway lists.
pub const MAX_GATE_ROLES: usize = 10;

/// `STATUS_LABELS` of giveaways: row status -> label.
pub const GIVEAWAY_STATUS_LABELS: [(&str, &str); 4] = [("ACTIVE", "Running"), ("DRAFT", "Draft"), ("ENDED", "Ended"), ("CANCELLED", "Cancelled")];

fn label_of(table: &[(&str, &'static str)], status: Option<&Value>) -> String {
    let s = status.and_then(Value::as_str);
    s.and_then(|s| table.iter().find(|(k, _)| *k == s)).map_or_else(|| js_string(status), |(_, l)| (*l).to_string())
}

/// `endTimeProblem(data, { required, now })`: the rules of the length / end time pair.
pub fn end_time_problem(data: &Map<String, Value>, required: bool, now_ms: f64) -> Option<Problem> {
    let duration = data.get("duration").and_then(Value::as_str).map(bot_util::js::trim).filter(|s| !s.is_empty());
    let ends_at = data.get("endsAt").filter(|v| !v.is_null());
    if duration.is_none() && !truthy(ends_at) {
        return required.then(|| Problem::new("duration", "Set how long the giveaway runs, or when it ends."));
    }
    if duration.is_some() && truthy(ends_at) {
        return Some(Problem::new("endsAt", "Use either the length or the end time, not both."));
    }
    let ms = if let Some(d) = duration {
        match parse_duration(d) {
            Some(ms) => ms as f64,
            None => return Some(Problem::new("duration", "Use a length like 30m, 2h, 3d or 1w.")),
        }
    } else {
        let t = match ends_at {
            Some(Value::String(s)) => bot_util::js::date_parse(s),
            Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
            _ => f64::NAN,
        };
        let ms = t - now_ms;
        if !ms.is_finite() {
            return Some(Problem::new("endsAt", "Pick a valid end time."));
        }
        if ms <= 0.0 {
            return Some(Problem::new("endsAt", "That end time is in the past."));
        }
        ms
    };
    let path = if duration.is_some() { "duration" } else { "endsAt" };
    if ms < MIN_DURATION_MS {
        return Some(Problem::new(path, "A giveaway must run for at least 1 minute."));
    }
    if ms > MAX_DURATION_MS {
        return Some(Problem::new(path, "A giveaway can run for at most 90 days."));
    }
    None
}

/// A role may carry bonus odds once.
fn odds_problem(list: Option<&Value>) -> Option<Problem> {
    let list = list?.as_array()?;
    let mut seen: Vec<Value> = Vec::new();
    for (i, w) in list.iter().enumerate() {
        let role = w.get("roleId").cloned().unwrap_or(Value::String("\u{0}undefined".into()));
        if list_has(&seen, &role) {
            return Some(Problem::new(format!("roleWeights.{i}.roleId"), "Each role can have bonus odds only once."));
        }
        seen.push(role);
    }
    None
}

/// `checkGiveaway(data, { mode })`: what /giveaway start and /giveaway odds refuse beyond the field rules.
pub fn check_giveaway(data: &Map<String, Value>, ctx: &CheckCtx) -> Option<Problem> {
    if ctx.mode == Mode::Update {
        return odds_problem(data.get("roleWeights"));
    }
    if let Some(p) = end_time_problem(data, true, ctx.now_ms) {
        return Some(p);
    }
    let denied = as_list(data.get("deniedRoleIds"));
    if as_list(data.get("requiredRoleIds")).iter().any(|id| list_has(denied, id)) {
        return Some(Problem::new("deniedRoleIds", "The same role cannot be both required and blocked."));
    }
    odds_problem(data.get("roleWeights"))
}

/// The `check` of the `restart` action: `endTimeProblem(data, { required: false })`.
pub fn check_giveaway_restart(data: &Map<String, Value>, ctx: &CheckCtx) -> Option<Problem> {
    end_time_problem(data, false, ctx.now_ms)
}

fn plural(n: i64, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

fn entries_of(row: &Value) -> Value {
    row.get("_count").and_then(|c| c.get("entries")).filter(|v| !v.is_null()).cloned().unwrap_or(json!(0))
}

/// `giveawaySummary(row)`: the list line under a giveaway's prize.
pub fn giveaway_summary(row: &Value) -> String {
    let n = entries_of(row);
    let entries = if n.as_f64() == Some(1.0) { "1 entry".to_string() } else { format!("{} entries", js_string(Some(&n))) };
    match row.get("status").and_then(Value::as_str) {
        Some("ACTIVE") => format!("Running, ends {} · {entries}", utc_text(row.get("endsAt"))),
        Some("DRAFT") => format!("Draft (its message was deleted) · {entries}"),
        Some("ENDED") => {
            let ended = row.get("endedAt").filter(|v| !v.is_null()).or(row.get("endsAt"));
            let winners = row.get("winnerIds").and_then(Value::as_array).map_or(0, Vec::len) as i64;
            format!("Ended {} · {} · {entries}", utc_text(ended), plural(winners, "winner"))
        }
        _ => format!("Cancelled · {entries}"),
    }
}

/// `giveawayItem(row, { names })`: a Giveaway row (with `_count.entries`) as a collection item. `names`: display names
/// of the host and the winners by user id.
pub fn giveaway_item(row: &Value, names: &HashMap<String, String>) -> Value {
    let name_of = |id: Option<&Value>| match id.and_then(Value::as_str).and_then(|s| names.get(s)) {
        Some(n) => json!(n),
        None => raw(id),
    };
    let weights: Vec<Value> = as_list(row.get("roleWeights"))
        .iter()
        .filter(|w| w.get("roleId").is_some_and(Value::is_string) && int_of(w.get("weight")).is_some())
        .map(|w| json!({ "roleId": w["roleId"], "weight": w["weight"] }))
        .collect();
    let positive = |k: &str| match int_of(row.get(k)) {
        Some(n) if n > 0 => row.get(k).cloned().unwrap_or(Value::Null),
        _ => Value::Null,
    };
    strip_undefined(json!({
        "id": raw(row.get("id")),
        "prize": raw(row.get("prize")),
        "description": or_null(row.get("description")),
        "channelId": raw(row.get("channelId")),
        "winnerCount": raw(row.get("winnerCount")),
        "duration": null,
        "endsAt": iso(row.get("endsAt")),
        "requiredRoleIds": as_list(row.get("requiredRoleIds")),
        "deniedRoleIds": as_list(row.get("deniedRoleIds")),
        "roleWeights": weights,
        "xpPrize": positive("xpPrize"),
        "coinPrize": positive("coinPrize"),
        "pingRoleId": or_null(row.get("pingRoleId")),
        "embedColor": positive("embedColor"),
        "status": label_of(&GIVEAWAY_STATUS_LABELS, row.get("status")),
        "summary": giveaway_summary(row),
        "host": name_of(row.get("hostId")),
        "entries": entries_of(row),
        "winners": as_list(row.get("winnerIds")).iter().map(|w| name_of(Some(w))).collect::<Vec<_>>(),
        "endedAt": iso(row.get("endedAt")),
        "createdAt": iso(row.get("createdAt")),
        "messageUrl": if truthy(row.get("messageId")) { message_url(row.get("guildId"), row.get("channelId"), row.get("messageId")) } else { None },
    }))
}

// ── polls.js ──────────────────────────────────────────────────────────────────────────────────────────────────

pub const QUESTION_MAX: usize = 256;
pub const OPTION_MAX: usize = 80;
pub const OPTIONS_MIN: usize = 2;
pub const DURATION_MIN_MS: i64 = 60_000;
pub const DURATION_MAX_MS: i64 = 30 * 86_400_000;
/// Most allowed (or blocked) roles one poll lists.
pub const POLL_MAX_GATE_ROLES: usize = 10;
/// `STATUS_LABELS` of polls.
pub const POLL_STATUS_LABELS: [(&str, &str); 2] = [("ACTIVE", "Active"), ("ENDED", "Ended")];
/// /poll create accepts these channel kinds (and threads, which the dashboard never offers).
pub const POLL_CHANNEL_KINDS: [&str; 3] = ["text", "announcement", "voice"];

/// `durationProblem(duration)` of polls.js: the rules of the length (parseDurationOption in pollService.js).
pub fn poll_duration_problem(duration: Option<&Value>) -> Option<Problem> {
    let text = duration.and_then(Value::as_str).map(bot_util::js::trim).unwrap_or("");
    if text.is_empty() {
        return None;
    }
    let Some(ms) = parse_duration(text) else {
        return Some(Problem::new("duration", "Use a length like 30m, 2h, 1d or 1w."));
    };
    if ms < DURATION_MIN_MS {
        return Some(Problem::new("duration", "Polls must run for at least 1 minute."));
    }
    if ms > DURATION_MAX_MS {
        return Some(Problem::new("duration", "Polls can run for at most 30 days."));
    }
    None
}

/// `checkPoll(data)`: what /poll create refuses beyond the field rules.
pub fn check_poll(data: &Map<String, Value>, _ctx: &CheckCtx) -> Option<Problem> {
    let options_len = match data.get("options") {
        None | Some(Value::Null) => 0,
        Some(Value::Array(a)) => a.len(),
        Some(Value::String(s)) => crate::util::len16(s),
        Some(_) => usize::MAX,
    };
    if options_len < OPTIONS_MIN {
        return Some(Problem::new("options", format!("Add at least {OPTIONS_MIN} different options (a repeated option counts once).")));
    }
    if let Some(p) = poll_duration_problem(data.get("duration")) {
        return Some(p);
    }
    let denied = as_list(data.get("deniedRoleIds"));
    if as_list(data.get("allowedRoleIds")).iter().any(|id| list_has(denied, id)) {
        return Some(Problem::new("deniedRoleIds", "The allowed roles and the blocked roles must be different."));
    }
    None
}

/// The vote counts of a poll (`{ counts, votes, voters }` of the store's tally).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PollTally {
    pub counts: Vec<i64>,
    pub votes: i64,
    pub voters: i64,
}

fn lead_text(options: &[Value], counts: &[i64], votes: i64, ended: bool) -> Option<String> {
    let top = counts.iter().copied().fold(0, i64::max);
    if votes == 0 || top == 0 {
        return None;
    }
    let names: Vec<String> = options
        .iter()
        .enumerate()
        .filter(|(i, _)| counts.get(*i) == Some(&top))
        .map(|(_, o)| clamp_text(&js_string(Some(o)), 40))
        .collect();
    if names.len() == 1 {
        let pct = percentages(&counts.iter().map(|c| *c as f64).collect::<Vec<_>>());
        let at = counts.iter().position(|c| *c == top).unwrap_or(0);
        return Some(format!("{}: {} ({}%)", if ended { "won" } else { "leading" }, names[0], pct.get(at).copied().unwrap_or(0)));
    }
    let more = if names.len() > 3 { format!(" +{}", names.len() - 3) } else { String::new() };
    Some(format!("tie: {}{more}", names.iter().take(3).cloned().collect::<Vec<_>>().join(", ")))
}

/// `pollSummary(row, tally)`: the list line under a poll's question.
pub fn poll_summary(row: &Value, votes: i64, counts: &[i64]) -> String {
    let ended = row.get("status").and_then(Value::as_str) == Some("ENDED");
    let first = if ended {
        format!("Ended {}", utc_text(row.get("endsAt")))
    } else if truthy(row.get("endsAt")) {
        format!("Active, ends {}", utc_text(row.get("endsAt")))
    } else {
        "Active, open until ended".to_string()
    };
    let mut parts = vec![first, if votes == 1 { "1 vote".to_string() } else { format!("{votes} votes") }];
    if let Some(l) = lead_text(as_list(row.get("options")), counts, votes, ended) {
        parts.push(l);
    }
    parts.join(" · ")
}

/// `pollItem(row, { tally, names })`: a Poll row as a collection item.
pub fn poll_item(row: &Value, tally: Option<&PollTally>, names: &HashMap<String, String>) -> Value {
    let options = as_list(row.get("options"));
    let counts: Vec<i64> = (0..options.len()).map(|i| tally.and_then(|t| t.counts.get(i).copied()).unwrap_or(0)).collect();
    let votes = tally.map_or(0, |t| t.votes).max(counts.iter().sum());
    let shown = percentages(&counts.iter().map(|c| *c as f64).collect::<Vec<_>>());
    let results: Vec<String> = options
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let c = counts[i];
            format!("{}. {}: {} ({}%)", i + 1, js_string(Some(o)), if c == 1 { "1 vote".to_string() } else { format!("{c} votes") }, shown[i])
        })
        .collect();
    let author = match row.get("authorId").and_then(Value::as_str).and_then(|id| names.get(id)) {
        Some(n) => json!(n),
        None => raw(row.get("authorId")),
    };
    strip_undefined(json!({
        "id": raw(row.get("id")),
        "question": raw(row.get("question")),
        "options": options,
        "channelId": raw(row.get("channelId")),
        "maxChoices": raw(row.get("maxChoices")),
        "duration": null,
        "allowedRoleIds": as_list(row.get("allowedRoleIds")),
        "deniedRoleIds": as_list(row.get("deniedRoleIds")),
        "hideResults": row.get("hideResults") == Some(&Value::Bool(true)),
        "status": label_of(&POLL_STATUS_LABELS, row.get("status")),
        "summary": poll_summary(row, votes, &counts),
        "results": results,
        "votes": votes,
        "voters": tally.map_or(0, |t| t.voters),
        "endsAt": iso(row.get("endsAt")),
        "author": author,
        "createdAt": iso(row.get("createdAt")),
        "messageUrl": if truthy(row.get("messageId")) { message_url(row.get("guildId"), row.get("channelId"), row.get("messageId")) } else { None },
    }))
}
