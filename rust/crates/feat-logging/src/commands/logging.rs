//! Port of `src/commands/admin/logging.js`: `/logging enable | disable | status | channel | event | ignore add | ignore
//! remove | ignore list`. Writes go through the settings service only; the moderation log channel belongs to
//! `/modconfig logchannel`. An admin config command: public replies, works while the module is off.

use std::sync::{Arc, Mutex};

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::embed_builder::guild_embed;
use bot_core::interaction_helpers::{Choice, Payload, channel_usable_verdict, respond_autocomplete, safe_defer, safe_reply};
use bot_core::new_targets::{NewOptions, new_channel_option, resolve_new_options, reuse_note};
use bot_core::prelude::*;
use serde_json::{Value, json};
use serenity::all::{ChannelId, GuildChannel, GuildId, Permissions};
use serenity::builder::CreateEmbed;

use crate::logging_service::{
    HealthState, LOG_EVENTS, LogType, forget_dead_channels, get_log_channel_health, is_log_event_enabled, is_log_event_key, log_api, log_event, log_event_keys,
    toggle_log_events,
};

const LOG_CHANNEL_TYPES: [u8; 2] = [channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT];
const IGNORABLE_TYPES: [u8; 7] = [
    channel_type::GUILD_TEXT,
    channel_type::GUILD_ANNOUNCEMENT,
    channel_type::GUILD_VOICE,
    channel_type::GUILD_STAGE_VOICE,
    channel_type::GUILD_FORUM,
    channel_type::GUILD_MEDIA,
    channel_type::GUILD_CATEGORY,
];

/// `TYPE_CHOICES`.
pub const TYPE_CHOICES: [(&str, &str); 6] = [
    ("All log types (one channel for everything)", "all"),
    ("Messages (edits, deletes, invites)", "message"),
    ("Members (roles, nicknames, timeouts, bans)", "member"),
    ("Server (channels, roles, emojis, invites)", "server"),
    ("Voice (join, leave, move)", "voice"),
    ("Joins and leaves", "joinleave"),
];

/// `TYPE_VALUE_TO_KEYS[value]`.
pub fn type_value_to_keys(value: &str) -> Option<Vec<LogType>> {
    Some(match value {
        "all" => CONFIGURABLE_KEYS.to_vec(),
        "message" => vec![LogType::Message],
        "member" => vec![LogType::Member],
        "server" => vec![LogType::Server],
        "voice" => vec![LogType::Voice],
        "joinleave" => vec![LogType::JoinLeave],
        _ => return None,
    })
}

/// MOD is never offered.
pub const CONFIGURABLE_KEYS: [LogType; 5] = [LogType::Message, LogType::Member, LogType::Server, LogType::Voice, LogType::JoinLeave];
const BASE_NEEDS: [&str; 3] = ["ViewChannel", "SendMessages", "EmbedLinks"];
const MAX_IGNORED: usize = bot_config::limits::LOG_IGNORED_CHANNELS;

const ATTACH_HINT: &str = "ℹ️ Give me **Attach Files** there to get bulk-delete transcripts.";
const OFF_HINT: &str = "ℹ️ Audit logs are currently **off**. Turn them on with `/logging enable`.";

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("logging", "Audit logs: turn on/off, pick log channels, toggle events (MEE6 Moderator › Audit logs).")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(
            Opt::sub_command("enable", "Turn audit logging on.")
                .option(Opt::channel("channel", "Optional: send every log type to this channel.").channel_types(&LOG_CHANNEL_TYPES))
                .option(new_channel_option("channel", "channel")),
        )
        .option(Opt::sub_command("disable", "Turn audit logging off (settings are kept)."))
        .option(Opt::sub_command("status", "Show log channels, disabled events and ignored channels."))
        .option({
            let mut t = Opt::string("type", "Which logs go to the channel.").required(true);
            for (name, value) in TYPE_CHOICES {
                t = t.choice(name, value);
            }
            Opt::sub_command("channel", "Set or clear the channel for a log type.")
                .option(t)
                .option(Opt::channel("channel", "Log channel (leave empty to clear).").channel_types(&LOG_CHANNEL_TYPES))
                .option(new_channel_option("channel", "channel"))
        })
        .option(
            Opt::sub_command("event", "Turn one log event (or all) on or off.")
                .option(Opt::string("name", "Log event, or \"all\".").required(true).autocomplete(true).max_length(32))
                .option(Opt::string("state", "on or off").required(true).choice("on", "on").choice("off", "off")),
        )
        .option(
            Opt::sub_command_group("ignore", "Channels whose message events are never logged.")
                .option(
                    Opt::sub_command("add", "Stop logging message events in a channel or category.")
                        .option(Opt::channel("channel", "Channel or category to ignore.").required(true).channel_types(&IGNORABLE_TYPES)),
                )
                .option(
                    Opt::sub_command("remove", "Log message events in a channel again.")
                        .option(Opt::channel("channel", "Channel or category to stop ignoring.").required(true).channel_types(&IGNORABLE_TYPES)),
                )
                .option(Opt::sub_command("list", "List ignored channels.")),
        )
}

