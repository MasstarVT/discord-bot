//! The runtime of `automationService.js`: facts, the action executor, the dispatcher (cooldowns, the run budget, the
//! per-guild run queue, loop protection), every hook body and the persistent button handler.

use std::sync::{Arc, Mutex};

use bot_core::App;
use bot_core::adapters::{ImageUrlOptions, placeholder_channel, placeholder_guild, placeholder_member, placeholder_user, user_display_avatar_url_with, user_display_name};
use bot_core::hooks::events::ReactionEvent;
use bot_core::interaction::Responder;
use bot_core::interaction_helpers::{PermTarget, check_assignable_role, check_channel_usable_for, missing_bot_permissions, pingable_role_ids};
use bot_core::permissions::{find_channel, is_thread, permission_names};
use bot_core::settings::Settings;
use bot_util::placeholder_parser::{RenderContext, VarValue, render_placeholders};
use bot_util::text::{clamp_text, join_with_limit};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use serenity::all::{
    AutoArchiveDuration, ButtonStyle, ChannelId, ChannelType, EmojiId, Guild, GuildChannel, GuildId, Member, Message, MessageFlags, MessageId,
    MessageReference, MessageReferenceKind, MessageType, ReactionType, RoleId, User, UserId,
};
use serenity::builder::{
    CreateActionRow, CreateAllowedMentions, CreateButton, CreateEmbed, CreateEmbedAuthor, CreateEmbedFooter, CreateInteractionResponseFollowup, CreateMessage,
    CreateThread,
};

use super::options::{bot_user_id, role_mention_ids};
use super::pure::*;
use super::store::{AutomationLite, bump_uses, get_automations, lock, now_ms, state, warn_allowed};
use super::tables::*;

/// The reaction of a `REACTION_ADDED` run: its matching key and how it shows (`{reaction}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionInfo {
    pub key: String,
    pub display: String,
}

/// `getSettings()` of a hook (memoised per dispatch).
pub type SettingsFn = Arc<dyn Fn() -> BoxFuture<'static, Option<Settings>> + Send + Sync>;

/// The event an automation runs for (`base`): who, where, which message, plus the trigger specifics.
#[derive(Clone)]
pub struct Base {
    pub trigger: &'static str,
    pub guild_id: GuildId,
    pub member: Option<Member>,
    pub user: Option<User>,
    pub channel: Option<GuildChannel>,
    /// The full message (`None` while a reaction's message is not fetched yet).
    pub message: Option<Message>,
    pub reaction: Option<ReactionInfo>,
    /// `ensureMessage()` of a reaction: fetched only once a cheap condition set passed.
    pub reaction_event: Option<Arc<ReactionEvent>>,
    pub old_content: Option<String>,
    /// Gained or lost roles (`ROLE_GAINED` / `ROLE_LOST`).
    pub role_ids: Option<Vec<String>>,
    pub voice_channel: Option<GuildChannel>,
    pub thread: Option<GuildChannel>,
    pub captcha_pending: bool,
    /// The triggering message no longer exists (`MESSAGE_DELETED`).
    pub message_gone: bool,
    pub depth: u32,
    pub button: Option<ButtonFact>,
    /// The clicked button's interaction (`BUTTON_CLICKED`).
    pub interaction: Option<Responder>,
    pub get_settings: Option<SettingsFn>,
}

impl Base {
    pub fn new(trigger: &'static str, guild_id: GuildId) -> Self {
        Self {
            trigger,
            guild_id,
            member: None,
            user: None,
            channel: None,
            message: None,
            reaction: None,
            reaction_event: None,
            old_content: None,
            role_ids: None,
            voice_channel: None,
            thread: None,
            captcha_pending: false,
            message_gone: false,
            depth: 0,
            button: None,
            interaction: None,
            get_settings: None,
        }
    }

    fn user_id(&self) -> Option<UserId> {
        self.user.as_ref().map(|u| u.id).or_else(|| self.member.as_ref().map(|m| m.user.id))
    }
}

fn reaction_key_of(r: &ReactionType) -> String {
    match r {
        ReactionType::Custom { id, .. } => id.to_string(),
        ReactionType::Unicode(s) => strip_fe0f(s),
        _ => String::new(),
    }
}

/// `buildFacts(trigger, base)`: plain facts for the evaluator. `guild` is the cached guild (category lookups and
/// discord.js's role cache).
pub fn build_facts(trigger: &str, base: &Base, guild: Option<&Guild>) -> Facts {
    let channel = base.channel.as_ref();
    let thread = channel.is_some_and(is_thread);
    let mut f = Facts {
        user_id: base.user_id().map(|u| u.to_string()),
        channel_id: channel.map(|c| c.id.to_string()),
        parent_id: if thread { channel.and_then(|c| c.parent_id).map(|p| p.to_string()) } else { None },
        category_id: if thread {
            channel.and_then(|c| c.parent_id).and_then(|p| guild.and_then(|g| find_channel(g, p))).and_then(|p| p.parent_id).map(|p| p.to_string())
        } else {
            channel.and_then(|c| c.parent_id).map(|p| p.to_string())
        },
        ..Facts::default()
    };
    if let Some(m) = &base.member {
        f.member_role_ids = Some(match guild {
            Some(g) => bot_core::interaction_helpers::member_role_ids(g, m).iter().map(|r| r.to_string()).collect(),
            None => std::iter::once(base.guild_id.to_string()).chain(m.roles.iter().map(|r| r.to_string())).collect(),
        });
    }
    if (trigger == "ROLE_GAINED" || trigger == "ROLE_LOST") && base.role_ids.is_some() {
        f.changed_role_ids = base.role_ids.clone();
    }
    if trigger == "BUTTON_CLICKED" {
        f.button = base.button.clone();
    }
    if trigger == "REACTION_ADDED"
        && let Some(r) = &base.reaction
    {
        f.reaction_key = Some(strip_fe0f(&r.key));
    }
    if let Some(msg) = &base.message {
        if T_CONTENT.contains(&trigger) {
            f.content = Some(msg.content.clone());
            f.attachments = Some(msg.attachments.iter().map(|a| AttachmentFact { name: Some(a.filename.clone()), content_type: a.content_type.clone() }).collect());
            f.is_reply = Some(msg.kind == MessageType::InlineReply);
        }
        if T_COUNTS.contains(&trigger) {
            let mut counts = std::collections::HashMap::new();
            for r in &msg.reactions {
                let key = reaction_key_of(&r.reaction_type);
                if !key.is_empty() {
                    *counts.entry(key).or_insert(0.0) += r.count as f64;
                }
            }
            f.reaction_counts = Some(counts);
        }
    }
    f
}

// ── Variables ────────────────────────────────────────────────────────────────

/// The thread an automation works in (it may only exist after `CREATE_THREAD` ran).
pub type ThreadCell = Arc<Mutex<Option<GuildChannel>>>;

/// `message.url`.
pub fn message_url(guild_id: GuildId, message: &Message) -> String {
    format!("https://discord.com/channels/{guild_id}/{}/{}", message.channel_id, message.id)
}

