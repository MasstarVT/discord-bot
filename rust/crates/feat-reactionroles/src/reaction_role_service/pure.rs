//! The pure helpers of `reactionRoleService.js`: constants, emoji and link parsing, names, the error texts, the payload
//! builders and the role-change planner. No I/O.

use std::collections::HashSet;
use std::sync::LazyLock;

use bot_util::js::{trim, utf16_len};
use bot_util::text::clamp_text;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use unicode_segmentation::UnicodeSegmentation;

// ── Constants ─────────────────────────────────────────────────────────────────

/// `RR_TYPES`.
pub const RR_TYPES: [&str; 4] = ["NORMAL", "UNIQUE", "VERIFY", "REVERSE"];
/// `RR_LAYOUTS`.
pub const RR_LAYOUTS: [&str; 3] = ["BUTTONS", "SELECT", "REACTIONS"];
/// `LIST_PAGE_SIZE`.
pub const LIST_PAGE_SIZE: usize = 10;
/// `RATE_LIMIT`: at most `max` role changes per member in `window_ms`.
pub const RATE_LIMIT_MAX: i64 = 8;
pub const RATE_LIMIT_WINDOW_MS: i64 = 10_000;
/// `INDEX_MEMO_TTL_MS`.
pub const INDEX_MEMO_TTL_MS: i64 = 60_000;
/// `INDEX_CACHE_TTL_S`.
pub const INDEX_CACHE_TTL_S: u64 = 300;
/// `SUPPRESS_TTL_MS`.
pub const SUPPRESS_TTL_MS: i64 = 15_000;
/// `DEFAULT_TITLE`.
pub const DEFAULT_TITLE: &str = "Reaction Roles";

/// `MAX_ITEMS`, in the JS key order.
pub const MAX_ITEMS: [(&str, usize); 3] = [("BUTTONS", 25), ("SELECT", 25), ("REACTIONS", 20)];
/// `STYLE_MAP`, in the JS key order.
pub const STYLE_MAP: [(&str, i32); 4] = [("Primary", 1), ("Secondary", 2), ("Success", 3), ("Danger", 4)];
/// `TYPE_LABELS`, in the JS key order.
pub const TYPE_LABELS: [(&str, &str); 4] = [("NORMAL", "Normal"), ("UNIQUE", "Unique"), ("VERIFY", "Verify"), ("REVERSE", "Reverse")];
/// `LAYOUT_LABELS`, in the JS key order.
pub const LAYOUT_LABELS: [(&str, &str); 3] = [("BUTTONS", "Buttons"), ("SELECT", "Dropdown"), ("REACTIONS", "Emoji reactions")];
/// `DEFAULT_DESCRIPTIONS`, in the JS key order.
pub const DEFAULT_DESCRIPTIONS: [(&str, &str); 3] = [
    ("BUTTONS", "Press a button below to manage your roles."),
    ("SELECT", "Use the menu below to manage your roles."),
    ("REACTIONS", "React below to manage your roles."),
];
/// `PLACEHOLDERS`, in the JS key order.
pub const PLACEHOLDERS: [(&str, &str); 4] = [
    ("NORMAL", "Choose the roles you want"),
    ("UNIQUE", "Choose a role"),
    ("VERIFY", "Choose a role to get"),
    ("REVERSE", "Choose a role to remove"),
];
/// The keys of `RR_TEMPLATES`.
pub const RR_TEMPLATE_KEYS: [&str; 2] = ["verify", "notifications"];

fn lookup<T: Copy>(table: &[(&str, T)], key: &str) -> Option<T> {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// `MAX_ITEMS[layout]` (`BUTTONS` 25, `SELECT` 25, `REACTIONS` 20); `None` for an unknown layout.
pub fn max_items(layout: &str) -> Option<usize> {
    lookup(&MAX_ITEMS, layout)
}

/// `STYLE_MAP[name]`: the button style of a `style` choice.
pub fn style_map(name: &str) -> Option<i32> {
    lookup(&STYLE_MAP, name)
}

/// `TYPE_LABELS[type]`.
pub fn type_label(kind: &str) -> Option<&'static str> {
    lookup(&TYPE_LABELS, kind)
}

/// `LAYOUT_LABELS[layout]`.
pub fn layout_label(layout: &str) -> Option<&'static str> {
    lookup(&LAYOUT_LABELS, layout)
}

/// `DEFAULT_DESCRIPTIONS[layout]`.
pub fn default_description(layout: &str) -> Option<&'static str> {
    lookup(&DEFAULT_DESCRIPTIONS, layout)
}

/// `PLACEHOLDERS[type]`.
pub fn placeholder_for(kind: &str) -> Option<&'static str> {
    lookup(&PLACEHOLDERS, kind)
}

/// `indexKey(guildId)`: the Redis key of a guild's panel index.
pub fn index_key(guild_id: &str) -> String {
    format!("guild:{guild_id}:rrpanels")
}

/// The first item of an `RR_TEMPLATES` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RrTemplateItem {
    pub label: Option<&'static str>,
    pub emoji: &'static str,
    pub style: i32,
}

/// An `RR_TEMPLATES` entry (the presets without their own roles).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RrTemplate {
    pub kind: &'static str,
    pub layout: &'static str,
    pub title: &'static str,
    /// The description per layout (`BUTTONS`, `SELECT`, `REACTIONS`).
    pub description: [(&'static str, &'static str); 3],
    pub item: RrTemplateItem,
}

