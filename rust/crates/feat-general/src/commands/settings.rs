//! Port of `src/commands/admin/settings.js`: general server settings (MEE6 Settings): timezone, timezone roles,
//! default embed colour, custom-command prefix, the `/search urban` age-restriction switch and a read-only overview.
//! Every read and write goes through the settings service; nothing here touches the tables or the cache directly.

use std::sync::{Arc, Mutex};

use bot_config::colors;
use bot_config::limits::TIMEZONE_ROLES;
use bot_config::{MODULE_NAMES, get_module};
use bot_core::command::CommandOption as Opt;
use bot_core::custom_embed::{format_color, parse_color_str};
use bot_core::embed_builder::apply_guild_color;
use bot_core::interaction_helpers::{Choice, Payload, reply_error, respond_autocomplete, safe_defer, safe_reply, with_cached_guild};
use bot_core::markdown::escape_markdown;
use bot_core::prelude::*;
use bot_db::GuildSettings;
use bot_util::jsre::LazyRe;
use bot_util::js;
use bot_util::text::{clamp_text, default_more, join_with_limit};
use bot_util::timezones::{
    SearchOptions, TimezoneRole, ZoneErrorCode, describe_local_time_ms, normalize_timezone_roles, parse_zone_input, utc_offset_label_ms,
    zone_from_role_name, zone_input_error, zone_label_ms,
};
use serde_json::{Value, json};
use serenity::all::Permissions;
use serenity::builder::CreateEmbed;

// The timezone helpers live in bot-util (shared with /timezone and /timestamp); re-exported for the dashboard and the
// tests, as the JS re-exports them.
pub use bot_util::timezones::{LEGACY_TZ_NAMES, POPULAR_TIMEZONES, describe_local_time_ms as describe_local_time, list_time_zones, normalize_time_zone, search_time_zones, utc_offset_label};

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("settings", "Server settings: timezone, timezone roles, embed colour, custom-command prefix, /search urban.")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(Opt::sub_command("show", "Show the general settings, enabled modules and moderator roles."))
        .option(
            Opt::sub_command("timezone", "Set the server timezone used by reminders, birthdays and other schedules.").option(
                Opt::string("zone", "Region/City, e.g. Europe/London or America/New_York. Type an offset like +5:30 to search.")
                    .required(true)
                    .max_length(64)
                    .autocomplete(true),
            ),
        )
        .option(
            Opt::sub_command("color", "Set the colour of info embeds, or reset it to the default.").option(
                Opt::string("value", "Hex (#5865F2 or #fff), a colour name (red, blurple…) or reset.").required(true).max_length(16).autocomplete(true),
            ),
        )
        .option(
            Opt::sub_command("prefix", "Set the prefix for custom commands (MEE6 !command). Default: !").option(
                Opt::string("prefix", "1–5 characters, no spaces, e.g. ! ? . $ or +. Type off to disable.").required(true).min_length(1).max_length(5),
            ),
        )
        .option(
            Opt::sub_command("urban", "Choose where /search urban (Urban Dictionary) may be used.")
                .option(Opt::boolean("nsfw-only", "True: age-restricted channels only (default). False: every channel.").required(true)),
        )
        .option(
            Opt::sub_command_group("timezone-role", "Roles such as @CST that give members a timezone in /timestamp.")
                .option(
                    Opt::sub_command("add", "Link a role to a timezone. Members with the role get it in /timestamp.")
                        .option(Opt::role("role", "The timezone role, e.g. @CST.").required(true))
                        .option(
                            Opt::string("zone", "Region/City or an abbreviation like CST. Empty: read it from the role name.").max_length(64).autocomplete(true),
                        ),
                )
                .option(
                    Opt::sub_command("remove", "Unlink a timezone role. The role itself is kept.")
                        .option(Opt::role("role", "The timezone role.").required(true)),
                )
                .option(Opt::sub_command("list", "Show the timezone roles and the time in each.")),
        )
}

/// The command (core, ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), None, execute)
        .permission_level(PermissionLevel::Admin)
        .autocomplete(|ctx| async move { autocomplete(ctx).await })
}

// ── Colour ───────────────────────────────────────────────────────────────────

/// `COLOR_NAMES`: the names the colour option suggests.
pub const COLOR_NAMES: [&str; 11] = ["blurple", "red", "green", "blue", "yellow", "orange", "purple", "pink", "white", "black", "grey"];
/// `COLOR_RESET_WORDS`.
pub const COLOR_RESET_WORDS: [&str; 4] = ["reset", "default", "none", "clear"];

/// A parsed colour option: `color: None` means "reset to the default".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorInput {
    pub color: Option<u32>,
    /// Pure black was saved as `#010101` (Discord renders 0 as "no colour").
    pub adjusted: bool,
}

