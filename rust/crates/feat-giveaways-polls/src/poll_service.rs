//! Port of `src/services/pollService.js`: MEE6 `/poll` + `/poll-end`. Modal-built polls with 2-20 options, persistent
//! button (single choice) or select (multi choice) voting, role allow/deny checked at vote time, atomic votes (advisory
//! lock per (poll, user) + `FOR SHARE` on the poll row), live results throttled to one edit per poll per 5 s and a
//! scheduler-timed end (`poll.end`). Everything durable is in PostgreSQL; the in-process state (the pending create
//! forms, the render throttle, the render lock) lives in `app.ext` and may vanish on a restart, as in the JS.
//!
//! The polls are the bot's own embed + components messages, as in the JS (not Discord's native poll object), so every
//! message body is built here as the JSON Discord receives and sent through [`crate::discord_ops`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, RwLock};

use bot_config::{ModuleKey, custom_id, limits};
use bot_core::embed_builder::{error_embed, guild_embed, success_embed};
use bot_core::interaction_helpers::{CHANNEL_POST_NEEDS, Payload, cached_channel, is_interaction_gone, safe_defer, safe_reply};
use bot_core::permissions::{is_thread, permission_names};
use bot_core::prelude::*;
use bot_db::sqlx::{self, Row};
use bot_db::{GuildSettings, InsertBuilder, POLL_INFO, POLL_VOTE_INFO, Poll, PollStatus, TxOptions};
use bot_util::text::{clamp_text, format_number, join_with_limit_default, percentages, progress_bar};
use bot_util::time::discord_timestamp;
use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use serenity::all::{ActionRowComponent, ButtonStyle, GuildChannel, InputTextStyle, Member, MessageFlags, Permissions, Timestamp};
use serenity::builder::{
    CreateActionRow, CreateButton, CreateEmbed, CreateInputText, CreateModal, CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption,
};

use crate::discord_ops::{DiscordError, ops};
use crate::giveaway_service::{embed_json, err_arg, now_of, row_json};
use crate::guild_view::{self, channel_id_of, guild_id_of};

// ── Constants ─────────────────────────────────────────────────────────────────

/// `POLL`.
pub mod poll {
    pub const QUESTION_MAX: usize = 256;
    pub const OPTION_MAX: usize = 80;
    pub const OPTIONS_MIN: usize = 2;
    pub const OPTIONS_MAX: usize = bot_config::limits::POLL_OPTIONS;
    pub const ACTIVE_MAX: usize = bot_config::limits::ACTIVE_POLLS;
    pub const OPTIONS_INPUT_MAX: u16 = 2000;
    pub const DURATION_MIN_MS: i64 = 60_000;
    pub const DURATION_MAX_MS: i64 = 30 * 86_400_000;
    pub const PENDING_TTL_MS: i64 = 15 * 60_000;
    pub const PENDING_MAX: usize = 500;
    pub const RENDER_INTERVAL_MS: i64 = 5_000;
    pub const BUTTONS_PER_ROW: usize = 5;
    pub const BAR_WIDTH: f64 = 10.0;
}

pub const END_JOB_TYPE: &str = "poll.end";
/// `endJobKey(pollId)`.
pub fn end_job_key(poll_id: &str) -> String {
    format!("poll.end:{poll_id}")
}

const RENDER_SWEEP_AT: usize = 500;
const RENDER_IDLE_MS: i64 = 10 * 60_000;
const WARN_EVERY_MS: i64 = 10 * 60_000;

const UNKNOWN_CHANNEL: u64 = 10003;
const UNKNOWN_MESSAGE: u64 = 10008;
fn no_access_code(code: Option<u64>) -> bool {
    matches!(code, Some(50001) | Some(50013) | Some(50083))
}

/// `{ timeout: 10_000, maxWait: 10_000 }`.
const TX_OPTS: TxOptions = TxOptions::TEN_SECONDS;

// ── Raw SQL (no timestamps are involved, so no SQL_NOW) ─────────────────────────

const CREATE_LOCK_SQL: &str = "SELECT 1 AS ok FROM pg_advisory_xact_lock(hashtext($1))";
const VOTE_LOCK_SQL: &str = "SELECT 1 AS ok FROM pg_advisory_xact_lock(hashtext($1))";
const POLL_STATUS_SQL: &str = r#"SELECT status::text AS status FROM "Poll" WHERE id = $1 FOR SHARE"#;
const TALLY_TOTALS_SQL: &str = r#"SELECT COUNT(*)::int AS votes, COUNT(DISTINCT "userId")::int AS voters FROM "PollVote" WHERE "pollId" = $1"#;

/// `RAW_SQL`: one entry per raw statement, with its sample parameter (`'0'`, or `'0:0'` keys for the locks).
pub const RAW_SQL: &[(&str, &str, &str)] = &[
    ("polls.createLock", CREATE_LOCK_SQL, "poll-create:0"),
    ("polls.voteLock", VOTE_LOCK_SQL, "poll-vote:0:0"),
    ("polls.status", POLL_STATUS_SQL, "0"),
    ("polls.totals", TALLY_TOTALS_SQL, "0"),
];

const SQL_BY_ID: &str = r#"SELECT * FROM "Poll" WHERE "id" = $1"#;
const SQL_BY_ID_GUILD: &str = r#"SELECT * FROM "Poll" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#;
const SQL_COUNT_ACTIVE: &str = r#"SELECT COUNT(*)::bigint AS n FROM "Poll" WHERE "guildId" = $1 AND "status" = 'ACTIVE'"#;
const SQL_GROUPS: &str = r#"SELECT "optionIndex", COUNT(*)::bigint AS n FROM "PollVote" WHERE "pollId" = $1 GROUP BY "optionIndex""#;
const SQL_ACTIVE_CHANNELS: &str = r#"SELECT "id", "channelId" FROM "Poll" WHERE "guildId" = $1 AND "status" = 'ACTIVE'"#;
const SQL_DELETE: &str = r#"DELETE FROM "Poll" WHERE "id" = $1"#;
const SQL_SET_MESSAGE: &str = r#"UPDATE "Poll" SET "messageId" = $2, "updatedAt" = $3 WHERE "id" = $1"#;
const SQL_USER_VOTES: &str = r#"SELECT "optionIndex" FROM "PollVote" WHERE "pollId" = $1 AND "userId" = $2"#;
const SQL_DELETE_VOTES: &str = r#"DELETE FROM "PollVote" WHERE "pollId" = $1 AND "userId" = $2 AND "optionIndex" = ANY($3)"#;
const SQL_END: &str = r#"UPDATE "Poll" SET "status" = 'ENDED', "endsAt" = $2, "updatedAt" = $2 WHERE "id" = $1 AND "status" = 'ACTIVE'"#;
const SQL_END_IN_GUILD: &str =
    r#"UPDATE "Poll" SET "status" = 'ENDED', "endsAt" = $2, "updatedAt" = $2 WHERE "id" = $1 AND "status" = 'ACTIVE' AND "guildId" = $3"#;
const SQL_EXISTS: &str = r#"SELECT "id" FROM "Poll" WHERE "id" = $1 LIMIT 1"#;
const SQL_EXISTS_IN_GUILD: &str = r#"SELECT "id" FROM "Poll" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#;
const SQL_ACTIVE_BY_MESSAGE: &str = r#"SELECT "id" FROM "Poll" WHERE "guildId" = $1 AND "messageId" = $2 AND "status" = 'ACTIVE' LIMIT 1"#;
const SQL_ACTIVE_BY_MESSAGES: &str = r#"SELECT "id" FROM "Poll" WHERE "guildId" = $1 AND "status" = 'ACTIVE' AND "messageId" = ANY($2)"#;
const SQL_ACTIVE_IN_CHANNEL: &str = r#"SELECT "id" FROM "Poll" WHERE "guildId" = $1 AND "channelId" = $2 AND "status" = 'ACTIVE'"#;
const SQL_LIST_ACTIVE: &str = r#"SELECT * FROM "Poll" WHERE "guildId" = $1 AND "status" = 'ACTIVE' ORDER BY "createdAt" ASC LIMIT $2"#;
const SQL_VOTES_BY_POLL: &str = r#"SELECT "pollId", COUNT(*)::bigint AS n FROM "PollVote" WHERE "pollId" = ANY($1) GROUP BY "pollId""#;

// ── Pure helpers ──────────────────────────────────────────────────────────────

/// Why a `duration` option was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationError {
    Invalid,
    TooShort,
    TooLong,
}

impl DurationError {
    /// The JS error key (`invalid`, `too-short`, `too-long`).
    pub fn key(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::TooShort => "too-short",
            Self::TooLong => "too-long",
        }
    }
}

/// `parseDurationOption(raw)`: `Ok(None)` for no end time.
pub fn parse_duration_option(raw: Option<&str>) -> Result<Option<i64>, DurationError> {
    let Some(raw) = raw.map(bot_util::js::trim).filter(|s| !s.is_empty()) else { return Ok(None) };
    let Some(ms) = bot_util::parse_duration::parse_duration(raw) else { return Err(DurationError::Invalid) };
    if ms < poll::DURATION_MIN_MS {
        return Err(DurationError::TooShort);
    }
    if ms > poll::DURATION_MAX_MS {
        return Err(DurationError::TooLong);
    }
    Ok(Some(ms))
}

fn ulen(s: &str) -> usize {
    bot_util::js::utf16_len(s)
}

/// `validateQuestion(raw)`: `(question, errors)`.
pub fn validate_question(raw: &str) -> (String, Vec<String>) {
    let question = bot_util::js::trim(raw).to_string();
    let mut errors = Vec::new();
    if question.is_empty() {
        errors.push("The question is empty.".to_string());
    } else if ulen(&question) > poll::QUESTION_MAX {
        errors.push(format!("The question is {} characters (max {}).", ulen(&question), poll::QUESTION_MAX));
    }
    (question, errors)
}