/// `buildVars(base, run)`: the feature variables (`ctx.vars`); `{thread}` and `{thread.name}` read the thread when
/// they are rendered.
pub fn build_vars(base: &Base, automation_name: &str, thread: &ThreadCell, guild: Option<&Guild>) -> Vec<(String, VarValue)> {
    let mut vars: Vec<(String, VarValue)> = vec![
        ("automation".into(), VarValue::Text(automation_name.to_string())),
        ("reaction".into(), VarValue::Text(base.reaction.as_ref().map(|r| r.display.clone()).unwrap_or_default())),
    ];
    if let Some(msg) = &base.message {
        vars.push(("message.content".into(), VarValue::Text(clamp_text(&msg.content, 1500))));
        vars.push(("message.link".into(), VarValue::Text(message_url(base.guild_id, msg))));
        vars.push(("message.id".into(), VarValue::Text(msg.id.to_string())));
        vars.push(("message.author".into(), VarValue::Text(format!("<@{}>", msg.author.id))));
        vars.push(("message.author.name".into(), VarValue::Text(msg.author.name.clone())));
    }
    if let Some(old) = &base.old_content {
        vars.push(("message.old_content".into(), VarValue::Text(clamp_text(old, 1500))));
    }
    if let Some(role_id) = base.role_ids.as_ref().and_then(|r| r.first()) {
        vars.push(("role".into(), VarValue::Text(format!("<@&{role_id}>"))));
        vars.push(("role.id".into(), VarValue::Text(role_id.clone())));
        let name = role_id
            .parse::<u64>()
            .ok()
            .filter(|v| *v != 0)
            .and_then(|v| guild.and_then(|g| g.roles.get(&RoleId::new(v))))
            .map(|r| r.name.clone())
            .unwrap_or_default();
        vars.push(("role.name".into(), VarValue::Text(name)));
    }
    if let Some(v) = &base.voice_channel {
        vars.push(("voice".into(), VarValue::Text(format!("<#{}>", v.id))));
        vars.push(("voice.name".into(), VarValue::Text(v.name.clone())));
    }
    let t1 = thread.clone();
    vars.push(("thread".into(), VarValue::Lazy(Box::new(move || Ok(Some(lock(&t1).as_ref().map(|t| format!("<#{}>", t.id)).unwrap_or_default()))))));
    let t2 = thread.clone();
    vars.push(("thread.name".into(), VarValue::Lazy(Box::new(move || Ok(Some(lock(&t2).as_ref().map(|t| t.name.clone()).unwrap_or_default()))))));
    vars
}

// ── Execution ────────────────────────────────────────────────────────────────

struct Run {
    automation: AutomationLite,
    depth: u32,
    thread: ThreadCell,
    message_deleted: bool,
    settings: Option<Settings>,
    guild: Option<Guild>,
    bot_id: UserId,
}

impl Run {
    fn thread(&self) -> Option<GuildChannel> {
        lock(&self.thread).clone()
    }

    fn set_thread(&self, t: Option<GuildChannel>) {
        *lock(&self.thread) = t;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Done,
    Skipped,
}

type ActionResult = Result<Outcome, ActionError>;

/// `recordProblem(a, i, text, guildId)`: logs (at most once per action per 10 min) and stores the last problem for
/// `/automation info`.
async fn record_problem(app: &App, a: &AutomationLite, i: usize, text: &str, guild_id: GuildId) {
    if text.is_empty() || !warn_allowed(app, &format!("{}:{i}", a.id)) {
        return;
    }
    let action_type = a.actions.get(i).and_then(|x| x.get("type")).and_then(Value::as_str);
    bot_util::log_warn!(format!("automations: {guild_id} \"{}\" action {} ({}): {text}", a.name, i + 1, action_type.unwrap_or("undefined")));
    let label = action_type.and_then(action_def).map_or("Action", |d| d.label);
    let at = bot_util::js::to_iso_string(now_ms(app) as f64).unwrap_or_default();
    // Problem key writes ignore failures: it is a diagnostic shown by /automation info, nothing depends on it.
    let value = json!({ "at": at, "text": clamp_text(&format!("Action {} ({label}): {text}", i + 1), 300) });
    app.cache().set(&problem_key(&a.id), &value, PROBLEM_TTL_SECONDS).await;
}

fn reason_for(a: &AutomationLite) -> String {
    clamp_text(&format!("Automation: {}", a.name), 512)
}

fn render_context(base: &Base, run: &Run, channel: Option<&GuildChannel>) -> RenderContext {
    let guild = run.guild.as_ref();
    RenderContext {
        member: base.member.as_ref().map(|m| placeholder_member(m, guild)),
        user: base.user.as_ref().map(placeholder_user).or_else(|| base.member.as_ref().map(|m| placeholder_user(&m.user))),
        guild: guild.map(placeholder_guild),
        // {channel} is where it happened, or the target channel for triggers that have no channel.
        channel: base.channel.as_ref().or(channel).map(placeholder_channel),
        vars: Some(build_vars(base, &run.automation.name, &run.thread, guild)),
        args: None,
    }
}

fn render_text(template: &str, base: &Base, run: &Run, max: usize, channel: Option<&GuildChannel>) -> String {
    render_placeholders(template, &render_context(base, run, channel), Some(max))
}

/// `allowedMentionsFor(channel, guild, template)`: role mentions typed by the admin ping only when the bot may ping
/// them there; users always may.
pub fn allowed_mentions_for(guild: Option<&Guild>, channel: Option<&GuildChannel>, bot_id: UserId, template: &str) -> CreateAllowedMentions {
    let ids = role_mention_ids(template);
    let base = CreateAllowedMentions::new().all_users(true);
    let (Some(g), Some(ch)) = (guild, channel) else { return base };
    if ids.is_empty() {
        return base;
    }
    let me = g.members.get(&bot_id);
    let executor = me.map(|m| (bot_id, m.roles.as_slice()));
    let p = pingable_role_ids(g, ch, executor, &ids);
    if p.allowed.is_empty() {
        return base;
    }
    base.roles(p.allowed.iter().filter_map(|id| id.parse::<u64>().ok().filter(|v| *v != 0)).map(RoleId::new))
}

fn button_components(automation_id: &str, button: Option<&Value>, base: &Base, run: &Run, channel: Option<&GuildChannel>) -> Vec<CreateActionRow> {
    let Some(button) = button.filter(|b| b.is_object()) else { return Vec::new() };
    let rendered = render_text(sfield(button, "label"), base, run, BUTTON_LABEL_MAX, channel);
    let label = bot_util::js::trim(&rendered);
    let label = clamp_text(if label.is_empty() { "Click" } else { label }, BUTTON_LABEL_MAX);
    let b = match button.get("url").and_then(Value::as_str) {
        Some(url) => CreateButton::new_link(url).label(label),
        None => {
            let style = match sfield(button, "style") {
                "SECONDARY" => ButtonStyle::Secondary,
                "SUCCESS" => ButtonStyle::Success,
                "DANGER" => ButtonStyle::Danger,
                _ => ButtonStyle::Primary,
            };
            let index = js_string(button.get("index"));
            let id = bot_config::build_custom_id("automation", &[automation_id, index.as_str()]).unwrap_or_else(|_| format!("automation:{automation_id}:{index}"));
            CreateButton::new(id).style(style).label(label)
        }
    };
    vec![CreateActionRow::Buttons(vec![b])]
}

fn channel_of(run: &Run, id: &str) -> Option<GuildChannel> {
    let g = run.guild.as_ref()?;
    find_channel(g, ChannelId::new(id.parse::<u64>().ok().filter(|v| *v != 0)?)).cloned()
}

fn channel_for(action: &Value, base_channel: Option<&GuildChannel>, run: &Run) -> Option<GuildChannel> {
    match action.get("channelId").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        Some(id) => channel_of(run, id),
        None => base_channel.cloned(),
    }
}

