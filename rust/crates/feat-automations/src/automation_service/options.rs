//! The `/automation` option-to-parameter mapping, the configuration-time checks of the admin who writes an automation,
//! the import reference check and the live problem list of `automationService.js`.

use std::collections::{HashMap, HashSet};

use bot_core::App;
use bot_core::interaction_helpers::{ChannelUsable, PermTarget, channel_usable_verdict, check_assignable_role, check_channel_usable_for, missing_bot_permissions, pingable_role_ids};
use bot_core::permissions::{self, find_channel, permission_names};
use bot_core::privacy_guards::{MemberRef, member_channel_problem};
use bot_db::GuildSettings;
use serde_json::{Map, Value, json};
use serenity::all::{ChannelId, ChannelType, Guild, GuildChannel, Member, RoleId, UserId};

use super::pure::*;
use super::store::list_automation_refs;
use super::tables::*;

fn parse_id(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|v| *v != 0)
}

fn channel_of(guild: &Guild, id: &str) -> Option<GuildChannel> {
    parse_id(id).and_then(|v| find_channel(guild, ChannelId::new(v))).cloned()
}

/// The bot's user id (`client.user.id`), when connected.
pub(crate) fn bot_user_id(app: &App) -> UserId {
    app.discord().map_or(UserId::new(1), |d| d.cache.current_user().id)
}

/// `emojiResolver(guild)`: the guild's emojis (by id, then by name), then every emoji the bot can see
/// (`client.emojis.cache`, by id, then by name).
pub fn emoji_resolver_app<'a>(app: &'a App, guild: Option<&'a Guild>) -> impl Fn(&str) -> Option<CustomEmoji> + 'a {
    move |q: &str| {
        if let Some(found) = emoji_resolver(guild)(q) {
            return Some(found);
        }
        let d = app.discord()?;
        let of = |e: &serenity::all::Emoji| CustomEmoji { id: e.id.to_string(), name: e.name.clone(), animated: e.animated };
        let ids = d.cache.guilds();
        if let Some(v) = q.parse::<u64>().ok().filter(|v| *v != 0) {
            let eid = serenity::all::EmojiId::new(v);
            for gid in &ids {
                if let Some(e) = d.cache.guild(*gid).and_then(|g| g.emojis.get(&eid).map(of)) {
                    return Some(e);
                }
            }
        }
        for gid in &ids {
            if let Some(e) = d.cache.guild(*gid).and_then(|g| g.emojis.values().find(|e| e.name == q).map(of)) {
                return Some(e);
            }
        }
        None
    }
}

/// A channel picked in an option: its id and type (the cached channel is looked up from the id).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptChannel {
    pub id: ChannelId,
    pub kind: ChannelType,
}

/// A user picked in an option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptUser {
    pub id: UserId,
    pub bot: bool,
}

/// The options of `/automation condition add`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConditionOptions {
    pub channel: Option<OptChannel>,
    pub role: Option<RoleId>,
    pub user: Option<OptUser>,
    pub text: Option<String>,
    pub number: Option<i64>,
    pub button: Option<String>,
}

/// The options of `/automation action add`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActionOptions {
    pub channel: Option<OptChannel>,
    pub text: Option<String>,
    pub role: Option<RoleId>,
    pub user: Option<OptUser>,
    pub emoji: Option<String>,
    pub amount: Option<i64>,
    pub embed: Option<String>,
    pub button_label: Option<String>,
    pub button_url: Option<String>,
}

/// Why an option mapping failed (`errorKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Value,
    Perms,
    Role,
    Ping,
    Reward,
    Embed,
}

/// The result of [`condition_params_from_options`] / [`action_params_from_options`].
#[derive(Debug, Clone, PartialEq)]
pub struct ParamsResult {
    pub ok: bool,
    /// The parameters (an object) when `ok`.
    pub params: Value,
    pub error: Option<String>,
    pub error_kind: Option<ErrorKind>,
    pub missing: Vec<String>,
    pub channel: Option<ChannelId>,
    /// `'coins'` or `'xp'`.
    pub reward_kind: Option<&'static str>,
    pub embed_name: Option<String>,
    /// Options given that this type does not read.
    pub ignored: Vec<String>,
}

impl ParamsResult {
    fn ok(params: Value) -> Self {
        Self { ok: true, params, error: None, error_kind: None, missing: Vec::new(), channel: None, reward_kind: None, embed_name: None, ignored: Vec::new() }
    }

    fn fail(error: impl Into<String>, kind: ErrorKind) -> Self {
        Self { ok: false, params: Value::Null, error: Some(error.into()), error_kind: Some(kind), ..Self::ok(Value::Null) }
    }

    fn value(error: impl Into<String>) -> Self {
        Self::fail(error, ErrorKind::Value)
    }

    fn with_ignored(mut self, ignored: Vec<String>) -> Self {
        self.ignored = ignored;
        self
    }
}

fn condition_option_use(t: &str) -> &'static [&'static str] {
    match t {
        "CHANNEL_IN" | "CHANNEL_NOT_IN" | "CATEGORY_IN" | "CATEGORY_NOT_IN" => &["channel", "text"],
        "CONTAINS" | "NOT_CONTAINS" | "EXACT" | "MATCHES_REGEX" | "REACTED_WITH" => &["text"],
        "HAS_REACTIONS" => &["number", "text"],
        "HAS_ANY_ROLE" | "HAS_ALL_ROLES" | "HAS_NOT_ANY_ROLE" | "HAS_NOT_ALL_ROLES" | "GAINED_ROLE" | "LOST_ROLE" => &["role", "text"],
        "USER_IS" => &["user", "text"],
        "BUTTON_IS" => &["button"],
        _ => &[],
    }
}
const ALL_CONDITION_OPTIONS: [&str; 6] = ["channel", "role", "user", "text", "number", "button"];