/// `LIST_MARKER_RE` (`/^(?:[-*•]|\d{1,2}[.)])\s+/`): the length of a leading list marker and its spaces.
fn list_marker_len(line: &str) -> usize {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    if matches!(chars.first(), Some('-' | '*' | '•')) {
        i = 1;
    } else {
        while i < chars.len() && i < 2 && chars[i].is_ascii_digit() {
            i += 1;
        }
        if i == 0 || !matches!(chars.get(i), Some('.' | ')')) {
            return 0;
        }
        i += 1;
    }
    let start = i;
    while i < chars.len() && bot_util::js::is_js_space(chars[i]) {
        i += 1;
    }
    if i == start {
        return 0;
    }
    chars[..i].iter().map(|c| c.len_utf8()).sum()
}

/// `parsePollOptions(raw)`: one option per line, a leading list marker stripped; every problem is collected.
pub fn parse_poll_options(raw: &str) -> (Vec<String>, Vec<String>) {
    let options: Vec<String> = raw
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .map(|line| {
            let t = bot_util::js::trim(line);
            bot_util::js::trim(&t[list_marker_len(t)..]).to_string()
        })
        .filter(|s| !s.is_empty())
        .collect();
    let mut errors = Vec::new();
    if options.len() < poll::OPTIONS_MIN {
        errors.push("Add at least 2 options (one per line).".to_string());
    }
    if options.len() > poll::OPTIONS_MAX {
        errors.push(format!("You entered {} options (max {}).", options.len(), poll::OPTIONS_MAX));
    }
    let mut too_long = 0;
    for (i, o) in options.iter().enumerate() {
        if ulen(o) <= poll::OPTION_MAX {
            continue;
        }
        too_long += 1;
        if too_long <= 5 {
            errors.push(format!("Option {} is {} characters (max {}).", i + 1, ulen(o), poll::OPTION_MAX));
        } else if too_long == 6 {
            errors.push("…".to_string());
        }
    }
    let mut first: HashMap<String, usize> = HashMap::new();
    for (j, o) in options.iter().enumerate() {
        let key = bot_util::js::lower(o);
        match first.get(&key) {
            Some(f) => errors.push(format!("Option {} repeats option {}.", j + 1, f + 1)),
            None => {
                first.insert(key, j);
            }
        }
    }
    (options, errors)
}

/// Why [`can_vote`] refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoteRefusal {
    Denied { role_id: String },
    NotAllowed { role_id: String },
    AllowedRoleGone,
}

/// `canVote(roleIds, poll, existingRoleIds)`: deny wins over allow. A deleted allowed role is ignored, but an allow
/// list that pointed only at deleted roles refuses everyone.
pub fn can_vote(role_ids: &[String], allowed_role_ids: &[String], denied_role_ids: &[String], existing: Option<&HashSet<String>>) -> Result<(), VoteRefusal> {
    let held: HashSet<&String> = role_ids.iter().collect();
    if let Some(d) = denied_role_ids.iter().find(|id| held.contains(id)) {
        return Err(VoteRefusal::Denied { role_id: d.clone() });
    }
    let allowed: Vec<&String> = match existing {
        Some(ex) => allowed_role_ids.iter().filter(|id| ex.contains(*id)).collect(),
        None => allowed_role_ids.iter().collect(),
    };
    if !allowed_role_ids.is_empty() && allowed.is_empty() {
        return Err(VoteRefusal::AllowedRoleGone);
    }
    if !allowed.is_empty() && !allowed.iter().any(|id| held.contains(id)) {
        return Err(VoteRefusal::NotAllowed { role_id: allowed[0].clone() });
    }
    Ok(())
}

/// The result of a vote toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToggleResult {
    Added,
    Removed,
    Switched,
    Limit,
}

/// `applyVoteToggle(current, idx, maxChoices)`.
pub fn apply_vote_toggle(current: &[i32], idx: i32, max_choices: i32) -> (Vec<i32>, ToggleResult) {
    let mut cur = current.to_vec();
    cur.sort_unstable();
    if cur.contains(&idx) {
        return (cur.into_iter().filter(|i| *i != idx).collect(), ToggleResult::Removed);
    }
    if max_choices == 1 {
        return (vec![idx], if cur.is_empty() { ToggleResult::Added } else { ToggleResult::Switched });
    }
    if cur.len() as i32 >= max_choices {
        return (cur, ToggleResult::Limit);
    }
    cur.push(idx);
    cur.sort_unstable();
    (cur, ToggleResult::Added)
}

/// `normalizeSelection(values, optionCount, maxChoices)`: the validated, sorted, de-duplicated selection.
pub fn normalize_selection<S: AsRef<str>>(values: &[S], option_count: usize, max_choices: i32) -> Option<Vec<i32>> {
    let mut out: Vec<i32> = Vec::new();
    for v in values {
        let v = v.as_ref();
        if v.is_empty() || v.len() > 2 || !v.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let n: i32 = v.parse().ok()?;
        if n as usize >= option_count {
            return None;
        }
        if !out.contains(&n) {
            out.push(n);
        }
    }
    if out.len() as i32 > max_choices {
        return None;
    }
    out.sort_unstable();
    Some(out)
}

/// `{ counts, votes, voters }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Tally {
    pub counts: Vec<i64>,
    pub votes: i64,
    pub voters: i64,
}

/// `emptyTally(n)`.
pub fn empty_tally(n: usize) -> Tally {
    Tally { counts: vec![0; n], votes: 0, voters: 0 }
}

/// `tallyFromGroups(n, groups, { votes, voters })`; `groups` are `(optionIndex, count)`.
pub fn tally_from_groups(n: usize, groups: &[(i64, i64)], votes: i64, voters: i64) -> Tally {
    let mut counts = vec![0i64; n];
    for (i, c) in groups {
        if *i >= 0 && (*i as usize) < n {
            counts[*i as usize] = *c;
        }
    }
    // The two queries are not one snapshot: never report fewer votes than the bars add up to.
    let sum: i64 = counts.iter().sum();
    Tally { counts, votes: votes.max(sum), voters }
}

/// `winners(counts)`: indexes with the highest count; empty when nobody voted.
pub fn winners(counts: &[i64]) -> Vec<usize> {
    let max = counts.iter().copied().fold(0, i64::max);
    if max == 0 {
        return Vec::new();
    }
    counts.iter().enumerate().filter(|(_, c)| **c == max).map(|(i, _)| i).collect()
}

/// `percent(count, total)`: one option's share, rounded on its own.
pub fn percent(count: i64, total: i64) -> i64 {
    if total == 0 { 0 } else { ((count * 100) as f64 / total as f64 + 0.5).floor() as i64 }
}

fn plural(n: i64, word: &str) -> String {
    format!("{word}{}", if n == 1 { "" } else { "s" })
}

fn shown_percentages(options: &[String], counts: &[i64]) -> Vec<i64> {
    percentages(&options.iter().enumerate().map(|(i, _)| counts.get(i).copied().unwrap_or(0) as f64).collect::<Vec<_>>())
}

