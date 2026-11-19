//! Port of `src/services/embedService.js` (pkg-embeds). Saved embeds (`SavedEmbed` rows holding a MessageTemplate),
//! the pure template mutators behind every `/embed` subcommand, rendering, publish / update of the live message, the
//! Welcome Channel presets, the `embed-modal` submit handler, the `messageDelete` hook body and the contracts other
//! crates use:
//!
//! * [`get_saved_embed_template`]`(app, guild_id, name)` -> `Option<Value>` (never fails; raw, placeholders unexpanded)
//! * [`search_saved_embeds`]`(pool, guild_id, query, take)` -> `Vec<SearchRow>` (never fails; empty on error)
//! * [`normalize_embed_name`]`(input)` -> slug or `None`
//!
//! Redis (`guild:{id}:embed:{name}`) is a read accelerator for [`get_saved_embed_template`] only; PostgreSQL is the
//! source of truth. Templates are JSON values in the JS shape, read and written through `bot_core::custom_embed`, so
//! the JS bot and this one accept each other's rows and cache entries.
//!
//! Discord writes (posting, editing, deleting, creating the welcome channel and its invite) go through
//! [`EmbedDiscord`]; the default ([`HttpDiscord`]) uses the app's serenity client and cache, and
//! [`set_discord_ops`] replaces it for one app (tests drive the flows with a fake, as the JS tests do with fake
//! channels).

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bot_core::App;
use bot_core::adapters::{placeholder_channel, placeholder_guild, placeholder_member, placeholder_user, user_tag};
use bot_core::custom_embed::{
    Rendered, RenderOptions, empty_template, is_http_url, normalize_template, parse_template_modal, render_template, spec_from_embed,
    template_to_json, validate_template,
};
use bot_core::embed_builder::{error_embed, success_embed};
use bot_core::interaction::ModalCtx;
use bot_core::interaction_helpers::{
    ChannelUsable, Interactive, Payload, PermTarget, bot_permissions, cached_channel, cached_guild, channel_usable_verdict, is_interaction_gone,
    missing_from, pingable_role_ids,
};
use bot_core::permissions;
use bot_core::privacy_guards::{MEMBER_CHANNEL_NEEDS, MemberRef, member_channel_problem};

use bot_db::{GuildSettings, InsertBuilder, PgPool, SAVED_EMBED_INFO, SavedEmbed, SelectBuilder, UpdateBuilder, delete_where};
use bot_util::js::{lower, trim, utf16_len};
use bot_util::placeholder_parser::{RenderContext, placeholder_help, render_placeholders_detailed};
use bot_util::text::{clamp_text, default_more, join_with_limit};
use serde::Serialize;
use serde_json::{Map, Value, json};
use serenity::all::{
    ChannelId, ChannelType, GuildChannel, GuildId, Member, MessageId, PermissionOverwrite, PermissionOverwriteType, Permissions, RoleId, User, UserId,
};
use serenity::builder::{CreateAllowedMentions, CreateAttachment, CreateChannel, CreateInvite};

// ── Constants ──────────────────────────────────────────────────────────────────

/// `EMBED_NAME_MAX`.
pub const EMBED_NAME_MAX: usize = 32;
/// `EMBED_CACHE_TTL_SECONDS`.
pub const EMBED_CACHE_TTL_SECONDS: u64 = 300;
/// `EMBED_CACHE_MISS_TTL_SECONDS`.
pub const EMBED_CACHE_MISS_TTL_SECONDS: u64 = 60;
/// `IMPORT_MAX_BYTES`.
pub const IMPORT_MAX_BYTES: usize = 20_480;
/// `BUTTON_URL_MAX`.
pub const BUTTON_URL_MAX: usize = 512;
/// `PRESET_NAMES`.
pub const PRESET_NAMES: [&str; 4] = ["welcome-greeting", "welcome-rules", "welcome-links", "welcome-team"];
/// `CLEAR_WORDS`.
pub const CLEAR_WORDS: [&str; 4] = ["none", "clear", "remove", "-"];

const MAX_FIELDS: usize = 25;
const MAX_BUTTONS: usize = 25;
const MAX_ROLE_MENTIONS: usize = 100;
const MAX_UNKNOWN_TOKENS: usize = 10;

/// `GUILD_LIMITS.SAVED_EMBEDS`.
pub const SAVED_EMBEDS_LIMIT: usize = bot_config::limits::SAVED_EMBEDS;

/// The only shared state: serialises every write, publish, update and delete of one embed in this process
/// (`embedLock`, keyed by the row id), one per app.
#[derive(Default)]
pub struct EmbedLock(pub bot_core::sync::KeyedMutex);

fn run_locked<T, F, Fut>(app: &App, id: &str, f: F) -> impl std::future::Future<Output = T> + Send + 'static + use<T, F, Fut>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    app.ext::<EmbedLock>().0.run(id, f)
}

// ── Errors ─────────────────────────────────────────────────────────────────────

/// `EmbedEditError.code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditCode {
    /// `'INVALID'` (default).
    Invalid,
    /// `'NOTFOUND'`.
    NotFound,
    /// `'TAKEN'`.
    Taken,
    /// `'LIMIT'`.
    Limit,
}

impl EditCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "INVALID",
            Self::NotFound => "NOTFOUND",
            Self::Taken => "TAKEN",
            Self::Limit => "LIMIT",
        }
    }
}

/// `EmbedEditError`: a failure whose message is shown to the admin verbatim. A rejected save also carries the
/// rejected `template` and the list of `errors`, so the caller can hand the text back as a draft file.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbedEditError {
    pub message: String,
    pub code: EditCode,
    pub errors: Vec<String>,
    pub template: Option<Box<Value>>,
}

impl std::fmt::Display for EmbedEditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for EmbedEditError {}

impl EmbedEditError {
    /// `new EmbedEditError(message)` (code INVALID).
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), code: EditCode::Invalid, errors: Vec::new(), template: None }
    }

    /// `new EmbedEditError(message, code)`.
    pub fn with_code(message: impl Into<String>, code: EditCode) -> Self {
        Self { code, ..Self::new(message) }
    }

    /// A rejected save: `new EmbedEditError(formatErrors(errors), 'INVALID', { errors, template })`.
    pub fn rejected(errors: Vec<String>, template: Value) -> Self {
        Self { message: format_errors(&errors), code: EditCode::Invalid, errors, template: Some(Box::new(template)) }
    }
}

/// `embedCacheKey(guildId, name)`.
pub fn embed_cache_key(guild_id: &str, name: &str) -> String {
    format!("guild:{guild_id}:embed:{name}")
}

/// `notFoundText(name)`.
pub fn not_found_text(name: &str) -> String {
    format!("There is no saved embed called **{name}**. See `/embed list`.")
}

/// `takenText(name)`.
pub fn taken_text(name: &str) -> String {
    format!("An embed called **{name}** already exists. Edit it with `/embed edit name:{name}`.")
}

/// `limitText()`.
pub fn limit_text() -> String {
    format!("This server already has {SAVED_EMBEDS_LIMIT} saved embeds, the maximum. Delete one with `/embed delete` first.")
}

