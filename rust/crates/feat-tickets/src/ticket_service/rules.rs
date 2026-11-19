//! The pure rules of `src/services/ticketService.js`: constants, who may do what, the state machine, numbers and channel
//! names, option emoji, row classification, the rename window, overwrites, templates, Discord error wording and the
//! `ticket.managersync` payload merge. No I/O here.

use std::sync::LazyLock;

use bot_core::custom_embed::normalize_template;
use bot_util::js::{trim, utf16_len, utf16_prefix};
use bot_util::jsre::{JsRegex, u16s};
use bot_util::placeholder_parser::{PlaceholderMember, PlaceholderUser, RenderContext, VarValue, render_placeholders};
use bot_util::text::{clamp_text, clamp_text_with};
use serde_json::{Map, Value, json};
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

// ── Constants ─────────────────────────────────────────────────────────────────

pub const PENDING_PREFIX: &str = "pending:";
pub const PENDING_STALE_MS: f64 = 5.0 * 60_000.0;
pub const MAX_ADDED_USERS: usize = 20;
pub const MAX_TRANSCRIPT_MESSAGES: usize = 5000;
pub const TRANSCRIPT_PAGE_SIZE: u8 = 100;
pub const TRANSCRIPT_DEADLINE_MS: u64 = 90_000;
/// Under Discord's 10 MiB default upload limit.
pub const MAX_TRANSCRIPT_BYTES: usize = 8 * 1024 * 1024;
pub const DELETE_DELAY_MS: u64 = 5_000;
pub const RENAME_WINDOW_MS: f64 = 10.0 * 60_000.0;
pub const RENAME_MAX: usize = 2;
pub const CATEGORY_CHILD_LIMIT: usize = 50;
pub const PANEL_MEMO_TTL_MS: i64 = 60_000;
pub const PANEL_MEMO_MAX: usize = 500;
pub const MANAGER_SYNC_MAX: i64 = 50;
pub const MANAGER_SYNC_DEADLINE_MS: i64 = 60_000;
pub const RECONCILE_FETCH_MAX: usize = 50;
/// Time one run of the manager-sync job works before it hands the rest to a follow-up run.
pub const MANAGER_SYNC_JOB_BUDGET_MS: i64 = 40_000;
pub const RENAME_LOG_MAX: usize = 1000;

/// `DEFAULT_CATEGORY_NAMES` (open, claimed, closed).
pub const DEFAULT_CATEGORY_NAMES: [(&str, &str); 3] = [("open", "Tickets"), ("claimed", "Claimed Tickets"), ("closed", "Closed Tickets")];

/// The default category name of a kind.
pub fn default_category_name(kind: CategoryKind) -> &'static str {
    match kind {
        CategoryKind::Open => "Tickets",
        CategoryKind::Claimed => "Claimed Tickets",
        CategoryKind::Closed => "Closed Tickets",
    }
}

/// `DEFAULT_OPTION`: the option every new panel starts with.
pub const DEFAULT_OPTION_LABEL: &str = "Open Ticket";
pub const DEFAULT_OPTION_EMOJI: &str = "✉️";
pub const DEFAULT_OPTION_BUTTON_STYLE: i32 = 1;

/// `BUTTON_STYLE_NAMES`.
pub fn button_style_name(style: i32) -> Option<&'static str> {
    match style {
        1 => Some("Blue"),
        2 => Some("Grey"),
        3 => Some("Green"),
        4 => Some("Red"),
        _ => None,
    }
}

pub const NAME_FORMAT_TOKENS: [&str; 6] = ["number", "user.name", "user.username", "user.id", "user.display_name", "topic"];

/// Overwrite permission sets (PermissionFlagsBits key names).
pub const MEMBER_ALLOW: [&str; 5] = ["ViewChannel", "SendMessages", "ReadMessageHistory", "AttachFiles", "EmbedLinks"];
pub const STAFF_ALLOW: [&str; 5] = MEMBER_ALLOW;
pub const BOT_ALLOW: [&str; 6] = ["ViewChannel", "SendMessages", "ReadMessageHistory", "AttachFiles", "EmbedLinks", "ManageChannels"];
/// The bot must hold every permission it grants; ManageRoles for later overwrite edits.
pub const OPEN_REQUIRED_PERMS: [&str; 7] = ["ViewChannel", "ManageChannels", "ManageRoles", "SendMessages", "ReadMessageHistory", "EmbedLinks", "AttachFiles"];
pub const CATEGORY_PERMS: [&str; 3] = ["ViewChannel", "ManageChannels", "ManageRoles"];
pub const TRANSCRIPT_POST_PERMS: [&str; 4] = ["ViewChannel", "SendMessages", "EmbedLinks", "AttachFiles"];