fn action_option_use(t: &str) -> &'static [&'static str] {
    match t {
        "CREATE_THREAD" => &["text", "channel"],
        "SEND_MESSAGE" => &["text", "channel", "button_label", "button_url"],
        "REPLY" | "SEND_IN_THREAD" => &["text", "button_label", "button_url"],
        "REPOST" => &["channel"],
        "REACT" | "REMOVE_REACTION" => &["emoji"],
        "GIVE_ROLES" | "TAKE_ROLES" => &["role", "text"],
        "ADD_TO_THREAD" => &["user", "text"],
        "GIVE_COINS" | "TAKE_COINS" | "GIVE_XP" | "TAKE_XP" => &["amount"],
        "SEND_EMBED" => &["embed", "channel"],
        _ => &[],
    }
}
const ALL_ACTION_OPTIONS: [&str; 9] = ["channel", "text", "role", "user", "emoji", "amount", "embed", "button_label", "button_url"];

fn given_str(s: &Option<String>) -> bool {
    s.as_deref().is_some_and(|s| !s.is_empty())
}

impl ConditionOptions {
    fn given(&self, name: &str) -> bool {
        match name {
            "channel" => self.channel.is_some(),
            "role" => self.role.is_some(),
            "user" => self.user.is_some(),
            "text" => given_str(&self.text),
            "number" => self.number.is_some(),
            "button" => given_str(&self.button),
            _ => false,
        }
    }
}

impl ActionOptions {
    fn given(&self, name: &str) -> bool {
        match name {
            "channel" => self.channel.is_some(),
            "text" => given_str(&self.text),
            "role" => self.role.is_some(),
            "user" => self.user.is_some(),
            "emoji" => given_str(&self.emoji),
            "amount" => self.amount.is_some(),
            "embed" => given_str(&self.embed),
            "button_label" => given_str(&self.button_label),
            "button_url" => given_str(&self.button_url),
            _ => false,
        }
    }
}

/// The role option plus role mentions / ids in `text`, each checked to exist; @everyone is refused.
fn collect_roles(role: Option<RoleId>, text: Option<&str>, guild: &Guild) -> Result<Vec<String>, String> {
    let mut ids: Vec<String> = Vec::new();
    if let Some(r) = role {
        ids.push(r.to_string());
    }
    for id in parse_id_list(text, IdKind::Role) {
        if !parse_id(&id).is_some_and(|v| guild.roles.contains_key(&RoleId::new(v))) {
            return Err(format!("Unknown role: {id}"));
        }
        ids.push(id);
    }
    let mut roles: Vec<String> = Vec::new();
    for id in ids {
        if !roles.contains(&id) {
            roles.push(id);
        }
    }
    if roles.iter().any(|id| *id == guild.id.to_string()) {
        return Err("The @everyone role cannot be used here.".into());
    }
    if roles.is_empty() { Err("Pick a role or mention roles in text.".into()) } else { Ok(roles) }
}

/// A member of the guild (cache, then one REST fetch).
async fn find_member(app: &App, guild: &Guild, id: &str) -> Option<Member> {
    let uid = UserId::new(parse_id(id)?);
    if let Some(m) = guild.members.get(&uid) {
        return Some(m.clone());
    }
    let http = app.http()?;
    http.get_member(guild.id, uid).await.ok()
}

/// The user option plus user mentions / ids in `text`, each resolved to a member: `(id, bot)`.
async fn collect_users(app: &App, user: Option<OptUser>, text: Option<&str>, guild: &Guild, required: bool) -> Result<Vec<(String, bool)>, String> {
    let mut members: Vec<(String, bool)> = Vec::new();
    if let Some(u) = user {
        members.push((u.id.to_string(), u.bot));
    }
    for id in parse_id_list(text, IdKind::User) {
        let Some(m) = find_member(app, guild, &id).await else { return Err(format!("Unknown user: {id}")) };
        members.push((id, m.user.bot));
    }
    let mut seen = HashSet::new();
    let list: Vec<(String, bool)> = members.into_iter().filter(|(id, _)| seen.insert(id.clone())).collect();
    if required && list.is_empty() {
        return Err("Pick a user or mention users in text.".into());
    }
    Ok(list)
}