impl RrTemplate {
    /// `tpl.description[layout]`.
    pub fn description_for(&self, layout: &str) -> Option<&'static str> {
        self.description.iter().find(|(l, _)| *l == layout).map(|(_, d)| *d)
    }
}

/// `RR_TEMPLATES[key]`: `verify` and `notifications`.
pub fn rr_template(key: &str) -> Option<RrTemplate> {
    match key {
        "verify" => Some(RrTemplate {
            kind: "VERIFY",
            layout: "BUTTONS",
            title: "✅ Verification",
            description: [
                ("BUTTONS", "Read the rules, then press **Verify** below to unlock the rest of the server."),
                ("SELECT", "Read the rules, then pick **Verify** in the menu below to unlock the rest of the server."),
                ("REACTIONS", "Read the rules, then react with ✅ below to unlock the rest of the server."),
            ],
            item: RrTemplateItem { label: Some("Verify"), emoji: "✅", style: 3 },
        }),
        "notifications" => Some(RrTemplate {
            kind: "NORMAL",
            layout: "BUTTONS",
            title: "🔔 Notification Roles",
            description: [
                ("BUTTONS", "Pick the announcements you want to be pinged for. Press a button again to stop."),
                ("SELECT", "Pick the announcements you want to be pinged for. Your choice replaces your current notification roles."),
                ("REACTIONS", "React to get pinged for an announcement type. Remove your reaction to stop."),
            ],
            item: RrTemplateItem { label: None, emoji: "🔔", style: 2 },
        }),
        _ => None,
    }
}

// ── Emoji ─────────────────────────────────────────────────────────────────────

/// `UNICODE_EMOJI_RE`: one standard emoji (a flag, a keycap, or a pictograph with an optional skin tone, tag sequence
/// or ZWJ chain).
/// The properties come from V8's own tables ([`super::emoji_tables`]): the regex crate ships older Unicode data.
pub static UNICODE_EMOJI_RE: LazyLock<Regex> = LazyLock::new(|| {
    use super::emoji_tables::{EMOJI_MODIFIER, EMOJI_PRESENTATION, EXTENDED_PICTOGRAPHIC, REGIONAL_INDICATOR};
    let class = |sets: &[&[(u32, u32)]]| {
        let mut s = String::from("[");
        for set in sets {
            for (a, b) in set.iter() {
                if a == b {
                    s.push_str(&format!(r"\x{{{a:X}}}"));
                } else {
                    s.push_str(&format!(r"\x{{{a:X}}}-\x{{{b:X}}}"));
                }
            }
        }
        s.push(']');
        s
    };
    let ri = class(&[REGIONAL_INDICATOR]);
    let base = class(&[EXTENDED_PICTOGRAPHIC, EMOJI_PRESENTATION]);
    let modifier = class(&[EMOJI_MODIFIER]);
    let pict = format!(r"{base}(?:\x{{FE0F}}|{modifier})?");
    Regex::new(&format!(r"^(?:{ri}{{2}}|[#*0-9]\x{{FE0F}}?\x{{20E3}}|{pict}(?:[\x{{E0020}}-\x{{E007E}}]+\x{{E007F}})?(?:\x{{200D}}{pict})*)$"))
        .unwrap_or_else(|e| unreachable!("UNICODE_EMOJI_RE: {e}"))
});

/// `CUSTOM_EMOJI_RE`: `<a:name:id>` (names of 1-32 word characters).
static CUSTOM_EMOJI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^<(a)?:([A-Za-z0-9_]{1,32}):([0-9]{17,20})>$").unwrap_or_else(|e| unreachable!("{e}")));

fn graphemes_of(s: &str) -> Vec<&str> {
    s.graphemes(true).collect()
}

/// `normalizeEmojiKey(key)`: strips the variation selector so ❤ and ❤️ compare equal.
pub fn normalize_emoji_key(key: &str) -> String {
    key.replace('\u{FE0F}', "")
}

/// `emojiKeyOfStored(stored)`: the reaction key of a stored emoji (the custom emoji id, or the unicode string without
/// U+FE0F), or `None`.
pub fn emoji_key_of_stored(stored: Option<&str>) -> Option<String> {
    let s = stored.filter(|s| !s.is_empty())?;
    if let Some(m) = CUSTOM_EMOJI_RE.captures(s) {
        return Some(m[3].to_string());
    }
    Some(normalize_emoji_key(s)).filter(|k| !k.is_empty())
}

/// `isUnicodeEmoji(str)`: exactly one grapheme and a standard emoji.
pub fn is_unicode_emoji(s: &str) -> bool {
    !s.is_empty() && graphemes_of(s).len() == 1 && UNICODE_EMOJI_RE.is_match(s)
}

/// The emoji of a button or select option (`componentEmoji`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ComponentEmoji {
    Custom { id: String, name: String, animated: bool },
    Unicode { name: String },
}

impl ComponentEmoji {
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// The serenity form.
    pub fn to_reaction_type(&self) -> serenity::all::ReactionType {
        match self {
            ComponentEmoji::Custom { id, name, animated } => serenity::all::ReactionType::Custom {
                animated: *animated,
                id: serenity::all::EmojiId::new(id.parse::<u64>().unwrap_or(1).max(1)),
                name: Some(name.clone()),
            },
            ComponentEmoji::Unicode { name } => serenity::all::ReactionType::Unicode(name.clone()),
        }
    }
}

