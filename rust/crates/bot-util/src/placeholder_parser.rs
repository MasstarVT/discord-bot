//! Port of `src/utils/placeholderParser.js`: the one placeholder engine for every message (welcome, tags, templates,
//! feeds): {user.name}, {server.member_count}, {channel}, {random:1:100}, argument tokens {1} {1:default} {2...}, and
//! feature variables. Replacement is a single pass, so substituted values are never re-scanned. The JS reads
//! discord.js objects; here the caller (bot-core) fills plain structs with exactly the values the JS reads.

use crate::js;
use crate::jsre::{JsMatch, LazyRe};
use crate::lazy_re;
use crate::logger;
use crate::text::{clamp_text, ordinal};
use std::collections::{BTreeSet, HashMap};

/// What the placeholders read from a user (`discord.js` User).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlaceholderUser {
    pub id: String,
    pub username: Option<String>,
    /// `user.tag` when the library provides it as a string.
    pub tag: Option<String>,
    pub discriminator: Option<String>,
    pub global_name: Option<String>,
    pub bot: bool,
    /// Epoch ms.
    pub created_timestamp: Option<f64>,
    /// `displayAvatarURL({ extension: 'png', size: 256 })`.
    pub avatar_url: Option<String>,
}

/// What the placeholders read from a guild.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlaceholderGuild {
    pub id: String,
    pub name: Option<String>,
    pub owner_id: Option<String>,
    /// `Number(memberCount)`; None is 0.
    pub member_count: Option<f64>,
    /// `String(verificationLevel)`.
    pub verification_level: Option<String>,
    /// When the bot joined (epoch ms).
    pub joined_timestamp: Option<f64>,
    pub created_timestamp: Option<f64>,
    /// `String(premiumSubscriptionCount)`; None is '0'.
    pub premium_subscription_count: Option<String>,
    /// `String(premiumTier)`; None is '0'.
    pub premium_tier: Option<String>,
    /// `iconURL({ extension: 'png', size: 256 })`.
    pub icon_url: Option<String>,
}

/// What the placeholders read from a guild member.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlaceholderMember {
    pub display_name: Option<String>,
    pub joined_timestamp: Option<f64>,
    /// The member's `displayAvatarURL({ extension: 'png', size: 256 })` (server avatar first).
    pub avatar_url: Option<String>,
    pub user: Option<PlaceholderUser>,
    pub guild: Option<PlaceholderGuild>,
}

/// What the placeholders read from a channel.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlaceholderChannel {
    pub id: String,
    pub name: Option<String>,
    /// Discord's numeric channel type.
    pub kind: Option<i64>,
}

/// A feature variable (`ctx.vars`).
pub enum VarValue {
    Text(String),
    Number(f64),
    Bool(bool),
    /// null or undefined: renders as ''.
    Null,
    /// Called when the token is rendered; None renders as '', an Err is logged and renders as ''.
    Lazy(Box<dyn Fn() -> Result<Option<String>, String> + Send + Sync>),
}

impl std::fmt::Debug for VarValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(s) => write!(f, "Text({s:?})"),
            Self::Number(n) => write!(f, "Number({n})"),
            Self::Bool(b) => write!(f, "Bool({b})"),
            Self::Null => write!(f, "Null"),
            Self::Lazy(_) => write!(f, "Lazy"),
        }
    }
}

impl From<&str> for VarValue {
    fn from(s: &str) -> Self {
        Self::Text(s.to_string())
    }
}
impl From<String> for VarValue {
    fn from(s: String) -> Self {
        Self::Text(s)
    }
}
impl From<f64> for VarValue {
    fn from(n: f64) -> Self {
        Self::Number(n)
    }
}
impl From<i64> for VarValue {
    fn from(n: i64) -> Self {
        Self::Number(n as f64)
    }
}
impl From<bool> for VarValue {
    fn from(b: bool) -> Self {
        Self::Bool(b)
    }
}

/// The render context (`RenderContext`).
#[derive(Debug, Default)]
pub struct RenderContext {
    pub member: Option<PlaceholderMember>,
    pub user: Option<PlaceholderUser>,
    pub guild: Option<PlaceholderGuild>,
    pub channel: Option<PlaceholderChannel>,
    /// Feature variables, in the order given (a later key wins over an earlier one with the same normalised name).
    pub vars: Option<Vec<(String, VarValue)>>,
    /// Command arguments; argument tokens are read only when this is set.
    pub args: Option<Vec<String>>,
}

/// The result of [`render_placeholders_detailed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub text: String,
    /// Argument numbers that were asked for but not given (sorted).
    pub missing_args: Vec<u32>,
    /// Tokens left untouched because nothing knows them (with braces, original case, unique, at most 50).
    pub unknown_tokens: Vec<String>,
}