/// `parseColorInput(input)`: `None` when it is not a colour.
pub fn parse_color_input(input: &str) -> Option<ColorInput> {
    let s = js::lower(js::trim(input));
    if s.is_empty() {
        return None;
    }
    if COLOR_RESET_WORDS.contains(&s.as_str()) {
        return Some(ColorInput { color: None, adjusted: false });
    }
    let c = parse_color_str(&s)?;
    if c > 0xFF_FFFF {
        return None;
    }
    if c == 0 {
        return Some(ColorInput { color: Some(0x010101), adjusted: true });
    }
    Some(ColorInput { color: Some(c), adjusted: false })
}

fn fmt_color(c: i64) -> String {
    format_color(&json!(c))
}

/// `suggestColors(query, { current })`: autocomplete choices for the colour option (at most 25).
pub fn suggest_colors(query: &str, current: Option<i64>) -> Vec<Choice> {
    let q = js::lower(js::trim(query));
    let mut out = Vec::new();
    if q.is_empty()
        && let Some(c) = current
    {
        out.push(Choice::new(format!("Current: {}", fmt_color(c)), fmt_color(c)));
    }
    if !q.is_empty()
        && let Some(ColorInput { color: Some(c), .. }) = parse_color_input(&q)
        && !COLOR_NAMES.contains(&q.as_str())
    {
        out.push(Choice::new(format!("{} (custom)", fmt_color(i64::from(c))), js::utf16_prefix(js::trim(query), 16).to_string()));
    }
    if q.is_empty() || COLOR_RESET_WORDS.iter().any(|w| w.starts_with(q.as_str())) {
        out.push(Choice::new(format!("reset (default colour {})", fmt_color(i64::from(colors::INFO))), "reset"));
    }
    for n in COLOR_NAMES {
        if !q.is_empty() && !n.contains(q.as_str()) {
            continue;
        }
        if let Some(c) = parse_color_str(n) {
            out.push(Choice::new(format!("{n} ({})", fmt_color(i64::from(c))), n));
        }
    }
    out.truncate(25);
    out
}

// ── Prefix ───────────────────────────────────────────────────────────────────

/// `PREFIX_MAX`.
pub const PREFIX_MAX: usize = 5;
static PREFIX_FORBIDDEN: LazyRe = bot_util::lazy_re!(r"[`<>@#:\\/]", "u");
static WHITESPACE: LazyRe = bot_util::lazy_re!(r"\s", "u");
static CONTROL: LazyRe = bot_util::lazy_re!(r"\p{C}", "u");

/// Why a prefix was refused (`code`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixError {
    Empty,
    Spaces,
    TooLong,
    InvalidChars,
}

impl PrefixError {
    /// `'EMPTY'`, `'SPACES'`, `'TOO_LONG'` or `'INVALID_CHARS'`.
    pub fn code(self) -> &'static str {
        match self {
            Self::Empty => "EMPTY",
            Self::Spaces => "SPACES",
            Self::TooLong => "TOO_LONG",
            Self::InvalidChars => "INVALID_CHARS",
        }
    }

    /// `PREFIX_ERRORS[code]`.
    pub fn message(self) -> &'static str {
        match self {
            Self::Empty => "The prefix can't be empty.",
            Self::Spaces => "The prefix can't contain spaces.",
            Self::TooLong => "The prefix can be at most 5 characters.",
            Self::InvalidChars => {
                "The prefix can't contain `/`, `@`, `#`, `:`, `<`, `>`, backslashes, backticks or invisible characters, because Discord uses them for commands, mentions and emoji."
            }
        }
    }
}

/// `validatePrefix(input)`: the prefix to store. `off` (any case) is accepted and stored as the empty string, which
/// disables prefix invocation of custom commands (`/tag use` keeps working).
pub fn validate_prefix(input: &str) -> Result<String, PrefixError> {
    let s = js::trim(input);
    if s.is_empty() {
        return Err(PrefixError::Empty);
    }
    if js::lower(s) == "off" {
        return Ok(String::new());
    }
    if WHITESPACE.test(s) {
        return Err(PrefixError::Spaces);
    }
    if s.chars().count() > PREFIX_MAX {
        return Err(PrefixError::TooLong);
    }
    if CONTROL.test(s) || PREFIX_FORBIDDEN.test(s) {
        return Err(PrefixError::InvalidChars);
    }
    Ok(s.to_string())
}

// ── Overview ─────────────────────────────────────────────────────────────────

fn module_label(key: &str) -> String {
    get_module(key).map(|m| format!("{} {}", m.emoji, m.label)).unwrap_or_default()
}