/// `formatOptionLines(options, tally, { hidden, ended })`: one block per option, joined with newlines.
pub fn format_option_lines(options: &[String], tally: &Tally, hidden: bool, ended: bool) -> String {
    let counts = &tally.counts;
    let shown = shown_percentages(options, counts);
    let win: HashSet<usize> = if ended && !hidden { winners(counts).into_iter().collect() } else { HashSet::new() };
    options
        .iter()
        .enumerate()
        .map(|(i, opt)| {
            if hidden {
                return format!("**{}.** {opt}", i + 1);
            }
            let c = counts.get(i).copied().unwrap_or(0);
            let p = shown.get(i).copied().unwrap_or(0);
            let bar = progress_bar(p as f64 / 100.0, poll::BAR_WIDTH, "▰", "▱");
            format!(
                "**{}.** {}{opt}\n{bar} **{p}%** · {} {}",
                i + 1,
                if win.contains(&i) { "🏆 " } else { "" },
                format_number(c as f64),
                plural(c, "vote")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `buttonLabel(options, i)`: "1. Yes" for short option sets, otherwise just the number.
pub fn button_label(options: &[String], i: usize) -> String {
    let compact = options.len() <= 10 && options.iter().all(|o| ulen(o) <= 25);
    clamp_text(&if compact { format!("{}. {}", i + 1, options[i]) } else { (i + 1).to_string() }, 80)
}

fn cid(prefix: &str, params: &[String]) -> String {
    bot_config::build_custom_id(prefix, params).unwrap_or_else(|_| format!("{prefix}:{}", params.join(":")))
}

/// `buildPollComponents(poll, { disabled, selected })`: one button per option (single choice) or one select (multi
/// choice); `selected` pre-fills the select (the personal ballot).
pub fn build_poll_components(p: &Poll, disabled: bool, selected: Option<&[i32]>) -> Vec<CreateActionRow> {
    if p.max_choices == 1 {
        let mut rows: Vec<Vec<CreateButton>> = Vec::new();
        for i in 0..p.options.len() {
            if i % poll::BUTTONS_PER_ROW == 0 {
                rows.push(Vec::new());
            }
            let b = CreateButton::new(cid("poll", &["vote".into(), p.id.clone(), i.to_string()]))
                .label(button_label(&p.options, i))
                .style(ButtonStyle::Secondary)
                .disabled(disabled);
            if let Some(r) = rows.last_mut() {
                r.push(b);
            }
        }
        return rows.into_iter().map(CreateActionRow::Buttons).collect();
    }
    let options: Vec<CreateSelectMenuOption> = p
        .options
        .iter()
        .enumerate()
        .map(|(i, opt)| {
            CreateSelectMenuOption::new(clamp_text(&format!("{}. {opt}", i + 1), 100), i.to_string())
                .default_selection(selected.is_some_and(|s| s.contains(&(i as i32))))
        })
        .collect();
    let select = CreateSelectMenu::new(cid("poll-select", std::slice::from_ref(&p.id)), CreateSelectMenuKind::String { options })
        .min_values(0)
        .max_values(p.max_choices.clamp(0, 25) as u8)
        .placeholder(if disabled { "This poll has ended".to_string() } else { format!("Choose up to {} answers", p.max_choices) })
        .disabled(disabled);
    vec![CreateActionRow::SelectMenu(select)]
}

fn votes_line(p: &Poll, t: &Tally) -> String {
    format!(
        "🗳️ **{}** {} from **{}** {} · {}",
        format_number(t.votes as f64),
        plural(t.votes, "vote"),
        format_number(t.voters as f64),
        plural(t.voters, "member"),
        if p.max_choices == 1 { "one choice per member".to_string() } else { format!("up to {} choices per member", p.max_choices) }
    )
}

fn ends_line(ends_at: &DateTime<Utc>) -> String {
    format!("⏰ Ends {} ({})", discord_timestamp(ends_at, "R"), discord_timestamp(ends_at, "f"))
}

fn is_ended(p: &Poll) -> bool {
    p.status == PollStatus::Ended
}

/// `buildPollEmbed(poll, tally, { settings, ended })`: the poll message embed.
pub fn build_poll_embed(p: &Poll, t: &Tally, settings: Option<&GuildSettings>, ended: bool) -> CreateEmbed {
    let hidden = p.hide_results && !ended;
    let mut lines = vec![format_option_lines(&p.options, t, hidden, ended), String::new()];
    if ended {
        lines.push(format!("🏁 **Poll ended**{}", p.ends_at.map(|e| format!(" {}", discord_timestamp(&e, "R"))).unwrap_or_default()));
    }
    lines.push(votes_line(p, t));
    if !ended {
        lines.push(match &p.ends_at {
            Some(e) => ends_line(e),
            None => "⏰ Open until a moderator ends it".to_string(),
        });
    }
    if !ended && p.hide_results {
        lines.push("🔒 Results are hidden until the poll ends.".into());
    }
    if !p.allowed_role_ids.is_empty() {
        lines.push(format!("✅ Only {} can vote.", p.allowed_role_ids.iter().map(|r| format!("<@&{r}>")).collect::<Vec<_>>().join(", ")));
    }
    if !p.denied_role_ids.is_empty() {
        lines.push(format!("🚫 {} can't vote.", p.denied_role_ids.iter().map(|r| format!("<@&{r}>")).collect::<Vec<_>>().join(", ")));
    }
    lines.push(format!("👤 Started by <@{}>", p.author_id));
    let footer = if ended {
        "This poll has ended"
    } else if p.max_choices == 1 {
        "Click a button to vote · click it again to remove your vote"
    } else {
        "Pick your answers in the menu · each pick replaces your previous votes"
    };
    guild_embed(settings, Some(&clamp_text(&format!("📊 {}", p.question), 256)), Some(&clamp_text(&lines.join("\n"), 4096)), Some(footer))
        .timestamp(Timestamp::from(p.created_at))
}

/// `buildPollPayload(poll, tally, { settings, ended })` as the message body Discord receives.
pub fn build_poll_payload(p: &Poll, t: &Tally, settings: Option<&GuildSettings>, ended: bool) -> Value {
    json!({
        "embeds": [embed_json(&build_poll_embed(p, t, settings, ended))],
        "components": build_poll_components(p, ended, None).iter().map(row_json).collect::<Vec<_>>(),
        "allowed_mentions": { "parse": [] },
    })
}

/// [`build_poll_payload`] with `ended` from the poll's status (the JS default).
pub fn build_poll_payload_default(p: &Poll, t: &Tally, settings: Option<&GuildSettings>) -> Value {
    build_poll_payload(p, t, settings, is_ended(p))
}

/// `jumpUrl(poll)`: the message link, or `None` before the first send.
pub fn jump_url(p: &Poll) -> Option<String> {
    p.message_id.as_ref().map(|m| format!("https://discord.com/channels/{}/{}/{m}", p.guild_id, p.channel_id))
}

fn summary_line(p: &Poll, t: &Tally) -> String {
    let w = winners(&t.counts);
    if w.is_empty() {
        return "No votes were cast.".into();
    }
    let c = t.counts[w[0]];
    if w.len() == 1 {
        let shown = shown_percentages(&p.options, &t.counts);
        return format!(
            "🏆 **{}** won with **{}** {} ({}%).",
            clamp_text(&p.options[w[0]], poll::OPTION_MAX),
            format_number(c as f64),
            plural(c, "vote"),
            shown[w[0]]
        );
    }
    let names: Vec<String> = w.iter().map(|i| format!("**{}**", clamp_text(&p.options[*i], poll::OPTION_MAX))).collect();
    format!("🤝 Tie between {} with **{}** {} each.", join_with_limit_default(&names), format_number(c as f64), plural(c, "vote"))
}

/// `buildResultsEmbed(poll, tally, { settings, live, messageExists })`: the results post when a poll ends
/// (`live: false`) and `/poll results` (a private peek).
pub fn build_results_embed(p: &Poll, t: &Tally, settings: Option<&GuildSettings>, live: bool, message_exists: bool) -> CreateEmbed {
    let mut lines = vec![format!("**{}**", clamp_text(&p.question, 512))];
    if !live {
        lines.push(summary_line(p, t));
    }
    lines.push(String::new());
    lines.push(format_option_lines(&p.options, t, false, !live));
    lines.push(String::new());
    lines.push(votes_line(p, t));
    if live && let Some(e) = &p.ends_at {
        lines.push(ends_line(e));
    }
    if live && p.hide_results {
        lines.push("🔒 Members can't see these results until the poll ends.".into());
    }
    if let Some(url) = jump_url(p).filter(|_| message_exists) {
        lines.push(format!("[Jump to the poll]({url})"));
    }
    guild_embed(settings, Some(if live { "📊 Live results" } else { "📊 Poll results" }), Some(&clamp_text(&lines.join("\n"), 4096)), None)
}

// ── The pending create forms ──────────────────────────────────────────────────

/// `createPendingStore({ ttlMs, max, now })`: insertion-ordered; `put` sweeps expired entries and evicts the oldest
/// beyond `max`; `take` removes and returns the value (`None` when missing or older than `ttl_ms`).
pub struct PendingStore<T> {
    ttl_ms: i64,
    max: usize,
    entries: Mutex<VecDeque<(String, T, i64)>>,
}

impl<T: Clone> PendingStore<T> {
    pub fn new(ttl_ms: i64, max: usize) -> Self {
        Self { ttl_ms, max, entries: Mutex::new(VecDeque::new()) }
    }

    pub fn put(&self, key: &str, value: T, now_ms: i64) {
        let mut m = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        while m.front().is_some_and(|(_, _, at)| now_ms - at > self.ttl_ms) {
            m.pop_front();
        }
        m.retain(|(k, _, _)| k != key);
        m.push_back((key.to_string(), value, now_ms));
        while m.len() > self.max {
            m.pop_front();
        }
    }

    pub fn take(&self, key: &str, now_ms: i64) -> Option<T> {
        let mut m = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let at = m.iter().position(|(k, _, _)| k == key)?;
        let (_, v, t) = m.remove(at)?;
        (now_ms - t <= self.ttl_ms).then_some(v)
    }

    pub fn size(&self) -> usize {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// What `/poll create` stores until the form is submitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingPoll {
    pub guild_id: String,
    pub channel_id: String,
    pub user_id: String,
    pub max_choices: i64,
    pub allowed_role_ids: Vec<String>,
    pub denied_role_ids: Vec<String>,
    pub hide_results: bool,
    pub duration_ms: Option<i64>,
}

/// `pendingPolls`, one per app.
pub struct PendingPolls(pub PendingStore<PendingPoll>);

impl Default for PendingPolls {
    fn default() -> Self {
        Self(PendingStore::new(poll::PENDING_TTL_MS, poll::PENDING_MAX))
    }
}

/// `buildCreateModal(nonce)`: the create form; the nonce is the `/poll create` interaction id.
pub fn build_create_modal(nonce: &str) -> CreateModal {
    let field = |id: &str| bot_config::build_custom_id::<String>(id, &[]).unwrap_or_else(|_| id.to_string());
    CreateModal::new(cid("poll-modal", &[nonce.to_string()]), "Create a poll").components(vec![
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Short, "Question", field("poll-question"))
                .required(true)
                .min_length(1)
                .max_length(poll::QUESTION_MAX as u16)
                .placeholder("What should we play tonight?"),
        ),
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Paragraph, "Options (one per line, 2–20)", field("poll-options"))
                .required(true)
                .min_length(3)
                .max_length(poll::OPTIONS_INPUT_MAX)
                .placeholder("Option 1\nOption 2\nOption 3"),
        ),
    ])
}

/// `channelProblem(channel, member)`: the checks every poll location must pass, for the bot and the moderator.
pub fn channel_problem(app: &App, channel: Option<&GuildChannel>, member: Option<&Member>) -> Option<CreateEmbed> {
    let Some(channel) = channel else {
        return Some(error_embed("Channel Not Found", Some("I can't see that channel. Pick one with the `channel` option."), None));
    };
    let usable = guild_view::channel_usable(app, channel.guild_id, channel, &CHANNEL_POST_NEEDS);
    if !usable.ok {
        let text = if !usable.missing.is_empty() {
            format!("I need **{}** in <#{}> to post a poll.", permission_names(&usable.missing).join(", "), channel.id)
        } else {
            format!("{}.", usable.reason.unwrap_or_default())
        };
        return Some(error_embed("Cannot Post There", Some(&text), None));
    }
    let need = Permissions::VIEW_CHANNEL | if is_thread(channel) { Permissions::SEND_MESSAGES_IN_THREADS } else { Permissions::SEND_MESSAGES };
    if !member.is_some_and(|m| guild_view::member_has_in(app, channel.guild_id, channel, m, need)) {
        return Some(error_embed("Cannot Post There", Some(&format!("You need permission to view and send messages in <#{}>.", channel.id)), None));
    }
    None
}

/// `sendFailureReason(err)`: why `channel.send` failed, as a sentence fragment.
pub fn send_failure_reason(err: Option<&DiscordError>) -> String {
    match err.and_then(|e| e.code) {
        Some(50013) | Some(50001) => "I'm missing permissions there.".into(),
        Some(50083) => "that thread is archived.".into(),
        Some(UNKNOWN_CHANNEL) => "the channel no longer exists.".into(),
        Some(c) => format!("Discord error {c}."),
        None => "Discord error unknown.".into(),
    }
}

// ── Tally ─────────────────────────────────────────────────────────────────────

/// `prisma.poll.findUnique({ where: { id } })`.
pub async fn find_poll(db: &bot_db::PgPool, id: &str) -> anyhow::Result<Option<Poll>> {
    Ok(sqlx::query_as::<_, Poll>(SQL_BY_ID).bind(id).fetch_optional(db).await?)
}

/// `prisma.poll.findFirst({ where: { id, guildId } })`.
pub async fn find_poll_in(db: &bot_db::PgPool, id: &str, guild_id: &str) -> anyhow::Result<Option<Poll>> {
    Ok(sqlx::query_as::<_, Poll>(SQL_BY_ID_GUILD).bind(id).bind(guild_id).fetch_optional(db).await?)
}

/// `getTally(poll)`.
pub async fn get_tally(db: &bot_db::PgPool, p: &Poll) -> anyhow::Result<Tally> {
    let (groups, totals) = tokio::join!(
        sqlx::query(SQL_GROUPS).bind(&p.id).fetch_all(db),
        sqlx::query(TALLY_TOTALS_SQL).bind(&p.id).fetch_optional(db)
    );
    let groups: Vec<(i64, i64)> =
        groups?.iter().filter_map(|r| Some((i64::from(r.try_get::<i32, _>("optionIndex").ok()?), r.try_get::<i64, _>("n").ok()?))).collect();
    let (votes, voters) = match totals? {
        Some(r) => (i64::from(r.try_get::<i32, _>("votes").unwrap_or(0)), i64::from(r.try_get::<i32, _>("voters").unwrap_or(0))),
        None => (0, 0),
    };
    Ok(tally_from_groups(p.options.len(), &groups, votes, voters))
}

// ── Creating ──────────────────────────────────────────────────────────────────

/// The question, options and max choices of a new poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollInput {
    pub question: String,
    pub options: Vec<String>,
    pub max_choices: i64,
}

