//! Port of `src/services/captchaService.js`: captcha verification (the `captcha` module).
//!
//! Everything lives here, as in the JS: pure helpers, the captcha image ([`image`]), the challenge flow, the panel,
//! the lock-down helpers, the verify-existing batch, diagnostics ([`panel`]) and the hook, job and component bodies
//! ([`flow`]). `/captcha` ([`crate::commands::captcha`]) only builds the command and orchestrates replies.
//!
//! Data: `GuildSettings.captcha*` (read through the settings service only) and `CaptchaVerification`, a row per member
//! that is "pending" (joined while the gate was on) or holds a live image challenge. A cache miss never deletes anything:
//! rows go away on verification, on a leave event, after a kick, or when a REST fetch says the member is unknown.
//!
//! The answer is never stored in clear text, never logged, and never appears in a customId, the message text, the
//! embed or the attachment (static name, no description). Custom ids are all static.

pub mod flow;
pub mod image;
pub mod panel;

pub use flow::*;
pub use image::*;
pub use panel::*;

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use bot_core::App;
use bot_core::embed_builder::apply_guild_color;
use bot_core::prelude::*;
use bot_db::{CAPTCHA_VERIFICATION_INFO, CaptchaVerification, GuildSettings, InsertBuilder, PgPool, SelectBuilder};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use serenity::all::{ButtonStyle, Guild, GuildChannel, Member, Permissions, ReactionType, Role, RoleId, UserId};
use serenity::builder::{
    CreateActionRow, CreateAllowedMentions, CreateAttachment, CreateButton, CreateEmbed, CreateInteractionResponseMessage,
    EditInteractionResponse,
};
use sha2::{Digest, Sha256};

// ── Constants ─────────────────────────────────────────────────────────────────

/// 20 glyphs. Left out: 0 O D Q, 1 I L, 2 Z, 5 S, 8 B, G, U V, lowercase.
pub const CAPTCHA_ALPHABET: &str = "ACEFHJKMNPRTWXY34679";
pub const CODE_LENGTH: usize = 6;
pub const CHALLENGE_TTL_MS: i64 = 10 * 60_000;
pub const MAX_ATTEMPTS: i32 = 3;
pub const TIMEOUT_MIN_MINUTES: i64 = 5;
/// 7 days.
pub const TIMEOUT_MAX_MINUTES: i64 = 10_080;
pub const IMAGE_WIDTH: u32 = 320;
pub const IMAGE_HEIGHT: u32 = 120;
pub const IMAGE_FILE_NAME: &str = "captcha.png";
pub const IMAGE_TIMEOUT_MS: u64 = 5_000;

/// A sliding-window cap: at most `max` in `window_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub max: i64,
    pub window_ms: i64,
}

pub const USER_IMAGE_LIMIT: Limit = Limit { max: 10, window_ms: 600_000 };
pub const GUILD_IMAGE_LIMIT: Limit = Limit { max: 120, window_ms: 60_000 };
pub const KICK_DM_LIMIT: Limit = Limit { max: 30, window_ms: 60_000 };
pub const VERIFY_EXISTING_BATCH: usize = 2000;
/// The fallback form lookup in the "role required" reply.
pub const FORM_LOOKUP_MS: u64 = 2_000;
/// MEE6 "@verified".
pub const DEFAULT_ROLE_NAME: &str = "verified";
/// MEE6 "#verification".
pub const DEFAULT_CHANNEL_NAME: &str = "verification";

/// `BUTTON_STYLES`: the stored style names and their Discord styles.
pub const BUTTON_STYLES: [(&str, ButtonStyle); 4] =
    [("SUCCESS", ButtonStyle::Success), ("PRIMARY", ButtonStyle::Primary), ("SECONDARY", ButtonStyle::Secondary), ("DANGER", ButtonStyle::Danger)];

/// `STYLE_NAMES`: how `/captcha message` names the styles.
pub const STYLE_NAMES: [(&str, &str); 4] = [("SUCCESS", "Green"), ("PRIMARY", "Blurple"), ("SECONDARY", "Grey"), ("DANGER", "Red")];

/// `DEFAULT_VERIFY_BUTTON`.
pub const DEFAULT_VERIFY_LABEL: &str = "Verify";
pub const DEFAULT_VERIFY_STYLE: &str = "SUCCESS";

/// `CUSTOM_IDS` (all static).
pub mod custom_ids {
    pub const START: &str = "captcha:start";
    pub const ANSWER: &str = "captcha:answer";
    pub const MODAL_ANSWER: &str = "captcha-modal:answer";
    pub const MODAL_PANEL: &str = "captcha-modal:panel";
    pub const ANSWER_FIELD: &str = "captcha-answer";
}

/// `PANEL_NEEDS`.
pub const PANEL_NEEDS: [&str; 4] = ["ViewChannel", "SendMessages", "EmbedLinks", "ReadMessageHistory"];

/// `FORM_BUTTON_LABEL`.
pub const FORM_BUTTON_LABEL: &str = "Fill out the form";
const LINKED_ROLE_HELP: &str = "is a Linked Role: connect the account it asks for in **User Settings → Connections**, then click the server name → **Linked Roles** and claim it. Then press **Verify** again.";

/// `auditReason(interaction, what)`: `"<what> by <user tag>"`, at most 512.
pub fn audit_reason(user_tag: &str, what: &str) -> String {
    bot_util::text::clamp_text(&format!("{what} by {user_tag}"), 512)
}

/// `CaptchaConfigError`: an admin-facing refusal shown as an error embed (`title`, default "Not Saved").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptchaConfigError {
    pub message: String,
    pub title: Option<String>,
}

impl CaptchaConfigError {
    pub fn new(message: impl Into<String>, title: Option<&str>) -> Self {
        Self { message: message.into(), title: title.map(str::to_string) }
    }
}

impl std::fmt::Display for CaptchaConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CaptchaConfigError {}

/// `timeoutJobKey(guildId, userId)`.
pub fn timeout_job_key(guild_id: &str, user_id: &str) -> String {
    format!("captcha.timeout:{guild_id}:{user_id}")
}

/// `defaultPanelTemplate()`: our default panel (no colour: embedColor / COLORS.INFO applies).
pub fn default_panel_template() -> Value {
    json!({
        "content": null,
        "embed": {
            "title": "🛡️ Verification required",
            "description": "Welcome to **{server}**!\n\nTo make sure you are human, press **Verify** below. Once you pass, you get access to the rest of the server.",
            "footer": { "text": "Only you can see your captcha." },
        },
        "buttons": [],
    })
}

