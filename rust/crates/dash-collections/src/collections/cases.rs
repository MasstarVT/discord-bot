//! Port of `src/dashboard/collections/cases.js`: the `cases` collection (Moderation): the server's moderation cases,
//! newest first; creating one performs the action through the bot (task `mod.action`).

use std::collections::HashMap;

use bot_util::parse_duration::{format_duration, parse_duration};
use serde_json::{Map, Value, json};

use crate::collections::levels::{avatar_for, profile_label, profile_name};
use crate::discord::MemberProfile;
use crate::index::{CheckCtx, Mode, Problem};
use crate::util::{raw, strip_undefined, date_ms, iso, iso_string, js_string, len16, slice16, truthy};

/// Longest reason the action commands accept (their `reason` option).
pub const COMMAND_REASON_MAX: usize = 512;
/// Longest stored reason and proof link (/case reason and proof).
pub const REASON_MAX: usize = 1000;
pub const TIMED_ACTION_MIN_MS: i64 = 60_000;
pub const TIMED_ACTION_MAX_MS: i64 = 31_536_000_000;
pub const DELETE_DAYS_MAX: i64 = 7;
pub const MAX_CASE_ID: i64 = 2_147_483_647;

/// `CASE_ACTIONS` (value, label, description): what the dashboard can do, in the form's order.
pub const CASE_ACTIONS: [(&str, &str, &str); 7] = [
    ("WARN", "Warn", "Records a warning and DMs the member (/warn). Warnings can trigger your automatic punishments."),
    ("TIMEOUT", "Timeout", "Times the member out (/timeout). Over 28 days uses the mute role, when one is set."),
    ("UNMUTE", "Remove timeout or mute", "Removes the member's timeout and mute role (/unmute)."),
    ("KICK", "Kick", "Kicks the member (/kick). They can rejoin with an invite."),
    ("SOFTBAN", "Softban", "Bans and immediately unbans to delete their recent messages (/softban)."),
    ("BAN", "Ban", "Bans the user, also when they already left (/ban). Give a duration for a temporary ban."),
    ("UNBAN", "Unban", "Lifts a ban (/unban)."),
];
/// Actions whose command takes a proof link (/unban and /unmute have none).
pub const PROOF_ACTIONS: [&str; 5] = ["WARN", "TIMEOUT", "KICK", "SOFTBAN", "BAN"];
/// `CASE_TYPES` (value, label): labels of every case type.
pub const CASE_TYPES: [(&str, &str); 10] = [
    ("WARN", "Warn"),
    ("TIMEOUT", "Timeout"),
    ("MUTE", "Mute"),
    ("KICK", "Kick"),
    ("SOFTBAN", "Softban"),
    ("BAN", "Ban"),
    ("TEMPBAN", "Temporary ban"),
    ("UNBAN", "Unban"),
    ("UNMUTE", "Unmute"),
    ("NOTE", "Note"),
];
/// `CASE_STATUSES` (value, label).
pub const CASE_STATUSES: [(&str, &str); 5] = [
    ("active", "Active"),
    ("ending", "Expired (the bot is lifting it)"),
    ("expired", "Expired"),
    ("lifted", "Lifted"),
    ("pardoned", "Pardoned"),
];

fn type_label(t: &str) -> Option<&'static str> {
    CASE_TYPES.iter().find(|(v, _)| *v == t).map(|(_, l)| *l)
}

fn status_label(s: &str) -> Option<&'static str> {
    CASE_STATUSES.iter().find(|(v, _)| *v == s).map(|(_, l)| *l)
}

/// `durationProblem(action, text, out)`: a duration problem of the create form, or the parsed ms.
fn duration_problem(action: Option<&str>, text: Option<&Value>) -> Result<Option<i64>, Problem> {
    if action != Some("TIMEOUT") && action != Some("BAN") {
        return Ok(None);
    }
    let Some(text) = text.filter(|t| !t.is_null()) else {
        return if action == Some("TIMEOUT") {
            Err(Problem::new("duration", "A timeout needs a duration, e.g. 10m, 2h or 7d."))
        } else {
            Ok(None)
        };
    };
    let s = js_string(Some(text));
    let ms = parse_duration(&s).filter(|m| *m != 0);
    let Some(ms) = ms else {
        return Err(Problem::new("duration", format!("Couldn't read \"{}\". Use something like 12h, 7d or 1mo.", slice16(&s, 32))));
    };
    if ms < TIMED_ACTION_MIN_MS {
        return Err(Problem::new("duration", "Durations must be at least 1 minute."));
    }
    if ms > TIMED_ACTION_MAX_MS {
        let m = if action == Some("BAN") { "Temporary bans can last at most 1 year." } else { "Mutes can last at most 1 year." };
        return Err(Problem::new("duration", m));
    }
    Ok(Some(ms))
}