/// `componentEmoji(stored)`: the emoji in the form Discord accepts on buttons and select options, or `None` when the
/// stored string is not a valid emoji (legacy rows). Never pass a raw stored string to a component.
pub fn component_emoji(stored: Option<&str>) -> Option<ComponentEmoji> {
    let s = trim(stored?);
    if s.is_empty() {
        return None;
    }
    if let Some(m) = CUSTOM_EMOJI_RE.captures(s) {
        return Some(ComponentEmoji::Custom { id: m[3].to_string(), name: m[2].to_string(), animated: m.get(1).is_some() });
    }
    if is_unicode_emoji(s) {
        return Some(ComponentEmoji::Unicode { name: s.to_string() });
    }
    None
}

/// A custom emoji the bot can see (`client.emojis.cache` / `guild.emojis.cache`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmojiInfo {
    pub id: String,
    pub name: String,
    pub animated: bool,
    /// `emoji.available !== false`.
    pub available: bool,
    /// Limited to roles and the bot holds none of them.
    pub limited_for_bot: bool,
}

impl EmojiInfo {
    /// `emoji.toString()`.
    pub fn mention(&self) -> String {
        format!("<{}:{}:{}>", if self.animated { "a" } else { "" }, self.name, self.id)
    }
}

/// What [`parse_emoji_input`] looks emojis up in.
pub trait EmojiLookup {
    /// `guild.emojis.cache`.
    fn guild_emojis(&self) -> Vec<EmojiInfo>;
    /// `client.emojis.cache.get(id)`.
    fn client_emoji(&self, id: &str) -> Option<EmojiInfo>;
}

/// No emojis at all.
pub struct NoEmojis;

impl EmojiLookup for NoEmojis {
    fn guild_emojis(&self) -> Vec<EmojiInfo> {
        Vec::new()
    }
    fn client_emoji(&self, _id: &str) -> Option<EmojiInfo> {
        None
    }
}

/// The result of [`parse_emoji_input`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmojiInput {
    /// `{ ok: true, emoji, key }` (both `None` for an empty input).
    Ok { emoji: Option<String>, key: Option<String> },
    /// `{ ok: false, error }`.
    Err(String),
}

static SNOWFLAKE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9]{17,20}$").unwrap_or_else(|e| unreachable!("{e}")));
static COLON_NAME_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^:([A-Za-z0-9_]{2,32}):$").unwrap_or_else(|e| unreachable!("{e}")));
static NAME_LIKE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^:?[A-Za-z0-9_+\-]{1,40}:?$").unwrap_or_else(|e| unreachable!("{e}")));

fn find_emoji_error(name: &str) -> String {
    format!("I can't find a server emoji called :{name}:. For a standard emoji, pick it from the emoji picker instead of typing its name.")
}

/// `parseEmojiInput(input, { guild, client })`: validates and canonicalises an emoji typed by an admin. Never fails.
pub fn parse_emoji_input(input: Option<&str>, lookup: &dyn EmojiLookup) -> EmojiInput {
    let s = trim(input.unwrap_or_default());
    if s.is_empty() {
        return EmojiInput::Ok { emoji: None, key: None };
    }
    let mut id: Option<String> = None;
    if let Some(m) = CUSTOM_EMOJI_RE.captures(s) {
        id = Some(m[3].to_string());
    } else if SNOWFLAKE_RE.is_match(s) {
        id = Some(s.to_string());
    } else if let Some(n) = COLON_NAME_RE.captures(s) {
        let name = &n[1];
        let cached = lookup.guild_emojis();
        let found = cached.iter().find(|e| e.name == name).or_else(|| cached.iter().find(|e| e.name.to_lowercase() == name.to_lowercase()));
        match found {
            Some(e) => id = Some(e.id.clone()),
            None => return EmojiInput::Err(find_emoji_error(name)),
        }
    }

    if let Some(id) = id {
        let Some(e) = lookup.client_emoji(&id) else {
            return EmojiInput::Err("I can't use that emoji. Pick a standard emoji, or a custom emoji from a server I'm in.".into());
        };
        if !e.available {
            return EmojiInput::Err("That emoji is unavailable (its server may have lost a boost level).".into());
        }
        if e.limited_for_bot {
            return EmojiInput::Err("That emoji is limited to roles I don't have.".into());
        }
        return EmojiInput::Ok { emoji: Some(e.mention()), key: Some(e.id) };
    }

    if is_unicode_emoji(s) {
        return EmojiInput::Ok { emoji: Some(s.to_string()), key: Some(normalize_emoji_key(s)) };
    }
    let parts = graphemes_of(s);
    if parts.len() >= 2 && parts.iter().all(|g| is_unicode_emoji(g)) {
        return EmojiInput::Err("Use one emoji per role.".into());
    }
    if NAME_LIKE_RE.is_match(s) {
        return EmojiInput::Err(find_emoji_error(&s.replace(':', "")));
    }
    EmojiInput::Err(format!("\"{}\" is not an emoji.", clamp_text(s, 40)))
}

// ── Messages, names, errors ───────────────────────────────────────────────────

static MESSAGE_LINK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^<?https?://(?:(?:ptb|canary)\.)?discord(?:app)?\.com/channels/([0-9]{17,20})/([0-9]{17,20})/([0-9]{17,20})/?(?:\?\S*)?>?$")
        .unwrap_or_else(|e| unreachable!("{e}"))
});
static PAIR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([0-9]{17,20})-([0-9]{17,20})$").unwrap_or_else(|e| unreachable!("{e}")));

/// The result of [`parse_message_reference`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageRef {
    Ok { channel_id: String, message_id: String },
    Err(String),
}