// ── Replies ───────────────────────────────────────────────────────────────────

/// A message payload as the captcha flow sends it (`{ content, embeds, components, files, attachments, allowedMentions }`).
/// Unset fields are left out of an edit. `clear_attachments` is the JS `attachments: []`: editing a challenge message
/// with it removes the stale image.
#[derive(Debug, Clone, Default)]
pub struct Reply {
    pub content: Option<String>,
    pub embeds: Option<Vec<CreateEmbed>>,
    pub components: Option<Vec<CreateActionRow>>,
    pub files: Vec<CreateAttachment>,
    pub clear_attachments: bool,
    pub allowed_mentions: Option<CreateAllowedMentions>,
}

impl Reply {
    /// `full(embed)`: a full replacement payload (`{ content: '', embeds: [embed], components: [], attachments: [] }`).
    pub fn full(embed: CreateEmbed) -> Self {
        Self { content: Some(String::new()), embeds: Some(vec![embed]), components: Some(Vec::new()), files: Vec::new(), clear_attachments: true, allowed_mentions: None }
    }

    /// `{ content }` only.
    pub fn text(content: impl Into<String>) -> Self {
        Self { content: Some(content.into()), ..Self::default() }
    }

    /// `{ content, embeds: [], components: [], attachments: [] }`.
    pub fn text_only(content: impl Into<String>) -> Self {
        Self { content: Some(content.into()), embeds: Some(Vec::new()), components: Some(Vec::new()), files: Vec::new(), clear_attachments: true, allowed_mentions: None }
    }

    /// `interaction.editReply(payload)`.
    pub fn to_edit(&self) -> EditInteractionResponse {
        let mut e = EditInteractionResponse::new();
        if let Some(c) = &self.content {
            e = e.content(c.clone());
        }
        if let Some(em) = &self.embeds {
            e = e.embeds(em.clone());
        }
        if let Some(c) = &self.components {
            e = e.components(c.clone());
        }
        if let Some(a) = &self.allowed_mentions {
            e = e.allowed_mentions(a.clone());
        }
        if !self.files.is_empty() {
            // New files replace every existing attachment (the JS `attachments: []` plus `files`).
            for f in &self.files {
                e = e.new_attachment(f.clone());
            }
        } else if self.clear_attachments {
            e = e.clear_attachments();
        }
        e
    }

    /// `interaction.reply({ ...payload, ephemeral: true })` (`attachments` only exists on edits, so it is dropped).
    pub fn to_ephemeral_reply(&self) -> CreateInteractionResponseMessage {
        let mut m = CreateInteractionResponseMessage::new().ephemeral(true);
        if let Some(c) = &self.content {
            m = m.content(c.clone());
        }
        if let Some(em) = &self.embeds {
            m = m.embeds(em.clone());
        }
        if let Some(c) = &self.components {
            m = m.components(c.clone());
        }
        if let Some(a) = &self.allowed_mentions {
            m = m.allowed_mentions(a.clone());
        }
        if !self.files.is_empty() {
            m = m.add_files(self.files.clone());
        }
        m
    }

    /// The embeds as JSON (tests and logs).
    pub fn embeds_json(&self) -> Vec<Value> {
        self.embeds.iter().flatten().map(|e| serde_json::to_value(e).unwrap_or(Value::Null)).collect()
    }
}

/// The facts of the "you need a role" reply.
#[derive(Debug, Clone, Default)]
pub struct NeedsRole {
    pub role_ids: Vec<String>,
    pub linked_role_ids: Vec<String>,
    /// The Applications Apply button, when the fallback form is usable.
    pub form_button: Option<CreateButton>,
}

/// `needsRoleText({ roleIds, linkedRoleIds, hasForm })`.
pub fn needs_role_text(role_ids: &[String], linked_role_ids: &[String], has_form: bool) -> String {
    let mentions: Vec<String> = role_ids.iter().map(|id| format!("<@&{id}>")).collect();
    let mut lines = vec![format!("To verify here you need one of these roles: {}.", mentions.join(", "))];
    for id in linked_role_ids {
        lines.push(format!("**<@&{id}>** {LINKED_ROLE_HELP}"));
    }
    if has_form {
        lines.push(if linked_role_ids.is_empty() {
            format!("Don't have one? Press **{FORM_BUTTON_LABEL}** below; staff will review it.")
        } else {
            format!("No linked account? Press **{FORM_BUTTON_LABEL}** below; staff will review it.")
        });
    } else if linked_role_ids.is_empty() {
        lines.push("If you think you should have one, please ask a server admin.".to_string());
    }
    lines.join("\n\n")
}

/// `MSG`: the messages shown to members (all ephemeral). Every factory builds a fresh payload (fresh timestamp).
pub mod msg {
    use super::*;

    /// `MSG.MODULE_OFF`.
    pub fn module_off() -> String {
        let label = bot_config::get_module("captcha").map(|m| m.label).unwrap_or("Verification");
        format!("The **{label}** module is turned off on this server.")
    }
    pub fn temp_unavailable() -> Reply {
        Reply::full(error_embed("Temporarily Unavailable", Some("Server settings could not be loaded. Please try again in a moment."), None))
    }
    pub fn not_set_up() -> Reply {
        Reply::full(error_embed("Verification Unavailable", Some("Verification isn't set up correctly on this server. Please tell a server admin."), None))
    }
    pub fn already() -> Reply {
        Reply::full(success_embed("Already Verified", Some("You're already verified."), None))
    }
    pub fn screening_first() -> Reply {
        Reply::full(warn_embed(
            "Rules Screening First",
            Some("Please complete this server's rules screening first (the prompt at the bottom of the channel list), then press **Verify** again."),
            None,
        ))
    }
    pub fn needs_role(n: NeedsRole) -> Reply {
        let text = needs_role_text(&n.role_ids, &n.linked_role_ids, n.form_button.is_some());
        let mut r = Reply::full(warn_embed("Role Required", Some(&text), None));
        r.components = Some(n.form_button.map(|b| vec![CreateActionRow::Buttons(vec![b])]).unwrap_or_default());
        // Role mentions in an embed never ping; this keeps it so if the text ever moves to content.
        r.allowed_mentions = Some(CreateAllowedMentions::new());
        r
    }
    pub fn rate_user() -> Reply {
        Reply::full(warn_embed("Slow Down", Some("You've asked for a lot of codes. Please try again in a few minutes."), None))
    }
    pub fn rate_guild() -> Reply {
        Reply::full(warn_embed("Busy", Some("Lots of people are verifying right now. Please try again in a minute."), None))
    }
    pub fn image_failed() -> Reply {
        Reply::full(error_embed("Try Again", Some("I couldn't draw a captcha just now. Press **Verify** again in a moment."), None))
    }
    pub fn role_failed() -> Reply {
        Reply::full(error_embed(
            "Verification Failed",
            Some("I couldn't give you the verified role because I'm missing permissions. Please tell a server admin."),
            None,
        ))
    }
    pub fn verified(guild_name: &str) -> Reply {
        let text = format!("You now have access to **{}**. Welcome!", bot_util::text::clamp_text(guild_name, 100));
        Reply::full(success_embed("Verified", Some(&text), None))
    }
    /// `MSG.FONT_MISSING`.
    pub const FONT_MISSING: &str =
        "The captcha font (assets/fonts/Nunito-Bold.ttf) is missing and no system font is available, so the one-click button is used instead.";
}