/// `conditionParamsFromOptions(type, opts, { guild })`: the `condition add` options of one type as condition
/// parameters. Never fails.
pub async fn condition_params_from_options(app: &App, type_: &str, opts: &ConditionOptions, guild: &Guild) -> ParamsResult {
    let Some(def) = condition_def(type_) else { return ParamsResult::value("Unknown condition type. Pick one from the list.") };
    let used = condition_option_use(def.key);
    let ignored: Vec<String> = ALL_CONDITION_OPTIONS.iter().filter(|n| opts.given(n) && !used.contains(n)).map(|n| n.to_string()).collect();
    let text = opts.text.as_deref();
    let out = match def.key {
        "CHANNEL_IN" | "CHANNEL_NOT_IN" | "CATEGORY_IN" | "CATEGORY_NOT_IN" => {
            let want_category = def.key.starts_with("CATEGORY");
            let mut picked: Vec<(String, ChannelType)> = Vec::new();
            if let Some(c) = opts.channel {
                let kind = find_channel(guild, c.id).map_or(c.kind, |ch| ch.kind);
                picked.push((c.id.to_string(), kind));
            }
            let mut fail = None;
            for id in parse_id_list(text, IdKind::Channel) {
                match channel_of(guild, &id) {
                    Some(ch) => picked.push((ch.id.to_string(), ch.kind)),
                    None => {
                        fail = Some(format!("Unknown channel: {id}"));
                        break;
                    }
                }
            }
            if let Some(f) = fail {
                ParamsResult::value(f)
            } else {
                let mut ids: Vec<String> = Vec::new();
                let mut bad = None;
                for (id, kind) in picked {
                    let is_category = kind == ChannelType::Category;
                    if want_category && !is_category {
                        bad = Some("Pick a category (not a channel).");
                        break;
                    }
                    if !want_category && is_category {
                        bad = Some("That is a category. Use \"Category is one of\" instead.");
                        break;
                    }
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
                match bad {
                    Some(b) => ParamsResult::value(b),
                    None if ids.is_empty() => {
                        ParamsResult::value(if want_category { "Pick a category (not a channel)." } else { "Pick a channel or mention channels in text." })
                    }
                    None => {
                        let mut m = Map::new();
                        m.insert(if want_category { "categoryIds" } else { "channelIds" }.into(), json!(ids));
                        ParamsResult::ok(Value::Object(m))
                    }
                }
            }
        }
        "CONTAINS" | "NOT_CONTAINS" => {
            let words = parse_words(text);
            if words.is_empty() {
                ParamsResult::value("Give words in text, separated by commas.")
            } else if words.iter().any(|w| bot_util::js::utf16_len(w) > 100) {
                ParamsResult::value("Each word can be at most 100 characters.")
            } else {
                ParamsResult::ok(json!({ "words": words }))
            }
        }
        "EXACT" => {
            let t = bot_util::js::trim(text.unwrap_or(""));
            if t.is_empty() {
                ParamsResult::value("Give the exact text in text.")
            } else if bot_util::js::utf16_len(t) > 500 {
                ParamsResult::value("The text can be at most 500 characters.")
            } else {
                ParamsResult::ok(json!({ "texts": [t] }))
            }
        }
        "MATCHES_REGEX" => {
            let t = bot_util::js::trim(text.unwrap_or(""));
            if t.is_empty() {
                ParamsResult::value("Give the regex in text.")
            } else {
                let (pattern, flags) = split_regex_literal(t);
                match bot_util::safe_regex::validate_regex(&pattern, &flags) {
                    Err(reason) => ParamsResult::value(format!("Invalid regex: {reason}")),
                    Ok(()) => ParamsResult::ok(json!({ "pattern": pattern, "flags": flags })),
                }
            }
        }
        "ATTACHMENT_AUDIO" | "ATTACHMENT_IMAGE" | "ATTACHMENT_VIDEO" | "ATTACHMENT_TEXT" | "IS_REPLY" | "IS_NOT_REPLY" => ParamsResult::ok(json!({})),
        "REACTED_WITH" => {
            let tokens: Vec<&str> = bot_util::js::trim(text.unwrap_or("")).split(bot_util::js::is_js_space).filter(|t| !t.is_empty()).collect();
            if tokens.is_empty() || tokens.len() > 10 {
                ParamsResult::value("Give one or more emojis in text.")
            } else {
                let resolver = emoji_resolver_app(app, Some(guild));
                let mut emojis = Vec::new();
                let mut err = None;
                for t in tokens {
                    match parse_emoji_input(t, &resolver) {
                        Ok(e) => emojis.push(e),
                        Err(e) => {
                            err = Some(e);
                            break;
                        }
                    }
                }
                match err {
                    Some(e) => ParamsResult::value(e),
                    None => ParamsResult::ok(json!({ "emojis": emojis })),
                }
            }
        }
        "HAS_REACTIONS" => match opts.number {
            None => ParamsResult::value("Give the minimum count in number."),
            Some(min) => {
                let raw = text.unwrap_or("");
                if bot_util::js::trim(raw).is_empty() {
                    ParamsResult::ok(json!({ "min": min, "emoji": null }))
                } else {
                    match parse_emoji_input(raw, emoji_resolver_app(app, Some(guild))) {
                        Ok(e) => ParamsResult::ok(json!({ "min": min, "emoji": e })),
                        Err(e) => ParamsResult::value(e),
                    }
                }
            }
        },
        "HAS_ANY_ROLE" | "HAS_ALL_ROLES" | "HAS_NOT_ANY_ROLE" | "HAS_NOT_ALL_ROLES" | "GAINED_ROLE" | "LOST_ROLE" => {
            match collect_roles(opts.role, text, guild) {
                Ok(ids) => ParamsResult::ok(json!({ "roleIds": ids })),
                Err(e) => ParamsResult::value(e),
            }
        }
        "USER_IS" => match collect_users(app, opts.user, text, guild, true).await {
            Ok(list) => ParamsResult::ok(json!({ "userIds": list.into_iter().map(|(id, _)| id).collect::<Vec<_>>() })),
            Err(e) => ParamsResult::value(e),
        },
        "BUTTON_IS" => {
            let raw = bot_util::js::trim(opts.button.as_deref().unwrap_or(""));
            let parsed = raw.split_once(':').filter(|(id, n)| is_automation_id(id) && (1..=2).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit()));
            match parsed {
                None => ParamsResult::value("Pick a button from the list."),
                Some((id, n)) => {
                    let index: i64 = n.parse().unwrap_or(0);
                    let exists = super::store::list_automation_buttons(app.db(), &guild.id.to_string()).await.iter().any(|b| b.automation_id == id && b.index == index);
                    if exists { ParamsResult::ok(json!({ "buttons": [{ "automationId": id, "index": index }] })) } else { ParamsResult::value("Pick a button from the list.") }
                }
            }
        }
        _ => ParamsResult::value("Unknown condition type. Pick one from the list."),
    };
    out.with_ignored(ignored)
}