fn dedupe(list: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in list {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// `formatErrors(errors)`: the bullet list shown for a rejected save (at most 10 entries).
pub fn format_errors(errors: &[String]) -> String {
    dedupe(errors.iter().cloned()).into_iter().take(10).map(|e| format!("• {e}")).collect::<Vec<_>>().join("\n")
}

// ── Names ──────────────────────────────────────────────────────────────────────

/// `normalizeEmbedName(input)`: a slug (lowercase, accents removed, spaces to `-`, only `a-z 0-9 - _`, at most 32),
/// or `None` when nothing usable remains.
pub fn normalize_embed_name(input: &str) -> Option<String> {
    use unicode_normalization::UnicodeNormalization;
    use unicode_normalization::char::is_combining_mark;
    let decomposed: String = input.nfkd().filter(|c| !is_combining_mark(*c)).collect();
    let lowered = lower(trim(&decomposed));
    // `\s+` -> '-'
    let mut dashed = String::with_capacity(lowered.len());
    let mut in_space = false;
    for c in lowered.chars() {
        if bot_util::js::is_js_space(c) {
            if !in_space {
                dashed.push('-');
            }
            in_space = true;
        } else {
            dashed.push(c);
            in_space = false;
        }
    }
    // `[^a-z0-9_-]` removed, then `-{2,}` collapsed.
    let mut s = String::with_capacity(dashed.len());
    for c in dashed.chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_' || *c == '-') {
        if c == '-' && s.ends_with('-') {
            continue;
        }
        s.push(c);
    }
    let s: String = s.trim_start_matches('-').chars().take(EMBED_NAME_MAX).collect();
    let s = s.trim_end_matches('-').to_string();
    (!s.is_empty()).then_some(s)
}

/// `isClearWord(str)`: true for `none`, `clear`, `remove` and `-` (any case).
pub fn is_clear_word(s: &str) -> bool {
    let w = lower(trim(s));
    CLEAR_WORDS.contains(&w.as_str())
}

// ── URL and emoji checks ───────────────────────────────────────────────────────

/// `/^\{[a-z0-9_.]+\}$/i`.
fn is_single_placeholder(v: &str) -> bool {
    v.strip_prefix('{')
        .and_then(|r| r.strip_suffix('}'))
        .is_some_and(|inner| !inner.is_empty() && inner.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.'))
}

/// `/^https?:\/\//i`.
fn starts_with_http(v: &str) -> bool {
    let l = v.get(..8).map(str::to_ascii_lowercase).unwrap_or_else(|| v.to_ascii_lowercase());
    l.starts_with("http://") || l.starts_with("https://")
}

/// `checkImageUrl(str)`: an http(s) link, or a single placeholder such as `{user.avatar}` / `{server.icon}`.
pub fn check_image_url(s: &str) -> Result<String, EmbedEditError> {
    let v = trim(s);
    let n = utf16_len(v);
    if n > 0 && n <= 2048 && (is_http_url(v) || is_single_placeholder(v)) {
        return Ok(v.to_string());
    }
    Err(EmbedEditError::new("Images must be an http(s) link or a variable like {user.avatar} or {server.icon}."))
}

/// `checkLinkUrl(str, max = 2048)`: an http(s) link of at most `max` characters.
pub fn check_link_url(s: &str, max: usize) -> Result<String, EmbedEditError> {
    let v = trim(s);
    let n = utf16_len(v);
    if n > 0 && n <= max && starts_with_http(v) && is_http_url(v) {
        return Ok(v.to_string());
    }
    Err(EmbedEditError::new(format!("Links must start with http:// or https:// and be at most {max} characters.")))
}

/// `CUSTOM_EMOJI_RE` (`^<(a?):([A-Za-z0-9_]{1,32}):(\d{17,20})>$`): `(animated, name, id)`.
pub fn parse_custom_emoji(s: &str) -> Option<(bool, &str, &str)> {
    let inner = s.strip_prefix('<')?.strip_suffix('>')?;
    let mut parts = inner.split(':');
    let (a, name, id) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || !(a.is_empty() || a == "a") {
        return None;
    }
    let name_ok = (1..=32).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    let id_ok = (17..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit());
    (name_ok && id_ok).then_some((a == "a", name, id))
}

/// `\p{Extended_Pictographic}`, from the table bot-core generated from the JS regex engine.
fn pictographic_ranges() -> &'static [(u32, u32)] {
    static RANGES: std::sync::LazyLock<Vec<(u32, u32)>> = std::sync::LazyLock::new(|| {
        let raw: Value = serde_json::from_str(include_str!("../../bot-core/data/emoji.json")).unwrap_or(Value::Null);
        raw["extendedPictographic"]
            .as_array()
            .map(|a| a.iter().filter_map(|r| Some((r[0].as_u64()? as u32, r[1].as_u64()? as u32))).collect())
            .unwrap_or_default()
    });
    &RANGES
}

fn is_ext_pict(c: char) -> bool {
    let cp = c as u32;
    pictographic_ranges()
        .binary_search_by(|(lo, hi)| {
            if cp < *lo {
                std::cmp::Ordering::Greater
            } else if cp > *hi {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

fn is_regional_indicator(c: char) -> bool {
    ('\u{1F1E6}'..='\u{1F1FF}').contains(&c)
}

fn is_emoji_modifier(c: char) -> bool {
    ('\u{1F3FB}'..='\u{1F3FF}').contains(&c)
}

const ZWJ: char = '\u{200D}';
const VS16: char = '\u{FE0F}';
const KEYCAP: char = '\u{20E3}';

/// `UNICODE_EMOJI_RE` plus the "exactly one grapheme" check of `isEmojiSyntax`, for a string the regex accepts:
/// `(?:\p{Extended_Pictographic}|\p{Regional_Indicator}{2}|[0-9#*]\u{FE0F}?⃣)[\u{FE0F}‍\p{Extended_Pictographic}\p{Emoji_Modifier}]*`.
/// A pictographic after the first only joins the same grapheme when a ZWJ follows a pictographic (plus extenders)
/// right before it (UAX #29 GB11); FE0F, the modifiers, the keycap mark and ZWJ never start a new one.
fn is_single_unicode_emoji(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let mut i = match chars.as_slice() {
        [c, ..] if is_ext_pict(*c) => 1,
        [a, b, ..] if is_regional_indicator(*a) && is_regional_indicator(*b) => 2,
        [c, rest @ ..] if c.is_ascii_digit() || *c == '#' || *c == '*' => match rest {
            [VS16, KEYCAP, ..] => 3,
            [KEYCAP, ..] => 2,
            _ => return false,
        },
        _ => return false,
    };
    while i < chars.len() {
        let c = chars[i];
        if is_ext_pict(c) {
            // GB11: ExtPict Extend* ZWJ × ExtPict.
            if i < 2 || chars[i - 1] != ZWJ {
                return false;
            }
            let mut k = i - 2;
            while chars[k] == VS16 || chars[k] == KEYCAP || is_emoji_modifier(chars[k]) {
                if k == 0 {
                    return false;
                }
                k -= 1;
            }
            if !is_ext_pict(chars[k]) {
                return false;
            }
        } else if !(c == VS16 || c == ZWJ || is_emoji_modifier(c)) {
            return false;
        }
        i += 1;
    }
    true
}

/// `isEmojiSyntax(str)`: a custom emoji (`<:name:id>` / `<a:name:id>`) or exactly one Unicode emoji.
pub fn is_emoji_syntax(s: &str) -> bool {
    let s = trim(s);
    if parse_custom_emoji(s).is_some() {
        return true;
    }
    let n = utf16_len(s);
    if n == 0 || n > 32 {
        return false;
    }
    is_single_unicode_emoji(s)
}

/// `parseButtonEmoji(str, isKnownEmoji)`: validates a button emoji and returns it trimmed. `is_known(id)` says
/// whether the bot can see a custom emoji.
pub fn parse_button_emoji(s: &str, is_known: impl Fn(&str) -> bool) -> Result<String, EmbedEditError> {
    let s = trim(s);
    if !is_emoji_syntax(s) {
        return Err(EmbedEditError::new("That isn't a valid emoji. Use a standard emoji or a custom emoji from a server I'm in."));
    }
    if let Some((_, _, id)) = parse_custom_emoji(s)
        && !is_known(id)
    {
        return Err(EmbedEditError::new("I can only use custom emojis from servers I'm in."));
    }
    Ok(s.to_string())
}

// ── Validation ─────────────────────────────────────────────────────────────────

/// `validateForSave(tpl)`: the foundation checks (non-empty, Discord limits) plus the link-button rules. Empty means
/// the template may be saved.
pub fn validate_for_save(tpl: &Value) -> Vec<String> {
    let mut errors = validate_template(tpl, true).errors;
    let t = normalize_template(tpl);
    for (idx, b) in t["buttons"].as_array().into_iter().flatten().enumerate() {
        let i = idx + 1;
        let label = b["label"].as_str().unwrap_or("");
        if trim(label).is_empty() {
            errors.push(format!("Button {i} has no label."));
        }
        let url_ok = b["url"].as_str().is_some_and(|u| utf16_len(u) <= BUTTON_URL_MAX && starts_with_http(u) && is_http_url(u));
        if !url_ok {
            errors.push(format!("Button {i} needs an http(s) link of at most {BUTTON_URL_MAX} characters."));
        }
        if let Some(e) = b["emoji"].as_str().filter(|e| !e.is_empty())
            && !is_emoji_syntax(e)
        {
            errors.push(format!("Button {i} has an invalid emoji."));
        }
    }
    dedupe(errors)
}

// ── Rendering ──────────────────────────────────────────────────────────────────

/// `resolveDefaultColor(settings)`: the guild's default embed colour, or the bot's info colour.
pub fn resolve_default_color(settings: Option<&GuildSettings>) -> u32 {
    resolve_default_color_of(settings.and_then(|s| s.embed_color).map(i64::from))
}

/// [`resolve_default_color`] of a raw `embedColor`.
pub fn resolve_default_color_of(embed_color: Option<i64>) -> u32 {
    match embed_color {
        Some(c) if (0..=0xFF_FFFF).contains(&c) => c as u32,
        _ => bot_config::colors::INFO,
    }
}

/// `extractRoleMentionIds(content)`: the distinct `<@&id>` role ids in a message text (at most 100).
pub fn extract_role_mention_ids(content: Option<&str>) -> Vec<String> {
    let text = content.unwrap_or("");
    let mut out: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(pos) = rest.find("<@&") {
        let after = &rest[pos + 3..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if (17..=20).contains(&digits) && after.as_bytes().get(digits) == Some(&b'>') {
            let id = after[..digits].to_string();
            if !out.contains(&id) {
                out.push(id);
            }
            rest = &after[digits + 1..];
        } else {
            rest = &rest[pos + 1..];
        }
    }
    out.truncate(MAX_ROLE_MENTIONS);
    out
}

/// An `allowedMentions` object as the JS builds it: `{ parse: [] }`, or `{ parse: ['users'], roles: [...] }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MentionPolicy {
    pub parse: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roles: Option<Vec<String>>,
}

impl MentionPolicy {
    /// `{ parse: [] }`: pings nobody.
    pub fn none() -> Self {
        Self { parse: Vec::new(), roles: None }
    }

    /// `{ parse: ['users'], roles }`.
    pub fn users_and(roles: Vec<String>) -> Self {
        Self { parse: vec!["users".into()], roles: Some(roles) }
    }

    /// The serenity form.
    pub fn to_allowed(&self) -> CreateAllowedMentions {
        let mut m = CreateAllowedMentions::new();
        if self.parse.iter().any(|p| p == "users") {
            m = m.all_users(true);
        }
        if let Some(roles) = &self.roles {
            m = m.roles(roles.iter().filter_map(|r| r.parse::<u64>().ok().filter(|v| *v != 0)).map(RoleId::new).collect::<Vec<_>>());
        }
        m
    }
}

/// `resolvePublishMentions`'s result: `{ allowedMentions, refused }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishMentions {
    pub allowed_mentions: MentionPolicy,
    /// Role ids typed in the text that will not ping.
    pub refused: Vec<String>,
}

/// `resolvePublishMentions(content, channel, member)`: user mentions ping, `@everyone`/`@here` never do, and a
/// literal `<@&id>` role mention pings only when the role is mentionable or the member may mention every role in the
/// channel (`pingableRoleIds`).
pub fn resolve_publish_mentions(
    guild: &serenity::all::Guild,
    content: Option<&str>,
    channel: &GuildChannel,
    member: Option<(UserId, &[RoleId])>,
) -> PublishMentions {
    let ids = extract_role_mention_ids(content);
    if ids.is_empty() {
        return PublishMentions { allowed_mentions: MentionPolicy::users_and(Vec::new()), refused: Vec::new() };
    }
    let p = pingable_role_ids(guild, channel, member, &ids);
    PublishMentions { allowed_mentions: MentionPolicy::users_and(p.allowed), refused: p.refused }
}

/// `mentions` of [`render_saved_embed`] and [`mention_policy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MentionMode {
    /// `'publish'`.
    Publish,
    /// `'none'` (preview, update, presets).
    None,
}

/// `mentionPolicy(content, mode, { channel, member })`: `content` is the text the admin wrote (never the rendered
/// text). Without a guild and channel no role is allowed (fail closed).
pub fn mention_policy(
    content: Option<&str>,
    mode: MentionMode,
    where_: Option<(&serenity::all::Guild, &GuildChannel)>,
    member: Option<(UserId, &[RoleId])>,
) -> MentionPolicy {
    if mode != MentionMode::Publish {
        return MentionPolicy::none();
    }
    match where_ {
        None => MentionPolicy::users_and(Vec::new()),
        Some((g, c)) => resolve_publish_mentions(g, content, c, member).allowed_mentions,
    }
}

/// The member as `(id, roles)` for the permission helpers.
pub fn member_key(member: Option<&Member>) -> Option<(UserId, &[RoleId])> {
    member.map(|m| (m.user.id, m.roles.as_slice()))
}

/// The render context of a saved embed: the member running the command as `{user}`, its guild and a channel
/// (`{ member, user: member?.user ?? null, guild, channel }`).
pub fn embed_render_context(guild: Option<&serenity::all::Guild>, member: Option<&Member>, user: Option<&User>, channel: Option<&GuildChannel>) -> RenderContext {
    RenderContext {
        member: member.map(|m| placeholder_member(m, guild)),
        user: member.map(|m| &m.user).or(user).map(placeholder_user),
        guild: guild.map(placeholder_guild),
        channel: channel.map(placeholder_channel),
        vars: None,
        args: None,
    }
}

/// `renderSavedEmbed(tpl, ctx, { mentions })` with the mention policy already decided ([`mention_policy`]): the
/// message payload, or `None` when it renders empty. The colour defaults to the guild's.
pub fn render_saved_embed(tpl: &Value, ctx: &RenderContext, settings: Option<&GuildSettings>, mentions: &MentionPolicy) -> Option<Rendered> {
    render_template(tpl, ctx, RenderOptions { default_color: Some(resolve_default_color(settings)), allowed_mentions: Some(mentions.to_allowed()), extra_components: Vec::new() })
}

/// `collectUnknownTokens(tpl, renderCtx)`: the distinct unknown `{variables}` in the text parts (as typed, with
/// braces), at most 10.
pub fn collect_unknown_tokens(tpl: &Value, ctx: &RenderContext) -> Vec<String> {
    let t = normalize_template(tpl);
    let e = &t["embed"];
    let mut strings: Vec<Option<&str>> = vec![
        t["content"].as_str(),
        e.get("title").and_then(Value::as_str),
        e.get("description").and_then(Value::as_str),
        e.get("author").and_then(|a| a.get("name")).and_then(Value::as_str),
        e.get("footer").and_then(|f| f.get("text")).and_then(Value::as_str),
    ];
    for f in e.get("fields").and_then(Value::as_array).into_iter().flatten() {
        strings.push(f.get("name").and_then(Value::as_str));
        strings.push(f.get("value").and_then(Value::as_str));
    }
    for b in t["buttons"].as_array().into_iter().flatten() {
        strings.push(b.get("label").and_then(Value::as_str));
    }
    let mut seen: Vec<String> = Vec::new();
    for s in strings.into_iter().flatten().filter(|s| !s.is_empty()) {
        for token in render_placeholders_detailed(s, ctx, None).unknown_tokens {
            if !seen.contains(&token) {
                seen.push(token);
            }
            if seen.len() >= MAX_UNKNOWN_TOKENS {
                return seen;
            }
        }
    }
    seen
}

/// `publishHint(row, name)`: the line appended to a change reply when the embed is published (else `''`).
pub fn publish_hint(channel_id: Option<&str>, message_id: Option<&str>, name: &str) -> String {
    match (channel_id.filter(|c| !c.is_empty()), message_id.filter(|m| !m.is_empty())) {
        (Some(c), Some(_)) => format!("\nIt is published in <#{c}>. Run `/embed update name:{name}` to apply the change there."),
        _ => String::new(),
    }
}

/// [`publish_hint`] of a row.
pub fn publish_hint_of(row: &SavedEmbed) -> String {
    publish_hint(row.channel_id.as_deref(), row.message_id.as_deref(), &row.name)
}

/// `unknownVariablesNote(tpl, renderCtx)`: the warning line for unknown variables (else `''`).
pub fn unknown_variables_note(tpl: &Value, ctx: &RenderContext) -> String {
    let tokens = collect_unknown_tokens(tpl, ctx);
    if tokens.is_empty() {
        return String::new();
    }
    format!("\n⚠️ Unknown variables are shown as typed: {}", tokens.iter().map(|t| format!("`{t}`")).collect::<Vec<_>>().join(", "))
}

/// The file name of [`draft_attachment`].
pub const DRAFT_FILE_NAME: &str = "embed-draft.json";

/// `draftAttachment(tpl)`: a JSON file holding the template, so typed text is never lost on a rejected form.
pub fn draft_attachment(tpl: &Value) -> CreateAttachment {
    CreateAttachment::bytes(template_to_json(tpl).into_bytes(), DRAFT_FILE_NAME)
}

// ── Pure template mutators (1-based indexes; never change the argument) ────────

/// A value an option may keep, clear or set (JS `undefined` / `null` / a value).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Change<T> {
    /// `undefined`: keep the current value.
    #[default]
    Keep,
    /// `null`: clear it.
    Clear,
    /// A new value.
    Set(T),
}

impl<T: Serialize> Change<T> {
    fn json(&self) -> Option<Value> {
        match self {
            Self::Keep => None,
            Self::Clear => Some(Value::Null),
            Self::Set(v) => Some(json!(v)),
        }
    }
}

fn clone_template(tpl: &Value) -> Value {
    normalize_template(tpl)
}

fn embed_of(t: &mut Value) -> &mut Map<String, Value> {
    if !t["embed"].is_object() {
        t["embed"] = json!({});
    }
    t["embed"].as_object_mut().unwrap_or_else(|| unreachable!("set above"))
}

fn fields_of(embed: &mut Map<String, Value>) -> &mut Vec<Value> {
    if !embed.get("fields").is_some_and(Value::is_array) {
        embed.insert("fields".into(), json!([]));
    }
    embed.get_mut("fields").and_then(Value::as_array_mut).unwrap_or_else(|| unreachable!("set above"))
}

fn buttons_of(t: &mut Value) -> &mut Vec<Value> {
    if !t["buttons"].is_array() {
        t["buttons"] = json!([]);
    }
    t["buttons"].as_array_mut().unwrap_or_else(|| unreachable!("set above"))
}

/// `clampIndex(position, length)`: a 1-based position as an insert index (`min(max(position - 1, 0), length)`).
fn clamp_index(position: i64, length: usize) -> usize {
    (position - 1).max(0).min(length as i64) as usize
}

fn check_index(index: i64, length: usize, what: &str) -> Result<usize, EmbedEditError> {
    if index < 1 || index as usize > length {
        return Err(EmbedEditError::new(format!("There is no {what} {index}.")));
    }
    Ok(index as usize - 1)
}

/// `addField(tpl, { title, value, inline, position })`: adds a field (default: last; `position` is 1-based).
pub fn add_field(tpl: &Value, title: &str, value: &str, inline: bool, position: Option<i64>) -> Result<Value, EmbedEditError> {
    let mut t = clone_template(tpl);
    let (name, text) = (trim(title), trim(value));
    if name.is_empty() || text.is_empty() {
        return Err(EmbedEditError::new("Field title and text cannot be empty."));
    }
    let fields = fields_of(embed_of(&mut t));
    if fields.len() >= MAX_FIELDS {
        return Err(EmbedEditError::new(format!("An embed can have at most {MAX_FIELDS} fields.")));
    }
    let at = position.map(|p| clamp_index(p, fields.len())).unwrap_or(fields.len());
    fields.insert(at, json!({ "name": name, "value": text, "inline": inline }));
    Ok(t)
}

/// What [`edit_field`] changes (`None`: keep).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldEdit {
    pub title: Option<String>,
    pub value: Option<String>,
    pub inline: Option<bool>,
    pub position: Option<i64>,
}

/// `editField(tpl, index, { title, value, inline, position })`: changes a field; only the given parts change.
pub fn edit_field(tpl: &Value, index: i64, edit: &FieldEdit) -> Result<Value, EmbedEditError> {
    let mut t = clone_template(tpl);
    let fields = fields_of(embed_of(&mut t));
    let i = check_index(index, fields.len(), "field")?;
    let mut field = fields[i].clone();
    if let Some(title) = &edit.title {
        if trim(title).is_empty() {
            return Err(EmbedEditError::new("Field title and text cannot be empty."));
        }
        field["name"] = json!(trim(title));
    }
    if let Some(value) = &edit.value {
        if trim(value).is_empty() {
            return Err(EmbedEditError::new("Field title and text cannot be empty."));
        }
        field["value"] = json!(trim(value));
    }
    if let Some(inline) = edit.inline {
        field["inline"] = json!(inline);
    }
    match edit.position {
        Some(p) => {
            fields.remove(i);
            let at = clamp_index(p, fields.len());
            fields.insert(at, field);
        }
        None => fields[i] = field,
    }
    Ok(t)
}

/// `removeField(tpl, index)`.
pub fn remove_field(tpl: &Value, index: i64) -> Result<Value, EmbedEditError> {
    let mut t = clone_template(tpl);
    let len = t["embed"].get("fields").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
    let i = check_index(index, len, "field")?;
    if let Some(fields) = t["embed"].get_mut("fields").and_then(Value::as_array_mut) {
        fields.remove(i);
    }
    Ok(t)
}

/// `addButton(tpl, { label, url, emoji, position })`: adds a link button (http(s) URL of at most 512 characters).
pub fn add_button(tpl: &Value, label: &str, url: &str, emoji: Option<&str>, position: Option<i64>) -> Result<Value, EmbedEditError> {
    let mut t = clone_template(tpl);
    let text = trim(label);
    if text.is_empty() {
        return Err(EmbedEditError::new("The button needs a label."));
    }
    let link = check_link_url(url, BUTTON_URL_MAX)?;
    let buttons = buttons_of(&mut t);
    if buttons.len() >= MAX_BUTTONS {
        return Err(EmbedEditError::new(format!("A message can have at most {MAX_BUTTONS} link buttons.")));
    }
    let at = position.map(|p| clamp_index(p, buttons.len())).unwrap_or(buttons.len());
    buttons.insert(at, json!({ "label": text, "url": link, "emoji": emoji }));
    Ok(t)
}

/// `removeButton(tpl, index)`.
pub fn remove_button(tpl: &Value, index: i64) -> Result<Value, EmbedEditError> {
    let mut t = clone_template(tpl);
    let buttons = buttons_of(&mut t);
    let i = check_index(index, buttons.len(), "button")?;
    buttons.remove(i);
    Ok(t)
}

/// `setImages(tpl, { image, thumbnail })`: sets the large image and/or the thumbnail.
pub fn set_images(tpl: &Value, image: &Change<String>, thumbnail: &Change<String>) -> Value {
    let mut t = clone_template(tpl);
    let embed = embed_of(&mut t);
    if let Some(v) = image.json() {
        embed.insert("image".into(), v);
    }
    if let Some(v) = thumbnail.json() {
        embed.insert("thumbnail".into(), v);
    }
    t
}

fn current<'a>(embed: &'a Map<String, Value>, key: &str) -> Option<&'a Map<String, Value>> {
    embed.get(key).and_then(Value::as_object)
}