/// `buildOverviewEmbed(settings, guild, { now })`: the `/settings show` embed. `role_exists` is
/// `guild.roles.cache.has(id)`.
pub fn build_overview_embed(settings: &GuildSettings, guild_name: Option<&str>, role_exists: &dyn Fn(&str) -> bool, now_ms: i64) -> CreateEmbed {
    // Timezone
    let lt = describe_local_time_ms(&settings.timezone, now_ms as f64);
    let tz_value = if lt.valid {
        format!("`{}`\n{} · local time **{}** ({})", settings.timezone, lt.offset, lt.time, lt.weekday)
    } else {
        format!(
            "`{}` ⚠️ not a valid timezone, so UTC is used. Fix it with `/settings timezone`.",
            clamp_text(&settings.timezone.replace('`', ""), 64)
        )
    };

    // Colour
    let color_value = match settings.embed_color {
        Some(c) => format!("`{}`", fmt_color(i64::from(c))),
        None => format!("Default (`{}`)", fmt_color(i64::from(colors::INFO))),
    };

    // Prefix. '' means prefix invocation is off (/settings prefix off).
    let custom_commands_line = if is_module_enabled(Some(settings), Some(ModuleKey::Customcommands)) {
        "Custom commands: on"
    } else {
        "Custom commands are off · `/module enable customcommands`"
    };
    let prefix_value = if settings.custom_prefix.is_empty() {
        format!("Prefix: off · `/tag use` only\n{custom_commands_line}")
    } else {
        format!("`{}`\n{custom_commands_line}", settings.custom_prefix)
    };

    // Modules
    let enabled: Vec<&str> = MODULE_NAMES.iter().copied().filter(|k| bot_config::is_module_enabled_in(Some(settings), Some(k))).collect();
    let head = format!("**{}** of **{}** enabled · `/module list`", enabled.len(), MODULE_NAMES.len());
    let modules_value = if enabled.is_empty() {
        head.clone()
    } else {
        let labels: Vec<String> = enabled.iter().map(|k| module_label(k)).collect();
        format!("{head}\n{}", join_with_limit(&labels, ", ", 1024 - js::utf16_len(&head) - 1, default_more))
    };

    // Urban Dictionary gate. Fail closed, like /search urban itself: only an explicit false opens it up.
    let urban_value = if !settings.urban_nsfw_only {
        "Urban Dictionary: everywhere · `/settings urban`"
    } else {
        "Urban Dictionary: age-restricted channels only · `/settings urban`"
    };

    // Moderator roles (read-only; edited in /modconfig)
    let mut ids: Vec<&str> = Vec::new();
    for id in &settings.mod_role_ids {
        if !ids.contains(&id.as_str()) {
            ids.push(id);
        }
    }
    let live: Vec<&str> = ids.iter().copied().filter(|id| role_exists(id)).collect();
    let missing = ids.len() - live.len();
    let tail = "Edit with `/modconfig`. Admins: Administrator or Manage Server. Moderators: these roles, or Kick/Ban/Timeout Members.";
    let mut roles = if live.is_empty() {
        "None set".to_string()
    } else {
        let mentions: Vec<String> = live.iter().map(|id| format!("<@&{id}>")).collect();
        join_with_limit(&mentions, ", ", 1024 - js::utf16_len(tail) - 40, default_more)
    };
    if missing > 0 {
        roles.push_str(&format!(" (+{missing} deleted role{})", if missing == 1 { "" } else { "s" }));
    }

    let description = format!("General settings for **{}**.", escape_markdown(&clamp_text(guild_name.unwrap_or("this server"), 100)));
    guild_embed(Some(settings), Some("⚙️ Server Settings"), Some(&description), Some("Change with /settings timezone · color · prefix · urban"))
        .field("🌍 Timezone", clamp_text(&tz_value, 1024), true)
        .field("🎨 Embed colour", color_value, true)
        .field("❗ Custom-command prefix", clamp_text(&prefix_value, 1024), true)
        .field("🧩 Modules", clamp_text(&modules_value, 1024), false)
        .field("🔎 Search", urban_value, false)
        .field("🛡️ Moderator roles", clamp_text(&format!("{roles}\n{tail}"), 1024), false)
}

// ── Shared reply plumbing ────────────────────────────────────────────────────

/// `reply(embeds)`: never pings.
fn reply(embeds: Vec<CreateEmbed>) -> Payload {
    Payload::new().embeds(embeds).no_mentions()
}

/// `shown(raw)`: user input inside an error text, without code-span breakers, bounded.
pub fn shown(raw: &str) -> String {
    let cleaned: String = raw.chars().filter(|c| *c != '`' && *c != '\\').collect();
    let s = clamp_text(js::trim(&cleaned), 64);
    if s.is_empty() { "(empty)".into() } else { s }
}

