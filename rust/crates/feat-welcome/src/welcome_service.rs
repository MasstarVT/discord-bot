//! Port of `src/services/welcomeService.js`: Welcome & Goodbye.
//!
//! Everything behind `/welcome` (goodbye group included) and the welcome plugin: the default texts, healing of stored
//! JSON, the text/embed truth table, payload building (one render path for real joins and previews), the card
//! orchestration, the channel / DM / goodbye senders, autoroles (immediate, delayed through the durable scheduler,
//! screening watch, captcha gate), the hook and job bodies, and the setup modals.
//!
//! Settings are the typed [`GuildSettings`] row (JSON columns are `serde_json::Value`). Invite attribution belongs to
//! the invites package. Discord REST calls go through [`crate::io`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use bot_config::{JobType, PermissionLevel, colors, limits};
use bot_core::App;
use bot_core::adapters::{placeholder_channel, placeholder_guild, placeholder_member, placeholder_user};
use bot_core::custom_embed::{
    RenderOptions, build_template_modal, format_color, is_http_url, normalize_legacy_embed, parse_color_str, parse_template_modal,
    render_template, template_modal_json, validate_template,
};
use bot_core::embed_builder::{apply_guild_color, error_embed, info_embed, success_embed, warn_embed};
use bot_core::error::{BoxFuture, PermanentJobError};
use bot_core::hooks::HookCtx;
use bot_core::hooks::events::{GuildMemberAdd, GuildMemberRemove, GuildMemberUpdate, RoleDelete};
use bot_core::interaction::{CommandCtx, ModalCtx};
use bot_core::interaction_helpers::{PermTarget, Payload, cached_channel, cached_guild, check_assignable_role_cached, check_channel_usable, missing_bot_permissions};
use bot_core::permissions::permission_names;
use bot_core::scheduler::{CancelJobs, Job, JobCtx, JobOutcome, OnConflict, ScheduleJob};
use bot_core::settings::{Settings, SettingsPatch};
use bot_db::GuildSettings;
use bot_util::async_util::{Throttle, create_throttle, with_timeout};
use bot_util::js::{is_js_space, lower, trim, utf16_len};
use bot_util::placeholder_parser::{RenderContext, render_placeholders, render_placeholders_detailed};
use bot_util::text::{clamp_text, join_with_limit, js_round};
use serde_json::{Map, Value, json};
use serenity::all::{ButtonStyle, ChannelId, Guild, GuildChannel, GuildId, Member, Role, RoleId, User, UserId};
use serenity::builder::{CreateActionRow, CreateAllowedMentions, CreateAttachment, CreateButton, CreateEmbed, CreateModal};

use crate::io::{IoError, io};

// ── Constants ──────────────────────────────────────────────────────────────────

pub const DEFAULT_WELCOME_MESSAGE: &str = "Hey {user}, welcome to **{server}**!";
pub const DEFAULT_WELCOME_DM_MESSAGE: &str = "Have a great time here in **{server}**!";
pub const DEFAULT_GOODBYE_MESSAGE: &str = "**{user.idname}** just left the server";

/// `DEFAULT_WELCOME_EMBED` (the embed does not repeat the greeting: the content line carries it).
pub fn default_welcome_embed() -> Value {
    json!({
        "title": "👋 Welcome to {server}!",
        "description": "You are our **{server.member_count.ordinal}** member. Enjoy your stay!",
        "thumbnail": "{user.avatar}",
        "color": 0x57F287,
        "timestamp": true,
    })
}

/// `DEFAULT_GOODBYE_EMBED`.
pub fn default_goodbye_embed() -> Value {
    json!({ "description": DEFAULT_GOODBYE_MESSAGE, "thumbnail": "{user.avatar}", "color": 0x99AAB5, "timestamp": true })
}

/// Seven days.
pub const MAX_AUTOROLE_DELAY_SECONDS: i64 = 604_800;
/// Give up on members still pending Membership Screening after this.
pub const SCREENING_WATCH_MS: i64 = 7 * 24 * 3_600_000;
/// The watch job's poll interval.
pub const SCREENING_POLL_MS: i64 = 6 * 3_600_000;
/// The overall card budget inside the join hook.
pub const CARD_TIMEOUT_MS: u64 = 6_000;
/// `FLOOD_CAPS`: per guild per minute.
pub const FLOOD_CAPS: [(&str, i64); 4] = [("msg", 60), ("dm", 30), ("bye", 60), ("card", 60)];
pub const CARD_FILE_NAME: &str = "welcome.png";

/// The `ctx.state` key the captcha plugin sets (a `bool`) when it holds a join back for verification.
pub const CAPTCHA_PENDING_KEY: &str = "captchaPending";

/// Which message: the channel welcome, the welcome DM or the goodbye.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Welcome,
    Dm,
    Goodbye,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Welcome => "welcome",
            Kind::Dm => "dm",
            Kind::Goodbye => "goodbye",
        }
    }

    pub fn from_name(s: &str) -> Option<Kind> {
        match s {
            "welcome" => Some(Kind::Welcome),
            "dm" => Some(Kind::Dm),
            "goodbye" => Some(Kind::Goodbye),
            _ => None,
        }
    }

    /// `MESSAGE_FIELDS[kind]`.
    pub fn message_field(self) -> &'static str {
        match self {
            Kind::Welcome => "welcomeMessage",
            Kind::Dm => "welcomeDmMessage",
            Kind::Goodbye => "goodbyeMessage",
        }
    }

    /// `EMBED_FIELDS[kind]`.
    pub fn embed_field(self) -> &'static str {
        match self {
            Kind::Welcome => "welcomeEmbed",
            Kind::Dm => "welcomeDmEmbed",
            Kind::Goodbye => "goodbyeEmbed",
        }
    }

    /// `DEFAULT_MESSAGES[kind]`.
    pub fn default_message(self) -> &'static str {
        match self {
            Kind::Welcome => DEFAULT_WELCOME_MESSAGE,
            Kind::Dm => DEFAULT_WELCOME_DM_MESSAGE,
            Kind::Goodbye => DEFAULT_GOODBYE_MESSAGE,
        }
    }

    /// `MODAL_IDS[kind]`: the setup form's customId (a registered prefix, no params).
    pub fn modal_id(self) -> &'static str {
        match self {
            Kind::Welcome => "welcome-setup",
            Kind::Dm => "welcome-dm-setup",
            Kind::Goodbye => "goodbye-setup",
        }
    }

    fn modal_title(self) -> &'static str {
        match self {
            Kind::Welcome => "Welcome message",
            Kind::Dm => "Welcome DM",
            Kind::Goodbye => "Goodbye message",
        }
    }

    fn submit_title(self) -> &'static str {
        match self {
            Kind::Welcome => "Welcome Message Saved",
            Kind::Dm => "Welcome DM Saved",
            Kind::Goodbye => "Goodbye Message Saved",
        }
    }

    fn preview_command(self) -> &'static str {
        match self {
            Kind::Welcome => "/welcome test",
            Kind::Dm => "/welcome test type:dm",
            Kind::Goodbye => "/welcome goodbye test",
        }
    }
}

/// `JSON_COLUMNS`: the Json columns this package writes.
pub const JSON_COLUMNS: [&str; 4] = ["welcomeEmbed", "welcomeCardStyle", "welcomeDmEmbed", "goodbyeEmbed"];

/// `WelcomeConfigError`: a refused configuration change, with an optional embed title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WelcomeConfigError {
    pub message: String,
    pub title: Option<String>,
}

impl WelcomeConfigError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), title: None }
    }

    pub fn titled(message: impl Into<String>, title: impl Into<String>) -> Self {
        Self { message: message.into(), title: Some(title.into()) }
    }
}

impl std::fmt::Display for WelcomeConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for WelcomeConfigError {}

/// `autoroleJobKey(guildId, userId)`.
pub fn autorole_job_key(guild_id: &str, user_id: &str) -> String {
    format!("welcome.autorole:{guild_id}:{user_id}")
}

/// The module-level throttles (`warnThrottle`, `floodThrottle`), one per app.
pub struct WelcomeThrottles {
    warn: Throttle,
    flood: Throttle,
}

impl Default for WelcomeThrottles {
    fn default() -> Self {
        Self { warn: create_throttle(10 * 60_000), flood: create_throttle(60_000) }
    }
}

fn warn_once(app: &App, key: &str, message: String) {
    if app.ext::<WelcomeThrottles>().warn.allow(key) {
        bot_util::log_warn!(message);
    }
}

fn clamp_int(v: f64, lo: i64, hi: i64) -> i64 {
    if !v.is_finite() {
        return lo;
    }
    (v.trunc() as i64).clamp(lo, hi)
}

fn valid_color(c: Option<i64>) -> Option<u32> {
    c.filter(|c| (0..=0xff_ffff).contains(c)).map(|c| c as u32)
}

fn non_empty(v: Option<&str>) -> Option<String> {
    v.filter(|s| !s.is_empty()).map(String::from)
}

fn channel_label(id: Option<&str>) -> String {
    match id.filter(|s| !s.is_empty()) {
        Some(i) => format!("<#{i}>"),
        None => "not set".into(),
    }
}

fn parse_id(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|v| *v != 0)
}

fn now_ms(app: &App) -> i64 {
    app.clock().now_ms()
}

fn ms_to_utc(ms: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms).unwrap_or_else(chrono::Utc::now)
}

/// A settings row as the JSON the JS reads (Prisma field names).
pub fn settings_json(s: &GuildSettings) -> Value {
    serde_json::to_value(s).unwrap_or(Value::Null)
}

// ── Writing settings ───────────────────────────────────────────────────────────

/// `toDbData(data)`: the JS maps `null` on the Json columns to `Prisma.DbNull`. bot-db already binds `null` on a
/// `Json?` column as SQL NULL, so the patch is returned unchanged (kept for API parity).
pub fn to_db_data(data: SettingsPatch) -> SettingsPatch {
    data
}

/// `saveSettings(guildId, data)`.
pub async fn save_settings(app: &App, guild_id: &str, data: SettingsPatch) -> anyhow::Result<Settings> {
    Ok(app.settings().update_settings(guild_id, to_db_data(data)).await?)
}

/// `saveSettingsWith(guildId, mutator)`.
pub async fn save_settings_with<F>(app: &App, guild_id: &str, mutator: F) -> anyhow::Result<Settings>
where
    F: FnOnce(GuildSettings) -> anyhow::Result<SettingsPatch> + Send + 'static,
{
    app.settings().update_settings_with(guild_id, move |cur| mutator(cur).map(to_db_data)).await
}

// ── Embed JSON: healing, content test, serialisation ───────────────────────────

fn is_placeholder_url(s: &str) -> bool {
    s.contains('{') && s.contains('}')
}

fn is_attachment_url(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("attachment://") else { return false };
    let n = utf16_len(rest);
    (1..=200).contains(&n) && !rest.chars().any(|c| c == '/' || is_js_space(c))
}

fn image_url_ok(v: &Value) -> bool {
    v.as_str().is_some_and(|s| is_http_url(s) || is_placeholder_url(s) || is_attachment_url(s))
}

fn non_blank(v: Option<&Value>) -> bool {
    v.and_then(Value::as_str).is_some_and(|s| !trim(s).is_empty())
}

/// `hasEmbedContent(spec)`: a non-empty title, description, field list, author name or image. A footer-only or
/// thumbnail-only embed counts as empty (R053).
pub fn has_embed_content(spec: &Value) -> bool {
    if !spec.is_object() {
        return false;
    }
    non_blank(spec.get("title"))
        || non_blank(spec.get("description"))
        || spec.get("fields").and_then(Value::as_array).is_some_and(|f| !f.is_empty())
        || non_blank(spec.get("author").and_then(|a| a.get("name")))
        || non_blank(spec.get("image"))
}

/// `healEmbed(json)`: a stored embed (EmbedSpec or the legacy shape) as a safe EmbedSpec, or `None` when it has no
/// content. Invalid colours are dropped, strings pre-clamped, bad image URLs removed.
pub fn heal_embed(json: Option<&Value>) -> Option<Value> {
    let json = json.filter(|v| v.is_object())?;
    let mut spec = normalize_legacy_embed(json);
    let obj = spec.as_object_mut()?;
    if obj.get("color").and_then(Value::as_i64).and_then(|c| valid_color(Some(c))).is_none() {
        obj.shift_remove("color");
    }
    if let Some(t) = obj.get("title").and_then(Value::as_str).filter(|s| !s.is_empty()).map(|s| clamp_text(s, 256)) {
        obj.insert("title".into(), json!(t));
    }
    if let Some(t) = obj.get("description").and_then(Value::as_str).filter(|s| !s.is_empty()).map(|s| clamp_text(s, 4096)) {
        obj.insert("description".into(), json!(t));
    }
    if let Some(footer) = obj.get("footer").cloned() {
        match footer.get("text").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            Some(t) => {
                let mut f = footer.as_object().cloned().unwrap_or_default();
                f.insert("text".into(), json!(clamp_text(t, 2048)));
                obj.insert("footer".into(), Value::Object(f));
            }
            None => {
                obj.shift_remove("footer");
            }
        }
    }
    if let Some(author) = obj.get("author").cloned() {
        match author.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            Some(n) => {
                let mut a = author.as_object().cloned().unwrap_or_default();
                a.insert("name".into(), json!(clamp_text(n, 256)));
                obj.insert("author".into(), Value::Object(a));
            }
            None => {
                obj.shift_remove("author");
            }
        }
    }
    if let Some(fields) = obj.get("fields").and_then(Value::as_array).cloned() {
        let list: Vec<Value> = fields
            .iter()
            .take(25)
            .map(|f| {
                json!({
                    "name": clamp_text(f.get("name").and_then(Value::as_str).unwrap_or(""), 256),
                    "value": clamp_text(f.get("value").and_then(Value::as_str).unwrap_or(""), 1024),
                    "inline": f.get("inline").and_then(Value::as_bool).unwrap_or(false),
                })
            })
            .collect();
        if list.is_empty() {
            obj.shift_remove("fields");
        } else {
            obj.insert("fields".into(), Value::Array(list));
        }
    }
    for key in ["thumbnail", "image"] {
        if let Some(v) = obj.get(key)
            && !image_url_ok(v)
        {
            obj.shift_remove(key);
        }
    }
    for key in ["author", "footer"] {
        let bad = obj.get(key).and_then(|a| a.get("iconUrl")).is_some_and(|u| is_truthy_str(u) && !image_url_ok(u));
        if bad && let Some(Value::Object(a)) = obj.get_mut(key) {
            a.insert("iconUrl".into(), Value::Null);
        }
    }
    if has_embed_content(&spec) { Some(spec) } else { None }
}

