//! Port of `src/commands/utility/timestamp.js`: `/timestamp` turns a typed time ("fri 7pm", "tomorrow 18:30",
//! "7pm CST") into Discord timestamps, which every reader sees in their own timezone. The text is read in the
//! caller's zone: their own (`/timezone set`), else their timezone role, else the server timezone.

use std::time::Duration;

use bot_core::command::CommandOption as Opt;
use bot_core::interaction_helpers::{Choice, Payload, cached_guild, reply_error, respond_autocomplete, safe_reply};
use bot_core::prelude::*;
use bot_db::GuildSettings;
use bot_util::js;
use bot_util::time::{ParsedWhen, discord_timestamp_ms, parse_when_ms, zoned_parts_ms};
use bot_util::timezones::{ResolvedTimezone, ZoneSource, format_in_zone_ms, zone_label_ms, zone_source_text};
use serenity::all::{GuildId, Member};
use serenity::builder::CreateEmbed;

use crate::user_timezone_service::{get_user_timezone, member_role_ids, resolve_member_timezone};

/// Discord's timestamp styles, in the order the reply lists them: `(value, name)`.
pub const STYLES: [(&str, &str); 7] = [
    ("F", "Long date and time (Friday, October 16, 2026 7:00 PM)"),
    ("f", "Date and time (October 16, 2026 7:00 PM)"),
    ("D", "Long date (October 16, 2026)"),
    ("d", "Short date (10/16/2026)"),
    ("t", "Time (7:00 PM)"),
    ("T", "Time with seconds (7:00:00 PM)"),
    ("R", "Relative (in 2 hours)"),
];

/// The examples autocomplete shows for an empty `when`.
pub const EXAMPLES: [&str; 5] = ["now", "in 2h", "tomorrow 12pm", "fri 7pm", "next mon 9am"];

const MONTH_NAMES: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

/// `futureDayExample(zone, now)`: a calendar day a week from now in `zone`, as a hint would type it ("oct 29") and
/// as an ISO date. Always ahead, so a hint never shows a day that has already passed.
pub fn future_day_example(zone: &str, now_ms: f64) -> (String, String) {
    let p = zoned_parts_ms(now_ms, zone);
    let d = js::utc_fields(js::date_utc(p.year as f64, f64::from(p.month) - 1.0, f64::from(p.day) + 7.0, 0.0, 0.0, 0.0, 0.0));
    let month = d.month0 as usize;
    (format!("{} {}", MONTH_NAMES[month % 12], d.day), format!("{}-{:02}-{:02}", d.year, month + 1, d.day))
}

/// The command's data.
pub fn data() -> SlashCommand {
    let mut style = Opt::string("style", "How the posted timestamp looks (with public: True). Default: long date and time.");
    for (value, name) in STYLES {
        style = style.choice(name, value);
    }
    SlashCommand::new("timestamp", "Turn a time into a timestamp that everyone sees in their own timezone.")
        .option(Opt::string("when", "e.g. fri 7pm, tomorrow 18:30, oct 20 9am, in 2h, 7pm CST").required(true).max_length(100).autocomplete(true))
        .option(style)
        .option(Opt::boolean("public", "True: post the timestamp in this channel. False (default): only you see the codes."))
}

/// The command (core, MEMBER).
pub fn command() -> CommandDef {
    CommandDef::new("utility", data(), None, execute).permission_level(PermissionLevel::Member).autocomplete(|ctx| async move { autocomplete(ctx).await })
}

/// Echoes user input inside a code span: no span breakers, bounded.
fn shown(raw: &str) -> String {
    let cleaned: String = raw.chars().filter(|c| *c != '`' && *c != '\\').collect();
    let s = bot_util::text::clamp_text(js::trim(&cleaned), 64);
    if s.is_empty() { "(empty)".to_string() } else { s }
}