/// `TZ_ERRORS[code]`: title and text for a refused `/settings timezone` zone.
pub fn tz_error(code: ZoneErrorCode, raw: &str) -> (&'static str, String) {
    match code {
        ZoneErrorCode::Empty => ("Missing Timezone", "Pick a timezone from the list, e.g. `Europe/London`.".into()),
        ZoneErrorCode::Offset => (
            "Pick a City",
            "Fixed offsets like `+05:30` ignore daylight saving time. Type the offset in the **zone** box to see the cities that use it, then pick one.".into(),
        ),
        ZoneErrorCode::Abbreviation => (
            "Ambiguous Timezone",
            "Abbreviations and country names such as `EST`, `CET` or `Japan` are ambiguous. Pick a Region/City zone such as `America/New_York`, `Europe/Paris` or `Asia/Tokyo`.".into(),
        ),
        ZoneErrorCode::Unknown | ZoneErrorCode::Ambiguous => (
            "Unknown Timezone",
            format!("`{}` is not a timezone I know. Pick one from the list, e.g. `Europe/London`, `America/New_York` or `Asia/Tokyo`.", shown(raw)),
        ),
    }
}

/// The invalid-colour text.
pub fn invalid_color_text(raw: &str) -> String {
    format!(
        "`{}` is not a colour. Use a hex code (`#5865F2` or `#fff`), a name (red, green, blue, blurple, yellow, orange, purple, pink, white, black, grey) or `reset`.",
        shown(raw)
    )
}

fn now_ms(ctx: &CommandCtx) -> i64 {
    ctx.app.clock().now_ms()
}

fn guild_key(ctx: &CommandCtx) -> anyhow::Result<String> {
    ctx.guild_id().map(|g| g.to_string()).ok_or_else(|| anyhow::anyhow!("/settings outside a guild"))
}

/// A field of the row as the JS object holds it (`previous?.[field] ?? null`).
fn field_json(row: Option<&GuildSettings>, field: &str) -> Value {
    row.and_then(|r| serde_json::to_value(r).ok()).and_then(|v| v.get(field).cloned()).unwrap_or(Value::Null)
}

/// `saveSetting(...)`: writes one setting through the settings service. The saved row, or `None` after answering
/// "Could Not Save".
async fn save_setting(ctx: &CommandCtx, sub: &str, field: &str, value: Value, previous: Option<&GuildSettings>) -> Option<Settings> {
    let g = guild_key(ctx).ok()?;
    let row = match ctx.app.settings().update_settings(&g, SettingsPatch::new().set(field, value.clone())).await {
        Ok(row) => row,
        Err(e) => {
            bot_util::log_error!(
                format!("[settings] {sub} save failed"),
                bot_core::error::log_arg(&anyhow::Error::from(e)),
                bot_util::logger::LogArg::Json(json!({ "guildId": g }))
            );
            safe_reply(
                &ctx.responder,
                reply(vec![error_embed("Could Not Save", Some("The setting could not be saved. Please try again in a moment."), None)]),
            )
            .await;
            return None;
        }
    };
    let old = field_json(previous, field);
    bot_util::log_info!(format!("[settings] {g}: {field} {old} → {value} by {}", ctx.user().id));
    Some(row)
}

async fn no_change(ctx: &CommandCtx, previous: Option<&GuildSettings>, text: &str) {
    safe_reply(&ctx.responder, reply(vec![apply_guild_color(info_embed("No Change", Some(text), None), previous)])).await;
}

// ── Subcommands ──────────────────────────────────────────────────────────────

async fn run_show(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/settings outside a guild"))?;
    let Some(settings) = ctx.app.settings().get_settings_safe(&guild_id.to_string()).await else {
        safe_reply(
            &ctx.responder,
            reply(vec![error_embed("Settings Unavailable", Some("Server settings could not be loaded. Please try again in a moment."), None)]),
        )
        .await;
        return Ok(());
    };
    let (name, role_ids) = with_cached_guild(&ctx.app, guild_id, |g| (Some(g.name.clone()), g.roles.keys().map(|r| r.to_string()).collect::<Vec<_>>()))
        .unwrap_or((None, Vec::new()));
    let embed = build_overview_embed(&settings, name.as_deref(), &|id| role_ids.iter().any(|r| r == id), now_ms(&ctx));
    safe_reply(&ctx.responder, reply(vec![embed])).await;
    Ok(())
}

async fn run_timezone(ctx: CommandCtx) -> BotResult {
    let raw = ctx.get_string("zone").ok_or_else(|| anyhow::anyhow!("Required option \"zone\" not found."))?;
    let zone = match normalize_time_zone(&raw) {
        Ok(z) => z,
        Err(code) => {
            let (title, text) = tz_error(code, &raw);
            reply_error(&ctx.responder, title, Some(&text), true).await;
            return Ok(());
        }
    };
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let g = guild_key(&ctx)?;
    let now = now_ms(&ctx) as f64;
    let previous = ctx.app.settings().get_settings_safe(&g).await;
    if previous.as_ref().is_some_and(|p| p.timezone == zone) {
        let text = format!("The server timezone is already **{zone}** ({}).", utc_offset_label_ms(&zone, now));
        no_change(&ctx, previous.as_deref(), &text).await;
        return Ok(());
    }
    if save_setting(&ctx, "timezone", "timezone", json!(zone), previous.as_deref()).await.is_none() {
        return Ok(());
    }
    let lt = describe_local_time_ms(&zone, now);
    let mut lines = vec![
        format!("The server timezone is now **{zone}** ({}).", lt.offset),
        format!("Local time there: **{}** on {}, {}.", lt.time, lt.weekday, lt.date),
    ];
    if let Some(p) = previous.as_ref().filter(|p| !p.timezone.is_empty() && p.timezone != zone) {
        lines.push(format!("Previously: `{}`.", shown(&p.timezone)));
    }
    lines.push(String::new());
    lines.push("Reminders, birthdays and other schedules use this timezone from their next run.".into());
    safe_reply(&ctx.responder, reply(vec![success_embed("Timezone Updated", Some(&lines.join("\n")), None)])).await;
    Ok(())
}