static TOKEN_RE: LazyRe = lazy_re!(r"\{([^{}\n]{1,64})\}", "");
const MAX_UNKNOWN_TOKENS: usize = 50;

fn stamp(ms: Option<f64>, style: char) -> String {
    match ms {
        Some(v) if v.is_finite() => format!("<t:{}:{style}>", js::number_to_string((v / 1000.0).floor())),
        _ => String::new(),
    }
}

/// A zero-width space after '<' keeps free text as typed but no longer a mention, role or channel tag.
fn unmention(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        out.push(c);
        if c == '<' && matches!(it.peek(), Some('@' | '#')) {
            out.push('\u{200b}');
        }
    }
    out
}

fn channel_type_name(kind: Option<i64>) -> &'static str {
    match kind {
        Some(0) => "text",
        Some(2) => "voice",
        Some(4) => "category",
        Some(5) => "announcement",
        Some(13) => "stage",
        Some(15) => "forum",
        Some(16) => "media",
        Some(10..=12) => "thread",
        _ => "other",
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ns {
    User,
    Server,
    Channel,
}

struct Objects<'a> {
    u: Option<&'a PlaceholderUser>,
    m: Option<&'a PlaceholderMember>,
    g: Option<&'a PlaceholderGuild>,
    c: Option<&'a PlaceholderChannel>,
}

// (aliases, namespace, description)
const BUILTINS: &[(&[&str], Ns, &str)] = &[
    (&["{user}", "{user.mention}"], Ns::User, "Mention of the member"),
    (&["{user.name}", "{user.username}"], Ns::User, "Username"),
    (&["{user.idname}", "{user.tag}"], Ns::User, "User tag (the username when there is no discriminator)"),
    (&["{user.id}"], Ns::User, "User ID"),
    (&["{user.avatar}", "{user.avatar_url}"], Ns::User, "Avatar URL"),
    (&["{user.bot}"], Ns::User, "True or False"),
    (&["{user.display_name}", "{user.nick}", "{user.nickname}"], Ns::User, "Server nickname, falling back to the display name"),
    (&["{user.created}"], Ns::User, "Account age, relative"),
    (&["{user.created_at}"], Ns::User, "Account creation date"),
    (&["{user.joined}"], Ns::User, "Time since joining the server, relative"),
    (&["{user.joined_at}"], Ns::User, "Date the member joined the server"),
    (&["{server}", "{server.name}"], Ns::Server, "Server name"),
    (&["{server.id}"], Ns::Server, "Server ID"),
    (&["{server.icon}", "{server.icon_url}"], Ns::Server, "Server icon URL"),
    (&["{server.owner}"], Ns::Server, "Mention of the server owner"),
    (&["{server.owner_id}"], Ns::Server, "ID of the server owner"),
    (&["{server.member_count}"], Ns::Server, "Member count, e.g. 1,234"),
    (&["{server.member_count.ordinal}"], Ns::Server, "Member count as an ordinal, e.g. 1,234th"),
    (&["{server.verification_level}"], Ns::Server, "Verification level (0-4)"),
    (&["{server.joined_at}"], Ns::Server, "Date the bot joined the server"),
    (&["{server.created_at}"], Ns::Server, "Date the server was created"),
    (&["{server.boost_count}"], Ns::Server, "Number of boosts"),
    (&["{server.boost_level}"], Ns::Server, "Boost tier"),
    (&["{channel}", "{channel.mention}"], Ns::Channel, "Mention of the channel"),
    (&["{channel.name}"], Ns::Channel, "Channel name"),
    (&["{channel.id}"], Ns::Channel, "Channel ID"),
    (&["{channel.type}"], Ns::Channel, "Channel type: text, voice, category, announcement, stage, forum, media, thread or other"),
];

fn builtin_ns(name: &str) -> Option<Ns> {
    BUILTINS.iter().find(|(aliases, _, _)| aliases.iter().any(|a| &a[1..a.len() - 1] == name)).map(|(_, ns, _)| *ns)
}