/// What the bot needs in the ticket channel for each lifecycle action (`ACTION_PERMS`).
pub fn action_perms(action: Action) -> Option<&'static [&'static str]> {
    Some(match action {
        Action::Claim => &["ViewChannel", "ManageChannels", "SendMessages"],
        Action::Close => &["ViewChannel", "ManageChannels", "ManageRoles", "SendMessages"],
        Action::Reopen => &["ViewChannel", "ManageChannels", "ManageRoles", "SendMessages"],
        Action::Add => &["ViewChannel", "ManageRoles", "SendMessages"],
        Action::Remove => &["ViewChannel", "ManageRoles", "SendMessages"],
        Action::Rename => &["ViewChannel", "ManageChannels"],
        Action::Delete => &["ViewChannel", "ManageChannels"],
        Action::Transcript => return None,
    })
}

/// `DEFAULT_INTRO_TEMPLATE`.
pub fn default_intro_template() -> Value {
    json!({
        "content": "{user}",
        "embed": {
            "title": "Ticket #{number}",
            "description": "Thanks for reaching out, {user.display_name}! A member of our team will be with you shortly.\nPlease describe your issue in as much detail as you can.",
        },
        "buttons": [],
    })
}

// ── Results ───────────────────────────────────────────────────────────────────

/// A refused service call: the JS `{ ok: false, code, data }`.
#[derive(Debug, Clone, PartialEq)]
pub struct Fail {
    pub code: String,
    pub data: Value,
}

impl Fail {
    pub fn new(code: &str) -> Self {
        Self { code: code.to_string(), data: Value::Null }
    }

    pub fn with(code: &str, data: Value) -> Self {
        Self { code: code.to_string(), data }
    }

    /// `d.<key>` as text ('' when absent).
    pub fn str(&self, key: &str) -> String {
        match self.data.get(key) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(v) => v.to_string(),
        }
    }

    /// The JS result object (`{ ok: false, code, data? }`).
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("ok".into(), json!(false));
        m.insert("code".into(), json!(self.code));
        if !self.data.is_null() {
            m.insert("data".into(), self.data.clone());
        }
        Value::Object(m)
    }
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.code)
    }
}

pub type Outcome<T> = Result<T, Fail>;

// ── Who may do what (§6.2) ────────────────────────────────────────────────────

/// A lifecycle action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Claim,
    Close,
    Reopen,
    Delete,
    Add,
    Remove,
    Transcript,
    Rename,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Claim => "claim",
            Action::Close => "close",
            Action::Reopen => "reopen",
            Action::Delete => "delete",
            Action::Add => "add",
            Action::Remove => "remove",
            Action::Transcript => "transcript",
            Action::Rename => "rename",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "claim" => Action::Claim,
            "close" => Action::Close,
            "reopen" => Action::Reopen,
            "delete" => Action::Delete,
            "add" => Action::Add,
            "remove" => Action::Remove,
            "transcript" => Action::Transcript,
            "rename" => Action::Rename,
            _ => return None,
        })
    }
}

/// `canPerform(action, { isOwner, isStaff })`: role-based checks only.
pub fn can_perform(action: Action, is_owner: bool, is_staff: bool) -> Outcome<()> {
    match action {
        Action::Claim => {
            if !is_staff {
                return Err(Fail::new("NOT_STAFF"));
            }
            if is_owner {
                return Err(Fail::new("OWN_TICKET"));
            }
            Ok(())
        }
        Action::Close => {
            if is_owner || is_staff {
                Ok(())
            } else {
                Err(Fail::new("NOT_OWNER_OR_STAFF"))
            }
        }
        _ => {
            if is_staff {
                Ok(())
            } else {
                Err(Fail::new("NOT_STAFF"))
            }
        }
    }
}