/// `createPollRow(guildId, pending, input)`: inserts the poll under the guild cap. `Ok(None)` is `LIMIT`.
pub async fn create_poll_row(app: &App, guild_id: &str, pending: &PendingPoll, input: &PollInput) -> anyhow::Result<Option<Poll>> {
    app.settings().ensure_guild_settings(guild_id).await?;
    let ends_at = pending.duration_ms.filter(|d| *d != 0).and_then(|d| DateTime::from_timestamp_millis(app.clock().now_ms() + d));
    let insert = InsertBuilder::new(&POLL_INFO)
        .values([
            ("guildId", json!(guild_id)),
            ("channelId", json!(pending.channel_id)),
            ("authorId", json!(pending.user_id)),
            ("question", json!(input.question)),
            ("options", json!(input.options)),
            ("maxChoices", json!(input.max_choices)),
            ("allowedRoleIds", json!(pending.allowed_role_ids)),
            ("deniedRoleIds", json!(pending.denied_role_ids)),
            ("hideResults", json!(pending.hide_results)),
            ("endsAt", ends_at.map(|e| json!(bot_db::format_js_date(&e))).unwrap_or(Value::Null)),
        ])?
        .build()?;
    let key = format!("poll-create:{guild_id}");
    let gid = guild_id.to_string();
    let out = bot_db::transaction(app.db(), TX_OPTS, move |tx| {
        Box::pin(async move {
            sqlx::query(CREATE_LOCK_SQL).bind(&key).execute(&mut *tx).await.map_err(bot_db::DbError::from)?;
            let active: i64 = sqlx::query(SQL_COUNT_ACTIVE).bind(&gid).fetch_one(&mut *tx).await.map_err(bot_db::DbError::from)?.try_get("n").map_err(bot_db::DbError::from)?;
            if active >= poll::ACTIVE_MAX as i64 {
                return Ok::<_, bot_db::DbError>(None);
            }
            let row: Poll = insert.fetch_one(&mut *tx).await?;
            Ok(Some(row))
        })
    })
    .await?;
    Ok(out)
}

/// `reconcileDeletedChannels(client, guild)`: ends (reason `deleted`) every active poll whose channel is gone. Acts
/// only on a REST Unknown Channel while the guild is available: a cache miss is never judged a deletion.
pub async fn reconcile_deleted_channels(app: &App, guild_id: &str) -> anyhow::Result<usize> {
    if !guild_view::guild_available(app, guild_id) {
        return Ok(0);
    }
    let rows = sqlx::query(SQL_ACTIVE_CHANNELS).bind(guild_id).fetch_all(app.db()).await?;
    let mut by_channel: Vec<(String, Vec<String>)> = Vec::new();
    for r in rows {
        let (Ok(id), Ok(ch)) = (r.try_get::<String, _>("id"), r.try_get::<String, _>("channelId")) else { continue };
        match by_channel.iter_mut().find(|(c, _)| *c == ch) {
            Some((_, ids)) => ids.push(id),
            None => by_channel.push((ch, vec![id])),
        }
    }
    let gid = guild_id_of(guild_id);
    let mut ended = 0;
    for (channel_id, ids) in by_channel {
        if let (Some(g), Some(c)) = (gid, channel_id_of(&channel_id))
            && cached_channel(app, g, c).is_some()
        {
            continue;
        }
        match ops(app).fetch_channel(&channel_id).await {
            Ok(_) => continue, // it exists (archived threads are often uncached)
            Err(e) if e.code != Some(UNKNOWN_CHANNEL) || !guild_view::guild_available(app, guild_id) => continue,
            Err(_) => {}
        }
        for id in ids {
            if end_poll(app, &id, EndReason::Deleted, Some(guild_id)).await?.is_ok() {
                ended += 1;
            }
        }
    }
    Ok(ended)
}

/// `createPoll(client, guild, pending, input)`: on `LIMIT` it first reconciles polls whose channel was deleted, then
/// retries once. `Ok(None)` is `LIMIT`.
pub async fn create_poll(app: &App, guild_id: &str, pending: &PendingPoll, input: &PollInput) -> anyhow::Result<Option<Poll>> {
    let res = create_poll_row(app, guild_id, pending, input).await?;
    if res.is_some() {
        return Ok(res);
    }
    match reconcile_deleted_channels(app, guild_id).await {
        Ok(n) if n > 0 => create_poll_row(app, guild_id, pending, input).await,
        Ok(_) => Ok(None),
        Err(e) => {
            bot_util::log_warn!("poll: reconciling deleted channels failed", bot_core::error::log_arg(&e));
            Ok(None)
        }
    }
}

fn echo_safe(s: &str) -> String {
    s.replace("```", "`\u{200b}``")
}

/// The "copy it to try again" block appended to expired or invalid form errors (within the 4096 description).
fn echo_block(question: &str, options_raw: &str, base_length: usize) -> String {
    let head = "\n\nYour input (copy it to try again):\n```\n";
    let tail = "\n```";
    let room = 4096usize.saturating_sub(base_length + ulen(head) + ulen(tail));
    format!("{head}{}{tail}", clamp_text(&format!("{}\n---\n{}", echo_safe(question), echo_safe(options_raw)), room))
}

/// Why [`post_poll`] failed.
#[derive(Debug, Clone, PartialEq)]
pub enum PostError {
    Limit,
    Send(DiscordError),
    Save(String),
}

impl PostError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Limit => "LIMIT",
            Self::Send(_) => "SEND",
            Self::Save(_) => "SAVE",
        }
    }
}

/// What [`post_poll`] returns.
#[derive(Debug, Clone, PartialEq)]
pub struct Posted {
    pub poll: Poll,
    pub timer_failed: bool,
}

/// `postPoll({ client, guild, channel, pending, question, options, maxChoices })`: creates a poll, posts it in
/// `channel`, stores its message id and schedules its end; the steps the `/poll` form and the dashboard share. A
/// failed post or save removes the poll again (and the posted message). The outer `Err` is a database error of the
/// create step (the JS throws there too).
pub async fn post_poll(app: &App, guild_id: &str, channel: &GuildChannel, pending: &PendingPoll, input: &PollInput) -> anyhow::Result<Result<Posted, PostError>> {
    post_poll_with(app, guild_id, channel, pending, input, "poll").await
}

/// [`post_poll`] with the log prefix of the caller (the JS `deps.logPrefix`; the dashboard passes `dashboard poll`).
pub async fn post_poll_with(
    app: &App,
    guild_id: &str,
    channel: &GuildChannel,
    pending: &PendingPoll,
    input: &PollInput,
    log: &str,
) -> anyhow::Result<Result<Posted, PostError>> {
    let Some(p) = create_poll(app, guild_id, pending, input).await? else { return Ok(Err(PostError::Limit)) };
    let settings = app.settings().get_settings_safe(guild_id).await;
    let body = build_poll_payload_default(&p, &empty_tally(p.options.len()), settings.as_deref());
    let msg = match ops(app).send_message(guild_id, &channel.id.to_string(), body).await {
        Ok(m) => m,
        Err(e) => {
            bot_util::log_warn!(format!("{log}: could not post the poll message"), err_arg(&e));
            let _ = sqlx::query(SQL_DELETE).bind(&p.id).execute(app.db()).await;
            return Ok(Err(PostError::Send(e)));
        }
    };
    if let Err(e) = sqlx::query(SQL_SET_MESSAGE).bind(&p.id).bind(&msg.id).bind(now_of(app).naive_utc()).execute(app.db()).await {
        bot_util::log_error!(format!("{log}: could not store the poll message id"), err_arg(&e));
        let _ = ops(app).delete_message(&msg.channel_id, &msg.id).await;
        let _ = sqlx::query(SQL_DELETE).bind(&p.id).execute(app.db()).await;
        return Ok(Err(PostError::Save(e.to_string())));
    }
    let mut timer_failed = false;
    if pending.duration_ms.is_some_and(|d| d != 0)
        && let Some(ends_at) = p.ends_at
    {
        let job = ScheduleJob::new(guild_id, JobType::PollEnd, ends_at).payload(json!({ "pollId": p.id })).dedupe_key(end_job_key(&p.id));
        if let Err(e) = app.scheduler().schedule_job(job).await {
            timer_failed = true;
            bot_util::log_error!(format!("{log}: could not schedule poll.end"), bot_core::error::log_arg(&e));
        }
    }
    let mut poll = p;
    poll.message_id = Some(msg.id);
    Ok(Ok(Posted { poll, timer_failed }))
}