fn or_current(change: &Change<String>, cur: Option<&Map<String, Value>>, key: &str) -> Value {
    change.json().unwrap_or_else(|| cur.and_then(|c| c.get(key)).filter(|v| !v.is_null()).cloned().unwrap_or(Value::Null))
}

/// `setAuthor(tpl, { text, iconUrl, url })`: a clear word as `text` removes the author line; otherwise the given parts
/// are merged into the current one (the caller converts clear words in `iconUrl`/`url` and validates the URLs).
pub fn set_author(tpl: &Value, text: Option<&str>, icon_url: &Change<String>, url: &Change<String>) -> Result<Value, EmbedEditError> {
    let mut t = clone_template(tpl);
    let embed = embed_of(&mut t);
    if let Some(text) = text
        && is_clear_word(text)
    {
        embed.insert("author".into(), Value::Null);
        return Ok(t);
    }
    let cur = current(embed, "author").cloned();
    if cur.is_none() && text.is_none() {
        return Err(EmbedEditError::new("Set the author text first (text:)."));
    }
    let name = match text {
        Some(s) => trim(s).to_string(),
        None => cur.as_ref().and_then(|c| c.get("name")).and_then(Value::as_str).unwrap_or("").to_string(),
    };
    if name.is_empty() {
        return Err(EmbedEditError::new("The author text cannot be empty."));
    }
    let author = json!({ "name": name, "iconUrl": or_current(icon_url, cur.as_ref(), "iconUrl"), "url": or_current(url, cur.as_ref(), "url") });
    embed.insert("author".into(), author);
    Ok(t)
}

/// `setFooter(tpl, { text, iconUrl, timestamp })`: a clear word as `text` removes the footer; `timestamp` sets the
/// timestamp flag.
pub fn set_footer(tpl: &Value, text: Option<&str>, icon_url: &Change<String>, timestamp: Option<bool>) -> Result<Value, EmbedEditError> {
    let mut t = clone_template(tpl);
    let embed = embed_of(&mut t);
    if text.is_some_and(is_clear_word) {
        embed.insert("footer".into(), Value::Null);
    } else if text.is_some() || *icon_url != Change::Keep {
        let cur = current(embed, "footer").cloned();
        if cur.is_none() && text.is_none() {
            return Err(EmbedEditError::new("Set the footer text first (text:)."));
        }
        let body = match text {
            Some(s) => trim(s).to_string(),
            None => cur.as_ref().and_then(|c| c.get("text")).and_then(Value::as_str).unwrap_or("").to_string(),
        };
        if body.is_empty() {
            return Err(EmbedEditError::new("The footer text cannot be empty."));
        }
        embed.insert("footer".into(), json!({ "text": body, "iconUrl": or_current(icon_url, cur.as_ref(), "iconUrl") }));
    }
    if let Some(ts) = timestamp {
        embed.insert("timestamp".into(), json!(ts));
    }
    Ok(t)
}

/// `setColor(tpl, color)`: 0..0xFFFFFF, or `None` for the server default.
pub fn set_color(tpl: &Value, color: Option<u32>) -> Value {
    let mut t = clone_template(tpl);
    embed_of(&mut t).insert("color".into(), json!(color));
    t
}

/// `setTitleUrl(tpl, url)`: the title link, or `None` to remove it.
pub fn set_title_url(tpl: &Value, url: Option<&str>) -> Value {
    let mut t = clone_template(tpl);
    embed_of(&mut t).insert("url".into(), json!(url));
    t
}

// ── Import ─────────────────────────────────────────────────────────────────────

/// `String(v)` (`${v}`) of a JSON value; `undefined` for a missing one.
fn js_text(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => bot_util::js::number_to_string(n.as_f64().unwrap_or(0.0)),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Array(a)) => a.iter().map(|x| if x.is_null() { String::new() } else { js_text(Some(x)) }).collect::<Vec<_>>().join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// JS truthiness of a JSON value.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

fn discord_shape_to_template(obj: &Map<String, Value>, warnings: &mut Vec<String>) -> Value {
    let embeds = obj.get("embeds").and_then(Value::as_array).cloned().unwrap_or_default();
    if embeds.len() > 1 {
        warnings.push("Only the first embed was imported.".into());
    }
    let mut buttons = Vec::new();
    let mut skipped = false;
    for row in obj.get("components").and_then(Value::as_array).into_iter().flatten() {
        for c in row.get("components").and_then(Value::as_array).into_iter().flatten() {
            let is_link = c.get("type").and_then(Value::as_f64) == Some(2.0) && c.get("style").and_then(Value::as_f64) == Some(5.0) && truthy(c.get("url"));
            if is_link {
                let emoji = match c.get("emoji") {
                    Some(e) if truthy(Some(e)) => {
                        if truthy(e.get("id")) {
                            let a = if truthy(e.get("animated")) { "a" } else { "" };
                            json!(format!("<{a}:{}:{}>", js_text(e.get("name")), js_text(e.get("id"))))
                        } else {
                            match e.get("name") {
                                None | Some(Value::Null) => Value::Null,
                                Some(v) => v.clone(),
                            }
                        }
                    }
                    _ => Value::Null,
                };
                let label = match c.get("label") {
                    None | Some(Value::Null) => json!(""),
                    Some(v) => v.clone(),
                };
                buttons.push(json!({ "label": label, "url": c["url"], "emoji": emoji }));
            } else {
                skipped = true;
            }
        }
    }
    if skipped {
        warnings.push("Only link buttons were imported.".into());
    }
    json!({
        "content": obj.get("content").filter(|v| v.is_string()).cloned().unwrap_or(Value::Null),
        "embed": embeds.first().filter(|e| truthy(Some(e))).map(spec_from_embed).unwrap_or(Value::Null),
        "buttons": buttons,
    })
}

/// `parseImportPayload(text)`'s result: `template` is `None` when `errors` is not empty.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportResult {
    pub template: Option<Value>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// `parseImportPayload(text)`: imported JSON (our own export, a bare embed, Discord message JSON or a Discohook