/// The command (`module 'logging'`, `gated = false`, ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Logging), execute)
        .gated(false)
        .permission_level(PermissionLevel::Admin)
        .autocomplete(|ctx| async move { autocomplete(ctx).await })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// The option's channel as a guild channel (cache, then REST).
async fn resolve_channel(app: &App, guild_id: GuildId, id: ChannelId) -> Option<GuildChannel> {
    if let Some(c) = bot_core::interaction_helpers::cached_channel(app, guild_id, id) {
        return Some(c);
    }
    log_api(app).fetch_channel(guild_id, id).await.ok().flatten()
}

/// The bot's permissions in a channel (cached or fetched), `None` when they cannot be computed.
fn bot_perms_in(app: &App, guild_id: GuildId, channel: &GuildChannel) -> Option<Permissions> {
    let d = app.discord()?;
    let bot = d.cache.current_user().id;
    let g = d.cache.guild(guild_id)?;
    let me = bot_core::permissions::me(&g, bot)?;
    bot_core::permissions::member_permissions_in(&g, channel, bot, &me.roles, true)
}

/// `describeUsable(channel, needs)`: `None` when the bot can post there, otherwise the sentence to show.
fn describe_usable(app: &App, guild_id: GuildId, channel: &GuildChannel) -> Option<String> {
    let u = channel_usable_verdict(Some(channel), bot_perms_in(app, guild_id, channel), &BASE_NEEDS);
    if u.ok {
        return None;
    }
    Some(if u.missing.is_empty() {
        u.reason.unwrap_or_else(|| format!("I cannot post in <#{}>.", channel.id))
    } else {
        format!("I need **{}** in <#{}> to post logs there.", bot_core::permissions::permission_names(&u.missing).join(", "), channel.id)
    })
}

fn lacks_attach(app: &App, guild_id: GuildId, channel: &GuildChannel) -> bool {
    !channel_usable_verdict(Some(channel), bot_perms_in(app, guild_id, channel), &["AttachFiles"]).ok
}

async fn reply(ctx: &CommandCtx, embed: CreateEmbed) -> BotResult {
    safe_reply(&ctx.responder, Payload::embed(embed)).await;
    Ok(())
}

fn patch_of(keys: &[LogType], value: Value) -> SettingsPatch {
    let mut p = SettingsPatch::new();
    for k in keys {
        p.insert(k.field(), value.clone());
    }
    p
}

// ── Subcommands ───────────────────────────────────────────────────────────────

