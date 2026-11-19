//! Port of `src/services/applicationService.js`: Applications.
//!
//! Admin-built forms (up to 15 questions, 5 per modal page); a member fills them in through modals, and the answers
//! are posted to a review channel where a moderator accepts (roles + DM) or denies (reason + DM) them. Everything
//! lives here: the pure rules and builders, the database access for `ApplicationForm` / `ApplicationSubmission`, the
//! apply flow, the review flow and the cleanup hook bodies. The plugin only declares them and `/application` is a
//! thin shell over them.
//!
//! customIds (`src/config/customIds.js`):
//! * `application:apply:{formId}`: the panel's Apply button
//! * `application:next:{submissionId}:{page}`: the ephemeral "Continue (2/3)" button (page is 0-based)
//! * `application:accept:{submissionId}`, `application:deny:{submissionId}`: review buttons
//! * `application-modal:page:{submissionId}:{page}`: one page of questions (fields `application-q1..q5`)
//! * `application-modal:deny:{submissionId}`: the optional deny reason (field `application-note`)

use std::sync::Arc;

use bot_config::{ModuleKey, PermissionLevel, colors, limits};
use bot_core::App;
use bot_core::embed_builder::{error_embed, success_embed, warn_embed};
use bot_core::hooks::HookCtx;
use bot_core::hooks::events::{ChannelDelete, MessageDelete, RoleDelete};
use bot_core::interaction::{ComponentCtx, ModalCtx, ack_budget_ms};
use bot_core::interaction_helpers::{Interactive, Payload, cached_channel, cached_guild, check_assignable_role_cached, check_channel_usable, is_interaction_gone};
use bot_db::sqlx::{self, PgPool, Row};
use bot_db::{APPLICATION_FORM_INFO, APPLICATION_SUBMISSION_INFO, ApplicationForm, ApplicationSubmission, DbError, InsertBuilder, UpdateBuilder, delete_where};
use bot_util::async_util::with_timeout;
use bot_util::js::{trim, utf16_prefix};
use bot_util::placeholder_parser::{RenderContext, VarValue, render_placeholders};
use bot_util::text::clamp_text;
use bot_util::time::discord_timestamp_ms;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use serenity::all::{ButtonStyle, ChannelId, Embed, GuildId, InputTextStyle, Member, MessageId, ReactionType, RoleId, User, UserId};
use serenity::builder::{CreateActionRow, CreateButton, CreateEmbed, CreateEmbedFooter, CreateInputText, CreateModal};

use crate::io::io;

// ── Constants ─────────────────────────────────────────────────────────────────

/// `APPLICATION`.
pub mod application {
    /// Questions per modal page (Discord allows 5 inputs per modal).
    pub const PAGE_SIZE: usize = 5;
    /// Form name; the modal title "{name} (3/3)" must fit Discord's 45.
    pub const NAME_MAX: usize = 32;
    /// A question label is a modal input label (Discord max 45).
    pub const LABEL_MAX: usize = 45;
    /// Modal input placeholder (Discord max 100).
    pub const PLACEHOLDER_MAX: usize = 100;
    pub const DESCRIPTION_MAX: usize = 1000;
    /// acceptMessage / denyMessage.
    pub const MESSAGE_MAX: usize = 1000;
    /// The deny reason.
    pub const NOTE_MAX: usize = 500;
    /// One year.
    pub const COOLDOWN_MAX_HOURS: i64 = 8760;
    /// Longest answer to a SHORT question.
    pub const SHORT_MAX: usize = 300;
    /// Longest answer to a PARAGRAPH question.
    pub const PARAGRAPH_MAX: usize = 1000;
    /// Every answer is cut to ANSWER_BUDGET / (number of questions), so a review post always fits one message.
    pub const ANSWER_BUDGET: usize = 3800;
    /// The "Accepted/Denied by" field added to a reviewed post.
    pub const OUTCOME_MAX: usize = 900;
    pub const DM_TIMEOUT_MS: u64 = 5_000;
    pub const NO_ANSWER: &str = "*No answer*";
    pub const APPLY_LABEL: &str = "Apply";
    pub const ACCEPT_EMOJI: &str = "✅";
    pub const DENY_EMOJI: &str = "❌";
}

use application as A;

pub const DEFAULT_ACCEPT_MESSAGE: &str = "Your application for **{form}** in **{server}** was accepted. Welcome aboard!";
pub const DEFAULT_DENY_MESSAGE: &str = "Your application for **{form}** in **{server}** was not accepted this time.";

/// The modal's field ids, one per question slot on a page.
pub const QUESTION_FIELDS: [&str; 5] = ["application-q1", "application-q2", "application-q3", "application-q4", "application-q5"];
/// The deny form's field id.
pub const NOTE_FIELD: &str = "application-note";

/// Unknown channel / message / member / user, missing access or permission: the target is gone, nothing to retry.
const GONE_CODES: [u64; 6] = [10003, 10007, 10008, 10013, 50001, 50013];

/// `formLock`: per form (config writes) and per form+member (draft creation), one per app.
#[derive(Default)]
pub struct FormLock(pub bot_core::sync::KeyedMutex);

fn form_lock<T, F, Fut>(app: &App, key: &str, f: F) -> impl std::future::Future<Output = T> + Send + 'static + use<T, F, Fut>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    app.ext::<FormLock>().0.run(key, f)
}

fn parse_id(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|v| *v != 0)
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

/// A question's answer box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionStyle {
    Short,
    Paragraph,
}

impl QuestionStyle {
    pub fn as_str(self) -> &'static str {
        match self {
            QuestionStyle::Short => "SHORT",
            QuestionStyle::Paragraph => "PARAGRAPH",
        }
    }
}

/// One question: `{ label, style, required, placeholder? }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub label: String,
    pub style: QuestionStyle,
    pub required: bool,
    pub placeholder: Option<String>,
}

impl Question {
    /// The stored JSON (`placeholder` only when set).
    pub fn to_json(&self) -> Value {
        let mut v = json!({ "label": self.label, "style": self.style.as_str(), "required": self.required });
        if let Some(p) = &self.placeholder {
            v["placeholder"] = json!(p);
        }
        v
    }
}

/// The questions as the stored JSON array.
pub fn questions_json(questions: &[Question]) -> Value {
    Value::Array(questions.iter().map(Question::to_json).collect())
}

/// `parseQuestions(raw)`: the valid questions of a form's JSON column, at most the limit. Anything malformed is
/// dropped, so a hand-edited row can never break the modal builder.
pub fn parse_questions(raw: &Value) -> Vec<Question> {
    let mut out = Vec::new();
    for q in raw.as_array().into_iter().flatten() {
        let label = q.get("label").and_then(Value::as_str).map(|l| utf16_prefix(trim(l), A::LABEL_MAX).to_string()).unwrap_or_default();
        if label.is_empty() {
            continue;
        }
        let placeholder = q.get("placeholder").and_then(Value::as_str).map(|p| utf16_prefix(trim(p), A::PLACEHOLDER_MAX).to_string()).unwrap_or_default();
        out.push(Question {
            label,
            style: if q.get("style").and_then(Value::as_str) == Some("PARAGRAPH") { QuestionStyle::Paragraph } else { QuestionStyle::Short },
            required: q.get("required") != Some(&Value::Bool(false)),
            placeholder: (!placeholder.is_empty()).then_some(placeholder),
        });
        if out.len() >= limits::APPLICATION_QUESTIONS {
            break;
        }
    }
    out
}

/// `pageCount(count)`: how many modal pages `count` questions take (at least 1).
pub fn page_count(count: usize) -> usize {
    count.div_ceil(A::PAGE_SIZE).max(1)
}

/// `pageSlice(questions, page)`: the questions on a 0-based page.
pub fn page_slice<T>(questions: &[T], page: usize) -> &[T] {
    let start = (page * A::PAGE_SIZE).min(questions.len());
    let end = ((page + 1) * A::PAGE_SIZE).min(questions.len());
    &questions[start..end]
}

/// `answerLimit(style, total)`: the longest answer to a question of `style` on a form with `total` questions.
pub fn answer_limit(style: QuestionStyle, total: usize) -> usize {
    let own = if style == QuestionStyle::Paragraph { A::PARAGRAPH_MAX } else { A::SHORT_MAX };
    own.min(A::ANSWER_BUDGET / total.max(1)).max(1)
}

/// `cleanAnswer(raw, limit)`: trimmed and cut to `limit` characters.
pub fn clean_answer(raw: Option<&str>, limit: usize) -> String {
    utf16_prefix(trim(raw.unwrap_or("")), limit).to_string()
}

/// `normalizeAnswers(questions, stored)`: one string per current question. An answer counts only for the question
/// that was asked.
pub fn normalize_answers(questions: &[Question], stored: &Value) -> Vec<String> {
    let list = stored.as_array();
    questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let a = list.and_then(|l| l.get(i));
            match (a.and_then(|a| a.get("question")).and_then(Value::as_str), a.and_then(|a| a.get("answer")).and_then(Value::as_str)) {
                (Some(question), Some(answer)) if question == q.label => answer.to_string(),
                _ => String::new(),
            }
        })
        .collect()
}

/// `mergeAnswers(questions, stored, page, values)`: the stored shape `[{ question, answer }]` with one page written
/// over `stored`.
pub fn merge_answers(questions: &[Question], stored: &Value, page: usize, values: &[String]) -> Value {
    let mut current = normalize_answers(questions, stored);
    for (j, v) in values.iter().enumerate() {
        let i = page * A::PAGE_SIZE + j;
        if i < current.len() {
            current[i] = v.clone();
        }
    }
    Value::Array(questions.iter().zip(current).map(|(q, a)| json!({ "question": q.label, "answer": a })).collect())
}

/// `missingRequired(questions, answers)`: indexes of required questions without an answer.
pub fn missing_required(questions: &[Question], answers: &[String]) -> Vec<usize> {
    questions.iter().enumerate().filter(|(i, q)| q.required && trim(answers.get(*i).map(String::as_str).unwrap_or("")).is_empty()).map(|(i, _)| i).collect()
}

/// `isClearWord(value)`: `none`, `clear` or `default` (any case) means "remove it".
pub fn is_clear_word(value: &str) -> bool {
    matches!(bot_util::js::lower(trim(value)).as_str(), "none" | "clear" | "default")
}

/// Why an apply attempt (or a form write) was refused: the JS `{ ok: false, code, data }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    ModuleOff,
    FormNotFound,
    FormDisabled,
    NotSetUp,
    ReviewChannel { reason: String },
    MissingRole { role_ids: Vec<String> },
    Pending,
    Cooldown { until: DateTime<Utc> },
    Slow,
    ServerOnly,
    Error,
    FormLimit,
    NameTaken,
    QuestionLimit,
    QuestionDup,
    QuestionNotFound,
    RoleLimit,
}

impl Refusal {
    /// The JS code.
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::ModuleOff => "MODULE_OFF",
            Refusal::FormNotFound => "FORM_NOT_FOUND",
            Refusal::FormDisabled => "FORM_DISABLED",
            Refusal::NotSetUp => "NOT_SET_UP",
            Refusal::ReviewChannel { .. } => "REVIEW_CHANNEL",
            Refusal::MissingRole { .. } => "MISSING_ROLE",
            Refusal::Pending => "PENDING",
            Refusal::Cooldown { .. } => "COOLDOWN",
            Refusal::Slow => "SLOW",
            Refusal::ServerOnly => "SERVER_ONLY",
            Refusal::Error => "ERROR",
            Refusal::FormLimit => "FORM_LIMIT",
            Refusal::NameTaken => "NAME_TAKEN",
            Refusal::QuestionLimit => "QUESTION_LIMIT",
            Refusal::QuestionDup => "QUESTION_DUP",
            Refusal::QuestionNotFound => "QUESTION_NOT_FOUND",
            Refusal::RoleLimit => "ROLE_LIMIT",
        }
    }
}