/// `/^\/(.+)\/([a-z]*)$/s`: a `/pattern/flags` literal, else the text with flags `i`.
fn split_regex_literal(t: &str) -> (String, String) {
    if let Some(rest) = t.strip_prefix('/')
        && let Some(slash) = rest.rfind('/')
    {
        let (pattern, flags) = (&rest[..slash], &rest[slash + 1..]);
        if !pattern.is_empty() && flags.bytes().all(|b| b.is_ascii_lowercase()) {
            return (pattern.to_string(), flags.to_string());
        }
    }
    (t.to_string(), "i".to_string())
}

/// The context of [`action_params_from_options`].
#[derive(Clone, Copy)]
pub struct ActionContext<'a> {
    pub guild: &'a Guild,
    /// The configuring admin (`interaction.member`).
    pub member: Option<&'a Member>,
    pub settings: Option<&'a GuildSettings>,
    /// Where the admin runs the command (a reply or thread message is checked there).
    pub fallback_channel: Option<&'a GuildChannel>,
}

fn channel_problem(channel: ChannelId, check: &ChannelUsable) -> ParamsResult {
    if !check.missing.is_empty() {
        let mut r = ParamsResult::fail(check.reason.clone().unwrap_or_default(), ErrorKind::Perms);
        r.missing = check.missing.clone();
        r.channel = Some(channel);
        return r;
    }
    ParamsResult::value(check.reason.clone().unwrap_or_else(|| "That channel cannot be used.".into()))
}

fn usable(app: &App, guild: &Guild, ch: Option<&GuildChannel>, needs: &[&str]) -> ChannelUsable {
    match ch {
        Some(c) => check_channel_usable_for(app, guild.id, c, needs),
        None => channel_usable_verdict(None, None, needs),
    }
}

/// `buttonFromOptions(opts, type)`: `Ok(None)` for no button on a type that takes one.
fn button_from_options(opts: &ActionOptions) -> Result<Value, String> {
    let label = bot_util::js::trim(opts.button_label.as_deref().unwrap_or("")).to_string();
    let url = bot_util::js::trim(opts.button_url.as_deref().unwrap_or("")).to_string();
    if label.is_empty() && url.is_empty() {
        return Ok(Value::Null);
    }
    if label.is_empty() {
        return Err("Give button_label too.".into());
    }
    if bot_util::js::utf16_len(&label) > BUTTON_LABEL_MAX {
        return Err(format!("The button label can be at most {BUTTON_LABEL_MAX} characters."));
    }
    if !url.is_empty() {
        if bot_util::js::utf16_len(&url) > BUTTON_URL_MAX || !bot_core::custom_embed::is_http_url(&url) {
            return Err("The button link must start with http:// or https://.".into());
        }
        return Ok(json!({ "label": label, "url": url, "style": "PRIMARY", "index": null }));
    }
    // An automation button: its number is assigned under the row lock by append_action.
    Ok(json!({ "label": label, "url": null, "style": "PRIMARY", "index": null }))
}

fn with_button(mut action: Map<String, Value>, opts: &ActionOptions) -> ParamsResult {
    match button_from_options(opts) {
        Err(e) => ParamsResult::value(e),
        Ok(b) => {
            action.insert("button".into(), b);
            ParamsResult::ok(Value::Object(action))
        }
    }
}