// ── Modal submit ──────────────────────────────────────────────────────────────

/// `poll-modal:{nonce}`: validates the form, posts the poll and schedules its end.
pub async fn handle_create_modal(ctx: ModalCtx) -> BotResult {
    let (_, params) = ctx.parsed();
    let nonce = params.first().cloned().unwrap_or_default();
    let question = ctx.value("poll-question").unwrap_or_default();
    let options_raw = ctx.value("poll-options").unwrap_or_default();
    let fail = |embed: CreateEmbed| safe_reply(&ctx.responder, Payload::embed(embed).ephemeral(true));

    let Some(pending) = ctx.app.ext::<PendingPolls>().0.take(&nonce, ctx.app.clock().now_ms()) else {
        let base = "This poll form has expired (forms last 15 minutes and do not survive a bot restart). Run `/poll create` again.";
        let text = format!("{base}{}", echo_block(&question, &options_raw, ulen(base)));
        fail(error_embed("Form Expired", Some(&text), None)).await;
        return Ok(());
    };
    let guild_id = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
    if pending.user_id != ctx.user().id.to_string() || pending.guild_id != guild_id {
        fail(error_embed("Form Expired", Some("This form is not yours. Run `/poll create` to start a poll."), None)).await;
        return Ok(());
    }
    let (q, q_errors) = validate_question(&question);
    let (options, o_errors) = parse_poll_options(&options_raw);
    let problems: Vec<String> = q_errors.into_iter().chain(o_errors).collect();
    if !problems.is_empty() {
        let base = clamp_text(&problems.iter().map(|e| format!("• {e}")).collect::<Vec<_>>().join("\n"), 1500);
        let text = format!("{base}{}", echo_block(&question, &options_raw, ulen(&base)));
        fail(error_embed("Invalid Poll", Some(&text), None)).await;
        return Ok(());
    }

    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let reply = |embed: CreateEmbed| safe_reply(&ctx.responder, Payload::embed(embed));
    let max_choices = pending.max_choices.min(options.len() as i64);
    let clamped = max_choices < pending.max_choices;

    // `guild.channels.cache.get(id) ?? guild.channels.fetch(id).catch(() => null)`: a channel of another guild is not
    // this guild's channel.
    let channel = guild_view::fetch_guild_channel(&ctx.app, &guild_id, &pending.channel_id).await.ok().flatten();
    if let Some(problem) = channel_problem(&ctx.app, channel.as_ref(), ctx.member()) {
        reply(problem).await;
        return Ok(());
    }
    let Some(channel) = channel else { return Ok(()) };

    let input = PollInput { question: q, options, max_choices };
    let out = post_poll(&ctx.app, &guild_id, &channel, &pending, &input).await?;
    let posted = match out {
        Ok(p) => p,
        Err(PostError::Limit) => {
            reply(error_embed(
                "Too Many Polls",
                Some(&format!("This server already has **{}** active polls. End one with `/poll end` first.", poll::ACTIVE_MAX)),
                None,
            ))
            .await;
            return Ok(());
        }
        Err(PostError::Send(e)) => {
            reply(error_embed("Could Not Post Poll", Some(&format!("I couldn't post the poll in <#{}>: {}", channel.id, send_failure_reason(Some(&e)))), None))
                .await;
            return Ok(());
        }
        Err(PostError::Save(_)) => {
            reply(error_embed("Could Not Post Poll", Some("I couldn't save the poll. Please try again in a moment."), None)).await;
            return Ok(());
        }
    };
    let p = &posted.poll;
    let mut lines = vec![format!("Your poll is live in <#{}>. [Jump to poll]({})", channel.id, jump_url(p).unwrap_or_default())];
    if pending.duration_ms.is_some_and(|d| d != 0)
        && let Some(e) = &p.ends_at
    {
        lines.push(format!("It ends {}.", discord_timestamp(e, "R")));
    }
    if clamped {
        lines.push(format!("Max choices was lowered to **{max_choices}** (the number of options)."));
    }
    if posted.timer_failed {
        lines.push("⚠️ The end timer could not be scheduled. End the poll with `/poll end`.".into());
    }
    reply(success_embed("Poll Posted", Some(&lines.join("\n")), None)).await;
    Ok(())
}

// ── Voting ────────────────────────────────────────────────────────────────────

const EXPIRED_TEXT: &str = "This poll has ended.";
const VOTE_FAILED: &str = "Something went wrong while saving your vote. Please try again.";

/// What a vote asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoteInput {
    /// Toggle one option (the button).
    Toggle(i32),
    /// Replace the vote set (the select).
    Set(Vec<i32>),
}

/// The result of [`apply_vote`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoteOutcome {
    Gone,
    Ended,
    /// `result` is `added`, `removed`, `switched`, `limit`, `set` or `cleared`.
    Applied { result: &'static str, next: Vec<i32>, changed: bool },
}

/// `applyVote({ pollId, userId, maxChoices, idx, set })`: the vote transaction. The advisory lock on (poll, user)
/// guards "one option in a single-choice poll"; `FOR SHARE` on the poll row serialises votes against the end.
pub async fn apply_vote(app: &App, poll_id: &str, user_id: &str, max_choices: i32, input: VoteInput) -> anyhow::Result<VoteOutcome> {
    let (pid, uid) = (poll_id.to_string(), user_id.to_string());
    let out = bot_db::transaction(app.db(), TX_OPTS, move |tx| {
        Box::pin(async move {
            let e = bot_db::DbError::from;
            sqlx::query(VOTE_LOCK_SQL).bind(format!("poll-vote:{pid}:{uid}")).execute(&mut *tx).await.map_err(e)?;
            let Some(row) = sqlx::query(POLL_STATUS_SQL).bind(&pid).fetch_optional(&mut *tx).await.map_err(e)? else {
                return Ok::<_, bot_db::DbError>(VoteOutcome::Gone);
            };
            if row.try_get::<String, _>("status").map_err(e)? != "ACTIVE" {
                return Ok(VoteOutcome::Ended);
            }
            let mut current: Vec<i32> = sqlx::query(SQL_USER_VOTES)
                .bind(&pid)
                .bind(&uid)
                .fetch_all(&mut *tx)
                .await
                .map_err(e)?
                .iter()
                .filter_map(|r| r.try_get::<i32, _>("optionIndex").ok())
                .collect();
            current.sort_unstable();
            let (next, result) = match input {
                VoteInput::Set(set) => {
                    let r = if set.is_empty() { "cleared" } else { "set" };
                    (set, r)
                }
                VoteInput::Toggle(idx) => {
                    let (next, r) = apply_vote_toggle(&current, idx, max_choices);
                    let name = match r {
                        ToggleResult::Added => "added",
                        ToggleResult::Removed => "removed",
                        ToggleResult::Switched => "switched",
                        ToggleResult::Limit => "limit",
                    };
                    (next, name)
                }
            };
            if result == "limit" {
                return Ok(VoteOutcome::Applied { result, next: current, changed: false });
            }
            let del: Vec<i32> = current.iter().filter(|i| !next.contains(i)).copied().collect();
            let add: Vec<i32> = next.iter().filter(|i| !current.contains(i)).copied().collect();
            if !del.is_empty() {
                sqlx::query(SQL_DELETE_VOTES).bind(&pid).bind(&uid).bind(&del).execute(&mut *tx).await.map_err(e)?;
            }
            for i in &add {
                InsertBuilder::new(&POLL_VOTE_INFO)
                    .values([("pollId", json!(pid)), ("userId", json!(uid)), ("optionIndex", json!(i))])?
                    .on_conflict_do_nothing(&["pollId", "userId", "optionIndex"])?
                    .without_returning()
                    .build()?
                    .execute(&mut *tx)
                    .await?;
            }
            Ok(VoteOutcome::Applied { result, next, changed: !del.is_empty() || !add.is_empty() })
        })
    })
    .await?;
    Ok(out)
}

/// `true` when any component of a message is still enabled.
pub fn has_live_components(components: &[serenity::all::ActionRow]) -> bool {
    components.iter().any(|row| {
        row.components.iter().any(|c| match c {
            ActionRowComponent::Button(b) => !b.disabled,
            ActionRowComponent::SelectMenu(s) => !s.disabled,
            _ => false,
        })
    })
}

/// Re-renders the message of an already ended poll whose components are still live (the process died mid-end).
async fn heal_ended_message(ctx: &ComponentCtx, p: &Poll) {
    let msg = &ctx.interaction.message;
    if Some(msg.id.to_string()) != p.message_id || !has_live_components(&msg.components) {
        return;
    }
    let (app, poll) = (ctx.app.clone(), p.clone());
    let channel_id = msg.channel_id.to_string();
    let message_id = msg.id.to_string();
    let r: anyhow::Result<()> = render_locked(&ctx.app, &p.id, move || async move {
        let tally = get_tally(app.db(), &poll).await?;
        let settings = app.settings().get_settings_safe(&poll.guild_id).await;
        ops(&app).edit_message(&channel_id, &message_id, build_poll_payload(&poll, &tally, settings.as_deref(), true)).await?;
        Ok(())
    })
    .await;
    if let Err(e) = r {
        bot_util::log_warn!(format!("poll: could not finalise the message of ended poll {}", p.id), bot_core::error::log_arg(&e));
    }
}