// ── In-app state (never authoritative; no timers, no I/O at construction) ─────

/// The per-app captcha state (`userMutex`, `inFlight`, `bulkRuns`, `warnThrottle`).
#[derive(Default)]
pub struct CaptchaState {
    /// `${guildId}:${userId}`: Verify, the answer form and the timeout job.
    pub user_mutex: bot_core::sync::KeyedMutex,
    /// `${guildId}:${userId}` while [`complete_verification`] runs.
    in_flight: Mutex<HashSet<String>>,
    /// One verify-existing batch per guild.
    pub(crate) bulk_runs: Mutex<std::collections::HashMap<String, Arc<Mutex<BulkRun>>>>,
    warn_throttle: OnceLock<bot_util::async_util::Throttle>,
}

pub(crate) fn state(app: &App) -> Arc<CaptchaState> {
    app.ext::<CaptchaState>()
}

/// `throttledWarn(key, message)`: at most once per 10 minutes per key.
pub(crate) fn throttled_warn(app: &App, key: &str, message: &str) {
    let st = state(app);
    let t = st.warn_throttle.get_or_init(|| bot_core::clock::clock_throttle(app.clock(), 10 * 60_000));
    if t.allow(key) {
        bot_util::log_warn!(message.to_string());
    }
}

/// Removes the in-flight mark when dropped (the JS `finally`).
pub(crate) struct InFlightGuard {
    st: Arc<CaptchaState>,
    key: String,
}

impl InFlightGuard {
    pub(crate) fn new(app: &App, key: String) -> Self {
        let st = state(app);
        if let Ok(mut s) = st.in_flight.lock() {
            s.insert(key.clone());
        }
        Self { st, key }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Ok(mut s) = self.st.in_flight.lock() {
            s.remove(&self.key);
        }
    }
}

/// Whether [`complete_verification`] is running for this member.
pub fn is_in_flight(app: &App, guild_id: &str, user_id: &str) -> bool {
    state(app).in_flight.lock().map(|s| s.contains(&format!("{guild_id}:{user_id}"))).unwrap_or(false)
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

/// `generateCode(rand)`: a random code; `rand(n)` returns an integer in `[0, n)` (the default is the OS-seeded CSPRNG).
pub fn generate_code_with(mut rand: impl FnMut(usize) -> usize) -> String {
    let glyphs: Vec<char> = CAPTCHA_ALPHABET.chars().collect();
    (0..CODE_LENGTH).map(|_| glyphs[rand(glyphs.len()).min(glyphs.len() - 1)]).collect()
}

/// `generateCode()` with `crypto.randomInt`.
pub fn generate_code() -> String {
    generate_code_with(|n| rand::random_range(0..n))
}

/// `normalizeAnswer(input)`: upper-cases and drops spaces, dashes, underscores and dots.
pub fn normalize_answer(input: Option<&str>) -> String {
    input.unwrap_or("").to_uppercase().chars().filter(|c| !(bot_util::js::is_js_space(*c) || matches!(c, '-' | '_' | '.'))).collect()
}

/// `hashAnswer(guildId, userId, answer)`: sha256 hex of `guildId:userId:NORMALIZED`.
pub fn hash_answer(guild_id: &str, user_id: &str, answer: &str) -> String {
    let digest = Sha256::digest(format!("{guild_id}:{user_id}:{}", normalize_answer(Some(answer))).as_bytes());
    hex::encode(digest)
}

fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `answerMatches(hash, guildId, userId, input)`: constant-time; a malformed hash or a wrong-length answer is false.
pub fn answer_matches(hash: Option<&str>, guild_id: &str, user_id: &str, input: Option<&str>) -> bool {
    let Some(hash) = hash.filter(|h| is_hash(h)) else { return false };
    let n = normalize_answer(input);
    if bot_util::js::utf16_len(&n) != CODE_LENGTH {
        return false;
    }
    let (Ok(a), Ok(b)) = (hex::decode(hash_answer(guild_id, user_id, &n)), hex::decode(hash.to_ascii_lowercase())) else { return false };
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The challenge part of a `CaptchaVerification` row.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Challenge {
    pub answer_hash: Option<String>,
    pub attempts: i32,
    pub expires_at: Option<DateTime<Utc>>,
}

impl From<&CaptchaVerification> for Challenge {
    fn from(r: &CaptchaVerification) -> Self {
        Self { answer_hash: r.answer_hash.clone(), attempts: r.attempts, expires_at: r.expires_at }
    }
}

/// What [`evaluate_answer`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    None,
    Expired,
    Exhausted,
    Ok,
    Wrong,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Expired => "expired",
            Self::Exhausted => "exhausted",
            Self::Ok => "ok",
            Self::Wrong => "wrong",
        }
    }
}

/// `evaluateAnswer(...)`'s result: `{ result, attemptsLeft? }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Evaluation {
    pub result: Verdict,
    pub attempts_left: Option<i32>,
}

/// `evaluateAnswer({ row, guildId, userId, input, now })`: judges a submitted answer against the stored challenge.
pub fn evaluate_answer(row: Option<&Challenge>, guild_id: &str, user_id: &str, input: Option<&str>, now_ms: i64) -> Evaluation {
    let e = |result, attempts_left| Evaluation { result, attempts_left };
    let Some(row) = row.filter(|r| r.answer_hash.is_some()) else { return e(Verdict::None, None) };
    match row.expires_at {
        Some(at) if at.timestamp_millis() > now_ms => {}
        _ => return e(Verdict::Expired, None),
    }
    if row.attempts >= MAX_ATTEMPTS {
        return e(Verdict::Exhausted, Some(0));
    }
    if answer_matches(row.answer_hash.as_deref(), guild_id, user_id, input) {
        return e(Verdict::Ok, None);
    }
    let used = row.attempts + 1;
    if used >= MAX_ATTEMPTS {
        return e(Verdict::Exhausted, Some(0));
    }
    e(Verdict::Wrong, Some(MAX_ATTEMPTS - used))
}