/// `usableTarget`: the send target, or `None` after recording why it cannot be used.
async fn usable_target(app: &App, action: &Value, base_channel: Option<&GuildChannel>, base: &Base, run: &Run, i: usize, needs: &[&str]) -> Option<GuildChannel> {
    let Some(ch) = channel_for(action, base_channel, run) else {
        let has_id = action.get("channelId").and_then(Value::as_str).is_some_and(|s| !s.is_empty());
        record_problem(app, &run.automation, i, if has_id { "The channel was deleted" } else { "There is no channel for this action" }, base.guild_id).await;
        return None;
    };
    let check = check_channel_usable_for(app, base.guild_id, &ch, needs);
    if !check.ok {
        record_problem(app, &run.automation, i, check.reason.as_deref().unwrap_or_default(), base.guild_id).await;
        return None;
    }
    Some(ch)
}

async fn missing_perms(app: &App, run: &Run, base: &Base, i: usize, channel: Option<&GuildChannel>, perms: &[&str]) -> bool {
    let missing = match channel {
        Some(c) => missing_bot_permissions(app, PermTarget::Channel(base.guild_id, c.id), perms),
        None => perms.iter().map(|p| p.to_string()).collect(),
    };
    if missing.is_empty() {
        return false;
    }
    let where_ = channel.map_or_else(|| "undefined".to_string(), |c| c.id.to_string());
    record_problem(app, &run.automation, i, &format!("I need {} in <#{where_}>.", permission_names(&missing).join(", ")), base.guild_id).await;
    true
}

/// `{message.content}` / `{message.old_content}` in any spelling.
pub fn copies_message(texts: &[&str]) -> bool {
    texts.iter().any(|t| {
        let lower = t.to_lowercase();
        let b = lower.as_bytes();
        let mut i = 0;
        while let Some(pos) = lower[i..].find('{') {
            let mut j = i + pos + 1;
            while j < b.len() && bot_util::js::is_js_space(lower[j..].chars().next().unwrap_or('x')) {
                j += lower[j..].chars().next().map_or(1, char::len_utf8);
            }
            let rest = &lower[j..];
            let after = rest.strip_prefix("message.").and_then(|r| r.strip_prefix("old_").or(Some(r))).and_then(|r| r.strip_prefix("content"));
            if let Some(after) = after {
                let trimmed = bot_util::js::trim_start(after);
                if trimmed.starts_with('}') {
                    return true;
                }
            }
            i = i + pos + 1;
        }
        false
    })
}

/// The run-time privacy rule: a message in a channel @everyone cannot view is never copied into one it can.
async fn leaks_private_message(app: &App, run: &Run, base: &Base, i: usize, target: &GuildChannel, copies: bool) -> bool {
    let Some(message) = &base.message else { return false };
    if !copies {
        return false;
    }
    let source = base.channel.clone().or_else(|| channel_of(run, &message.channel_id.to_string()));
    let Some(g) = run.guild.as_ref() else { return false };
    if !bot_core::privacy_guards::copy_leaks_private(g, source.as_ref(), Some(target)) {
        return false;
    }
    let text = format!("Skipped: the message is in a channel @everyone cannot see, but <#{}> is visible to @everyone.", target.id);
    record_problem(app, &run.automation, i, &text, base.guild_id).await;
    true
}

/// `repostEmbed(message, base, run)`.
fn repost_embed(message: &Message, base: &Base, run: &Run) -> CreateEmbed {
    let author = &message.author;
    let display = match base.member.as_ref().filter(|m| m.user.id == author.id) {
        Some(m) => bot_core::adapters::member_display_name(m),
        None => message.member.as_ref().and_then(|m| m.nick.clone()).unwrap_or_else(|| user_display_name(author)),
    };
    let description = clamp_text(&message.content, 4096);
    let mut embed = bot_core::embed_builder::guild_embed(run.settings.as_deref(), None, (!description.is_empty()).then_some(description.as_str()), None);
    let icon = user_display_avatar_url_with(author, ImageUrlOptions::new("png", Some(128)));
    embed = embed.author(CreateEmbedAuthor::new(clamp_text(if display.is_empty() { "Unknown" } else { &display }, 256)).icon_url(icon));
    let image = message
        .attachments
        .iter()
        .position(|a| attachment_kind(Some(&a.filename), a.content_type.as_deref()) == Some("IMAGE") && !a.filename.starts_with("SPOILER_"));
    if let Some(k) = image {
        embed = embed.image(message.attachments[k].url.clone());
    }
    let others: Vec<String> = message
        .attachments
        .iter()
        .enumerate()
        .filter(|(k, _)| Some(*k) != image)
        .map(|(_, a)| format!("[{}]({})", clamp_text(if a.filename.is_empty() { "file" } else { &a.filename }, 40), a.url))
        .collect();
    if !others.is_empty() {
        embed = embed.field("Attachments", join_with_limit(&others, "\n", 1024, bot_util::text::default_more), false);
    }
    let channel_name = channel_of(run, &message.channel_id.to_string()).map(|c| c.name).or_else(|| base.channel.as_ref().map(|c| c.name.clone()));
    embed = embed.footer(CreateEmbedFooter::new(clamp_text(&format!("#{}", channel_name.as_deref().unwrap_or("unknown")), 128)));
    embed.timestamp(message.timestamp)
}

fn stored_reaction_type(stored: &str) -> ReactionType {
    match parse_custom_emoji(stored) {
        Some((animated, name, id)) => ReactionType::Custom { animated, id: EmojiId::new(id.parse().unwrap_or(1)), name: Some(name.to_string()) },
        None => ReactionType::Unicode(stored.to_string()),
    }
}

fn archive_duration(minutes: i64) -> AutoArchiveDuration {
    match minutes {
        60 => AutoArchiveDuration::OneHour,
        4320 => AutoArchiveDuration::ThreeDays,
        10080 => AutoArchiveDuration::OneWeek,
        _ => AutoArchiveDuration::OneDay,
    }
}

fn is_text_or_announcement(kind: ChannelType) -> bool {
    matches!(kind, ChannelType::Text | ChannelType::News)
}

async fn fetch_thread_of(app: &App, channel: ChannelId, message: MessageId) -> Option<GuildChannel> {
    let http = app.http()?;
    http.get_message(channel, message).await.ok().and_then(|m| m.thread)
}