/// The facts [`judge_apply`] decides on (the caller loads them).
#[derive(Debug, Clone)]
pub struct ApplyFacts<'a> {
    pub form: &'a ApplicationForm,
    pub member_role_ids: &'a [String],
    pub review_channel_problem: Option<String>,
    pub pending: bool,
    pub last_denied_at: Option<DateTime<Utc>>,
    pub now: DateTime<Utc>,
}

/// `judgeApply(facts)`: whether a member may start (or resume) an application.
pub fn judge_apply(f: &ApplyFacts<'_>) -> Result<(), Refusal> {
    let form = f.form;
    if !form.enabled {
        return Err(Refusal::FormDisabled);
    }
    if form.review_channel_id.as_deref().is_none_or(str::is_empty) || parse_questions(&form.questions).is_empty() {
        return Err(Refusal::NotSetUp);
    }
    if let Some(reason) = f.review_channel_problem.clone().filter(|r| !r.is_empty()) {
        return Err(Refusal::ReviewChannel { reason });
    }
    if !form.required_role_ids.is_empty() && !form.required_role_ids.iter().any(|id| f.member_role_ids.contains(id)) {
        return Err(Refusal::MissingRole { role_ids: form.required_role_ids.clone() });
    }
    if f.pending {
        return Err(Refusal::Pending);
    }
    if let Some(at) = f.last_denied_at
        && form.cooldown_hours > 0
    {
        let until = at + chrono::Duration::milliseconds(i64::from(form.cooldown_hours) * 3_600_000);
        if until > f.now {
            return Err(Refusal::Cooldown { until });
        }
    }
    Ok(())
}

fn module_label(key: ModuleKey) -> &'static str {
    bot_config::MODULES.iter().find(|m| m.key == key).map(|m| m.label).unwrap_or("Applications")
}

/// `moduleOffEmbed()`: the warning every member-facing path answers with while the module is off (the router's
/// wording).
pub fn module_off_embed() -> CreateEmbed {
    let text = format!(
        "The **{}** module is turned off on this server. An admin can enable it with `/module enable applications`.",
        module_label(ModuleKey::Applications)
    );
    warn_embed("Module Disabled", Some(&text), None)
}

fn ts(at: DateTime<Utc>, style: &str) -> String {
    discord_timestamp_ms(at.timestamp_millis() as f64, style)
}

/// `refusalEmbed(code, data)`: the embed explaining why an apply attempt was refused.
pub fn refusal_embed(r: &Refusal) -> CreateEmbed {
    let e = |t: &str, d: &str| error_embed(t, Some(d), None);
    match r {
        Refusal::ModuleOff => module_off_embed(),
        Refusal::FormNotFound => e("Application Not Found", "Pick an application from the list."),
        Refusal::FormDisabled => e("Applications Closed", "This application is not open right now."),
        Refusal::NotSetUp => e("Not Ready", "This application is not set up yet. An admin needs to set a review channel and add questions."),
        Refusal::ReviewChannel { reason } => {
            let reason = if reason.is_empty() { "The review channel is not usable." } else { reason.as_str() };
            e("Cannot Apply Right Now", &format!("{reason} Please tell an admin."))
        }
        Refusal::MissingRole { role_ids } => {
            let list: Vec<String> = role_ids.iter().map(|id| format!("<@&{id}>")).collect();
            e("Not Eligible", &format!("You need one of these roles to apply: {}.", list.join(", ")))
        }
        Refusal::Pending => e("Already Applied", "Your application is waiting for review. You will get a message when it has been decided."),
        Refusal::Cooldown { until } => e("Please Wait", &format!("Your last application was denied. You can apply again {}.", ts(*until, "R"))),
        Refusal::Slow => e("Please Try Again", "That took too long. Please try again in a moment."),
        Refusal::ServerOnly => e("Server Only", "Applications can only be used in a server."),
        _ => e("Something Went Wrong", "The application could not be started. Please try again in a moment."),
    }
}

// ── Builders ──────────────────────────────────────────────────────────────────

fn emoji(s: &str) -> ReactionType {
    ReactionType::Unicode(s.to_string())
}

fn cid(prefix: &str, params: &[String]) -> String {
    bot_config::build_custom_id(prefix, params).unwrap_or_default()
}

/// `buildApplyModal({ form, questions, submission, page })` as the JSON Discord receives: one page of questions,
/// prefilled with what the member already answered.
pub fn apply_modal_json(form_name: &str, questions: &[Question], submission_id: &str, stored_answers: &Value, page: usize) -> Value {
    let total = page_count(questions.len());
    let title = clamp_text(&if total > 1 { format!("{form_name} ({}/{total})", page + 1) } else { form_name.to_string() }, 45);
    let saved = normalize_answers(questions, stored_answers);
    let rows: Vec<Value> = page_slice(questions, page)
        .iter()
        .enumerate()
        .map(|(j, q)| {
            let limit = answer_limit(q.style, questions.len());
            let mut input = json!({
                "type": 4,
                "custom_id": QUESTION_FIELDS[j],
                "label": clamp_text(&q.label, A::LABEL_MAX),
                "style": if q.style == QuestionStyle::Paragraph { 2 } else { 1 },
                "required": q.required,
                "max_length": limit,
            });
            if let Some(p) = &q.placeholder {
                input["placeholder"] = json!(clamp_text(p, A::PLACEHOLDER_MAX));
            }
            if let Some(prev) = saved.get(page * A::PAGE_SIZE + j).filter(|p| !p.is_empty()) {
                input["value"] = json!(utf16_prefix(prev, limit));
            }
            json!({ "type": 1, "components": [input] })
        })
        .collect();
    json!({ "custom_id": cid("application-modal", &["page".into(), submission_id.into(), page.to_string()]), "title": title, "components": rows })
}

/// A modal JSON (as built here) as a serenity modal.
pub fn modal_from_json(json: &Value) -> CreateModal {
    let rows = json["components"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| {
            let i = &row["components"][0];
            let style = if i["style"] == 2 { InputTextStyle::Paragraph } else { InputTextStyle::Short };
            let mut input = CreateInputText::new(style, i["label"].as_str().unwrap_or_default(), i["custom_id"].as_str().unwrap_or_default())
                .required(i["required"].as_bool().unwrap_or(false))
                .max_length(i["max_length"].as_u64().unwrap_or(4000) as u16);
            if let Some(p) = i["placeholder"].as_str() {
                input = input.placeholder(p);
            }
            if let Some(v) = i["value"].as_str() {
                input = input.value(v);
            }
            CreateActionRow::InputText(input)
        })
        .collect();
    CreateModal::new(json["custom_id"].as_str().unwrap_or_default(), json["title"].as_str().unwrap_or_default()).components(rows)
}

/// `buildApplyModal(...)` as a serenity modal.
pub fn build_apply_modal(form_name: &str, questions: &[Question], submission_id: &str, stored_answers: &Value, page: usize) -> CreateModal {
    modal_from_json(&apply_modal_json(form_name, questions, submission_id, stored_answers, page))
}

/// `buildDenyModal(submissionId)` as JSON: one optional reason.
pub fn deny_modal_json(submission_id: &str) -> Value {
    json!({
        "custom_id": cid("application-modal", &["deny".into(), submission_id.into()]),
        "title": "Deny application",
        "components": [{ "type": 1, "components": [{
            "type": 4, "custom_id": NOTE_FIELD, "label": "Reason (optional, sent to the applicant)", "style": 2, "required": false, "max_length": A::NOTE_MAX,
        }] }],
    })
}

/// `buildDenyModal(submissionId)`.
pub fn build_deny_modal(submission_id: &str) -> CreateModal {
    modal_from_json(&deny_modal_json(submission_id))
}

/// `continueRow(submissionId, page, total, label)`: a modal cannot open another modal, so the next page waits behind
/// a button.
pub fn continue_row(submission_id: &str, page: usize, total: usize, label: Option<&str>) -> CreateActionRow {
    let label = label.map(String::from).unwrap_or_else(|| format!("Continue ({}/{total})", page + 1));
    CreateActionRow::Buttons(vec![
        CreateButton::new(cid("application", &["next".into(), submission_id.into(), page.to_string()])).label(label).emoji(emoji("📝")).style(ButtonStyle::Primary),
    ])
}

/// `reviewRow(submissionId)`: Accept / Deny under a review post.
pub fn review_row(submission_id: &str) -> CreateActionRow {
    CreateActionRow::Buttons(vec![
        CreateButton::new(cid("application", &["accept".into(), submission_id.into()])).label("Accept").emoji(emoji(A::ACCEPT_EMOJI)).style(ButtonStyle::Success),
        CreateButton::new(cid("application", &["deny".into(), submission_id.into()])).label("Deny").emoji(emoji(A::DENY_EMOJI)).style(ButtonStyle::Danger),
    ])
}

/// `applyCustomId(formId)`: the customId of the Apply button of a form.
pub fn apply_custom_id(form_id: &str) -> String {
    cid("application", &["apply".into(), form_id.into()])
}

/// `buildApplyButton(form, label)`: an Apply button (captcha's fallback "Fill out the form" is the same button with
/// another label).
pub fn build_apply_button(form_id: &str, label: Option<&str>) -> CreateButton {
    CreateButton::new(apply_custom_id(form_id)).label(label.unwrap_or(A::APPLY_LABEL)).emoji(emoji("📝")).style(ButtonStyle::Primary)
}

/// `buildPanelPayload(form, guild)`: the panel message with its Apply button. `ctx` renders the description's
/// placeholders (`{ guild }`).
pub fn build_panel_payload(form: &ApplicationForm, ctx: &RenderContext) -> Payload {
    let count = parse_questions(&form.questions).len();
    let rendered = form.description.as_deref().filter(|d| !d.is_empty()).map(|d| render_placeholders(d, ctx, None)).filter(|d| !d.is_empty());
    let description = clamp_text(rendered.as_deref().unwrap_or("Press **Apply** to fill in the application."), 4000);
    let embed = CreateEmbed::new()
        .color(colors::INFO)
        .title(clamp_text(&format!("📝 {}", form.name), 256))
        .description(description)
        .footer(CreateEmbedFooter::new(format!("{count} question{}", if count == 1 { "" } else { "s" })));
    Payload::embed(embed).components(vec![CreateActionRow::Buttons(vec![build_apply_button(&form.id, None)])]).no_mentions()
}

/// `user.displayAvatarURL({ size: 128 })` (discord.js's default `webp`, animated as `gif`).
pub fn avatar_url_128(user: &User) -> String {
    match &user.avatar {
        Some(h) => {
            let h = h.to_string();
            let ext = if h.starts_with("a_") { "gif" } else { "webp" };
            format!("https://cdn.discordapp.com/avatars/{}/{h}.{ext}?size=128", user.id)
        }
        None => bot_core::adapters::default_avatar_url(user.id, &bot_core::adapters::discriminator(user)),
    }
}

/// What a review post shows about the applicant.
#[derive(Debug, Clone)]
pub struct Applicant {
    pub id: String,
    pub username: String,
    pub created_ms: f64,
    pub joined_ms: Option<f64>,
    pub avatar_url: Option<String>,
}