/// The two refusals of [`validate_timeout_minutes`].
pub const TIMEOUT_NOT_WHOLE: &str = "Use a whole number of minutes.";
pub const TIMEOUT_OUT_OF_RANGE: &str = "Use 0 (never) or between 5 and 10080 minutes (7 days).";

/// `validateTimeoutMinutes(n)` for a JSON value (`Number.isInteger`): the minutes, or the refusal text.
pub fn validate_timeout_value(n: &Value) -> Result<i64, String> {
    let Some(f) = n.as_f64().filter(|f| f.is_finite() && f.fract() == 0.0) else { return Err(TIMEOUT_NOT_WHOLE.into()) };
    validate_timeout_minutes(f as i64)
}

/// `validateTimeoutMinutes(n)` for a whole number.
pub fn validate_timeout_minutes(n: i64) -> Result<i64, String> {
    if n == 0 {
        return Ok(0);
    }
    if !(TIMEOUT_MIN_MINUTES..=TIMEOUT_MAX_MINUTES).contains(&n) {
        return Err(TIMEOUT_OUT_OF_RANGE.into());
    }
    Ok(n)
}

/// `timeoutRunAt(joinedTimestamp, minutes, now)`: counts from the join but is never less than a minute away.
pub fn timeout_run_at(joined_ms: Option<i64>, minutes: i64, now_ms: i64) -> DateTime<Utc> {
    let at = (now_ms + 60_000).max(joined_ms.unwrap_or(now_ms) + minutes * 60_000);
    DateTime::from_timestamp_millis(at).unwrap_or_else(Utc::now)
}

/// The stored verify button (`{ label, style }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyButton {
    pub label: String,
    pub style: String,
}

impl VerifyButton {
    pub fn to_json(&self) -> Value {
        json!({ "label": self.label, "style": self.style })
    }

    /// The Discord style.
    pub fn button_style(&self) -> ButtonStyle {
        BUTTON_STYLES.iter().find(|(k, _)| *k == self.style).map(|(_, s)| *s).unwrap_or(ButtonStyle::Success)
    }

    /// `STYLE_NAMES[style]`.
    pub fn style_name(&self) -> &'static str {
        STYLE_NAMES.iter().find(|(k, _)| *k == self.style).map(|(_, n)| *n).unwrap_or("Green")
    }
}

/// `parseVerifyButton(raw)`: invalid or missing parts fall back to the defaults. Never fails.
pub fn parse_verify_button(raw: Option<&Value>) -> VerifyButton {
    let label = raw
        .and_then(|r| r.get("label"))
        .and_then(Value::as_str)
        .map(bot_util::js::trim)
        .filter(|l| !l.is_empty())
        .map(|l| bot_util::text::clamp_text(l, 80))
        .unwrap_or_else(|| DEFAULT_VERIFY_LABEL.to_string());
    let style = raw
        .and_then(|r| r.get("style"))
        .and_then(Value::as_str)
        .filter(|s| BUTTON_STYLES.iter().any(|(k, _)| k == s))
        .unwrap_or(DEFAULT_VERIFY_STYLE)
        .to_string();
    VerifyButton { label, style }
}

/// `panelTemplate(settings)` on the raw `captchaTemplate`: the normalized template (the stored `verifyButton` key is
/// dropped), or the default when the stored one has no text and no embed.
pub fn panel_template_of(raw: Option<&Value>) -> Value {
    if let Some(r) = raw.filter(|r| r.is_object() || r.is_array()) {
        let t = bot_core::custom_embed::normalize_template(r);
        let has_content = t["content"].as_str().is_some_and(|c| !c.is_empty());
        if has_content || !t["embed"].is_null() {
            return t;
        }
    }
    bot_core::custom_embed::normalize_template(&default_panel_template())
}

/// `panelTemplate(settings)`.
pub fn panel_template(settings: Option<&GuildSettings>) -> Value {
    panel_template_of(settings.and_then(|s| s.captcha_template.as_ref()))
}

/// The placeholder context of a message that has no member: `{server}` and `{channel}` only (`{user}` stays literal).
pub fn guild_render_context(guild: Option<&Guild>, channel: Option<&GuildChannel>) -> bot_util::placeholder_parser::RenderContext {
    bot_util::placeholder_parser::RenderContext {
        guild: guild.map(bot_core::adapters::placeholder_guild),
        channel: channel.map(bot_core::adapters::placeholder_channel),
        ..Default::default()
    }
}

/// `buildPanelPayload(settings, guild, channel)`: the template plus the Verify button row (always last). Never `None`.
pub fn build_panel_payload(settings: Option<&GuildSettings>, ctx: &bot_util::placeholder_parser::RenderContext) -> bot_core::custom_embed::Rendered {
    use bot_core::custom_embed::{RenderOptions, normalize_template, render_template};
    let btn = parse_verify_button(settings.and_then(|s| s.captcha_template.as_ref()).and_then(|t| t.get("verifyButton")));
    let row = CreateActionRow::Buttons(vec![
        CreateButton::new(custom_ids::START).label(btn.label.clone()).style(btn.button_style()).emoji(ReactionType::Unicode("✅".into())),
    ]);
    let opts = || RenderOptions {
        default_color: Some(settings.and_then(|s| s.embed_color).map(|c| c as u32).unwrap_or(bot_config::colors::INFO)),
        // The panel never pings.
        allowed_mentions: Some(CreateAllowedMentions::new()),
        extra_components: vec![row.clone()],
    };
    let rendered = render_template(&panel_template(settings), ctx, opts());
    match rendered {
        // A colour-only or otherwise invisible template renders to just the button: show the default text instead.
        Some(r) if r.content.is_some() || !r.embeds.is_empty() => r,
        _ => render_template(&normalize_template(&default_panel_template()), ctx, opts()).unwrap_or(bot_core::custom_embed::Rendered {
            content: None,
            embeds: Vec::new(),
            link_rows: Vec::new(),
            extra: vec![row],
            allowed_mentions: CreateAllowedMentions::new(),
        }),
    }
}