async fn run_create_thread(app: &App, action: &Value, run: &mut Run, base: &Base, i: usize) -> ActionResult {
    let Some(http) = app.http() else { return Err(ActionError::other("Discord is not connected")) };
    let rendered = render_text(sfield(action, "name"), base, run, THREAD_NAME_MAX, None);
    let name = bot_util::js::trim(&rendered);
    let name = if name.is_empty() { "Thread".to_string() } else { name.to_string() };
    let reason = reason_for(&run.automation);
    let minutes = js_int(action.get("autoArchiveMinutes").unwrap_or(&Value::Null)).unwrap_or(1440);
    if let Some(id) = action.get("channelId").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        let Some(ch) = channel_of(run, id) else {
            record_problem(app, &run.automation, i, "The channel was deleted", base.guild_id).await;
            return Ok(Outcome::Skipped);
        };
        if !is_text_or_announcement(ch.kind) {
            record_problem(app, &run.automation, i, "That channel cannot hold threads", base.guild_id).await;
            return Ok(Outcome::Skipped);
        }
        if missing_perms(app, run, base, i, Some(&ch), &["ViewChannel", "CreatePublicThreads", "SendMessagesInThreads"]).await {
            return Ok(Outcome::Skipped);
        }
        if leaks_private_message(app, run, base, i, &ch, copies_message(&[sfield(action, "name")])).await {
            return Ok(Outcome::Skipped);
        }
        let kind = if ch.kind == ChannelType::News { ChannelType::NewsThread } else { ChannelType::PublicThread };
        let builder = CreateThread::new(name).auto_archive_duration(archive_duration(minutes)).kind(kind).audit_log_reason(&reason);
        let thread = ch.id.create_thread(http.as_ref(), builder).await?;
        bot_core::discord::cache_created_channel(app, &thread);
        run.set_thread(Some(thread));
        return Ok(Outcome::Done);
    }
    let Some(message) = &base.message else { return Ok(Outcome::Skipped) };
    let Some(channel) = base.channel.as_ref().filter(|c| is_text_or_announcement(c.kind)) else { return Ok(Outcome::Skipped) };
    if run.message_deleted {
        return Ok(Outcome::Skipped);
    }
    let has_thread = message.thread.is_some() || message.flags.is_some_and(|f| f.contains(MessageFlags::HAS_THREAD));
    if has_thread {
        let thread = match &message.thread {
            Some(t) => Some(t.clone()),
            None => fetch_thread_of(app, message.channel_id, message.id).await,
        };
        let done = thread.is_some();
        run.set_thread(thread);
        return Ok(if done { Outcome::Done } else { Outcome::Skipped });
    }
    if missing_perms(app, run, base, i, Some(channel), &["ViewChannel", "CreatePublicThreads", "ReadMessageHistory"]).await {
        return Ok(Outcome::Skipped);
    }
    let builder = CreateThread::new(name).auto_archive_duration(archive_duration(minutes)).audit_log_reason(&reason);
    match message.channel_id.create_thread_from_message(http.as_ref(), message.id, builder).await {
        Ok(thread) => {
            bot_core::discord::cache_created_channel(app, &thread);
            run.set_thread(Some(thread));
        }
        Err(e) => {
            if bot_core::discord::error_code(&e) != Some(160004) {
                return Err(e.into());
            }
            run.set_thread(fetch_thread_of(app, message.channel_id, message.id).await);
        }
    }
    Ok(Outcome::Done)
}

async fn send_to(app: &App, channel: ChannelId, msg: CreateMessage) -> Result<(), ActionError> {
    let Some(http) = app.http() else { return Err(ActionError::other("Discord is not connected")) };
    channel.send_message(http.as_ref(), msg).await?;
    Ok(())
}

async fn run_send_message(app: &App, action: &Value, run: &mut Run, base: &Base, i: usize) -> ActionResult {
    let Some(ch) = usable_target(app, action, base.channel.as_ref(), base, run, i, &["ViewChannel", "SendMessages"]).await else { return Ok(Outcome::Skipped) };
    let label = action.get("button").map(|b| sfield(b, "label")).unwrap_or("");
    if leaks_private_message(app, run, base, i, &ch, copies_message(&[sfield(action, "content"), label])).await {
        return Ok(Outcome::Skipped);
    }
    let content = render_text(sfield(action, "content"), base, run, CONTENT_MAX, Some(&ch));
    if bot_util::js::trim(&content).is_empty() {
        return Ok(Outcome::Skipped); // Discord rejects an empty message
    }
    let msg = CreateMessage::new()
        .content(content)
        .components(button_components(&run.automation.id, action.get("button"), base, run, Some(&ch)))
        .allowed_mentions(allowed_mentions_for(run.guild.as_ref(), Some(&ch), run.bot_id, sfield(action, "content")));
    send_to(app, ch.id, msg).await?;
    Ok(Outcome::Done)
}

async fn run_send_embed(app: &App, action: &Value, run: &mut Run, base: &Base, i: usize) -> ActionResult {
    let name = sfield(action, "embedName");
    let Some(tpl) = feat_embeds::embed_service::get_saved_embed_template(app, &base.guild_id.to_string(), name).await else {
        record_problem(app, &run.automation, i, &format!("Saved embed \"{name}\" not found"), base.guild_id).await;
        return Ok(Outcome::Skipped);
    };
    let Some(ch) = usable_target(app, action, base.channel.as_ref(), base, run, i, &["ViewChannel", "SendMessages", "EmbedLinks"]).await else { return Ok(Outcome::Skipped) };
    if leaks_private_message(app, run, base, i, &ch, copies_message(&[&tpl.to_string()])).await {
        return Ok(Outcome::Skipped);
    }
    let default_color = run.settings.as_ref().and_then(|s| s.embed_color).map(|c| c as u32).unwrap_or(bot_config::colors::INFO);
    let opts = bot_core::custom_embed::RenderOptions {
        default_color: Some(default_color),
        allowed_mentions: Some(allowed_mentions_for(run.guild.as_ref(), Some(&ch), run.bot_id, tpl.get("content").and_then(Value::as_str).unwrap_or(""))),
        extra_components: Vec::new(),
    };
    let Some(rendered) = bot_core::custom_embed::render_template(&tpl, &render_context(base, run, Some(&ch)), opts) else { return Ok(Outcome::Skipped) };
    send_to(app, ch.id, rendered.to_payload().to_message()).await?;
    Ok(Outcome::Done)
}