/// The text of a vote refusal.
pub fn refusal_text(r: &VoteRefusal) -> String {
    match r {
        VoteRefusal::NotAllowed { role_id } => format!("Only members with <@&{role_id}> can vote in this poll."),
        VoteRefusal::Denied { role_id } => format!("Members with <@&{role_id}> can't vote in this poll."),
        VoteRefusal::AllowedRoleGone => "The role allowed to vote in this poll no longer exists. Ask a moderator to end it with /poll end.".into(),
    }
}

/// `prepareVote(...)`: the poll exists, is open, the option exists and the voter may vote. `Err((text, terminal))`.
async fn prepare_vote(ctx: &ComponentCtx, poll_id: &str, idx: Option<i32>, heal: bool) -> anyhow::Result<Result<Poll, (String, bool)>> {
    let guild_id = ctx.guild_id().map(|g| g.to_string());
    let Some(p) = find_poll(ctx.app.db(), poll_id).await?.filter(|p| Some(&p.guild_id) == guild_id.as_ref()) else {
        return Ok(Err(("This poll no longer exists.".into(), true)));
    };
    if p.status == PollStatus::Ended {
        if heal {
            heal_ended_message(ctx, &p).await;
        }
        return Ok(Err((EXPIRED_TEXT.into(), true)));
    }
    if p.ends_at.is_some_and(|e| e.timestamp_millis() <= ctx.app.clock().now_ms()) {
        // The end job is late or missing: end it now (the conditional update picks one winner).
        let _ = end_poll(&ctx.app, &p.id, EndReason::Expired, Some(&p.guild_id)).await?;
        return Ok(Err((EXPIRED_TEXT.into(), true)));
    }
    if idx.is_some_and(|i| i as usize >= p.options.len()) {
        return Ok(Err(("That option no longer exists.".into(), false)));
    }
    let Some(member) = ctx.member() else {
        return Ok(Err(("I can't check your roles right now. Please try again in a moment.".into(), false)));
    };
    let existing: HashSet<String> = guild_view::role_ids(&ctx.app, &p.guild_id).into_iter().collect();
    let roles: Vec<String> = member.roles.iter().map(|r| r.to_string()).collect();
    if let Err(r) = can_vote(&roles, &p.allowed_role_ids, &p.denied_role_ids, (!existing.is_empty()).then_some(&existing)) {
        return Ok(Err((refusal_text(&r), false)));
    }
    Ok(Ok(p))
}

fn option_name(p: &Poll, i: i32) -> String {
    format!("**{}. {}**", i + 1, clamp_text(p.options.get(i as usize).map(String::as_str).unwrap_or(""), poll::OPTION_MAX))
}

fn list_of(p: &Poll, indexes: &[i32]) -> String {
    let names: Vec<String> = indexes.iter().map(|i| option_name(p, *i)).collect();
    bot_util::text::join_with_limit(&names, ", ", 1800, bot_util::text::default_more)
}

/// `buttonReply(poll, idx, out)`: the private answer to a button vote.
pub fn button_reply(p: &Poll, idx: i32, result: &str, next: &[i32]) -> String {
    if result == "limit" {
        return format!("You can pick at most **{}** options. Remove a vote first.", p.max_choices);
    }
    if p.max_choices <= 1 {
        return match result {
            "added" => format!("✅ You voted for {}.", option_name(p, idx)),
            "switched" => format!("🔁 Your vote changed to {}.", option_name(p, idx)),
            _ => format!("🗑️ Your vote for {} was removed.", option_name(p, idx)),
        };
    }
    let head = if result == "added" { "✅ Vote added." } else { "🗑️ Vote removed." };
    if next.is_empty() {
        format!("{head} You have no votes in this poll.")
    } else {
        format!("{head} Your votes ({}/{}): {}", next.len(), p.max_choices, list_of(p, next))
    }
}

/// `poll:vote:{pollId}:{optionIndex}`: toggles one option. Defers first.
pub async fn handle_vote_button(ctx: ComponentCtx) -> BotResult {
    let (_, params) = ctx.parsed();
    let action = params.first().map(String::as_str);
    let poll_id = params.get(1).cloned().unwrap_or_default();
    let idx_raw = params.get(2).cloned().unwrap_or_default();
    let idx_ok = !idx_raw.is_empty() && idx_raw.len() <= 2 && idx_raw.bytes().all(|b| b.is_ascii_digit());
    if action != Some("vote") || poll_id.is_empty() || !idx_ok {
        safe_reply(&ctx.responder, Payload::text("This button is no longer active.").ephemeral(true)).await;
        return Ok(());
    }
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let reply = |content: String| safe_reply(&ctx.responder, Payload::text(content).no_mentions());
    let idx: i32 = idx_raw.parse().unwrap_or(0);
    let run: anyhow::Result<()> = async {
        let p = match prepare_vote(&ctx, &poll_id, Some(idx), true).await? {
            Ok(p) => p,
            Err((text, _)) => {
                reply(text).await;
                return Ok(());
            }
        };
        match apply_vote(&ctx.app, &poll_id, &ctx.user().id.to_string(), p.max_choices, VoteInput::Toggle(idx)).await? {
            VoteOutcome::Gone => {
                reply("This poll no longer exists.".into()).await;
            }
            VoteOutcome::Ended => {
                reply(EXPIRED_TEXT.into()).await;
            }
            VoteOutcome::Applied { result, next, changed } => {
                reply(button_reply(&p, idx, result, &next)).await;
                if changed {
                    request_render(&ctx.app, &poll_id);
                }
            }
        }
        Ok(())
    }
    .await;
    if let Err(e) = run {
        bot_util::log_error!("poll vote failed", bot_core::error::log_arg(&e));
        reply(VOTE_FAILED.into()).await;
    }
    Ok(())
}

/// `poll-select:{pollId}`: the submitted selection replaces the member's votes. Reached from the poll message or from
/// the personal copy of the menu in the member's ephemeral confirmation (edited in place).
pub async fn handle_vote_select(ctx: ComponentCtx) -> BotResult {
    let (_, params) = ctx.parsed();
    let poll_id = params.first().cloned().unwrap_or_default();
    let from_ephemeral = ctx.interaction.message.flags.is_some_and(|f| f.contains(MessageFlags::EPHEMERAL));
    let acked = if from_ephemeral { ctx.defer_update().await.map_err(Some) } else { safe_defer_result(&ctx).await };
    if let Err(e) = acked {
        if let Some(e) = e {
            if is_interaction_gone(&e) {
                bot_util::log_debug!(format!("poll select: interaction gone ({})", e.code().map(|c| c.to_string()).unwrap_or_default()));
            } else {
                bot_util::log_warn!("poll select: could not acknowledge", err_arg(&e));
            }
        }
        return Ok(());
    }
    // deferUpdate leaves safeReply with followUp, which would post a new message: the ballot is edited in place.
    let c = &ctx;
    let edit = |payload: Payload| async move {
        let mut p = payload.no_mentions();
        p.ephemeral = false;
        if let Err(e) = c.edit_reply(p.to_edit()).await {
            if is_interaction_gone(&e) {
                bot_util::log_debug!(format!("poll select: interaction gone ({})", e.code().map(|c| c.to_string()).unwrap_or_default()));
            } else {
                bot_util::log_warn!("poll select: reply failed", err_arg(&e));
            }
        }
    };
    let edit = &edit;
    let run: anyhow::Result<()> = async {
        if poll_id.is_empty() {
            edit(Payload::text("This menu is no longer active.").components(Vec::new())).await;
            return Ok(());
        }
        let p = match prepare_vote(&ctx, &poll_id, None, !from_ephemeral).await? {
            Ok(p) => p,
            Err((text, terminal)) => {
                let payload = if terminal { Payload::text(text).components(Vec::new()) } else { Payload::text(text) };
                edit(payload).await;
                return Ok(());
            }
        };
        let Some(set) = normalize_selection(&ctx.values(), p.options.len(), p.max_choices) else {
            edit(Payload::text("That selection is not valid for this poll.")).await;
            return Ok(());
        };
        match apply_vote(&ctx.app, &poll_id, &ctx.user().id.to_string(), p.max_choices, VoteInput::Set(set.clone())).await? {
            VoteOutcome::Gone => edit(Payload::text("This poll no longer exists.").components(Vec::new())).await,
            VoteOutcome::Ended => edit(Payload::text(EXPIRED_TEXT).components(Vec::new())).await,
            VoteOutcome::Applied { changed, .. } => {
                let content = if set.is_empty() {
                    "🗑️ Your votes were cleared.".to_string()
                } else {
                    format!("✅ Your votes ({}/{}): {}", set.len(), p.max_choices, list_of(&p, &set))
                };
                edit(Payload::text(content).components(build_poll_components(&p, false, Some(&set)))).await;
                if changed {
                    request_render(&ctx.app, &poll_id);
                }
            }
        }
        Ok(())
    }
    .await;
    if let Err(e) = run {
        bot_util::log_error!("poll vote failed", bot_core::error::log_arg(&e));
        edit(Payload::text(VOTE_FAILED)).await;
    }
    Ok(())
}

/// `safeDefer(interaction, { ephemeral: true })` keeping the error (`Err(None)`: refused without an error).
async fn safe_defer_result(ctx: &ComponentCtx) -> Result<(), Option<bot_core::interaction::InteractionError>> {
    // Like safeDefer: an interaction that is already acknowledged counts as deferred.
    if safe_defer(&ctx.responder, true).await { Ok(()) } else { Err(None) }
}

// ── Live results (render throttle) ────────────────────────────────────────────

/// The poll render lock (`renderMutex`), shared by live renders and the final edit of [`end_poll`]. One per app.
#[derive(Default)]
pub struct PollRenderLock(pub bot_core::sync::KeyedMutex);

fn render_locked<T, F, Fut>(app: &App, poll_id: &str, f: F) -> impl std::future::Future<Output = T> + Send + 'static + use<T, F, Fut>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    app.ext::<PollRenderLock>().0.run(poll_id, f)
}

/// What a render did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderOutcome {
    Ok,
    Skip,
    /// The message or channel was deleted: the poll should end.
    Gone,
}

/// A replacement render (`__setRenderImpl`).
pub type RenderFn = Arc<dyn Fn(App, String) -> BoxFuture<'static, anyhow::Result<RenderOutcome>> + Send + Sync>;