/// Where an action came from (`source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Button,
    Command,
}

/// A successful transition: the next status (`None` for actions that keep it) and whether a claim is a takeover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub to: Option<&'static str>,
    pub takeover: bool,
}

/// `transitionFor(action, ticket, { actorId, isAdmin, source })`: the pure state machine. `status` is the stored label.
pub fn transition_for(action: Action, status: &str, claimed_by_id: Option<&str>, actor_id: &str, is_admin: bool, source: Source) -> Outcome<Transition> {
    let claimed = claimed_by_id.filter(|c| !c.is_empty());
    let ok = |to: &'static str| Ok(Transition { to: Some(to), takeover: false });
    match action {
        Action::Claim => {
            if status == "CLOSED" {
                return Err(Fail::new("IS_CLOSED"));
            }
            if status == "CLAIMED"
                && let Some(c) = claimed
            {
                if c == actor_id {
                    return Err(Fail::new("ALREADY_CLAIMED_SELF"));
                }
                if is_admin {
                    return Ok(Transition { to: Some("CLAIMED"), takeover: true });
                }
                return Err(Fail::with("ALREADY_CLAIMED", json!({ "claimedById": c })));
            }
            ok("CLAIMED")
        }
        Action::Close => {
            if status == "CLOSED" {
                Err(Fail::new("ALREADY_CLOSED"))
            } else {
                ok("CLOSED")
            }
        }
        Action::Reopen => {
            if status != "CLOSED" {
                return Err(Fail::new("NOT_CLOSED"));
            }
            ok(if claimed.is_some() { "CLAIMED" } else { "OPEN" })
        }
        Action::Delete => {
            if source == Source::Button && status != "CLOSED" {
                return Err(Fail::new("CLOSE_FIRST"));
            }
            ok("DELETED")
        }
        Action::Add | Action::Remove | Action::Rename | Action::Transcript => Ok(Transition { to: None, takeover: false }),
    }
}

// ── Numbers and channel names (§6.5) ──────────────────────────────────────────

/// `formatTicketNumber(n)`: '0001'; numbers of 10000 and up print unpadded.
pub fn format_ticket_number(n: i64) -> String {
    let s = n.to_string();
    if s.len() >= 4 { s } else { format!("{}{s}", "0".repeat(4 - s.len())) }
}

static WS_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"\s+", "gu"));
static NOT_NAME_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"[^\p{L}\p{N}_-]", "gu"));
static DASHES_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"-{2,}", "g"));
static EDGES_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"^[-_]+|[-_]+$", "g"));
static TOKEN_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"\{([^{}]*)\}", "g"));

/// `sanitizeChannelName(str)`: Discord-style channel name (lower case, letters of any script, digits, - and _);
/// possibly empty.
pub fn sanitize_channel_name(s: &str) -> String {
    let s: String = s.nfkc().collect::<String>().to_lowercase();
    let s = WS_RE.replace_all(&s, "-");
    let s = NOT_NAME_RE.replace_all(&s, "");
    let s = DASHES_RE.replace_all(&s, "-");
    let s = EDGES_RE.replace_all(&s, "");
    clamp_text_with(&s, 100, "")
}

/// `validateNameFormat(fmt)`: the trimmed format, or the error text.
pub fn validate_name_format(fmt: &str) -> Result<String, String> {
    let f = trim(fmt).to_string();
    if f.is_empty() || utf16_len(&f) > 90 {
        return Err("The name format must be 1–90 characters.".into());
    }
    let units = u16s(&f);
    for m in TOKEN_RE.find_all(&units) {
        let inner = m.group(&units, 1).unwrap_or_default();
        let key = trim(&inner).to_lowercase();
        if !NAME_FORMAT_TOKENS.contains(&key.as_str()) {
            return Err(format!("Unknown placeholder `{{{inner}}}`. Use {{number}}, {{user.name}}, {{user.id}}, {{user.display_name}} or {{topic}}."));
        }
    }
    if sanitize_channel_name(&TOKEN_RE.replace_all(&f, "x")).is_empty() {
        return Err("The name format has no usable characters.".into());
    }
    Ok(f)
}