async fn run_reply(app: &App, action: &Value, run: &mut Run, base: &Base, i: usize) -> ActionResult {
    let ch = base.channel.clone();
    let content = render_text(sfield(action, "content"), base, run, CONTENT_MAX, ch.as_ref());
    if bot_util::js::trim(&content).is_empty() {
        return Ok(Outcome::Skipped);
    }
    let components = button_components(&run.automation.id, action.get("button"), base, run, ch.as_ref());
    let mentions = allowed_mentions_for(run.guild.as_ref(), ch.as_ref(), run.bot_id, sfield(action, "content"));
    if run.automation.trigger == "BUTTON_CLICKED" {
        let Some(r) = &base.interaction else { return Ok(Outcome::Skipped) };
        let follow = CreateInteractionResponseFollowup::new().content(content).components(components).allowed_mentions(mentions).ephemeral(true);
        r.follow_up(follow).await.map_err(|e| ActionError::Api { code: e.code(), message: Some(e.to_string()) })?;
        return Ok(Outcome::Done);
    }
    let check = match &ch {
        Some(c) => check_channel_usable_for(app, base.guild_id, c, &["ViewChannel", "SendMessages", "ReadMessageHistory"]),
        None => bot_core::interaction_helpers::channel_usable_verdict(None, None, &[]),
    };
    let Some(ch) = ch.filter(|_| check.ok) else {
        record_problem(app, &run.automation, i, check.reason.as_deref().unwrap_or_default(), base.guild_id).await;
        return Ok(Outcome::Skipped);
    };
    let mut msg = CreateMessage::new().content(content).components(components).allowed_mentions(mentions);
    if let Some(m) = base.message.as_ref().filter(|_| !run.message_deleted) {
        msg = msg.reference_message(MessageReference::new(MessageReferenceKind::Default, m.channel_id).message_id(m.id).fail_if_not_exists(false));
    }
    send_to(app, ch.id, msg).await?;
    Ok(Outcome::Done)
}

async fn run_repost(app: &App, action: &Value, run: &mut Run, base: &Base, i: usize) -> ActionResult {
    let Some(message) = base.message.clone() else { return Ok(Outcome::Skipped) };
    let Some(ch) = usable_target(app, action, None, base, run, i, &["ViewChannel", "SendMessages", "EmbedLinks"]).await else { return Ok(Outcome::Skipped) };
    if leaks_private_message(app, run, base, i, &ch, true).await {
        return Ok(Outcome::Skipped);
    }
    let key = repost_key(&run.automation.id, &message.id.to_string());
    // Degrade decision: at most one repost per source message. setNX null (Redis down) falls back to this process's set
    // (this shard owns the guild), so a Redis outage can never repost the same message twice from here.
    let first = app.cache().set_nx(&key, "1", REPOST_DEDUPE_TTL_SECONDS).await;
    if first == Some(false) {
        return Ok(Outcome::Skipped);
    }
    if first.is_none() && !lock(&state(app).local_reposts).add(&key, (REPOST_DEDUPE_TTL_SECONDS * 1000) as i64, now_ms(app)) {
        return Ok(Outcome::Skipped);
    }
    let embed = repost_embed(&message, base, run);
    let components = if run.automation.trigger == "MESSAGE_DELETED" {
        Vec::new()
    } else {
        vec![CreateActionRow::Buttons(vec![CreateButton::new_link(message_url(base.guild_id, &message)).label("Jump to message")])]
    };
    let msg = CreateMessage::new().embed(embed).components(components).allowed_mentions(CreateAllowedMentions::new());
    if let Err(e) = send_to(app, ch.id, msg).await {
        // The send failed: release the dedupe key so a later trigger can try again.
        app.cache().del_one(&key).await;
        lock(&state(app).local_reposts).delete(&key);
        return Err(e);
    }
    Ok(Outcome::Done)
}

async fn run_pin(app: &App, run: &mut Run, base: &Base, i: usize) -> ActionResult {
    let Some(message) = &base.message else { return Ok(Outcome::Skipped) };
    if run.message_deleted || message.pinned {
        return Ok(Outcome::Skipped);
    }
    // The JS also needs PinMessages here. serenity drops that bit (1 << 51) when it reads roles and overwrites, so the
    // cache can never show it; Discord's own refusal (50013) is recorded as the problem instead.
    if missing_perms(app, run, base, i, base.channel.as_ref(), &["ViewChannel", "ReadMessageHistory"]).await {
        return Ok(Outcome::Skipped);
    }
    let Some(http) = app.http() else { return Err(ActionError::other("Discord is not connected")) };
    http.pin_message(message.channel_id, message.id, Some(&reason_for(&run.automation))).await?;
    Ok(Outcome::Done)
}

async fn run_delete(app: &App, run: &mut Run, base: &Base, i: usize) -> ActionResult {
    let Some(message) = &base.message else { return Ok(Outcome::Skipped) };
    if run.message_deleted {
        return Ok(Outcome::Skipped);
    }
    if message.author.id != run.bot_id && missing_perms(app, run, base, i, base.channel.as_ref(), &["ViewChannel", "ManageMessages"]).await {
        return Ok(Outcome::Skipped);
    }
    // Logging and starboard read this mark: an automation delete is not a member delete.
    bot_core::deletion_tracker::mark_bot_deletion(app, &[message.id.to_string()], Some("automations"), None);
    let Some(http) = app.http() else { return Err(ActionError::other("Discord is not connected")) };
    if let Err(e) = http.delete_message(message.channel_id, message.id, None).await
        && bot_core::discord::error_code(&e) != Some(10008)
    {
        return Err(e.into());
    }
    run.message_deleted = true;
    Ok(Outcome::Done)
}

async fn run_react(app: &App, action: &Value, run: &mut Run, base: &Base, i: usize) -> ActionResult {
    let Some(message) = &base.message else { return Ok(Outcome::Skipped) };
    if run.message_deleted {
        return Ok(Outcome::Skipped);
    }
    let emoji = sfield(action, "emoji");
    let key = emoji_key_of(emoji);
    if message.reactions.iter().any(|r| r.me && reaction_key_of(&r.reaction_type) == key) {
        return Ok(Outcome::Skipped);
    }
    if missing_perms(app, run, base, i, base.channel.as_ref(), &["ViewChannel", "AddReactions", "ReadMessageHistory"]).await {
        return Ok(Outcome::Skipped);
    }
    let Some(http) = app.http() else { return Err(ActionError::other("Discord is not connected")) };
    http.create_reaction(message.channel_id, message.id, &stored_reaction_type(emoji)).await?;
    Ok(Outcome::Done)
}

async fn run_remove_reaction(app: &App, action: &Value, run: &mut Run, base: &Base, i: usize) -> ActionResult {
    let Some(message) = &base.message else { return Ok(Outcome::Skipped) };
    if run.message_deleted {
        return Ok(Outcome::Skipped);
    }
    let key = match action.get("emoji").and_then(Value::as_str).filter(|e| !e.is_empty()) {
        Some(e) => Some(emoji_key_of(e)),
        None => base.reaction.as_ref().map(|r| r.key.clone()),
    };
    let (Some(key), Some(user_id)) = (key, base.user.as_ref().map(|u| u.id)) else { return Ok(Outcome::Skipped) };
    let Some(reaction) = message.reactions.iter().find(|r| reaction_key_of(&r.reaction_type) == key) else { return Ok(Outcome::Skipped) };
    if missing_perms(app, run, base, i, base.channel.as_ref(), &["ViewChannel", "ManageMessages"]).await {
        return Ok(Outcome::Skipped);
    }
    let Some(http) = app.http() else { return Err(ActionError::other("Discord is not connected")) };
    http.delete_reaction(message.channel_id, message.id, user_id, &reaction.reaction_type).await?;
    Ok(Outcome::Done)
}