fn is_truthy_str(v: &Value) -> bool {
    match v {
        Value::String(s) => !s.is_empty(),
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        _ => true,
    }
}

const EMBED_KEYS: [&str; 10] = ["title", "description", "url", "color", "author", "footer", "thumbnail", "image", "fields", "timestamp"];

fn present(v: &Value) -> bool {
    !(v.is_null() || v.as_str() == Some(""))
}

fn copy_present(obj: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    for (k, v) in obj.as_object().into_iter().flatten() {
        if present(v) {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

/// `serializeEmbed(spec)`: the shape written to the database, only the EmbedSpec keys that carry a value (nested
/// objects the same way). The legacy `card` key is never written.
pub fn serialize_embed(spec: &Value) -> Value {
    let mut out = Map::new();
    let Some(obj) = spec.as_object() else { return Value::Object(out) };
    for key in EMBED_KEYS {
        let Some(v) = obj.get(key) else { continue };
        if !present(v) {
            continue;
        }
        match key {
            "author" | "footer" => {
                let nested = copy_present(v);
                if !nested.is_empty() {
                    out.insert(key.into(), Value::Object(nested));
                }
            }
            "fields" => {
                if let Some(a) = v.as_array().filter(|a| !a.is_empty()) {
                    out.insert("fields".into(), Value::Array(a.iter().map(|f| Value::Object(copy_present(f))).collect()));
                }
            }
            _ => {
                out.insert(key.into(), v.clone());
            }
        }
    }
    Value::Object(out)
}

// ── Card style JSON ────────────────────────────────────────────────────────────

/// `parseCardStyle(json)`'s result: every key `None` when absent or invalid.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CardStyle {
    pub enabled: Option<bool>,
    pub background: Option<String>,
    pub background_url: Option<String>,
    pub text_color: Option<String>,
    pub accent_color: Option<String>,
    pub overlay_opacity: Option<f64>,
}

impl CardStyle {
    /// The JS object (`{ enabled, background, backgroundUrl, textColor, accentColor, overlayOpacity }`).
    pub fn to_json(&self) -> Value {
        json!({
            "enabled": self.enabled,
            "background": self.background,
            "backgroundUrl": self.background_url,
            "textColor": self.text_color,
            "accentColor": self.accent_color,
            "overlayOpacity": self.overlay_opacity.map(js_number),
        })
    }

    /// The style the card is drawn with.
    pub fn to_card_style(&self) -> bot_cards::WelcomeCardStyle {
        bot_cards::WelcomeCardStyle {
            background: self.background.clone(),
            background_url: self.background_url.clone(),
            text_color: self.text_color.clone(),
            accent_color: self.accent_color.clone(),
            overlay_opacity: self.overlay_opacity,
        }
    }
}

fn hex_color(v: Option<&Value>) -> Option<String> {
    let s = v?.as_str()?;
    let ok = s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|b| b.is_ascii_hexdigit());
    ok.then(|| s.to_ascii_lowercase())
}

/// A number as `JSON.stringify` writes it (`1`, not `1.0`).
pub fn js_number(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 { json!(n as i64) } else { json!(n) }
}

/// `Math.round(Math.min(1, Math.max(0, x)) * 100) / 100`.
pub fn round_opacity(x: f64) -> f64 {
    js_round(x.clamp(0.0, 1.0) * 100.0) / 100.0
}

/// `parseCardStyle(json)`.
pub fn parse_card_style(json: Option<&Value>) -> CardStyle {
    let mut out = CardStyle::default();
    let Some(obj) = json.and_then(Value::as_object) else { return out };
    out.background = hex_color(obj.get("background"));
    out.text_color = hex_color(obj.get("textColor"));
    out.accent_color = hex_color(obj.get("accentColor"));
    out.background_url = obj.get("backgroundUrl").and_then(Value::as_str).filter(|u| utf16_len(u) <= 2048 && is_http_url(u)).map(String::from);
    if let Some(o) = obj.get("overlayOpacity").and_then(Value::as_f64).filter(|f| f.is_finite()) {
        out.overlay_opacity = Some(round_opacity(o));
    }
    if let Some(b) = obj.get("enabled").and_then(Value::as_bool) {
        out.enabled = Some(b);
    }
    out
}

/// `serializeCardStyle(style)`: the v1 shape written to `GuildSettings.welcomeCardStyle` (every key present).
pub fn serialize_card_style(s: &CardStyle) -> Value {
    json!({
        "v": 1,
        "enabled": s.enabled.unwrap_or(true),
        "background": s.background,
        "backgroundUrl": s.background_url,
        "textColor": s.text_color,
        "accentColor": s.accent_color,
        "overlayOpacity": s.overlay_opacity.map(js_number),
    })
}

/// `welcomeCardEnabled(settings)`: the style's own flag, then a legacy `welcomeEmbed.card` of false, otherwise on.
pub fn welcome_card_enabled(settings: Option<&GuildSettings>) -> bool {
    let Some(s) = settings else { return true };
    if let Some(e) = parse_card_style(s.welcome_card_style.as_ref()).enabled {
        return e;
    }
    match s.welcome_embed.as_ref().and_then(Value::as_object) {
        Some(legacy) => legacy.get("card") != Some(&Value::Bool(false)),
        None => true,
    }
}

/// `materialiseWelcome(cur, data)`: must wrap every mutator result that writes `welcomeEmbed`. It keeps a legacy
/// `card: false` (moved into `welcomeCardStyle`) and freezes the channel text that was effective before the embed
/// changed. Mutates `data`.
pub fn materialise_welcome(cur: &GuildSettings, data: &mut SettingsPatch) {
    let style = parse_card_style(cur.welcome_card_style.as_ref());
    if style.enabled.is_none() && !data.0.contains_key("welcomeCardStyle") {
        let mut s = style;
        s.enabled = Some(welcome_card_enabled(Some(cur)));
        data.insert("welcomeCardStyle", serialize_card_style(&s));
    }
    if cur.welcome_message.is_none() && !data.0.contains_key("welcomeMessage") {
        data.insert("welcomeMessage", resolve_template(Kind::Welcome, Some(cur), false).content.unwrap_or_default());
    }
}

// ── Template resolution (the truth table) ──────────────────────────────────────

/// What a message consists of, before rendering.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub content: Option<String>,
    pub embed: Option<Value>,
}

/// `resolveTemplate(kind, settings, { forceText })`: `''` content becomes `None`; `force_text` (no Embed Links) drops
/// every embed.
pub fn resolve_template(kind: Kind, settings: Option<&GuildSettings>, force_text: bool) -> Resolved {
    let Some(s) = settings else {
        // `settings ?? {}`: every flag undefined.
        return match kind {
            Kind::Dm => Resolved { content: Some(DEFAULT_WELCOME_DM_MESSAGE.into()), embed: None },
            Kind::Goodbye => Resolved { content: Some(DEFAULT_GOODBYE_MESSAGE.into()), embed: None },
            Kind::Welcome if force_text => Resolved { content: Some(DEFAULT_WELCOME_MESSAGE.into()), embed: None },
            Kind::Welcome => Resolved { content: Some(DEFAULT_WELCOME_MESSAGE.into()), embed: Some(default_welcome_embed()) },
        };
    };
    match kind {
        Kind::Dm => Resolved {
            content: non_empty(Some(s.welcome_dm_message.as_deref().unwrap_or(DEFAULT_WELCOME_DM_MESSAGE))),
            embed: if force_text { None } else { heal_embed(s.welcome_dm_embed.as_ref()) },
        },
        Kind::Goodbye => {
            let m = s.goodbye_message.as_deref();
            if !(s.goodbye_use_embed && !force_text) {
                return Resolved { content: non_empty(Some(m.unwrap_or(DEFAULT_GOODBYE_MESSAGE))), embed: None };
            }
            let custom = heal_embed(s.goodbye_embed.as_ref());
            Resolved { content: non_empty(m), embed: Some(custom.unwrap_or_else(default_goodbye_embed)) }
        }
        Kind::Welcome => {
            let m = s.welcome_message.as_deref();
            if !(s.welcome_use_embed && !force_text) {
                return Resolved { content: non_empty(Some(m.unwrap_or(DEFAULT_WELCOME_MESSAGE))), embed: None };
            }
            match heal_embed(s.welcome_embed.as_ref()) {
                // Legacy configs keep sending no text line.
                Some(custom) => Resolved { content: non_empty(m), embed: Some(custom) },
                None => Resolved { content: non_empty(Some(m.unwrap_or(DEFAULT_WELCOME_MESSAGE))), embed: Some(default_welcome_embed()) },
            }
        }
    }
}

// ── Render context ─────────────────────────────────────────────────────────────

/// The placeholder context of a message (`{ member, user, guild, channel }`).
pub fn render_ctx(guild: Option<&Guild>, member: Option<&Member>, user: Option<&User>, channel: Option<&GuildChannel>) -> RenderContext {
    let person = member.map(|m| &m.user).or(user);
    RenderContext {
        member: member.map(|m| placeholder_member(m, guild)),
        user: person.map(placeholder_user),
        guild: guild.map(placeholder_guild),
        channel: channel.map(placeholder_channel),
        vars: None,
        args: None,
    }
}

// ── Payload building ───────────────────────────────────────────────────────────

/// The DM's disabled "Sent from …" button row.
pub fn dm_button_row(guild_name: Option<&str>) -> CreateActionRow {
    let id = bot_config::custom_id!("noop", "welcome-dm").unwrap_or_default();
    CreateActionRow::Buttons(vec![
        CreateButton::new(id)
            .label(clamp_text(&format!("Sent from {}", guild_name.unwrap_or("a server")), 80))
            .style(ButtonStyle::Secondary)
            .disabled(true),
    ])
}

/// What [`build_welcome_payload`] reads.
pub struct PayloadInput<'a> {
    pub settings: Option<&'a GuildSettings>,
    pub guild: Option<&'a Guild>,
    pub member: Option<&'a Member>,
    pub user: Option<&'a User>,
    pub channel: Option<&'a GuildChannel>,
    pub card: Option<Vec<u8>>,
    pub force_text: bool,
    pub preview: bool,
}

/// A built message: the payload to send and the JSON the JS returns (for comparisons).
#[derive(Debug, Clone)]
pub struct WelcomeMessage {
    pub payload: Payload,
    /// `{ content?, embeds, components, allowedMentions, files? }` (files by name).
    pub json: Value,
}

fn mentions_of(json: &Value) -> CreateAllowedMentions {
    let mut m = CreateAllowedMentions::new();
    let users: Vec<UserId> = json["users"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).filter_map(parse_id).map(UserId::new).collect();
    if json.get("users").is_some() {
        m = m.users(users);
    }
    m
}