async fn run_color(ctx: CommandCtx) -> BotResult {
    let raw = ctx.get_string("value").ok_or_else(|| anyhow::anyhow!("Required option \"value\" not found."))?;
    let Some(r) = parse_color_input(&raw) else {
        reply_error(&ctx.responder, "Invalid Colour", Some(&invalid_color_text(&raw)), true).await;
        return Ok(());
    };
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let g = guild_key(&ctx)?;
    let previous = ctx.app.settings().get_settings_safe(&g).await;
    let wanted = r.color.map(i64::from);
    if let Some(p) = previous.as_ref()
        && p.embed_color.map(i64::from) == wanted
    {
        let text = match r.color {
            None => format!("The embed colour is already the default (**{}**).", fmt_color(i64::from(colors::INFO))),
            Some(c) => format!("The embed colour is already **{}**.", fmt_color(i64::from(c))),
        };
        no_change(&ctx, previous.as_deref(), &text).await;
        return Ok(());
    }
    let Some(row) = save_setting(&ctx, "color", "embedColor", json!(r.color), previous.as_deref()).await else { return Ok(()) };
    let first = match r.color {
        None => success_embed(
            "Embed Colour Reset",
            Some(&format!("Info embeds use the default colour **{}** again.", fmt_color(i64::from(colors::INFO)))),
            None,
        ),
        Some(c) => {
            let mut lines = vec![format!("Info embeds now use **{}**.", fmt_color(i64::from(c)))];
            if r.adjusted {
                lines.push("Pure black (#000000) shows as no colour in Discord, so **#010101** was saved instead.".into());
            }
            lines.push("Success, error and warning embeds keep their fixed colours.".into());
            success_embed("Embed Colour Updated", Some(&lines.join("\n")), None)
        }
    };
    let preview = guild_embed(Some(&row), Some("🎨 Preview"), Some("This is how info embeds from this bot look now."), None);
    safe_reply(&ctx.responder, reply(vec![first, preview])).await;
    Ok(())
}

async fn run_prefix(ctx: CommandCtx) -> BotResult {
    let raw = ctx.get_string("prefix").ok_or_else(|| anyhow::anyhow!("Required option \"prefix\" not found."))?;
    let prefix = match validate_prefix(&raw) {
        Ok(p) => p,
        Err(e) => {
            reply_error(&ctx.responder, "Invalid Prefix", Some(e.message()), true).await;
            return Ok(());
        }
    };
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let g = guild_key(&ctx)?;
    let previous = ctx.app.settings().get_settings_safe(&g).await;
    if previous.as_ref().is_some_and(|p| p.custom_prefix == prefix) {
        let text = if prefix.is_empty() {
            "Prefix commands are already off. `/tag use` still works.".to_string()
        } else {
            format!("The custom-command prefix is already `{prefix}`.")
        };
        no_change(&ctx, previous.as_deref(), &text).await;
        return Ok(());
    }
    let Some(row) = save_setting(&ctx, "prefix", "customPrefix", json!(prefix), previous.as_deref()).await else { return Ok(()) };
    if prefix.is_empty() {
        safe_reply(&ctx.responder, reply(vec![success_embed("Prefix Updated", Some("Prefix commands are off. /tag use still works."), None)])).await;
        return Ok(());
    }
    let mut text = format!("Custom commands now answer to `{prefix}name`, e.g. `{prefix}rules`.\nSlash commands are not affected.");
    if !is_module_enabled(Some(&row), Some(ModuleKey::Customcommands)) {
        text.push_str("\n\n⚠️ The **Custom Commands** module is off, so the prefix does nothing yet. Enable it with `/module enable customcommands`.");
    }
    safe_reply(&ctx.responder, reply(vec![success_embed("Prefix Updated", Some(&text), None)])).await;
    Ok(())
}