/// `roughRelative(ms)`: rough distance from now for autocomplete names: 'in 3 hours', '2 days ago', 'now'.
pub fn rough_relative(ms: f64) -> String {
    let abs = ms.abs();
    let units: [(f64, &str); 5] = [(31_536_000_000.0, "year"), (2_592_000_000.0, "month"), (86_400_000.0, "day"), (3_600_000.0, "hour"), (60_000.0, "minute")];
    for (size, unit) in units {
        if abs >= size {
            let n = bot_util::text::js_round(abs / size);
            let text = format!("{} {unit}{}", js::number_to_string(n), if n == 1.0 { "" } else { "s" });
            return if ms > 0.0 { format!("in {text}") } else { format!("{text} ago") };
        }
    }
    "now".to_string()
}

/// `suggestWhen(typed, zone, now)`: autocomplete choices for `when`: a preview of how the typed text is read, or
/// examples when empty. The value is always the text itself, so execute reads it again. `(name, value)` pairs.
pub fn suggest_when(typed: &str, zone: &str, now_ms: f64) -> Vec<(String, String)> {
    let q = js::utf16_prefix(js::trim(typed), 100).to_string();
    let mut out = Vec::new();
    let inputs: Vec<String> = if q.is_empty() { EXAMPLES.iter().map(|s| s.to_string()).collect() } else { vec![q.clone()] };
    for input in inputs {
        let Some(r) = parse_when_ms(&input, zone, now_ms) else { continue };
        let preview = format!(
            "{} · {} · {}",
            format_in_zone_ms(r.date_ms, &r.zone),
            zone_label_ms(&r.zone, r.date_ms),
            rough_relative(r.date_ms - now_ms)
        );
        out.push((if q.is_empty() { format!("{input} → {preview}") } else { preview }, input));
    }
    if !q.is_empty() && out.is_empty() {
        out.push((
            format!(
                "Can't read \"{}\" yet. Try: fri 7pm · tomorrow 18:30 · {} 9am · in 2h",
                bot_util::text::clamp_text(&q, 30),
                future_day_example(zone, now_ms).0
            ),
            q,
        ));
    }
    out
}

/// `buildTimestampEmbed(parsed, resolved, { style, personalUnknown, settings })`: the private reply: how the text
/// was read, then every style as a copyable code and its preview.
pub fn build_timestamp_embed(parsed: &ParsedWhen, resolved: &ResolvedTimezone, style: &str, personal_unknown: bool, settings: Option<&GuildSettings>) -> CreateEmbed {
    let mut order = vec![style];
    order.extend(STYLES.iter().map(|(v, _)| *v).filter(|v| *v != style));
    let mut lines = vec![
        format!("**{}** · {}", format_in_zone_ms(parsed.date_ms, &parsed.zone), zone_label_ms(&parsed.zone, parsed.date_ms)),
        format!(
            "Read in {}. Everyone sees these in their own timezone.",
            zone_source_text(resolved.source.as_str(), resolved.role_id.as_deref(), parsed.zone_from_input)
        ),
    ];
    if !parsed.zone_from_input && resolved.source == ZoneSource::Role && resolved.role_count > 1 {
        lines.push("You have several timezone roles, so the highest one was used.".into());
    }
    if !parsed.zone_from_input && personal_unknown {
        lines.push("⚠️ Your saved timezone could not be loaded just now, so this one was used.".into());
    }
    lines.push(String::new());
    lines.push("Copy a code into any message:".into());
    for st in order {
        let code = discord_timestamp_ms(parsed.date_ms, st);
        lines.push(format!("`{code}` {code}"));
    }
    if !parsed.zone_from_input && matches!(resolved.source, ZoneSource::Server | ZoneSource::Default) {
        lines.push(String::new());
        lines.push("Not your timezone? Save yours with `/timezone set`, or add one to the time, e.g. `7pm CST`.".into());
    }
    guild_embed(settings, Some("🕒 Timestamp"), Some(&lines.join("\n")), None)
}

/// What [`load_caller_zone`] found.
pub struct CallerZone {
    pub resolved: ResolvedTimezone,
    pub settings: Option<Settings>,
    pub personal_unknown: bool,
}