async fn run_role_change(app: &App, give: bool, action: &Value, run: &mut Run, base: &Base, i: usize) -> ActionResult {
    let Some(member) = &base.member else { return Ok(Outcome::Skipped) };
    let a = run.automation.clone();
    // Captcha verification must not be bypassed by an automation: skip role gifts while it is pending.
    if give && a.trigger == "MEMBER_JOINED" && base.captcha_pending {
        bot_util::log_debug!(format!("automations: \"{}\" skipped GIVE_ROLES for {}: verification is pending", a.name, member.user.id));
        return Ok(Outcome::Skipped);
    }
    let reason = reason_for(&a);
    let mut marked = false;
    let mut changed = 0;
    // One role at a time: the array form PATCHes the whole role list.
    for id in str_list(action, "roleIds") {
        let rid = id.parse::<u64>().ok().filter(|v| *v != 0).map(RoleId::new);
        let role = rid.and_then(|r| run.guild.as_ref().and_then(|g| g.roles.get(&r)).cloned());
        let Some(role) = role else {
            record_problem(app, &a, i, "The role was deleted", base.guild_id).await;
            continue;
        };
        let check = match run.guild.as_ref() {
            Some(g) => check_assignable_role(g, Some(&role), run.bot_id, None, false),
            None => continue,
        };
        if !check.ok {
            record_problem(app, &a, i, check.reason.as_deref().unwrap_or_default(), base.guild_id).await;
            continue;
        }
        let has = member.roles.contains(&role.id);
        if if give { has } else { !has } {
            continue;
        }
        if !marked {
            // Before the first Discord call: the guildMemberUpdate it causes reads the depth.
            lock(&state(app).causes).mark(&base.guild_id.to_string(), &member.user.id.to_string(), run.depth + 1, now_ms(app));
            marked = true;
        }
        let Some(http) = app.http() else {
            record_problem(app, &a, i, "Discord is not connected", base.guild_id).await;
            continue;
        };
        let res = if give {
            http.add_member_role(base.guild_id, member.user.id, role.id, Some(&reason)).await
        } else {
            http.remove_member_role(base.guild_id, member.user.id, role.id, Some(&reason)).await
        };
        match res {
            Ok(()) => changed += 1,
            Err(e) => {
                let err = ActionError::from_serenity(&e);
                match discord_error_text(&err) {
                    Some(text) => record_problem(app, &a, i, &text, base.guild_id).await,
                    None => bot_util::log_debug!(format!("automations: role {id} skipped ({})", err.code().map_or("undefined".into(), |c| c.to_string()))),
                }
            }
        }
    }
    Ok(if changed > 0 { Outcome::Done } else { Outcome::Skipped })
}

async fn run_reward(app: &App, grant: bool, kind: &str, action: &Value, run: &mut Run, base: &Base, i: usize) -> ActionResult {
    let Some(user_id) = base.member.as_ref().map(|m| m.user.id).or_else(|| base.user.as_ref().map(|u| u.id)) else { return Ok(Outcome::Skipped) };
    let a = run.automation.clone();
    // Level-up role sync can fire ROLE_GAINED: mark the chain depth before the XP call.
    if kind == "xp" {
        lock(&state(app).causes).mark(&base.guild_id.to_string(), &user_id.to_string(), run.depth + 1, now_ms(app));
    }
    let rk = if kind == "xp" { bot_core::rewards::RewardKind::Xp } else { bot_core::rewards::RewardKind::Coins };
    let amount = js_int(action.get("amount").unwrap_or(&Value::Null)).unwrap_or(0);
    let args = bot_core::rewards::RewardArgs::new(base.guild_id, user_id.to_string(), amount)
        .reason(format!("automation:{}", a.name))
        .member(base.member.clone())
        .channel(base.channel.as_ref().map(|c| c.id));
    let res = if grant { app.rewards().grant(app, rk, args).await } else { app.rewards().take(app, rk, args).await };
    if res.ok {
        return Ok(Outcome::Done);
    }
    let pkg = if kind == "xp" { "Levels" } else { "Economy" };
    let op = if grant { "grant" } else { "take" };
    match res.reason.as_deref() {
        Some("insufficient") => bot_util::log_debug!(format!("automations: \"{}\" skipped {op} {kind}: insufficient balance", a.name)),
        Some("module-disabled") => record_problem(app, &a, i, &format!("The {pkg} module is off"), base.guild_id).await,
        Some("unavailable") => record_problem(app, &a, i, &format!("The {pkg} package is not installed"), base.guild_id).await,
        other => record_problem(app, &a, i, other.unwrap_or("The reward failed"), base.guild_id).await,
    }
    Ok(Outcome::Skipped)
}

async fn run_add_to_thread(app: &App, action: &Value, run: &mut Run, base: &Base, i: usize) -> ActionResult {
    let Some(thread) = run.thread() else { return Ok(Outcome::Skipped) };
    if thread.thread_metadata.is_some_and(|m| m.archived || m.locked) {
        return Ok(Outcome::Skipped);
    }
    let mut ids = str_list(action, "userIds");
    if ids.is_empty() {
        ids.extend(base.member.as_ref().map(|m| m.user.id).or_else(|| base.user.as_ref().map(|u| u.id)).map(|u| u.to_string()));
    }
    let Some(http) = app.http() else { return Err(ActionError::other("Discord is not connected")) };
    let mut added = 0;
    for id in ids {
        let Some(uid) = id.parse::<u64>().ok().filter(|v| *v != 0).map(UserId::new) else { continue };
        match http.add_thread_channel_member(thread.id, uid).await {
            Ok(()) => added += 1,
            Err(e) => {
                if let Some(text) = discord_error_text(&ActionError::from_serenity(&e)) {
                    record_problem(app, &run.automation, i, &text, base.guild_id).await;
                }
            }
        }
    }
    Ok(if added > 0 { Outcome::Done } else { Outcome::Skipped })
}

async fn run_send_in_thread(app: &App, action: &Value, run: &mut Run, base: &Base, i: usize) -> ActionResult {
    let Some(thread) = run.thread() else { return Ok(Outcome::Skipped) };
    let check = check_channel_usable_for(app, base.guild_id, &thread, &["ViewChannel", "SendMessages"]);
    if !check.ok {
        record_problem(app, &run.automation, i, check.reason.as_deref().unwrap_or_default(), base.guild_id).await;
        return Ok(Outcome::Skipped);
    }
    let label = action.get("button").map(|b| sfield(b, "label")).unwrap_or("");
    if leaks_private_message(app, run, base, i, &thread, copies_message(&[sfield(action, "content"), label])).await {
        return Ok(Outcome::Skipped);
    }
    let content = render_text(sfield(action, "content"), base, run, CONTENT_MAX, Some(&thread));
    if bot_util::js::trim(&content).is_empty() {
        return Ok(Outcome::Skipped);
    }
    let msg = CreateMessage::new()
        .content(content)
        .components(button_components(&run.automation.id, action.get("button"), base, run, Some(&thread)))
        .allowed_mentions(allowed_mentions_for(run.guild.as_ref(), Some(&thread), run.bot_id, sfield(action, "content")));
    send_to(app, thread.id, msg).await?;
    Ok(Outcome::Done)
}