/// backup). Pure.
pub fn parse_import_payload(text: &str) -> ImportResult {
    let mut warnings = Vec::new();
    let fail = |errors: Vec<String>, warnings: Vec<String>| ImportResult { template: None, errors, warnings };
    if text.len() > IMPORT_MAX_BYTES {
        return fail(vec!["The JSON must be 20 KB or smaller.".into()], warnings);
    }
    let mut obj: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return fail(vec!["That is not valid JSON.".into()], warnings),
    };
    // A Discohook backup wraps the messages: { messages: [{ data: { content, embeds, components } }] }.
    if let Some(messages) = obj.get("messages").and_then(Value::as_array).cloned()
        && obj.is_object()
    {
        if messages.len() > 1 {
            warnings.push("Only the first message was imported.".into());
        }
        obj = match messages.first().and_then(|m| m.get("data")) {
            None | Some(Value::Null) => json!({}),
            Some(d) => d.clone(),
        };
    }
    let Some(map) = obj.as_object() else {
        return fail(vec!["The JSON must be an object.".into()], warnings);
    };
    let is_discord_shape = (map.get("embeds").is_some_and(Value::is_array) || map.get("components").is_some_and(Value::is_array)) && !map.contains_key("embed");
    let template = normalize_template(&if is_discord_shape { discord_shape_to_template(map, &mut warnings) } else { obj.clone() });
    let errors = validate_for_save(&template);
    ImportResult { template: errors.is_empty().then_some(template), errors, warnings }
}

// ── Welcome Channel presets ────────────────────────────────────────────────────

/// `buildWelcomePresets({ inviteUrl, modRoleIds })`: the four Welcome Channel presets in [`PRESET_NAMES`] order (no
/// `{user…}` tokens: they render with the admin who runs the command).
pub fn build_welcome_presets(invite_url: Option<&str>, mod_role_ids: &[String]) -> Vec<(&'static str, Value)> {
    let invite_url = invite_url.filter(|u| !u.is_empty());
    let team_roles = if mod_role_ids.is_empty() {
        String::new()
    } else {
        let mentions: Vec<String> = mod_role_ids.iter().map(|id| format!("<@&{id}>")).collect();
        format!("**Moderators:** {}\n", join_with_limit(&mentions, " ", 1500, default_more))
    };
    let raw = [
        (
            "welcome-greeting",
            json!({ "content": null, "buttons": [], "embed": {
                "title": "👋 Welcome to {server}!",
                "description": "We're glad you're here.\n\n📜 Read the rules below.\n🔗 Check out the links.\n💬 Then come and say hi!",
                "thumbnail": "{server.icon}",
                "footer": { "text": "{server}", "iconUrl": "{server.icon}" },
            } }),
        ),
        (
            "welcome-rules",
            json!({ "content": null, "embed": {
                "title": "📜 Server Rules",
                "description": "**1.** Be respectful. No harassment, hate speech or discrimination.\n**2.** No spam or self-promotion without permission.\n**3.** Keep content safe for work and in the right channels.\n**4.** No illegal content and no sharing of personal information.\n**5.** Follow the Discord Terms of Service and Community Guidelines.\n**6.** The moderators have the final say.",
                "footer": { "text": "Breaking the rules can lead to a warning, mute, kick or ban." },
            }, "buttons": [
                { "label": "Discord Terms", "url": "https://discord.com/terms", "emoji": "📄" },
                { "label": "Community Guidelines", "url": "https://discord.com/guidelines", "emoji": "🛡\u{FE0F}" },
            ] }),
        ),
        (
            "welcome-links",
            json!({ "content": null, "embed": {
                "title": "🔗 Links",
                "description": if invite_url.is_some() {
                    "Invite your friends with the button below!\n\nAdmins: add your website and socials with `/embed button add name:welcome-links`."
                } else {
                    "Admins: add your website, socials and an invite link with `/embed button add name:welcome-links`."
                },
            }, "buttons": match invite_url {
                Some(u) => json!([{ "label": "Invite your friends", "url": u, "emoji": "📨" }]),
                None => json!([]),
            } }),
        ),
        (
            "welcome-team",
            json!({ "content": null, "buttons": [], "embed": {
                "title": "🛡\u{FE0F} Staff Team",
                "description": format!("**Owner:** {{server.owner}}\n{team_roles}\nNeed help? Reach out to a member of the staff team."),
            } }),
        ),
    ];
    raw.into_iter().map(|(name, t)| (name, normalize_template(&t))).collect()
}

// ── messageDelete hook ─────────────────────────────────────────────────────────

/// `isEmbedDeleteCandidate(ctx)`: the synchronous hook filter. Only the bot's own messages, or an uncached one, can
/// be a published embed (without a Discord connection the bot's id is unknown, so only uncached messages pass).
pub fn is_embed_delete_candidate(app: &App, data: &bot_core::hooks::events::MessageDelete) -> bool {
    if data.message_id.get() == 0 || data.guild_id.get() == 0 {
        return false;
    }
    match &data.message {
        None => true,
        Some(_) if data.partial => true,
        Some(m) => app.discord().is_some_and(|d| m.author.id == d.bot_user_id()),
    }
}

/// `onEmbedMessageDelete(ctx)`: a deleted published message makes its saved embed a draft again (the channel id is
/// kept as a hint). Never fails.
pub async fn on_embed_message_delete(pool: &PgPool, guild_id: &str, message_id: &str) {
    let run = async {
        UpdateBuilder::new(&SAVED_EMBED_INFO)
            .set("messageId", Value::Null)?
            .filter("guildId", json!(guild_id))?
            .filter("messageId", json!(message_id))?
            .without_returning()
            .build()?
            .execute(pool)
            .await
    };
    if let Err(e) = run.await {
        bot_util::log_warn!("embeds: could not clear a deleted message id", bot_core::error::log_arg(&anyhow::Error::from(e)));
    }
}

// ── Data access ────────────────────────────────────────────────────────────────

/// `getSavedEmbed(guildId, name)`: the row; fails on a database error.
pub async fn get_saved_embed(pool: &PgPool, guild_id: &str, name: &str) -> anyhow::Result<Option<SavedEmbed>> {
    let q = SelectBuilder::new(&SAVED_EMBED_INFO).filter("guildId", json!(guild_id))?.filter("name", json!(name))?.build()?;
    Ok(q.fetch_optional::<SavedEmbed, _>(pool).await?)
}

/// `prisma.savedEmbed.findFirst({ where: { id, guildId } })` (`guild_id` `None`: by id alone, `findUnique`).
pub async fn get_saved_embed_by_id(pool: &PgPool, guild_id: Option<&str>, id: &str) -> anyhow::Result<Option<SavedEmbed>> {
    let mut q = SelectBuilder::new(&SAVED_EMBED_INFO).filter("id", json!(id))?;
    if let Some(g) = guild_id {
        q = q.filter("guildId", json!(g))?;
    }
    Ok(q.build()?.fetch_optional::<SavedEmbed, _>(pool).await?)
}

/// `requireSavedEmbed(guildId, name)`: the row, or an [`EmbedEditError`] NOTFOUND.
pub async fn require_saved_embed(pool: &PgPool, guild_id: &str, name: &str) -> anyhow::Result<SavedEmbed> {
    match get_saved_embed(pool, guild_id, name).await? {
        Some(r) => Ok(r),
        None => Err(EmbedEditError::with_code(not_found_text(name), EditCode::NotFound).into()),
    }
}

/// `countSavedEmbeds(guildId)`.
pub async fn count_saved_embeds(pool: &PgPool, guild_id: &str) -> anyhow::Result<i64> {
    let n: i64 = bot_db::sqlx::query_scalar(r#"SELECT COUNT(*)::bigint FROM "SavedEmbed" WHERE "guildId" = $1"#).bind(guild_id).fetch_one(pool).await?;
    Ok(n)
}

/// A row of [`list_saved_embeds`] (`{ name, channelId, messageId, updatedAt }`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListedEmbed {
    pub name: String,
    pub channel_id: Option<String>,
    pub message_id: Option<String>,
    #[serde(with = "bot_db::serde_js::date")]
    pub updated_at: bot_db::sqlx::types::chrono::DateTime<bot_db::sqlx::types::chrono::Utc>,
}

/// `listSavedEmbeds(guildId)`: every saved embed of a guild (the cap plus 50 at most), ordered by name.
pub async fn list_saved_embeds(pool: &PgPool, guild_id: &str) -> anyhow::Result<Vec<ListedEmbed>> {
    let q = SelectBuilder::new(&SAVED_EMBED_INFO).filter("guildId", json!(guild_id))?.order_by("name", true)?.limit(SAVED_EMBEDS_LIMIT as i64 + 50).build()?;
    let rows = q.fetch_all::<SavedEmbed, _>(pool).await?;
    Ok(rows.into_iter().map(|r| ListedEmbed { name: r.name, channel_id: r.channel_id, message_id: r.message_id, updated_at: r.updated_at }).collect())
}

/// A row of [`search_saved_embeds`] (`{ id, name, channelId, messageId }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchRow {
    pub id: String,
    pub name: String,
    pub channel_id: Option<String>,
    pub message_id: Option<String>,
}