/// The challenge embed's text.
pub fn challenge_text(expires_at: &DateTime<Utc>) -> String {
    format!(
        "Type the **{CODE_LENGTH} characters** from the image, then press **Enter code**.\nCapital letters and numbers only; case does not matter. You have **{MAX_ATTEMPTS} tries**. This code expires {}.",
        bot_util::time::discord_timestamp(expires_at, "R")
    )
}

/// The challenge's button row: Enter code, New image.
pub fn challenge_row() -> CreateActionRow {
    CreateActionRow::Buttons(vec![
        CreateButton::new(custom_ids::ANSWER).label("Enter code").style(ButtonStyle::Primary).emoji(ReactionType::Unicode("⌨️".into())),
        CreateButton::new(custom_ids::START).label("New image").style(ButtonStyle::Secondary).emoji(ReactionType::Unicode("🔄".into())),
    ])
}

/// `buildChallengePayload({ settings, expiresAt, image, content })`: the ephemeral challenge message. The attachment has a
/// static name and no description (alt text).
pub fn build_challenge_payload(settings: Option<&GuildSettings>, expires_at: &DateTime<Utc>, image: Vec<u8>, content: &str) -> Reply {
    let embed = apply_guild_color(info_embed("🔐 Verification", Some(&challenge_text(expires_at)), None), settings)
        .image(format!("attachment://{IMAGE_FILE_NAME}"));
    Reply {
        content: Some(content.to_string()),
        embeds: Some(vec![embed]),
        components: Some(vec![challenge_row()]),
        files: vec![CreateAttachment::bytes(image, IMAGE_FILE_NAME)],
        // Removes the previous image when editing.
        clear_attachments: true,
        allowed_mentions: None,
    }
}

/// `isRoleNewlyAdded({ oldMember, newMember, oldKnown, roleId })`: with an uncached old member the role being present is
/// enough.
pub fn is_role_newly_added(old_roles: Option<&[RoleId]>, new_roles: &[RoleId], old_known: bool, role_id: Option<&str>) -> bool {
    let Some(rid) = role_id.filter(|r| !r.is_empty()) else { return false };
    let has = |roles: &[RoleId]| roles.iter().any(|r| r.to_string() == rid);
    has(new_roles) && (!old_known || !old_roles.is_some_and(has))
}

/// `isLocked(guild)`: @everyone cannot view channels (the flag itself; no Administrator shortcut).
pub fn is_locked(guild: &Guild) -> bool {
    let everyone = bot_core::permissions::everyone_role_id(guild);
    let perms = guild.roles.get(&everyone).map(|r| r.permissions).unwrap_or_else(Permissions::empty);
    !bot_core::permissions::has_exact(perms, Permissions::VIEW_CHANNEL)
}

/// `everyoneCanView(channel)`.
pub fn everyone_can_view(guild: &Guild, channel: &GuildChannel) -> bool {
    bot_core::privacy_guards::everyone_can_view(guild, Some(channel)) == Some(true)
}

/// `botSeesWithoutEveryone(guild)`: the bot would still see channels without @everyone's View Channels (Administrator
/// counts).
pub fn bot_sees_without_everyone(guild: &Guild, bot_id: UserId) -> bool {
    let Some(me) = bot_core::permissions::me(guild, bot_id) else { return false };
    me.roles.iter().filter(|r| r.get() != guild.id.get()).filter_map(|r| guild.roles.get(r)).any(|r| bot_core::permissions::js_has(r.permissions, Permissions::VIEW_CHANNEL))
}

/// Which lock-down steps are still needed (`true` = needed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockPlan {
    pub channel: bool,
    pub role: bool,
    pub everyone: bool,
}

/// `lockPlan(guild, role, channel)`.
pub fn lock_plan(guild: &Guild, role: &Role, channel: &GuildChannel) -> LockPlan {
    use serenity::all::PermissionOverwriteType;
    let everyone_allows = channel.permission_overwrites.iter().any(|o| {
        matches!(o.kind, PermissionOverwriteType::Role(r) if r.get() == guild.id.get()) && bot_core::permissions::js_has(o.allow, Permissions::VIEW_CHANNEL)
    });
    LockPlan {
        channel: !everyone_allows,
        role: !bot_core::permissions::js_has(role.permissions, Permissions::VIEW_CHANNEL),
        everyone: !is_locked(guild),
    }
}

/// `isKickExempt(member, settings)`: the owner, bots, moderators and above, immune roles.
pub fn is_kick_exempt(guild: &Guild, member: &Member, settings: Option<&GuildSettings>, bot_owner_id: Option<&str>) -> bool {
    if member.user.id == guild.owner_id || member.user.bot {
        return true;
    }
    let access = bot_core::permission_checker::MemberAccess {
        user_id: member.user.id.to_string(),
        guild_owner_id: Some(guild.owner_id.to_string()),
        permissions: Some(bot_core::permissions::guild_permissions(guild, member.user.id, &member.roles)),
        role_ids: bot_core::interaction_helpers::member_role_ids(guild, member).iter().map(ToString::to_string).collect(),
    };
    if resolve_permission_level(Some(&access), settings, bot_owner_id) >= PermissionLevel::Moderator {
        return true;
    }
    let immune = settings.map(|s| s.immune_role_ids.clone()).unwrap_or_default();
    immune.iter().any(|id| member.roles.iter().any(|r| r.to_string() == *id))
}

/// A yes/no check with the reason of a no.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub ok: bool,
    pub reason: Option<String>,
}

impl Check {
    pub fn ok() -> Self {
        Self { ok: true, reason: None }
    }
    pub fn no(reason: impl Into<String>) -> Self {
        Self { ok: false, reason: Some(reason.into()) }
    }
}

/// `checkVerifiedRole(guild, role, settings, { executor })`: whether a role may be the verified role.
pub fn check_verified_role(guild: &Guild, role: Option<&Role>, bot_id: UserId, settings: Option<&GuildSettings>, executor: Option<&Member>) -> Check {
    let chk = bot_core::interaction_helpers::check_assignable_role(guild, role, bot_id, executor, false);
    if !chk.ok {
        let reason = chk.reason.clone().unwrap_or_default();
        let hint = if reason.to_lowercase().contains("my highest role") {
            " Move my highest role above it in **Server Settings → Roles**."
        } else if !chk.dangerous.is_empty() {
            " Everyone who verifies gets this role, so roles with these permissions are refused."
        } else {
            ""
        };
        return Check::no(format!("{reason}{hint}"));
    }
    let Some(role) = role else { return Check::no("Role not found.") };
    let id = role.id.to_string();
    if let Some(s) = settings {
        if s.mod_role_ids.contains(&id) {
            return Check::no("That's a moderator role (/modconfig): everyone who verifies would become a moderator.");
        }
        if s.immune_role_ids.contains(&id) {
            return Check::no("That's an immune role (/modconfig): everyone who verifies would be immune to moderation.");
        }
        if s.mute_role_id.as_deref() == Some(id.as_str()) {
            return Check::no("That's the mute role.");
        }
    }
    Check::ok()
}