async fn run_action(app: &App, action: &Value, run: &mut Run, base: &Base, i: usize) -> Option<ActionResult> {
    let t = action.get("type").and_then(Value::as_str)?;
    Some(match t {
        "CREATE_THREAD" => run_create_thread(app, action, run, base, i).await,
        "SEND_MESSAGE" => run_send_message(app, action, run, base, i).await,
        "SEND_EMBED" => run_send_embed(app, action, run, base, i).await,
        "REPLY" => run_reply(app, action, run, base, i).await,
        "REPOST" => run_repost(app, action, run, base, i).await,
        "PIN" => run_pin(app, run, base, i).await,
        "DELETE" => run_delete(app, run, base, i).await,
        "REACT" => run_react(app, action, run, base, i).await,
        "REMOVE_REACTION" => run_remove_reaction(app, action, run, base, i).await,
        "GIVE_ROLES" => run_role_change(app, true, action, run, base, i).await,
        "TAKE_ROLES" => run_role_change(app, false, action, run, base, i).await,
        "ADD_TO_THREAD" => run_add_to_thread(app, action, run, base, i).await,
        "SEND_IN_THREAD" => run_send_in_thread(app, action, run, base, i).await,
        "GIVE_COINS" => run_reward(app, true, "coins", action, run, base, i).await,
        "TAKE_COINS" => run_reward(app, false, "coins", action, run, base, i).await,
        "GIVE_XP" => run_reward(app, true, "xp", action, run, base, i).await,
        "TAKE_XP" => run_reward(app, false, "xp", action, run, base, i).await,
        _ => return None,
    })
}

/// `executeAutomation(a, base)`: runs one automation's actions in order; each action is isolated. Never fails.
pub async fn execute_automation(app: &App, a: &AutomationLite, base: &Base) {
    let settings = match &base.get_settings {
        Some(f) => f().await,
        None => app.settings().get_settings_safe(&base.guild_id.to_string()).await,
    };
    let initial_thread = base.thread.clone().or_else(|| base.channel.clone().filter(is_thread));
    let mut run = Run {
        automation: a.clone(),
        depth: base.depth,
        thread: Arc::new(Mutex::new(initial_thread)),
        message_deleted: base.message_gone,
        settings,
        guild: bot_core::interaction_helpers::cached_guild(app, base.guild_id),
        bot_id: bot_user_id(app),
    };
    let mut attempted = 0;
    for (i, action) in a.actions.iter().enumerate() {
        match run_action(app, action, &mut run, base, i).await {
            None => {}
            Some(Ok(Outcome::Skipped)) => {}
            Some(Ok(Outcome::Done)) => attempted += 1,
            Some(Err(err)) => {
                attempted += 1;
                match discord_error_text(&err) {
                    Some(text) => record_problem(app, a, i, &text, base.guild_id).await,
                    None => bot_util::log_debug!(format!(
                        "automations: \"{}\" action {} skipped ({})",
                        a.name,
                        i + 1,
                        err.code().map_or("undefined".into(), |c| c.to_string())
                    )),
                }
            }
        }
    }
    if attempted > 0 {
        bump_uses(app, &a.id);
    }
}

// ── Dispatch ─────────────────────────────────────────────────────────────────

async fn take_cooldown(app: &App, guild_id: &str, automation_id: &str, user_id: &str, seconds: i64) -> bool {
    let key = cooldown_key(guild_id, automation_id, user_id);
    // Degrade decision: setNX true -> run, false -> skip, None (Redis down) -> the in-process set. A guild's events
    // always land on its own shard, so that is consistent; the cooldown never fails open to "no cooldown".
    match app.cache().set_nx(&key, "1", seconds).await {
        Some(true) => true,
        Some(false) => false,
        None => lock(&state(app).local_cooldowns).add(&key, seconds * 1000, now_ms(app)),
    }
}

/// Undoes [`take_cooldown`] for a run that did not start.
async fn give_back_cooldown(app: &App, guild_id: &str, automation_id: &str, user_id: &str) {
    let key = cooldown_key(guild_id, automation_id, user_id);
    lock(&state(app).local_cooldowns).delete(&key);
    app.cache().del_one(&key).await;
}

/// `enqueueRun(guildId, fn)`: `None` when the guild's queue is full (the run is dropped), else the run's future (the
/// run is already queued and runs whether or not it is awaited).
fn enqueue_run(app: &App, guild_id: &str, a: AutomationLite, base: Base) -> Option<impl std::future::Future<Output = ()> + Send + 'static> {
    let st = state(app);
    {
        let mut pending = lock(&st.pending);
        let n = pending.entry(guild_id.to_string()).or_insert(0);
        if *n >= MAX_PENDING_RUNS {
            if warn_allowed(app, &format!("queue:{guild_id}")) {
                bot_util::log_warn!(format!("automations: run queue is full in {guild_id}; dropping a run"));
            }
            return None;
        }
        *n += 1;
    }
    let (app2, g) = (app.clone(), guild_id.to_string());
    let fut = st.guild_queue.run(guild_id, move || async move {
        let (app3, a2, b2) = (app2.clone(), a, base);
        let res = bot_util::async_util::with_timeout(
            async move {
                execute_automation(&app3, &a2, &b2).await;
                Ok::<(), std::convert::Infallible>(())
            },
            RUN_TIMEOUT_MS,
            "automation run",
        )
        .await;
        {
            let st = state(&app2);
            let mut pending = lock(&st.pending);
            let left = pending.get(&g).copied().unwrap_or(1).saturating_sub(1);
            if left == 0 {
                pending.remove(&g);
            } else {
                pending.insert(g.clone(), left);
            }
        }
        if let Err(e) = res {
            bot_util::log_error!("automations: run failed", bot_util::logger::LogArg::from(e.to_string()));
        }
    });
    Some(fut)
}