/// `buildWelcomePayload(kind, opts)`: the message for a join, DM or leave. One render path for real events and for
/// `/… test`. `None` when nothing would be sent.
pub fn build_welcome_payload(kind: Kind, input: PayloadInput<'_>) -> Option<WelcomeMessage> {
    let ctx = render_ctx(input.guild, input.member, input.user, input.channel);
    let user_id = input.member.map(|m| m.user.id).or(input.user.map(|u| u.id));
    let allowed = if input.preview || kind != Kind::Welcome {
        json!({ "parse": [] })
    } else {
        json!({ "parse": [], "users": user_id.map(|u| vec![u.to_string()]).unwrap_or_default() })
    };
    let guild_name = input.guild.map(|g| g.name.as_str());
    let extra: Vec<CreateActionRow> = if kind == Kind::Dm { vec![dm_button_row(guild_name)] } else { Vec::new() };
    let Resolved { content, embed } = resolve_template(kind, input.settings, input.force_text);
    let mut embed = embed;
    let has_card = input.card.is_some();
    if has_card
        && let Some(Value::Object(e)) = embed.as_mut()
        && !e.get("image").is_some_and(is_truthy_str)
    {
        e.insert("image".into(), json!(format!("attachment://{CARD_FILE_NAME}")));
    }
    let configured = valid_color(input.settings.and_then(|s| s.embed_color).map(i64::from));
    let default_color = configured.unwrap_or(if kind == Kind::Goodbye { colors::NEUTRAL } else { colors::SUCCESS });
    let tpl = json!({ "content": content, "embed": embed, "buttons": [] });
    let opts = RenderOptions { default_color: Some(default_color), allowed_mentions: Some(mentions_of(&allowed)), extra_components: extra.clone() };
    let rendered = render_template(&tpl, &ctx, opts);

    let (mut payload, mut out) = match rendered {
        Some(r) => {
            let mut j = r.to_json();
            j["allowedMentions"] = allowed.clone();
            (r.to_payload(), j)
        }
        None if has_card => {
            let comps: Vec<Value> = extra.iter().map(|r| serde_json::to_value(r).unwrap_or(Value::Null)).collect();
            (
                Payload::new().embeds(Vec::new()).components(extra.clone()).allowed_mentions(mentions_of(&allowed)),
                json!({ "embeds": [], "components": comps, "allowedMentions": allowed }),
            )
        }
        None => return None,
    };
    if let Some(card) = input.card {
        payload = payload.file(CreateAttachment::bytes(card, CARD_FILE_NAME));
        out["files"] = json!([CARD_FILE_NAME]);
    }
    let empty = out.get("content").is_none() && out["embeds"].as_array().is_none_or(|e| e.is_empty()) && !has_card;
    if kind == Kind::Dm && empty {
        return None; // a button alone is empty
    }
    Some(WelcomeMessage { payload, json: out })
}

// ── Flood caps ─────────────────────────────────────────────────────────────────

/// `welcome:rate:{guildId}:{kind}`.
pub fn flood_key(guild_id: &str, kind: &str) -> String {
    format!("welcome:rate:{guild_id}:{kind}")
}

/// `underFloodCap(guildId, kind)`: fail-open (a Redis outage never stops a welcome).
pub async fn under_flood_cap(app: &App, guild_id: &str, kind: &str) -> bool {
    let cap = FLOOD_CAPS.iter().find(|(k, _)| *k == kind).map(|(_, n)| *n).unwrap_or(i64::MAX);
    let n = app.cache().sliding_window(&flood_key(guild_id, kind), 60_000, None).await;
    if let Some(n) = n
        && n > cap
    {
        if app.ext::<WelcomeThrottles>().flood.allow(&format!("{guild_id}:{kind}")) {
            bot_util::log_warn!(format!("Welcome {kind} flood cap reached in {guild_id}; skipping"));
        }
        return false;
    }
    true
}

// ── Card orchestration ─────────────────────────────────────────────────────────

/// `member.displayAvatarURL({ extension: 'png', forceStatic: true, size: 256 })`: the server avatar first, always PNG.
pub fn card_avatar_url(guild_id: GuildId, member: &Member) -> String {
    bot_core::adapters::member_static_png_avatar_url(guild_id, member)
}

/// What the card shows for a member of a guild.
pub fn card_data(guild: Option<&Guild>, member: &Member) -> bot_cards::WelcomeCardData {
    bot_cards::WelcomeCardData {
        display_name: Some(bot_core::adapters::member_display_name(member)),
        username: Some(member.user.name.clone()),
        guild_name: guild.map(|g| g.name.clone()),
        member_count: guild.map(|g| g.member_count as f64),
        avatar_url: Some(card_avatar_url(member.guild_id, member)),
    }
}

/// `renderWelcomeCard(member, settings, { preview })`: never fails; `None` means "send without a card". A preview
/// bypasses the flood cap.
pub async fn render_welcome_card(app: &App, member: &Member, settings: Option<&GuildSettings>, preview: bool) -> Option<Vec<u8>> {
    let guild_id = member.guild_id.to_string();
    if !preview && !under_flood_cap(app, &guild_id, "card").await {
        return None; // over the cap: sent without a card
    }
    let guild = cached_guild(app, member.guild_id);
    let data = card_data(guild.as_ref(), member);
    let style = parse_card_style(settings.and_then(|s| s.welcome_card_style.as_ref())).to_card_style();
    let fut = io(app).render_card(data, style);
    match with_timeout(fut, CARD_TIMEOUT_MS, "welcome card").await {
        Ok(png) => Some(png),
        Err(e) => {
            warn_once(app, &format!("{guild_id}:card"), format!("Welcome card could not be rendered in {guild_id}: {e}"));
            None
        }
    }
}

type CardFactory = Box<dyn FnOnce() -> BoxFuture<Option<Vec<u8>>> + Send>;

/// `memoOnce(() => renderWelcomeCard(member, settings))`: one card render shared by the channel message and the DM.
#[derive(Clone)]
pub struct CardMemo {
    cell: Arc<tokio::sync::OnceCell<Option<Vec<u8>>>>,
    factory: Arc<Mutex<Option<CardFactory>>>,
}

impl CardMemo {
    pub fn new(f: impl FnOnce() -> BoxFuture<Option<Vec<u8>>> + Send + 'static) -> Self {
        Self { cell: Arc::new(tokio::sync::OnceCell::new()), factory: Arc::new(Mutex::new(Some(Box::new(f)))) }
    }

    /// A memo that always gives `card` (tests, previews).
    pub fn fixed(card: Option<Vec<u8>>) -> Self {
        Self::new(move || Box::pin(async move { card }))
    }

    /// The card, rendered on the first call only.
    pub async fn get(&self) -> Option<Vec<u8>> {
        let factory = self.factory.clone();
        self.cell
            .get_or_init(|| async move {
                let f = factory.lock().unwrap_or_else(|e| e.into_inner()).take();
                match f {
                    Some(f) => f().await,
                    None => None,
                }
            })
            .await
            .clone()
    }

    /// Whether the card was asked for.
    pub fn was_rendered(&self) -> bool {
        self.cell.initialized()
    }
}

// ── Senders ────────────────────────────────────────────────────────────────────

fn report_send_error(app: &App, err: &IoError, guild_id: &str, what: &str) {
    if let Some(code) = err.code.filter(|c| [10003, 10004, 50001, 50013].contains(c)) {
        warn_once(app, &format!("{guild_id}:{what}:{code}"), format!("{what} could not be sent in {guild_id}: {} ({code})", err.message));
        return;
    }
    bot_util::log_error!(format!("{what} failed in {guild_id}"), bot_util::logger::LogArg::error(err));
}

/// Sends with the documented fallbacks: 50013 with files: once without the card; 50035: once with the default text.
async fn send_with_fallback(
    app: &App,
    channel_id: ChannelId,
    payload: Payload,
    guild_id: &str,
    what: &str,
    without_card: Option<&(dyn Fn() -> Option<Payload> + Sync)>,
    plain: Option<&(dyn Fn() -> Option<Payload> + Sync)>,
) {
    let io = io(app);
    let has_files = !payload.files.is_empty();
    let Err(err) = io.send_message(channel_id, payload).await else { return };
    let mut current = err;
    if current.code == Some(50013)
        && has_files
        && let Some(retry) = without_card.and_then(|f| f())
    {
        match io.send_message(channel_id, retry).await {
            Ok(_) => return,
            Err(e2) => current = e2,
        }
    }
    if current.code == Some(50035)
        && let Some(plain) = plain
    {
        warn_once(
            app,
            &format!("{guild_id}:{what}:50035"),
            format!("{what} had an invalid form body in {guild_id} ({}); sent the default text", current.message),
        );
        if let Some(retry) = plain() {
            match io.send_message(channel_id, retry).await {
                Ok(_) => return,
                Err(e3) => current = e3,
            }
        }
    }
    report_send_error(app, &current, guild_id, what);
}

/// The channel checks shared by the welcome and goodbye senders: the usable channel, or `None` (logged once).
/// `resolveChannel(guild, id)`: the cached channel, else `guild.channels.fetch(id)` over REST (then cached).
async fn resolve_channel(app: &App, guild_id: GuildId, id: &str) -> Option<GuildChannel> {
    let c = parse_id(id)?;
    bot_core::interaction_helpers::resolve_channel(app, guild_id, ChannelId::new(c)).await
}

async fn usable_channel(app: &App, guild_id: GuildId, id: &str, what: &str) -> Option<GuildChannel> {
    let channel = resolve_channel(app, guild_id, id).await;
    let Some(channel) = channel else {
        warn_once(app, &format!("{guild_id}:{what}:missing"), format!("{what} channel {id} is missing in {guild_id}"));
        return None;
    };
    let u = check_channel_usable(app, guild_id, Some(channel.id), &["ViewChannel", "SendMessages"]);
    if !u.ok {
        let why = if u.missing.is_empty() { u.reason.unwrap_or_default() } else { u.missing.join(", ") };
        warn_once(app, &format!("{guild_id}:{what}:perms"), format!("{what} channel {id} is unusable in {guild_id}: {why}"));
        return None;
    }
    Some(channel)
}

fn can(app: &App, guild_id: GuildId, channel_id: ChannelId, perm: &str) -> bool {
    missing_bot_permissions(app, PermTarget::Channel(guild_id, channel_id), &[perm]).is_empty()
}

/// `sendWelcomeChannel(member, settings, getCard)`: posts the welcome message in the welcome channel. Never fails.
pub async fn send_welcome_channel(app: &App, member: &Member, settings: &GuildSettings, get_card: &CardMemo) {
    let Some(channel_id) = settings.welcome_channel_id.as_deref().filter(|c| !c.is_empty()) else { return };
    if !(settings.welcome_enabled && settings.welcome_message_enabled) {
        return;
    }
    let guild_id = member.guild_id;
    let g = guild_id.to_string();
    let Some(channel) = usable_channel(app, guild_id, channel_id, "Welcome").await else { return };
    if !under_flood_cap(app, &g, "msg").await {
        return;
    }
    let can_embed = can(app, guild_id, channel.id, "EmbedLinks");
    let can_attach = can(app, guild_id, channel.id, "AttachFiles");
    // Without Attach Files the card is dropped instead of losing the whole welcome (R229).
    let card = if welcome_card_enabled(Some(settings)) && can_attach { get_card.get().await } else { None };
    let guild = cached_guild(app, guild_id);
    let build = |c: Option<Vec<u8>>| {
        build_welcome_payload(
            Kind::Welcome,
            PayloadInput {
                settings: Some(settings),
                guild: guild.as_ref(),
                member: Some(member),
                user: None,
                channel: Some(&channel),
                card: c,
                force_text: !can_embed,
                preview: false,
            },
        )
        .map(|m| m.payload)
    };
    let Some(payload) = build(card) else { return };
    let without_card = || build(None);
    let plain = || {
        let ctx = render_ctx(guild.as_ref(), Some(member), Some(&member.user), Some(&channel));
        Some(
            Payload::text(render_placeholders(DEFAULT_WELCOME_MESSAGE, &ctx, Some(2000)))
                .allowed_mentions(CreateAllowedMentions::new().users(vec![member.user.id])),
        )
    };
    send_with_fallback(app, channel.id, payload, &g, "Welcome message", Some(&without_card), Some(&plain)).await;
}

/// `sendWelcomeDm(member, settings, getCard)`: bots never get one; closed DMs are skipped silently. Never fails.
pub async fn send_welcome_dm(app: &App, member: &Member, settings: &GuildSettings, get_card: &CardMemo) {
    if !(settings.welcome_enabled && settings.welcome_dm_enabled) || member.user.bot {
        return;
    }
    let guild_id = member.guild_id;
    let g = guild_id.to_string();
    if !under_flood_cap(app, &g, "dm").await {
        return;
    }
    let card = if settings.welcome_dm_card { get_card.get().await } else { None };
    let guild = cached_guild(app, guild_id);
    let input = |s: &GuildSettings, card: Option<Vec<u8>>| {
        build_welcome_payload(
            Kind::Dm,
            PayloadInput { settings: Some(s), guild: guild.as_ref(), member: Some(member), user: None, channel: None, card, force_text: false, preview: false },
        )
    };
    let Some(msg) = input(settings, card) else { return };
    let io = io(app);
    let Err(err) = io.send_dm(member.user.id, msg.payload).await else { return };
    match err.code {
        Some(50007) => bot_util::log_debug!(format!("Welcome DM skipped for {}: DMs are closed", member.user.id)),
        Some(50035) => {
            let mut plain_settings = settings.clone();
            plain_settings.welcome_dm_message = None;
            plain_settings.welcome_dm_embed = None;
            if let Some(plain) = input(&plain_settings, None)
                && let Err(e) = io.send_dm(member.user.id, plain.payload).await
            {
                warn_once(app, &format!("{g}:dm"), format!("Welcome DM failed in {g}: {}", e.message));
            }
        }
        code => {
            let c = code.map(|c| c.to_string()).unwrap_or_else(|| "error".into());
            warn_once(app, &format!("{g}:dm"), format!("Welcome DM failed in {g}: {} ({c})", err.message));
        }
    }
}