// The getter of each token (by its first alias).
fn builtin_value(name: &str, o: &Objects<'_>) -> String {
    let u = o.u;
    let g = o.g;
    let c = o.c;
    let m = o.m;
    let us = |f: fn(&PlaceholderUser) -> String| u.map(f).unwrap_or_default();
    match name {
        "user" | "user.mention" => us(|u| format!("<@{}>", u.id)),
        "user.name" | "user.username" => us(|u| u.username.clone().unwrap_or_default()),
        "user.idname" | "user.tag" => us(|u| match &u.tag {
            Some(t) => t.clone(),
            None => match u.discriminator.as_deref() {
                Some(d) if !d.is_empty() && d != "0" => format!("{}#{d}", u.username.as_deref().unwrap_or("undefined")),
                _ => u.username.clone().unwrap_or_default(),
            },
        }),
        "user.id" => us(|u| u.id.clone()),
        "user.avatar" | "user.avatar_url" => match m {
            Some(m) => m.avatar_url.clone().unwrap_or_default(),
            None => us(|u| u.avatar_url.clone().unwrap_or_default()),
        },
        "user.bot" => us(|u| if u.bot { "True".into() } else { "False".into() }),
        "user.display_name" | "user.nick" | "user.nickname" => {
            let name = m
                .and_then(|m| m.display_name.clone())
                .or_else(|| u.and_then(|u| u.global_name.clone()))
                .or_else(|| u.and_then(|u| u.username.clone()))
                .unwrap_or_default();
            unmention(&name)
        }
        "user.created" => stamp(u.and_then(|u| u.created_timestamp), 'R'),
        "user.created_at" => stamp(u.and_then(|u| u.created_timestamp), 'D'),
        "user.joined" => stamp(m.and_then(|m| m.joined_timestamp), 'R'),
        "user.joined_at" => stamp(m.and_then(|m| m.joined_timestamp), 'D'),
        "server" | "server.name" => unmention(&g.and_then(|g| g.name.clone()).unwrap_or_default()),
        "server.id" => g.map(|g| g.id.clone()).unwrap_or_default(),
        "server.icon" | "server.icon_url" => g.and_then(|g| g.icon_url.clone()).unwrap_or_default(),
        "server.owner" => match g.and_then(|g| g.owner_id.as_deref()) {
            Some(id) if !id.is_empty() => format!("<@{id}>"),
            _ => String::new(),
        },
        "server.owner_id" => g.and_then(|g| g.owner_id.clone()).unwrap_or_default(),
        "server.member_count" => js::to_locale_en_us(g.and_then(|g| g.member_count).unwrap_or(0.0), 3),
        "server.member_count.ordinal" => ordinal(g.and_then(|g| g.member_count).unwrap_or(0.0)),
        "server.verification_level" => g.and_then(|g| g.verification_level.clone()).unwrap_or_default(),
        "server.joined_at" => stamp(g.and_then(|g| g.joined_timestamp), 'D'),
        "server.created_at" => stamp(g.and_then(|g| g.created_timestamp), 'D'),
        "server.boost_count" => g.and_then(|g| g.premium_subscription_count.clone()).unwrap_or_else(|| "0".into()),
        "server.boost_level" => g.and_then(|g| g.premium_tier.clone()).unwrap_or_else(|| "0".into()),
        "channel" | "channel.mention" => c.map(|c| format!("<#{}>", c.id)).unwrap_or_default(),
        "channel.name" => unmention(&c.and_then(|c| c.name.clone()).unwrap_or_default()),
        "channel.id" => c.map(|c| c.id.clone()).unwrap_or_default(),
        "channel.type" => channel_type_name(c.and_then(|c| c.kind)).into(),
        _ => String::new(),
    }
}

/// Every built-in token and alias with a short description (`PLACEHOLDERS`), in the JS order.
pub fn placeholders() -> Vec<(&'static str, &'static str)> {
    let mut out: Vec<(&'static str, &'static str)> = Vec::new();
    for (aliases, _, description) in BUILTINS {
        for a in *aliases {
            out.push((a, description));
        }
    }
    out.push(("{random}", "Random number from 0 to 10"));
    out.push(("{random:x:y}", "Random number from x to y, e.g. {random:1:100}"));
    out
}

static ARG_SINGLE: LazyRe = lazy_re!(r"^(\d{1,3})$", "");
static ARG_DEFAULT: LazyRe = lazy_re!(r"^(\d{1,3}):([\s\S]*)$", "");
static ARG_FROM: LazyRe = lazy_re!(r"^(\d{1,3})\.\.\.$", "");
static ARG_UP_TO: LazyRe = lazy_re!(r"^\.\.\.(\d{1,3})$", "");
static ARG_RANGE: LazyRe = lazy_re!(r"^(\d{1,3})\.\.\.(\d{1,3})$", "");
static RANDOM_RANGE_RE: LazyRe = lazy_re!(r"^random:(-?\d{1,10}):(-?\d{1,10})$", "");