/// `renderChannelName(format, { number, user, member, topic })`: always a usable channel name.
pub fn render_channel_name(format: &str, number: i64, user: Option<PlaceholderUser>, member: Option<PlaceholderMember>, topic: Option<&str>) -> String {
    let ctx = RenderContext {
        user,
        member,
        vars: Some(vec![
            ("number".to_string(), VarValue::Text(format_ticket_number(number))),
            ("topic".to_string(), VarValue::Text(topic.unwrap_or("").to_string())),
        ]),
        ..Default::default()
    };
    let out = render_placeholders(format, &ctx, None);
    let clean = sanitize_channel_name(&out);
    if clean.is_empty() { format!("ticket-{}", format_ticket_number(number)) } else { clean }
}

/// `normalizePanelName(name)`: trimmed, 1-50 characters, one line; `None` when invalid.
pub fn normalize_panel_name(name: &str) -> Option<String> {
    let n = trim(name);
    let len = utf16_len(n);
    if (1..=50).contains(&len) && !n.contains(['\r', '\n']) { Some(n.to_string()) } else { None }
}

// ── Option emoji (§6.6) ───────────────────────────────────────────────────────

static UNICODE_EMOJI_RE: LazyLock<JsRegex> = LazyLock::new(|| {
    JsRegex::new(
        "^(?:\\p{Regional_Indicator}{2}|[#*0-9]\u{FE0F}?\u{20E3}|(?:\\p{Extended_Pictographic}|\\p{Emoji_Presentation})(?:\u{FE0F}|\\p{Emoji_Modifier})?(?:[\\u{E0020}-\\u{E007E}]+\\u{E007F})?(?:\u{200D}(?:\\p{Extended_Pictographic}|\\p{Emoji_Presentation})(?:\u{FE0F}|\\p{Emoji_Modifier})?)*)$",
        "u",
    )
});
static CUSTOM_EMOJI_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"^<(a?):(\w{1,32}):(\d{17,20})>$", ""));
static NAMED_EMOJI_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"^:(\w{2,32}):$", ""));

/// `isUnicodeEmoji(s)`: true when the string is one Unicode emoji.
pub fn is_unicode_emoji(s: &str) -> bool {
    UNICODE_EMOJI_RE.test(s)
}

/// A server emoji, as `parseOptionEmoji` sees `guild.emojis.cache`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuildEmoji {
    pub id: String,
    pub name: String,
    pub animated: bool,
}

impl GuildEmoji {
    /// `emoji.toString()`.
    pub fn mention(&self) -> String {
        format!("<{}:{}:{}>", if self.animated { "a" } else { "" }, self.name, self.id)
    }
}

/// `parseOptionEmoji(input, { client, guild })`. `bot_has_emoji(id)` is `client.emojis.cache.has(id)`; `guild_emojis`
/// is the server's emoji list. `Ok(None)` means no emoji.
pub fn parse_option_emoji(input: &str, bot_has_emoji: &dyn Fn(&str) -> bool, guild_emojis: &[GuildEmoji]) -> Result<Option<String>, String> {
    let s = trim(input);
    if s.is_empty() || s.to_lowercase() == "none" {
        return Ok(None);
    }
    if let Some((t, m)) = CUSTOM_EMOJI_RE.exec(s) {
        let anim = m.group(&t, 1).unwrap_or_default();
        let name = m.group(&t, 2).unwrap_or_default();
        let id = m.group(&t, 3).unwrap_or_default();
        if bot_has_emoji(&id) {
            return Ok(Some(format!("<{anim}:{name}:{id}>")));
        }
        return Err("I can't use that emoji. Use a standard emoji or one from a server I'm in.".into());
    }
    if let Some((t, m)) = NAMED_EMOJI_RE.exec(s) {
        let name = m.group(&t, 1).unwrap_or_default();
        let lower = name.to_lowercase();
        return match guild_emojis.iter().find(|e| e.name.to_lowercase() == lower) {
            Some(e) => Ok(Some(e.mention())),
            None => Err(format!("I can't find a server emoji called :{name}:.")),
        };
    }
    let segments: Vec<&str> = s.graphemes(true).collect();
    if segments.len() == 1 && is_unicode_emoji(segments[0]) {
        return Ok(Some(s.to_string()));
    }
    Err(format!("\"{}\" is not a single emoji.", clamp_text(s, 40)))
}

// ── Row classification and the rename window (§6.7.4, §6.8.5) ─────────────────