/// `sendGoodbye(member, user, settings, guild)`: posts the goodbye for a member who left (kicks and bans included).
/// `member` is `None` when it was not cached (partial). Never fails.
pub async fn send_goodbye(app: &App, guild_id: GuildId, member: Option<&Member>, user: &User, settings: &GuildSettings) {
    if !(settings.welcome_enabled && settings.goodbye_enabled) {
        return;
    }
    let Some(channel_id) = settings.goodbye_channel_id.as_deref().filter(|c| !c.is_empty()) else { return };
    let g = guild_id.to_string();
    let Some(channel) = usable_channel(app, guild_id, channel_id, "Goodbye").await else { return };
    if !under_flood_cap(app, &g, "bye").await {
        return;
    }
    let can_embed = can(app, guild_id, channel.id, "EmbedLinks");
    let guild = cached_guild(app, guild_id);
    let user = member.map(|m| &m.user).unwrap_or(user);
    let Some(msg) = build_welcome_payload(
        Kind::Goodbye,
        PayloadInput { settings: Some(settings), guild: guild.as_ref(), member, user: Some(user), channel: Some(&channel), card: None, force_text: !can_embed, preview: false },
    ) else {
        return;
    };
    let plain = || {
        let ctx = render_ctx(guild.as_ref(), member, Some(user), Some(&channel));
        Some(Payload::text(render_placeholders(DEFAULT_GOODBYE_MESSAGE, &ctx, Some(2000))).no_mentions())
    };
    send_with_fallback(app, channel.id, msg.payload, &g, "Goodbye message", None, Some(&plain)).await;
}

// ── Autoroles: pure helpers ────────────────────────────────────────────────────

const DELAY_ZERO_WORDS: [&str; 7] = ["0", "none", "off", "no", "immediate", "immediately", "instant"];

/// `parseDelayInput(input)`: '0' / 'none' = immediately, a bare number = seconds, otherwise a duration. The seconds,
/// or the error text.
pub fn parse_delay_input(input: &str) -> Result<i64, String> {
    let s = lower(trim(input));
    if DELAY_ZERO_WORDS.contains(&s.as_str()) {
        return Ok(0);
    }
    let seconds = if (1..=7).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit()) {
        s.parse::<i64>().unwrap_or(0)
    } else {
        match bot_util::parse_duration::parse_duration(&s) {
            Some(ms) => js_round(ms as f64 / 1000.0) as i64,
            None => return Err("Use a duration like 30s, 10m, 2h or 1d (0 = immediately).".into()),
        }
    };
    if seconds > MAX_AUTOROLE_DELAY_SECONDS {
        return Err("The longest autorole delay is 7 days.".into());
    }
    Ok(seconds)
}

/// `delayText(seconds)`: "as soon as they join" or "after **10 minutes** (…)".
pub fn delay_text(seconds: i64) -> String {
    let s = seconds.clamp(0, MAX_AUTOROLE_DELAY_SECONDS);
    if s == 0 {
        "as soon as they join".into()
    } else {
        format!(
            "after **{}** (the delay applies to every autorole and counts from when they joined)",
            bot_util::parse_duration::format_duration_long((s * 1000) as f64)
        )
    }
}

/// `resolveAutoroles(guild, settings)`: the configured autoroles split into roles that exist and ids that no longer
/// do, in order (duplicates once).
pub fn resolve_autoroles(guild: Option<&Guild>, settings: Option<&GuildSettings>) -> (Vec<Role>, Vec<String>) {
    let mut roles = Vec::new();
    let mut missing = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for id in settings.map(|s| s.autorole_ids.as_slice()).unwrap_or_default() {
        if !seen.insert(id.clone()) {
            continue;
        }
        match guild.and_then(|g| parse_id(id).and_then(|r| g.roles.get(&RoleId::new(r)))) {
            Some(r) => roles.push(r.clone()),
            None => missing.push(id.clone()),
        }
    }
    (roles, missing)
}

/// `isCaptchaGated(member, settings)`: the captcha gate is on, its role exists in the guild and the member does not
/// have it yet. A deleted captcha role never gates; bots are never gated.
pub fn is_captcha_gated(guild: Option<&Guild>, member: &Member, settings: Option<&GuildSettings>) -> bool {
    if member.user.bot {
        return false;
    }
    let Some(s) = settings else { return false };
    let Some(role) = s.captcha_role_id.as_deref().and_then(parse_id).map(RoleId::new) else { return false };
    s.captcha_enabled && guild.is_some_and(|g| g.roles.contains_key(&role)) && !member.roles.contains(&role)
}

/// The member's join time in ms.
pub fn joined_ms(member: &Member) -> Option<i64> {
    member.joined_at.map(|t| t.timestamp_millis())
}

/// `autoroleDueAt(member, settings, now)`: the join time plus the delay (clamped to 0..7 days).
pub fn autorole_due_at(joined: Option<i64>, settings: Option<&GuildSettings>, now: i64) -> i64 {
    let delay = clamp_int(settings.map(|s| f64::from(s.autorole_delay)).unwrap_or(0.0), 0, MAX_AUTOROLE_DELAY_SECONDS);
    joined.unwrap_or(now) + delay * 1000
}

/// What [`is_screening_completion`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screening {
    /// The old member was cached and was pending.
    Cached,
    /// The old member was not cached; the member is no longer pending and joined recently enough that a watch may
    /// exist.
    Maybe,
}

/// `isScreeningCompletion({ oldMember, newMember, oldKnown, now })`. `None` is not a completion.
pub fn is_screening_completion(old_pending: Option<bool>, new_pending: Option<bool>, old_known: bool, new_joined: Option<i64>, now: i64) -> Option<Screening> {
    if new_pending != Some(false) {
        return None;
    }
    if old_known {
        return (old_pending == Some(true)).then_some(Screening::Cached);
    }
    let joined = new_joined.filter(|j| *j != 0)?;
    if now - joined > SCREENING_WATCH_MS {
        return None;
    }
    Some(Screening::Maybe)
}

fn is_snowflake(s: &str) -> bool {
    (17..=20).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

/// `resolveAutoroleInput(guild, settings, input)`: the autorole an input names among the configured ones (an id, a
/// role mention, or a role name).
pub fn resolve_autorole_input(guild: Option<&Guild>, settings: Option<&GuildSettings>, input: &str) -> Option<String> {
    let ids = settings.map(|s| s.autorole_ids.clone()).unwrap_or_default();
    let raw = trim(input);
    if is_snowflake(raw) {
        return ids.contains(&raw.to_string()).then(|| raw.to_string());
    }
    if let Some(inner) = raw.strip_prefix("<@&").and_then(|r| r.strip_suffix('>'))
        && is_snowflake(inner)
    {
        return ids.contains(&inner.to_string()).then(|| inner.to_string());
    }
    let name = lower(raw.strip_prefix('@').unwrap_or(raw));
    if name.is_empty() {
        return None;
    }
    ids.into_iter().find(|id| {
        guild.and_then(|g| parse_id(id).and_then(|r| g.roles.get(&RoleId::new(r)))).is_some_and(|r| lower(&r.name) == name)
    })
}

// ── Autoroles: granting and scheduling ─────────────────────────────────────────

/// One role that could not be given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedRole {
    pub role_id: String,
    pub reason: String,
}

/// What [`grant_autoroles`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Granted {
    pub added: Vec<String>,
    pub failed: Vec<FailedRole>,
}

/// `grantAutoroles(member, roles, { rethrowTransient })`: gives the roles one at a time; held roles are skipped.
/// `rethrow_transient` lets unexpected errors reach the scheduler's retry.
pub async fn grant_autoroles(app: &App, member: &Member, roles: &[Role], rethrow_transient: bool) -> Result<Granted, IoError> {
    let mut out = Granted::default();
    let guild_id = member.guild_id;
    let io = io(app);
    for role in roles {
        if member.roles.contains(&role.id) {
            continue;
        }
        let chk = check_assignable_role_cached(app, guild_id, role.id, None, false);
        if !chk.ok {
            let reason = chk.reason.unwrap_or_default();
            warn_once(app, &format!("{guild_id}:autorole:{}", role.id), format!("Autorole {} skipped in {guild_id}: {reason}", role.id));
            out.failed.push(FailedRole { role_id: role.id.to_string(), reason });
            continue;
        }
        match io.add_role(guild_id, member.user.id, role.id, Some("Welcome autorole".into())).await {
            Ok(()) => out.added.push(role.id.to_string()),
            Err(err) => {
                let code = err.code;
                if code == Some(10007) {
                    break; // the member left
                }
                if matches!(code, Some(10011 | 50013 | 50001)) {
                    warn_once(
                        app,
                        &format!("{guild_id}:autorole:{}", role.id),
                        format!("Autorole {} could not be given in {guild_id}: {} ({})", role.id, err.message, code.unwrap_or(0)),
                    );
                    out.failed.push(FailedRole { role_id: role.id.to_string(), reason: err.message });
                    continue;
                }
                if rethrow_transient {
                    return Err(err);
                }
                bot_util::log_warn!(format!("Autorole {} failed for {} in {guild_id}: {}", role.id, member.user.id, err.message));
                out.failed.push(FailedRole { role_id: role.id.to_string(), reason: err.message });
            }
        }
    }
    Ok(out)
}

/// `scheduleOrGrant(member, settings, opts)`'s options.
#[derive(Debug, Clone, Copy, Default)]
pub struct GrantOptions {
    /// Skips only the captcha gate.
    pub force: bool,
    pub captcha_pending: bool,
    pub cancel_existing: bool,
}

/// `scheduleOrGrant(member, settings, opts)`. Fails on a database failure. The gates are evaluated at schedule time,
/// so a delayed-grant job needs no flags.
pub async fn schedule_or_grant(app: &App, member: &Member, settings: &GuildSettings, opts: GrantOptions) -> anyhow::Result<()> {
    let guild_id = member.guild_id;
    let g = guild_id.to_string();
    let guild = cached_guild(app, guild_id);
    let (roles, _) = resolve_autoroles(guild.as_ref(), Some(settings));
    if roles.is_empty() {
        return Ok(());
    }
    let user = member.user.id.to_string();
    let key = autorole_job_key(&g, &user);
    let now = now_ms(app);

    // Membership Screening not done: a durable watch job grants once it passes. `force` never skips this gate.
    if member.pending {
        app.scheduler()
            .schedule_job(
                ScheduleJob::new(g.clone(), JobType::WelcomeAutorole, ms_to_utc(now + SCREENING_POLL_MS))
                    .payload(json!({ "userId": user, "watch": true }))
                    .dedupe_key(key)
                    .on_conflict(OnConflict::Keep),
            )
            .await?;
        return Ok(());
    }
    if !opts.force && (opts.captcha_pending || is_captcha_gated(guild.as_ref(), member, Some(settings))) {
        return Ok(());
    }
    let due = autorole_due_at(joined_ms(member), Some(settings), now);
    if due <= now + 5_000 {
        let _ = grant_autoroles(app, member, &roles, false).await;
        if opts.cancel_existing {
            // Drops a screening watch or a stale delayed job.
            app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(key), ..Default::default() }).await;
        }
        return Ok(());
    }
    app.scheduler()
        .schedule_job(
            ScheduleJob::new(g, JobType::WelcomeAutorole, ms_to_utc(due))
                .payload(json!({ "userId": user }))
                .dedupe_key(key)
                .on_conflict(OnConflict::Replace),
        )
        .await?;
    Ok(())
}

/// `assignAutorole(member, { force })`: the contract for the captcha package. Gives (or schedules) the autoroles.
/// `force` skips only the captcha gate; a member still pending Rules Screening is never given roles here. Never
/// fails.
pub async fn assign_autorole(app: &App, member: &Member, force: bool) {
    let g = member.guild_id.to_string();
    let Some(settings) = app.settings().get_settings_safe(&g).await else { return };
    let opts = GrantOptions { force, captcha_pending: false, cancel_existing: true };
    if let Err(e) = schedule_or_grant(app, member, &settings, opts).await {
        bot_util::log_error!(format!("assignAutorole failed for {} in {g}", member.user.id), bot_core::error::log_arg(&e));
    }
}

// ── Hooks and job body ─────────────────────────────────────────────────────────

/// `guildMemberAdd`. Autoroles first and isolated, so message rendering can never delay or skip them; the module
/// flag is checked here because the hook itself is not gated.
pub async fn on_member_join(ctx: Arc<HookCtx<GuildMemberAdd>>) -> bot_core::BotResult {
    let Some(settings) = ctx.get_settings().await else { return Ok(()) };
    let member = &ctx.data.member;
    let app = &ctx.app;
    if !settings.autorole_ids.is_empty() {
        let pending = ctx.state.get::<bool>(CAPTCHA_PENDING_KEY) == Some(true);
        let opts = GrantOptions { force: false, captcha_pending: pending, cancel_existing: false };
        if let Err(e) = schedule_or_grant(app, member, &settings, opts).await {
            bot_util::log_error!(format!("Autorole failed for {} in {}", member.user.id, ctx.data.guild_id), bot_core::error::log_arg(&e));
        }
    }
    if !settings.welcome_enabled {
        return Ok(());
    }
    let (a, m, s) = (app.clone(), member.clone(), settings.clone());
    let get_card = CardMemo::new(move || Box::pin(async move { render_welcome_card(&a, &m, Some(&s), false).await }));
    tokio::join!(send_welcome_channel(app, member, &settings, &get_card), send_welcome_dm(app, member, &settings, &get_card));
    Ok(())
}