/// `searchSavedEmbeds(guildId, query, take = 25)`: names containing the normalised query, case-insensitive, ordered
/// by name (Prisma's `contains` + `mode: 'insensitive'`: an unescaped `ILIKE`, so `_` matches any one character).
/// `take` is clamped to 1..100 (0 means 25). Never fails: an error gives `[]`.
pub async fn search_saved_embeds(pool: &PgPool, guild_id: &str, query: &str, take: i64) -> Vec<SearchRow> {
    let q = normalize_embed_name(query).unwrap_or_default();
    let limit = (if take == 0 { 25 } else { take }).clamp(1, 100);
    let result = if q.is_empty() {
        bot_db::sqlx::query_as::<_, SavedEmbed>(r#"SELECT * FROM "SavedEmbed" WHERE "guildId" = $1 ORDER BY "name" ASC LIMIT $2"#)
            .bind(guild_id)
            .bind(limit)
            .fetch_all(pool)
            .await
    } else {
        bot_db::sqlx::query_as::<_, SavedEmbed>(r#"SELECT * FROM "SavedEmbed" WHERE "guildId" = $1 AND "name"::text ILIKE $2 ORDER BY "name" ASC LIMIT $3"#)
            .bind(guild_id)
            .bind(format!("%{q}%"))
            .bind(limit)
            .fetch_all(pool)
            .await
    };
    match result {
        Ok(rows) => rows.into_iter().map(|r| SearchRow { id: r.id, name: r.name, channel_id: r.channel_id, message_id: r.message_id }).collect(),
        Err(e) => {
            bot_util::log_debug!(format!("embeds: searchSavedEmbeds failed: {e}"));
            Vec::new()
        }
    }
}

/// The raw statements of this crate (`RAW_SQL`), for the database test.
pub const RAW_SQL: [(&str, &str); 3] = [
    ("countSavedEmbeds", r#"SELECT COUNT(*)::bigint FROM "SavedEmbed" WHERE "guildId" = $1"#),
    ("searchSavedEmbeds:all", r#"SELECT * FROM "SavedEmbed" WHERE "guildId" = $1 ORDER BY "name" ASC LIMIT $2"#),
    ("searchSavedEmbeds", r#"SELECT * FROM "SavedEmbed" WHERE "guildId" = $1 AND "name"::text ILIKE $2 ORDER BY "name" ASC LIMIT $3"#),
];

/// `getSavedEmbedTemplate(guildId, name)`: the raw template of a saved embed (placeholders unexpanded), or `None` when
/// it does not exist, the name is invalid, or anything fails. Does not check the embeds module toggle (the caller
/// decides). Fail-open cache: readers fill with SET NX (300 s, a miss 60 s), so they never overwrite a newer
/// write-through value.
pub async fn get_saved_embed_template(app: &App, guild_id: &str, name: &str) -> Option<Value> {
    let n = normalize_embed_name(name)?;
    if guild_id.is_empty() {
        return None;
    }
    let key = embed_cache_key(guild_id, &n);
    if let Some(hit) = app.cache().get(&key).await
        && (hit.is_object() || hit.is_array())
    {
        if hit.get("__none").is_some_and(|v| truthy(Some(v))) {
            return None;
        }
        return Some(normalize_template(&hit));
    }
    let row = match get_saved_embed(app.db(), guild_id, &n).await {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_warn!("embeds: getSavedEmbedTemplate failed", bot_core::error::log_arg(&e));
            return None;
        }
    };
    let Some(row) = row else {
        app.cache().set_nx(&key, &json!({ "__none": true }), EMBED_CACHE_MISS_TTL_SECONDS).await;
        return None;
    };
    let tpl = normalize_template(&row.template);
    app.cache().set_nx(&key, &tpl, EMBED_CACHE_TTL_SECONDS).await;
    Some(tpl)
}

/// `createSavedEmbed(guildId, name, template, createdBy)`: validates, ensures the GuildSettings row, enforces the
/// per-guild cap (soft: concurrent creates may overshoot), then writes through to the cache. Fails with an
/// [`EmbedEditError`] INVALID (with `errors`/`template`), LIMIT or TAKEN.
pub async fn create_saved_embed(app: &App, guild_id: &str, name: &str, template: &Value, created_by: &str) -> anyhow::Result<SavedEmbed> {
    let next = normalize_template(template);
    let errors = validate_for_save(&next);
    if !errors.is_empty() {
        return Err(EmbedEditError::rejected(errors, next).into());
    }
    app.settings().ensure_guild_settings(guild_id).await?;
    if count_saved_embeds(app.db(), guild_id).await? >= SAVED_EMBEDS_LIMIT as i64 {
        return Err(EmbedEditError::with_code(limit_text(), EditCode::Limit).into());
    }
    let q = InsertBuilder::new(&SAVED_EMBED_INFO)
        .value("guildId", json!(guild_id))?
        .value("name", json!(name))?
        .value("template", next.clone())?
        .value("createdBy", json!(created_by))?
        .build()?;
    let row = match q.fetch_one::<SavedEmbed, _>(app.db()).await {
        Ok(r) => r,
        Err(e) if e.is_unique_violation() => return Err(EmbedEditError::with_code(taken_text(name), EditCode::Taken).into()),
        Err(e) => return Err(e.into()),
    };
    app.cache().set(&embed_cache_key(guild_id, name), &next, EMBED_CACHE_TTL_SECONDS).await;
    Ok(row)
}

/// `UPDATE "SavedEmbed" SET ... WHERE id` returning the row (`None`: it was deleted, Prisma's P2025).
async fn update_row(pool: &PgPool, id: &str, data: Vec<(&str, Value)>) -> anyhow::Result<Option<SavedEmbed>> {
    let q = UpdateBuilder::new(&SAVED_EMBED_INFO).set_all(data)?.filter("id", json!(id))?.build()?;
    Ok(q.fetch_optional::<SavedEmbed, _>(pool).await?)
}

/// `lockedUpdate(guildId, id, compute, name)`: re-reads the row under the per-embed lock, computes the next template
/// from the fresh one (`compute(fresh_template, fresh_row)` returns the template, the errors found while building it
/// and an extra value for the caller), validates it, saves it and writes it through to the cache.
pub async fn locked_update<T, F>(app: &App, guild_id: &str, id: &str, compute: F, name: Option<&str>) -> anyhow::Result<(SavedEmbed, T)>
where
    F: FnOnce(&Value, &SavedEmbed) -> Result<(Value, Vec<String>, T), EmbedEditError> + Send + 'static,
    T: Send + 'static,
{
    let (app2, g, i, n) = (app.clone(), guild_id.to_string(), id.to_string(), name.map(str::to_string));
    run_locked(app, id, move || async move {
        let Some(fresh) = get_saved_embed_by_id(app2.db(), Some(&g), &i).await? else {
            let text = match &n {
                Some(n) => not_found_text(n),
                None => "That saved embed no longer exists. See `/embed list`.".to_string(),
            };
            return Err(EmbedEditError::with_code(text, EditCode::NotFound).into());
        };
        let (out, extra_errors, extra) = compute(&normalize_template(&fresh.template), &fresh)?;
        let next = normalize_template(&out);
        let errors = dedupe(extra_errors.into_iter().chain(validate_for_save(&next)));
        if !errors.is_empty() {
            return Err(EmbedEditError::rejected(errors, next).into());
        }
        let Some(saved) = update_row(app2.db(), &i, vec![("template", next.clone())]).await? else {
            return Err(EmbedEditError::with_code(not_found_text(&fresh.name), EditCode::NotFound).into());
        };
        app2.cache().set(&embed_cache_key(&g, &fresh.name), &next, EMBED_CACHE_TTL_SECONDS).await;
        Ok((saved, extra))
    })
    .await
}

/// `mutateSavedEmbed(guildId, name, mutator)` whose mutator also returns a value computed from the fresh template
/// (the field position a reply names, ...).
pub async fn mutate_saved_embed_with<T, F>(app: &App, guild_id: &str, name: &str, mutator: F) -> anyhow::Result<(SavedEmbed, T)>
where
    F: FnOnce(&Value) -> Result<(Value, T), EmbedEditError> + Send + 'static,
    T: Send + 'static,
{
    let Some(row) = get_saved_embed(app.db(), guild_id, name).await? else {
        return Err(EmbedEditError::with_code(not_found_text(name), EditCode::NotFound).into());
    };
    locked_update(app, guild_id, &row.id, move |tpl, _| mutator(tpl).map(|(t, x)| (t, Vec::new(), x)), Some(name)).await
}

/// `mutateSavedEmbed(guildId, name, mutator)`: applies a pure mutator to a saved embed under the per-embed lock
/// (`name` already normalised). Fails with NOTFOUND or INVALID (the mutator's message or the validation list).
pub async fn mutate_saved_embed<F>(app: &App, guild_id: &str, name: &str, mutator: F) -> anyhow::Result<SavedEmbed>
where
    F: FnOnce(&Value) -> Result<Value, EmbedEditError> + Send + 'static,
{
    Ok(mutate_saved_embed_with(app, guild_id, name, move |t| mutator(t).map(|v| (v, ()))).await?.0)
}

/// `replaceTemplate(guildId, id, template)`: replaces the template of a saved embed (import with overwrite).
pub async fn replace_template(app: &App, guild_id: &str, id: &str, template: Value) -> anyhow::Result<SavedEmbed> {
    Ok(locked_update(app, guild_id, id, move |_, _| Ok((template, Vec::new(), ())), None).await?.0)
}

/// `setPublished(id, { channelId, messageId })`: records where an embed was published. A row deleted meanwhile is
/// not an error.
pub async fn set_published(pool: &PgPool, id: &str, channel_id: Option<&str>, message_id: Option<&str>) -> anyhow::Result<()> {
    update_row(pool, id, vec![("channelId", json!(channel_id)), ("messageId", json!(message_id))]).await?;
    Ok(())
}

// ── Discord ────────────────────────────────────────────────────────────────────

/// A failed Discord call: `code` is the API error code (`DiscordAPIError.code`); `api` is false for anything that is
/// not an API error (network, no connection), which the JS rethrows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordFailure {
    pub code: Option<u64>,
    pub message: String,
    pub api: bool,
}

impl DiscordFailure {
    /// An API error with `code`.
    pub fn api(code: u64, message: impl Into<String>) -> Self {
        Self { code: Some(code), message: message.into(), api: true }
    }

    /// Not an API error.
    pub fn other(message: impl Into<String>) -> Self {
        Self { code: None, message: message.into(), api: false }
    }

    pub fn of(err: &serenity::Error) -> Self {
        let code = bot_core::discord::error_code(err);
        Self { code, message: bot_core::discord::error_message(err), api: code.is_some() }
    }
}

impl std::fmt::Display for DiscordFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DiscordFailure {}

/// `RESTJSONErrorCodes` used here.
pub mod codes {
    pub const UNKNOWN_CHANNEL: u64 = 10003;
    pub const UNKNOWN_MESSAGE: u64 = 10008;
    pub const MAXIMUM_GUILD_CHANNELS: u64 = 30013;
    pub const MISSING_ACCESS: u64 = 50001;
    pub const CANNOT_EDIT_OTHER_USERS_MESSAGE: u64 = 50005;
    pub const MISSING_PERMISSIONS: u64 = 50013;
    pub const INVALID_ACTION_ON_ARCHIVED_THREAD: u64 = 50083;
    pub const THREAD_LOCKED: u64 = 160005;
}

/// A message the bot posted or fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostedMessage {
    pub id: String,
    pub channel_id: String,
    pub author_id: String,
    /// `message.url`.
    pub url: String,
}

/// What the welcome channel is created with (`guild.channels.create({...})`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WelcomeChannelSpec {
    pub name: String,
    pub position: u16,
    pub topic: String,
    pub reason: String,
    /// `@everyone` (the guild id) is denied these.
    pub everyone_deny: Permissions,
    /// The bot is allowed these.
    pub bot_id: UserId,
    pub bot_allow: Permissions,
}

/// The Discord calls of this crate. [`HttpDiscord`] is the real one.
#[async_trait]
pub trait EmbedDiscord: Send + Sync {
    /// `channel.send(payload)`.
    async fn send(&self, guild_id: GuildId, channel_id: ChannelId, payload: &Payload) -> Result<PostedMessage, DiscordFailure>;
    /// `guild.channels.cache.get(id) ?? guild.channels.fetch(id)`.
    async fn fetch_channel(&self, guild_id: GuildId, channel_id: ChannelId) -> Result<GuildChannel, DiscordFailure>;
    /// `channel.messages.fetch(id)`.
    async fn fetch_message(&self, guild_id: GuildId, channel_id: ChannelId, message_id: MessageId) -> Result<PostedMessage, DiscordFailure>;
    /// `message.edit(payload)`.
    async fn edit_message(&self, channel_id: ChannelId, message_id: MessageId, payload: &Payload) -> Result<(), DiscordFailure>;
    /// `channel.messages.delete(id)`.
    async fn delete_message(&self, channel_id: ChannelId, message_id: MessageId) -> Result<(), DiscordFailure>;
    /// `channel.createInvite({ maxAge: 0, maxUses: 0, unique: false, reason })`: the invite URL.
    async fn create_invite(&self, channel_id: ChannelId, reason: &str) -> Result<String, DiscordFailure>;
    /// `guild.channels.create({...})`.
    async fn create_channel(&self, guild_id: GuildId, spec: &WelcomeChannelSpec) -> Result<GuildChannel, DiscordFailure>;
}

/// The real Discord calls, through the app's serenity client (and its cache for channel lookups).
pub struct HttpDiscord {
    pub app: App,
}

impl HttpDiscord {
    fn http(&self) -> Result<Arc<serenity::all::Http>, DiscordFailure> {
        self.app.http().ok_or_else(|| DiscordFailure::other("Discord is not connected"))
    }
}

fn message_link(guild_id: GuildId, channel_id: ChannelId, message_id: MessageId) -> String {
    format!("https://discord.com/channels/{guild_id}/{channel_id}/{message_id}")
}

#[async_trait]
impl EmbedDiscord for HttpDiscord {
    async fn send(&self, guild_id: GuildId, channel_id: ChannelId, payload: &Payload) -> Result<PostedMessage, DiscordFailure> {
        let http = self.http()?;
        let m = channel_id.send_message(http.as_ref(), payload.to_message()).await.map_err(|e| DiscordFailure::of(&e))?;
        Ok(PostedMessage { id: m.id.to_string(), channel_id: channel_id.to_string(), author_id: m.author.id.to_string(), url: message_link(guild_id, channel_id, m.id) })
    }

    async fn fetch_channel(&self, guild_id: GuildId, channel_id: ChannelId) -> Result<GuildChannel, DiscordFailure> {
        // `guild.channels.cache.get(id) ?? guild.channels.fetch(id)`: a fetched channel is put into the cache too.
        use bot_core::interaction_helpers::FetchChannelError;
        bot_core::interaction_helpers::fetch_channel(&self.app, guild_id, channel_id).await.map_err(|e| match e {
            FetchChannelError::Rest(e) => DiscordFailure::of(&e),
            FetchChannelError::Unowned => DiscordFailure::api(codes::UNKNOWN_CHANNEL, "Unknown Channel"),
            FetchChannelError::Offline => DiscordFailure::other("Discord is not connected"),
        })
    }

    async fn fetch_message(&self, guild_id: GuildId, channel_id: ChannelId, message_id: MessageId) -> Result<PostedMessage, DiscordFailure> {
        let http = self.http()?;
        let m = http.get_message(channel_id, message_id).await.map_err(|e| DiscordFailure::of(&e))?;
        Ok(PostedMessage { id: m.id.to_string(), channel_id: channel_id.to_string(), author_id: m.author.id.to_string(), url: message_link(guild_id, channel_id, m.id) })
    }

    async fn edit_message(&self, channel_id: ChannelId, message_id: MessageId, payload: &Payload) -> Result<(), DiscordFailure> {
        let http = self.http()?;
        channel_id.edit_message(http.as_ref(), message_id, payload.to_message_edit()).await.map(|_| ()).map_err(|e| DiscordFailure::of(&e))
    }

    async fn delete_message(&self, channel_id: ChannelId, message_id: MessageId) -> Result<(), DiscordFailure> {
        let http = self.http()?;
        channel_id.delete_message(http.as_ref(), message_id).await.map_err(|e| DiscordFailure::of(&e))
    }

    async fn create_invite(&self, channel_id: ChannelId, reason: &str) -> Result<String, DiscordFailure> {
        let http = self.http()?;
        let invite = channel_id
            .create_invite(http.as_ref(), CreateInvite::new().max_age(0).max_uses(0).unique(false).audit_log_reason(reason))
            .await
            .map_err(|e| DiscordFailure::of(&e))?;
        Ok(invite.url())
    }

    async fn create_channel(&self, guild_id: GuildId, spec: &WelcomeChannelSpec) -> Result<GuildChannel, DiscordFailure> {
        let http = self.http()?;
        let overwrites = vec![
            PermissionOverwrite { allow: Permissions::empty(), deny: spec.everyone_deny, kind: PermissionOverwriteType::Role(RoleId::new(guild_id.get())) },
            PermissionOverwrite { allow: spec.bot_allow, deny: Permissions::empty(), kind: PermissionOverwriteType::Member(spec.bot_id) },
        ];
        let builder = CreateChannel::new(spec.name.clone())
            .kind(ChannelType::Text)
            .position(spec.position)
            .topic(spec.topic.clone())
            .permissions(overwrites)
            .audit_log_reason(&spec.reason);
        let channel = guild_id.create_channel(http.as_ref(), builder).await.map_err(|e| DiscordFailure::of(&e))?;
        // discord.js adds the created channel to the cache (`channels._add`) before CHANNEL_CREATE arrives.
        bot_core::discord::cache_created_channel(&self.app, &channel);
        Ok(channel)
    }
}

/// The Discord calls an app uses (the default is [`HttpDiscord`]; [`set_discord_ops`] replaces it).
#[derive(Default)]
pub struct DiscordOps(RwLock<Option<Arc<dyn EmbedDiscord>>>);

/// Replaces the Discord calls of this app (tests).
pub fn set_discord_ops(app: &App, ops: Arc<dyn EmbedDiscord>) {
    *app.ext::<DiscordOps>().0.write().unwrap_or_else(|e| e.into_inner()) = Some(ops);
}

/// The Discord calls of this app.
pub fn discord_ops(app: &App) -> Arc<dyn EmbedDiscord> {
    if let Some(o) = app.ext::<DiscordOps>().0.read().unwrap_or_else(|e| e.into_inner()).clone() {
        return o;
    }
    Arc::new(HttpDiscord { app: app.clone() })
}

/// The bot's permissions in `channel` (a channel object that may not be cached yet, like one just created).
pub fn bot_permissions_in(app: &App, guild_id: GuildId, channel: &GuildChannel) -> Option<Permissions> {
    let d = app.discord()?;
    let bot = d.cache.current_user().id;
    let guild = d.cache.guild(guild_id)?;
    let me = permissions::me(&guild, bot)?;
    permissions::member_permissions_in(&guild, channel, bot, &me.roles, true)
}

/// `checkChannelUsable(channel, { needs })` for a channel object.
pub fn channel_usable(app: &App, guild_id: GuildId, channel: &GuildChannel, needs: &[&str]) -> ChannelUsable {
    channel_usable_verdict(Some(channel), bot_permissions_in(app, guild_id, channel), needs)
}

/// `memberChannelProblem(channel, member, needs)` on the cached guild (`None` when the guild is not cached).
pub fn member_problem(app: &App, guild_id: GuildId, channel: Option<&GuildChannel>, member: Option<&Member>, needs: &[&str]) -> Option<String> {
    let guild = cached_guild(app, guild_id)?;
    member_channel_problem(&guild, channel, member.map(MemberRef::of), needs)
}

fn parse_snowflake(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|v| *v != 0)
}