#[derive(Default)]
struct RenderSlot {
    last_at: i64,
    token: Option<u64>,
}

/// `renderState`, `warnedAt` and the render override, one per app.
#[derive(Default)]
pub struct RenderState {
    slots: Mutex<HashMap<String, RenderSlot>>,
    next_token: std::sync::atomic::AtomicU64,
    warned_at: Mutex<HashMap<String, i64>>,
    render_impl: RwLock<Option<RenderFn>>,
}

impl RenderState {
    /// Queued renders (tests).
    pub fn queued(&self) -> usize {
        self.slots.lock().unwrap_or_else(|e| e.into_inner()).values().filter(|s| s.token.is_some()).count()
    }
}

/// `__setRenderImpl(fn)`: replaces the Discord render (tests); `None` restores it.
pub fn set_render_impl(app: &App, f: Option<RenderFn>) {
    *app.ext::<RenderState>().render_impl.write().unwrap_or_else(|e| e.into_inner()) = f;
}

fn warn_throttled(app: &App, key: &str, message: &str) {
    let now = app.clock().now_ms();
    let st = app.ext::<RenderState>();
    let mut w = st.warned_at.lock().unwrap_or_else(|e| e.into_inner());
    if now - w.get(key).copied().unwrap_or(0) < WARN_EVERY_MS {
        return;
    }
    if w.len() > 200 {
        w.retain(|_, t| now - *t < WARN_EVERY_MS);
    }
    w.insert(key.to_string(), now);
    drop(w);
    bot_util::log_warn!(message.to_string());
}

/// `requestRender(client, pollId)`: at most one edit per poll per 5 s. The first request after a quiet period renders
/// at once; requests inside the window collapse into one trailing render that reads the database when it runs. The
/// slot is cleared before the render starts, so a vote that commits meanwhile queues another render.
pub fn request_render(app: &App, poll_id: &str) {
    let st = app.ext::<RenderState>();
    let now = app.clock().now_ms();
    let (wait, token) = {
        let mut slots = st.slots.lock().unwrap_or_else(|e| e.into_inner());
        if slots.get(poll_id).is_some_and(|s| s.token.is_some()) {
            return; // a trailing render is already queued
        }
        if slots.len() > RENDER_SWEEP_AT {
            slots.retain(|id, s| s.token.is_some() || id == poll_id || now - s.last_at <= RENDER_IDLE_MS);
        }
        let token = st.next_token.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let s = slots.entry(poll_id.to_string()).or_default();
        s.token = Some(token);
        ((s.last_at + poll::RENDER_INTERVAL_MS - now).max(0), token)
    };
    let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
    let (app, id) = (app.clone(), poll_id.to_string());
    handle.spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(wait as u64)).await;
        let st = app.ext::<RenderState>();
        {
            let mut slots = st.slots.lock().unwrap_or_else(|e| e.into_inner());
            match slots.get_mut(&id) {
                Some(s) if s.token == Some(token) => {
                    s.last_at = app.clock().now_ms();
                    s.token = None;
                }
                _ => return, // cancelled
            }
        }
        let custom = st.render_impl.read().unwrap_or_else(|e| e.into_inner()).clone();
        let (a, pid) = (app.clone(), id.clone());
        let outcome = render_locked(&app, &id, move || async move {
            match custom {
                Some(f) => f(a, pid).await,
                None => render_poll_message(&a, &pid).await,
            }
        })
        .await;
        match outcome {
            Ok(RenderOutcome::Gone) => {
                if let Err(e) = end_poll(&app, &id, EndReason::Deleted, None).await {
                    bot_util::log_error!(format!("poll render failed ({id})"), bot_core::error::log_arg(&e));
                }
            }
            Ok(_) => {}
            Err(e) => bot_util::log_error!(format!("poll render failed ({id})"), bot_core::error::log_arg(&e)),
        }
    });
}

/// `cancelRender(pollId)`: drops a queued render (the poll ended).
pub fn cancel_render(app: &App, poll_id: &str) {
    app.ext::<RenderState>().slots.lock().unwrap_or_else(|e| e.into_inner()).remove(poll_id);
}

/// Where a poll's channel is.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // one per lookup, never stored
pub enum PollChannel {
    Found(GuildChannel),
    Gone,
    NoAccess,
}

/// `resolvePollChannel(client, poll)`: `Gone` only for a REST Unknown Channel (a cache miss is not a deletion).
pub async fn resolve_poll_channel(app: &App, p: &Poll) -> Result<PollChannel, DiscordError> {
    if let (Some(g), Some(c)) = (guild_id_of(&p.guild_id), channel_id_of(&p.channel_id))
        && let Some(ch) = cached_channel(app, g, c)
    {
        return Ok(PollChannel::Found(ch));
    }
    match ops(app).fetch_channel(&p.channel_id).await {
        Ok(Some(ch)) => Ok(PollChannel::Found(ch)),
        Ok(None) => Ok(PollChannel::NoAccess),
        Err(e) if e.code == Some(UNKNOWN_CHANNEL) => Ok(PollChannel::Gone),
        Err(e) if no_access_code(e.code) => Ok(PollChannel::NoAccess),
        Err(e) => Err(e),
    }
}

/// `renderPollMessage(client, pollId)`: edits the poll message with fresh counts.
pub async fn render_poll_message(app: &App, poll_id: &str) -> anyhow::Result<RenderOutcome> {
    let Some(p) = find_poll(app.db(), poll_id).await? else { return Ok(RenderOutcome::Skip) };
    // The final edit belongs to end_poll; both run under the render lock.
    let Some(mid) = p.message_id.clone().filter(|_| p.status == PollStatus::Active) else { return Ok(RenderOutcome::Skip) };
    if !guild_view::guild_available(app, &p.guild_id) {
        return Ok(RenderOutcome::Skip);
    }
    let channel = match resolve_poll_channel(app, &p).await {
        Ok(PollChannel::Found(c)) => c,
        Ok(PollChannel::Gone) => return Ok(RenderOutcome::Gone),
        Ok(PollChannel::NoAccess) => {
            warn_throttled(app, &format!("render:{poll_id}"), &format!("poll {poll_id}: cannot reach its channel to update the results"));
            return Ok(RenderOutcome::Skip);
        }
        Err(e) => return Err(e.into()),
    };
    let tally = get_tally(app.db(), &p).await?;
    let settings = app.settings().get_settings_safe(&p.guild_id).await;
    match ops(app).edit_message(&channel.id.to_string(), &mid, build_poll_payload(&p, &tally, settings.as_deref(), false)).await {
        Ok(()) => Ok(RenderOutcome::Ok),
        Err(e) if matches!(e.code, Some(UNKNOWN_MESSAGE) | Some(UNKNOWN_CHANNEL)) => Ok(RenderOutcome::Gone),
        Err(e) if no_access_code(e.code) => {
            warn_throttled(
                app,
                &format!("render:{poll_id}"),
                &format!("poll {poll_id}: cannot edit its message (Discord {})", e.code.unwrap_or_default()),
            );
            Ok(RenderOutcome::Skip)
        }
        Err(e) => Err(e.into()),
    }
}

// ── Ending ────────────────────────────────────────────────────────────────────

/// Why a poll ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    Manual,
    Timer,
    Expired,
    Deleted,
}

/// Why [`end_poll`] did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndPollError {
    NotFound,
    AlreadyEnded,
}

impl EndPollError {
    pub fn code(self) -> &'static str {
        match self {
            Self::NotFound => "NOT_FOUND",
            Self::AlreadyEnded => "ALREADY_ENDED",
        }
    }
}

/// What [`end_poll`] returns on success.
#[derive(Debug, Clone, PartialEq)]
pub struct PollEnded {
    pub poll: Option<Poll>,
    pub tally: Option<Tally>,
    pub posted: bool,
    pub message_gone: bool,
}

/// `endPoll(client, pollId, { reason, guildId })`: the conditional update picks exactly one winner among the end job,
/// `/poll end`, the delete hooks and the vote self-heal. Every Discord step after the status change is caught here.
/// The outer `Err` only when the status UPDATE fails (a database error).
pub async fn end_poll(app: &App, poll_id: &str, reason: EndReason, guild_id: Option<&str>) -> anyhow::Result<Result<PollEnded, EndPollError>> {
    let now = now_of(app).naive_utc();
    let count = match guild_id {
        Some(g) => sqlx::query(SQL_END_IN_GUILD).bind(poll_id).bind(now).bind(g).execute(app.db()).await?,
        None => sqlx::query(SQL_END).bind(poll_id).bind(now).execute(app.db()).await?,
    }
    .rows_affected();
    if count == 0 {
        let row = match guild_id {
            Some(g) => sqlx::query(SQL_EXISTS_IN_GUILD).bind(poll_id).bind(g).fetch_optional(app.db()).await?,
            None => sqlx::query(SQL_EXISTS).bind(poll_id).fetch_optional(app.db()).await?,
        };
        return Ok(Err(if row.is_some() { EndPollError::AlreadyEnded } else { EndPollError::NotFound }));
    }
    let mut out = PollEnded { poll: None, tally: None, posted: false, message_gone: false };
    if let Err(e) = finish_end(app, poll_id, reason, &mut out).await {
        bot_util::log_error!(format!("poll {poll_id}: finishing the end failed"), bot_core::error::log_arg(&e));
    }
    Ok(Ok(out))
}