/// `actionPayload(data)`: the task payload of a create (the fields the chosen action takes).
pub fn action_payload(data: &Map<String, Value>) -> Value {
    let action = data.get("action").and_then(Value::as_str);
    let ms = duration_problem(action, data.get("duration")).ok().flatten();
    let days = |k: &str, d: i64| match data.get(k).filter(|v| !v.is_null()) {
        Some(v) => v.as_f64().unwrap_or(f64::NAN),
        None => d as f64,
    };
    let delete_days = match action {
        Some("BAN") => Some(days("deleteDays", 0)),
        Some("SOFTBAN") => Some(days("softbanDeleteDays", DELETE_DAYS_MAX)),
        _ => None,
    };
    let proof = if action.is_some_and(|a| PROOF_ACTIONS.contains(&a)) {
        data.get("proofUrl").filter(|v| !v.is_null()).cloned().unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    json!({
        "action": data.get("action").cloned().unwrap_or(Value::Null),
        "userId": data.get("userId").cloned().unwrap_or(Value::Null),
        "reason": data.get("reason").filter(|v| !v.is_null()).cloned().unwrap_or(Value::Null),
        "durationMs": ms,
        "deleteMessageSeconds": delete_days.map(|d| dash_schema::js::js_num(d * 86_400.0)),
        "proofUrl": proof,
    })
}

/// The `check` of cases: the rules of the commands that the field types cannot express.
pub fn check_case(data: &Map<String, Value>, ctx: &CheckCtx) -> Option<Problem> {
    match ctx.mode {
        Mode::Create => {
            let action = data.get("action").and_then(Value::as_str);
            if !action.is_some_and(|a| CASE_ACTIONS.iter().any(|(v, _, _)| *v == a)) {
                return Some(Problem::new("action", "Pick an action."));
            }
            if action == Some("WARN") && !truthy(data.get("reason")) {
                return Some(Problem::new("reason", "A warning needs a reason."));
            }
            if let Some(Value::String(r)) = data.get("reason")
                && !r.is_empty()
                && len16(r) > COMMAND_REASON_MAX
            {
                return Some(Problem::new("reason", format!("The reason can be at most {COMMAND_REASON_MAX} characters.")));
            }
            duration_problem(action, data.get("duration")).err()
        }
        Mode::Update => {
            if data.contains_key("reason") && !truthy(data.get("reason")) {
                return Some(Problem::new("reason", "The reason can't be empty."));
            }
            if data.get("proofUrl") == Some(&Value::Null) && ctx.current.is_some_and(|c| truthy(c.get("proofUrl"))) {
                return Some(Problem::new("proofUrl", "A proof link can be replaced but not removed."));
            }
            None
        }
        Mode::Action => None,
    }
}

/// `caseType(row)`: a BAN with an end is a temporary ban.
pub fn case_type(row: &Value) -> Value {
    if row.get("type").and_then(Value::as_str) == Some("BAN") && truthy(row.get("expiresAt")) {
        json!("TEMPBAN")
    } else {
        row.get("type").cloned().unwrap_or(Value::Null)
    }
}

/// `caseStatus(row, now)`: active, ending, expired, lifted or pardoned; None for one-off actions.
pub fn case_status(row: &Value, now_ms: f64) -> Option<&'static str> {
    let ty = row.get("type").and_then(Value::as_str);
    let active = truthy(row.get("active"));
    if ty == Some("WARN") {
        return Some(if active { "active" } else { "pardoned" });
    }
    if !matches!(ty, Some("TIMEOUT" | "MUTE" | "BAN")) {
        return None;
    }
    let ended = date_ms(row.get("expiresAt")).is_some_and(|t| t <= now_ms);
    Some(match (active, ended) {
        (true, true) => "ending",
        (true, false) => "active",
        (false, true) => "expired",
        (false, false) => "lifted",
    })
}

fn action_of_type(t: &str) -> Option<&'static str> {
    Some(match t {
        "WARN" => "WARN",
        "TIMEOUT" | "MUTE" => "TIMEOUT",
        "UNMUTE" => "UNMUTE",
        "KICK" => "KICK",
        "SOFTBAN" => "SOFTBAN",
        "BAN" => "BAN",
        "UNBAN" => "UNBAN",
        _ => return None,
    })
}