fn normalise_var_key(key: &str) -> String {
    let mut k = js::trim(key);
    if k.starts_with('{') && k.ends_with('}') && !k.is_empty() {
        k = if k.len() >= 2 { &k[1..k.len() - 1] } else { "" };
    }
    k.to_lowercase()
}

/// A random number in [0, 1) (`Math.random`), from the std hasher's per-process random keys.
fn random() -> f64 {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    (h.finish() >> 11) as f64 / (1u64 << 53) as f64
}

fn clamp_int(n: f64) -> f64 {
    n.clamp(-1e9, 1e9)
}

fn resolve_arg(token: &str, args: &[String], missing: &mut BTreeSet<u32>) -> Option<String> {
    let num = |m: &JsMatch, t: &[u16], i: usize| -> usize { m.group(t, i).and_then(|x| x.parse().ok()).unwrap_or(0) };
    if let Some((t, m)) = ARG_SINGLE.exec(token) {
        let n = num(&m, &t, 1);
        if n < 1 {
            return None;
        }
        if n > args.len() {
            missing.insert(n as u32);
            return Some(String::new());
        }
        return Some(args[n - 1].clone());
    }
    if let Some((t, m)) = ARG_DEFAULT.exec(token) {
        let n = num(&m, &t, 1);
        if n < 1 {
            return None;
        }
        return match args.get(n - 1) {
            Some(given) if !given.is_empty() => Some(given.clone()),
            _ => Some(m.group(&t, 2).unwrap_or_default()),
        };
    }
    if let Some((t, m)) = ARG_FROM.exec(token) {
        let n = num(&m, &t, 1);
        return if n < 1 { None } else { Some(args.get(n - 1..).unwrap_or(&[]).join(" ")) };
    }
    if let Some((t, m)) = ARG_UP_TO.exec(token) {
        let n = num(&m, &t, 1);
        return if n < 1 { None } else { Some(args[..n.min(args.len())].join(" ")) };
    }
    if let Some((t, m)) = ARG_RANGE.exec(token) {
        let a = num(&m, &t, 1);
        let b = num(&m, &t, 2);
        if a < 1 || b < 1 {
            return None;
        }
        return Some(if a <= b { args.get(a - 1..b.min(args.len())).map(|s| s.join(" ")).unwrap_or_default() } else { String::new() });
    }
    if token == "..." {
        return Some(args.join(" "));
    }
    None
}

/// Renders placeholders and reports what could not be resolved. `max_length` (when > 0) clamps the result.
pub fn render_placeholders_detailed(template: &str, ctx: &RenderContext, max_length: Option<usize>) -> Rendered {
    if template.is_empty() {
        return Rendered { text: String::new(), missing_args: Vec::new(), unknown_tokens: Vec::new() };
    }
    let mut var_map: HashMap<String, &VarValue> = HashMap::new();
    if let Some(vars) = &ctx.vars {
        for (k, v) in vars {
            var_map.insert(normalise_var_key(k), v);
        }
    }
    let objects = Objects {
        u: ctx.member.as_ref().and_then(|m| m.user.as_ref()).or(ctx.user.as_ref()),
        m: ctx.member.as_ref(),
        g: ctx.guild.as_ref().or_else(|| ctx.member.as_ref().and_then(|m| m.guild.as_ref())),
        c: ctx.channel.as_ref(),
    };
    let mut missing = BTreeSet::new();
    let mut unknown: Vec<String> = Vec::new();

    let resolve = |inner: &str, missing: &mut BTreeSet<u32>| -> Option<String> {
        let trimmed = js::trim(inner);
        let name = trimmed.to_lowercase();
        if let Some(value) = var_map.get(&name) {
            return Some(match value {
                VarValue::Text(s) => s.clone(),
                VarValue::Number(n) => js::number_to_string(*n),
                VarValue::Bool(b) => b.to_string(),
                VarValue::Null => String::new(),
                VarValue::Lazy(f) => match f() {
                    Ok(v) => v.unwrap_or_default(),
                    Err(e) => {
                        logger::warn(&[format!("Placeholder var {{{name}}} threw: {e}").into()]);
                        String::new()
                    }
                },
            });
        }
        if let Some(args) = &ctx.args
            && let Some(out) = resolve_arg(trimmed, args, missing) {
                return Some(out);
            }
        if name == "random" {
            return Some(js::number_to_string((random() * 11.0).floor()));
        }
        if let Some((t, m)) = RANDOM_RANGE_RE.exec(&name) {
            let mut x = clamp_int(m.group(&t, 1).and_then(|v| v.parse().ok()).unwrap_or(0.0));
            let mut y = clamp_int(m.group(&t, 2).and_then(|v| v.parse().ok()).unwrap_or(0.0));
            if x > y {
                std::mem::swap(&mut x, &mut y);
            }
            return Some(js::number_to_string(x + (random() * (y - x + 1.0)).floor()));
        }
        let ns = builtin_ns(&name)?;
        let present = match ns {
            Ns::User => objects.u.is_some(),
            Ns::Server => objects.g.is_some(),
            Ns::Channel => objects.c.is_some(),
        };
        if !present {
            return None; // a known token without its context stays untouched
        }
        Some(builtin_value(&name, &objects))
    };

    let mut text = TOKEN_RE.replace_all_with(template, |m, t| {
        let whole = m.text(t);
        let inner = m.group(t, 1).unwrap_or_default();
        if let Some(out) = resolve(&inner, &mut missing) {
            return out;
        }
        let is_builtin = builtin_ns(&js::trim(&inner).to_lowercase()).is_some();
        if !is_builtin && unknown.len() < MAX_UNKNOWN_TOKENS && !unknown.contains(&whole) {
            unknown.push(whole.clone());
        }
        whole
    });
    if let Some(max) = max_length.filter(|m| *m > 0) {
        text = clamp_text(&text, max);
    }
    Rendered { text, missing_args: missing.into_iter().collect(), unknown_tokens: unknown }
}