async fn run_urban(ctx: CommandCtx) -> BotResult {
    let nsfw_only = ctx.get_boolean("nsfw-only").ok_or_else(|| anyhow::anyhow!("Required option \"nsfw-only\" not found."))?;
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let g = guild_key(&ctx)?;
    let previous = ctx.app.settings().get_settings_safe(&g).await;
    if previous.as_ref().is_some_and(|p| p.urban_nsfw_only == nsfw_only) {
        let text = if nsfw_only {
            "/search urban already only works in age-restricted channels."
        } else {
            "/search urban already works in every channel."
        };
        no_change(&ctx, previous.as_deref(), text).await;
        return Ok(());
    }
    let Some(row) = save_setting(&ctx, "urban", "urbanNsfwOnly", json!(nsfw_only), previous.as_deref()).await else { return Ok(()) };
    let mut text = if nsfw_only {
        "/search urban now only works in age-restricted channels.".to_string()
    } else {
        "/search urban now works in every channel.".to_string()
    };
    if !is_module_enabled(Some(&row), Some(ModuleKey::Search)) {
        text.push_str("\n\n⚠️ The **Search Anything** module is off, so /search urban is unavailable. Enable it with `/module enable search`.");
    }
    safe_reply(&ctx.responder, reply(vec![success_embed("Urban Dictionary", Some(&text), None)])).await;
    Ok(())
}

// ── Timezone roles ───────────────────────────────────────────────────────────

/// `ConfigError`: a refusal decided inside the settings update (answered as "Not Saved").
#[derive(Debug, Clone)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

/// `timezoneRoleLine(link, guild, now)`: "• @CST → `America/Chicago` (UTC-05:00) · 14:12 now".
pub fn timezone_role_line(link: &TimezoneRole, role_exists: bool, now_ms: i64) -> String {
    let who = if role_exists { format!("<@&{}>", link.role_id) } else { format!("deleted role `{}`", link.role_id) };
    let now = now_ms as f64;
    format!("• {who} → `{}` ({}) · {} now", link.zone, utc_offset_label_ms(&link.zone, now), describe_local_time_ms(&link.zone, now).time)
}

/// The stored list as JSON (`[{ roleId, zone }]`).
pub fn timezone_roles_json(list: &[TimezoneRole]) -> Value {
    Value::Array(list.iter().map(|l| json!({ "roleId": l.role_id, "zone": l.zone })).collect())
}

/// `saveTimezoneRoles(interaction, change)`: the row-locked read-modify-write of `timezoneRoles`. Links to deleted
/// roles are dropped on every write (when the guild's roles are known). The saved row, or `None` after answering.
async fn save_timezone_roles<F>(ctx: &CommandCtx, change: F) -> Option<Settings>
where
    F: FnOnce(Vec<TimezoneRole>) -> Result<Vec<TimezoneRole>, ConfigError> + Send + 'static,
{
    let g = guild_key(ctx).ok()?;
    let live: Vec<String> = ctx
        .guild_id()
        .and_then(|gid| with_cached_guild(&ctx.app, gid, |guild| guild.roles.keys().map(|r| r.to_string()).collect()))
        .unwrap_or_default();
    let result = ctx
        .app
        .settings()
        .update_settings_with(&g, move |cur| {
            let list: Vec<TimezoneRole> = normalize_timezone_roles(cur.timezone_roles.as_ref().unwrap_or(&Value::Null))
                .into_iter()
                .filter(|l| live.is_empty() || live.contains(&l.role_id))
                .collect();
            let next = change(list)?;
            Ok(SettingsPatch::new().set("timezoneRoles", timezone_roles_json(&next)))
        })
        .await;
    match result {
        Ok(row) => Some(row),
        Err(e) => {
            if let Some(c) = e.downcast_ref::<ConfigError>() {
                safe_reply(&ctx.responder, reply(vec![error_embed("Not Saved", Some(&c.0), None)])).await;
                return None;
            }
            bot_util::log_error!(
                "[settings] timezone-role save failed",
                bot_core::error::log_arg(&e),
                bot_util::logger::LogArg::Json(json!({ "guildId": g }))
            );
            safe_reply(
                &ctx.responder,
                reply(vec![error_embed("Could Not Save", Some("The timezone roles could not be saved. Please try again in a moment."), None)]),
            )
            .await;
            None
        }
    }
}