/// [`check_verified_role`] on the cached guild (the dashboard and the commands). `Role not found.` offline.
pub fn check_verified_role_cached(app: &App, guild_id: serenity::all::GuildId, role_id: RoleId, settings: Option<&GuildSettings>, executor: Option<&Member>) -> Check {
    let Some(bot) = bot_user_id(app) else { return Check::no("Role not found.") };
    bot_core::interaction_helpers::with_cached_guild(app, guild_id, |g| check_verified_role(g, g.roles.get(&role_id), bot, settings, executor))
        .unwrap_or_else(|| Check::no("Role not found."))
}

/// `checkVerifiedRole(guild, interaction.options.getRole(...), ...)`: the cached role, else the role Discord resolved
/// in the command option (discord.js hands the command the resolved role even when the cache lacks it).
pub fn check_verified_role_resolved(
    app: &App,
    guild_id: serenity::all::GuildId,
    role_id: RoleId,
    resolved: Option<&Role>,
    settings: Option<&GuildSettings>,
    executor: Option<&Member>,
) -> Check {
    let Some(bot) = bot_user_id(app) else { return Check::no("Role not found.") };
    bot_core::interaction_helpers::with_cached_guild(app, guild_id, |g| {
        let role = g.roles.get(&role_id).or(resolved.filter(|r| r.id == role_id));
        check_verified_role(g, role, bot, settings, executor)
    })
    .unwrap_or_else(|| Check::no("Role not found."))
}

/// discord.js `guild.roles._add(resolvedRole)`: a role resolved in a command option goes into the cache when the cache
/// lacks it, so later cache lookups find it, as in the JS.
pub fn adopt_resolved_role(app: &App, guild_id: serenity::all::GuildId, role: Option<&Role>) {
    if let Some(r) = role
        && bot_core::interaction_helpers::with_cached_guild(app, guild_id, |g| !g.roles.contains_key(&r.id)).unwrap_or(false)
    {
        bot_core::discord::cache_created_role(app, guild_id, r);
    }
}

pub(crate) fn bot_user_id(app: &App) -> Option<UserId> {
    app.discord().map(|d| d.cache.current_user().id)
}

/// `missingRequirement({ requiredRoleIds, memberRoleIds, guildRoleIds, exceptRoleId })`: the roles the member needs one
/// of, or `None` when nothing holds them back. Deleted ids and `except` (the verified role) are ignored.
pub fn missing_requirement<S: AsRef<str>>(required: &[S], member_role_ids: &[S], guild_role_ids: &[S], except: Option<&str>) -> Option<Vec<String>> {
    let exist: HashSet<&str> = guild_role_ids.iter().map(AsRef::as_ref).collect();
    let mut live: Vec<String> = Vec::new();
    for id in required.iter().map(AsRef::as_ref) {
        if Some(id) != except && exist.contains(id) && !live.iter().any(|l| l == id) {
            live.push(id.to_string());
        }
    }
    if live.is_empty() {
        return None;
    }
    let have: HashSet<&str> = member_role_ids.iter().map(AsRef::as_ref).collect();
    if live.iter().any(|id| have.contains(id.as_str())) { None } else { Some(live) }
}

/// `checkRequiredRole({ role, guildId, verifiedRoleId })`: whether a role may be required. A Linked Role always passes.
pub fn check_required_role(role_id: &str, managed: bool, guild_connections: bool, guild_id: &str, verified_role_id: Option<&str>) -> Check {
    if role_id == guild_id {
        return Check::no("@everyone is not a useful requirement: everybody has it.");
    }
    if Some(role_id) == verified_role_id {
        return Check::no("That's the verified role: nobody has it before they verify, so nobody could.");
    }
    if managed && !guild_connections {
        return Check::no("That's a bot or integration role, which members don't earn. Pick a normal role or a Linked Role.");
    }
    Check::ok()
}

/// What [`plan_required_roles`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequireOutcome {
    Added,
    Already,
    Removed,
    Absent,
    Cleared,
    Unchanged,
}

impl RequireOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Already => "already",
            Self::Removed => "removed",
            Self::Absent => "absent",
            Self::Cleared => "cleared",
            Self::Unchanged => "unchanged",
        }
    }
}

/// The new required-role list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolePlan {
    pub role_ids: Vec<String>,
    pub outcome: RequireOutcome,
    pub changed: bool,
    pub dropped: usize,
}

/// A change asked of [`plan_required_roles`].
#[derive(Debug, Clone, Default)]
pub struct RequireChange<'a> {
    pub current: &'a [String],
    pub guild_role_ids: &'a [String],
    pub add_id: Option<&'a str>,
    pub remove_id: Option<&'a str>,
    pub clear: bool,
    /// Default `LIMITS.CAPTCHA_REQUIRED_ROLES`.
    pub max: Option<usize>,
}

/// `planRequiredRoles(...)`: the new required-role list after `/captcha require`. Deleted roles drop out whenever the
/// list is saved. `Err` is the refusal text (the cap).
pub fn plan_required_roles(c: &RequireChange<'_>) -> Result<RolePlan, String> {
    let max = c.max.unwrap_or(bot_config::limits::CAPTCHA_REQUIRED_ROLES);
    let mut before: Vec<String> = Vec::new();
    for id in c.current {
        if !before.contains(id) {
            before.push(id.clone());
        }
    }
    let exist: HashSet<&str> = c.guild_role_ids.iter().map(String::as_str).collect();
    let kept: Vec<String> = if c.clear { Vec::new() } else { before.iter().filter(|id| exist.contains(id.as_str())).cloned().collect() };
    let dropped = if c.clear { 0 } else { before.len() - kept.len() };
    let mut role_ids = kept.clone();
    let mut outcome = if c.clear { RequireOutcome::Cleared } else { RequireOutcome::Unchanged };
    if !c.clear && c.add_id.is_some_and(|a| !a.is_empty()) {
        let add = c.add_id.unwrap_or_default();
        if kept.iter().any(|k| k == add) {
            outcome = RequireOutcome::Already;
        } else if kept.len() >= max {
            return Err(format!("At most {max} roles can be required. Remove one first."));
        } else {
            role_ids.push(add.to_string());
            outcome = RequireOutcome::Added;
        }
    } else if !c.clear && c.remove_id.is_some_and(|r| !r.is_empty()) {
        let rm = c.remove_id.unwrap_or_default();
        if kept.iter().any(|k| k == rm) {
            role_ids.retain(|id| id != rm);
            outcome = RequireOutcome::Removed;
        } else {
            outcome = RequireOutcome::Absent;
        }
    }
    let changed = role_ids != before;
    Ok(RolePlan { role_ids, outcome, changed, dropped })
}