/// The fields `classifyTicketRows` reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowRef {
    pub id: String,
    pub channel_id: String,
    pub created_at_ms: i64,
}

/// The result of [`classify_ticket_rows`]: live rows, and the ids of abandoned placeholders and of rows whose channel is
/// gone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Classified {
    pub live: Vec<RowRef>,
    pub stale_pending: Vec<String>,
    pub stale_gone: Vec<String>,
}

/// `classifyTicketRows(rows, hasChannel, now)`. `has_channel` must mean "not confirmed gone".
pub fn classify_ticket_rows(rows: &[RowRef], has_channel: &dyn Fn(&str) -> bool, now_ms: i64) -> Classified {
    let mut out = Classified::default();
    for row in rows {
        if row.channel_id.starts_with(PENDING_PREFIX) {
            if ((now_ms - row.created_at_ms) as f64) < PENDING_STALE_MS {
                out.live.push(row.clone());
            } else {
                out.stale_pending.push(row.id.clone());
            }
        } else if has_channel(&row.channel_id) {
            out.live.push(row.clone());
        } else {
            out.stale_gone.push(row.id.clone());
        }
    }
    out
}

/// `renameWindowCheck(timestamps, now)`: whether a rename is allowed, and when the next one is (ms).
pub fn rename_window_check(timestamps: &[f64], now_ms: f64) -> (bool, Option<f64>) {
    let recent: Vec<f64> = timestamps.iter().copied().filter(|t| now_ms - t < RENAME_WINDOW_MS).collect();
    if recent.len() >= RENAME_MAX {
        let min = recent.iter().copied().fold(f64::INFINITY, f64::min);
        return (false, Some(min + RENAME_WINDOW_MS));
    }
    (true, None)
}

// ── Overwrites, managers and categories (§6.7.3, §6.7.5) ──────────────────────

/// `liveManagerRoleIds(panel, guild)`: manager roles that still exist. `role_exists` is `guild.roles.cache.has`.
pub fn live_manager_role_ids(manager_role_ids: &[String], guild_id: &str, role_exists: &dyn Fn(&str) -> bool) -> Vec<String> {
    manager_role_ids.iter().filter(|id| id.as_str() != guild_id && role_exists(id)).cloned().collect()
}

/// One permission overwrite as the JS builds it: `{ id, type, allow?, deny? }` (type 0 role, 1 member).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverwriteSpec {
    pub id: String,
    pub kind: u8,
    pub allow: Option<Vec<&'static str>>,
    pub deny: Option<Vec<&'static str>>,
}

impl OverwriteSpec {
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("id".into(), json!(self.id));
        m.insert("type".into(), json!(self.kind));
        if let Some(a) = &self.allow {
            m.insert("allow".into(), json!(a));
        }
        if let Some(d) = &self.deny {
            m.insert("deny".into(), json!(d));
        }
        Value::Object(m)
    }
}

/// The inputs of [`build_ticket_overwrites`].
#[derive(Debug, Clone, Default)]
pub struct OverwriteInput<'a> {
    pub guild_id: &'a str,
    pub bot_id: &'a str,
    pub owner_id: &'a str,
    pub manager_role_ids: &'a [String],
    pub added_user_ids: &'a [String],
    pub closed: bool,
}

/// `buildTicketOverwrites(...)`: every entry carries an explicit type; a repeated id keeps its first entry.
pub fn build_ticket_overwrites(i: &OverwriteInput<'_>) -> Vec<OverwriteSpec> {
    let member = |id: &str| OverwriteSpec {
        id: id.to_string(),
        kind: 1,
        allow: Some(if i.closed { vec!["ViewChannel", "ReadMessageHistory"] } else { MEMBER_ALLOW.to_vec() }),
        deny: Some(if i.closed { vec!["SendMessages"] } else { vec![] }),
    };
    let mut all = vec![
        OverwriteSpec { id: i.guild_id.to_string(), kind: 0, allow: None, deny: Some(vec!["ViewChannel"]) },
        OverwriteSpec { id: i.bot_id.to_string(), kind: 1, allow: Some(BOT_ALLOW.to_vec()), deny: None },
        member(i.owner_id),
    ];
    all.extend(i.manager_role_ids.iter().map(|id| OverwriteSpec { id: id.clone(), kind: 0, allow: Some(STAFF_ALLOW.to_vec()), deny: None }));
    all.extend(i.added_user_ids.iter().map(|id| member(id)));
    let mut seen = std::collections::HashSet::new();
    all.into_iter().filter(|o| seen.insert(o.id.clone())).collect()
}