async fn run_timezone_role_add(ctx: CommandCtx) -> BotResult {
    let role = crate::commands::info::option_role(&ctx, "role").ok_or_else(|| anyhow::anyhow!("Required option \"role\" not found."))?;
    let role_id = role.id.to_string();
    if ctx.guild_id().map(|g| g.to_string()).as_deref() == Some(role_id.as_str()) {
        reply_error(
            &ctx.responder,
            "Pick a Role",
            Some("Pick a timezone role such as @CST, not @everyone. To set the timezone for everyone else, use `/settings timezone`."),
            true,
        )
        .await;
        return Ok(());
    }
    let raw = ctx.get_string("zone").filter(|s| !s.is_empty());
    let zone = match &raw {
        Some(raw) => match parse_zone_input(raw) {
            Ok(p) => p.zone,
            Err(e) => {
                let (title, text) = zone_input_error(raw, e.code.as_str(), e.options.as_deref().unwrap_or(&[]));
                reply_error(&ctx.responder, &title, Some(&text), true).await;
                return Ok(());
            }
        },
        None => match zone_from_role_name(&role.name) {
            Some(z) => z,
            None => {
                let text = format!(
                    "I can't tell the timezone from the name **{}**. Run it again with `zone:`, e.g. `America/Chicago` or `CST`.",
                    escape_markdown(&clamp_text(&role.name, 100))
                );
                reply_error(&ctx.responder, "Which Timezone?", Some(&text), true).await;
                return Ok(());
            }
        },
    };
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let previous: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let (prev, rid, z) = (previous.clone(), role_id.clone(), zone.clone());
    let saved = save_timezone_roles(&ctx, move |list| {
        let before = list.iter().find(|l| l.role_id == rid).map(|l| l.zone.clone());
        if before.is_none() && list.len() >= TIMEZONE_ROLES {
            return Err(ConfigError(format!("You can have at most {TIMEZONE_ROLES} timezone roles. Remove one first.")));
        }
        if let Ok(mut p) = prev.lock() {
            p.clone_from(&before);
        }
        let mut next: Vec<TimezoneRole> = list.into_iter().filter(|l| l.role_id != rid).collect();
        next.push(TimezoneRole { role_id: rid, zone: z });
        Ok(next)
    })
    .await;
    if saved.is_none() {
        return Ok(());
    }
    let previous = previous.lock().map(|p| p.clone()).unwrap_or(None);
    let g = guild_key(&ctx)?;
    bot_util::log_info!(format!(
        "[settings] {g}: timezone role {role_id} {} → {zone} by {}",
        serde_json::to_string(&previous).unwrap_or_else(|_| "null".into()),
        ctx.user().id
    ));
    let now = now_ms(&ctx) as f64;
    let mut lines = vec![
        format!(
            "Members with <@&{role_id}> get **{}** in /timestamp{}.",
            zone_label_ms(&zone, now),
            if raw.is_some() { "" } else { " (read from the role name)" }
        ),
        format!(
            "It's **{}** there now. A member's own `/timezone set` still wins over the role.",
            describe_local_time_ms(&zone, now).time
        ),
    ];
    if let Some(p) = previous.as_ref().filter(|p| **p != zone) {
        lines.push(format!("Previously: `{p}`."));
    }
    lines.push(String::new());
    lines.push("Hand the roles out with `/reactionrole` and make the panel type **Unique**, so members hold only one timezone role.".into());
    safe_reply(&ctx.responder, reply(vec![success_embed("Timezone Role Saved", Some(&lines.join("\n")), None)])).await;
    Ok(())
}

async fn run_timezone_role_remove(ctx: CommandCtx) -> BotResult {
    let role_id = ctx.get_role_id("role").ok_or_else(|| anyhow::anyhow!("Required option \"role\" not found."))?.to_string();
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let rid = role_id.clone();
    let saved = save_timezone_roles(&ctx, move |list| {
        if !list.iter().any(|l| l.role_id == rid) {
            return Err(ConfigError(format!("<@&{rid}> is not a timezone role.")));
        }
        Ok(list.into_iter().filter(|l| l.role_id != rid).collect())
    })
    .await;
    if saved.is_none() {
        return Ok(());
    }
    let g = guild_key(&ctx)?;
    bot_util::log_info!(format!("[settings] {g}: timezone role {role_id} removed by {}", ctx.user().id));
    let text = format!("<@&{role_id}> no longer gives a timezone. The role itself is unchanged.");
    safe_reply(&ctx.responder, reply(vec![success_embed("Timezone Role Removed", Some(&text), None)])).await;
    Ok(())
}

/// The `/settings timezone-role list` embed (`role_exists` is `guild.roles.cache.has(id)`).
pub fn timezone_role_list_embed(settings: &GuildSettings, role_exists: &dyn Fn(&str) -> bool, now_ms: i64) -> CreateEmbed {
    let links = normalize_timezone_roles(settings.timezone_roles.as_ref().unwrap_or(&Value::Null));
    if links.is_empty() {
        return apply_guild_color(
            info_embed(
                "Timezone Roles",
                Some("No timezone roles yet. Link one with `/settings timezone-role add role:@CST`, then hand the roles out with `/reactionrole`."),
                None,
            ),
            Some(settings),
        );
    }
    let lines: Vec<String> = links.iter().map(|l| timezone_role_line(l, role_exists(&l.role_id), now_ms)).collect();
    let body = join_with_limit(&lines, "\n", 3600, default_more);
    let tail = format!(
        "\n\nMembers without one of these roles use `{}` (the server timezone) unless they saved their own with `/timezone set`. When a member has several, the highest role wins.",
        settings.timezone
    );
    apply_guild_color(info_embed(&format!("Timezone Roles ({}/{TIMEZONE_ROLES})", links.len()), Some(&format!("{body}{tail}")), None), Some(settings))
}