/// `guildMemberRemove`. Cancels pending autorole jobs always; posts the goodbye when enabled.
pub async fn on_member_leave(ctx: Arc<HookCtx<GuildMemberRemove>>) -> bot_core::BotResult {
    let Some(settings) = ctx.get_settings().await else { return Ok(()) };
    let g = ctx.data.guild_id.to_string();
    if !settings.autorole_ids.is_empty() {
        ctx.app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(autorole_job_key(&g, &ctx.data.user.id.to_string())), ..Default::default() }).await;
    }
    if settings.welcome_enabled && settings.goodbye_enabled {
        send_goodbye(&ctx.app, ctx.data.guild_id, ctx.data.member.as_ref(), &ctx.data.user, &settings).await;
    }
    Ok(())
}

/// The screening-completion test of a `guildMemberUpdate` payload.
pub fn update_completion(app: &App, data: &GuildMemberUpdate) -> Option<Screening> {
    is_screening_completion(data.old_member.as_ref().map(|m| m.pending), Some(data.member.pending), data.old_known, joined_ms(&data.member), now_ms(app))
}

/// `guildMemberUpdate`: Membership Screening completed (cached transition, or an uncached old member).
pub async fn on_member_update(ctx: Arc<HookCtx<GuildMemberUpdate>>) -> bot_core::BotResult {
    let Some(kind) = update_completion(&ctx.app, &ctx.data) else { return Ok(()) };
    let Some(settings) = ctx.get_settings().await else { return Ok(()) };
    if settings.autorole_ids.is_empty() {
        return Ok(());
    }
    let member = &ctx.data.member;
    let g = ctx.data.guild_id.to_string();
    if kind == Screening::Maybe {
        // Uncached old member: only act when we were watching this join, so roles a moderator removed are never
        // re-added on unrelated updates.
        let job = ctx.app.scheduler().get_job(&autorole_job_key(&g, &member.user.id.to_string())).await;
        if !job.is_some_and(|j| j.payload.get("watch") == Some(&Value::Bool(true))) {
            return Ok(());
        }
    }
    let opts = GrantOptions { force: false, captcha_pending: false, cancel_existing: true };
    if let Err(e) = schedule_or_grant(&ctx.app, member, &settings, opts).await {
        bot_util::log_error!(format!("Screening autorole failed for {} in {g}", member.user.id), bot_core::error::log_arg(&e));
    }
    Ok(())
}

/// `roleDelete`: prunes the deleted role from the autorole list (autoroles are not module-gated, neither is this).
pub async fn on_role_delete(ctx: Arc<HookCtx<RoleDelete>>) -> bot_core::BotResult {
    prune_deleted_role(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.role_id.to_string(), ctx.get_settings().await.as_deref()).await
}

/// The body of [`on_role_delete`].
pub async fn prune_deleted_role(app: &App, guild_id: &str, role_id: &str, settings: Option<&GuildSettings>) -> bot_core::BotResult {
    if !settings.is_some_and(|s| s.autorole_ids.iter().any(|r| r == role_id)) {
        return Ok(());
    }
    let role = role_id.to_string();
    app.settings()
        .update_settings_with(guild_id, move |cur| {
            if !cur.autorole_ids.contains(&role) {
                return Ok(SettingsPatch::new());
            }
            let keep: Vec<String> = cur.autorole_ids.into_iter().filter(|id| *id != role).collect();
            Ok(SettingsPatch::new().set("autoroleIds", json!(keep)))
        })
        .await?;
    Ok(())
}

/// The `welcome.autorole` payload: a delayed grant (`{ userId }`) or a screening watch (`{ userId, watch: true }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutorolePayload {
    pub user_id: String,
    pub watch: bool,
}

/// Reads a `welcome.autorole` payload; `None` when it is invalid.
pub fn parse_autorole_payload(payload: &Value) -> Option<AutorolePayload> {
    let user_id = payload.get("userId").and_then(Value::as_str).filter(|u| is_snowflake(u))?;
    Some(AutorolePayload { user_id: user_id.to_string(), watch: payload.get("watch").and_then(Value::as_bool).unwrap_or(false) })
}

/// `runAutoroleJob(job, { guild })`: a delayed grant or a screening watch. Re-reads the current autorole list when it
/// fires. Idempotent. The captcha gate is re-checked only in the screening-watch branch.
pub async fn run_autorole_job(job: Job, ctx: JobCtx) -> bot_core::BotResult<JobOutcome> {
    let Some(p) = parse_autorole_payload(&job.payload) else {
        return Err(PermanentJobError::new("welcome.autorole: invalid payload").into());
    };
    let app = &ctx.app;
    let settings = app.settings().get_settings(&ctx.guild_id).await?; // a DB failure: the scheduler retries
    let guild_id = GuildId::new(parse_id(&ctx.guild_id).ok_or_else(|| PermanentJobError::new("welcome.autorole: invalid guild"))?);
    let guild = cached_guild(app, guild_id);
    let (roles, _) = resolve_autoroles(guild.as_ref(), Some(&settings));
    if roles.is_empty() {
        return Ok(JobOutcome::Done);
    }
    let user = UserId::new(parse_id(&p.user_id).unwrap_or(1));
    let member = match io(app).fetch_member(guild_id, user).await {
        Ok(m) => m,
        Err(e) if matches!(e.code, Some(10007 | 10013)) => return Ok(JobOutcome::Done), // the member left: done
        Err(e) => return Err(e.into()),
    };
    let now = now_ms(app);
    if p.watch {
        if member.pending {
            if now - joined_ms(&member).unwrap_or(0) >= SCREENING_WATCH_MS {
                return Ok(JobOutcome::Done); // give up after 7 days
            }
            return Ok(JobOutcome::Reschedule { next_run_at: ms_to_utc(now + SCREENING_POLL_MS), payload: None });
        }
        // Screening completed while no event reached us (the bot was offline): evaluate now.
        if is_captcha_gated(guild.as_ref(), &member, Some(&settings)) {
            return Ok(JobOutcome::Done);
        }
        let due = autorole_due_at(joined_ms(&member), Some(&settings), now);
        if due > now + 5_000 {
            return Ok(JobOutcome::Reschedule { next_run_at: ms_to_utc(due), payload: Some(json!({ "userId": p.user_id })) });
        }
    }
    grant_autoroles(app, &member, &roles, true).await?; // permission failures do not fail (no retry)
    Ok(JobOutcome::Done)
}

// ── Diagnostics ────────────────────────────────────────────────────────────────

const MODULE_OFF_LINE: &str = "⚠️ The Welcome & Goodbye module is off (`/welcome enable`).";

/// `USER_TOKEN_RE` (`/\{user(?:\.mention)?\}/i`).
pub fn has_user_token(text: &str) -> bool {
    let l = text.to_lowercase();
    l.contains("{user}") || l.contains("{user.mention}")
}

fn channel_problems(app: &App, guild_id: GuildId, id: Option<&str>, label: &str, needs: &[&str], set_command: &str) -> Vec<String> {
    let Some(id) = id.filter(|s| !s.is_empty()) else { return vec![format!("⚠️ No {label} channel is set (`{set_command}`).")] };
    let channel = parse_id(id).and_then(|c| cached_channel(app, guild_id, ChannelId::new(c)));
    let Some(channel) = channel else {
        return vec![format!("⚠️ The {label} channel <#{id}> was not found. Pick another with `{set_command}`.")];
    };
    let u = check_channel_usable(app, guild_id, Some(channel.id), needs);
    if u.ok {
        return Vec::new();
    }
    let line = if u.missing.is_empty() {
        format!("{}.", u.reason.unwrap_or_default())
    } else {
        format!("I need **{}** in <#{id}>.", permission_names(&u.missing).join(", "))
    };
    vec![format!("⚠️ {line}")]
}

/// `welcomeChannelNeeds(settings)`: the permissions the welcome channel needs for the current settings.
pub fn welcome_channel_needs(settings: Option<&GuildSettings>) -> Vec<&'static str> {
    let mut needs = vec!["ViewChannel", "SendMessages"];
    if settings.is_none_or(|s| s.welcome_use_embed) {
        needs.push("EmbedLinks");
    }
    if welcome_card_enabled(settings) {
        needs.push("AttachFiles");
    }
    needs
}

/// What [`diagnose_welcome`] checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Diagnose {
    Welcome,
    Dm,
    Goodbye,
    Autoroles,
}

/// `diagnoseWelcome(guild, settings, kind)`: short lines about what stops a feature from working. Lines start with a
/// warning sign; the one tip starts with a light bulb. Reads the guild from the cache.
pub fn diagnose_welcome(app: &App, guild_id: GuildId, settings: &GuildSettings, kind: Diagnose) -> Vec<String> {
    let s = settings;
    let mut out = Vec::new();
    if kind != Diagnose::Autoroles && !s.welcome_enabled {
        out.push(MODULE_OFF_LINE.to_string());
    }
    match kind {
        Diagnose::Welcome => {
            if !s.welcome_message_enabled {
                out.push("⚠️ The channel welcome message is turned off (`/welcome message enabled:true`).".into());
            }
            out.extend(channel_problems(app, guild_id, s.welcome_channel_id.as_deref(), "welcome", &welcome_channel_needs(Some(s)), "/welcome channel"));
            let r = resolve_template(Kind::Welcome, Some(s), false);
            if r.content.is_none() && r.embed.is_none() && !welcome_card_enabled(Some(s)) {
                out.push("⚠️ Nothing would be sent: there is no text, embed or card.".into());
            }
            if !has_user_token(r.content.as_deref().unwrap_or("")) {
                out.push("💡 Add `{user}` to the text so new members get pinged.".into());
            }
        }
        Diagnose::Dm => {
            if !s.welcome_dm_enabled {
                out.push("⚠️ The welcome DM is off (`/welcome dm enable`).".into());
            }
            let r = resolve_template(Kind::Dm, Some(s), false);
            if r.content.is_none() && r.embed.is_none() && !s.welcome_dm_card {
                out.push("⚠️ Nothing would be sent: the DM has no text, embed or card.".into());
            }
        }
        Diagnose::Goodbye => {
            if !s.goodbye_enabled {
                out.push("⚠️ The goodbye message is off (`/welcome goodbye enable`).".into());
            }
            let mut needs = vec!["ViewChannel", "SendMessages"];
            if s.goodbye_use_embed {
                needs.push("EmbedLinks");
            }
            out.extend(channel_problems(app, guild_id, s.goodbye_channel_id.as_deref(), "goodbye", &needs, "/welcome goodbye channel"));
        }
        Diagnose::Autoroles => {
            let guild = cached_guild(app, guild_id);
            let (roles, missing) = resolve_autoroles(guild.as_ref(), Some(s));
            if !missing.is_empty() {
                let n = missing.len();
                let list: Vec<String> = missing.iter().map(|id| format!("`{id}`")).collect();
                out.push(format!(
                    "⚠️ {n} autorole{} no longer exist{} and will be skipped: {}.",
                    if n == 1 { "" } else { "s" },
                    if n == 1 { "s" } else { "" },
                    list.join(", ")
                ));
            }
            for role in &roles {
                let chk = check_assignable_role_cached(app, guild_id, role.id, None, false);
                if !chk.ok {
                    out.push(format!("⚠️ <@&{}>: {}", role.id, chk.reason.unwrap_or_default()));
                }
            }
            if !s.welcome_enabled && !s.autorole_ids.is_empty() {
                out.push("⚠️ Autoroles run even while the module is off.".into());
            }
        }
    }
    out
}

/// `unknownTokenWarning(text, ctx)`: "Unknown variables: `{usr}`" for tokens nothing knows; `""` when all are fine.
pub fn unknown_token_warning(text: &str, ctx: &RenderContext) -> String {
    let r = render_placeholders_detailed(text, ctx, None);
    if r.unknown_tokens.is_empty() {
        return String::new();
    }
    let items: Vec<String> = r.unknown_tokens.iter().map(|t| format!("`{t}`")).collect();
    format!("Unknown variables: {}", join_with_limit(&items, ", ", 500, bot_util::text::default_more))
}

/// `autoroleNotes(guild, settings)`: the captcha gate and Membership Screening hold roles back.
pub fn autorole_notes(guild_features: &[String], settings: Option<&GuildSettings>) -> Vec<String> {
    let mut notes = Vec::new();
    if settings.is_some_and(|s| s.captcha_enabled) {
        notes.push("Members get autoroles after they pass verification.".into());
    }
    if guild_features.iter().any(|f| f == "MEMBER_VERIFICATION_GATE_ENABLED") {
        notes.push("Members get autoroles after they accept the server rules.".into());
    }
    notes
}

/// The features of a cached guild.
pub fn guild_features(app: &App, guild_id: GuildId) -> Vec<String> {
    cached_guild(app, guild_id).map(|g| g.features.clone()).unwrap_or_default()
}

// ── Settings changes used by the commands ──────────────────────────────────────

fn config_err(e: WelcomeConfigError) -> anyhow::Error {
    anyhow::Error::new(e)
}