/// A category kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CategoryKind {
    Open,
    Claimed,
    Closed,
}

impl CategoryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CategoryKind::Open => "open",
            CategoryKind::Claimed => "claimed",
            CategoryKind::Closed => "closed",
        }
    }

    /// `CATEGORY_FIELDS[kind]`.
    pub fn field(self) -> &'static str {
        match self {
            CategoryKind::Open => "openCategoryId",
            CategoryKind::Claimed => "claimedCategoryId",
            CategoryKind::Closed => "closedCategoryId",
        }
    }
}

/// `resolveCategoryId(kind, { panel, option })`: the configured category id of a kind.
pub fn resolve_category_id(kind: CategoryKind, panel: Option<&bot_db::TicketPanel>, option: Option<&bot_db::TicketPanelOption>) -> Option<String> {
    match kind {
        CategoryKind::Open => option.and_then(|o| o.category_id.clone()).or_else(|| panel.and_then(|p| p.open_category_id.clone())),
        CategoryKind::Claimed => panel.and_then(|p| p.claimed_category_id.clone()),
        CategoryKind::Closed => panel.and_then(|p| p.closed_category_id.clone()),
    }
}

// ── Templates (§6.4.1) ────────────────────────────────────────────────────────

/// `defaultPanelTemplate(style)`.
pub fn default_panel_template(style_select: bool) -> Value {
    json!({
        "content": null,
        "embed": {
            "title": "How can we help?",
            "description": if style_select {
                "Choose a topic below to open a private ticket with our team."
            } else {
                "Press a button below to open a private ticket with our team."
            },
        },
        "buttons": [],
    })
}

fn escape_regex(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if ".*+?^${}()|[]\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `templateUsesToken(tpl, token)`: the template mentions `{token}` in its content, title, description or footer.
pub fn template_uses_token(tpl: &Value, token: &str) -> bool {
    let t = normalize_template(tpl);
    let re = JsRegex::new(&format!(r"\{{\s*{}\s*\}}", escape_regex(token)), "i");
    let e = &t["embed"];
    [t.get("content"), e.get("title"), e.get("description"), e.get("footer").and_then(|f| f.get("text"))]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|s| re.test(s))
}

/// `MODAL_MAX`.
pub const MODAL_MAX_CONTENT: usize = 2000;
pub const MODAL_MAX_TITLE: usize = 256;
pub const MODAL_MAX_DESCRIPTION: usize = 4000;
pub const MODAL_MAX_FOOTER: usize = 2048;

fn cut_field(v: &mut Value, max: usize) {
    if let Some(s) = v.as_str().filter(|s| !s.is_empty()) {
        *v = Value::String(utf16_prefix(s, max).to_string());
    }
}

/// `clampTemplateForModal(tpl)`: a copy cut to the form's input limits.
pub fn clamp_template_for_modal(tpl: &Value) -> Value {
    let mut t = normalize_template(tpl);
    if let Some(c) = t.get_mut("content") {
        cut_field(c, MODAL_MAX_CONTENT);
    }
    if let Some(e) = t.get_mut("embed").filter(|e| e.is_object()) {
        if let Some(v) = e.get_mut("title") {
            cut_field(v, MODAL_MAX_TITLE);
        }
        if let Some(v) = e.get_mut("description") {
            cut_field(v, MODAL_MAX_DESCRIPTION);
        }
        if let Some(v) = e.get_mut("footer").and_then(|f| f.get_mut("text")) {
            cut_field(v, MODAL_MAX_FOOTER);
        }
    }
    t
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(_) => true,
    }
}

fn has_visible_embed(e: &Value) -> bool {
    if !e.is_object() {
        return false;
    }
    truthy(e.get("title"))
        || truthy(e.get("description"))
        || e.get("fields").and_then(Value::as_array).is_some_and(|f| !f.is_empty())
        || truthy(e.get("author").and_then(|a| a.get("name")))
        || truthy(e.get("footer").and_then(|f| f.get("text")))
        || truthy(e.get("image"))
        || truthy(e.get("thumbnail"))
}