// ── Delete ─────────────────────────────────────────────────────────────────────

/// `deleteSavedEmbedAndMessage`'s result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeleteOutcome {
    pub deleted: bool,
    pub message_deleted: bool,
}

/// `deleteSavedEmbedAndMessage({ guild, row, deleteMessage })`: deletes a saved embed and, on request, the message it
/// published.
pub async fn delete_saved_embed_and_message(app: &App, guild_id: GuildId, row: &SavedEmbed, delete_message: bool) -> anyhow::Result<DeleteOutcome> {
    let (app2, row2) = (app.clone(), row.clone());
    run_locked(app, &row.id, move || async move {
        let n = delete_where(&SAVED_EMBED_INFO, [("id", json!(row2.id))])?.execute(app2.db()).await?;
        if n == 0 {
            return Ok(DeleteOutcome { deleted: false, message_deleted: false });
        }
        app2.cache().set(&embed_cache_key(&row2.guild_id, &row2.name), &json!({ "__none": true }), EMBED_CACHE_MISS_TTL_SECONDS).await;
        let mut message_deleted = false;
        if delete_message
            && let (Some(mid), Some(cid)) = (row2.message_id.as_deref().and_then(parse_snowflake), row2.channel_id.as_deref().and_then(parse_snowflake))
        {
            let ops = discord_ops(&app2);
            let (cid, mid) = (ChannelId::new(cid), MessageId::new(mid));
            let result = match ops.fetch_channel(guild_id, cid).await {
                Ok(_) => {
                    bot_core::deletion_tracker::mark_bot_deletion(&app2, &[mid.to_string()], Some("embeds"), None);
                    ops.delete_message(cid, mid).await
                }
                Err(e) => Err(e),
            };
            match result {
                Ok(()) => message_deleted = true,
                Err(e) if e.code == Some(codes::UNKNOWN_MESSAGE) => message_deleted = true,
                Err(e) => bot_util::log_warn!("embeds: could not delete the published message", bot_core::error::log_arg(&anyhow::Error::from(e))),
            }
        }
        Ok(DeleteOutcome { deleted: true, message_deleted })
    })
    .await
}

// ── Publish and update ─────────────────────────────────────────────────────────

/// `publishSavedEmbed`'s result.
#[derive(Debug, Clone, PartialEq)]
pub enum PublishOutcome {
    /// `{ ok: true, message, previous, refused }`.
    Posted { message: PostedMessage, previous: Option<(String, String)>, refused: Vec<String> },
    /// `MEMBER_NO_ACCESS`.
    MemberNoAccess,
    /// `CHANNEL`: the bot cannot post there.
    Channel { missing: Vec<String>, reason: Option<String> },
    /// `EMPTY`.
    Empty,
    /// `DISCORD`.
    Discord { error: DiscordFailure },
}

/// `publishSavedEmbed({ row, channel, member, settings })`: posts a saved embed in a channel and records it as the
/// tracked message (a previous tracked copy is left alone). Fails with NOTFOUND when the row vanished, and on any
/// error that is not a Discord API error.
pub async fn publish_saved_embed(
    app: &App,
    row: &SavedEmbed,
    guild_id: GuildId,
    channel_id: ChannelId,
    member: Option<&Member>,
    settings: Option<Arc<GuildSettings>>,
) -> anyhow::Result<PublishOutcome> {
    let (app2, row2, member2) = (app.clone(), row.clone(), member.cloned());
    run_locked(app, &row.id, move || async move {
        // The JS had the option's resolved channel: one the cache does not hold yet is fetched.
        let channel = discord_ops(&app2).fetch_channel(guild_id, channel_id).await.ok();
        // The member needs View Channel and Send Messages there (the owner and Administrators are exempt).
        if member_problem(&app2, guild_id, channel.as_ref(), member2.as_ref(), &MEMBER_CHANNEL_NEEDS).is_some() {
            return Ok(PublishOutcome::MemberNoAccess);
        }
        let usable = match &channel {
            Some(c) => channel_usable(&app2, guild_id, c, &bot_core::interaction_helpers::CHANNEL_POST_NEEDS),
            None => channel_usable_verdict(None, None, &bot_core::interaction_helpers::CHANNEL_POST_NEEDS),
        };
        let Some(channel) = channel.filter(|_| usable.ok) else {
            return Ok(PublishOutcome::Channel { missing: usable.missing, reason: usable.reason });
        };
        let Some(fresh) = get_saved_embed_by_id(app2.db(), None, &row2.id).await? else {
            return Err(EmbedEditError::with_code(not_found_text(&row2.name), EditCode::NotFound).into());
        };
        let template = normalize_template(&fresh.template);
        let guild = cached_guild(&app2, guild_id);
        let ctx = embed_render_context(guild.as_ref(), member2.as_ref(), None, Some(&channel));
        let content = template["content"].as_str();
        let mentions = mention_policy(content, MentionMode::Publish, guild.as_ref().map(|g| (g, &channel)), member_key(member2.as_ref()));
        let Some(rendered) = render_saved_embed(&template, &ctx, settings.as_deref(), &mentions) else {
            return Ok(PublishOutcome::Empty);
        };
        let refused = match guild.as_ref() {
            Some(g) => resolve_publish_mentions(g, content, &channel, member_key(member2.as_ref())).refused,
            None => extract_role_mention_ids(content),
        };
        let message = match discord_ops(&app2).send(guild_id, channel_id, &rendered.to_payload()).await {
            Ok(m) => m,
            Err(e) if !e.api => return Err(e.into()),
            Err(e) => {
                bot_util::log_warn!(format!("embeds: publish of \"{}\" refused by Discord ({})", row2.name, e.code.unwrap_or(0)), e.message.clone());
                let denied = matches!(e.code, Some(codes::MISSING_PERMISSIONS | codes::MISSING_ACCESS));
                return Ok(if denied { PublishOutcome::Channel { missing: Vec::new(), reason: None } } else { PublishOutcome::Discord { error: e } });
            }
        };
        let previous = fresh.message_id.clone().map(|m| (fresh.channel_id.clone().unwrap_or_default(), m));
        set_published(app2.db(), &row2.id, Some(&channel_id.to_string()), Some(&message.id)).await?;
        Ok(PublishOutcome::Posted { message, previous, refused })
    })
    .await
}

/// `updatePublishedEmbed`'s result.
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateOutcome {
    /// `{ ok: true, message, channel }`.
    Updated { message: PostedMessage, channel_id: String },
    /// `NOT_PUBLISHED`, with the fresh row.
    NotPublished { row: Box<SavedEmbed> },
    /// `CHANNEL_GONE`: the embed is a draft again.
    ChannelGone,
    /// `MESSAGE_GONE`: the embed is a draft again.
    MessageGone,
    /// `MEMBER_NO_ACCESS`.
    MemberNoAccess,
    /// `NO_ACCESS`: the bot lacks permissions (`missing` may be empty).
    NoAccess { missing: Vec<String> },
    /// `ARCHIVED`.
    Archived,
    /// `EMPTY`.
    Empty,
    /// `DISCORD`.
    Discord { error: DiscordFailure },
    /// `UNAVAILABLE`.
    Unavailable,
}

fn is_denied(code: Option<u64>) -> bool {
    matches!(code, Some(codes::MISSING_ACCESS | codes::MISSING_PERMISSIONS))
}