/// `updateMessage(guildId, kind, { text, reset, enabled })`: edits one of the three texts and, for the welcome
/// text, its sub-toggle. `None` when there was nothing to change; a [`WelcomeConfigError`] when refused.
pub async fn update_message(app: &App, guild_id: &str, kind: Kind, text: Option<&str>, reset: bool, enabled: Option<bool>) -> anyhow::Result<Option<Settings>> {
    if text.is_some() && reset {
        return Err(config_err(WelcomeConfigError::new("Use either text or reset, not both.")));
    }
    let mut data = SettingsPatch::new();
    if let Some(t) = text {
        let v = validate_template(&json!({ "content": t }), false);
        if !v.ok {
            return Err(config_err(WelcomeConfigError::new(join_with_limit(&v.errors, "\n", 1000, bot_util::text::default_more))));
        }
        data.insert(kind.message_field(), t);
    } else if reset {
        data.insert(kind.message_field(), Value::Null);
    }
    if let Some(e) = enabled
        && kind == Kind::Welcome
    {
        data.insert("welcomeMessageEnabled", e);
    }
    if data.is_empty() {
        return Ok(None);
    }
    Ok(Some(save_settings(app, guild_id, data).await?))
}

const PLACEHOLDER_IMAGES: [&str; 4] = ["{user.avatar}", "{user.avatar_url}", "{server.icon}", "{server.icon_url}"];

/// `THUMBNAILS[choice]`.
fn thumbnail_value(choice: &str) -> Option<Value> {
    match choice {
        "avatar" => Some(json!("{user.avatar}")),
        "server" => Some(json!("{server.icon}")),
        "none" => Some(Value::Null),
        _ => None,
    }
}

/// `/welcome embed` and `/welcome goodbye embed` options (`image_url` is welcome-only).
#[derive(Debug, Clone, Default)]
pub struct EmbedOptions {
    pub enabled: Option<bool>,
    pub thumbnail: Option<String>,
    pub image_url: Option<String>,
    pub reset: bool,
}

/// `updateEmbedOptions(guildId, kind, opts)`: the new settings, or a [`WelcomeConfigError`].
pub async fn update_embed_options(app: &App, guild_id: &str, kind: Kind, opts: EmbedOptions) -> anyhow::Result<Settings> {
    let image_given = opts.image_url.is_some() && kind == Kind::Welcome;
    if opts.enabled.is_none() && opts.thumbnail.is_none() && !image_given && !opts.reset {
        return Err(config_err(WelcomeConfigError::new("Choose at least one option.")));
    }
    if opts.reset && (opts.thumbnail.is_some() || image_given) {
        return Err(config_err(WelcomeConfigError::new("Use reset on its own.")));
    }
    let mut image = Value::Null;
    if image_given {
        let raw = trim(opts.image_url.as_deref().unwrap_or("")).to_string();
        if lower(&raw) == "none" {
            image = Value::Null;
        } else if is_http_url(&raw) || PLACEHOLDER_IMAGES.contains(&raw.as_str()) {
            image = json!(raw);
        } else {
            return Err(config_err(WelcomeConfigError::new("The image must be an http(s) URL, {user.avatar}, {server.icon} or none.")));
        }
    }
    let embed_field = kind.embed_field();
    let use_field = if kind == Kind::Welcome { "welcomeUseEmbed" } else { "goodbyeUseEmbed" };
    save_settings_with(app, guild_id, move |cur| {
        let mut data = SettingsPatch::new();
        if opts.reset {
            data.insert(embed_field, Value::Null);
        } else if opts.thumbnail.is_some() || image_given {
            let stored = if kind == Kind::Welcome { cur.welcome_embed.as_ref() } else { cur.goodbye_embed.as_ref() };
            let mut base = heal_embed(stored).unwrap_or_else(|| if kind == Kind::Welcome { default_welcome_embed() } else { default_goodbye_embed() });
            if let Some(t) = opts.thumbnail.as_deref() {
                match thumbnail_value(t) {
                    Some(v) => base["thumbnail"] = v,
                    None => {
                        if let Some(o) = base.as_object_mut() {
                            o.shift_remove("thumbnail");
                        }
                    }
                }
            }
            if image_given {
                base["image"] = image;
            }
            data.insert(embed_field, serialize_embed(&base));
        }
        if let Some(e) = opts.enabled {
            data.insert(use_field, e);
        }
        // Only a welcome embed write needs the card flag preserved and the text frozen.
        if kind == Kind::Welcome && data.0.contains_key(embed_field) {
            materialise_welcome(&cur, &mut data);
        }
        Ok(data)
    })
    .await
}

/// `colorOption(label, value)`: `'#rrggbb'`, or a [`WelcomeConfigError`] naming the option.
pub fn color_option(label: &str, value: &str) -> Result<String, WelcomeConfigError> {
    match parse_color_str(value) {
        Some(n) => Ok(format_color(&json!(n)).to_lowercase()),
        None => Err(WelcomeConfigError::new(format!("{label} is not a colour. Use a hex code like #5865F2 or a name like blurple."))),
    }
}

/// `/welcome card` options.
#[derive(Debug, Clone, Default)]
pub struct CardStyleOptions {
    pub enabled: Option<bool>,
    pub background: Option<String>,
    pub background_url: Option<String>,
    pub text_color: Option<String>,
    pub accent_color: Option<String>,
    pub overlay: Option<f64>,
    pub reset: bool,
}

impl CardStyleOptions {
    /// `reset || any option given`.
    pub fn given(&self) -> bool {
        self.reset
            || self.enabled.is_some()
            || self.background.is_some()
            || self.background_url.is_some()
            || self.text_color.is_some()
            || self.accent_color.is_some()
            || self.overlay.is_some()
    }
}

/// `updateCardStyle(guildId, opts)`: validates everything first (the background image is downloaded once), then
/// writes the style. Colour options become `'#rrggbb'`.
pub async fn update_card_style(app: &App, guild_id: &str, opts: CardStyleOptions) -> anyhow::Result<Settings> {
    #[derive(Default)]
    struct Patch {
        background: Option<String>,
        text_color: Option<String>,
        accent_color: Option<String>,
        overlay: Option<f64>,
        background_url: Option<Option<String>>,
    }
    let mut patch = Patch::default();
    if let Some(b) = opts.background.as_deref() {
        patch.background = Some(color_option("Background", b).map_err(config_err)?);
    }
    if let Some(t) = opts.text_color.as_deref() {
        patch.text_color = Some(color_option("Text colour", t).map_err(config_err)?);
    }
    if let Some(a) = opts.accent_color.as_deref() {
        patch.accent_color = Some(color_option("Accent colour", a).map_err(config_err)?);
    }
    if let Some(o) = opts.overlay {
        patch.overlay = Some(round_opacity(o));
    }
    if let Some(u) = opts.background_url.as_deref() {
        let url = trim(u).to_string();
        if lower(&url) == "none" {
            patch.background_url = Some(None);
        } else {
            match io(app).validate_background(url.clone()).await {
                bot_cards::welcome_card::BackgroundCheck::Ok => patch.background_url = Some(Some(url)),
                bot_cards::welcome_card::BackgroundCheck::Invalid { reason } => {
                    return Err(config_err(WelcomeConfigError::titled(reason, "Image Not Loaded")));
                }
            }
        }
    }
    let (reset, enabled) = (opts.reset, opts.enabled);
    save_settings_with(app, guild_id, move |cur| {
        let mut next = if reset { CardStyle::default() } else { parse_card_style(cur.welcome_card_style.as_ref()) };
        next.enabled = Some(welcome_card_enabled(Some(&cur)));
        if let Some(b) = patch.background {
            next.background = Some(b);
        }
        if let Some(u) = patch.background_url {
            next.background_url = u;
        }
        if let Some(t) = patch.text_color {
            next.text_color = Some(t);
        }
        if let Some(a) = patch.accent_color {
            next.accent_color = Some(a);
        }
        if let Some(o) = patch.overlay {
            next.overlay_opacity = Some(o);
        }
        if let Some(e) = enabled {
            next.enabled = Some(e); // applied last
        }
        Ok(SettingsPatch::new().set("welcomeCardStyle", serialize_card_style(&next)))
    })
    .await
}

fn js_num(n: f64) -> String {
    bot_util::js::number_to_string(n)
}

/// `describeCardStyle(style)`: one line for status and replies.
pub fn describe_card_style(s: &CardStyle) -> String {
    let set = s.background.is_some() || s.background_url.is_some() || s.text_color.is_some() || s.accent_color.is_some() || s.overlay_opacity.is_some();
    if !set {
        return "Default".into();
    }
    format!(
        "background {}, image {}, text {}, accent {}, overlay {}",
        s.background.as_deref().unwrap_or("default"),
        if s.background_url.is_some() { "yes" } else { "no" },
        s.text_color.as_deref().unwrap_or("default"),
        s.accent_color.as_deref().unwrap_or("default"),
        js_num(s.overlay_opacity.unwrap_or(0.5))
    )
}

/// discord.js `guild.roles._add(resolvedRole)`: a role Discord resolved in a command option goes into the cache when
/// the cache lacks it, so later cache lookups (`guild.roles.cache.get(id)`) find it, as in the JS.
pub fn adopt_resolved_role(app: &App, guild_id: GuildId, role: Option<&Role>) {
    if let Some(r) = role
        && cached_guild(app, guild_id).is_some_and(|g| !g.roles.contains_key(&r.id))
    {
        bot_core::discord::cache_created_role(app, guild_id, r);
    }
}

/// `addAutorole({ guild, executor, role, delay })`: the new settings and whether the role was already an autorole
/// (then only the delay changed). The delay is written only when given (R074).
pub async fn add_autorole(app: &App, guild_id: GuildId, executor: Option<&Member>, role_id: RoleId, delay: Option<&str>) -> anyhow::Result<(Settings, bool)> {
    add_autorole_resolved(app, guild_id, executor, role_id, None, delay).await
}

/// `guild.roles.cache.get(picked.id) ?? picked` checked with `checkAssignableRole`: the cached role first, else the
/// role Discord resolved in the command option.
fn check_autorole(app: &App, guild_id: GuildId, role_id: RoleId, resolved: Option<&Role>, executor: Option<&Member>) -> bot_core::interaction_helpers::RoleCheck {
    if let (Some(picked), Some(d)) = (resolved, app.discord())
        && let Some(guild) = cached_guild(app, guild_id)
    {
        let bot = d.cache.current_user().id;
        let role = guild.roles.get(&role_id).cloned().unwrap_or_else(|| picked.clone());
        return bot_core::interaction_helpers::check_assignable_role(&guild, Some(&role), bot, executor, false);
    }
    check_assignable_role_cached(app, guild_id, role_id, executor, false)
}

/// [`add_autorole`] with the role Discord resolved in the command option (used when the role is not cached yet).
pub async fn add_autorole_resolved(
    app: &App,
    guild_id: GuildId,
    executor: Option<&Member>,
    role_id: RoleId,
    resolved: Option<&Role>,
    delay: Option<&str>,
) -> anyhow::Result<(Settings, bool)> {
    let chk = check_autorole(app, guild_id, role_id, resolved, executor);
    if !chk.ok {
        let reason = chk.reason.unwrap_or_default();
        let hint = if !chk.dangerous.is_empty() {
            " Autoroles are given to everyone who joins, so roles with these permissions are refused."
        } else if reason.to_lowercase().contains("highest role") {
            " Move my highest role above it in **Server Settings → Roles**."
        } else {
            ""
        };
        return Err(config_err(WelcomeConfigError::titled(format!("{reason}{hint}"), "Cannot Use This Role")));
    }
    let seconds = match delay {
        Some(d) => Some(parse_delay_input(d).map_err(|e| config_err(WelcomeConfigError::new(e)))?),
        None => None,
    };
    let already = Arc::new(AtomicBool::new(false));
    let flag = already.clone();
    let role = role_id.to_string();
    let settings = save_settings_with(app, &guild_id.to_string(), move |cur| {
        if cur.autorole_ids.contains(&role) {
            let Some(s) = seconds else {
                return Err(config_err(WelcomeConfigError::titled(format!("<@&{role}> is already an autorole."), "Already Added")));
            };
            flag.store(true, Ordering::SeqCst);
            return Ok(SettingsPatch::new().set("autoroleDelay", s));
        }
        if cur.autorole_ids.len() >= limits::AUTOROLES {
            return Err(config_err(WelcomeConfigError::new(format!("You can have up to {} autoroles.", limits::AUTOROLES))));
        }
        let mut ids = cur.autorole_ids.clone();
        ids.push(role);
        let mut p = SettingsPatch::new().set("autoroleIds", json!(ids));
        if let Some(s) = seconds {
            p.insert("autoroleDelay", s);
        }
        Ok(p)
    })
    .await?;
    Ok((settings, already.load(Ordering::SeqCst)))
}

/// `removeAutorole(guild, settings, input)`: removes an autorole (deleted roles included). `(None, None)` when the
/// input is not an autorole.
pub async fn remove_autorole(app: &App, guild_id: GuildId, settings: &GuildSettings, input: &str) -> anyhow::Result<(Option<Settings>, Option<String>)> {
    let guild = cached_guild(app, guild_id);
    let Some(id) = resolve_autorole_input(guild.as_ref(), Some(settings), input) else { return Ok((None, None)) };
    let rid = id.clone();
    let after = save_settings_with(app, &guild_id.to_string(), move |cur| {
        if !cur.autorole_ids.contains(&rid) {
            return Ok(SettingsPatch::new());
        }
        let keep: Vec<String> = cur.autorole_ids.into_iter().filter(|x| *x != rid).collect();
        Ok(SettingsPatch::new().set("autoroleIds", json!(keep)))
    })
    .await?;
    Ok((Some(after), Some(id)))
}