/// `parseMessageReference(input, { guildId, channelId })`: a message link, a `channelId-messageId` pair (Shift + Copy
/// ID) or a bare message id of the current channel.
pub fn parse_message_reference(input: &str, guild_id: &str, channel_id: &str) -> MessageRef {
    let s = trim(input);
    if let Some(link) = MESSAGE_LINK_RE.captures(s) {
        if &link[1] != guild_id {
            return MessageRef::Err("That message is in a different server.".into());
        }
        return MessageRef::Ok { channel_id: link[2].to_string(), message_id: link[3].to_string() };
    }
    if let Some(pair) = PAIR_RE.captures(s) {
        return MessageRef::Ok { channel_id: pair[1].to_string(), message_id: pair[2].to_string() };
    }
    if SNOWFLAKE_RE.is_match(s) {
        return MessageRef::Ok { channel_id: channel_id.to_string(), message_id: s.to_string() };
    }
    MessageRef::Err("Give a message link (right-click the message → Copy Message Link), or the ID of a message in this channel.".into())
}

/// `defaultAttachTitle(message, channelName)`: its first text line, its first embed title, or the channel name. Never
/// empty, at most 100.
pub fn default_attach_title(content: &str, first_embed_title: Option<&str>, channel_name: &str) -> String {
    let mut title: Option<String> = None;
    for line in content.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let stripped: String = line.chars().filter(|c| !matches!(c, '*' | '_' | '~' | '`' | '>' | '|' | '#')).collect();
        let t = trim(&stripped);
        if !t.is_empty() {
            title = Some(t.to_string());
            break;
        }
    }
    let title = title.or_else(|| first_embed_title.filter(|t| !t.is_empty()).map(str::to_string));
    let out = clamp_text(&title.unwrap_or_else(|| format!("Message in #{channel_name}")), 100);
    if out.is_empty() { "Attached message".to_string() } else { out }
}

/// `panelChoiceName({ title, layout, itemCount })`: the autocomplete name of a panel, at most 100 characters.
pub fn panel_choice_name(title: &str, layout: &str, item_count: i64) -> String {
    let suffix = format!(" · {} · {item_count} role(s)", layout_label(layout).unwrap_or(layout));
    let room = 100usize.saturating_sub(utf16_len(&suffix));
    format!("{}{suffix}", clamp_text(title, room))
}

/// `itemChoiceName(item, guild)`: the autocomplete name of an item; `role_name` is the cached role's name.
pub fn item_choice_name(label: &str, emoji: Option<&str>, role_name: Option<&str>) -> String {
    let prefix = match emoji {
        Some(e) if is_unicode_emoji(e) => format!("{e} "),
        _ => String::new(),
    };
    let who = match role_name {
        Some(n) => format!("@{n}"),
        None => "deleted role".to_string(),
    };
    clamp_text(&format!("{prefix}{label} · {who}"), 100)
}

static ROLE_MENTION_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^<@&([0-9]{17,20})>$").unwrap_or_else(|e| unreachable!("{e}")));

/// `resolveItem(group, input)`: by item id, by role id or `<@&id>`, then by label (case-insensitive).
pub fn resolve_item<'a>(items: &'a [bot_db::ReactionRoleItem], input: &str) -> Option<&'a bot_db::ReactionRoleItem> {
    let s = trim(input);
    if s.is_empty() {
        return None;
    }
    if let Some(i) = items.iter().find(|i| i.id == s) {
        return Some(i);
    }
    let role = ROLE_MENTION_RE.captures(s).map(|m| m[1].to_string()).or_else(|| SNOWFLAKE_RE.is_match(s).then(|| s.to_string()));
    if let Some(role) = role
        && let Some(i) = items.iter().find(|i| i.role_id == role)
    {
        return Some(i);
    }
    let lower = s.to_lowercase();
    items.iter().find(|i| trim(&i.label).to_lowercase() == lower)
}

/// `describeDiscordError(err)`: lowercase, no final period, so callers can embed it in a sentence.
pub fn describe_discord_error(code: Option<i64>) -> String {
    let text = match code {
        Some(10003) => "the channel no longer exists",
        Some(10008) => "the message no longer exists",
        Some(10011) => "that role no longer exists",
        Some(10014) => "I can't use that emoji",
        Some(30010) => "that message already has 20 different reactions",
        Some(50001) => "I can't see that channel",
        Some(50005) => "I can't edit a message someone else sent",
        Some(50013) => "I'm missing permissions there",
        Some(50035) => "Discord rejected the panel content",
        Some(50083) => "the thread is archived",
        Some(c) => return format!("Discord error {c}"),
        None => return "Discord error unknown".to_string(),
    };
    text.to_string()
}

// ── Payload builders ──────────────────────────────────────────────────────────

/// One item, as the payload builders read it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemView {
    pub custom_id: String,
    pub role_id: String,
    pub label: String,
    #[serde(default)]
    pub emoji: Option<String>,
    pub style: i32,
    pub position: i32,
    #[serde(default)]
    pub description: Option<String>,
}

impl From<&bot_db::ReactionRoleItem> for ItemView {
    fn from(i: &bot_db::ReactionRoleItem) -> Self {
        Self {
            custom_id: i.custom_id.clone(),
            role_id: i.role_id.clone(),
            label: i.label.clone(),
            emoji: i.emoji.clone(),
            style: i.style,
            position: i.position,
            description: i.description.clone(),
        }
    }
}