/// `updatePublishedEmbed({ row, guild, member, settings })`: edits the published message to match the current
/// template (re-rendered with `member` as `{user}`; an edit never pings). A deleted message or channel turns the embed
/// back into a draft (REST 10008 / 10003 only).
pub async fn update_published_embed(
    app: &App,
    row: &SavedEmbed,
    guild_id: GuildId,
    member: Option<&Member>,
    settings: Option<Arc<GuildSettings>>,
) -> anyhow::Result<UpdateOutcome> {
    let (app2, row2, member2) = (app.clone(), row.clone(), member.cloned());
    run_locked(app, &row.id, move || async move {
        let Some(fresh) = get_saved_embed_by_id(app2.db(), None, &row2.id).await? else {
            return Err(EmbedEditError::with_code(not_found_text(&row2.name), EditCode::NotFound).into());
        };
        let (Some(mid), Some(cid)) = (fresh.message_id.clone().filter(|s| !s.is_empty()), fresh.channel_id.clone().filter(|s| !s.is_empty())) else {
            return Ok(UpdateOutcome::NotPublished { row: Box::new(fresh) });
        };
        // A missing entity only counts when the guild is available and Discord itself said "unknown".
        if app2.guild_directory().guild_state(&guild_id.to_string()) == bot_core::discord::GuildState::Unavailable {
            return Ok(UpdateOutcome::Unavailable);
        }
        let draft_again = |data: Vec<(&'static str, Value)>| {
            let (pool, id) = (app2.db().clone(), fresh.id.clone());
            async move { update_row(&pool, &id, data).await.map(|_| ()) }
        };
        let ops = discord_ops(&app2);
        let (Some(c), Some(m)) = (parse_snowflake(&cid), parse_snowflake(&mid)) else {
            draft_again(vec![("channelId", Value::Null), ("messageId", Value::Null)]).await?;
            return Ok(UpdateOutcome::ChannelGone);
        };
        let (cid, mid) = (ChannelId::new(c), MessageId::new(m));
        let channel = match ops.fetch_channel(guild_id, cid).await {
            Ok(ch) => ch,
            Err(e) if e.code == Some(codes::UNKNOWN_CHANNEL) => {
                draft_again(vec![("channelId", Value::Null), ("messageId", Value::Null)]).await?;
                return Ok(UpdateOutcome::ChannelGone);
            }
            Err(e) if is_denied(e.code) => return Ok(UpdateOutcome::NoAccess { missing: Vec::new() }),
            Err(e) if e.api => return Ok(UpdateOutcome::Discord { error: e }),
            Err(e) => return Err(e.into()),
        };
        // Editing a message the member cannot see would let them change a private channel's embed.
        if member_problem(&app2, guild_id, Some(&channel), member2.as_ref(), &["ViewChannel"]).is_some() {
            return Ok(UpdateOutcome::MemberNoAccess);
        }
        let usable = channel_usable(&app2, guild_id, &channel, &["ViewChannel", "ReadMessageHistory", "EmbedLinks"]);
        if !usable.ok {
            return Ok(UpdateOutcome::NoAccess { missing: usable.missing });
        }
        let message = match ops.fetch_message(guild_id, cid, mid).await {
            Ok(m) => m,
            Err(e) if e.code == Some(codes::UNKNOWN_MESSAGE) => {
                draft_again(vec![("messageId", Value::Null)]).await?;
                return Ok(UpdateOutcome::MessageGone);
            }
            Err(e) if is_denied(e.code) => return Ok(UpdateOutcome::NoAccess { missing: Vec::new() }),
            Err(e) if e.api => return Ok(UpdateOutcome::Discord { error: e }),
            Err(e) => return Err(e.into()),
        };
        let bot_id = app2.discord().map(|d| d.bot_user_id().to_string());
        if bot_id.as_deref() != Some(message.author_id.as_str()) {
            draft_again(vec![("messageId", Value::Null)]).await?;
            return Ok(UpdateOutcome::MessageGone);
        }
        let guild = cached_guild(&app2, guild_id);
        let ctx = embed_render_context(guild.as_ref(), member2.as_ref(), None, Some(&channel));
        let Some(rendered) = render_saved_embed(&normalize_template(&fresh.template), &ctx, settings.as_deref(), &MentionPolicy::none()) else {
            return Ok(UpdateOutcome::Empty);
        };
        // An empty content clears old text; [] clears old embeds and buttons.
        let mut edit = rendered.to_payload().no_mentions();
        edit.content = Some(rendered.content.clone().unwrap_or_default());
        if let Err(e) = ops.edit_message(cid, mid, &edit).await {
            match e.code {
                Some(codes::CANNOT_EDIT_OTHER_USERS_MESSAGE | codes::UNKNOWN_MESSAGE) => {
                    draft_again(vec![("messageId", Value::Null)]).await?;
                    return Ok(UpdateOutcome::MessageGone);
                }
                Some(codes::INVALID_ACTION_ON_ARCHIVED_THREAD | codes::THREAD_LOCKED) => return Ok(UpdateOutcome::Archived),
                c if is_denied(c) => return Ok(UpdateOutcome::NoAccess { missing: Vec::new() }),
                _ if e.api => {
                    bot_util::log_warn!(format!("embeds: update of \"{}\" refused by Discord ({})", fresh.name, e.code.unwrap_or(0)), e.message.clone());
                    return Ok(UpdateOutcome::Discord { error: e });
                }
                _ => return Err(e.into()),
            }
        }
        Ok(UpdateOutcome::Updated { message, channel_id: cid.to_string() })
    })
    .await
}

// ── Welcome Channel ────────────────────────────────────────────────────────────

/// Where the links preset's invite came from (`invite.source`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteInfo {
    /// `'vanity'`, `'created'` or `'none'`.
    pub source: &'static str,
    pub url: Option<String>,
}

async fn resolve_invite(app: &App, ops: &dyn EmbedDiscord, guild_id: GuildId, channel: &GuildChannel) -> InviteInfo {
    if let Some(code) = cached_guild(app, guild_id).and_then(|g| g.vanity_url_code).filter(|c| !c.is_empty()) {
        return InviteInfo { source: "vanity", url: Some(format!("https://discord.gg/{code}")) };
    }
    if missing_from(bot_permissions_in(app, guild_id, channel), &["CreateInstantInvite"]).is_empty() {
        // unique: false reuses an equivalent invite, so re-running does not pile up permanent invites.
        match ops.create_invite(channel.id, "Welcome Channel invite link").await {
            Ok(url) => return InviteInfo { source: "created", url: Some(url) },
            Err(e) => bot_util::log_debug!(format!("embeds: could not create the welcome invite: {}", e.message)),
        }
    }
    InviteInfo { source: "none", url: None }
}

/// The permissions the bot needs to create the welcome channel.
pub const WELCOME_CREATE_NEEDS: [&str; 5] = ["ManageChannels", "ViewChannel", "SendMessages", "EmbedLinks", "ReadMessageHistory"];

async fn create_welcome_channel(app: &App, ops: &dyn EmbedDiscord, guild_id: GuildId, user: &User) -> Result<GuildChannel, Box<WelcomeOutcome>> {
    let missing = missing_from(bot_permissions(app, PermTarget::Guild(guild_id)), &WELCOME_CREATE_NEEDS);
    if !missing.is_empty() {
        return Err(Box::new(WelcomeOutcome::BotPerms { missing }));
    }
    let me_perms = bot_permissions(app, PermTarget::Guild(guild_id)).unwrap_or_else(Permissions::empty);
    // Discord only lets a bot deny permission bits it has itself.
    let mut deny = Permissions::empty();
    for k in ["SendMessages", "SendMessagesInThreads", "CreatePublicThreads", "CreatePrivateThreads"] {
        if let Some(f) = permissions::flag(k)
            && permissions::js_has(me_perms, f)
        {
            deny |= f;
        }
    }
    let guild_name = cached_guild(app, guild_id).map(|g| g.name).unwrap_or_default();
    let bot_id = app.discord().map(|d| d.bot_user_id()).unwrap_or_else(|| UserId::new(1));
    let spec = WelcomeChannelSpec {
        name: "welcome".into(),
        position: 0,
        topic: clamp_text(&format!("Welcome to {guild_name}!"), 1024),
        reason: format!("Welcome channel set up by {}", user_tag(user)),
        everyone_deny: deny,
        bot_id,
        bot_allow: Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES | Permissions::EMBED_LINKS | Permissions::READ_MESSAGE_HISTORY,
    };
    match ops.create_channel(guild_id, &spec).await {
        Ok(c) => Ok(c),
        Err(e) if e.code == Some(codes::MAXIMUM_GUILD_CHANNELS) => Err(Box::new(WelcomeOutcome::TooManyChannels)),
        Err(e) if e.code == Some(codes::MISSING_PERMISSIONS) => Err(Box::new(WelcomeOutcome::BotPerms { missing: vec!["ManageChannels".into()] })),
        Err(e) if e.api => Err(Box::new(WelcomeOutcome::Discord { error: e })),
        Err(e) => Err(Box::new(WelcomeOutcome::Failed(e.message))),
    }
}

/// `setUpWelcomeChannel`'s result.
#[derive(Debug, Clone, PartialEq)]
pub enum WelcomeOutcome {
    /// `{ ok: true, channel, createdChannel, createdPresets, reusedPresets, posted, tip, invite }`.
    Ready {
        channel_id: String,
        created_channel: bool,
        created_presets: Vec<String>,
        reused_presets: Vec<String>,
        posted: Vec<String>,
        tip: Option<String>,
        invite: Option<InviteInfo>,
    },
    /// `LIMIT`.
    Limit { needed: usize, count: i64 },
    /// `CHANNEL`.
    Channel { channel_id: String, missing: Vec<String>, reason: Option<String> },
    /// `ALREADY`.
    Already { channel_id: String },
    /// `BOT_PERMS`.
    BotPerms { missing: Vec<String> },
    /// `TOO_MANY_CHANNELS`.
    TooManyChannels,
    /// `WRONG_CHANNEL`.
    WrongChannel,
    /// `DISCORD`.
    Discord { error: DiscordFailure },
    /// `PARTIAL`.
    Partial { channel_id: String, posted: Vec<String>, unposted: Vec<String>, error: String },
    /// A non-API failure while creating the channel (the JS rethrows it).
    Failed(String),
}

/// What `/embed welcome-channel` passes (`{ interaction, create, channel, settings }`).
#[derive(Debug, Clone)]
pub struct WelcomeArgs {
    pub guild_id: GuildId,
    pub user: User,
    pub member: Option<Member>,
    /// `interaction.channel`.
    pub interaction_channel_id: ChannelId,
    pub create: bool,
    /// `channel:` (an existing channel).
    pub channel_id: Option<ChannelId>,
    pub settings: Option<Arc<GuildSettings>>,
}