async fn sub_enable(ctx: &CommandCtx, guild_id: GuildId) -> BotResult {
    let g = guild_id.to_string();
    let mut channel = None;
    if let Some(id) = ctx.get_channel_id("channel") {
        let resolved = resolve_channel(&ctx.app, guild_id, id).await;
        let problem = match &resolved {
            Some(c) => describe_usable(&ctx.app, guild_id, c),
            None => Some("I cannot see that channel.".into()),
        };
        if let Some(p) = problem {
            return reply(ctx, error_embed("Missing Permissions", Some(&p), None)).await;
        }
        channel = resolved;
    }

    let mut patch = SettingsPatch::new().set("loggingEnabled", true);
    if let Some(c) = &channel {
        for t in CONFIGURABLE_KEYS {
            patch.insert(t.field(), json!(c.id.to_string()));
        }
    }
    let settings = ctx.app.settings().update_settings(&g, patch).await?;
    forget_dead_channels(&ctx.app, &g, None);

    let configured: Vec<LogType> = CONFIGURABLE_KEYS.into_iter().filter(|k| k.channel_of(&settings).is_some()).collect();
    if channel.is_none() && configured.is_empty() {
        return reply(
            ctx,
            warn_embed(
                "Audit Logs Enabled — No Channel Yet",
                Some("Logging is on, but no log channel is set, so nothing will be posted. Use `/logging channel` or `/logging enable channel:#logs`."),
                None,
            ),
        )
        .await;
    }
    let mut lines: Vec<String> = match &channel {
        Some(c) => vec![format!("Every log type will be posted in <#{}>.", c.id)],
        None => configured.iter().map(|k| format!("**{}** → <#{}>", k.label(), k.channel_of(&settings).unwrap_or_default())).collect(),
    };
    for k in &configured {
        let health = get_log_channel_health(&ctx.app, guild_id, k.channel_of(&settings));
        if health.state != HealthState::Ok {
            lines.push(format!("⚠️ **{}**: {}", k.label(), health.text));
        }
    }
    if let Some(r) = reuse_note(ctx) {
        lines.push(r);
    }
    if let Some(c) = &channel
        && lacks_attach(&ctx.app, guild_id, c)
    {
        lines.push(ATTACH_HINT.into());
    }
    reply(ctx, success_embed("Audit Logs Enabled", Some(&lines.join("\n")), None)).await
}

async fn sub_disable(ctx: &CommandCtx, guild_id: GuildId) -> BotResult {
    ctx.app.settings().update_settings(&guild_id.to_string(), SettingsPatch::new().set("loggingEnabled", false)).await?;
    reply(
        ctx,
        success_embed(
            "Audit Logs Disabled",
            Some("No audit logs will be posted. Your log channels, event toggles and ignored channels are kept. The moderation log (`/modconfig logchannel`) is not affected."),
            None,
        ),
    )
    .await
}

/// The `/logging status` fields: `(name, value)`.
pub fn status_fields(app: &App, guild_id: GuildId, settings: &bot_db::GuildSettings) -> (Vec<(String, String)>, String) {
    let mut channel_lines: Vec<String> = CONFIGURABLE_KEYS
        .iter()
        .map(|k| match k.channel_of(settings) {
            None => format!("**{}** → not set", k.label()),
            Some(id) => {
                let h = get_log_channel_health(app, guild_id, Some(id));
                if h.state == HealthState::Ok { format!("**{}** → <#{id}> ✅", k.label()) } else { format!("**{}** → <#{id}> ⚠️ {}", k.label(), h.text) }
            }
        })
        .collect();
    channel_lines.push(match settings.mod_log_channel_id.as_deref().filter(|c| !c.is_empty()) {
        Some(id) => format!("**Moderation** → <#{id}> (set with `/modconfig logchannel`)"),
        None => "**Moderation** → not set (`/modconfig logchannel`)".into(),
    });
    let keys = log_event_keys();
    let disabled: Vec<&str> = keys.iter().copied().filter(|k| !is_log_event_enabled(Some(settings), k)).collect();
    let ignored = &settings.log_ignored_channel_ids;
    let disabled_text = if disabled.is_empty() {
        "None — every event is logged.".to_string()
    } else {
        let list: Vec<String> = disabled.iter().filter_map(|k| log_event(k)).map(|e| format!("{} {}", e.emoji, e.label)).collect();
        bot_util::text::join_with_limit(&list, "\n", 1024, bot_util::text::default_more)
    };
    let ignored_text = if ignored.is_empty() {
        "None.".to_string()
    } else {
        let list: Vec<String> = ignored.iter().map(|id| format!("<#{id}>")).collect();
        bot_util::text::join_with_limit(&list, ", ", 1024, bot_util::text::default_more)
    };
    let fields = vec![
        ("Status".to_string(), if settings.logging_enabled { "✅ On".to_string() } else { "❌ Off — turn it on with `/logging enable`".to_string() }),
        ("Channels".to_string(), channel_lines.join("\n")),
        ("Disabled events".to_string(), disabled_text),
        ("Ignored channels".to_string(), ignored_text),
    ];
    let footer = format!("{}/{} events on · MEE6: Moderator › Audit logs", keys.len() - disabled.len(), keys.len());
    (fields, footer)
}