/// A panel, as the payload builders read it (a stored row or a draft).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PanelView {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    pub layout: String,
    #[serde(default)]
    pub max_roles: Option<i32>,
    #[serde(default)]
    pub plain_text: bool,
    #[serde(default)]
    pub message_content: Option<String>,
    #[serde(default)]
    pub placeholder: Option<String>,
    #[serde(default)]
    pub items: Vec<ItemView>,
}

/// `panelFooter(group)`: the footer line of a panel for its mode, layout and role cap.
pub fn panel_footer(kind: &str, layout: &str, max_roles: Option<i32>) -> String {
    let per_layout = |buttons: &'static str, select: &'static str, reactions: &'static str| match layout {
        "SELECT" => select,
        "REACTIONS" => reactions,
        _ => buttons,
    };
    let normal = || {
        per_layout(
            "Press a button again to remove its role",
            "Your selection replaces your roles from this panel",
            "Remove your reaction to remove the role",
        )
    };
    let mut text = match kind {
        "UNIQUE" => "You can have one role from this panel".to_string(),
        "VERIFY" => "Roles from this panel can't be removed here".to_string(),
        "REVERSE" => per_layout(
            "Pressing a button removes that role",
            "Picking a role removes it",
            "Reacting removes the role; remove your reaction to get it back",
        )
        .to_string(),
        _ => normal().to_string(),
    };
    if kind == "NORMAL"
        && let Some(m) = max_roles.filter(|m| *m != 0)
    {
        text.push_str(&format!(" · Max {m} role(s)"));
    }
    text
}

fn sorted_items(items: &[ItemView]) -> Vec<ItemView> {
    let mut v = items.to_vec();
    v.sort_by_key(|i| i.position);
    v
}

/// One select option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectOption {
    pub label: String,
    pub value: String,
    pub default: bool,
    pub emoji: Option<ComponentEmoji>,
    pub description: Option<String>,
}

/// A string select menu in its own row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectRow {
    pub custom_id: String,
    pub placeholder: String,
    pub min_values: u8,
    pub max_values: u8,
    pub options: Vec<SelectOption>,
}

/// One button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ButtonSpec {
    pub custom_id: String,
    pub label: String,
    pub style: i32,
    pub emoji: Option<ComponentEmoji>,
}

/// An action row of a panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Buttons(Vec<ButtonSpec>),
    Select(SelectRow),
}

impl Row {
    /// discord.js `row.toJSON()`.
    pub fn to_json(&self) -> Value {
        match self {
            Row::Buttons(buttons) => json!({
                "type": 1,
                "components": buttons.iter().map(|b| {
                    let mut o = json!({ "type": 2, "custom_id": b.custom_id, "label": b.label, "style": b.style });
                    if let Some(e) = &b.emoji { o["emoji"] = e.to_json(); }
                    o
                }).collect::<Vec<_>>(),
            }),
            Row::Select(s) => {
                let options: Vec<Value> = s
                    .options
                    .iter()
                    .map(|o| {
                        let mut v = json!({ "label": o.label, "value": o.value, "default": o.default });
                        if let Some(e) = &o.emoji {
                            v["emoji"] = e.to_json();
                        }
                        if let Some(d) = &o.description {
                            v["description"] = json!(d);
                        }
                        v
                    })
                    .collect();
                json!({
                    "type": 1,
                    "components": [{
                        "type": 3, "custom_id": s.custom_id, "placeholder": s.placeholder,
                        "min_values": s.min_values, "max_values": s.max_values, "options": options,
                    }],
                })
            }
        }
    }

    /// The serenity builder.
    pub fn to_builder(&self) -> serenity::builder::CreateActionRow {
        use serenity::builder::{CreateActionRow, CreateButton, CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption};
        match self {
            Row::Buttons(buttons) => CreateActionRow::Buttons(
                buttons
                    .iter()
                    .map(|b| {
                        let style = match b.style {
                            2 => serenity::all::ButtonStyle::Secondary,
                            3 => serenity::all::ButtonStyle::Success,
                            4 => serenity::all::ButtonStyle::Danger,
                            _ => serenity::all::ButtonStyle::Primary,
                        };
                        let mut btn = CreateButton::new(b.custom_id.clone()).label(b.label.clone()).style(style);
                        if let Some(e) = &b.emoji {
                            btn = btn.emoji(e.to_reaction_type());
                        }
                        btn
                    })
                    .collect(),
            ),
            Row::Select(s) => {
                let options = s
                    .options
                    .iter()
                    .map(|o| {
                        let mut opt = CreateSelectMenuOption::new(o.label.clone(), o.value.clone()).default_selection(o.default);
                        if let Some(e) = &o.emoji {
                            opt = opt.emoji(e.to_reaction_type());
                        }
                        if let Some(d) = &o.description {
                            opt = opt.description(d.clone());
                        }
                        opt
                    })
                    .collect();
                CreateActionRow::SelectMenu(
                    CreateSelectMenu::new(s.custom_id.clone(), CreateSelectMenuKind::String { options })
                        .placeholder(s.placeholder.clone())
                        .min_values(s.min_values)
                        .max_values(s.max_values),
                )
            }
        }
    }
}

/// `buildCustomId('rr-select', groupId)`.
pub fn select_custom_id(group_id: &str) -> String {
    bot_config::custom_id!("rr-select", group_id).unwrap_or_else(|_| format!("rr-select:{group_id}"))
}