/// `setAutoroleDelay(guildId, input)`: the new settings and the seconds, or a [`WelcomeConfigError`].
pub async fn set_autorole_delay(app: &App, guild_id: &str, input: &str) -> anyhow::Result<(Settings, i64)> {
    let seconds = parse_delay_input(input).map_err(|e| config_err(WelcomeConfigError::new(e)))?;
    let s = save_settings(app, guild_id, SettingsPatch::new().set("autoroleDelay", seconds)).await?;
    Ok((s, seconds))
}

/// `pruneMissingAutoroles(guild, missingIds)`: removes autorole ids whose roles no longer exist (R230). Returns how
/// many were pruned. `existing` is the guild's current role ids.
pub async fn prune_missing_autoroles(app: &App, guild_id: &str, missing_ids: &[String], existing: Vec<String>) -> anyhow::Result<usize> {
    if missing_ids.is_empty() {
        return Ok(0);
    }
    let gone: std::collections::HashSet<String> = missing_ids.iter().cloned().collect();
    app.settings()
        .update_settings_with(guild_id, move |cur| {
            let keep: Vec<String> = cur.autorole_ids.iter().filter(|id| !gone.contains(*id) || existing.contains(id)).cloned().collect();
            if keep.len() == cur.autorole_ids.len() {
                return Ok(SettingsPatch::new());
            }
            Ok(SettingsPatch::new().set("autoroleIds", json!(keep)))
        })
        .await?;
    Ok(missing_ids.len())
}

// ── Status ─────────────────────────────────────────────────────────────────────

fn one_line(text: &str) -> String {
    let mut out = String::new();
    let mut space = false;
    for c in text.chars() {
        if is_js_space(c) {
            if !space {
                out.push(' ');
            }
            space = true;
        } else {
            out.push(c);
            space = false;
        }
    }
    trim(&out).to_string()
}

fn quote(text: Option<&str>) -> String {
    match text.filter(|t| !t.is_empty()) {
        Some(t) => format!("\"{}\"", clamp_text(&one_line(t), 300)),
        None => "*(no text)*".into(),
    }
}

/// `describeMode(kind, settings)`: the mode line of a message, e.g. "Text + embed (custom)".
pub fn describe_mode(kind: Kind, s: &GuildSettings) -> String {
    match kind {
        Kind::Welcome => {
            if !s.welcome_use_embed {
                "Text only".into()
            } else {
                format!("Text + embed ({})", if heal_embed(s.welcome_embed.as_ref()).is_some() { "custom" } else { "default" })
            }
        }
        Kind::Goodbye => {
            if s.goodbye_use_embed {
                format!("Embed ({})", if heal_embed(s.goodbye_embed.as_ref()).is_some() { "custom" } else { "default" })
            } else {
                "Text only".into()
            }
        }
        Kind::Dm => {
            if heal_embed(s.welcome_dm_embed.as_ref()).is_some() {
                "Text + embed".into()
            } else {
                "Text".into()
            }
        }
    }
}

fn on_off(b: bool) -> &'static str {
    if b { "On" } else { "Off" }
}

fn status_field(name: &str, value: &str) -> (String, String, bool) {
    let v = clamp_text(value, 1024);
    (clamp_text(name, 256), if v.is_empty() { "—".into() } else { v }, false)
}

/// The fields of the `/welcome status` embed (`(name, value, inline)`).
pub fn status_fields(app: &App, guild_id: GuildId, s: &GuildSettings) -> Vec<(String, String, bool)> {
    let mut fields = Vec::new();
    fields.push(status_field("Module", if s.welcome_enabled { "✅ On" } else { "❌ Off — `/welcome enable`" }));
    let welcome = resolve_template(Kind::Welcome, Some(s), false);
    fields.push(status_field(
        "Channel message",
        &[
            format!("{} · {}", on_off(s.welcome_message_enabled), channel_label(s.welcome_channel_id.as_deref())),
            describe_mode(Kind::Welcome, s),
            format!("Card {}", on_off(welcome_card_enabled(Some(s)))),
            quote(welcome.content.as_deref()),
        ]
        .join("\n"),
    ));
    let dm = resolve_template(Kind::Dm, Some(s), false);
    fields.push(status_field(
        "Welcome DM",
        &[
            format!("{} · Card {}", on_off(s.welcome_dm_enabled), on_off(s.welcome_dm_card)),
            format!("{}{}", quote(dm.content.as_deref()), if dm.embed.is_some() { " + embed" } else { "" }),
        ]
        .join("\n"),
    ));
    let bye = resolve_template(Kind::Goodbye, Some(s), false);
    fields.push(status_field(
        "Goodbye",
        &[
            format!("{} · {}", on_off(s.goodbye_enabled), channel_label(s.goodbye_channel_id.as_deref())),
            describe_mode(Kind::Goodbye, s),
            quote(bye.content.as_deref()),
        ]
        .join("\n"),
    ));
    fields.push(status_field("Card style", &describe_card_style(&parse_card_style(s.welcome_card_style.as_ref()))));
    let ids = &s.autorole_ids;
    let delay = clamp_int(f64::from(s.autorole_delay), 0, MAX_AUTOROLE_DELAY_SECONDS);
    let autoroles = if ids.is_empty() {
        "None".to_string()
    } else {
        let mentions: Vec<String> = ids.iter().map(|id| format!("<@&{id}>")).collect();
        let when = if delay == 0 {
            "immediately".to_string()
        } else {
            format!("{} after joining", bot_util::parse_duration::format_duration_long((delay * 1000) as f64))
        };
        format!("{}\nDelay: {when}", join_with_limit(&mentions, ", ", 900, bot_util::text::default_more))
    };
    fields.push(status_field("Autoroles", &autoroles));
    let mut problems: Vec<String> = Vec::new();
    for k in [Diagnose::Welcome, Diagnose::Dm, Diagnose::Goodbye, Diagnose::Autoroles] {
        for line in diagnose_welcome(app, guild_id, s, k) {
            if !problems.contains(&line) {
                problems.push(line);
            }
        }
    }
    if !problems.is_empty() {
        fields.push(status_field("Problems", &join_with_limit(&problems, "\n", 1024, bot_util::text::default_more)));
    }
    fields
}

/// `buildStatusEmbed(guild, settings)`: the `/welcome status` embed.
pub fn build_status_embed(app: &App, guild_id: GuildId, s: &GuildSettings) -> CreateEmbed {
    let mut embed = apply_guild_color(info_embed("Welcome & Goodbye", None, None), Some(s));
    for (name, value, inline) in status_fields(app, guild_id, s) {
        embed = embed.field(name, value, inline);
    }
    embed
}

// ── Setup modals ───────────────────────────────────────────────────────────────

/// `setupBase(kind, settings)`: the template the setup form starts from (what is actually sent).
pub fn setup_base(kind: Kind, s: Option<&GuildSettings>) -> Value {
    match kind {
        Kind::Dm => json!({
            "content": s.and_then(|s| s.welcome_dm_message.clone()).unwrap_or_else(|| DEFAULT_WELCOME_DM_MESSAGE.into()),
            "embed": s.and_then(|s| heal_embed(s.welcome_dm_embed.as_ref())),
            "buttons": [],
        }),
        Kind::Goodbye => json!({
            "content": resolve_template(Kind::Goodbye, s, false).content.unwrap_or_default(),
            "embed": s.and_then(|s| heal_embed(s.goodbye_embed.as_ref())).unwrap_or_else(default_goodbye_embed),
            "buttons": [],
        }),
        Kind::Welcome => json!({
            "content": resolve_template(Kind::Welcome, s, false).content.unwrap_or_default(),
            "embed": s.and_then(|s| heal_embed(s.welcome_embed.as_ref())).unwrap_or_else(default_welcome_embed),
            "buttons": [],
        }),
    }
}

/// `buildSetupModal(kind, settings)`: the setup form, prefilled from what is actually sent. The builder clamps every
/// prefill and drops an invalid colour, so Discord never rejects the form.
pub fn build_setup_modal(kind: Kind, s: Option<&GuildSettings>) -> CreateModal {
    build_template_modal(kind.modal_id(), Some(kind.modal_title()), &setup_base(kind, s), false)
        .unwrap_or_else(|_| CreateModal::new(kind.modal_id(), kind.modal_title()))
}

/// The setup form as the JSON Discord receives (tests compare it with the JS).
pub fn setup_modal_json(kind: Kind, s: Option<&GuildSettings>) -> Value {
    template_modal_json(kind.modal_id(), Some(kind.modal_title()), &setup_base(kind, s), false).unwrap_or(Value::Null)
}

/// `isStaleSetupForm(interaction)`: true when the submitted form lacks any of today's input ids (a form opened before
/// the upgrade). `present(id)` tells whether the form has that input.
pub fn is_stale_setup_form(present: impl Fn(&str) -> bool) -> bool {
    !bot_core::custom_embed::TEMPLATE_FIELD_IDS.iter().all(|(_, id)| present(id))
}

async fn edit_embed<T: std::ops::Deref<Target = bot_core::Responder>>(ctx: &T, embed: CreateEmbed) {
    let _ = ctx.edit_reply(Payload::embed(embed).to_edit()).await;
}

/// `handleSetupModalSubmit(kind, interaction)`: the submit handler of the three setup forms. Replies publicly
/// (admin convention).
pub async fn handle_setup_modal_submit(kind: Kind, ctx: ModalCtx) -> bot_core::BotResult {
    ctx.defer_reply(false).await?;
    let Some(guild_id) = ctx.guild_id() else { return Ok(()) };
    let g = guild_id.to_string();
    let settings = match ctx.app.settings().get_settings(&g).await {
        Ok(s) => s,
        Err(e) => {
            bot_util::log_warn!(format!("Setup form could not load settings for {g}: {e}"));
            edit_embed(&ctx, error_embed("Temporarily Unavailable", Some("Settings could not be loaded. Nothing was saved."), None)).await;
            return Ok(());
        }
    };
    let access = ctx.member_access();
    let level = bot_core::permission_checker::resolve_permission_level(access.as_ref(), Some(&settings), ctx.app.config().bot_owner_id.as_deref());
    if level < PermissionLevel::Admin {
        edit_embed(&ctx, error_embed("Insufficient Permissions", Some("You need the Admin level to change these settings."), None)).await;
        return Ok(());
    }
    // Mandatory: a form opened before the upgrade carries the retired ids and must never parse as "everything empty".
    if is_stale_setup_form(|id| ctx.value(id).is_some()) {
        edit_embed(&ctx, error_embed("Form Expired", Some("This form is out of date. Run the command again."), None)).await;
        return Ok(());
    }
    // An emptied form is allowed (the warnings below explain what is sent), so the empty-message check is off.
    let (template, errors) = parse_template_modal(|id| ctx.value(id), &setup_base(kind, Some(&settings)), false);
    if !errors.is_empty() {
        edit_embed(&ctx, error_embed("Not Saved", Some(&join_with_limit(&errors, "\n", 4000, bot_util::text::default_more)), None)).await;
        return Ok(());
    }
    let embed = template.get("embed").cloned().unwrap_or(Value::Null);
    let embed_json = if has_embed_content(&embed) { serialize_embed(&embed) } else { Value::Null };
    let content = template.get("content").and_then(Value::as_str).unwrap_or("").to_string();
    let after = match kind {
        Kind::Welcome => {
            let (c, e) = (content.clone(), embed_json.clone());
            save_settings_with(&ctx.app, &g, move |cur| {
                let mut data = SettingsPatch::new().set("welcomeMessage", c).set("welcomeEmbed", e);
                materialise_welcome(&cur, &mut data);
                Ok(data)
            })
            .await?
        }
        Kind::Dm => save_settings(&ctx.app, &g, SettingsPatch::new().set("welcomeDmMessage", content.clone()).set("welcomeDmEmbed", embed_json.clone())).await?,
        Kind::Goodbye => save_settings(&ctx.app, &g, SettingsPatch::new().set("goodbyeMessage", content.clone()).set("goodbyeEmbed", embed_json.clone())).await?,
    };

    let mut warnings: Vec<String> = Vec::new();
    let guild = cached_guild(&ctx.app, guild_id);
    let channel = cached_channel(&ctx.app, guild_id, ctx.interaction.channel_id);
    let rctx = render_ctx(guild.as_ref(), ctx.member(), Some(ctx.user()), channel.as_ref());
    let parts: Vec<&str> = [
        template.get("content").and_then(Value::as_str),
        embed.get("title").and_then(Value::as_str),
        embed.get("description").and_then(Value::as_str),
        embed.get("footer").and_then(|f| f.get("text")).and_then(Value::as_str),
    ]
    .into_iter()
    .flatten()
    .filter(|s| !s.is_empty())
    .collect();
    let unknown = unknown_token_warning(&parts.join("\n"), &rctx);
    if !unknown.is_empty() {
        warnings.push(unknown);
    }
    let has_embed = !embed_json.is_null();
    match kind {
        Kind::Welcome => {
            if !has_embed && after.welcome_use_embed {
                warnings.push("The embed is empty, so the default embed is used. For text only, run `/welcome embed enabled:false`.".into());
            }
            if has_embed && !after.welcome_use_embed {
                warnings.push("Embed saved, but the welcome is in text-only mode (`/welcome embed enabled:true`).".into());
            }
            if !after.welcome_message_enabled {
                warnings.push("The channel welcome message is turned off (`/welcome message enabled:true`).".into());
            }
        }
        Kind::Goodbye => {
            if !has_embed && after.goodbye_use_embed {
                warnings.push("The embed is empty, so the default embed is used. For text only, run `/welcome goodbye embed enabled:false`.".into());
            }
            if !after.goodbye_enabled {
                warnings.push("The goodbye message is off (`/welcome goodbye enable`).".into());
            }
        }
        Kind::Dm => {
            if !after.welcome_dm_enabled {
                warnings.push("The welcome DM is off (`/welcome dm enable`).".into());
            }
        }
    }
    if !after.welcome_enabled {
        warnings.push("The Welcome & Goodbye module is off (`/welcome enable`).".into());
    }
    let resolved = resolve_template(kind, Some(&after), false);
    let card = match kind {
        Kind::Dm => after.welcome_dm_card,
        Kind::Welcome => welcome_card_enabled(Some(&after)),
        Kind::Goodbye => false,
    };
    if resolved.content.is_none() && resolved.embed.is_none() && !card {
        warnings.push("Nothing would be sent with these settings.".into());
    }
    let mut description = format!("Preview it with `{}`.", kind.preview_command());
    if !warnings.is_empty() {
        let lines: Vec<String> = warnings.iter().map(|w| format!("⚠️ {w}")).collect();
        description.push_str(&format!("\n\n{}", lines.join("\n")));
    }
    edit_embed(&ctx, success_embed(kind.submit_title(), Some(&description), None)).await;
    Ok(())
}