async fn sub_status(ctx: &CommandCtx, guild_id: GuildId) -> BotResult {
    let settings = ctx.app.settings().get_settings(&guild_id.to_string()).await?;
    let (fields, footer) = status_fields(&ctx.app, guild_id, &settings);
    let mut embed = guild_embed(Some(&settings), Some("📜 Audit Logs"), None, Some(&footer));
    for (n, v) in fields {
        embed = embed.field(n, v, false);
    }
    reply(ctx, embed).await
}

async fn sub_channel(ctx: &CommandCtx, guild_id: GuildId) -> BotResult {
    let g = guild_id.to_string();
    let kind = ctx.get_string("type").unwrap_or_default();
    let Some(keys) = type_value_to_keys(&kind) else {
        return reply(ctx, error_embed("Unknown Type", Some("Pick a log type from the list."), None)).await;
    };
    let labels = if kind == "all" { "All".to_string() } else { keys.iter().map(|k| k.label()).collect::<Vec<_>>().join(", ") };

    let Some(id) = ctx.get_channel_id("channel") else {
        ctx.app.settings().update_settings(&g, patch_of(&keys, Value::Null)).await?;
        return reply(ctx, success_embed("Log Channel Cleared", Some(&format!("**{labels}** logs will no longer be posted.")), None)).await;
    };
    let channel = resolve_channel(&ctx.app, guild_id, id).await;
    let problem = match &channel {
        Some(c) => describe_usable(&ctx.app, guild_id, c),
        None => Some("I cannot see that channel.".into()),
    };
    if let Some(p) = problem {
        return reply(ctx, error_embed("Missing Permissions", Some(&p), None)).await;
    }
    let Some(channel) = channel else { return Ok(()) };
    let settings = ctx.app.settings().update_settings(&g, patch_of(&keys, json!(channel.id.to_string()))).await?;
    forget_dead_channels(&ctx.app, &g, Some(&channel.id.to_string()));

    let mut lines = vec![format!("**{labels}** logs will be posted in <#{}>.", channel.id)];
    if let Some(r) = reuse_note(ctx) {
        lines.push(r);
    }
    if keys.contains(&LogType::Message) && lacks_attach(&ctx.app, guild_id, &channel) {
        lines.push(ATTACH_HINT.into());
    }
    if !settings.logging_enabled {
        lines.push(OFF_HINT.into());
    }
    reply(ctx, success_embed("Log Channel Set", Some(&lines.join("\n")), None)).await
}

async fn sub_event(ctx: &CommandCtx, guild_id: GuildId) -> BotResult {
    let name = bot_util::js::trim(&ctx.get_string("name").unwrap_or_default()).to_string();
    let state = ctx.get_string("state").unwrap_or_default();
    if name != "all" && !is_log_event_key(&name) {
        return reply(ctx, error_embed("Unknown Event", Some("Pick an event from the list. `/logging status` shows which events are off."), None)).await;
    }
    let on = state == "on";
    let already = Arc::new(Mutex::new(false));
    let (a, n) = (already.clone(), name.clone());
    let settings = ctx
        .app
        .settings()
        .update_settings_with(&guild_id.to_string(), move |cur| {
            let before = toggle_log_events(&cur.disabled_log_events, "", true); // the sanitised current list
            let after = toggle_log_events(&cur.disabled_log_events, &n, on);
            *a.lock().unwrap_or_else(|e| e.into_inner()) = before == after;
            Ok(SettingsPatch::new().set("disabledLogEvents", json!(after)))
        })
        .await?;
    let mut first = if name == "all" {
        format!("Every log event is now **{state}**.")
    } else {
        let e = log_event(&name).copied().unwrap_or(LOG_EVENTS[0]);
        format!("{} **{}** is now **{state}**.", e.emoji, e.label)
    };
    if *already.lock().unwrap_or_else(|e| e.into_inner()) {
        first = format!("It was already {state}. {first}");
    }
    let mut lines = vec![first];
    if !settings.logging_enabled {
        lines.push(OFF_HINT.into());
    }
    reply(ctx, success_embed("Log Event Updated", Some(&lines.join("\n")), None)).await
}