/// `actionParamsFromOptions(type, opts, { guild, member, settings, channel })`: the `action add` options of one type
/// as an action, with the add-time checks (bot permissions, assignable roles, reward modules, saved embeds, role
/// pings). `params.button.index` is left null: `append_action` assigns it under the row lock. Never fails.
pub async fn action_params_from_options(app: &App, type_: &str, opts: &ActionOptions, ctx: ActionContext<'_>) -> ParamsResult {
    let Some(def) = action_def(type_) else { return ParamsResult::value("Unknown action type. Pick one from the list.") };
    let guild = ctx.guild;
    let used = action_option_use(def.key);
    let ignored: Vec<String> = ALL_ACTION_OPTIONS.iter().filter(|n| opts.given(n) && !used.contains(n)).map(|n| n.to_string()).collect();
    // The JS had the option's resolved channel object: one the cache does not hold yet is fetched over REST.
    let target_cached: Option<GuildChannel> = match opts.channel {
        Some(c) => match find_channel(guild, c.id) {
            Some(ch) => Some(ch.clone()),
            None => bot_core::interaction_helpers::resolve_channel(app, guild.id, c.id).await,
        },
        None => None,
    };
    let target_kind = opts.channel.map(|c| target_cached.as_ref().map_or(c.kind, |ch| ch.kind));
    let target_id = opts.channel.map(|c| c.id);
    let text = bot_util::js::trim(opts.text.as_deref().unwrap_or("")).to_string();
    let n = bot_util::js::utf16_len(&text);
    let ping_channel = target_cached.as_ref().or(if opts.channel.is_some() { None } else { ctx.fallback_channel });
    let out = match def.key {
        "CREATE_THREAD" => {
            let name = if text.is_empty() { DEFAULT_THREAD_NAME.to_string() } else { text.clone() };
            if bot_util::js::utf16_len(&name) > THREAD_NAME_MAX {
                ParamsResult::value(format!("The thread name can be at most {THREAD_NAME_MAX} characters."))
            } else if let (Some(id), Some(kind)) = (target_id, target_kind) {
                if !matches!(kind, ChannelType::Text | ChannelType::News) {
                    ParamsResult::value("Pick a text or announcement channel.")
                } else {
                    let needs = ["ViewChannel", "CreatePublicThreads", "SendMessagesInThreads"];
                    let missing = match &target_cached {
                        Some(ch) => bot_core::interaction_helpers::missing_from(bot_core::interaction_helpers::bot_permissions_for(app, guild.id, ch), &needs),
                        None => missing_bot_permissions(app, PermTarget::Channel(guild.id, id), &needs),
                    };
                    if missing.is_empty() {
                        ParamsResult::ok(json!({ "name": name, "channelId": id.to_string(), "autoArchiveMinutes": 1440 }))
                    } else {
                        let mut r = ParamsResult::fail(format!("I need {} in <#{id}>.", permission_names(&missing).join(", ")), ErrorKind::Perms);
                        r.missing = missing;
                        r.channel = Some(id);
                        r
                    }
                }
            } else {
                ParamsResult::ok(json!({ "name": name, "channelId": null, "autoArchiveMinutes": 1440 }))
            }
        }
        "SEND_MESSAGE" => {
            if text.is_empty() {
                ParamsResult::value("Give the message in text.")
            } else if n > CONTENT_MAX {
                ParamsResult::value(format!("The message can be at most {CONTENT_MAX} characters."))
            } else {
                let check = target_id.map(|id| (id, usable(app, guild, target_cached.as_ref(), &["ViewChannel", "SendMessages"])));
                match check {
                    Some((id, c)) if !c.ok => channel_problem(id, &c),
                    _ => match role_ping_refusal(guild, ctx.member, ping_channel, &text) {
                        Some(p) => ParamsResult::fail(p, ErrorKind::Ping),
                        None => {
                            let mut m = Map::new();
                            m.insert("content".into(), json!(text));
                            m.insert("channelId".into(), target_id.map_or(Value::Null, |c| json!(c.to_string())));
                            with_button(m, opts)
                        }
                    },
                }
            }
        }
        "REPLY" | "SEND_IN_THREAD" => {
            if text.is_empty() {
                ParamsResult::value("Give the message in text.")
            } else if n > CONTENT_MAX {
                ParamsResult::value(format!("The message can be at most {CONTENT_MAX} characters."))
            } else {
                match role_ping_refusal(guild, ctx.member, ctx.fallback_channel, &text) {
                    Some(p) => ParamsResult::fail(p, ErrorKind::Ping),
                    None => {
                        let mut m = Map::new();
                        m.insert("content".into(), json!(text));
                        with_button(m, opts)
                    }
                }
            }
        }
        "REPOST" => match target_id {
            None => ParamsResult::value("Pick the channel to repost to."),
            Some(id) => {
                let c = usable(app, guild, target_cached.as_ref(), &["ViewChannel", "SendMessages", "EmbedLinks"]);
                if c.ok { ParamsResult::ok(json!({ "channelId": id.to_string() })) } else { channel_problem(id, &c) }
            }
        },
        "PIN" | "DELETE" => ParamsResult::ok(json!({})),
        "REACT" | "REMOVE_REACTION" => {
            let raw = bot_util::js::trim(opts.emoji.as_deref().unwrap_or(""));
            if raw.is_empty() {
                if def.key == "REACT" { ParamsResult::value("Give the emoji in emoji.") } else { ParamsResult::ok(json!({ "emoji": null })) }
            } else {
                match parse_emoji_input(raw, emoji_resolver_app(app, Some(guild))) {
                    Ok(e) => ParamsResult::ok(json!({ "emoji": e })),
                    Err(e) => ParamsResult::value(e),
                }
            }
        }
        "GIVE_ROLES" | "TAKE_ROLES" => match collect_roles(opts.role, opts.text.as_deref(), guild) {
            Err(e) => ParamsResult::value(e),
            Ok(ids) if ids.len() > 10 => ParamsResult::value("Pick at most 10 roles."),
            Ok(ids) => {
                let bot = bot_user_id(app);
                let refused = ids.iter().find_map(|id| {
                    let role = parse_id(id).and_then(|v| guild.roles.get(&RoleId::new(v)));
                    let check = check_assignable_role(guild, role, bot, ctx.member, false);
                    (!check.ok).then(|| check.reason.unwrap_or_default())
                });
                match refused {
                    Some(reason) => ParamsResult::fail(reason, ErrorKind::Role),
                    None => ParamsResult::ok(json!({ "roleIds": ids })),
                }
            }
        },
        "ADD_TO_THREAD" => match collect_users(app, opts.user, opts.text.as_deref(), guild, false).await {
            Err(e) => ParamsResult::value(e),
            Ok(list) if list.iter().any(|(_, bot)| *bot) => ParamsResult::value("Bots cannot be added to threads by automations."),
            Ok(list) if list.len() > 10 => ParamsResult::value("Pick at most 10 members."),
            Ok(list) => ParamsResult::ok(json!({ "userIds": list.into_iter().map(|(id, _)| id).collect::<Vec<_>>() })),
        },
        "GIVE_COINS" | "TAKE_COINS" | "GIVE_XP" | "TAKE_XP" => match opts.amount {
            None => ParamsResult::value("Give the amount."),
            Some(amount) => {
                let kind = def.reward.unwrap_or("coins");
                let rk = if kind == "xp" { bot_core::rewards::RewardKind::Xp } else { bot_core::rewards::RewardKind::Coins };
                if app.rewards().is_reward_available(rk, ctx.settings) {
                    ParamsResult::ok(json!({ "amount": amount }))
                } else {
                    let mut r = ParamsResult::fail("That reward is not available.", ErrorKind::Reward);
                    r.reward_kind = Some(kind);
                    r
                }
            }
        },
        "SEND_EMBED" => match opts.embed.as_deref().and_then(normalize_automation_name) {
            None => ParamsResult::value("Give the saved embed name in embed."),
            Some(embed_name) => {
                let template = feat_embeds::embed_service::get_saved_embed_template(app, &guild.id.to_string(), &embed_name).await;
                match template {
                    None => {
                        let mut r = ParamsResult::fail("Embed not found.", ErrorKind::Embed);
                        r.embed_name = Some(embed_name);
                        r
                    }
                    Some(tpl) => {
                        let check = target_id.map(|id| (id, usable(app, guild, target_cached.as_ref(), &["ViewChannel", "SendMessages", "EmbedLinks"])));
                        match check {
                            Some((id, c)) if !c.ok => channel_problem(id, &c),
                            _ => match role_ping_refusal(guild, ctx.member, ping_channel, tpl.get("content").and_then(Value::as_str).unwrap_or("")) {
                                Some(p) => ParamsResult::fail(p, ErrorKind::Ping),
                                None => ParamsResult::ok(json!({ "embedName": embed_name, "channelId": target_id.map(|c| c.to_string()) })),
                            },
                        }
                    }
                }
            }
        },
        _ => ParamsResult::value("Unknown action type. Pick one from the list."),
    };
    out.with_ignored(ignored)
}