/// `buildSelectRow(group, { selectedRoleIds, placeholder })`: the select menu row of a dropdown panel, or `None`
/// without items.
pub fn build_select_row(group: &PanelView, selected_role_ids: Option<&[String]>, placeholder: Option<&str>) -> Option<SelectRow> {
    let items: Vec<ItemView> = sorted_items(&group.items).into_iter().take(25).collect();
    if items.is_empty() {
        return None;
    }
    let max = if group.kind == "UNIQUE" {
        1
    } else {
        let cap = match group.max_roles.filter(|m| *m != 0) {
            Some(m) if group.kind == "NORMAL" => i64::from(m),
            _ => 25,
        };
        (items.len() as i64).min(cap)
    };
    // Discord rejects more defaults than max_values; legacy data could hold more roles than the cap.
    let selected: HashSet<String> = selected_role_ids
        .unwrap_or_default()
        .iter()
        .filter(|id| items.iter().any(|i| &i.role_id == *id))
        .take(max.max(0) as usize)
        .cloned()
        .collect();
    let placeholder = placeholder
        .map(str::to_string)
        .or_else(|| group.placeholder.clone())
        .unwrap_or_else(|| placeholder_for(&group.kind).unwrap_or("Choose the roles you want").to_string());
    let options = items
        .iter()
        .map(|item| {
            let label = clamp_text(&item.label, 100);
            SelectOption {
                label: if label.is_empty() { "—".into() } else { label },
                value: item.role_id.clone(),
                default: selected.contains(&item.role_id),
                emoji: component_emoji(item.emoji.as_deref()),
                description: item.description.as_deref().filter(|d| !d.is_empty()).map(|d| clamp_text(d, 100)),
            }
        })
        .collect();
    Some(SelectRow {
        custom_id: select_custom_id(&group.id),
        placeholder: clamp_text(&placeholder, 150),
        min_values: 0,
        max_values: max.clamp(0, 25) as u8,
        options,
    })
}

/// The message payload of a panel (`buildPanelPayload`).
#[derive(Debug, Clone, PartialEq)]
pub struct PanelPayload {
    pub content: Option<String>,
    /// discord.js `EmbedBuilder.toJSON()` of each embed.
    pub embeds: Vec<Value>,
    pub components: Vec<Row>,
}

impl PanelPayload {
    /// The payload as the JS returned it, serialised (`allowedMentions: { parse: [] }`).
    pub fn to_json(&self) -> Value {
        json!({
            "content": self.content,
            "embeds": self.embeds,
            "components": self.components.iter().map(Row::to_json).collect::<Vec<_>>(),
            "allowedMentions": { "parse": [] },
        })
    }

    /// The REST body discord.js sends for `channel.send(payload)` / `message.edit(payload)` (a `null` content
    /// becomes `""`).
    pub fn to_api_body(&self) -> Value {
        json!({
            "content": self.content.clone().unwrap_or_default(),
            "embeds": self.embeds,
            "components": self.components.iter().map(Row::to_json).collect::<Vec<_>>(),
            "allowed_mentions": { "parse": [] },
        })
    }
}

/// Whether the JS `buildPanelPayload` throws for this panel: an embed panel whose body is empty (a stored description of
/// `''` and no listing, i.e. a button or dropdown panel with items) makes discord.js's `EmbedBuilder.setDescription('')`
/// throw a validation error without a numeric code. The callers then fail with "Discord error unknown" before
/// anything is sent, as the JS does.
pub fn panel_payload_throws(group: &PanelView) -> bool {
    !group.plain_text && group.description.as_deref() == Some("") && !group.items.is_empty() && group.layout != "REACTIONS"
}