// ── Test previews ──────────────────────────────────────────────────────────────

/// `buildTestPreview(interaction, settings, kind)`: the exact message a join or leave would produce, rendered for the
/// invoker, with no pings. Uses the same payload builder as real events.
pub async fn build_test_preview(ctx: &CommandCtx, settings: &GuildSettings, kind: Kind) -> bot_core::BotResult {
    ctx.defer_reply(false).await?;
    let Some(guild_id) = ctx.guild_id() else { return Ok(()) };
    let configured = if kind == Kind::Goodbye { settings.goodbye_channel_id.as_deref() } else { settings.welcome_channel_id.as_deref() };
    let configured = match configured {
        Some(id) => resolve_channel(&ctx.app, guild_id, id).await,
        None => None,
    };
    let channel = configured.or_else(|| cached_channel(&ctx.app, guild_id, ctx.interaction.channel_id));
    let member = ctx.member().cloned();
    let card_on = match kind {
        Kind::Welcome => welcome_card_enabled(Some(settings)),
        Kind::Dm => settings.welcome_dm_card,
        Kind::Goodbye => false,
    };
    let card = match (&member, card_on) {
        (Some(m), true) => render_welcome_card(&ctx.app, m, Some(settings), true).await,
        _ => None,
    };
    let guild = cached_guild(&ctx.app, guild_id);
    let built = build_welcome_payload(
        kind,
        PayloadInput {
            settings: Some(settings),
            guild: guild.as_ref(),
            member: member.as_ref(),
            user: Some(ctx.user()),
            channel: channel.as_ref(),
            card,
            force_text: false,
            preview: true,
        },
    );
    match built {
        None => {
            let w = warn_embed(
                "Nothing To Send",
                Some("With the current settings nothing would be sent. Set a text with the message subcommand or turn on the embed or card."),
                None,
            );
            let _ = ctx.edit_reply(Payload::embed(w).to_edit()).await;
        }
        Some(m) => {
            let _ = ctx.edit_reply(m.payload.to_edit()).await;
        }
    }
    let k = match kind {
        Kind::Welcome => Diagnose::Welcome,
        Kind::Dm => Diagnose::Dm,
        Kind::Goodbye => Diagnose::Goodbye,
    };
    let problems = diagnose_welcome(&ctx.app, guild_id, settings, k);
    if !problems.is_empty() {
        let w = warn_embed("Setup Problems", Some(&join_with_limit(&problems, "\n", 4000, bot_util::text::default_more)), None);
        let _ = ctx.follow_up(Payload::embed(w).to_follow_up()).await;
    }
    Ok(())
}

// ── Command helpers (shared by /welcome and /welcome goodbye) ───────────────────

/// `settingsForAck(interaction)`: the memo first, otherwise a lookup bounded by the remaining acknowledgement
/// budget. `None` = unavailable.
pub async fn settings_for_ack(app: &App, guild_id: &str, budget_ms: u64) -> Option<Settings> {
    if let Some(s) = app.settings().peek_settings(guild_id) {
        return Some(s);
    }
    let (a, g) = (app.clone(), guild_id.to_string());
    let load = async move { Ok::<_, std::convert::Infallible>(a.settings().get_settings_safe(&g).await) };
    with_timeout(load, budget_ms, "welcome setup").await.ok().flatten()
}

/// `checkConfigChannel(guild, picked, needs)`: the bot must be able to use the chosen channel for this feature.
/// `Err(message)` otherwise.
pub async fn check_config_channel(app: &App, guild_id: GuildId, picked: ChannelId, needs: &[&str]) -> Result<(), String> {
    // `guild.channels.cache.get(id) ?? await guild.channels.fetch(id)`: an uncached channel is fetched (and cached).
    let u = bot_core::interaction_helpers::check_channel_usable_fetch(app, guild_id, Some(picked), needs).await;
    if u.ok {
        return Ok(());
    }
    Err(if u.missing.is_empty() {
        format!("{}.", u.reason.unwrap_or_default())
    } else {
        format!("I need **{}** in <#{picked}>. Fix the channel permissions and run this again.", permission_names(&u.missing).join(", "))
    })
}

fn describe_image(v: Option<&Value>) -> String {
    match v.and_then(Value::as_str).filter(|s| !s.is_empty()) {
        None => "none".into(),
        Some("{user.avatar}" | "{user.avatar_url}") => "member avatar".into(),
        Some("{server.icon}" | "{server.icon_url}") => "server icon".into(),
        Some(s) => clamp_text(s, 100),
    }
}

/// `describeEmbedSettings(kind, settings)`: the mode and the images of the welcome or goodbye embed.
pub fn describe_embed_settings(kind: Kind, s: &GuildSettings) -> String {
    let spec = if kind == Kind::Goodbye {
        heal_embed(s.goodbye_embed.as_ref()).unwrap_or_else(default_goodbye_embed)
    } else {
        heal_embed(s.welcome_embed.as_ref()).unwrap_or_else(default_welcome_embed)
    };
    let mut lines = vec![format!("**Mode:** {}", describe_mode(kind, s)), format!("**Thumbnail:** {}", describe_image(spec.get("thumbnail")))];
    if kind == Kind::Welcome {
        lines.push(format!("**Image:** {}", describe_image(spec.get("image"))));
    }
    lines.join("\n")
}

fn code_block(text: &str) -> String {
    format!("```\n{}\n```", clamp_text(&text.replace("```", "'''"), 1000))
}

/// `messageInfoEmbed(title, kind, settings)`: `/… message` without options.
pub fn message_info_embed(title: &str, kind: Kind, s: &GuildSettings) -> CreateEmbed {
    let content = resolve_template(kind, Some(s), false).content;
    let mut lines = Vec::new();
    if kind == Kind::Welcome {
        lines.push(format!("**Channel message:** {}", if s.welcome_message_enabled { "On" } else { "Off" }));
    }
    lines.push(format!("**Mode:** {}", describe_mode(kind, s)));
    lines.push(content.as_deref().map(code_block).unwrap_or_else(|| "*(no text)*".into()));
    info_embed(title, Some(&lines.join("\n")), None)
}

/// `messageUpdateEmbed(title, kind, after, change, ctx)`: what changed, the text now in effect, unknown variables and
/// tips.
pub fn message_update_embed(title: &str, kind: Kind, after: &GuildSettings, text: Option<&str>, reset: bool, enabled: Option<bool>, ctx: &RenderContext) -> CreateEmbed {
    let mut lines = Vec::new();
    if text.is_some() {
        lines.push("The text was saved.".to_string());
    }
    if reset {
        lines.push("The text was reset to the default.".into());
    }
    if let Some(e) = enabled
        && kind == Kind::Welcome
    {
        lines.push(format!("The channel welcome message is now **{}**.", if e { "on" } else { "off" }));
    }
    let content = resolve_template(kind, Some(after), false).content;
    lines.push(content.as_deref().map(code_block).unwrap_or_else(|| "*(no text)*".into()));
    let unknown = text.map(|t| unknown_token_warning(t, ctx)).unwrap_or_default();
    if !unknown.is_empty() {
        lines.push(format!("⚠️ {unknown}"));
    }
    let c = content.as_deref().unwrap_or("");
    if kind == Kind::Welcome && !has_user_token(c) {
        lines.push("💡 Add `{user}` to the text so new members get pinged.".into());
    }
    if kind == Kind::Goodbye && has_user_token(c) {
        lines.push("⚠️ Mentions don't notify members who left; `{user.name}` or `{user.idname}` reads better.".into());
    }
    success_embed(title, Some(&lines.join("\n")), None)
}

/// `cardSummary(settings)`: the card settings as markdown lines.
pub fn card_summary(s: &GuildSettings) -> String {
    let style = parse_card_style(s.welcome_card_style.as_ref());
    [
        format!("**Card:** {}", if welcome_card_enabled(Some(s)) { "On" } else { "Off" }),
        format!("**Background colour:** {}", style.background.as_deref().unwrap_or("default")),
        format!("**Background image:** {}", if style.background_url.is_some() { "set" } else { "none" }),
        format!("**Text colour:** {}", style.text_color.as_deref().unwrap_or("default")),
        format!("**Accent colour:** {}", style.accent_color.as_deref().unwrap_or("default")),
        format!("**Overlay:** {}", style.overlay_opacity.map(js_num).unwrap_or_else(|| "default (0.5 over an image)".into())),
    ]
    .join("\n")
}

/// `withCardPreview(embed, member, settings)`: the embed plus a preview of the card rendered for the invoker
/// (omitted when it fails).
pub async fn with_card_preview(app: &App, embed: CreateEmbed, member: Option<&Member>, settings: &GuildSettings) -> Payload {
    let buf = match member {
        Some(m) => render_welcome_card(app, m, Some(settings), true).await,
        None => None,
    };
    match buf {
        None => Payload::embed(embed),
        Some(png) => Payload::embed(embed.image(format!("attachment://{CARD_FILE_NAME}"))).file(CreateAttachment::bytes(png, CARD_FILE_NAME)),
    }
}

/// `cardChannelWarning(guild, settings)`: warns when the card is on and the welcome channel cannot take attachments.
pub fn card_channel_warning(app: &App, guild_id: GuildId, s: &GuildSettings) -> String {
    let Some(id) = s.welcome_channel_id.as_deref().filter(|c| !c.is_empty()) else { return String::new() };
    if !welcome_card_enabled(Some(s)) {
        return String::new();
    }
    let Some(channel) = parse_id(id).and_then(|c| cached_channel(app, guild_id, ChannelId::new(c))) else { return String::new() };
    if can(app, guild_id, channel.id, "AttachFiles") {
        return String::new();
    }
    format!("⚠️ I cannot attach files in <#{id}>, so the card is left out until I get **Attach Files** there.")
}

/// `autoroleListLines(guild, settings, { prunedCount })`: the delay, one line per role (with a problem when it
/// cannot be given), notes.
pub fn autorole_list_lines(app: &App, guild_id: GuildId, s: &GuildSettings, pruned_count: usize) -> Vec<String> {
    let guild = cached_guild(app, guild_id);
    let (roles, _) = resolve_autoroles(guild.as_ref(), Some(s));
    let seconds = clamp_int(f64::from(s.autorole_delay), 0, MAX_AUTOROLE_DELAY_SECONDS);
    let delay = if seconds == 0 {
        "immediately".to_string()
    } else {
        format!("{} after joining (applies to every autorole)", bot_util::parse_duration::format_duration_long((seconds * 1000) as f64))
    };
    let mut lines = vec![format!("**Delay:** {delay}"), String::new()];
    for role in &roles {
        let chk = check_assignable_role_cached(app, guild_id, role.id, None, false);
        let tail = if chk.ok { String::new() } else { format!(" — ⚠️ {}", chk.reason.unwrap_or_default()) };
        lines.push(format!("<@&{}>{tail}", role.id));
    }
    if pruned_count > 0 {
        lines.push(String::new());
        lines.push(format!("Removed {pruned_count} deleted role{} from the list.", if pruned_count == 1 { "" } else { "s" }));
    }
    let notes = autorole_notes(&guild.map(|g| g.features.clone()).unwrap_or_default(), Some(s));
    if !notes.is_empty() {
        lines.push(String::new());
        lines.extend(notes);
    }
    lines
}