async fn finish_end(app: &App, poll_id: &str, reason: EndReason, out: &mut PollEnded) -> anyhow::Result<()> {
    cancel_render(app, poll_id);
    if reason != EndReason::Timer {
        app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(end_job_key(poll_id)), ..Default::default() }).await;
    }
    let Some(p) = find_poll(app.db(), poll_id).await? else { return Ok(()) };
    out.poll = Some(p.clone());
    let tally = get_tally(app.db(), &p).await?; // read after the UPDATE: every committed vote is in
    out.tally = Some(tally.clone());
    let settings = app.settings().get_settings_safe(&p.guild_id).await;
    if reason == EndReason::Deleted {
        out.message_gone = true;
        return Ok(());
    }
    if !guild_view::guild_available(app, &p.guild_id) {
        bot_util::log_warn!(format!("poll {poll_id} ended, but its guild is unavailable: the message was not updated"));
        return Ok(());
    }

    // Final edit.
    let mut channel: Option<GuildChannel> = None;
    let mut channel_gone = false;
    match resolve_poll_channel(app, &p).await {
        Ok(PollChannel::Gone) => channel_gone = true,
        Ok(PollChannel::Found(c)) => channel = Some(c),
        Ok(PollChannel::NoAccess) => {}
        Err(e) => bot_util::log_warn!(format!("poll {poll_id}: could not look up its channel"), err_arg(&e)),
    }
    if channel_gone {
        out.message_gone = true;
    } else if let (Some(ch), Some(mid)) = (channel.clone(), p.message_id.clone()) {
        let (a, pp, t, s) = (app.clone(), p.clone(), tally.clone(), settings.clone());
        let edited = render_locked(app, poll_id, move || async move {
            ops(&a).edit_message(&ch.id.to_string(), &mid, build_poll_payload(&pp, &t, s.as_deref(), true)).await
        })
        .await;
        if let Err(e) = edited {
            match e.code {
                Some(UNKNOWN_MESSAGE) => out.message_gone = true,
                Some(UNKNOWN_CHANNEL) => {
                    channel_gone = true;
                    out.message_gone = true;
                    channel = None;
                }
                _ => bot_util::log_warn!(format!("poll {poll_id}: could not edit the final results onto its message"), err_arg(&e)),
            }
        }
    } else if channel.is_some() {
        out.message_gone = true;
    }

    // Results post: not while the module is off (no settings counts as on: posting results is harmless).
    let module_on = settings.is_none() || is_module_enabled(settings.as_deref(), Some(ModuleKey::Polls));
    if let Some(ch) = channel.filter(|_| !channel_gone && module_on)
        && guild_view::channel_usable(app, ch.guild_id, &ch, &CHANNEL_POST_NEEDS).ok
    {
        let can_ref = !out.message_gone && p.message_id.is_some() && guild_view::bot_has_in(app, ch.guild_id, &ch, Permissions::READ_MESSAGE_HISTORY);
        let embed = build_results_embed(&p, &tally, settings.as_deref(), false, !out.message_gone);
        let mut body = json!({ "embeds": [embed_json(&embed)], "allowed_mentions": { "parse": [] } });
        if can_ref && let Some(mid) = &p.message_id {
            body["message_reference"] = json!({ "message_id": mid, "fail_if_not_exists": false });
        }
        match ops(app).send_message(&p.guild_id, &ch.id.to_string(), body).await {
            Ok(_) => out.posted = true,
            Err(e) => bot_util::log_warn!(format!("poll {poll_id}: could not post the results"), err_arg(&e)),
        }
    }
    Ok(())
}

// ── Hooks and the job ─────────────────────────────────────────────────────────

/// messageDelete: the poll message was deleted, so the poll ends without a results message.
pub async fn on_message_delete(app: &App, guild_id: &str, message_id: &str) -> anyhow::Result<()> {
    let row = sqlx::query(SQL_ACTIVE_BY_MESSAGE).bind(guild_id).bind(message_id).fetch_optional(app.db()).await?;
    if let Some(id) = row.and_then(|r| r.try_get::<String, _>("id").ok()) {
        let _ = end_poll(app, &id, EndReason::Deleted, Some(guild_id)).await?;
    }
    Ok(())
}

async fn end_each(app: &App, guild_id: &str, ids: Vec<String>, what: &str) {
    for id in ids {
        if let Err(e) = end_poll(app, &id, EndReason::Deleted, Some(guild_id)).await {
            bot_util::log_error!(format!("poll {id}: ending after {what} failed"), bot_core::error::log_arg(&e));
        }
    }
}

/// messageDeleteBulk (e.g. `/purge`).
pub async fn on_message_delete_bulk(app: &App, guild_id: &str, message_ids: &[String]) -> anyhow::Result<()> {
    if message_ids.is_empty() {
        return Ok(());
    }
    let rows = sqlx::query(SQL_ACTIVE_BY_MESSAGES).bind(guild_id).bind(message_ids).fetch_all(app.db()).await?;
    end_each(app, guild_id, rows.iter().filter_map(|r| r.try_get::<String, _>("id").ok()).collect(), "a bulk delete").await;
    Ok(())
}

/// channelDelete: every active poll posted in the channel ends.
pub async fn on_channel_delete(app: &App, guild_id: &str, channel_id: &str) -> anyhow::Result<()> {
    let rows = sqlx::query(SQL_ACTIVE_IN_CHANNEL).bind(guild_id).bind(channel_id).fetch_all(app.db()).await?;
    end_each(app, guild_id, rows.iter().filter_map(|r| r.try_get::<String, _>("id").ok()).collect(), "a channel delete").await;
    Ok(())
}

/// The `poll.end` job payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndPayload {
    pub poll_id: String,
}

/// `handleEndJob(job, ctx)`: `ALREADY_ENDED` / `NOT_FOUND` are no-ops; a database failure retries.
pub async fn handle_end_job(job: Job, ctx: JobCtx) -> BotResult<JobOutcome> {
    let Some(id) = job.payload.get("pollId").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string) else {
        return Err(PermanentJobError::new("poll.end: missing pollId").into());
    };
    let _ = end_poll(&ctx.app, &id, EndReason::Timer, Some(&job.guild_id)).await?;
    Ok(JobOutcome::Done)
}

// ── Command queries ───────────────────────────────────────────────────────────

/// An autocomplete row.
#[derive(Debug, Clone, PartialEq)]
pub struct AutocompleteRow {
    pub id: String,
    pub question: String,
    pub created_at: DateTime<Utc>,
    pub status: Option<PollStatus>,
    pub channel_id: Option<String>,
}

/// Prisma's `contains` pattern for `ILIKE`: `%q%`. Prisma does not escape `%`, `_` or `\` in `q` (checked against
/// Prisma 5.22 on PostgreSQL), so neither does this.
pub fn like_contains(q: &str) -> String {
    format!("%{q}%")
}

/// `findPollsForAutocomplete({ guildId, channelId, sub, q })`: `end` = active polls in `channel_id`; `results` = any
/// poll of the guild. Filtered in the database, at most 25.
pub async fn find_polls_for_autocomplete(db: &bot_db::PgPool, guild_id: &str, channel_id: &str, sub: &str, q: &str) -> anyhow::Result<Vec<AutocompleteRow>> {
    let mut sql = String::from(r#"SELECT "id", "question", "createdAt", "status"::text AS status, "channelId" FROM "Poll" WHERE "guildId" = $1"#);
    let end = sub == "end";
    if end {
        sql.push_str(r#" AND "channelId" = $2 AND "status" = 'ACTIVE'"#);
    }
    if !q.is_empty() {
        sql.push_str(if end { r#" AND "question" ILIKE $3"# } else { r#" AND "question" ILIKE $2"# });
    }
    sql.push_str(r#" ORDER BY "createdAt" DESC LIMIT 25"#);
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(guild_id);
    if end {
        query = query.bind(channel_id);
    }
    if !q.is_empty() {
        query = query.bind(like_contains(q));
    }
    let rows = query.fetch_all(db).await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            Some(AutocompleteRow {
                id: r.try_get("id").ok()?,
                question: r.try_get("question").ok()?,
                created_at: r.try_get::<chrono::NaiveDateTime, _>("createdAt").ok()?.and_utc(),
                status: if end { None } else { r.try_get::<String, _>("status").ok().and_then(|s| PollStatus::parse(&s)) },
                channel_id: if end { None } else { r.try_get("channelId").ok() },
            })
        })
        .collect())
}

/// `listActivePolls(guildId)`: the active polls of a guild with their vote totals.
pub async fn list_active_polls(db: &bot_db::PgPool, guild_id: &str) -> anyhow::Result<(Vec<Poll>, HashMap<String, i64>)> {
    let polls = sqlx::query_as::<_, Poll>(SQL_LIST_ACTIVE).bind(guild_id).bind(limits::ACTIVE_POLLS as i64).fetch_all(db).await?;
    let mut votes = HashMap::new();
    if !polls.is_empty() {
        let ids: Vec<String> = polls.iter().map(|p| p.id.clone()).collect();
        for r in sqlx::query(SQL_VOTES_BY_POLL).bind(&ids).fetch_all(db).await? {
            if let (Ok(id), Ok(n)) = (r.try_get::<String, _>("pollId"), r.try_get::<i64, _>("n")) {
                votes.insert(id, n);
            }
        }
    }
    Ok((polls, votes))
}

/// `pollListLines(polls, votes, now)`: `/poll list` lines, one per poll.
pub fn poll_list_lines(polls: &[Poll], votes: &HashMap<String, i64>, now_ms: i64) -> Vec<String> {
    polls
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let title: String = clamp_text(&p.question, 80).chars().flat_map(|c| if c == '[' || c == ']' { vec!['\\', c] } else { vec![c] }).collect();
            let n = votes.get(&p.id).copied().unwrap_or(0);
            let end_text = match &p.ends_at {
                None => "no end time".to_string(),
                Some(e) if e.timestamp_millis() > now_ms => format!("ends {}", discord_timestamp(e, "R")),
                Some(_) => "ending…".to_string(),
            };
            let shown = match jump_url(p) {
                Some(link) => format!("[{title}]({link})"),
                None => title,
            };
            format!("**{}.** {shown} — <#{}> · {} {} · {end_text}", i + 1, p.channel_id, format_number(n as f64), plural(n, "vote"))
        })
        .collect()
}

/// The customId of a vote button (`poll:vote:{pollId}:{i}`).
pub fn vote_custom_id(poll_id: &str, i: usize) -> String {
    custom_id!("poll", "vote", poll_id, i).unwrap_or_else(|_| format!("poll:vote:{poll_id}:{i}"))
}