/// `fallbackFormProblem({ form, moduleOn, memberRoleIds, reviewChannelProblem })`: why a member could not use the
/// fallback application form (`None` when they can). The module switch comes first, then the Applications rules.
pub fn fallback_form_problem(form: Option<&bot_db::ApplicationForm>, module_on: bool, member_role_ids: &[String], review_channel_problem: Option<String>, now: DateTime<Utc>) -> Option<String> {
    use feat_welcome::application_service::{ApplyFacts, Refusal, judge_apply};
    if !module_on {
        return Some("The Applications module is off (`/module enable applications`).".into());
    }
    let Some(form) = form else { return Some("The form no longer exists (`/captcha require form:none` clears it).".into()) };
    let verdict = judge_apply(&ApplyFacts { form, member_role_ids, review_channel_problem, pending: false, last_denied_at: None, now });
    match verdict {
        Ok(()) => None,
        Err(Refusal::FormDisabled) => Some("The form is closed (disabled).".into()),
        Err(Refusal::NotSetUp) => Some(
            if form.review_channel_id.as_deref().is_some_and(|c| !c.is_empty()) { "The form has no questions." } else { "The form has no review channel." }.into(),
        ),
        Err(Refusal::ReviewChannel { reason }) => Some(if reason.is_empty() { "The form's review channel is not usable.".into() } else { reason }),
        Err(Refusal::MissingRole { .. }) => Some("The form has required roles of its own, so members without them cannot use it.".into()),
        Err(_) => Some("The form cannot be used right now.".into()),
    }
}

/// A verify-existing batch's counters (`run`).
#[derive(Debug, Clone, Default)]
pub struct BulkRun {
    pub total: usize,
    pub done: usize,
    pub ok: usize,
    pub failed: usize,
    pub left: usize,
    pub abort: bool,
    pub error: Option<String>,
    pub remaining: usize,
    pub role_id: String,
    pub targets: HashSet<String>,
    pub started_at: i64,
    pub channel_message: Option<serenity::all::MessageId>,
}

pub(crate) fn fmt_n(n: impl Into<f64>) -> String {
    bot_util::text::format_number(n.into())
}

/// `progressText(run)`.
pub fn progress_text(run: &BulkRun) -> String {
    format!(
        "**{}/{}** processed · ✅ {} given · ⚠️ {} failed · 🚪 {} left",
        fmt_n(run.done as f64),
        fmt_n(run.total as f64),
        fmt_n(run.ok as f64),
        fmt_n(run.failed as f64),
        fmt_n(run.left as f64)
    )
}

/// `discordErrorText(err)` from a Discord error code and message.
pub fn discord_error_text(code: Option<u64>, message: Option<&str>) -> String {
    match code {
        Some(50013) => "I am missing a permission for that.".into(),
        Some(50001) => "I can't access that channel.".into(),
        Some(10003) => "The channel no longer exists.".into(),
        Some(10011) => "The role no longer exists.".into(),
        Some(10008) => "The message no longer exists.".into(),
        Some(30013) => "This server has reached the maximum number of channels or roles.".into(),
        _ => bot_util::text::clamp_text(message.unwrap_or("Unknown error"), 200),
    }
}

/// [`discord_error_text`] of a serenity error.
pub fn discord_error_text_of(err: &serenity::Error) -> String {
    discord_error_text(bot_core::discord::error_code(err), Some(&bot_core::discord::error_message(err)))
}

// ── Settings for the first response ───────────────────────────────────────────

/// `settingsForAck(interaction)`: the memo, else a load bounded by the acknowledgement budget. `None` when the budget
/// ran out or the database is down. Never fails.
pub async fn settings_for_ack(app: &App, guild_id: &str, budget_ms: u64) -> Option<Settings> {
    app.settings().peek_or_load(guild_id, budget_ms).await
}

// ── Rate limits ───────────────────────────────────────────────────────────────

/// The guild-wide image cap key.
pub fn guild_image_key(guild_id: &str) -> String {
    format!("captcha:img:{guild_id}")
}

/// The per-member image cap key.
pub fn user_image_key(guild_id: &str, user_id: &str) -> String {
    format!("captcha:img:{guild_id}:{user_id}")
}

/// The kick-DM cap key.
pub fn kick_dm_key(guild_id: &str) -> String {
    format!("captcha:dm:{guild_id}")
}

/// `checkImageLimits(guildId, userId)`: both windows fail OPEN (Redis down): a cache outage must not lock members out.
pub async fn check_image_limits(app: &App, guild_id: &str, user_id: &str) -> Option<Reply> {
    let g = app.cache().sliding_window(&guild_image_key(guild_id), GUILD_IMAGE_LIMIT.window_ms, None).await;
    if g.is_some_and(|n| n > GUILD_IMAGE_LIMIT.max) {
        return Some(msg::rate_guild());
    }
    let u = app.cache().sliding_window(&user_image_key(guild_id, user_id), USER_IMAGE_LIMIT.window_ms, None).await;
    if u.is_some_and(|n| n > USER_IMAGE_LIMIT.max) {
        return Some(msg::rate_user());
    }
    None
}

// ── Database (CaptchaVerification) ─────────────────────────────────────────────