impl Applicant {
    pub fn of(user: &User, member: Option<&Member>) -> Self {
        Self {
            id: user.id.to_string(),
            username: user.name.clone(),
            created_ms: bot_core::adapters::snowflake_ms(user.id.get()),
            joined_ms: member.and_then(|m| m.joined_at).map(|t| t.timestamp_millis() as f64),
            avatar_url: Some(avatar_url_128(user)),
        }
    }
}

/// `buildReviewEmbed({ form, answers, user, member, submissionId, submittedAt })`: who applied, when their account
/// and membership started, and every answer.
pub fn build_review_embed(form_name: &str, answers: &Value, who: &Applicant, submission_id: &str, submitted_at: DateTime<Utc>) -> CreateEmbed {
    let mut lines = vec![
        format!("**Applicant:** <@{}> · `{}`", who.id, who.username.replace('`', "")),
        format!("**Account created:** {}", discord_timestamp_ms(who.created_ms, "R")),
    ];
    if let Some(j) = who.joined_ms.filter(|j| *j != 0.0) {
        lines.push(format!("**Joined the server:** {}", discord_timestamp_ms(j, "R")));
    }
    lines.push(format!("**Submitted:** {}", ts(submitted_at, "f")));
    let list: Vec<&Value> = answers.as_array().map(|a| a.iter().take(limits::APPLICATION_QUESTIONS).collect()).unwrap_or_default();
    let cap = (A::ANSWER_BUDGET / list.len().max(1)).clamp(1, 1024);
    let mut embed = CreateEmbed::new()
        .color(colors::INFO)
        .title(clamp_text(&format!("📝 {form_name}"), 256))
        .description(lines.join("\n"))
        .footer(CreateEmbedFooter::new(format!("Pending review · {submission_id}")))
        .timestamp(serenity::all::Timestamp::from(submitted_at));
    if let Some(a) = who.avatar_url.as_deref().filter(|a| !a.is_empty()) {
        embed = embed.thumbnail(a);
    }
    for (i, a) in list.iter().enumerate() {
        let text = trim(a.get("answer").and_then(Value::as_str).unwrap_or(""));
        let question = a.get("question").and_then(Value::as_str).unwrap_or("Question");
        let value = if text.is_empty() { A::NO_ANSWER.to_string() } else { clamp_text(text, cap) };
        embed = embed.field(clamp_text(&format!("{}. {question}", i + 1), 256), value, false);
    }
    embed
}

/// The decision on a submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Accepted,
    Denied,
}

impl Decision {
    pub fn status(self) -> &'static str {
        match self {
            Decision::Accepted => "ACCEPTED",
            Decision::Denied => "DENIED",
        }
    }
}

/// What [`build_outcome_embed`] adds to a review post.
#[derive(Debug, Clone, Copy)]
pub struct Outcome<'a> {
    pub form_name: Option<&'a str>,
    pub decision: Decision,
    pub reviewer_id: &'a str,
    pub note: Option<&'a str>,
    pub extra: Option<&'a str>,
    pub submission_id: &'a str,
    pub at: DateTime<Utc>,
}

/// `buildOutcomeEmbed(source, { form, status, reviewerId, note, extra, submissionId, at })`: the review post after a
/// decision: recoloured, the outcome added and the footer changed.
pub fn build_outcome_embed(source: Option<&Embed>, o: &Outcome<'_>) -> CreateEmbed {
    let Outcome { form_name, decision, reviewer_id, note, extra, submission_id, at } = *o;
    let accepted = decision == Decision::Accepted;
    let embed = match source {
        Some(e) => CreateEmbed::from(e.clone()),
        None => CreateEmbed::new().title(clamp_text(&format!("📝 {}", form_name.unwrap_or("Application")), 256)),
    };
    let head = format!(
        "{} **{}** by <@{reviewer_id}> {}",
        if accepted { A::ACCEPT_EMOJI } else { A::DENY_EMOJI },
        if accepted { "Accepted" } else { "Denied" },
        ts(at, "R")
    );
    let mut parts = vec![head];
    if let Some(n) = note.filter(|n| !n.is_empty()) {
        parts.push(format!("**Reason:** {n}"));
    }
    if let Some(x) = extra.filter(|x| !x.is_empty()) {
        parts.push(x.to_string());
    }
    embed
        .color(if accepted { colors::SUCCESS } else { colors::ERROR })
        .footer(CreateEmbedFooter::new(format!("{} · {submission_id}", if accepted { "Accepted" } else { "Denied" })))
        .field("Outcome", clamp_text(&parts.join("\n"), A::OUTCOME_MAX), false)
}

// ── Database ──────────────────────────────────────────────────────────────────