/// `loadCallerZone(interaction, { cacheOnly, budgetMs })`: the caller's zone. Lookups are bounded by the ack budget;
/// a slow or failed one falls back to the next source (`personal_unknown` says so).
pub async fn load_caller_zone(app: &App, guild_id: Option<GuildId>, user_id: &str, member: Option<&Member>, cache_only: bool, budget_ms: u64) -> CallerZone {
    let budget = Duration::from_millis(budget_ms);
    let settings_fut = async {
        let g = guild_id?.to_string();
        if let Some(s) = app.settings().peek_settings(&g) {
            return Some(s);
        }
        tokio::time::timeout(budget, app.settings().get_settings_safe(&g)).await.ok().flatten()
    };
    let personal_fut = async { tokio::time::timeout(budget, get_user_timezone(app, user_id, cache_only)).await.ok().flatten() };
    let (settings, personal) = tokio::join!(settings_fut, personal_fut);
    let guild = guild_id.and_then(|g| cached_guild(app, g));
    let roles = member_role_ids(member);
    let resolved = resolve_member_timezone(&roles, guild.as_ref(), settings.as_deref(), personal.clone().flatten().as_deref());
    CallerZone { resolved, settings, personal_unknown: personal.is_none() && !cache_only }
}

async fn execute(ctx: CommandCtx) -> BotResult {
    let raw = ctx.get_string("when").ok_or_else(|| anyhow::anyhow!("Required option \"when\" not found."))?;
    let style = ctx.get_string("style").unwrap_or_else(|| "F".into());
    let is_public = ctx.get_boolean("public").unwrap_or(false);

    let caller = load_caller_zone(&ctx.app, ctx.guild_id(), &ctx.user().id.to_string(), ctx.member(), false, ctx.ack_budget_ms()).await;
    let now_ms = ctx.app.clock().now_ms() as f64;
    let Some(parsed) = parse_when_ms(&raw, &caller.resolved.zone, now_ms) else {
        let (words, iso) = future_day_example(&caller.resolved.zone, now_ms);
        let desc = [
            format!("I couldn't read `{}` as a time.", shown(&raw)),
            format!("Try `fri 7pm`, `tomorrow 18:30`, `next mon 9am`, `{words} 7:30pm`, `{iso} 19:00`, `in 2h` or `7pm CST`."),
            format!(
                "Times are read in **{}**, {}.",
                zone_label_ms(&caller.resolved.zone, now_ms),
                zone_source_text(caller.resolved.source.as_str(), caller.resolved.role_id.as_deref(), false)
            ),
        ]
        .join("\n");
        reply_error(&ctx.responder, "Could Not Read That Time", Some(&desc), true).await;
        return Ok(());
    };

    if is_public {
        let main = discord_timestamp_ms(parsed.date_ms, &style);
        let content = if style == "R" { format!("🕒 {main}") } else { format!("🕒 {main} ({})", discord_timestamp_ms(parsed.date_ms, "R")) };
        safe_reply(&ctx.responder, Payload::text(content).no_mentions()).await;
        return Ok(());
    }
    let embed = build_timestamp_embed(&parsed, &caller.resolved, &style, caller.personal_unknown, caller.settings.as_deref());
    safe_reply(&ctx.responder, Payload::embed(embed).ephemeral(true).no_mentions()).await;
    Ok(())
}

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let mut choices: Vec<Choice> = Vec::new();
    if let Some((name, value)) = ctx.focused()
        && name == "when"
    {
        let budget = 1000.min(bot_core::interaction::ack_budget_ms(ctx.interaction.id));
        let caller = load_caller_zone(&ctx.app, ctx.guild_id(), &ctx.interaction.user.id.to_string(), ctx.interaction.member.as_deref(), true, budget).await;
        choices = suggest_when(&value, &caller.resolved.zone, ctx.app.clock().now_ms() as f64).into_iter().map(|(n, v)| Choice::new(n, v)).collect();
    }
    respond_autocomplete(&ctx.responder, &choices).await;
    Ok(())
}