fn case_duration(row: &Value) -> Option<String> {
    let exp = date_ms(row.get("expiresAt"))?;
    let created = date_ms(row.get("createdAt"))?;
    let ms = dash_schema::js::js_round((exp - created) / 60_000.0) * 60_000.0;
    (ms > 0.0).then(|| format_duration(ms))
}

/// `reasonText(reason)`: a stored reason on one line.
pub fn reason_text(reason: Option<&Value>) -> String {
    let s = match reason {
        None | Some(Value::Null) => String::new(),
        Some(v) => js_string(Some(v)),
    };
    // /\s*\n\s*/g -> ' '
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if bot_util::js::is_js_space(chars[i]) {
            let mut j = i;
            while j < chars.len() && bot_util::js::is_js_space(chars[j]) && chars[j] != '\n' {
                j += 1;
            }
            if j < chars.len() && chars[j] == '\n' {
                // The greedy \s* before \n may also have eaten earlier newlines: the leftmost match starts at i.
                let mut k = j + 1;
                while k < chars.len() && bot_util::js::is_js_space(chars[k]) {
                    k += 1;
                }
                out.push(' ');
                i = k;
                continue;
            }
            // No newline reachable through spaces only; but a later \n within the run of white space counts too.
            let mut k = i;
            let mut last_nl = None;
            while k < chars.len() && bot_util::js::is_js_space(chars[k]) {
                if chars[k] == '\n' {
                    last_nl = Some(k);
                }
                k += 1;
            }
            if last_nl.is_some() {
                out.push(' ');
                i = k;
                continue;
            }
            out.extend(&chars[i..k]);
            i = k;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    let t = bot_util::js::trim(&out);
    if t.is_empty() { "No reason provided".into() } else { t.to_string() }
}

/// `caseItem(row, { profiles, now })`: an Infraction row as a list item.
pub fn case_item(row: &Value, profiles: &HashMap<String, Option<MemberProfile>>, now_ms: f64) -> Value {
    let target_id = row.get("targetUserId").and_then(Value::as_str);
    let moderator_id = row.get("moderatorId").and_then(Value::as_str);
    let target = target_id.and_then(|id| profiles.get(id)).and_then(Option::as_ref);
    let moderator = moderator_id.and_then(|id| profiles.get(id)).and_then(Option::as_ref);
    let ty = case_type(row);
    let ty_s = js_string(Some(&ty));
    let status = case_status(row, now_ms);
    let created = iso_string(row.get("createdAt"));
    let case_id = js_string(row.get("caseId"));
    let summary: Vec<String> = [
        Some(profile_name(target, target_id)),
        status.and_then(status_label).map(str::to_string),
        created.as_ref().map(|c| c[..10].to_string()),
    ]
    .into_iter()
    .flatten()
    .filter(|s| !s.is_empty())
    .collect();
    strip_undefined(json!({
        "id": case_id,
        "action": row.get("type").and_then(Value::as_str).and_then(action_of_type),
        "userId": raw(row.get("targetUserId")),
        "reason": reason_text(row.get("reason")),
        "duration": case_duration(row),
        "deleteDays": null,
        "softbanDeleteDays": null,
        "proofUrl": row.get("proofUrl").filter(|v| !v.is_null()).cloned().unwrap_or(Value::Null),
        "avatarUrl": avatar_for(target, target_id.unwrap_or_default()),
        "title": format!("#{case_id} · {}", type_label(&ty_s).map_or(ty_s.clone(), str::to_string)),
        "summary": summary.join(" · "),
        "caseNumber": raw(row.get("caseId")),
        "type": ty,
        "member": profile_label(target, &js_string(row.get("targetUserId"))),
        "moderator": profile_label(moderator, &js_string(row.get("moderatorId"))),
        "status": status,
        "createdAt": created,
        "expiresAt": iso(row.get("expiresAt")),
    }))
}

/// `caseNumberQuery(query)`: a case number typed in the search box ("12" or "#12"), or None.
pub fn case_number_query(query: &str) -> Option<i64> {
    let q = bot_util::js::trim(query);
    let digits = q.strip_prefix('#').unwrap_or(q);
    if !(1..=10).contains(&digits.len()) || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: i64 = digits.parse().ok()?;
    (1..=MAX_CASE_ID).contains(&n).then_some(n)
}