/// The raw statements of this file (name, SQL), run in a rolled-back transaction by the tests.
pub const RAW_SQL: [(&str, &str); 6] = [
    ("countPending", r#"SELECT COUNT(*)::bigint FROM "CaptchaVerification" WHERE "guildId" = $1"#),
    ("deleteRow", r#"DELETE FROM "CaptchaVerification" WHERE "guildId" = $1 AND "userId" = $2"#),
    ("deleteStale", r#"DELETE FROM "CaptchaVerification" WHERE "guildId" = $1 AND "userId" = ANY($2)"#),
    (
        "countWrongTry",
        r#"UPDATE "CaptchaVerification" SET "attempts" = LEAST("attempts"::bigint + 1, 2147483647)::int, "updatedAt" = (now() AT TIME ZONE 'UTC') WHERE "guildId" = $1 AND "userId" = $2 AND "answerHash" = $3"#,
    ),
    (
        "clearChallenge",
        r#"UPDATE "CaptchaVerification" SET "answerHash" = NULL, "expiresAt" = NULL, "updatedAt" = (now() AT TIME ZONE 'UTC') WHERE "guildId" = $1 AND "userId" = $2"#,
    ),
    ("pendingUserIds", r#"SELECT "userId" FROM "CaptchaVerification" WHERE "guildId" = $1"#),
];

pub(crate) fn sql(name: &str) -> &'static str {
    RAW_SQL.iter().find(|(n, _)| *n == name).map(|(_, s)| *s).unwrap_or("SELECT 1")
}

fn db(e: bot_db::sqlx::Error) -> anyhow::Error {
    anyhow::Error::from(bot_db::DbError::from(e))
}

/// `prisma.captchaVerification.findUnique({ where: { guildId_userId } })`.
pub async fn find_row(pool: &PgPool, guild_id: &str, user_id: &str) -> anyhow::Result<Option<CaptchaVerification>> {
    let q = SelectBuilder::new(&CAPTCHA_VERIFICATION_INFO).filter("guildId", json!(guild_id))?.filter("userId", json!(user_id))?.build()?;
    Ok(q.fetch_optional::<CaptchaVerification, _>(pool).await?)
}

/// `deleteMany({ where: { guildId, userId } })`: the number of rows deleted.
pub async fn delete_row(pool: &PgPool, guild_id: &str, user_id: &str) -> anyhow::Result<u64> {
    let r = bot_db::sqlx::query(sql("deleteRow")).bind(guild_id).bind(user_id).execute(pool).await.map_err(db)?;
    Ok(r.rows_affected())
}

/// `deleteMany({ where: { guildId, userId: { in: ids } } })`.
pub async fn delete_rows(pool: &PgPool, guild_id: &str, user_ids: &[String]) -> anyhow::Result<u64> {
    if user_ids.is_empty() {
        return Ok(0);
    }
    let r = bot_db::sqlx::query(sql("deleteStale")).bind(guild_id).bind(user_ids).execute(pool).await.map_err(db)?;
    Ok(r.rows_affected())
}

/// `countPending(guildId)`: members still waiting to verify (pending rows, including unfinished challenges).
pub async fn count_pending(pool: &PgPool, guild_id: &str) -> anyhow::Result<i64> {
    let n: i64 = bot_db::sqlx::query_scalar(sql("countPending")).bind(guild_id).fetch_one(pool).await.map_err(db)?;
    Ok(n)
}

/// The user ids of the guild's rows.
pub async fn pending_user_ids(pool: &PgPool, guild_id: &str) -> anyhow::Result<Vec<String>> {
    let ids: Vec<String> = bot_db::sqlx::query_scalar(sql("pendingUserIds")).bind(guild_id).fetch_all(pool).await.map_err(db)?;
    Ok(ids)
}

/// A wrong try: `updateMany({ where: { guildId, userId, answerHash }, data: { attempts: { increment: 1 } } })`. Matching
/// on the hash means a stale submit never counts against a newer challenge.
pub async fn count_wrong_try(pool: &PgPool, guild_id: &str, user_id: &str, answer_hash: &str) -> anyhow::Result<u64> {
    let r = bot_db::sqlx::query(sql("countWrongTry")).bind(guild_id).bind(user_id).bind(answer_hash).execute(pool).await.map_err(db)?;
    Ok(r.rows_affected())
}

/// `updateMany({ where: { guildId, userId }, data: { answerHash: null, expiresAt: null } })`.
pub async fn clear_challenge(pool: &PgPool, guild_id: &str, user_id: &str) -> anyhow::Result<u64> {
    let r = bot_db::sqlx::query(sql("clearChallenge")).bind(guild_id).bind(user_id).execute(pool).await.map_err(db)?;
    Ok(r.rows_affected())
}

/// The join gate's row: `upsert({ create: { guildId, userId }, update: { answerHash: null, attempts: 0, expiresAt: null } })`.
pub async fn record_pending(app: &App, guild_id: &str, user_id: &str) -> anyhow::Result<CaptchaVerification> {
    app.settings().ensure_guild_settings(guild_id).await?;
    let q = InsertBuilder::new(&CAPTCHA_VERIFICATION_INFO)
        .values([("guildId", json!(guild_id)), ("userId", json!(user_id))])?
        .on_conflict_update(&["guildId", "userId"], [("answerHash", Value::Null), ("attempts", json!(0)), ("expiresAt", Value::Null)])?
        .build()?;
    Ok(q.fetch_one::<CaptchaVerification, _>(app.db()).await?)
}

/// The challenge write: `upsert({ create: { guildId, userId, answerHash, attempts: 0, expiresAt }, update: { answerHash,
/// attempts: 0, expiresAt } })`.
pub async fn store_challenge(app: &App, guild_id: &str, user_id: &str, answer_hash: &str, expires_at: DateTime<Utc>) -> anyhow::Result<CaptchaVerification> {
    app.settings().ensure_guild_settings(guild_id).await?;
    let at = json!(bot_db::format_js_date(&expires_at));
    let data = [("answerHash", json!(answer_hash)), ("attempts", json!(0)), ("expiresAt", at)];
    let q = InsertBuilder::new(&CAPTCHA_VERIFICATION_INFO)
        .values([("guildId", json!(guild_id)), ("userId", json!(user_id))])?
        .values(data.clone())?
        .on_conflict_update(&["guildId", "userId"], data)?
        .build()?;
    Ok(q.fetch_one::<CaptchaVerification, _>(app.db()).await?)
}

/// `resetPanelTemplate(guildId)`: restores the default panel text (SQL NULL, the JS `Prisma.DbNull`).
pub async fn reset_panel_template(app: &App, guild_id: &str) -> anyhow::Result<Settings> {
    Ok(app.settings().update_settings(guild_id, SettingsPatch::new().set("captchaTemplate", Value::Null)).await?)
}

/// Cancels a member's `captcha.timeout` job (never fails).
pub async fn cancel_timeout(app: &App, guild_id: &str, user_id: &str) -> u64 {
    app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(timeout_job_key(guild_id, user_id)), ..Default::default() }).await
}