/// `isEmptyTemplate(tpl)`: an empty form resets a template to its default (a colour-only embed counts as empty).
pub fn is_empty_template(tpl: &Value) -> bool {
    let t = normalize_template(tpl);
    let has_content = t.get("content").and_then(Value::as_str).is_some_and(|c| !trim(c).is_empty());
    !has_content && !has_visible_embed(&t["embed"])
}

/// `toJson(tpl)`: the normalised template as stored in a Json column.
pub fn template_to_column(tpl: &Value) -> Value {
    normalize_template(tpl)
}

// ── Discord errors (§6.13) ────────────────────────────────────────────────────

/// A failed Discord call, as the service sees it (`err.code`, `err.status`, the raw error codes, a timeout).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IoError {
    pub code: Option<u64>,
    pub status: Option<u16>,
    pub message: String,
    /// The nested `rawError.errors` codes (`CHANNEL_PARENT_MAX_CHANNELS`, ...).
    pub error_codes: Vec<String>,
    /// A `withTimeout` TimeoutError.
    pub timeout: bool,
    /// A rate limit (`RateLimitError`).
    pub rate_limited: bool,
}

impl IoError {
    pub fn code(code: u64, message: impl Into<String>) -> Self {
        Self { code: Some(code), message: message.into(), ..Default::default() }
    }

    pub fn offline() -> Self {
        Self { message: "Discord is not connected".into(), ..Default::default() }
    }

    pub fn timed_out(label: &str) -> Self {
        Self { message: format!("{label} timed out"), timeout: true, ..Default::default() }
    }

    pub fn is(&self, code: u64) -> bool {
        self.code == Some(code)
    }

    /// Missing access or permissions (50001, 50013).
    pub fn is_perms(&self) -> bool {
        self.is(50013) || self.is(50001)
    }

    /// From a serenity error.
    pub fn of(err: &serenity::Error) -> Self {
        let mut out = Self {
            code: bot_core::discord::error_code(err),
            status: bot_core::discord::error_status(err),
            message: bot_core::discord::error_message(err),
            ..Default::default()
        };
        if let serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(r)) = err {
            out.error_codes = r.error.errors.iter().map(|e| e.code.clone()).collect();
        }
        out
    }
}

impl std::fmt::Display for IoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for IoError {}

/// `isCategoryFullError(err)`: channel creation failed because the parent category holds 50 channels.
pub fn is_category_full_error(err: &IoError) -> bool {
    err.is(50035) && err.error_codes.iter().any(|c| c.contains("CHANNEL_PARENT_MAX_CHANNELS"))
}

/// `describeDiscordError(err)`: a short human reason.
pub fn describe_discord_error(err: &IoError) -> String {
    if err.timeout {
        return "Discord took too long to answer".into();
    }
    if is_category_full_error(err) {
        return "the category is full (50 channels)".into();
    }
    match err.code {
        Some(50013) | Some(50001) => return "I'm missing permissions there".into(),
        Some(10003) => return "the channel no longer exists".into(),
        Some(10008) => return "the message no longer exists".into(),
        Some(30013) => return "the server has reached 500 channels".into(),
        Some(50035) => return "Discord rejected the request".into(),
        Some(50007) => return "the user does not accept DMs".into(),
        _ => {}
    }
    if err.status == Some(429) || err.rate_limited {
        return "Discord is rate limiting me".into();
    }
    match (err.code, err.status) {
        (Some(c), _) => format!("Discord error {c}"),
        (None, Some(s)) => format!("Discord error {s}"),
        _ => "Discord error unknown".into(),
    }
}

// ── Manager back-fill job payload (ticket.managersync) ────────────────────────

const SYNC_ROLES_MAX: usize = 250;
static SYNC_PANEL_ID_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"^[A-Za-z0-9_-]{1,40}$", ""));
static SYNC_ROLE_ID_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"^\d{17,20}$", ""));

/// One panel's entry of a `ticket.managersync` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncEntry {
    pub roles: Vec<String>,
    pub done: i64,
    pub rev: i64,
}