/// `buildPanelPayload(group, { color })`. Pure; never fails for stored data (bad emojis are dropped, text is clamped).
pub fn build_panel_payload(group: &PanelView, color: Option<i64>) -> PanelPayload {
    let items = sorted_items(&group.items);
    let listing: Option<String> = if items.is_empty() {
        Some("*No roles have been added to this panel yet.*".to_string())
    } else if group.layout == "REACTIONS" {
        Some(
            items
                .iter()
                .map(|i| {
                    let e = if component_emoji(i.emoji.as_deref()).is_some() { i.emoji.clone().unwrap_or_default() } else { "▫️".into() };
                    format!("{e} — **{}**", i.label)
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )
    } else {
        None
    };
    let description = match &group.description {
        Some(d) => Some(d.clone()),
        None => Some(default_description(&group.layout).unwrap_or("Press a button below to manage your roles.").to_string()),
    };
    let body = [description, listing.clone()].into_iter().flatten().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n");
    let footer = panel_footer(&group.kind, &group.layout, group.max_roles);
    let title = {
        let t = clamp_text(&group.title, 256);
        if t.is_empty() { DEFAULT_TITLE.to_string() } else { t }
    };
    let message_content = group.message_content.clone().filter(|m| !m.is_empty());

    let (content, embeds) = if !group.plain_text {
        let color = color.filter(|c| (0..=0xff_ffff).contains(c)).unwrap_or(i64::from(bot_config::constants::colors::INFO));
        let embed = json!({
            "color": color,
            "title": title,
            "description": clamp_text(&body, 4096),
            "footer": { "text": clamp_text(&footer, 2048) },
        });
        (message_content.map(|m| clamp_text(&m, 2000)), vec![embed])
    } else {
        let content = match message_content {
            Some(m) => {
                let parts = [Some(m), listing, Some(format!("-# {footer}"))];
                clamp_text(&parts.into_iter().flatten().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n"), 2000)
            }
            None => {
                let parts = [format!("**{title}**"), body, format!("-# {footer}")];
                clamp_text(&parts.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n"), 2000)
            }
        };
        (Some(content), Vec::new())
    };

    let mut components = Vec::new();
    if !items.is_empty() {
        if group.layout == "BUTTONS" {
            let shown: Vec<&ItemView> = items.iter().take(25).collect();
            for chunk in shown.chunks(5) {
                components.push(Row::Buttons(
                    chunk
                        .iter()
                        .map(|item| {
                            let label = clamp_text(&item.label, 80);
                            ButtonSpec {
                                custom_id: item.custom_id.clone(),
                                label: if label.is_empty() { "—".into() } else { label },
                                style: if (1..=4).contains(&item.style) { item.style } else { 1 },
                                emoji: component_emoji(item.emoji.as_deref()),
                            }
                        })
                        .collect(),
                ));
            }
        } else if group.layout == "SELECT"
            && let Some(row) = build_select_row(group, None, None)
        {
            components.push(Row::Select(row));
        }
    }
    PanelPayload { content, embeds, components }
}

/// A new item of a panel (`draftItem(groupId, item, position)`): its customId is always `rr:{groupId}:{roleId}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftItem {
    pub custom_id: String,
    pub role_id: String,
    pub label: String,
    pub emoji: Option<String>,
    pub style: i32,
    pub description: Option<String>,
    pub position: i32,
}

/// The fields of a new item.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewItem {
    pub role_id: String,
    pub label: String,
    pub emoji: Option<String>,
    /// Default 1.
    pub style: Option<i32>,
    pub description: Option<String>,
}

/// `buildCustomId('rr', groupId, roleId)`.
pub fn button_custom_id(group_id: &str, role_id: &str) -> anyhow::Result<String> {
    Ok(bot_config::custom_id!("rr", group_id, role_id)?)
}

/// `draftItem(groupId, { roleId, label, emoji, style, description }, position)`.
pub fn draft_item(group_id: &str, item: &NewItem, position: i32) -> anyhow::Result<DraftItem> {
    Ok(DraftItem {
        custom_id: button_custom_id(group_id, &item.role_id)?,
        role_id: item.role_id.clone(),
        label: item.label.clone(),
        emoji: item.emoji.clone(),
        style: item.style.unwrap_or(1),
        description: item.description.clone(),
        position,
    })
}

impl From<&DraftItem> for ItemView {
    fn from(d: &DraftItem) -> Self {
        Self {
            custom_id: d.custom_id.clone(),
            role_id: d.role_id.clone(),
            label: d.label.clone(),
            emoji: d.emoji.clone(),
            style: d.style,
            position: d.position,
            description: d.description.clone(),
        }
    }
}

/// One item of the panel index (`{ roleId, emoji, key }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexItem {
    pub role_id: String,
    pub emoji: Option<String>,
    pub key: Option<String>,
}

/// `indexItems(group)`: the items as `{ roleId, emoji, key }`.
pub fn index_items(items: &[bot_db::ReactionRoleItem]) -> Vec<IndexItem> {
    items
        .iter()
        .map(|i| IndexItem { role_id: i.role_id.clone(), emoji: i.emoji.clone(), key: emoji_key_of_stored(i.emoji.as_deref()) })
        .collect()
}

// ── Role-change planner ───────────────────────────────────────────────────────

/// What triggered a role change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Button,
    Select,
    React,
    Unreact,
}

/// Why a plan changes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanReason {
    Max,
    Already,
    NotHeld,
    NoChange,
}

impl PlanReason {
    /// The JS string.
    pub fn as_str(self) -> &'static str {
        match self {
            PlanReason::Max => "max",
            PlanReason::Already => "already",
            PlanReason::NotHeld => "not-held",
            PlanReason::NoChange => "no-change",
        }
    }
}

/// What to change for one member.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    pub to_add: Vec<String>,
    pub to_remove: Vec<String>,
    pub skipped: Vec<String>,
    pub reason: Option<PlanReason>,
}