/// `dispatch(trigger, base, { awaitRuns })`: finds the automations of a trigger whose conditions pass and queues their
/// runs. Never fails.
pub async fn dispatch(app: &App, trigger: &str, mut base: Base, await_runs: bool) {
    let Some(trigger) = trigger_key(trigger) else { return };
    let guild_id = base.guild_id.to_string();
    let Some(by_trigger) = get_automations(app, &guild_id).await else { return }; // fail-closed while Postgres is unreachable
    let autos: Vec<AutomationLite> = by_trigger.get(trigger).cloned().unwrap_or_default();
    if autos.is_empty() {
        return;
    }
    if base.depth >= MAX_CHAIN_DEPTH {
        if state(app).chain_throttle.allow_at(&guild_id, now_ms(app)) {
            bot_util::log_warn!(format!("automations: chain depth {} reached in {guild_id}; stopping", base.depth));
        }
        return;
    }
    let facts0 = bot_core::interaction_helpers::with_cached_guild(app, base.guild_id, |g| build_facts(trigger, &base, Some(g)))
        .unwrap_or_else(|| build_facts(trigger, &base, None));
    let mut candidates: Vec<AutomationLite> = autos.into_iter().filter(|a| evaluate_conditions(&split_conditions(&a.conditions).0, &facts0)).collect();
    if candidates.is_empty() {
        return;
    }
    let mut facts = facts0;
    if trigger == "REACTION_ADDED" {
        // The message is fetched only now that a cheap condition set passed. Bot-authored messages are allowed here (the
        // reactor is never a bot, so there is no loop); system messages (joins, pins) are not.
        let msg = match (&base.message, &base.reaction_event) {
            (Some(m), _) => Some(m.clone()),
            (None, Some(ev)) => ev.ensure_message(app).await,
            (None, None) => None,
        };
        let Some(msg) = msg.filter(|m| !is_system_message(m)) else { return };
        base.message = Some(msg);
        facts = bot_core::interaction_helpers::with_cached_guild(app, base.guild_id, |g| build_facts(trigger, &base, Some(g)))
            .unwrap_or_else(|| build_facts(trigger, &base, None));
    }
    candidates.retain(|a| evaluate_conditions(&split_conditions(&a.conditions).1, &facts));

    for a in candidates {
        let cooled = a.cooldown_seconds > 0;
        let cooldown_user = facts.user_id.clone().unwrap_or_else(|| "none".into());
        if cooled && !take_cooldown(app, &guild_id, &a.id, &cooldown_user, a.cooldown_seconds).await {
            continue;
        }
        let allowed = lock(&state(app).budget).take(&guild_id, now_ms(app));
        if !allowed {
            // Nothing ran, so the member must not sit out a cooldown (up to 7 days) for a run that never happened.
            if cooled {
                give_back_cooldown(app, &guild_id, &a.id, &cooldown_user).await;
            }
            if warn_allowed(app, &format!("budget:{guild_id}")) {
                bot_util::log_warn!(format!("automations: run budget exceeded in {guild_id}"));
            }
            break;
        }
        let id = a.id.clone();
        match enqueue_run(app, &guild_id, a, base.clone()) {
            // The queue is full and the run was dropped: the cooldown goes back too.
            None => {
                if cooled {
                    give_back_cooldown(app, &guild_id, &id, &cooldown_user).await;
                }
            }
            Some(fut) => {
                if await_runs {
                    fut.await;
                }
            }
        }
    }
}

/// Waits until every run queued so far for the guild has finished (test seam: the JS tests flush the event loop).
pub async fn settle_runs(app: &App, guild_id: &str) {
    state(app).guild_queue.run(guild_id, || async {}).await;
}

/// The runs queued or running for a guild (`pending`).
pub fn pending_runs(app: &App, guild_id: &str) -> usize {
    lock(&state(app).pending).get(guild_id).copied().unwrap_or(0)
}

/// Test seam: blocks the guild's run queue until the returned sender is dropped or fired (the JS tests block a
/// `send()` instead). The blocking job is not counted in [`pending_runs`].
pub fn hold_run_queue(app: &App, guild_id: &str) -> tokio::sync::oneshot::Sender<()> {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    drop(state(app).guild_queue.run(guild_id, move || async move {
        let _ = rx.await;
    }));
    tx
}

/// JS `Number(s)` for the button index of a customId: trimmed, `''` is 0, `0x`/`0o`/`0b` integers, decimal and
/// exponent forms, `Infinity`; anything else is NaN (`None`).
fn js_number(s: &str) -> Option<f64> {
    let t = bot_util::js::trim(s);
    if t.is_empty() {
        return Some(0.0);
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
        if let Some(digits) = t.strip_prefix(prefix) {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return None;
            }
            return Some(digits.chars().fold(0.0, |acc, c| acc * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0))));
        }
    }
    let unsigned = t.strip_prefix(['+', '-']).unwrap_or(t);
    if unsigned == "Infinity" {
        return Some(if t.starts_with('-') { f64::NEG_INFINITY } else { f64::INFINITY });
    }
    // Rust also reads `inf`, `nan` and `infinity` in any case; JS does not.
    if !unsigned.starts_with(|c: char| c.is_ascii_digit() || c == '.') {
        return None;
    }
    t.parse::<f64>().ok()
}

/// The button number of `automation:{id}:{index}`: `Number(rawIndex)`, a whole number from 1 to 99.
pub fn button_index(raw: &str) -> Option<i64> {
    js_number(raw).filter(|n| n.is_finite() && n.fract() == 0.0).map(|n| n as i64).filter(|i| (1..=MAX_BUTTON_INDEX).contains(i))
}

/// discord.js `message.system`: any type but Default, Reply, ChatInputCommand and ContextMenuCommand.
pub fn is_system_message(m: &Message) -> bool {
    !matches!(m.kind, MessageType::Regular | MessageType::InlineReply | MessageType::ChatInputCommand | MessageType::ContextMenuCommand)
}

// ── Button handler ───────────────────────────────────────────────────────────

/// `onAutomationButton(interaction)`: the persistent `automation:{automationId}:{buttonIndex}` buttons. The click is
/// acknowledged first and changes nothing visible.
pub async fn on_automation_button(ctx: bot_core::interaction::ComponentCtx) -> bot_core::BotResult {
    let app = ctx.app.clone();
    let guild_id = ctx.guild_id().filter(|g| bot_core::interaction_helpers::with_cached_guild(&app, *g, |_| ()).is_some());
    let Some(guild_id) = guild_id else {
        let _ = ctx.reply_text("This button only works in a server.", true).await;
        return Ok(());
    };
    let (_, params) = ctx.parsed();
    let automation_id = params.first().cloned().unwrap_or_default();
    let raw_index = params.get(1).cloned().unwrap_or_default();
    let _ = ctx.defer_update().await;
    let Some(index) = button_index(&raw_index) else { return Ok(()) };
    if !is_automation_id(&automation_id) {
        return Ok(());
    }
    let mut base = Base::new("BUTTON_CLICKED", guild_id);
    base.member = ctx.member().cloned();
    base.user = Some(ctx.user().clone());
    base.channel = bot_core::interaction_helpers::cached_channel(&app, guild_id, ctx.interaction.channel_id);
    base.message = Some((*ctx.interaction.message).clone());
    base.interaction = Some(ctx.responder.clone());
    base.button = Some(ButtonFact { automation_id, index });
    dispatch(&app, "BUTTON_CLICKED", base, true).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{button_index, copies_message};

    #[test]
    fn button_index_reads_the_number_like_js_number() {
        // Expected values from Node: Number(x), then Number.isInteger(n) && 1 <= n <= 99.
        for (raw, want) in [
            ("5", Some(5)),
            (" 7 ", Some(7)),
            ("0x5", Some(5)),
            ("0b11", Some(3)),
            ("0o17", Some(15)),
            ("5.0", Some(5)),
            ("1e1", Some(10)),
            ("+4", Some(4)),
            ("99", Some(99)),
            ("100", None),
            ("0", None),
            ("", None),
            ("-1", None),
            ("1.5", None),
            ("inf", None),
            ("Infinity", None),
            ("nan", None),
            ("0x", None),
            ("-0x5", None),
            ("5a", None),
        ] {
            assert_eq!(button_index(raw), want, "{raw:?}");
        }
    }

    #[test]
    fn copies_message_matches_every_spelling() {
        assert!(copies_message(&["said: {message.content}"]));
        assert!(copies_message(&["said: { Message.Old_Content }"]));
        assert!(!copies_message(&["someone posted"]));
        assert!(!copies_message(&["{message.contents}"]));
        assert!(copies_message(&["{a} {message.content}"]));
    }
}