/// Renders placeholders in a template.
pub fn render_placeholders(template: &str, ctx: &RenderContext, max_length: Option<usize>) -> String {
    render_placeholders_detailed(template, ctx, max_length).text
}

/// Legacy wrapper: renders with a member and a guild. A missing member leaves the user tokens untouched.
pub fn parse_placeholders(template: &str, member: Option<PlaceholderMember>, guild: Option<PlaceholderGuild>) -> String {
    render_placeholders(template, &RenderContext { member, guild, ..Default::default() }, None)
}

const HELP_USER: &[&str] = &[
    "**User**",
    "`{user}` `{user.mention}` — mention",
    "`{user.name}` `{user.username}` — username",
    "`{user.display_name}` `{user.nick}` — display name",
    "`{user.idname}` `{user.tag}` — user tag",
    "`{user.id}` — user ID",
    "`{user.avatar_url}` `{user.avatar}` — avatar URL",
    "`{user.bot}` — True / False",
    "`{user.created}` `{user.created_at}` — account age / date",
    "`{user.joined}` `{user.joined_at}` — joined server (relative / date)",
];
const HELP_SERVER: &[&str] = &[
    "**Server**",
    "`{server}` `{server.name}` — server name",
    "`{server.id}` — server ID",
    "`{server.icon_url}` `{server.icon}` — icon URL",
    "`{server.owner}` `{server.owner_id}` — owner mention / ID",
    "`{server.member_count}` — member count",
    "`{server.member_count.ordinal}` — e.g. 1,234th",
    "`{server.boost_count}` `{server.boost_level}` — boosts / tier",
    "`{server.verification_level}` — verification level (0–4)",
    "`{server.created_at}` `{server.joined_at}` — created / bot joined",
];
const HELP_CHANNEL: &[&str] = &["**Channel**", "`{channel}` — mention · `{channel.name}` · `{channel.id}` · `{channel.type}`"];
const HELP_RANDOM: &[&str] = &["**Random**", "`{random}` — number 0–10 · `{random:1:100}` — number from 1 to 100"];
const HELP_ARGS: &[&str] = &[
    "**Arguments**",
    "`{1}` first word · `{1:default}` with a fallback · `{2...}` 2nd onwards",
    "`{...3}` words 1–3 · `{2...4}` words 2–4 · `{...}` everything",
];

fn help_group(g: &str) -> Option<&'static [&'static str]> {
    Some(match g {
        "user" => HELP_USER,
        "server" => HELP_SERVER,
        "channel" => HELP_CHANNEL,
        "random" => HELP_RANDOM,
        "args" => HELP_ARGS,
        _ => return None,
    })
}

/// Markdown help text for placeholder groups ('user', 'server', 'channel', 'random', 'args'); each block fits one
/// embed field. Unknown groups are skipped.
pub fn placeholder_help(groups: &[&str]) -> String {
    groups.iter().filter_map(|g| help_group(g)).map(|lines| lines.join("\n")).collect::<Vec<_>>().join("\n\n")
}

/// [`placeholder_help`] with the default groups (user, server, channel).
pub fn placeholder_help_default() -> String {
    placeholder_help(&["user", "server", "channel"])
}

/// Legacy help text for /welcome and /tag (`PLACEHOLDER_LIST`).
pub fn placeholder_list() -> String {
    placeholder_help(&["user", "server"])
}