/// `planRoleChange({ type, source, panelRoleIds, heldRoleIds, targetRoleIds, maxRoles })`: what to change for one
/// member, for one click, selection or reaction. Held roles must come from a member read inside the member lock.
pub fn plan_role_change(kind: &str, source: Source, panel_role_ids: &[String], held_role_ids: &[String], target_role_ids: &[String], max_roles: Option<i32>) -> Plan {
    let mut p_ids: Vec<String> = Vec::new();
    for id in panel_role_ids {
        if !p_ids.contains(id) {
            p_ids.push(id.clone());
        }
    }
    let held: HashSet<&String> = held_role_ids.iter().collect();
    let want: HashSet<&String> = target_role_ids.iter().collect();
    let h: Vec<String> = p_ids.iter().filter(|id| held.contains(id)).cloned().collect();
    let t: Vec<String> = p_ids.iter().filter(|id| want.contains(id)).cloned().collect();
    let max: Option<usize> = if kind == "NORMAL" { max_roles.filter(|m| *m != 0).map(|m| m.max(0) as usize) } else { None };
    let mut plan = Plan::default();
    let done = |mut plan: Plan, reason: Option<PlanReason>| {
        plan.reason = reason;
        if plan.reason.is_none() && plan.to_add.is_empty() && plan.to_remove.is_empty() {
            plan.reason = Some(PlanReason::NoChange);
        }
        plan
    };

    if source != Source::Select && t.is_empty() {
        return done(plan, Some(PlanReason::NoChange));
    }

    if source == Source::Select {
        return match kind {
            "NORMAL" => {
                let keep: Vec<&String> = h.iter().filter(|id| t.contains(id)).collect();
                let adds: Vec<String> = t.iter().filter(|id| !held.contains(id)).cloned().collect();
                plan.to_remove = h.iter().filter(|id| !t.contains(id)).cloned().collect();
                match max {
                    Some(m) if keep.len() + adds.len() > m => {
                        let room = m.saturating_sub(keep.len());
                        plan.to_add = adds[..room.min(adds.len())].to_vec();
                        plan.skipped = adds[room.min(adds.len())..].to_vec();
                    }
                    _ => plan.to_add = adds,
                }
                // Nothing changes but part of the choice was refused by the cap: say so instead of "no changes".
                if plan.to_add.is_empty() && plan.to_remove.is_empty() && !plan.skipped.is_empty() {
                    return done(plan, Some(PlanReason::Max));
                }
                done(plan, None)
            }
            "UNIQUE" => {
                if t.is_empty() {
                    plan.to_remove = h;
                    return done(plan, None);
                }
                let first = t[0].clone();
                if !held.contains(&first) {
                    plan.to_add = vec![first.clone()];
                }
                plan.to_remove = h.into_iter().filter(|id| *id != first).collect();
                done(plan, None)
            }
            "VERIFY" => {
                if t.is_empty() {
                    return done(plan, Some(PlanReason::NoChange));
                }
                plan.to_add = t.iter().filter(|id| !held.contains(id)).cloned().collect();
                let r = if plan.to_add.is_empty() { Some(PlanReason::Already) } else { None };
                done(plan, r)
            }
            "REVERSE" => {
                if t.is_empty() {
                    return done(plan, Some(PlanReason::NoChange));
                }
                plan.to_remove = t.iter().filter(|id| held.contains(id)).cloned().collect();
                let r = if plan.to_remove.is_empty() { Some(PlanReason::NotHeld) } else { None };
                done(plan, r)
            }
            _ => done(plan, Some(PlanReason::NoChange)),
        };
    }

    let r = t[0].clone();
    let has = held.contains(&r);
    match kind {
        "NORMAL" => match source {
            Source::Button => {
                if has {
                    plan.to_remove = vec![r];
                    return done(plan, None);
                }
                if max.is_some_and(|m| h.len() >= m) {
                    return done(plan, Some(PlanReason::Max));
                }
                plan.to_add = vec![r];
                done(plan, None)
            }
            Source::React => {
                if has {
                    return done(plan, Some(PlanReason::NoChange));
                }
                if max.is_some_and(|m| h.len() >= m) {
                    return done(plan, Some(PlanReason::Max));
                }
                plan.to_add = vec![r];
                done(plan, None)
            }
            _ => {
                if has {
                    plan.to_remove = vec![r];
                }
                done(plan, None)
            }
        },
        "UNIQUE" => match source {
            Source::Button => {
                if has {
                    plan.to_remove = h;
                    return done(plan, None);
                }
                plan.to_add = vec![r.clone()];
                plan.to_remove = h.into_iter().filter(|id| *id != r).collect();
                done(plan, None)
            }
            Source::React => {
                if !has {
                    plan.to_add = vec![r.clone()];
                }
                plan.to_remove = h.into_iter().filter(|id| *id != r).collect();
                done(plan, None)
            }
            _ => {
                if has {
                    plan.to_remove = vec![r];
                }
                done(plan, None)
            }
        },
        "VERIFY" => match source {
            Source::Button => {
                if has {
                    return done(plan, Some(PlanReason::Already));
                }
                plan.to_add = vec![r];
                done(plan, None)
            }
            Source::React => {
                if !has {
                    plan.to_add = vec![r];
                }
                done(plan, None)
            }
            // unreact never removes
            _ => done(plan, None),
        },
        "REVERSE" => match source {
            Source::Button => {
                if has {
                    plan.to_remove = vec![r];
                    return done(plan, None);
                }
                done(plan, Some(PlanReason::NotHeld))
            }
            Source::React => {
                if has {
                    plan.to_remove = vec![r];
                }
                done(plan, None)
            }
            _ => {
                // unreact: the role comes back
                if !has {
                    plan.to_add = vec![r];
                }
                done(plan, None)
            }
        },
        _ => done(plan, Some(PlanReason::NoChange)),
    }
}

/// `orderItems(items, order)`: `items` sorted by their role's place in `order`; roles not in it go last, in their given
/// order (a stable sort).
pub fn order_items<T: Clone>(items: &[T], order: &[String], role_of: impl Fn(&T) -> &str) -> Vec<T> {
    let rank = |r: &str| order.iter().position(|o| o == r).unwrap_or(order.len());
    let mut v: Vec<(usize, &T)> = items.iter().enumerate().collect();
    v.sort_by(|a, b| rank(role_of(a.1)).cmp(&rank(role_of(b.1))).then(a.0.cmp(&b.0)));
    v.into_iter().map(|(_, t)| t.clone()).collect()
}

/// `mentions(ids)`: role mentions joined, at most 900 characters, never cutting one.
pub fn mentions(ids: &[String]) -> String {
    let m: Vec<String> = ids.iter().map(|id| format!("<@&{id}>")).collect();
    bot_util::text::join_with_limit(&m, ", ", 900, bot_util::text::default_more)
}

/// `roleReason(group)`: the audit-log reason of a role change.
pub fn role_reason(title: &str) -> String {
    clamp_text(&format!("Reaction role: {title}"), 512)
}