/// Every raw statement of this service, by name (each is run in a rolled-back transaction by the tests).
pub const RAW_SQL: &[(&str, &str)] = &[
    ("listForms", r#"SELECT * FROM "ApplicationForm" WHERE "guildId" = $1 ORDER BY "createdAt" ASC"#),
    (
        "pendingByForm",
        r#"SELECT "formId", COUNT(*)::bigint AS "n" FROM "ApplicationSubmission" WHERE "guildId" = $1 AND "status" = 'PENDING'::"ApplicationStatus" AND "reviewMessageId" IS NOT NULL GROUP BY "formId""#,
    ),
    ("formById", r#"SELECT * FROM "ApplicationForm" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#),
    ("formByName", r#"SELECT * FROM "ApplicationForm" WHERE "guildId" = $1 AND "name"::text ILIKE $2 LIMIT 1"#),
    (
        "formsForAutocomplete",
        r#"SELECT "id", "name", "enabled" FROM "ApplicationForm" WHERE "guildId" = $1 AND ($2::boolean = false OR "enabled" = true) AND ($3::text IS NULL OR "name"::text ILIKE $3) ORDER BY "createdAt" ASC LIMIT 25"#,
    ),
    ("countForms", r#"SELECT COUNT(*)::bigint AS "n" FROM "ApplicationForm" WHERE "guildId" = $1"#),
    (
        "nameClash",
        r#"SELECT "id" FROM "ApplicationForm" WHERE "guildId" = $1 AND "name"::text ILIKE $2 AND ($3::text IS NULL OR "id" <> $3) LIMIT 1"#,
    ),
    (
        "pendingOfMember",
        r#"SELECT "id" FROM "ApplicationSubmission" WHERE "formId" = $1 AND "userId" = $2 AND "status" = 'PENDING'::"ApplicationStatus" AND "reviewMessageId" IS NOT NULL LIMIT 1"#,
    ),
    (
        "lastDenied",
        r#"SELECT "reviewedAt" FROM "ApplicationSubmission" WHERE "formId" = $1 AND "userId" = $2 AND "status" = 'DENIED'::"ApplicationStatus" AND "reviewedAt" IS NOT NULL ORDER BY "reviewedAt" DESC LIMIT 1"#,
    ),
    (
        "draftOfMember",
        r#"SELECT * FROM "ApplicationSubmission" WHERE "formId" = $1 AND "userId" = $2 AND "status" = 'DRAFT'::"ApplicationStatus" LIMIT 1"#,
    ),
    (
        "ownDraft",
        r#"SELECT * FROM "ApplicationSubmission" WHERE "id" = $1 AND "guildId" = $2 AND "userId" = $3 AND "status" = 'DRAFT'::"ApplicationStatus" LIMIT 1"#,
    ),
    ("formOf", r#"SELECT * FROM "ApplicationForm" WHERE "id" = $1 LIMIT 1"#),
    (
        "saveAnswers",
        r#"UPDATE "ApplicationSubmission" SET "answers" = $1, "updatedAt" = (now() AT TIME ZONE 'UTC') WHERE "id" = $2 AND "userId" = $3 AND "status" = 'DRAFT'::"ApplicationStatus""#,
    ),
    (
        "claimSubmit",
        r#"UPDATE "ApplicationSubmission" SET "status" = 'PENDING'::"ApplicationStatus", "submittedAt" = $1, "updatedAt" = (now() AT TIME ZONE 'UTC') WHERE "id" = $2 AND "userId" = $3 AND "status" = 'DRAFT'::"ApplicationStatus""#,
    ),
    (
        "revertSubmit",
        r#"UPDATE "ApplicationSubmission" SET "status" = 'DRAFT'::"ApplicationStatus", "submittedAt" = NULL, "updatedAt" = (now() AT TIME ZONE 'UTC') WHERE "id" = $1 AND "status" = 'PENDING'::"ApplicationStatus""#,
    ),
    (
        "setReviewMessage",
        r#"UPDATE "ApplicationSubmission" SET "reviewMessageId" = $1, "updatedAt" = (now() AT TIME ZONE 'UTC') WHERE "id" = $2"#,
    ),
    ("submissionById", r#"SELECT * FROM "ApplicationSubmission" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#),
    (
        "claimDecision",
        r#"UPDATE "ApplicationSubmission" SET "status" = $1::"ApplicationStatus", "reviewerId" = $2, "reviewedAt" = $3, "reviewNote" = $4, "updatedAt" = (now() AT TIME ZONE 'UTC') WHERE "id" = $5 AND "status" = 'PENDING'::"ApplicationStatus""#,
    ),
    (
        "pendingCount",
        r#"SELECT COUNT(*)::bigint AS "n" FROM "ApplicationSubmission" WHERE "formId" = $1 AND "status" = 'PENDING'::"ApplicationStatus" AND "reviewMessageId" IS NOT NULL"#,
    ),
    (
        "forgetPanelMessage",
        r#"UPDATE "ApplicationForm" SET "panelMessageId" = NULL, "panelChannelId" = NULL, "updatedAt" = (now() AT TIME ZONE 'UTC') WHERE "guildId" = $1 AND "panelMessageId" = $2"#,
    ),
    (
        "forgetReviewMessage",
        r#"UPDATE "ApplicationSubmission" SET "reviewMessageId" = NULL, "updatedAt" = (now() AT TIME ZONE 'UTC') WHERE "guildId" = $1 AND "reviewMessageId" = $2"#,
    ),
    (
        "formsInChannel",
        r#"SELECT "id", "reviewChannelId", "panelChannelId" FROM "ApplicationForm" WHERE "guildId" = $1 AND ("reviewChannelId" = $2 OR "panelChannelId" = $2)"#,
    ),
    (
        "orphanReviews",
        r#"UPDATE "ApplicationSubmission" SET "reviewMessageId" = NULL, "updatedAt" = (now() AT TIME ZONE 'UTC') WHERE "formId" = ANY($1) AND "reviewMessageId" IS NOT NULL"#,
    ),
    (
        "clearReviewChannel",
        r#"UPDATE "ApplicationForm" SET "reviewChannelId" = NULL, "updatedAt" = (now() AT TIME ZONE 'UTC') WHERE "id" = ANY($1)"#,
    ),
    (
        "clearPanel",
        r#"UPDATE "ApplicationForm" SET "panelChannelId" = NULL, "panelMessageId" = NULL, "updatedAt" = (now() AT TIME ZONE 'UTC') WHERE "id" = ANY($1)"#,
    ),
    (
        "formsWithRole",
        r#"SELECT "id", "acceptRoleIds", "requiredRoleIds" FROM "ApplicationForm" WHERE "guildId" = $1 AND ($2 = ANY("acceptRoleIds") OR $2 = ANY("requiredRoleIds"))"#,
    ),
    (
        "setRoleLists",
        r#"UPDATE "ApplicationForm" SET "acceptRoleIds" = $1, "requiredRoleIds" = $2, "enabled" = COALESCE($3::boolean, "enabled"), "updatedAt" = (now() AT TIME ZONE 'UTC') WHERE "id" = $4"#,
    ),
];

fn sql(name: &str) -> &'static str {
    RAW_SQL.iter().find(|(n, _)| *n == name).map(|(_, s)| *s).unwrap_or("SELECT 1")
}

fn db(e: sqlx::Error) -> anyhow::Error {
    anyhow::Error::new(DbError::from(e))
}

/// Prisma's `{ equals: x, mode: 'insensitive' }` on PostgreSQL: `ILIKE` with `\`, `%` and `_` escaped (checked
/// against Prisma: `axb%` does not find `a_b%`).
fn ilike_equals(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Prisma's `{ contains: x, mode: 'insensitive' }` on PostgreSQL: `ILIKE '%x%'` with the value unescaped (checked
/// against Prisma: `_` and `%` act as wildcards).
fn ilike_contains(value: &str) -> String {
    format!("%{value}%")
}

/// A form row with its number of submissions waiting for review (`listForms`).
#[derive(Debug, Clone)]
pub struct FormWithPending {
    pub form: ApplicationForm,
    pub pending: i64,
}

/// `listForms(guildId)`: every form of a guild with its number of submissions waiting for review.
pub async fn list_forms(pool: &PgPool, guild_id: &str) -> anyhow::Result<Vec<FormWithPending>> {
    let forms: Vec<ApplicationForm> = sqlx::query_as(sql("listForms")).bind(guild_id).fetch_all(pool).await.map_err(db)?;
    if forms.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(sql("pendingByForm")).bind(guild_id).fetch_all(pool).await.map_err(db)?;
    let counts: Vec<(String, i64)> = rows.iter().map(|r| (r.try_get::<String, _>("formId").unwrap_or_default(), r.try_get::<i64, _>("n").unwrap_or(0))).collect();
    Ok(forms
        .into_iter()
        .map(|f| {
            let pending = counts.iter().find(|(id, _)| *id == f.id).map(|(_, n)| *n).unwrap_or(0);
            FormWithPending { form: f, pending }
        })
        .collect())
}

fn ref_text(r: &str) -> String {
    utf16_prefix(trim(r), 100).to_string()
}

/// `findForm(guildId, ref)`: a form of this guild by id, or else by name (case-insensitive).
pub async fn find_form(pool: &PgPool, guild_id: &str, reference: &str) -> anyhow::Result<Option<ApplicationForm>> {
    let text = ref_text(reference);
    if text.is_empty() {
        return Ok(None);
    }
    let by_id: Option<ApplicationForm> = sqlx::query_as(sql("formById")).bind(&text).bind(guild_id).fetch_optional(pool).await.map_err(db)?;
    if by_id.is_some() {
        return Ok(by_id);
    }
    sqlx::query_as(sql("formByName")).bind(guild_id).bind(ilike_equals(&text)).fetch_optional(pool).await.map_err(db)
}

/// `findFormById(guildId, id)`: a form of this guild by id only (`None` when it is gone or belongs to another guild).
pub async fn find_form_by_id(pool: &PgPool, guild_id: &str, id: &str) -> anyhow::Result<Option<ApplicationForm>> {
    let text = ref_text(id);
    if text.is_empty() {
        return Ok(None);
    }
    sqlx::query_as(sql("formById")).bind(&text).bind(guild_id).fetch_optional(pool).await.map_err(db)
}

/// One autocomplete row (`{ id, name, enabled }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormChoice {
    pub id: String,
    pub name: String,
    pub enabled: bool,
}

/// `findFormsForAutocomplete(guildId, query, { enabledOnly })`: choices for the `form` option.
pub async fn find_forms_for_autocomplete(pool: &PgPool, guild_id: &str, query: &str, enabled_only: bool) -> anyhow::Result<Vec<FormChoice>> {
    let q = ref_text(query);
    let pattern = (!q.is_empty()).then(|| ilike_contains(&q));
    let rows = sqlx::query(sql("formsForAutocomplete")).bind(guild_id).bind(enabled_only).bind(pattern).fetch_all(pool).await.map_err(db)?;
    Ok(rows
        .iter()
        .map(|r| FormChoice {
            id: r.try_get("id").unwrap_or_default(),
            name: r.try_get("name").unwrap_or_default(),
            enabled: r.try_get("enabled").unwrap_or(false),
        })
        .collect())
}

async fn name_clash(pool: &PgPool, guild_id: &str, name: &str, except: Option<&str>) -> anyhow::Result<bool> {
    let row = sqlx::query(sql("nameClash")).bind(guild_id).bind(ilike_equals(name)).bind(except).fetch_optional(pool).await.map_err(db)?;
    Ok(row.is_some())
}

fn is_unique_violation(e: &anyhow::Error) -> bool {
    e.downcast_ref::<DbError>().is_some_and(DbError::is_unique_violation)
}

/// `createForm({ guildId, name, description, reviewChannelId })`: the cap and the case-insensitive name check run
/// under the guild's lock. Fails on a database failure.
pub async fn create_form(app: &App, guild_id: &str, name: &str, description: Option<&str>, review_channel_id: Option<&str>) -> anyhow::Result<Result<ApplicationForm, Refusal>> {
    app.settings().ensure_guild_settings(guild_id).await?;
    let (pool, g, n) = (app.db().clone(), guild_id.to_string(), name.to_string());
    let (d, r) = (description.map(String::from), review_channel_id.map(String::from));
    form_lock(app, &format!("guild:{guild_id}"), move || async move {
        let count: i64 = sqlx::query(sql("countForms")).bind(&g).fetch_one(&pool).await.map_err(db)?.try_get("n").map_err(db)?;
        if count >= limits::APPLICATION_FORMS as i64 {
            return Ok(Err(Refusal::FormLimit));
        }
        if name_clash(&pool, &g, &n, None).await? {
            return Ok(Err(Refusal::NameTaken));
        }
        let insert = InsertBuilder::new(&APPLICATION_FORM_INFO)
            .value("guildId", json!(g))?
            .value("name", json!(n))?
            .value("description", json!(d))?
            .value("reviewChannelId", json!(r))?
            .build()?;
        match insert.fetch_one::<ApplicationForm, _>(&pool).await {
            Ok(form) => Ok(Ok(form)),
            Err(e) => {
                let e = anyhow::Error::new(e);
                if is_unique_violation(&e) { Ok(Err(Refusal::NameTaken)) } else { Err(e) }
            }
        }
    })
    .await
}

/// `updateForm(guildId, formId, data)`: writes columns of one form (guild-scoped; Prisma field names). A rename that
/// collides case-insensitively is refused.
pub async fn update_form(app: &App, guild_id: &str, form_id: &str, data: serde_json::Map<String, Value>) -> anyhow::Result<Result<ApplicationForm, Refusal>> {
    let (pool, g, f) = (app.db().clone(), guild_id.to_string(), form_id.to_string());
    form_lock(app, &format!("form:{form_id}"), move || async move {
        if let Some(name) = data.get("name").and_then(Value::as_str)
            && name_clash(&pool, &g, name, Some(&f)).await?
        {
            return Ok(Err(Refusal::NameTaken));
        }
        if !data.is_empty() {
            let update = UpdateBuilder::new(&APPLICATION_FORM_INFO).set_all(data.iter().map(|(k, v)| (k.as_str(), v.clone())))?.filter("id", json!(f))?.filter("guildId", json!(g))?.without_returning().build()?;
            match update.execute(&pool).await {
                Ok(1) => {}
                Ok(_) => return Ok(Err(Refusal::FormNotFound)),
                Err(e) => {
                    let e = anyhow::Error::new(e);
                    return if is_unique_violation(&e) { Ok(Err(Refusal::NameTaken)) } else { Err(e) };
                }
            }
        }
        let form: Option<ApplicationForm> = sqlx::query_as(sql("formById")).bind(&f).bind(&g).fetch_optional(&pool).await.map_err(db)?;
        Ok(form.ok_or(Refusal::FormNotFound))
    })
    .await
}

/// `deleteForm(guildId, formId)`: deletes a form; its submissions go with it (cascade). The deleted row, or `None`.
pub async fn delete_form(pool: &PgPool, guild_id: &str, form_id: &str) -> anyhow::Result<Option<ApplicationForm>> {
    let form: Option<ApplicationForm> = sqlx::query_as(sql("formById")).bind(form_id).bind(guild_id).fetch_optional(pool).await.map_err(db)?;
    if form.is_none() {
        return Ok(None);
    }
    delete_where(&APPLICATION_FORM_INFO, [("id", json!(form_id)), ("guildId", json!(guild_id))])?.execute(pool).await?;
    Ok(form)
}

async fn set_questions(pool: &PgPool, guild_id: &str, form_id: &str, questions: &[Question]) -> anyhow::Result<()> {
    UpdateBuilder::new(&APPLICATION_FORM_INFO)
        .set("questions", questions_json(questions))?
        .filter("id", json!(form_id))?
        .filter("guildId", json!(guild_id))?
        .without_returning()
        .build()?
        .execute(pool)
        .await?;
    Ok(())
}

/// `addQuestion(guildId, formId, question)`: appends a question (cap and duplicate label checked under the form's
/// lock). The form with its new questions and the count.
pub async fn add_question(app: &App, guild_id: &str, form_id: &str, question: Question) -> anyhow::Result<Result<(ApplicationForm, usize), Refusal>> {
    let (pool, g, f) = (app.db().clone(), guild_id.to_string(), form_id.to_string());
    form_lock(app, &format!("form:{form_id}"), move || async move {
        let form: Option<ApplicationForm> = sqlx::query_as(sql("formById")).bind(&f).bind(&g).fetch_optional(&pool).await.map_err(db)?;
        let Some(mut form) = form else { return Ok(Err(Refusal::FormNotFound)) };
        let mut questions = parse_questions(&form.questions);
        if questions.len() >= limits::APPLICATION_QUESTIONS {
            return Ok(Err(Refusal::QuestionLimit));
        }
        let lower = question.label.to_lowercase();
        if questions.iter().any(|q| q.label.to_lowercase() == lower) {
            return Ok(Err(Refusal::QuestionDup));
        }
        questions.push(question);
        set_questions(&pool, &g, &f, &questions).await?;
        form.questions = questions_json(&questions);
        Ok(Ok((form, questions.len())))
    })
    .await
}

/// `removeQuestion(guildId, formId, number)`: removes question number `number` (1-based). The form and the removed
/// question.
pub async fn remove_question(app: &App, guild_id: &str, form_id: &str, number: Option<i64>) -> anyhow::Result<Result<(ApplicationForm, Question), Refusal>> {
    let (pool, g, f) = (app.db().clone(), guild_id.to_string(), form_id.to_string());
    form_lock(app, &format!("form:{form_id}"), move || async move {
        let form: Option<ApplicationForm> = sqlx::query_as(sql("formById")).bind(&f).bind(&g).fetch_optional(&pool).await.map_err(db)?;
        let Some(mut form) = form else { return Ok(Err(Refusal::FormNotFound)) };
        let mut questions = parse_questions(&form.questions);
        let Some(n) = number.filter(|n| *n >= 1 && (*n as usize) <= questions.len()) else { return Ok(Err(Refusal::QuestionNotFound)) };
        let removed = questions.remove(n as usize - 1);
        set_questions(&pool, &g, &f, &questions).await?;
        form.questions = questions_json(&questions);
        Ok(Ok((form, removed)))
    })
    .await
}

/// Which role list of a form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleList {
    /// `acceptRoleIds`: given on accept.
    Accept,
    /// `requiredRoleIds`: needed to apply.
    Required,
}

impl RoleList {
    pub fn column(self) -> &'static str {
        match self {
            RoleList::Accept => "acceptRoleIds",
            RoleList::Required => "requiredRoleIds",
        }
    }
}

/// `changeRole(guildId, formId, column, roleId, remove)`: adds or removes a role in one of the lists (cap checked
/// under the form's lock). The form and whether anything changed.
pub async fn change_role(app: &App, guild_id: &str, form_id: &str, list: RoleList, role_id: &str, remove: bool) -> anyhow::Result<Result<(ApplicationForm, bool), Refusal>> {
    let (pool, g, f, r) = (app.db().clone(), guild_id.to_string(), form_id.to_string(), role_id.to_string());
    form_lock(app, &format!("form:{form_id}"), move || async move {
        let form: Option<ApplicationForm> = sqlx::query_as(sql("formById")).bind(&f).bind(&g).fetch_optional(&pool).await.map_err(db)?;
        let Some(mut form) = form else { return Ok(Err(Refusal::FormNotFound)) };
        let ids = match list {
            RoleList::Accept => form.accept_role_ids.clone(),
            RoleList::Required => form.required_role_ids.clone(),
        };
        let has = ids.contains(&r);
        if if remove { !has } else { has } {
            return Ok(Ok((form, false)));
        }
        if !remove && ids.len() >= limits::APPLICATION_ROLES {
            return Ok(Err(Refusal::RoleLimit));
        }
        let next: Vec<String> = if remove { ids.into_iter().filter(|id| *id != r).collect() } else { ids.into_iter().chain([r]).collect() };
        UpdateBuilder::new(&APPLICATION_FORM_INFO)
            .set(list.column(), json!(next))?
            .filter("id", json!(f))?
            .filter("guildId", json!(g))?
            .without_returning()
            .build()?
            .execute(&pool)
            .await?;
        match list {
            RoleList::Accept => form.accept_role_ids = next,
            RoleList::Required => form.required_role_ids = next,
        }
        Ok(Ok((form, true)))
    })
    .await
}

// ── The panel ─────────────────────────────────────────────────────────────────

/// The placeholder context of a panel (`{ guild }`).
pub fn panel_ctx(app: &App, guild_id: GuildId) -> RenderContext {
    RenderContext { guild: cached_guild(app, guild_id).as_ref().map(bot_core::adapters::placeholder_guild), ..Default::default() }
}

/// `postPanel({ guild, form, channel })`: posts (or replaces) the Apply panel of a form. The old panel message is
/// deleted best-effort. The new message id, or the reason.
pub async fn post_panel(app: &App, guild_id: GuildId, form: &ApplicationForm, channel_id: ChannelId) -> Result<MessageId, String> {
    let usable = check_channel_usable(app, guild_id, Some(channel_id), &bot_core::interaction_helpers::CHANNEL_POST_NEEDS);
    if !usable.ok {
        return Err(usable.reason.unwrap_or_default());
    }
    let io = io(app);
    let message = match io.send_message(channel_id, build_panel_payload(form, &panel_ctx(app, guild_id))).await {
        Ok(m) => m,
        Err(e) => {
            bot_util::log_warn!(format!("[applications] {guild_id}: could not post the panel: {}", e.message));
            return Err("I could not post in that channel.".into());
        }
    };
    let (old_channel, old_message) = (form.panel_channel_id.clone(), form.panel_message_id.clone());
    let saved = async {
        UpdateBuilder::new(&APPLICATION_FORM_INFO)
            .set("panelChannelId", json!(channel_id.to_string()))?
            .set("panelMessageId", json!(message.to_string()))?
            .filter("id", json!(form.id))?
            .filter("guildId", json!(guild_id.to_string()))?
            .without_returning()
            .build()?
            .execute(app.db())
            .await
    }
    .await;
    if let Err(e) = saved {
        bot_util::log_error!(format!("[applications] {guild_id}: could not save the panel ids"), bot_util::logger::LogArg::error(&e));
        let _ = io.delete_message(channel_id, message).await;
        return Err("The panel could not be saved. Please try again in a moment.".into());
    }
    if let Some(old) = old_message.filter(|m| *m != message.to_string()) {
        delete_message_quietly(app, guild_id, old_channel.as_deref(), &old).await;
    }
    Ok(message)
}

/// `refreshPanel(guild, form)`: re-renders the form's posted panel. Never fails.
pub async fn refresh_panel(app: &App, guild_id: GuildId, form: &ApplicationForm) {
    let (Some(c), Some(m)) = (form.panel_channel_id.as_deref().and_then(parse_id), form.panel_message_id.as_deref().and_then(parse_id)) else { return };
    let Some(channel) = cached_channel(app, guild_id, ChannelId::new(c)) else { return };
    if !bot_core::permissions::is_text_based(&channel) {
        return;
    }
    let p = build_panel_payload(form, &panel_ctx(app, guild_id));
    let edit = Payload { content: None, embeds: p.embeds, components: p.components, files: Vec::new(), ephemeral: false, allowed_mentions: None };
    if let Err(e) = io(app).edit_message(channel.id, MessageId::new(m), edit).await
        && !e.code.is_some_and(|c| GONE_CODES.contains(&c))
    {
        bot_util::log_debug!(format!("[applications] could not refresh a panel: {}", e.message));
    }
}

/// `pendingCount(formId)`: submissions of a form waiting for review (orphaned ones are not counted).
pub async fn pending_count(pool: &PgPool, form_id: &str) -> anyhow::Result<i64> {
    let row = sqlx::query(sql("pendingCount")).bind(form_id).fetch_one(pool).await.map_err(db)?;
    row.try_get("n").map_err(db)
}

/// `deleteMessageQuietly(guild, channelId, messageId)`: a missing channel or message is fine. Never fails.
pub async fn delete_message_quietly(app: &App, guild_id: GuildId, channel_id: Option<&str>, message_id: &str) {
    let (Some(c), Some(m)) = (channel_id.and_then(parse_id), parse_id(message_id)) else { return };
    // `guild.channels.cache.get(id) ?? await guild.channels.fetch(id).catch(() => null)`.
    let Some(channel) = bot_core::interaction_helpers::resolve_channel(app, guild_id, ChannelId::new(c)).await else { return };
    if !bot_core::permissions::is_text_based(&channel) {
        return;
    }
    if let Err(e) = io(app).delete_message(channel.id, MessageId::new(m)).await
        && !e.code.is_some_and(|c| GONE_CODES.contains(&c))
    {
        bot_util::log_debug!(format!("[applications] could not delete message {message_id}: {}", e.message));
    }
}

// ── Applying ──────────────────────────────────────────────────────────────────

/// `startDraft({ form, userId, memberRoleIds, reviewChannelProblem, now })`: finds or creates the member's DRAFT
/// after the eligibility checks, under a per member+form lock so two clicks cannot create two drafts. A PENDING row
/// without a review message is orphaned and does not block.
pub async fn start_draft(
    app: &App,
    form: &ApplicationForm,
    user_id: &str,
    member_role_ids: &[String],
    review_channel_problem: Option<String>,
    now: DateTime<Utc>,
) -> anyhow::Result<Result<ApplicationSubmission, Refusal>> {
    let early = ApplyFacts { form, member_role_ids, review_channel_problem: review_channel_problem.clone(), pending: false, last_denied_at: None, now };
    if let Err(r) = judge_apply(&early) {
        return Ok(Err(r));
    }
    let (a, f, u, roles) = (app.clone(), form.clone(), user_id.to_string(), member_role_ids.to_vec());
    form_lock(app, &format!("draft:{}:{user_id}", form.id), move || async move {
        let pool = a.db();
        let pending = sqlx::query(sql("pendingOfMember")).bind(&f.id).bind(&u).fetch_optional(pool);
        let denied = async {
            if f.cooldown_hours > 0 {
                sqlx::query(sql("lastDenied")).bind(&f.id).bind(&u).fetch_optional(pool).await
            } else {
                Ok(None)
            }
        };
        let draft = sqlx::query_as::<_, ApplicationSubmission>(sql("draftOfMember")).bind(&f.id).bind(&u).fetch_optional(pool);
        let (pending, denied, draft) = tokio::join!(pending, denied, draft);
        let (pending, denied, draft) = (pending.map_err(db)?, denied.map_err(db)?, draft.map_err(db)?);
        let last_denied_at = denied.and_then(|r| r.try_get::<chrono::NaiveDateTime, _>("reviewedAt").ok()).map(|n| n.and_utc());
        let facts = ApplyFacts { form: &f, member_role_ids: &roles, review_channel_problem, pending: pending.is_some(), last_denied_at, now };
        if let Err(r) = judge_apply(&facts) {
            return Ok(Err(r));
        }
        if let Some(d) = draft {
            return Ok(Ok(d));
        }
        a.settings().ensure_guild_settings(&f.guild_id).await?;
        let insert = InsertBuilder::new(&APPLICATION_SUBMISSION_INFO)
            .value("guildId", json!(f.guild_id))?
            .value("formId", json!(f.id))?
            .value("userId", json!(u))?
            .build()?;
        Ok(Ok(insert.fetch_one::<ApplicationSubmission, _>(pool).await?))
    })
    .await
}

/// `reviewChannelIssue(guild, form)`: why the bot cannot post in the form's review channel (`None` when it can, or
/// when none is set: that is NOT_SET_UP).
pub fn review_channel_issue(app: &App, guild_id: GuildId, form: &ApplicationForm) -> Option<String> {
    let id = form.review_channel_id.as_deref().filter(|c| !c.is_empty())?;
    let Some(channel) = parse_id(id).and_then(|c| cached_channel(app, guild_id, ChannelId::new(c))) else {
        return Some("The review channel was deleted.".into());
    };
    let usable = check_channel_usable(app, guild_id, Some(channel.id), &bot_core::interaction_helpers::CHANNEL_POST_NEEDS);
    if usable.ok { None } else { usable.reason }
}

fn no_mentions_reply(embed: CreateEmbed) -> Payload {
    Payload::embed(embed).ephemeral(true).no_mentions()
}

/// `reply(interaction, embed)`: an ephemeral answer; never fails.
async fn reply<C: Interactive>(ctx: &C, embed: CreateEmbed) {
    if let Err(e) = ctx.responder().reply(no_mentions_reply(embed).to_reply()).await
        && !is_interaction_gone(&e)
    {
        bot_util::log_warn!(format!("[applications] reply failed: {e}"));
    }
}

/// `answer(interaction, embed, components)`: answers a deferred interaction, whatever it was deferred as.
async fn answer<C: Interactive>(ctx: &C, embed: CreateEmbed, components: Vec<CreateActionRow>) {
    let p = Payload::embed(embed).components(components).no_mentions();
    if let Err(e) = ctx.responder().edit_reply(p.to_edit()).await
        && !is_interaction_gone(&e)
    {
        bot_util::log_warn!(format!("[applications] could not answer: {e}"));
    }
}

async fn follow_up<C: Interactive>(ctx: &C, embed: CreateEmbed) {
    if let Err(e) = ctx.responder().follow_up(no_mentions_reply(embed).to_follow_up()).await
        && !is_interaction_gone(&e)
    {
        bot_util::log_warn!(format!("[applications] follow-up failed: {e}"));
    }
}

fn in_cached_guild<C: Interactive>(ctx: &C) -> Option<GuildId> {
    let g = ctx.guild_id()?;
    (ctx.member().is_some() && cached_guild(ctx.app(), g).is_some()).then_some(g)
}

/// `beginApplication(interaction, formRef)`: every check, then page 1 of the form as a modal (the first and only
/// response). Used by `/application apply` (a name or id) and by the panel's Apply button (an id).
pub async fn begin_application<C: Interactive>(ctx: &C, form_ref: &str) -> bot_core::BotResult {
    let Some(guild_id) = in_cached_guild(ctx) else {
        reply(ctx, refusal_embed(&Refusal::ServerOnly)).await;
        return Ok(());
    };
    let budget = ack_budget_ms(ctx.interaction_id());
    if budget == 0 {
        reply(ctx, refusal_embed(&Refusal::Slow)).await;
        return Ok(());
    }
    let app = ctx.app().clone();
    let user_id = ctx.user().id.to_string();
    // `[...member.roles.cache.keys()]`: @everyone (the guild id) included, roles the guild no longer has dropped.
    let roles: Vec<String> = match (ctx.member(), cached_guild(ctx.app(), guild_id)) {
        (Some(m), Some(g)) => bot_core::interaction_helpers::member_role_ids(&g, m).iter().map(|r| r.to_string()).collect(),
        _ => Vec::new(),
    };
    let reference = form_ref.to_string();
    let work = async move {
        let g = guild_id.to_string();
        let settings = match app.settings().peek_settings(&g) {
            Some(s) => Some(s),
            None => app.settings().get_settings_safe(&g).await,
        };
        let Some(settings) = settings else { return Ok::<_, anyhow::Error>(Err(Refusal::Slow)) };
        if !bot_core::settings::is_module_enabled(Some(&settings), Some(ModuleKey::Applications)) {
            return Ok(Err(Refusal::ModuleOff));
        }
        let Some(form) = find_form(app.db(), &g, &reference).await? else { return Ok(Err(Refusal::FormNotFound)) };
        let problem = review_channel_issue(&app, guild_id, &form);
        let started = start_draft(&app, &form, &user_id, &roles, problem, app.clock().now()).await?;
        Ok(started.map(|s| (form, s)))
    };
    let outcome = match with_timeout(work, budget, "application start").await {
        Ok(o) => o,
        Err(bot_util::async_util::WithTimeoutError::Timeout(_)) => Err(Refusal::Slow),
        Err(bot_util::async_util::WithTimeoutError::Inner(e)) => {
            bot_util::log_error!(format!("[applications] {guild_id}: could not start an application"), bot_core::error::log_arg(&e));
            Err(Refusal::Error)
        }
    };
    let (form, submission) = match outcome {
        Ok(v) => v,
        Err(r) => {
            reply(ctx, refusal_embed(&r)).await;
            return Ok(());
        }
    };
    let questions = parse_questions(&form.questions);
    let modal = build_apply_modal(&form.name, &questions, &submission.id, &submission.answers, 0);
    if let Err(e) = ctx.responder().show_modal(modal).await
        && !is_interaction_gone(&e)
    {
        bot_util::log_warn!(format!("[applications] {guild_id}: showModal failed: {e}"));
    }
    Ok(())
}

async fn own_draft(pool: &PgPool, id: &str, guild_id: &str, user_id: &str) -> anyhow::Result<Option<(ApplicationSubmission, ApplicationForm)>> {
    let sub: Option<ApplicationSubmission> = sqlx::query_as(sql("ownDraft")).bind(id).bind(guild_id).bind(user_id).fetch_optional(pool).await.map_err(db)?;
    let Some(sub) = sub else { return Ok(None) };
    let form: Option<ApplicationForm> = sqlx::query_as(sql("formOf")).bind(&sub.form_id).fetch_optional(pool).await.map_err(db)?;
    Ok(form.map(|f| (sub, f)))
}

fn page_of(raw: Option<&str>) -> Option<usize> {
    raw.and_then(|p| p.parse::<usize>().ok())
}

/// The Continue button: opens page N of the member's own draft.
async fn handle_next(ctx: &ComponentCtx, guild_id: GuildId, submission_id: &str, page_raw: Option<&str>) -> bot_core::BotResult {
    let budget = ack_budget_ms(ctx.interaction.id);
    if budget == 0 {
        reply(ctx, refusal_embed(&Refusal::Slow)).await;
        return Ok(());
    }
    let (pool, id, g, u) = (ctx.app.db().clone(), submission_id.to_string(), guild_id.to_string(), ctx.user().id.to_string());
    let lookup = async move { own_draft(&pool, &id, &g, &u).await };
    let found = match with_timeout(lookup, budget, "application draft").await {
        Ok(f) => f,
        Err(bot_util::async_util::WithTimeoutError::Timeout(_)) => {
            reply(ctx, refusal_embed(&Refusal::Slow)).await;
            return Ok(());
        }
        Err(bot_util::async_util::WithTimeoutError::Inner(e)) => {
            bot_util::log_warn!(format!("[applications] draft lookup failed: {e}"));
            reply(ctx, refusal_embed(&Refusal::Error)).await;
            return Ok(());
        }
    };
    let questions = found.as_ref().map(|(_, f)| parse_questions(&f.questions)).unwrap_or_default();
    let page = page_of(page_raw).filter(|p| *p < page_count(questions.len()));
    let (Some((sub, form)), Some(page)) = (found, page) else {
        reply(ctx, error_embed("Application Closed", Some("This application is no longer open. Start again with the Apply button or `/application apply`."), None)).await;
        return Ok(());
    };
    if !form.enabled {
        reply(ctx, refusal_embed(&Refusal::FormDisabled)).await;
        return Ok(());
    }
    if let Err(e) = ctx.show_modal(build_apply_modal(&form.name, &questions, &sub.id, &sub.answers, page)).await
        && !is_interaction_gone(&e)
    {
        bot_util::log_warn!(format!("[applications] showModal failed: {e}"));
    }
    Ok(())
}

const CLOSED_OR_SUBMITTED: &str = "This application is no longer open or was already submitted.";

/// One submitted page: saves the answers, then asks for the next page or finishes the application. Page 1 is
/// acknowledged with a new ephemeral reply (it may come from the public panel button); later pages come from the
/// ephemeral Continue message and replace it.
async fn handle_page_submit(ctx: &ModalCtx, guild_id: GuildId, submission_id: &str, page_raw: Option<&str>) -> bot_core::BotResult {
    let page = page_of(page_raw);
    let from_message = ctx.interaction.message.is_some();
    let ack = if page.is_some_and(|p| p > 0) && from_message { ctx.defer_update().await } else { ctx.defer_reply(true).await };
    if let Err(e) = ack {
        if !is_interaction_gone(&e) {
            bot_util::log_warn!(format!("[applications] acknowledging a page failed: {e}"));
        }
        return Ok(());
    }
    let user = ctx.user().id.to_string();
    let found = match own_draft(ctx.app.db(), submission_id, &guild_id.to_string(), &user).await {
        Ok(f) => f,
        Err(e) => {
            bot_util::log_warn!(format!("[applications] draft lookup failed: {e}"));
            answer(ctx, refusal_embed(&Refusal::Error), Vec::new()).await;
            return Ok(());
        }
    };
    let questions = found.as_ref().map(|(_, f)| parse_questions(&f.questions)).unwrap_or_default();
    let pages = page_count(questions.len());
    let (Some((sub, form)), Some(page)) = (found, page.filter(|p| *p < pages)) else {
        answer(ctx, error_embed("Application Closed", Some(CLOSED_OR_SUBMITTED), None), Vec::new()).await;
        return Ok(());
    };
    if !form.enabled {
        answer(ctx, refusal_embed(&Refusal::FormDisabled), Vec::new()).await;
        return Ok(());
    }
    // Every answer is saved, even when a required one is empty, so a retry is prefilled.
    let values: Vec<String> =
        page_slice(&questions, page).iter().enumerate().map(|(j, q)| clean_answer(ctx.value(QUESTION_FIELDS[j]).as_deref(), answer_limit(q.style, questions.len()))).collect();
    let merged = merge_answers(&questions, &sub.answers, page, &values);
    let saved = sqlx::query(sql("saveAnswers")).bind(&merged).bind(&sub.id).bind(&user).execute(ctx.app.db()).await;
    match saved {
        Ok(r) if r.rows_affected() == 1 => {}
        Ok(_) => {
            answer(ctx, error_embed("Application Closed", Some(CLOSED_OR_SUBMITTED), None), Vec::new()).await;
            return Ok(());
        }
        Err(e) => {
            bot_util::log_error!(format!("[applications] {guild_id}: could not save answers"), bot_util::logger::LogArg::error(&e));
            let e = error_embed("Could Not Save", Some("Your answers could not be saved. Please press the button and try again."), None);
            answer(ctx, e, vec![continue_row(&sub.id, page, pages, Some("Try again"))]).await;
            return Ok(());
        }
    }
    let answers: Vec<String> = merged.as_array().into_iter().flatten().map(|a| a["answer"].as_str().unwrap_or("").to_string()).collect();
    let missing = missing_required(&questions, &answers);
    let here: Vec<usize> = missing.iter().copied().filter(|i| i / A::PAGE_SIZE == page).collect();
    let names = |list: &[usize]| list.iter().map(|i| format!("• {}", questions[*i].label)).collect::<Vec<_>>().join("\n");
    if !here.is_empty() {
        let e = error_embed("Answer Required", Some(&format!("These questions need an answer:\n{}", names(&here))), None);
        answer(ctx, e, vec![continue_row(&sub.id, page, pages, Some("Fix answers"))]).await;
        return Ok(());
    }
    if page < pages - 1 {
        let e = success_embed(&format!("Page {} of {pages} saved", page + 1), Some("Press the button for the next questions."), None);
        answer(ctx, e, vec![continue_row(&sub.id, page + 1, pages, None)]).await;
        return Ok(());
    }
    if let Some(first) = missing.first() {
        // Questions were added after earlier pages were answered: send the member to the first page with a gap.
        let e = error_embed("Answer Required", Some(&format!("This form changed. These questions still need an answer:\n{}", names(&missing))), None);
        answer(ctx, e, vec![continue_row(&sub.id, first / A::PAGE_SIZE, pages, Some("Fix answers"))]).await;
        return Ok(());
    }
    finish_application(ctx, guild_id, &sub, &form, &merged).await
}

/// Moves a complete draft to PENDING and posts it for review; reverts to DRAFT when it cannot be posted.
async fn finish_application(ctx: &ModalCtx, guild_id: GuildId, sub: &ApplicationSubmission, form: &ApplicationForm, answers: &Value) -> bot_core::BotResult {
    let app = &ctx.app;
    let channel = form.review_channel_id.as_deref().and_then(parse_id).map(ChannelId::new);
    let Some(channel) = channel else {
        answer(ctx, refusal_embed(&Refusal::NotSetUp), Vec::new()).await;
        return Ok(());
    };
    if let Some(reason) = review_channel_issue(app, guild_id, form) {
        answer(ctx, refusal_embed(&Refusal::ReviewChannel { reason }), Vec::new()).await;
        return Ok(());
    }
    let submitted_at = app.clock().now();
    let user = ctx.user().id.to_string();
    // The claim: only one concurrent submit can move this draft to PENDING, so a double click posts once.
    let claim = sqlx::query(sql("claimSubmit")).bind(submitted_at.naive_utc()).bind(&sub.id).bind(&user).execute(app.db()).await;
    match claim {
        Ok(r) if r.rows_affected() == 1 => {}
        Ok(_) => {
            answer(ctx, error_embed("Already Submitted", Some("This application was already submitted."), None), Vec::new()).await;
            return Ok(());
        }
        Err(e) => {
            bot_util::log_error!(format!("[applications] {guild_id}: could not submit"), bot_util::logger::LogArg::error(&e));
            answer(ctx, refusal_embed(&Refusal::Error), Vec::new()).await;
            return Ok(());
        }
    }
    let who = Applicant::of(ctx.user(), ctx.member());
    let post = Payload::embed(build_review_embed(&form.name, answers, &who, &sub.id, submitted_at)).components(vec![review_row(&sub.id)]).no_mentions();
    let message = match io(app).send_message(channel, post).await {
        Ok(m) => m,
        Err(e) => {
            bot_util::log_warn!(format!("[applications] {guild_id}: could not post the review message: {}", e.message));
            if let Err(e) = sqlx::query(sql("revertSubmit")).bind(&sub.id).execute(app.db()).await {
                bot_util::log_error!(format!("[applications] {guild_id}: could not revert a submission"), bot_util::logger::LogArg::error(&e));
            }
            answer(ctx, refusal_embed(&Refusal::ReviewChannel { reason: "I could not post in the review channel.".into() }), Vec::new()).await;
            return Ok(());
        }
    };
    if let Err(e) = sqlx::query(sql("setReviewMessage")).bind(message.to_string()).bind(&sub.id).execute(app.db()).await {
        bot_util::log_error!(format!("[applications] {guild_id}: could not save the review message id"), bot_util::logger::LogArg::error(&e));
    }
    bot_util::log_info!(format!("[applications] {guild_id}: {user} submitted \"{}\" ({})", form.name, sub.id));
    let text = format!("Your application for **{}** was sent for review. You will get a message when it has been decided.", form.name);
    answer(ctx, success_embed("Application Submitted", Some(&text), None), Vec::new()).await;
    Ok(())
}

// ── Reviewing ─────────────────────────────────────────────────────────────────

/// What [`give_accept_roles`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcceptRoles {
    pub given: Vec<String>,
    pub already: Vec<String>,
    /// `(roleId, reason)`.
    pub failed: Vec<(String, String)>,
}

/// `giveAcceptRoles({ guild, member, roleIds, reason })`: reports what worked and what did not (deleted, above the
/// bot, managed, dangerous...).
pub async fn give_accept_roles(app: &App, guild_id: GuildId, member: &Member, role_ids: &[String], reason: &str) -> AcceptRoles {
    let mut out = AcceptRoles::default();
    let guild = cached_guild(app, guild_id);
    let io = io(app);
    for id in role_ids {
        let role = guild.as_ref().and_then(|g| parse_id(id).and_then(|r| g.roles.get(&RoleId::new(r))));
        let Some(role) = role else {
            out.failed.push((id.clone(), "the role no longer exists".into()));
            continue;
        };
        if member.roles.contains(&role.id) {
            out.already.push(id.clone());
            continue;
        }
        let check = check_assignable_role_cached(app, guild_id, role.id, None, false);
        if !check.ok {
            out.failed.push((id.clone(), check.reason.unwrap_or_default()));
            continue;
        }
        match io.add_role(guild_id, member.user.id, role.id, Some(clamp_text(reason, 512))).await {
            Ok(()) => out.given.push(id.clone()),
            Err(e) => out.failed.push((id.clone(), if e.code == Some(50013) { "I am missing permissions".into() } else { e.message })),
        }
    }
    out
}

/// `sendDecisionDm(...)`: DMs the applicant; never fails. `Err` carries a short reason for the reviewer.
async fn send_decision_dm(app: &App, guild_id: GuildId, user_id: &str, member: Option<&Member>, form: &ApplicationForm, decision: Decision, note: Option<&str>) -> Result<(), String> {
    let accepted = decision == Decision::Accepted;
    // The user first, so {user...} tokens resolve even when the applicant has left the server.
    let target = match member {
        Some(m) => Some(m.user.clone()),
        None => match parse_id(user_id) {
            Some(u) => io(app).fetch_user(UserId::new(u)).await.ok(),
            None => None,
        },
    };
    let Some(target) = target else { return Err("the user could not be found".into()) };
    let custom = if accepted { form.accept_message.as_deref() } else { form.deny_message.as_deref() };
    let template = custom.filter(|t| !t.is_empty()).unwrap_or(if accepted { DEFAULT_ACCEPT_MESSAGE } else { DEFAULT_DENY_MESSAGE });
    let guild = cached_guild(app, guild_id);
    let mut ctx = crate::welcome_service::render_ctx(guild.as_ref(), member, Some(&target), None);
    ctx.vars = Some(vec![("form".into(), VarValue::from(form.name.as_str())), ("reason".into(), VarValue::from(note.unwrap_or("")))]);
    let mut text = render_placeholders(template, &ctx, Some(3500));
    // A deny message that places {reason} itself already shows it.
    if !accepted
        && let Some(n) = note.filter(|n| !n.is_empty())
        && !template.to_lowercase().contains("{reason}")
    {
        text.push_str(&format!("\n\n**Reason:** {n}"));
    }
    let guild_name = guild.as_ref().map(|g| g.name.clone()).unwrap_or_default();
    let base = if accepted { success_embed("Application accepted", None, None) } else { error_embed("Application denied", None, None) };
    let embed = base.description(clamp_text(&text, 4000)).footer(CreateEmbedFooter::new(clamp_text(&guild_name, 100)));
    let send = io(app).send_dm(target.id, Payload::embed(embed).no_mentions());
    match with_timeout(send, A::DM_TIMEOUT_MS, "application DM").await {
        Ok(()) => Ok(()),
        Err(bot_util::async_util::WithTimeoutError::Inner(e)) if e.code == Some(50007) => Err("their DMs are closed".into()),
        Err(_) => Err("the message could not be sent".into()),
    }
}

/// `reviewerAllowed(interaction)`: MODERATOR or above (the router does not check buttons and modals). Settings come
/// from the memo or a bounded load; unavailable settings can never wrongly grant a moderator role.
async fn reviewer_allowed<C: Interactive>(ctx: &C) -> bool {
    let app = ctx.app();
    let g = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
    let mut settings = app.settings().peek_settings(&g);
    if settings.is_none() {
        let budget = ack_budget_ms(ctx.interaction_id());
        if budget > 0 {
            let (a, gid) = (app.clone(), g.clone());
            let load = async move { Ok::<_, std::convert::Infallible>(a.settings().get_settings_safe(&gid).await) };
            settings = with_timeout(load, budget, "application settings").await.ok().flatten();
        }
    }
    let access = bot_core::interaction::member_access(app, ctx.guild_id(), ctx.member());
    let level = bot_core::permission_checker::resolve_permission_level(access.as_ref(), settings.as_deref(), app.config().bot_owner_id.as_deref());
    level >= PermissionLevel::Moderator
}

fn not_reviewer() -> CreateEmbed {
    error_embed("Insufficient Permissions", Some("Only moderators can review applications."), None)
}

/// The interaction a decision is recorded from (the Accept button or the deny form).
enum ReviewCtx<'a> {
    Button(&'a ComponentCtx),
    Modal(&'a ModalCtx),
}

impl ReviewCtx<'_> {
    fn app(&self) -> &App {
        match self {
            ReviewCtx::Button(c) => &c.app,
            ReviewCtx::Modal(c) => &c.app,
        }
    }

    fn user(&self) -> &User {
        match self {
            ReviewCtx::Button(c) => c.user(),
            ReviewCtx::Modal(c) => c.user(),
        }
    }

    fn source_embed(&self) -> Option<Embed> {
        match self {
            ReviewCtx::Button(c) => c.interaction.message.embeds.first().cloned(),
            ReviewCtx::Modal(c) => c.interaction.message.as_ref().and_then(|m| m.embeds.first().cloned()),
        }
    }

    async fn follow_up(&self, embed: CreateEmbed) {
        match self {
            ReviewCtx::Button(c) => follow_up(*c, embed).await,
            ReviewCtx::Modal(c) => follow_up(*c, embed).await,
        }
    }

    /// `editReviewMessage(interaction, payload)`: edits the review post after a deferUpdate. Never fails.
    async fn edit_review_message(&self, payload: Payload) {
        let from_message = match self {
            ReviewCtx::Button(_) => true,
            ReviewCtx::Modal(c) => c.interaction.message.is_some(),
        };
        let result = match self {
            ReviewCtx::Button(c) => c.edit_reply(payload.to_edit()).await.map(|_| ()).map_err(|e| (e.code(), e.to_string())),
            ReviewCtx::Modal(c) if from_message => c.edit_reply(payload.to_edit()).await.map(|_| ()).map_err(|e| (e.code(), e.to_string())),
            ReviewCtx::Modal(c) => match c.interaction.message.as_ref() {
                Some(m) => io(&c.app).edit_message(m.channel_id, m.id, payload).await.map_err(|e| (e.code, e.message)),
                None => Ok(()),
            },
        };
        if let Err((code, message)) = result {
            let gone = code.is_some_and(|c| GONE_CODES.contains(&c) || bot_core::interaction_helpers::INTERACTION_GONE_CODES.contains(&c));
            if !gone {
                bot_util::log_warn!(format!("[applications] could not edit the review post: {message}"));
            }
        }
    }
}

fn empty_components() -> Payload {
    Payload { components: Some(Vec::new()), ..Default::default() }
}

/// `decide(...)`: records a decision on a PENDING submission. Claims it (two reviewers cannot both decide), gives
/// roles or sends the DM, and edits the review post. Runs after the interaction was acknowledged.
async fn decide(ctx: ReviewCtx<'_>, guild_id: GuildId, submission_id: &str, decision: Decision, note: Option<String>) -> bot_core::BotResult {
    let app = ctx.app().clone();
    let reviewer = ctx.user().id.to_string();
    let g = guild_id.to_string();
    let sub: Result<Option<ApplicationSubmission>, _> = sqlx::query_as(sql("submissionById")).bind(submission_id).bind(&g).fetch_optional(app.db()).await;
    let sub = match sub {
        Ok(s) => s,
        Err(e) => {
            bot_util::log_warn!(format!("[applications] {g}: review lookup failed: {e}"));
            ctx.follow_up(refusal_embed(&Refusal::Error)).await;
            return Ok(());
        }
    };
    let Some(sub) = sub else {
        ctx.edit_review_message(empty_components()).await;
        ctx.follow_up(error_embed("Application Gone", Some("This application no longer exists (its form was deleted)."), None)).await;
        return Ok(());
    };
    if sub.user_id == reviewer {
        ctx.follow_up(error_embed("Not Allowed", Some("You cannot review your own application."), None)).await;
        return Ok(());
    }
    let status = sub.status.as_str();
    if status != "PENDING" {
        ctx.edit_review_message(empty_components()).await;
        let by = sub.reviewer_id.as_deref().map(|r| format!(" by <@{r}>")).unwrap_or_default();
        ctx.follow_up(error_embed("Already Reviewed", Some(&format!("This application was already {}{by}.", status.to_lowercase())), None)).await;
        return Ok(());
    }
    // The JS loads the form with the submission (`include: { form: true }`): a failure is answered, never thrown.
    let form: Option<ApplicationForm> = match sqlx::query_as(sql("formOf")).bind(&sub.form_id).fetch_optional(app.db()).await {
        Ok(f) => f,
        Err(e) => {
            bot_util::log_warn!(format!("[applications] {g}: review lookup failed: {e}"));
            ctx.follow_up(refusal_embed(&Refusal::Error)).await;
            return Ok(());
        }
    };
    let Some(form) = form else {
        ctx.edit_review_message(empty_components()).await;
        ctx.follow_up(error_embed("Application Gone", Some("This application no longer exists (its form was deleted)."), None)).await;
        return Ok(());
    };
    let now = app.clock().now();
    let claim = sqlx::query(sql("claimDecision"))
        .bind(decision.status())
        .bind(&reviewer)
        .bind(now.naive_utc())
        .bind(note.as_deref())
        .bind(&sub.id)
        .execute(app.db())
        .await;
    match claim {
        Ok(r) if r.rows_affected() == 1 => {}
        Ok(_) => {
            ctx.follow_up(error_embed("Already Reviewed", Some("Someone else just reviewed this application."), None)).await;
            return Ok(());
        }
        Err(e) => {
            bot_util::log_error!(format!("[applications] {g}: could not record a review"), bot_util::logger::LogArg::error(&e));
            ctx.follow_up(refusal_embed(&Refusal::Error)).await;
            return Ok(());
        }
    }
    let accepted = decision == Decision::Accepted;
    let member = match parse_id(&sub.user_id) {
        Some(u) => io(&app).fetch_member(guild_id, UserId::new(u)).await.ok(),
        None => None,
    };
    let mut problems: Vec<String> = Vec::new();
    let mut extra: Option<String> = None;
    if accepted && !form.accept_role_ids.is_empty() {
        match member.as_ref() {
            None => problems.push("The applicant is no longer in the server, so no roles were given.".into()),
            Some(m) => {
                let roles = give_accept_roles(&app, guild_id, m, &form.accept_role_ids, &format!("Application accepted: {}", form.name)).await;
                let mut parts = Vec::new();
                let mention = |ids: &[String]| ids.iter().map(|id| format!("<@&{id}>")).collect::<Vec<_>>().join(" ");
                if !roles.given.is_empty() {
                    parts.push(format!("Roles given: {}", mention(&roles.given)));
                }
                if !roles.already.is_empty() {
                    parts.push(format!("Already had: {}", mention(&roles.already)));
                }
                for (id, reason) in &roles.failed {
                    let line = format!("<@&{id}>: {reason}");
                    problems.push(format!("Could not give {line}"));
                    parts.push(format!("⚠️ Could not give {line}"));
                }
                extra = (!parts.is_empty()).then(|| parts.join("\n"));
            }
        }
    }
    if let Err(reason) = send_decision_dm(&app, guild_id, &sub.user_id, member.as_ref(), &form, decision, note.as_deref()).await {
        problems.push(format!("The applicant was not messaged: {reason}."));
    }
    if member.is_none() && !accepted {
        problems.push("The applicant is no longer in the server.".into());
    }
    let source = ctx.source_embed();
    let embed = build_outcome_embed(
        source.as_ref(),
        &Outcome { form_name: Some(&form.name), decision, reviewer_id: &reviewer, note: note.as_deref(), extra: extra.as_deref(), submission_id: &sub.id, at: now },
    );
    ctx.edit_review_message(Payload::embed(embed).components(Vec::new()).no_mentions()).await;
    bot_util::log_info!(format!("[applications] {g}: {reviewer} {} {}", if accepted { "accepted" } else { "denied" }, sub.id));
    if !problems.is_empty() {
        ctx.follow_up(warn_embed("Decision Recorded", Some(&problems.join("\n")), None)).await;
    }
    Ok(())
}

/// The Accept / Deny buttons under a review post.
async fn handle_review_button(ctx: &ComponentCtx, guild_id: GuildId, action: &str, submission_id: &str) -> bot_core::BotResult {
    if !reviewer_allowed(ctx).await {
        reply(ctx, not_reviewer()).await;
        return Ok(());
    }
    if action == "deny" {
        if let Err(e) = ctx.show_modal(build_deny_modal(submission_id)).await
            && !is_interaction_gone(&e)
        {
            bot_util::log_warn!(format!("[applications] showModal failed: {e}"));
        }
        return Ok(());
    }
    if let Err(e) = ctx.defer_update().await {
        if !is_interaction_gone(&e) {
            bot_util::log_warn!(format!("[applications] acknowledging a review failed: {e}"));
        }
        return Ok(());
    }
    decide(ReviewCtx::Button(ctx), guild_id, submission_id, Decision::Accepted, None).await
}

/// The deny reason form.
async fn handle_deny_submit(ctx: &ModalCtx, guild_id: GuildId, submission_id: &str) -> bot_core::BotResult {
    if !reviewer_allowed(ctx).await {
        reply(ctx, not_reviewer()).await;
        return Ok(());
    }
    let note = ctx.value(NOTE_FIELD);
    let ack = if ctx.interaction.message.is_some() { ctx.defer_update().await } else { ctx.defer_reply(true).await };
    if let Err(e) = ack {
        if !is_interaction_gone(&e) {
            bot_util::log_warn!(format!("[applications] acknowledging a denial failed: {e}"));
        }
        return Ok(());
    }
    let note = clean_answer(note.as_deref(), A::NOTE_MAX);
    decide(ReviewCtx::Modal(ctx), guild_id, submission_id, Decision::Denied, (!note.is_empty()).then_some(note)).await
}

// ── Component handlers (registered by the plugin) ─────────────────────────────

/// `handleButton(interaction)`: `application:{apply|next|accept|deny}:...`.
pub async fn handle_button(ctx: ComponentCtx) -> bot_core::BotResult {
    let (_, params) = ctx.parsed();
    let action = params.first().map(String::as_str).unwrap_or("");
    let id = params.get(1).map(String::as_str).unwrap_or("");
    if action == "apply" {
        return begin_application(&ctx, id).await;
    }
    let Some(guild_id) = in_cached_guild(&ctx) else {
        reply(&ctx, refusal_embed(&Refusal::ServerOnly)).await;
        return Ok(());
    };
    match action {
        "next" => handle_next(&ctx, guild_id, id, params.get(2).map(String::as_str)).await,
        "accept" | "deny" => handle_review_button(&ctx, guild_id, action, id).await,
        _ => {
            reply(&ctx, error_embed("Button Expired", Some("This button is no longer active."), None)).await;
            Ok(())
        }
    }
}

/// `handleModal(interaction)`: `application-modal:page:{submissionId}:{page}` and `application-modal:deny:{id}`.
pub async fn handle_modal(ctx: ModalCtx) -> bot_core::BotResult {
    let (_, params) = ctx.parsed();
    let kind = params.first().map(String::as_str).unwrap_or("");
    let id = params.get(1).map(String::as_str).unwrap_or("");
    let Some(guild_id) = in_cached_guild(&ctx) else {
        reply(&ctx, refusal_embed(&Refusal::ServerOnly)).await;
        return Ok(());
    };
    match kind {
        "page" => handle_page_submit(&ctx, guild_id, id, params.get(2).map(String::as_str)).await,
        "deny" => handle_deny_submit(&ctx, guild_id, id).await,
        _ => {
            reply(&ctx, error_embed("Form Expired", Some("This form has expired. Please start again."), None)).await;
            Ok(())
        }
    }
}

// ── Cleanup hooks (registered `always`: dead references must go while the module is off too) ──

/// `onMessageDelete(ctx)`'s body: a bot message was deleted, forget it as a panel or as a review post.
pub async fn forget_message(pool: &PgPool, guild_id: &str, message_id: &str) {
    let run = async {
        sqlx::query(sql("forgetPanelMessage")).bind(guild_id).bind(message_id).execute(pool).await?;
        sqlx::query(sql("forgetReviewMessage")).bind(guild_id).bind(message_id).execute(pool).await?;
        Ok::<_, sqlx::Error>(())
    };
    if let Err(e) = run.await {
        bot_util::log_warn!(format!("[applications] {guild_id}: message cleanup failed: {e}"));
    }
}

/// `onMessageDelete(ctx)`.
pub async fn on_message_delete(ctx: Arc<HookCtx<MessageDelete>>) -> bot_core::BotResult {
    forget_message(ctx.app.db(), &ctx.data.guild_id.to_string(), &ctx.data.message_id.to_string()).await;
    Ok(())
}

/// `onChannelDelete(ctx)`'s body: the forms lose the review channel (they cannot be opened until an admin sets
/// another), their review posts are gone with it, and panels in it are forgotten.
pub async fn forget_channel(pool: &PgPool, guild_id: &str, channel_id: &str) {
    let run = async {
        let rows = sqlx::query(sql("formsInChannel")).bind(guild_id).bind(channel_id).fetch_all(pool).await?;
        if rows.is_empty() {
            return Ok::<_, sqlx::Error>(());
        }
        let mut review_ids = Vec::new();
        let mut panel_ids = Vec::new();
        for r in &rows {
            let id: String = r.try_get("id")?;
            if r.try_get::<Option<String>, _>("reviewChannelId")?.as_deref() == Some(channel_id) {
                review_ids.push(id.clone());
            }
            if r.try_get::<Option<String>, _>("panelChannelId")?.as_deref() == Some(channel_id) {
                panel_ids.push(id);
            }
        }
        if !review_ids.is_empty() {
            sqlx::query(sql("orphanReviews")).bind(&review_ids).execute(pool).await?;
            sqlx::query(sql("clearReviewChannel")).bind(&review_ids).execute(pool).await?;
        }
        if !panel_ids.is_empty() {
            sqlx::query(sql("clearPanel")).bind(&panel_ids).execute(pool).await?;
        }
        Ok(())
    };
    if let Err(e) = run.await {
        bot_util::log_warn!(format!("[applications] {guild_id}: channel cleanup failed: {e}"));
    }
}

/// `onChannelDelete(ctx)`.
pub async fn on_channel_delete(ctx: Arc<HookCtx<ChannelDelete>>) -> bot_core::BotResult {
    forget_channel(ctx.app.db(), &ctx.data.guild_id.to_string(), &ctx.data.channel.id.to_string()).await;
    Ok(())
}

/// `onRoleDelete(ctx)`'s body: the role leaves the forms' accept and required lists. A form that required that role
/// and has no other required role left is switched off instead of silently opening to everyone.
pub async fn forget_role(pool: &PgPool, guild_id: &str, role_id: &str) {
    let run = async {
        let rows = sqlx::query(sql("formsWithRole")).bind(guild_id).bind(role_id).fetch_all(pool).await?;
        for r in &rows {
            let id: String = r.try_get("id")?;
            let accept: Vec<String> = r.try_get::<Option<Vec<String>>, _>("acceptRoleIds")?.unwrap_or_default();
            let required_before: Vec<String> = r.try_get::<Option<Vec<String>>, _>("requiredRoleIds")?.unwrap_or_default();
            let required: Vec<String> = required_before.iter().filter(|x| *x != role_id).cloned().collect();
            let accept: Vec<String> = accept.into_iter().filter(|x| x != role_id).collect();
            let disable = (!required_before.is_empty() && required.is_empty()).then_some(false);
            sqlx::query(sql("setRoleLists")).bind(&accept).bind(&required).bind(disable).bind(&id).execute(pool).await?;
        }
        Ok::<_, sqlx::Error>(())
    };
    if let Err(e) = run.await {
        bot_util::log_warn!(format!("[applications] {guild_id}: role cleanup failed: {e}"));
    }
}

/// `onRoleDelete(ctx)`.
pub async fn on_role_delete(ctx: Arc<HookCtx<RoleDelete>>) -> bot_core::BotResult {
    forget_role(ctx.app.db(), &ctx.data.guild_id.to_string(), &ctx.data.role_id.to_string()).await;
    Ok(())
}