// ── Configuration-time checks ────────────────────────────────────────────────

/// `importRoleHierarchyProblem(guild, member, actions)`: the importing admin may only hand out roles below their own
/// highest role (the guild owner is exempt). Roles the server does not have are left to the reference check.
pub fn import_role_hierarchy_problem(guild: &Guild, member: Option<&Member>, actions: &[Value]) -> Option<String> {
    let member = member?;
    if member.user.id == guild.owner_id {
        return None;
    }
    let top = permissions::highest_position(guild, &member.roles);
    for (k, a) in actions.iter().enumerate() {
        for id in str_list(a, "roleIds") {
            let pos = parse_id(&id).and_then(|v| permissions::role_position(guild, RoleId::new(v)));
            if pos.is_some_and(|p| p >= top) {
                return Some(format!("Action {}: That role is above your highest role.", k + 1));
            }
        }
    }
    None
}

/// `actionChannelAccessProblem(guild, member, actions, { first })`: the first action whose target channel the
/// configuring member lacks access to (owner and Administrators are exempt).
pub fn action_channel_access_problem(guild: &Guild, member: Option<&Member>, actions: &[Value], first: usize) -> Option<String> {
    for (k, a) in actions.iter().enumerate() {
        let Some(needs) = a.get("type").and_then(Value::as_str).and_then(executor_channel_needs) else { continue };
        let Some(channel) = a.get("channelId").and_then(Value::as_str).and_then(|id| channel_of(guild, id)) else { continue };
        if let Some(problem) = member_channel_problem(guild, Some(&channel), member.map(MemberRef::of), needs) {
            return Some(format!("Action {}: {problem}", first + k));
        }
    }
    None
}

/// Unique `<@&id>` mentions in a text, in order.
pub fn role_mention_ids(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let b = text.as_bytes();
    let mut i = 0;
    while let Some(pos) = text[i..].find("<@&") {
        let start = i + pos + 3;
        let n = b[start..].iter().take_while(|c| c.is_ascii_digit()).count();
        if (17..=20).contains(&n) && b.get(start + n) == Some(&b'>') {
            let id = text[start..start + n].to_string();
            if !out.contains(&id) {
                out.push(id);
            }
            i = start + n + 1;
        } else {
            i = start;
        }
    }
    out
}

/// `rolePingRefusal(member, channel, text)`: role mentions the member could not ping themselves in `channel`. No
/// member or no channel means nothing to compare with.
pub fn role_ping_refusal(guild: &Guild, member: Option<&Member>, channel: Option<&GuildChannel>, text: &str) -> Option<String> {
    let (member, channel) = (member?, channel?);
    let ids = role_mention_ids(text);
    if ids.is_empty() {
        return None;
    }
    let p = pingable_role_ids(guild, channel, Some((member.user.id, &member.roles)), &ids);
    if p.refused.is_empty() {
        return None;
    }
    let list: Vec<String> = p.refused.iter().map(|id| format!("<@&{id}>")).collect();
    Some(format!(
        "{} {} not mentionable and you lack **Mention @everyone, @here and All Roles** in <#{}>.",
        list.join(" "),
        if p.refused.len() == 1 { "is" } else { "are" },
        channel.id
    ))
}

/// `importRolePingProblem(guild, member, actions, { fallbackChannel })`: [`role_ping_refusal`] over imported actions.
pub fn import_role_ping_problem(guild: &Guild, member: Option<&Member>, actions: &[Value], fallback_channel: Option<&GuildChannel>) -> Option<String> {
    for (k, a) in actions.iter().enumerate() {
        let t = a.get("type").and_then(Value::as_str).unwrap_or("");
        if !matches!(t, "SEND_MESSAGE" | "REPLY" | "SEND_IN_THREAD") {
            continue;
        }
        let channel = match a.get("channelId").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            Some(id) => channel_of(guild, id),
            None => fallback_channel.cloned(),
        };
        if let Some(p) = role_ping_refusal(guild, member, channel.as_ref(), sfield(a, "content")) {
            return Some(format!("Action {}: {p}", k + 1));
        }
    }
    None
}

/// The result of [`resolve_import_references`]: `warnings` save the automation off; `info` lines do not.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedImport {
    pub data: ImportData,
    pub warnings: Vec<String>,
    pub info: Vec<String>,
}