/// `setUpWelcomeChannel({ interaction, create, channel, settings })`: creates (or reuses) the four editable preset
/// embeds and posts them in order. Existing presets are never overwritten. Never pings.
pub async fn set_up_welcome_channel(app: &App, args: WelcomeArgs) -> anyhow::Result<WelcomeOutcome> {
    let guild_id = args.guild_id;
    let g = guild_id.to_string();
    let ops = discord_ops(app);

    // 1. Presets present, and room for the missing ones. Runs before any channel is created.
    let mut by_name: Vec<(String, SavedEmbed)> = Vec::new();
    for name in PRESET_NAMES {
        if let Some(r) = get_saved_embed(app.db(), &g, name).await? {
            by_name.push((name.to_string(), r));
        }
    }
    let get = |list: &Vec<(String, SavedEmbed)>, n: &str| list.iter().find(|(k, _)| k == n).map(|(_, r)| r.clone());
    let missing_names: Vec<&str> = PRESET_NAMES.iter().copied().filter(|n| get(&by_name, n).is_none()).collect();
    let count = count_saved_embeds(app.db(), &g).await?;
    if count + missing_names.len() as i64 > SAVED_EMBEDS_LIMIT as i64 {
        return Ok(WelcomeOutcome::Limit { needed: missing_names.len(), count });
    }

    // 2. Target channel.
    let mut created_channel = false;
    let mut tip = None;
    let target: GuildChannel = if let Some(cid) = args.channel_id {
        // The JS had the option's resolved channel: one the cache does not hold yet is fetched.
        let Some(ch) = ops.fetch_channel(guild_id, cid).await.ok() else {
            let v = channel_usable_verdict(None, None, &bot_core::interaction_helpers::CHANNEL_POST_NEEDS);
            return Ok(WelcomeOutcome::Channel { channel_id: cid.to_string(), missing: v.missing, reason: v.reason });
        };
        let usable = channel_usable(app, guild_id, &ch, &bot_core::interaction_helpers::CHANNEL_POST_NEEDS);
        if !usable.ok {
            return Ok(WelcomeOutcome::Channel { channel_id: cid.to_string(), missing: usable.missing, reason: usable.reason });
        }
        if let Some(guild) = cached_guild(app, guild_id)
            && let Some(everyone) = guild.roles.get(&permissions::everyone_role_id(&guild))
            && permissions::role_permissions_in(&guild, &ch, everyone, true).is_some_and(|p| permissions::js_has(p, Permissions::SEND_MESSAGES))
        {
            tip = Some("Tip: members can write in this channel; make it read-only if you want a MEE6-style welcome channel.".to_string());
        }
        ch
    } else if args.create {
        if let Some(greeting) = get(&by_name, "welcome-greeting")
            && greeting.message_id.as_deref().is_some_and(|m| !m.is_empty())
            && let Some(c) = greeting.channel_id.as_deref().filter(|c| !c.is_empty())
            && parse_snowflake(c).is_some_and(|id| cached_channel(app, guild_id, ChannelId::new(id)).is_some())
        {
            return Ok(WelcomeOutcome::Already { channel_id: c.to_string() });
        }
        match create_welcome_channel(app, ops.as_ref(), guild_id, &args.user).await {
            Ok(c) => {
                created_channel = true;
                c
            }
            Err(o) => match *o {
                WelcomeOutcome::Failed(msg) => anyhow::bail!(msg),
                o => return Ok(o),
            },
        }
    } else {
        let Some(ch) = ops.fetch_channel(guild_id, args.interaction_channel_id).await.ok().filter(|c| matches!(c.kind, ChannelType::Text | ChannelType::News)) else {
            return Ok(WelcomeOutcome::WrongChannel);
        };
        let usable = channel_usable(app, guild_id, &ch, &bot_core::interaction_helpers::CHANNEL_POST_NEEDS);
        if !usable.ok {
            return Ok(WelcomeOutcome::Channel { channel_id: ch.id.to_string(), missing: usable.missing, reason: usable.reason });
        }
        ch
    };

    // 3. Invite link, only when the links preset has to be created.
    let invite = if missing_names.contains(&"welcome-links") { Some(resolve_invite(app, ops.as_ref(), guild_id, &target).await) } else { None };

    // 4. Create the missing presets. Existing ones (possibly edited by admins) are never overwritten.
    let guild = cached_guild(app, guild_id);
    let mod_role_ids: Vec<String> = args
        .settings
        .as_ref()
        .map(|s| s.mod_role_ids.clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|id| guild.as_ref().is_some_and(|gd| parse_snowflake(id).is_some_and(|v| gd.roles.contains_key(&RoleId::new(v)))))
        .collect();
    let templates = build_welcome_presets(invite.as_ref().and_then(|i| i.url.as_deref()), &mod_role_ids);
    let mut created_presets: Vec<String> = Vec::new();
    for name in &missing_names {
        let tpl = templates.iter().find(|(n, _)| n == name).map(|(_, t)| t.clone()).unwrap_or_else(empty_template);
        match create_saved_embed(app, &g, name, &tpl, &args.user.id.to_string()).await {
            Ok(row) => {
                by_name.push((name.to_string(), row));
                created_presets.push(name.to_string());
            }
            Err(e) => {
                if e.downcast_ref::<EmbedEditError>().is_some_and(|x| x.code == EditCode::Taken)
                    && let Some(row) = get_saved_embed(app.db(), &g, name).await?
                {
                    // A concurrent run created it first.
                    by_name.push((name.to_string(), row));
                    continue;
                }
                return Err(e);
            }
        }
    }
    let reused_presets: Vec<String> = PRESET_NAMES.iter().filter(|n| !created_presets.iter().any(|c| c == *n)).map(|n| n.to_string()).collect();

    // 5. Publish in order; stop at the first failure and report what was posted.
    let mut posted: Vec<String> = Vec::new();
    for name in PRESET_NAMES {
        let Some(row) = get(&by_name, name) else { continue };
        let (app2, ops2, target2, member2, settings2, user2) = (app.clone(), ops.clone(), target.clone(), args.member.clone(), args.settings.clone(), args.user.clone());
        let id = row.id.clone();
        let outcome: anyhow::Result<&'static str> = run_locked(app, &row.id, move || async move {
            let Some(fresh) = get_saved_embed_by_id(app2.db(), None, &id).await? else { return Ok("gone") };
            let guild = cached_guild(&app2, guild_id);
            let ctx = embed_render_context(guild.as_ref(), member2.as_ref(), Some(&user2), Some(&target2));
            let Some(rendered) = render_saved_embed(&normalize_template(&fresh.template), &ctx, settings2.as_deref(), &MentionPolicy::none()) else {
                return Ok("empty");
            };
            let message = ops2.send(guild_id, target2.id, &rendered.to_payload()).await?;
            set_published(app2.db(), &id, Some(&target2.id.to_string()), Some(&message.id)).await?;
            Ok("posted")
        })
        .await;
        match outcome {
            Ok("posted") => posted.push(name.to_string()),
            Ok(_) => {}
            Err(e) => {
                bot_util::log_warn!(format!("embeds: welcome-channel stopped at \"{name}\""), bot_core::error::log_arg(&e));
                let message = e.downcast_ref::<DiscordFailure>().map(|d| d.message.clone()).unwrap_or_else(|| e.to_string());
                let unposted = PRESET_NAMES.iter().filter(|n| !posted.iter().any(|p| p == *n)).map(|n| n.to_string()).collect();
                return Ok(WelcomeOutcome::Partial { channel_id: target.id.to_string(), posted, unposted, error: message });
            }
        }
    }
    Ok(WelcomeOutcome::Ready { channel_id: target.id.to_string(), created_channel, created_presets, reused_presets, posted, tip, invite })
}

// ── Modal submit ───────────────────────────────────────────────────────────────

async fn modal_settings(ctx: &ModalCtx) -> Option<Arc<GuildSettings>> {
    let g = ctx.guild_id()?.to_string();
    if let Some(s) = ctx.app.settings().peek_settings(&g) {
        return Some(s);
    }
    let app = ctx.app.clone();
    let fut = async move { Ok::<_, std::convert::Infallible>(app.settings().get_settings_safe(&g).await) };
    bot_util::async_util::with_timeout(fut, ctx.ack_budget_ms(), "embed modal settings").await.ok().flatten()
}

/// `replyQuietly(interaction, payload)`: `editReply` once acknowledged, else an ephemeral `reply`; a gone interaction
/// is not worth a warning.
async fn reply_quietly(ctx: &ModalCtx, payload: Payload) {
    let result = if ctx.responder.is_acknowledged() {
        ctx.responder.edit_reply(payload.to_edit()).await.map(|_| ())
    } else {
        ctx.responder.reply(payload.ephemeral(true).to_reply()).await
    };
    if let Err(e) = result
        && !is_interaction_gone(&e)
    {
        bot_util::log_warn!("embeds: could not answer the form", e.to_string());
    }
}

/// The hint under a rejected form.
pub const DRAFT_HINT: &str = "Your text is attached; fix it and use `/embed import`.";

fn modal_render_context(ctx: &ModalCtx) -> RenderContext {
    let g = ctx.guild_id();
    let guild = g.and_then(|g| cached_guild(&ctx.app, g));
    let channel = g.and_then(|g| cached_channel(&ctx.app, g, Interactive::channel_id(ctx)));
    embed_render_context(guild.as_ref(), ctx.member(), Some(ctx.user()), channel.as_ref())
}

fn modal_template(ctx: &ModalCtx, base: &Value) -> (Value, Vec<String>) {
    parse_template_modal(|id| ctx.value(id), base, true)
}

/// `handleEmbedModalSubmit(interaction)`: the submit of `embed-modal:new:{name}` (create) and
/// `embed-modal:{savedEmbedId}` (edit). Registered as a persistent modal handler by the plugin (gated by the embeds
/// module).
pub async fn handle_embed_modal_submit(ctx: ModalCtx) -> bot_core::BotResult {
    let guild_id = match ctx.guild_id() {
        Some(g) if cached_guild(&ctx.app, g).is_some() => g,
        _ => {
            let _ = ctx.responder.reply(Payload::embed(error_embed("Server Only", Some("This form only works in a server."), None)).ephemeral(true).to_reply()).await;
            return Ok(());
        }
    };
    let settings = modal_settings(&ctx).await;
    // Defence in depth: modal submits bypass the router's permission level check.
    let access = ctx.member_access();
    let level = bot_core::permission_checker::resolve_permission_level(access.as_ref(), settings.as_deref(), ctx.app.config().bot_owner_id.as_deref());
    if level < bot_config::PermissionLevel::Admin {
        let embed = error_embed("Insufficient Permissions", Some("You need the Admin level to edit saved embeds."), None);
        let _ = ctx.responder.reply(Payload::embed(embed).ephemeral(true).to_reply()).await;
        return Ok(());
    }
    if let Err(e) = ctx.responder.defer_reply(false).await {
        if !is_interaction_gone(&e) {
            bot_util::log_warn!("embeds: could not defer the form reply", e.to_string());
        }
        return Ok(());
    }
    if let Err(e) = modal_body(&ctx, guild_id).await {
        bot_util::log_error!("embeds: form submit failed", bot_core::error::log_arg(&e));
        reply_quietly(&ctx, Payload::embed(error_embed("Something Went Wrong", Some("I could not save that right now. Please try again in a moment."), None))).await;
    }
    Ok(())
}

async fn modal_body(ctx: &ModalCtx, guild_id: GuildId) -> anyhow::Result<()> {
    let g = guild_id.to_string();
    let render_ctx = modal_render_context(ctx);
    let (_, params) = ctx.parsed();
    if params.first().map(String::as_str) == Some("new") {
        let Some(name) = normalize_embed_name(params.get(1).map(String::as_str).unwrap_or("")) else {
            let e = error_embed("Invalid Name", Some("Embed names can use letters, numbers, - and _ (up to 32 characters)."), None);
            reply_quietly(ctx, Payload::embed(e)).await;
            return Ok(());
        };
        let (template, errors) = modal_template(ctx, &empty_template());
        let all = dedupe(errors.into_iter().chain(validate_for_save(&template)));
        if !all.is_empty() {
            let e = error_embed("Invalid Embed", Some(&format!("{}\n\n{DRAFT_HINT}", format_errors(&all))), None);
            reply_quietly(ctx, Payload::embed(e).file(draft_attachment(&template))).await;
            return Ok(());
        }
        let row = match create_saved_embed(&ctx.app, &g, &name, &template, &ctx.user().id.to_string()).await {
            Ok(r) => r,
            Err(err) => {
                let Some(e) = err.downcast_ref::<EmbedEditError>() else { return Err(err) };
                let title = match e.code {
                    EditCode::Taken => "Name Taken",
                    EditCode::Limit => "Limit Reached",
                    _ => "Could Not Save",
                };
                let embed = error_embed(title, Some(&format!("{}\n\n{DRAFT_HINT}", e.message)), None);
                reply_quietly(ctx, Payload::embed(embed).file(draft_attachment(&template))).await;
                return Ok(());
            }
        };
        let tpl = normalize_template(&row.template);
        let text = format!(
            "Saved **{name}**. Next: `/embed field add`, `/embed button add`, `/embed preview`, then `/embed publish`.{}",
            unknown_variables_note(&tpl, &render_ctx)
        );
        let embed = success_embed("Embed Created", Some(&text), None).field("Variables", clamp_text(&placeholder_help(&["server", "channel"]), 1024), false);
        reply_quietly(ctx, Payload::embed(embed)).await;
        return Ok(());
    }

    // Edit: params[0] is the saved embed id.
    let id = params.first().cloned().unwrap_or_default();
    let Some(row) = get_saved_embed_by_id(ctx.app.db(), Some(&g), &id).await? else {
        let (template, _) = modal_template(ctx, &empty_template());
        let e = error_embed("Embed Not Found", Some(&format!("This embed was deleted while the form was open.\n\n{DRAFT_HINT}")), None);
        reply_quietly(ctx, Payload::embed(e).file(draft_attachment(&template))).await;
        return Ok(());
    };
    let (ctx2, name) = (ctx.clone(), row.name.clone());
    let saved = match locked_update(
        &ctx.app,
        &g,
        &row.id,
        move |tpl, _| {
            let (t, errors) = modal_template(&ctx2, tpl);
            Ok((t, errors, ()))
        },
        Some(&name),
    )
    .await
    {
        Ok((s, ())) => s,
        Err(err) => {
            let Some(e) = err.downcast_ref::<EmbedEditError>() else { return Err(err) };
            let payload = if e.code == EditCode::Invalid {
                let embed = error_embed("Invalid Embed", Some(&format!("{}\n\n{DRAFT_HINT}", e.message)), None);
                Payload::embed(embed).file(draft_attachment(e.template.as_deref().unwrap_or(&empty_template())))
            } else {
                Payload::embed(error_embed("Could Not Save", Some(&e.message), None))
            };
            reply_quietly(ctx, payload).await;
            return Ok(());
        }
    };
    let tpl = normalize_template(&saved.template);
    let text = format!(
        "Saved **{}**.{}{}",
        row.name,
        publish_hint(saved.channel_id.as_deref(), saved.message_id.as_deref(), &row.name),
        unknown_variables_note(&tpl, &render_ctx)
    );
    reply_quietly(ctx, Payload::embed(success_embed("Embed Saved", Some(&text), None))).await;
    Ok(())
}
