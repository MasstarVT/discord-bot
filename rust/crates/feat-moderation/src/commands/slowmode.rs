//! Port of `src/commands/moderation/slowmode.js`: `/slowmode` sets or removes a channel's or thread's slowmode, with
//! an autocomplete of presets. The reply follows `modVisibility`.

use bot_core::interaction_helpers::{Choice, Payload, reply_error, require_bot_permissions, respond_autocomplete, safe_defer, safe_reply};
use bot_core::prelude::*;
use bot_db::ModVisibility;
use bot_util::async_util::WithTimeoutError;
use bot_util::parse_duration::{format_duration_long, parse_duration};
use bot_util::text::clamp_text;
use serenity::all::{ChannelId, CreateAllowedMentions, CreateMessage, Permissions};
use serenity::builder::CreateEmbed;

use crate::guild_ops::{DiscordError, rest};

/// Discord's `rate_limit_per_user` limit: 6 hours.
pub const SLOWMODE_MAX_SECONDS: i64 = 21_600;
const OFF_WORDS: [&str; 8] = ["off", "none", "disable", "disabled", "false", "no", "0", "0s"];

/// `PRESETS`: (name, value).
pub const PRESETS: [(&str, &str); 14] = [
    ("Off", "off"),
    ("5 seconds", "5s"),
    ("10 seconds", "10s"),
    ("15 seconds", "15s"),
    ("30 seconds", "30s"),
    ("1 minute", "1m"),
    ("2 minutes", "2m"),
    ("5 minutes", "5m"),
    ("10 minutes", "10m"),
    ("15 minutes", "15m"),
    ("30 minutes", "30m"),
    ("1 hour", "1h"),
    ("2 hours", "2h"),
    ("6 hours", "6h"),
];

/// `SLOWMODE_TYPES`: text, voice, stage, forum, media and the three thread types.
pub fn slowmode_type(kind: u8) -> bool {
    matches!(kind, 0 | 2 | 13 | 15 | 16 | 11 | 12 | 10)
}

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("slowmode", "Set or remove a channel's slowmode (MEE6 /slowmode)")
        .default_member_permissions(Permissions::MODERATE_MEMBERS)
        .option(
            CommandOption::string("duration", "Interval such as 10s, 5m, 1h (max 6h), or off").required(true).max_length(16).autocomplete(true),
        )
        .option(CommandOption::channel("channel", "Channel or thread (default: this one)").channel_types(&[
            channel_type::GUILD_TEXT,
            channel_type::GUILD_VOICE,
            13,
            15,
            16,
            channel_type::PUBLIC_THREAD,
            channel_type::PRIVATE_THREAD,
            channel_type::ANNOUNCEMENT_THREAD,
        ]))
        .option(CommandOption::string("reason", "Reason (shown in the audit log)").max_length(512))
}

/// `/slowmode` (`module 'moderation'`, MODERATOR).
pub fn command() -> CommandDef {
    CommandDef::new("moderation", data(), Some(ModuleKey::Moderation), execute)
        .permission_level(PermissionLevel::Moderator)
        .autocomplete(|ctx| async move { autocomplete(ctx).await })
}

// ── Pure helpers ───────────────────────────────────────────────────────────────

/// Why a slowmode value was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlowmodeError {
    Invalid,
    TooShort,
    TooLong,
}

/// `parseSlowmode(input)`: a bare number means seconds; `off` (and friends) and 0 mean no slowmode.
pub fn parse_slowmode(input: &str) -> Result<i64, SlowmodeError> {
    let s = bot_util::js::trim(input).to_lowercase();
    if s.is_empty() || bot_util::js::utf16_len(&s) > 16 {
        return Err(SlowmodeError::Invalid);
    }
    if OFF_WORDS.contains(&s.as_str()) {
        return Ok(0);
    }
    let seconds = if s.bytes().all(|b| b.is_ascii_digit()) {
        let n = s.parse::<f64>().unwrap_or(f64::INFINITY);
        if n == 0.0 {
            return Ok(0);
        }
        if n > SLOWMODE_MAX_SECONDS as f64 {
            return Err(SlowmodeError::TooLong);
        }
        n as i64
    } else {
        let Some(ms) = parse_duration(&s) else { return Err(SlowmodeError::Invalid) };
        bot_util::text::js_round(ms as f64 / 1000.0) as i64
    };
    if seconds < 1 {
        return Err(SlowmodeError::TooShort);
    }
    if seconds > SLOWMODE_MAX_SECONDS {
        return Err(SlowmodeError::TooLong);
    }
    Ok(seconds)
}