/// `resolveImportReferences(guild, data, { settings, existingRow })`: checks an imported automation against this server
/// and remaps button references.
pub async fn resolve_import_references(
    app: &App,
    guild: &Guild,
    data: &ImportData,
    settings: Option<&GuildSettings>,
    existing_row_id: Option<&str>,
) -> ResolvedImport {
    let mut warnings = Vec::new();
    let mut info = Vec::new();
    let mut conditions = data.conditions.clone();
    let actions = data.actions.clone();
    let exported_id = data.exported_id.clone();
    let refs = list_automation_refs(app.db(), &guild.id.to_string()).await;
    let by_id: HashMap<&str, &super::store::AutomationRef> = refs.iter().map(|r| (r.id.as_str(), r)).collect();
    let by_name: HashMap<&str, &super::store::AutomationRef> = refs.iter().map(|r| (r.name.as_str(), r)).collect();
    let bot = bot_user_id(app);
    let mut member_ids: Vec<String> = Vec::new();
    let add_member = |ids: &mut Vec<String>, id: String| {
        if !ids.contains(&id) {
            ids.push(id);
        }
    };
    let check_channel = |w: &mut Vec<String>, label: &str, id: &str, category: bool| match channel_of(guild, id) {
        None => w.push(format!("{label}: Unknown channel {id}")),
        Some(ch) if category && ch.kind != ChannelType::Category => w.push(format!("{label}: {id} is not a category")),
        _ => {}
    };
    let check_role = |w: &mut Vec<String>, label: &str, id: &str, assignable: bool| {
        let role = parse_id(id).and_then(|v| guild.roles.get(&RoleId::new(v)));
        let Some(role) = role else {
            w.push(format!("{label}: Unknown role {id}"));
            return;
        };
        if assignable {
            let check = check_assignable_role(guild, Some(role), bot, None, false);
            if !check.ok {
                w.push(format!("{label}: {}", check.reason.unwrap_or_default()));
            }
        }
    };

    for (k, c) in conditions.iter_mut().enumerate() {
        let label = format!("Condition {}", k + 1);
        let t = c.get("type").and_then(Value::as_str).unwrap_or("").to_string();
        if t == "CHANNEL_IN" || t == "CHANNEL_NOT_IN" {
            for id in str_list(c, "channelIds") {
                check_channel(&mut warnings, &label, &id, false);
            }
        } else if t == "CATEGORY_IN" || t == "CATEGORY_NOT_IN" {
            for id in str_list(c, "categoryIds") {
                check_channel(&mut warnings, &label, &id, true);
            }
        } else if c.get("roleIds").is_some() {
            for id in str_list(c, "roleIds") {
                check_role(&mut warnings, &label, &id, false);
            }
        } else if t == "USER_IS" {
            for id in str_list(c, "userIds") {
                add_member(&mut member_ids, id);
            }
        } else if t == "BUTTON_IS"
            && let Some(buttons) = c.get_mut("buttons").and_then(Value::as_array_mut)
        {
            for b in buttons.iter_mut() {
                let aid = sfield(b, "automationId").to_string();
                if exported_id.as_deref().is_some_and(|e| e == aid) {
                    // A self reference: an existing row keeps its id; a new row gets the created id in create_automation.
                    if let Some(own) = existing_row_id
                        && let Some(m) = b.as_object_mut()
                    {
                        m.insert("automationId".into(), json!(own));
                    }
                } else if !by_id.contains_key(aid.as_str()) {
                    let hinted = b
                        .get("automationName")
                        .and_then(Value::as_str)
                        .map(|n| normalize_automation_name(n).unwrap_or_default())
                        .and_then(|n| by_name.get(n.as_str()).copied());
                    match hinted {
                        Some(h) => {
                            if let Some(m) = b.as_object_mut() {
                                m.insert("automationId".into(), json!(h.id));
                            }
                            info.push(format!("{label}: button reference remapped to \"{}\".", h.name));
                        }
                        None => warnings.push(format!("{label}: refers to a button of an automation that does not exist here.")),
                    }
                }
            }
        }
    }

    for (k, a) in actions.iter().enumerate() {
        let label = format!("Action {}", k + 1);
        if let Some(id) = a.get("channelId").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            check_channel(&mut warnings, &label, id, false);
        }
        if a.get("roleIds").is_some() {
            for id in str_list(a, "roleIds") {
                check_role(&mut warnings, &label, &id, true);
            }
        }
        for id in str_list(a, "userIds") {
            add_member(&mut member_ids, id);
        }
        let t = a.get("type").and_then(Value::as_str).unwrap_or("");
        if t == "SEND_EMBED" {
            let name = sfield(a, "embedName");
            if feat_embeds::embed_service::get_saved_embed_template(app, &guild.id.to_string(), name).await.is_none() {
                warnings.push(format!("{label}: Saved embed \"{name}\" not found"));
            }
        }
        if let Some(kind) = action_def(t).and_then(|d| d.reward) {
            let rk = if kind == "xp" { bot_core::rewards::RewardKind::Xp } else { bot_core::rewards::RewardKind::Coins };
            if !app.rewards().is_reward_available(rk, settings) {
                warnings.push(format!("{label}: {} rewards are not available (module off)", if kind == "xp" { "XP" } else { "Coins" }));
            }
        }
    }

    let mut fetches = 0;
    for id in member_ids {
        if parse_id(&id).is_some_and(|v| guild.members.contains_key(&UserId::new(v))) {
            continue;
        }
        if fetches >= MAX_IMPORT_MEMBER_FETCHES {
            warnings.push(format!("Member {id} could not be checked (too many members listed)"));
            continue;
        }
        fetches += 1;
        if find_member(app, guild, &id).await.is_none() {
            warnings.push(format!("Unknown member {id}"));
        }
    }

    // Normalising again drops the automationName hints and the duplicates a remap created.
    let data = ImportData {
        conditions: normalize_items(&Value::Array(conditions), ListKind::Condition),
        actions: normalize_items(&Value::Array(actions), ListKind::Action),
        ..data.clone()
    };
    ResolvedImport { data, warnings, info }
}

// ── Problems ─────────────────────────────────────────────────────────────────