async fn run_timezone_role_list(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/settings outside a guild"))?;
    let Some(settings) = ctx.app.settings().get_settings_safe(&guild_id.to_string()).await else {
        safe_reply(
            &ctx.responder,
            reply(vec![error_embed("Settings Unavailable", Some("Server settings could not be loaded. Please try again in a moment."), None)]),
        )
        .await;
        return Ok(());
    };
    let role_ids: Vec<String> = with_cached_guild(&ctx.app, guild_id, |g| g.roles.keys().map(|r| r.to_string()).collect()).unwrap_or_default();
    let embed = timezone_role_list_embed(&settings, &|id| role_ids.iter().any(|r| r == id), now_ms(&ctx));
    safe_reply(&ctx.responder, reply(vec![embed])).await;
    Ok(())
}

async fn execute(ctx: CommandCtx) -> BotResult {
    let sub = ctx.subcommand().unwrap_or_default();
    if ctx.subcommand_group().as_deref() == Some("timezone-role") {
        return match sub.as_str() {
            "add" => run_timezone_role_add(ctx).await,
            "remove" => run_timezone_role_remove(ctx).await,
            "list" => run_timezone_role_list(ctx).await,
            _ => Ok(()),
        };
    }
    match sub.as_str() {
        "show" => run_show(ctx).await,
        "timezone" => run_timezone(ctx).await,
        "color" => run_color(ctx).await,
        "prefix" => run_prefix(ctx).await,
        "urban" => run_urban(ctx).await,
        _ => Ok(()),
    }
}

// ── Autocomplete ─────────────────────────────────────────────────────────────

fn zone_choices(list: Vec<bot_util::timezones::ZoneChoice>) -> Vec<Choice> {
    list.into_iter().map(|c| Choice::new(c.name, c.value)).collect()
}

/// `suggestTimezoneRoleZones(interaction, typed)`: the zone box of `/settings timezone-role add`. With nothing typed,
/// the zone the chosen role's name names comes first ('@CST' → America/Chicago).
pub fn suggest_timezone_role_zones(typed: &str, role_name: Option<&str>, now_ms: i64) -> Vec<Choice> {
    let opts = SearchOptions { abbreviations: true, now_ms: now_ms as f64, ..SearchOptions::default() };
    let choices = zone_choices(search_time_zones(typed, &opts));
    if !js::trim(typed).is_empty() {
        return choices;
    }
    let Some(from_name) = role_name.and_then(zone_from_role_name) else { return choices };
    let mut out = vec![Choice::new(
        format!("{from_name} ({}) · from the role name", utc_offset_label_ms(&from_name, now_ms as f64)),
        from_name.clone(),
    )];
    out.extend(choices.into_iter().filter(|c| c.value.as_str() != Some(from_name.as_str())));
    out
}

/// The autocomplete choices of `/settings` (`group`, `sub`, the focused option and its text).
pub fn settings_choices(
    group: Option<&str>,
    sub: Option<&str>,
    focused: &str,
    typed: &str,
    current: Option<&GuildSettings>,
    role_name: Option<&str>,
    now_ms: i64,
) -> Vec<Choice> {
    if group == Some("timezone-role") {
        if sub == Some("add") && focused == "zone" {
            return suggest_timezone_role_zones(typed, role_name, now_ms);
        }
        return Vec::new();
    }
    if sub == Some("timezone") && focused == "zone" {
        let opts = SearchOptions { current: current.map(|s| s.timezone.as_str()), now_ms: now_ms as f64, ..SearchOptions::default() };
        return zone_choices(search_time_zones(typed, &opts));
    }
    if sub == Some("color") && focused == "value" {
        return suggest_colors(typed, current.and_then(|s| s.embed_color).map(i64::from));
    }
    Vec::new()
}

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let (focused, typed) = ctx.focused().unwrap_or_default();
    // Sync memo only; may be absent: no I/O on keystrokes.
    let current = ctx.guild_id().and_then(|g| ctx.app.settings().peek_settings(&g.to_string()));
    let role_name = match (ctx.guild_id(), ctx.get_role_id("role")) {
        (Some(g), Some(r)) => with_cached_guild(&ctx.app, g, |guild| guild.roles.get(&r).map(|role| role.name.clone())).flatten(),
        _ => None,
    };
    let group = ctx.subcommand_group();
    let sub = ctx.subcommand();
    let now = ctx.app.clock().now_ms();
    let choices = settings_choices(group.as_deref(), sub.as_deref(), &focused, &typed, current.as_deref(), role_name.as_deref(), now);
    respond_autocomplete(&ctx.responder, &choices).await; // clamps to 25 / 100 chars and catches
    Ok(())
}