async fn sub_ignore_add(ctx: &CommandCtx, guild_id: GuildId) -> BotResult {
    let id = ctx.get_channel_id("channel").ok_or_else(|| anyhow::anyhow!("Required option \"channel\" not found."))?.to_string();
    let channel = ctx.get_channel("channel");
    let outcome = Arc::new(Mutex::new("added"));
    let (o, cid) = (outcome.clone(), id.clone());
    ctx.app
        .settings()
        .update_settings_with(&guild_id.to_string(), move |cur| {
            let list = cur.log_ignored_channel_ids.clone();
            let mut out = o.lock().unwrap_or_else(|e| e.into_inner());
            if list.contains(&cid) {
                *out = "exists";
                return Ok(SettingsPatch::new());
            }
            if list.len() >= MAX_IGNORED {
                *out = "limit";
                return Ok(SettingsPatch::new());
            }
            let mut next = list;
            next.push(cid);
            Ok(SettingsPatch::new().set("logIgnoredChannelIds", json!(next)))
        })
        .await?;
    let outcome = *outcome.lock().unwrap_or_else(|e| e.into_inner());
    if outcome == "exists" {
        return reply(ctx, warn_embed("Already Ignored", Some(&format!("Message events in <#{id}> are already ignored.")), None)).await;
    }
    if outcome == "limit" {
        let text = format!("You can ignore at most {MAX_IGNORED} channels. Remove one with `/logging ignore remove` first.");
        return reply(ctx, error_embed("Limit Reached", Some(&text), None)).await;
    }
    // The resolved option, else the cached channel.
    let cached = ChannelId::new(id.parse::<u64>().unwrap_or(1).max(1));
    let cached = bot_core::interaction_helpers::cached_channel(&ctx.app, guild_id, cached);
    let kind = channel.as_ref().map(|c| u8::from(c.kind)).or(cached.as_ref().map(|c| u8::from(c.kind)));
    let name = channel.as_ref().and_then(|c| c.name.clone()).or(cached.map(|c| c.name)).unwrap_or_default();
    let place = if kind == Some(channel_type::GUILD_CATEGORY) {
        format!("in every channel of **{name}**")
    } else {
        format!("in <#{id}>")
    };
    reply(ctx, success_embed("Channel Ignored", Some(&format!("Deleted, edited and bulk-deleted messages and posted invites {place} will no longer be logged.")), None))
        .await
}

async fn sub_ignore_remove(ctx: &CommandCtx, guild_id: GuildId) -> BotResult {
    let id = ctx.get_channel_id("channel").ok_or_else(|| anyhow::anyhow!("Required option \"channel\" not found."))?.to_string();
    let outcome = Arc::new(Mutex::new("removed"));
    let (o, cid) = (outcome.clone(), id.clone());
    ctx.app
        .settings()
        .update_settings_with(&guild_id.to_string(), move |cur| {
            if !cur.log_ignored_channel_ids.contains(&cid) {
                *o.lock().unwrap_or_else(|e| e.into_inner()) = "missing";
                return Ok(SettingsPatch::new());
            }
            let next: Vec<String> = cur.log_ignored_channel_ids.iter().filter(|x| **x != cid).cloned().collect();
            Ok(SettingsPatch::new().set("logIgnoredChannelIds", json!(next)))
        })
        .await?;
    if *outcome.lock().unwrap_or_else(|e| e.into_inner()) == "missing" {
        return reply(ctx, warn_embed("Not Ignored", Some(&format!("<#{id}> is not on the ignore list.")), None)).await;
    }
    reply(ctx, success_embed("Channel No Longer Ignored", Some(&format!("Message events in <#{id}> are logged again.")), None)).await
}