impl SyncEntry {
    pub fn to_json(&self) -> Value {
        json!({ "roles": self.roles, "done": self.done, "rev": self.rev })
    }
}

fn js_integer(v: Option<&Value>) -> Option<i64> {
    let n = v?.as_number()?;
    if let Some(i) = n.as_i64() {
        return Some(i);
    }
    let f = n.as_f64()?;
    (f.is_finite() && f.fract() == 0.0).then_some(f as i64)
}

/// `readManagerSync(payload)`: the valid part of a job payload, in order; a panel with no valid role is dropped.
pub fn read_manager_sync(payload: &Value) -> Vec<(String, SyncEntry)> {
    let mut out: Vec<(String, SyncEntry)> = Vec::new();
    let Some(panels) = payload.get("panels").and_then(Value::as_object) else { return out };
    for (panel_id, entry) in panels {
        let Some(roles) = entry.get("roles").and_then(Value::as_array) else { continue };
        if !SYNC_PANEL_ID_RE.test(panel_id) {
            continue;
        }
        let mut seen: Vec<String> = Vec::new();
        for r in roles.iter().filter_map(Value::as_str) {
            if SYNC_ROLE_ID_RE.test(r) && !seen.iter().any(|s| s == r) {
                seen.push(r.to_string());
            }
        }
        seen.truncate(SYNC_ROLES_MAX);
        if seen.is_empty() {
            continue;
        }
        let done = js_integer(entry.get("done")).filter(|d| *d > 0).unwrap_or(0);
        let rev = js_integer(entry.get("rev")).unwrap_or(0);
        upsert(&mut out, panel_id.clone(), SyncEntry { roles: seen, done, rev });
    }
    out
}

fn upsert(list: &mut Vec<(String, SyncEntry)>, key: String, value: SyncEntry) {
    match list.iter_mut().find(|(k, _)| *k == key) {
        Some(slot) => slot.1 = value,
        None => list.push((key, value)),
    }
}

/// The JSON payload of a parsed list.
pub fn sync_payload_json(list: &[(String, SyncEntry)]) -> Value {
    let mut panels = Map::new();
    for (k, v) in list {
        panels.insert(k.clone(), v.to_json());
    }
    json!({ "panels": panels })
}

/// `mergeManagerSync(existing, incoming)`: roles unioned per panel, the panel starts over (`done` 0), its `rev` counts up.
pub fn merge_manager_sync(existing: &Value, incoming: &Value) -> Value {
    let mut out = read_manager_sync(existing);
    for (panel_id, entry) in read_manager_sync(incoming) {
        let prev = out.iter().find(|(k, _)| *k == panel_id).map(|(_, v)| v.clone());
        let mut roles: Vec<String> = prev.as_ref().map(|p| p.roles.clone()).unwrap_or_default();
        for r in entry.roles {
            if !roles.contains(&r) {
                roles.push(r);
            }
        }
        roles.truncate(SYNC_ROLES_MAX);
        let rev = prev.map(|p| p.rev).unwrap_or(0) + 1;
        upsert(&mut out, panel_id, SyncEntry { roles, done: 0, rev });
    }
    sync_payload_json(&out)
}

/// `managerSyncPayload(panelId, roleIds)`: the payload that queues a back-fill of `roleIds` on one panel.
pub fn manager_sync_payload(panel_id: &str, role_ids: &[String]) -> Value {
    merge_manager_sync(&Value::Null, &json!({ "panels": { panel_id: { "roles": role_ids } } }))
}

/// The `ticket.managersync` dedupe key of a guild.
pub fn manager_sync_dedupe_key(guild_id: &str) -> String {
    format!("ticket.managersync:{guild_id}")
}

/// `embedLength(embed.data)`: title, description, field names and values, footer text and author name (UTF-16 units).
pub fn embed_length(embed: &Value) -> usize {
    let s = |v: Option<&Value>| v.and_then(Value::as_str).map(utf16_len).unwrap_or(0);
    let mut n = s(embed.get("title")) + s(embed.get("description"));
    for f in embed.get("fields").and_then(Value::as_array).into_iter().flatten() {
        n += s(f.get("name")) + s(f.get("value"));
    }
    n + s(embed.get("footer").and_then(|f| f.get("text"))) + s(embed.get("author").and_then(|a| a.get("name")))
}