/// `formatSlowmode(seconds)`: 'Off' for 0.
pub fn format_slowmode(seconds: i64) -> String {
    if seconds == 0 { "Off".into() } else { format_duration_long((seconds * 1000) as f64) }
}

/// `buildSlowmodeEmbed({ channelId, oldSeconds, newSeconds, reason, moderatorId, forum })`.
pub fn build_slowmode_embed(channel_id: &str, old: i64, new: i64, reason: Option<&str>, moderator_id: Option<&str>, forum: bool) -> CreateEmbed {
    let title = if new == 0 {
        "Slowmode Disabled"
    } else if old == 0 {
        "Slowmode Enabled"
    } else {
        "Slowmode Updated"
    };
    let mut lines =
        vec![format!("**Channel:** <#{channel_id}>"), format!("**Interval:** {}", format_slowmode(new)), format!("**Previously:** {}", format_slowmode(old))];
    if let Some(r) = reason {
        lines.push(format!("**Reason:** {}", clamp_text(r, 1000)));
    }
    if let Some(m) = moderator_id {
        lines.push(format!("**Moderator:** <@{m}>"));
    }
    if forum {
        lines.push("*Applies to creating new posts in this forum.*".into());
    }
    success_embed(title, Some(&lines.join("\n")), None)
}

/// `editError(err, target, manageLabel)`: `None` error means the 15 s timeout.
pub fn edit_error(err: Option<&DiscordError>, target: ChannelId, manage_label: &str) -> CreateEmbed {
    let Some(e) = err else {
        return error_embed(
            "Still Waiting",
            Some("Discord is rate-limiting changes to this channel. The change may still apply in a moment; check the channel settings before trying again."),
            None,
        );
    };
    match e.code {
        Some(50001) | Some(50013) => {
            error_embed("Missing Permissions", Some(&format!("I couldn't change <#{target}>. Check that I have **{manage_label}** there.")), None)
        }
        Some(10003) => error_embed("Channel Not Found", Some("That channel no longer exists."), None),
        Some(50083) => error_embed("Thread Archived", Some("That thread is archived. Unarchive it, then try again."), None),
        Some(50024) | Some(50035) => error_embed("Unsupported Channel", Some("Discord rejected slowmode for this channel."), None),
        code => {
            bot_util::log_error!("slowmode edit failed".to_string(), bot_util::logger::LogArg::error(e));
            let c = code.map(|c| format!(" (code {c})")).unwrap_or_default();
            error_embed("Slowmode Failed", Some(&format!("Discord returned an error{c}. Please try again.")), None)
        }
    }
}

/// The autocomplete choices for what was typed.
pub fn slowmode_choices(typed: &str) -> Vec<Choice> {
    let v = bot_util::js::trim(typed).to_string();
    let q = v.to_lowercase();
    let mut choices = Vec::new();
    if !v.is_empty() {
        match parse_slowmode(&v) {
            Ok(s) if !PRESETS.iter().any(|(_, value)| *value == q) => {
                let cut = String::from_utf16_lossy(&v.encode_utf16().take(16).collect::<Vec<_>>());
                choices.push(Choice::new(format!("{} (custom)", format_slowmode(s)), cut));
            }
            Err(SlowmodeError::TooLong) => choices.push(Choice::new("Too long: the maximum is 6 hours", "6h")),
            _ => {}
        }
    }
    for (name, value) in PRESETS {
        if q.is_empty() || name.to_lowercase().contains(&q) || value.contains(q.as_str()) {
            choices.push(Choice::new(name, value));
        }
    }
    choices.truncate(25);
    choices
}

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let typed = ctx.focused_value();
    respond_autocomplete(&ctx.responder, &slowmode_choices(&typed)).await;
    Ok(())
}