async fn sub_ignore_list(ctx: &CommandCtx, guild_id: GuildId) -> BotResult {
    let settings = ctx.app.settings().get_settings(&guild_id.to_string()).await?;
    let ids = &settings.log_ignored_channel_ids;
    let known = crate::adapters::known_channels(&ctx.app, guild_id).unwrap_or_default();
    let body = if ids.is_empty() {
        "No channels are ignored.".to_string()
    } else {
        let list: Vec<String> = ids.iter().map(|id| if known.contains(id) { format!("<#{id}>") } else { format!("#deleted-channel (`{id}`)") }).collect();
        bot_util::text::join_with_limit(&list, "\n", 4000, bot_util::text::default_more)
    };
    let footer = format!("{}/{MAX_IGNORED}", ids.len());
    reply(ctx, guild_embed(Some(&settings), Some("🙈 Ignored Channels"), Some(&body), Some(&footer))).await
}

// ── Entry points ──────────────────────────────────────────────────────────────

async fn execute(mut ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let group = ctx.subcommand_group();
    let sub = ctx.subcommand().unwrap_or_default();
    let label = match &group {
        Some(g) => format!("{g} {sub}"),
        None => sub.clone(),
    };
    let Some(guild_id) = ctx.guild_id() else { return Ok(()) };
    if !resolve_new_options(&mut ctx, NewOptions { required: &[], private: &["channel"] }).await {
        return Ok(());
    }
    let result = match (group.as_deref(), sub.as_str()) {
        (Some("ignore"), "add") => sub_ignore_add(&ctx, guild_id).await,
        (Some("ignore"), "remove") => sub_ignore_remove(&ctx, guild_id).await,
        (Some("ignore"), _) => sub_ignore_list(&ctx, guild_id).await,
        (_, "enable") => sub_enable(&ctx, guild_id).await,
        (_, "disable") => sub_disable(&ctx, guild_id).await,
        (_, "status") => sub_status(&ctx, guild_id).await,
        (_, "channel") => sub_channel(&ctx, guild_id).await,
        (_, "event") => sub_event(&ctx, guild_id).await,
        _ => Ok(()),
    };
    if let Err(e) = result {
        bot_util::log_error!(format!("/logging {label} failed"), bot_core::error::log_arg(&e));
        let _ = ctx
            .edit_reply(serenity::builder::EditInteractionResponse::new().embed(error_embed(
                "Could Not Save",
                Some("Something went wrong while saving. Please try again in a moment."),
                None,
            )))
            .await;
    }
    Ok(())
}

/// The autocomplete candidates (`{ name, value }`) for a typed text, given the settings (`None`: every event on).
pub fn autocomplete_choices(focused: &str, settings: Option<&bot_db::GuildSettings>) -> Vec<(String, String)> {
    let focused = bot_util::js::lower(bot_util::js::trim(focused));
    let mut candidates: Vec<(String, String, String)> = vec![("All events".into(), "all".into(), "all events".into())];
    for e in LOG_EVENTS {
        let on = is_log_event_enabled(settings, e.key);
        let label = e.log_type.label();
        candidates.push((
            format!("{} {} — {label}", if on { "✅" } else { "❌" }, e.label),
            e.key.to_string(),
            bot_util::js::lower(&format!("{} {} {label}", e.key, e.label)),
        ));
    }
    candidates.into_iter().filter(|(_, _, s)| focused.is_empty() || s.contains(&focused)).take(25).map(|(n, v, _)| (n, v)).collect()
}

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let focused = ctx.focused_value();
    // The answer is due in 3 s: the settings lookup is memo-first and bounded. A miss means every event shows as on.
    let mut settings = None;
    if let Some(g) = ctx.guild_id() {
        let g = g.to_string();
        settings = ctx.app.settings().peek_settings(&g);
        if settings.is_none() {
            let budget = bot_core::interaction::ack_budget_ms(ctx.interaction.id);
            let app = ctx.app.clone();
            settings = tokio::time::timeout(std::time::Duration::from_millis(budget), async move { app.settings().get_settings_safe(&g).await }).await.ok().flatten();
        }
    }
    let choices: Vec<Choice> = autocomplete_choices(&focused, settings.as_deref()).into_iter().map(|(n, v)| Choice::new(n, v)).collect();
    respond_autocomplete(&ctx.responder, &choices).await;
    Ok(())
}