/// `automationProblems(guild, row, settings)`: live checks of a stored automation for `/automation info` and
/// `enable`. `guild` is `None` when it is not available (only the stored-data problems are listed then).
pub async fn automation_problems(app: &App, guild: Option<&Guild>, row: &super::store::AutomationRecord, settings: Option<&GuildSettings>) -> Vec<String> {
    let mut out = Vec::new();
    for inv in &row.invalid_conditions {
        out.push(format!("Condition {} is invalid and ignored: {}", inv.index, inv.error));
    }
    for inv in &row.invalid_actions {
        out.push(format!("Action {} is invalid and ignored: {}", inv.index, inv.error));
    }
    if row.enabled && row.actions.is_empty() {
        out.push("It is on but has no actions, so it never runs.".into());
    }
    let Some(guild) = guild else { return out };
    let chan_gone = |out: &mut Vec<String>, label: &str, id: &str| -> bool {
        if channel_of(guild, id).is_none() {
            out.push(format!("{label}: the channel {id} was deleted."));
            return true;
        }
        false
    };
    let role_gone = |out: &mut Vec<String>, label: &str, id: &str| -> bool {
        if !parse_id(id).is_some_and(|v| guild.roles.contains_key(&RoleId::new(v))) {
            out.push(format!("{label}: the role {id} was deleted."));
            return true;
        }
        false
    };
    let has_buttons = row.conditions.iter().any(|c| c.get("type").and_then(Value::as_str) == Some("BUTTON_IS"));
    let refs = if has_buttons { list_automation_refs(app.db(), &guild.id.to_string()).await } else { Vec::new() };
    for (k, c) in row.conditions.iter().enumerate() {
        let label = format!("Condition {}", k + 1);
        for id in str_list(c, "channelIds") {
            chan_gone(&mut out, &label, &id);
        }
        for id in str_list(c, "categoryIds") {
            chan_gone(&mut out, &label, &id);
        }
        for id in str_list(c, "roleIds") {
            role_gone(&mut out, &label, &id);
        }
        for b in c.get("buttons").and_then(Value::as_array).cloned().unwrap_or_default() {
            let aid = sfield(&b, "automationId");
            let index = js_int(b.get("index").unwrap_or(&Value::Null));
            match refs.iter().find(|r| r.id == aid) {
                None => out.push(format!("{label}: refers to a button of an automation that no longer exists.")),
                Some(r) => {
                    if !r.actions.iter().any(|a| automation_button_index(a).is_some() && automation_button_index(a) == index) {
                        out.push(format!("{label}: button {} no longer exists on \"{}\".", js_string(b.get("index")), r.name));
                    }
                }
            }
        }
    }
    let bot = bot_user_id(app);
    for (k, a) in row.actions.iter().enumerate() {
        let label = format!("Action {}", k + 1);
        let t = a.get("type").and_then(Value::as_str).unwrap_or("");
        let mut target: Option<GuildChannel> = None;
        if let Some(id) = a.get("channelId").and_then(Value::as_str).filter(|s| !s.is_empty())
            && !chan_gone(&mut out, &label, id)
        {
            target = channel_of(guild, id);
        }
        for id in str_list(a, "roleIds") {
            if !role_gone(&mut out, &label, &id) {
                let role = parse_id(&id).and_then(|v| guild.roles.get(&RoleId::new(v)));
                let check = check_assignable_role(guild, role, bot, None, false);
                if !check.ok {
                    out.push(format!("{label}: {}", check.reason.unwrap_or_default()));
                }
            }
        }
        if let Some(target) = &target {
            let need: Option<&[&str]> = match t {
                "CREATE_THREAD" => {
                    let missing = missing_bot_permissions(app, PermTarget::Channel(guild.id, target.id), &["ViewChannel", "CreatePublicThreads", "SendMessagesInThreads"]);
                    if !missing.is_empty() {
                        out.push(format!("{label}: I need **{}** in <#{}>.", permission_names(&missing).join(", "), target.id));
                    }
                    None
                }
                "SEND_MESSAGE" => Some(&["ViewChannel", "SendMessages"]),
                "SEND_EMBED" | "REPOST" => Some(&["ViewChannel", "SendMessages", "EmbedLinks"]),
                _ => None,
            };
            if let Some(need) = need {
                let check = check_channel_usable_for(app, guild.id, target, need);
                if !check.ok {
                    out.push(format!("{label}: {}", check.reason.unwrap_or_default()));
                }
            }
        }
        if t == "SEND_EMBED" {
            let name = sfield(a, "embedName");
            if feat_embeds::embed_service::get_saved_embed_template(app, &guild.id.to_string(), name).await.is_none() {
                out.push(format!("{label}: the saved embed \"{name}\" was not found."));
            }
        }
        if let Some(kind) = action_def(t).and_then(|d| d.reward) {
            let rk = if kind == "xp" { bot_core::rewards::RewardKind::Xp } else { bot_core::rewards::RewardKind::Coins };
            if !app.rewards().is_reward_available(rk, settings) {
                let (what, module) = if kind == "xp" { ("XP", "Levels") } else { ("Coin", "Economy") };
                out.push(format!("{label}: {what} rewards need the {module} module."));
            }
        }
    }
    if let Some(last) = app.cache().get(&problem_key(&row.id)).await
        && let Some(text) = last.get("text").and_then(Value::as_str)
    {
        let at = last.get("at").and_then(Value::as_str).map_or(f64::NAN, bot_util::js::date_parse);
        let sec = (at / 1000.0).floor();
        out.push(if sec.is_finite() { format!("Last problem <t:{}:R>: {text}", sec as i64) } else { format!("Last problem: {text}") });
    }
    out
}