// ── Command ────────────────────────────────────────────────────────────────────

async fn execute(ctx: CommandCtx) -> BotResult {
    let r = &ctx.responder;
    let Some(gid) = ctx.guild_id() else {
        reply_error(r, "Server Only", Some("Use this command in a server."), true).await;
        return Ok(());
    };

    // 1. Duration.
    let raw = ctx.get_string("duration").unwrap_or_default();
    let seconds = match parse_slowmode(&raw) {
        Ok(s) => s,
        Err(SlowmodeError::TooLong) => {
            reply_error(r, "Duration Too Long", Some("Slowmode can be at most **6 hours** (Discord limit)."), true).await;
            return Ok(());
        }
        Err(SlowmodeError::TooShort) => {
            reply_error(r, "Duration Too Short", Some("Slowmode must be at least 1 second, or `off`."), true).await;
            return Ok(());
        }
        Err(SlowmodeError::Invalid) => {
            let clean = clamp_text(&raw.replace('`', ""), 16);
            let text = format!("Couldn't read `{clean}`. Use a duration such as `10s`, `5m` or `1h` (max 6 hours), or `off`.");
            reply_error(r, "Invalid Duration", Some(&text), true).await;
            return Ok(());
        }
    };

    // 2. Target channel or thread (the lookup before the acknowledgement is bounded, rule 21).
    let picked = ctx.get_channel_id("channel");
    let target_id = picked.unwrap_or_else(|| ctx.channel_id());
    let mut target = bot_core::interaction_helpers::cached_channel(&ctx.app, gid, target_id);
    if target.is_none()
        && let Some(p) = picked
    {
        let (app, budget) = (ctx.app.clone(), ctx.ack_budget_ms());
        let fetch = async move { bot_core::interaction_helpers::fetch_channel(&app, gid, p).await };
        target = bot_util::async_util::with_timeout(fetch, budget, "slowmode channel fetch").await.ok();
    }
    let Some(target) = target else {
        reply_error(r, "Channel Not Found", Some("I can't see that channel. Pick one with the `channel` option."), true).await;
        return Ok(());
    };

    // 3. Type.
    let kind = u8::from(target.kind);
    if kind == channel_type::GUILD_ANNOUNCEMENT {
        reply_error(r, "Unsupported Channel", Some("Announcement channels do not support slowmode."), true).await;
        return Ok(());
    }
    if !slowmode_type(kind) {
        reply_error(r, "Unsupported Channel", Some("Slowmode can only be set on text, voice, stage, forum and media channels and on threads."), true).await;
        return Ok(());
    }

    // 4. Archived thread.
    let is_thread = bot_core::permissions::is_thread(&target);
    if is_thread && target.thread_metadata.is_some_and(|t| t.archived) {
        reply_error(r, "Thread Archived", Some("That thread is archived. Unarchive it, then try again."), true).await;
        return Ok(());
    }

    // 5. Which permission a change needs (threads need Manage Threads).
    let (manage_perm, manage_label, manage_flag) =
        if is_thread { ("ManageThreads", "Manage Threads", Permissions::MANAGE_THREADS) } else { ("ManageChannels", "Manage Channels", Permissions::MANAGE_CHANNELS) };

    // 6. The executor (another channel only). The router has just loaded the settings: normally a memo hit.
    let g = gid.to_string();
    let settings = match ctx.app.settings().peek_settings(&g) {
        Some(s) => Some(s),
        None => {
            let (a, gg) = (ctx.app.clone(), g.clone());
            let load = async move { Ok::<_, std::convert::Infallible>(a.settings().get_settings_safe(&gg).await) };
            bot_util::async_util::with_timeout(load, ctx.ack_budget_ms(), "slowmode settings").await.ok().flatten()
        }
    };
    if target.id != ctx.channel_id() {
        let level = resolve_permission_level(ctx.member_access().as_ref(), settings.as_deref(), ctx.app.config().bot_owner_id.as_deref());
        if level < PermissionLevel::Admin {
            let ok = match (bot_core::interaction_helpers::cached_guild(&ctx.app, gid), ctx.member()) {
                (Some(guild), Some(m)) => bot_core::permissions::member_permissions_in(&guild, &target, m.user.id, &m.roles, true)
                    .is_some_and(|p| bot_core::permissions::js_has(p, Permissions::VIEW_CHANNEL) && bot_core::permissions::js_has(p, manage_flag)),
                _ => false,
            };
            if !ok {
                let text = format!("You need **{manage_label}** in <#{}> to change its slowmode.", target.id);
                reply_error(r, "Missing Permissions", Some(&text), true).await;
                return Ok(());
            }
        }
    }

    // 7. The bot.
    if !require_bot_permissions(&ctx, &["ViewChannel", manage_perm], Some(target.id), true).await {
        return Ok(());
    }

    // 8. No change.
    let old = i64::from(target.rate_limit_per_user.unwrap_or(0));
    if old == seconds {
        let text = if seconds == 0 {
            format!("Slowmode in <#{}> is already off.", target.id)
        } else {
            format!("Slowmode in <#{}> is already set to {}.", target.id, format_slowmode(seconds))
        };
        safe_reply(r, Payload::embed(info_embed("No Change", Some(&text), None)).ephemeral(true)).await;
        return Ok(());
    }

    // 9. Defer.
    if !safe_defer(r, true).await {
        return Ok(());
    }

    // 10. Apply.
    let reason = ctx.get_string("reason").map(|s| bot_util::js::trim(&s).to_string()).filter(|s| !s.is_empty());
    let audit = clamp_text(
        &format!("{} | by {} via /slowmode", reason.as_deref().unwrap_or("No reason provided"), bot_core::adapters::user_tag(ctx.user())),
        512,
    );
    let edit = rest(&ctx.app).edit_slowmode(target.id, seconds as u16, audit);
    match bot_util::async_util::with_timeout(edit, 15_000, "slowmode edit").await {
        Ok(()) => {}
        Err(WithTimeoutError::Timeout(_)) => {
            safe_reply(r, Payload::embed(edit_error(None, target.id, manage_label))).await;
            return Ok(());
        }
        Err(WithTimeoutError::Inner(e)) => {
            safe_reply(r, Payload::embed(edit_error(Some(&e), target.id, manage_label))).await;
            return Ok(());
        }
    }

    // 11. Reply per modVisibility.
    let forum = kind == 15 || kind == 16;
    let visibility = settings.as_ref().map(|s| s.mod_visibility).unwrap_or(ModVisibility::Public);
    let channel = target.id.to_string();
    let moderator = ctx.user().id.to_string();
    let with_mod = build_slowmode_embed(&channel, old, seconds, reason.as_deref(), Some(&moderator), forum);
    let attempt = |label: &'static str, res: Result<(), bot_core::interaction::InteractionError>| {
        if let Err(e) = res
            && !bot_core::interaction_helpers::is_interaction_gone(&e)
        {
            bot_util::log_warn!(format!("slowmode: {label} failed"), bot_util::logger::LogArg::error(&e));
        }
    };
    match visibility {
        ModVisibility::Private => attempt("private reply", r.edit_reply(Payload::embed(with_mod).to_edit()).await.map(|_| ())),
        ModVisibility::Anonymous => {
            attempt("private confirmation", r.edit_reply(Payload::embed(with_mod).to_edit()).await.map(|_| ()));
            // An interaction follow-up shows who ran the command, so the anonymous notice is a plain channel message.
            let anonymous = build_slowmode_embed(&channel, old, seconds, reason.as_deref(), None, forum);
            let msg = CreateMessage::new().embed(anonymous).allowed_mentions(CreateAllowedMentions::new());
            if let Err(e) = rest(&ctx.app).send_message(ctx.channel_id(), msg).await {
                bot_util::log_warn!("slowmode: public notice failed".to_string(), bot_util::logger::LogArg::error(&e));
            }
        }
        ModVisibility::Public => {
            attempt("delete reply", r.delete_reply().await);
            attempt("public follow-up", r.follow_up(Payload::embed(with_mod).no_mentions().to_follow_up()).await.map(|_| ()));
        }
    }
    Ok(())
}
